//! Рисование полосами. Кадр бара целиком не собирается: список примитивов
//! (прямоугольники, глифы, иконки) раскладывается в буфер полосы шириной в несколько
//! десятков пикселей, полоса сразу уходит в X. Памяти нужно на одну полосу.

use crate::text::Text;
use crate::tray::Icon;
use alloc::{rc::Rc, vec::Vec};

#[cfg(target_endian = "big")]
compile_error!("odjkbar: as_bytes рассчитан на little-endian");

/// Примитив в абсолютных координатах бара.
pub enum Prim {
    Rect { x: i32, y: i32, w: i32, h: i32, color: u32 },
    /// idx -- индекс глифа в Text; (x, y) -- pen и baseline уже с подъёмом
    Glyph { idx: usize, x: i32, y: i32, color: u32 },
    /// иконка в прямоугольнике (x, y, w, h); исходные пиксели масштабируются при отрисовке
    Icon { x: i32, y: i32, w: i32, h: i32, icon: Rc<Icon> },
}

/// Буфер полосы 0xAARRGGBB: столбцы [x0, x0 + w), строки [0, h).
pub struct Canvas {
    pub x0: i32,
    pub w: i32,
    pub h: i32,
    px: Vec<u32>,
}

impl Canvas {
    pub const fn new() -> Self {
        Self { x0: 0, w: 0, h: 0, px: Vec::new() }
    }

    /// Задаёт полосу и заливает фон. Память растёт только при увеличении размера.
    pub fn reset(&mut self, x0: i32, w: i32, h: i32, bg: u32) {
        self.x0 = x0;
        self.w = w;
        self.h = h;
        self.px.clear();
        self.px.resize((w.max(0) * h.max(0)) as usize, bg);
    }

    /// Рисует все примитивы, попадающие в полосу.
    pub fn paint(&mut self, prims: &[Prim], t: &Text) {
        for p in prims {
            match *p {
                Prim::Rect { x, y, w, h, color } => {
                    let xa = (x - self.x0).max(0);
                    let xb = (x + w - self.x0).min(self.w);
                    let ya = y.max(0);
                    let yb = (y + h).min(self.h);
                    for yy in ya..yb {
                        for xx in xa..xb {
                            self.px[(yy * self.w + xx) as usize] = color;
                        }
                    }
                }
                Prim::Glyph { idx, x, y, color } => {
                    let g = t.glyph(idx);
                    for gy in 0..g.h {
                        let py = y + g.y0 + gy;
                        if py < 0 || py >= self.h {
                            continue;
                        }
                        for gx in 0..g.w {
                            let a = g.cov[(gy * g.w + gx) as usize];
                            if a != 0 {
                                self.blend(x + g.x0 + gx, py, color, a);
                            }
                        }
                    }
                }
                Prim::Icon { x, y, w, h, ref icon } => {
                    if w <= 0 || h <= 0 || icon.w <= 0 || icon.h <= 0 {
                        continue;
                    }
                    // ближайший сосед: без буфера под масштабированную копию
                    for yy in 0..h {
                        let sy = yy * icon.h / h;
                        for xx in 0..w {
                            let sx = xx * icon.w / w;
                            let v = icon.px[(sy * icon.w + sx) as usize];
                            let a = (v >> 24) as u8;
                            if a > 0 {
                                self.blend(x + xx, y + yy, v & 0xffffff, a);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Смешивание с фоном; x -- абсолютная координата бара.
    fn blend(&mut self, x: i32, y: i32, fg: u32, a: u8) {
        let sx = x - self.x0;
        if sx < 0 || y < 0 || sx >= self.w || y >= self.h {
            return;
        }
        let i = (y * self.w + sx) as usize;
        let d = self.px[i];
        let (a, ia) = (a as u32, 255 - a as u32);
        let mix = |sh: u32| ((((fg >> sh) & 0xff) * a + ((d >> sh) & 0xff) * ia) / 255) << sh;
        self.px[i] = mix(16) | mix(8) | mix(0);
    }

    /// Пиксели полосы как байты ZPixmap без копирования.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: u32 -> 4 байта подряд, длина считается точно, выравнивание u8 меньше
        unsafe { core::slice::from_raw_parts(self.px.as_ptr() as *const u8, self.px.len() * 4) }
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}
