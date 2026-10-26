//! Шрифты без загрузки в память.
//!
//! Файл шрифта открыт, но не отображён (mmap нет). Парсер читает через `pread`
//! только таблицы-заголовки и байты нужных глифов. Глиф растеризуется здесь же
//! (контур -> квадратичные кривые -> накопление площади) в маску покрытия 0..255,
//! которая кэшируется. Без внешних зависимостей, без libm, без f32::floor (это std).
//!
//! Поддерживается TrueType (таблица glyf, простые и составные глифы) и cmap форматов 4 и 12.

use alloc::{collections::BTreeMap, vec, vec::Vec};

/// Чтение `buf.len()` байт с абсолютного смещения. false -- ошибка или конец файла.
pub type ReadAt = fn(fd: i32, off: u64, buf: &mut [u8]) -> bool;

/// Разобранная строка "ПУТЬ:size=PT;ПОДЪЁМ".
pub struct Spec<'a> {
    pub path: &'a str,
    /// Размер в пикселях (пункты пересчитаны при 96 DPI).
    pub px: f32,
    pub lift: f32,
}

pub fn parse_spec(raw: &str) -> Spec<'_> {
    let mut parts = raw.split(';');
    let pat = parts.next().unwrap_or(raw);
    let lift = parts.next().and_then(|v| v.trim().parse::<f32>().ok()).unwrap_or(0.0);
    let size = pat
        .split(':')
        .find_map(|p| p.strip_prefix("size="))
        .and_then(|v| v.parse::<f32>().ok())
        .unwrap_or(12.0);
    let path = pat.split(':').next().unwrap_or(pat);
    Spec { path, px: size * 96.0 / 72.0, lift }
}

// ---------- арифметика без std ----------

fn floor_i(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) > x { i - 1 } else { i }
}
fn ceil_i(x: f32) -> i32 {
    -floor_i(-x)
}
/// Ближайшее целое (половины вверх).
pub fn round_i(x: f32) -> i32 {
    floor_i(x + 0.5)
}
/// min/max для f32 без libm (компилятор при opt-level=z зовёт fminf/fmaxf, которых нет без libc).
fn fmin(a: f32, b: f32) -> f32 {
    if a < b { a } else { b }
}
fn fmax(a: f32, b: f32) -> f32 {
    if a > b { a } else { b }
}
fn floor_f(x: f32) -> f32 {
    floor_i(x) as f32
}
fn ceil_f(x: f32) -> f32 {
    ceil_i(x) as f32
}

// ---------- чтение из файла ----------

#[derive(Clone, Copy)]
struct R {
    rd: ReadAt,
    fd: i32,
}

impl R {
    fn bytes<const N: usize>(&self, off: u32) -> Option<[u8; N]> {
        let mut b = [0u8; N];
        if (self.rd)(self.fd, off as u64, &mut b) { Some(b) } else { None }
    }
    fn u16(&self, off: u32) -> Option<u16> {
        self.bytes::<2>(off).map(u16::from_be_bytes)
    }
    fn i16(&self, off: u32) -> Option<i16> {
        self.bytes::<2>(off).map(i16::from_be_bytes)
    }
    fn u32(&self, off: u32) -> Option<u32> {
        self.bytes::<4>(off).map(u32::from_be_bytes)
    }
}

// ---------- шрифт ----------

/// Лёгкий авто-хинтинг: только по вертикали. Ключевые высоты шрифта (базовая линия,
/// x-height, заглавные, верх выносных, низ нижних) привязываются к целым пикселям,
/// остальные координаты Y между ними растягиваются линейно. Свесы круглых букв
/// (o, e, с) прижимаются к той же линии, поэтому горизонтали не размазываются на два ряда.
#[derive(Clone, Copy)]
struct Hint {
    n: usize,
    /// узлы: исходная высота (px, вверх) -> высота после хинтинга
    a: [f32; 10],
    b: [f32; 10],
}

impl Hint {
    const fn none() -> Self {
        Hint { n: 0, a: [0.0; 10], b: [0.0; 10] }
    }

    /// Добавляет узел; узлы должны идти по возрастанию, b не убывает (монотонность).
    fn push(&mut self, a: f32, b: f32) {
        if self.n >= 10 {
            return;
        }
        if self.n > 0 && a <= self.a[self.n - 1] {
            return;
        }
        let b = if self.n > 0 { fmax(b, self.b[self.n - 1]) } else { b };
        self.a[self.n] = a;
        self.b[self.n] = b;
        self.n += 1;
    }

    fn map(&self, y: f32) -> f32 {
        let n = self.n;
        if n == 0 {
            return y;
        }
        if y <= self.a[0] {
            return y + self.b[0] - self.a[0];
        }
        if y >= self.a[n - 1] {
            return y + self.b[n - 1] - self.a[n - 1];
        }
        for i in 0..n - 1 {
            if y < self.a[i + 1] {
                let t = (y - self.a[i]) / (self.a[i + 1] - self.a[i]);
                return self.b[i] + t * (self.b[i + 1] - self.b[i]);
            }
        }
        y
    }
}

#[derive(Clone, Copy)]
struct Face {
    hint: Hint,
    fd: i32,
    scale: f32,
    /// подъём глифов в пикселях (округлён)
    lift: i32,
    /// метрики основного шрифта в пикселях; desc < 0
    asc: f32,
    desc: f32,
    loca: u32,
    glyf: u32,
    hmtx: u32,
    /// число записей hmetrics
    nhm: u32,
    ngl: u32,
    long_loca: bool,
    /// абсолютное смещение выбранной подтаблицы cmap
    cmap: u32,
    cmap_fmt: u16,
}

/// Растеризованный глиф. Координаты пикселей относительно точки (pen_x, baseline).
pub struct Glyph {
    /// левый верхний угол маски относительно pen
    pub x0: i32,
    pub y0: i32,
    pub w: i32,
    pub h: i32,
    /// маска покрытия w*h, по строкам
    pub cov: Vec<u8>,
    /// шаг пера в пикселях
    pub adv: f32,
    /// вертикальные границы контура (y вниз, относительно baseline)
    pub ymin: f32,
    pub ymax: f32,
    /// подъём шрифта, из которого взят глиф
    pub lift: i32,
}

pub struct Text {
    rd: ReadAt,
    faces: Vec<Face>,
    /// символ -> (индекс шрифта, glyph id)
    chars: BTreeMap<char, (u8, u16)>,
    /// (индекс шрифта, glyph id) -> индекс в glyphs
    index: BTreeMap<(u8, u16), usize>,
    glyphs: Vec<Glyph>,
}

impl Text {
    /// Список: (открытый fd, размер px, подъём). None, если шрифтов нет или они не разобрались.
    pub fn new(list: &[(i32, f32, f32)], rd: ReadAt) -> Option<Self> {
        if list.is_empty() || list.len() > 255 {
            return None;
        }
        let mut faces = Vec::with_capacity(list.len());
        for &(fd, px, lift) in list {
            faces.push(load_face(R { rd, fd }, px, lift)?);
        }
        Some(Self { rd, faces, chars: BTreeMap::new(), index: BTreeMap::new(), glyphs: Vec::new() })
    }

    /// Глиф для символа: загружается один раз, потом берётся из кэша.
    pub fn resolve(&mut self, c: char) -> usize {
        let (fi, gid) = match self.chars.get(&c) {
            Some(&v) => v,
            None => {
                let mut found = (0u8, 0u16);
                for (i, f) in self.faces.iter().enumerate() {
                    let g = cmap_get(R { rd: self.rd, fd: f.fd }, f, c as u32);
                    if g != 0 {
                        found = (i as u8, g);
                        break;
                    }
                }
                self.chars.insert(c, found);
                found
            }
        };
        if let Some(&i) = self.index.get(&(fi, gid)) {
            return i;
        }
        let f = self.faces[fi as usize];
        let g = make_glyph(R { rd: self.rd, fd: f.fd }, &f, gid);
        self.glyphs.push(g);
        let i = self.glyphs.len() - 1;
        self.index.insert((fi, gid), i);
        i
    }

    pub fn glyph(&self, i: usize) -> &Glyph {
        &self.glyphs[i]
    }

    /// (asc, desc) основного шрифта в пикселях; desc отрицательный.
    pub fn metrics(&self) -> (f32, f32) {
        (self.faces[0].asc, self.faces[0].desc)
    }

    pub fn width(&mut self, s: &str) -> i32 {
        let mut w = 0.0f32;
        for c in s.chars() {
            let i = self.resolve(c);
            w += self.glyphs[i].adv;
        }
        round_i(w)
    }
}

fn load_face(r: R, px: f32, lift: f32) -> Option<Face> {
    let n = r.u16(4)? as u32;
    let mut dir = vec![0u8; n as usize * 16];
    if !(r.rd)(r.fd, 12, &mut dir) {
        return None;
    }
    let find = |tag: &[u8; 4]| -> Option<u32> {
        dir.chunks_exact(16)
            .find(|rec| &rec[0..4] == tag)
            .map(|rec| u32::from_be_bytes([rec[8], rec[9], rec[10], rec[11]]))
    };
    let head = find(b"head")?;
    let hhea = find(b"hhea")?;
    let hmtx = find(b"hmtx")?;
    let maxp = find(b"maxp")?;
    let cmap_t = find(b"cmap")?;
    let loca = find(b"loca")?;
    let glyf = find(b"glyf")?;

    let long_loca = r.i16(head + 50)? != 0;
    let hasc = r.i16(hhea + 4)? as f32;
    let hdesc = r.i16(hhea + 6)? as f32;
    // px задаёт высоту шрифта (ascender - descender), как PxScale в ab_glyph
    let height = hasc - hdesc;
    if height <= 0.0 {
        return None;
    }
    let scale = px / height;
    let asc = hasc * scale;
    let desc = hdesc * scale;
    let nhm = r.u16(hhea + 34)? as u32;
    let ngl = r.u16(maxp + 4)? as u32;

    // подтаблица cmap: предпочитаем формат 12 (полный Unicode), иначе 4
    let nt = r.u16(cmap_t + 2)? as u32;
    let mut best: Option<(u32, u16)> = None;
    let mut best_rank = 0;
    for i in 0..nt {
        let rec = cmap_t + 4 + i * 8;
        let (Some(pid), Some(off)) = (r.u16(rec), r.u32(rec + 4)) else { continue };
        let sub = cmap_t + off;
        let Some(fmt) = r.u16(sub) else { continue };
        let rank = match fmt {
            12 => 2,
            4 => 1,
            _ => 0,
        };
        if rank > best_rank && (pid == 0 || pid == 3) {
            best = Some((sub, fmt));
            best_rank = rank;
        }
    }
    let (cmap, cmap_fmt) = best?;

    let upem = r.u16(head + 18).unwrap_or(1000) as f32;
    let mut face = Face {
        hint: Hint::none(),
        fd: r.fd,
        scale,
        lift: round_i(lift),
        asc,
        desc,
        loca,
        glyf,
        hmtx,
        nhm,
        ngl,
        long_loca,
        cmap,
        cmap_fmt,
    };
    face.hint = build_hint(r, &face, upem * scale);
    Some(face)
}

/// Границы глифа по Y (единицы шрифта) из заголовка глифа.
fn glyph_ybounds(r: R, f: &Face, gid: u16) -> Option<(f32, f32)> {
    if gid == 0 || gid as u32 >= f.ngl {
        return None;
    }
    let g = gid as u32;
    let (o0, o1) = if f.long_loca {
        (r.u32(f.loca + 4 * g)?, r.u32(f.loca + 4 * g + 4)?)
    } else {
        (2 * r.u16(f.loca + 2 * g)? as u32, 2 * r.u16(f.loca + 2 * g + 2)? as u32)
    };
    if o1 <= o0 {
        return None;
    }
    let ymin = r.i16(f.glyf + o0 + 4)? as f32;
    let ymax = r.i16(f.glyf + o0 + 8)? as f32;
    Some((ymin, ymax))
}

/// Измеряет высоты по глифам x, H, l, p и строит таблицу привязки. Нет латиницы -- без хинтинга.
fn build_hint(r: R, f: &Face, em_px: f32) -> Hint {
    let mut h = Hint::none();
    let s = f.scale;
    // допуск на свес круглых букв
    let tol = fmin(fmax(0.03 * em_px, 0.2), 0.6);
    let ch = |c: char| glyph_ybounds(r, f, cmap_get(r, f, c as u32));
    let (Some((_, xh)), Some((_, cap))) = (ch('x'), ch('H')) else { return h };
    let xh = xh * s;
    let cap = cap * s;
    if xh < 2.0 * tol || cap <= xh + 2.0 * tol {
        return h;
    }
    // низ нижних выносных (p)
    if let Some((d, _)) = ch('p') {
        let d = d * s;
        let bd = round_i(d) as f32;
        if d < -0.75 && bd < -tol {
            h.push(d - tol, bd);
            h.push(d, bd);
        }
    }
    // базовая линия: свес вниз прижимается к 0
    h.push(-tol, 0.0);
    h.push(0.0, 0.0);
    let bx = round_i(xh + 0.1) as f32;
    h.push(xh, bx);
    h.push(xh + tol, bx);
    let bc = round_i(cap + 0.1) as f32;
    h.push(cap, bc);
    h.push(cap + tol, bc);
    // верх выносных (l), если он заметно выше заглавных
    if let Some((_, asc)) = ch('l') {
        let asc = asc * s;
        if asc > cap + tol {
            let ba = round_i(asc + 0.1) as f32;
            h.push(asc, ba);
            h.push(asc + tol, ba);
        }
    }
    h
}

fn cmap_get(r: R, f: &Face, c: u32) -> u16 {
    let v = match f.cmap_fmt {
        4 => cmap4(r, f.cmap, c),
        12 => cmap12(r, f.cmap, c),
        _ => None,
    };
    v.unwrap_or(0)
}

fn cmap4(r: R, s: u32, c: u32) -> Option<u16> {
    if c > 0xFFFF {
        return None;
    }
    let c = c as u16;
    let segx2 = r.u16(s + 6)? as u32;
    let seg = segx2 / 2;
    let ends = s + 14;
    let starts = ends + segx2 + 2;
    let deltas = starts + segx2;
    let ranges = deltas + segx2;
    // первый сегмент с end >= c
    let (mut lo, mut hi) = (0u32, seg);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if r.u16(ends + 2 * mid)? < c {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == seg {
        return None;
    }
    let start = r.u16(starts + 2 * lo)?;
    if c < start {
        return None;
    }
    let delta = r.u16(deltas + 2 * lo)?;
    let ro = r.u16(ranges + 2 * lo)? as u32;
    if ro == 0 {
        return Some(c.wrapping_add(delta));
    }
    let g = r.u16(ranges + 2 * lo + ro + 2 * (c - start) as u32)?;
    if g == 0 { None } else { Some(g.wrapping_add(delta)) }
}

fn cmap12(r: R, s: u32, c: u32) -> Option<u16> {
    let n = r.u32(s + 12)?;
    let base = s + 16;
    let (mut lo, mut hi) = (0u32, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let end = r.u32(base + mid * 12 + 4)?;
        if end < c {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == n {
        return None;
    }
    let g = base + lo * 12;
    let start = r.u32(g)?;
    if c < start {
        return None;
    }
    let sg = r.u32(g + 8)?;
    Some((sg.wrapping_add(c - start)) as u16)
}

// ---------- контуры ----------

#[derive(Clone, Copy)]
struct P {
    x: f32,
    y: f32,
    on: bool,
}

/// Разбор байтов глифа в памяти (уже прочитанного через pread).
struct B<'a> {
    d: &'a [u8],
    p: usize,
}

impl B<'_> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.d.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn i8(&mut self) -> Option<i8> {
        self.u8().map(|v| v as i8)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_be_bytes([*self.d.get(self.p)?, *self.d.get(self.p + 1)?]);
        self.p += 2;
        Some(v)
    }
    fn i16(&mut self) -> Option<i16> {
        self.u16().map(|v| v as i16)
    }
}

fn f2dot14(v: i16) -> f32 {
    v as f32 / 16384.0
}

/// Собирает контуры глифа в единицах шрифта. Ends -- абсолютные (исключающие) индексы в pts.
fn outline(r: R, f: &Face, gid: u16, depth: u32, pts: &mut Vec<P>, ends: &mut Vec<usize>) -> Option<()> {
    if depth > 4 || gid as u32 >= f.ngl {
        return Some(());
    }
    let g = gid as u32;
    let (o0, o1) = if f.long_loca {
        (r.u32(f.loca + 4 * g)?, r.u32(f.loca + 4 * g + 4)?)
    } else {
        (2 * r.u16(f.loca + 2 * g)? as u32, 2 * r.u16(f.loca + 2 * g + 2)? as u32)
    };
    if o1 <= o0 {
        return Some(());
    }
    let mut buf = vec![0u8; (o1 - o0) as usize];
    if !(r.rd)(r.fd, (f.glyf + o0) as u64, &mut buf) || buf.len() < 10 {
        return None;
    }
    let nc = i16::from_be_bytes([buf[0], buf[1]]);
    if nc >= 0 {
        simple(&buf, nc as usize, pts, ends)
    } else {
        composite(r, f, &buf, depth, pts, ends)
    }
}

fn simple(d: &[u8], nc: usize, pts: &mut Vec<P>, ends: &mut Vec<usize>) -> Option<()> {
    let mut b = B { d, p: 10 };
    let base = pts.len();
    let mut last = 0usize;
    for _ in 0..nc {
        last = b.u16()? as usize;
        ends.push(base + last + 1);
    }
    let n = last + 1;
    let ilen = b.u16()? as usize;
    b.p += ilen;

    let mut flags: Vec<u8> = Vec::with_capacity(n);
    while flags.len() < n {
        let f = b.u8()?;
        flags.push(f);
        if f & 8 != 0 {
            let rep = b.u8()?;
            for _ in 0..rep {
                flags.push(f);
            }
        }
    }
    flags.truncate(n);

    let mut xs = Vec::with_capacity(n);
    let mut x = 0i32;
    for &f in &flags {
        if f & 2 != 0 {
            let dv = b.u8()? as i32;
            x += if f & 16 != 0 { dv } else { -dv };
        } else if f & 16 == 0 {
            x += b.i16()? as i32;
        }
        xs.push(x as f32);
    }
    let mut ys = Vec::with_capacity(n);
    let mut y = 0i32;
    for &f in &flags {
        if f & 4 != 0 {
            let dv = b.u8()? as i32;
            y += if f & 32 != 0 { dv } else { -dv };
        } else if f & 32 == 0 {
            y += b.i16()? as i32;
        }
        ys.push(y as f32);
    }
    for i in 0..n {
        pts.push(P { x: xs[i], y: ys[i], on: flags[i] & 1 != 0 });
    }
    Some(())
}

fn composite(r: R, f: &Face, d: &[u8], depth: u32, pts: &mut Vec<P>, ends: &mut Vec<usize>) -> Option<()> {
    let mut b = B { d, p: 10 };
    loop {
        let flags = b.u16()?;
        let gi = b.u16()?;
        let (dx, dy) = if flags & 1 != 0 {
            (b.i16()? as f32, b.i16()? as f32)
        } else {
            (b.i8()? as f32, b.i8()? as f32)
        };
        // аргументы-точки (бит 1 не выставлен) не поддерживаем: смещение нулевое
        let (dx, dy) = if flags & 2 != 0 { (dx, dy) } else { (0.0, 0.0) };
        let (mut a, mut bm, mut c, mut dd) = (1.0f32, 0.0f32, 0.0f32, 1.0f32);
        if flags & 8 != 0 {
            a = f2dot14(b.i16()?);
            dd = a;
        } else if flags & 0x40 != 0 {
            a = f2dot14(b.i16()?);
            dd = f2dot14(b.i16()?);
        } else if flags & 0x80 != 0 {
            a = f2dot14(b.i16()?);
            bm = f2dot14(b.i16()?);
            c = f2dot14(b.i16()?);
            dd = f2dot14(b.i16()?);
        }
        let start = pts.len();
        let _ = outline(r, f, gi, depth + 1, pts, ends);
        for p in &mut pts[start..] {
            let (x, y) = (p.x, p.y);
            p.x = a * x + c * y + dx;
            p.y = bm * x + dd * y + dy;
        }
        if flags & 0x20 == 0 {
            break;
        }
    }
    Some(())
}

// ---------- растеризация ----------

/// Накопление площади по отрезку (метод накопления, как в font-rs). a -- буфер со строкой w.
fn line(a: &mut [f32], w: usize, h: usize, p0: (f32, f32), p1: (f32, f32)) {
    if p1.1 - p0.1 == 0.0 {
        return;
    }
    let (dir, p0, p1) = if p0.1 < p1.1 { (1.0f32, p0, p1) } else { (-1.0f32, p1, p0) };
    let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
    let mut x = p0.0;
    let y0 = fmax(p0.1, 0.0) as usize;
    let y1 = (ceil_i(p1.1).max(0) as usize).min(h);
    for y in y0..y1 {
        let ls = y * w;
        let dy = fmin((y + 1) as f32, p1.1) - fmax(y as f32, p0.1);
        let xnext = x + dxdy * dy;
        let d = dy * dir;
        let (xa, xb) = if x < xnext { (x, xnext) } else { (xnext, x) };
        let xa_f = floor_f(xa);
        let xa_i = xa_f as i32;
        let xb_c = ceil_f(xb);
        let xb_i = xb_c as i32;
        let base = ls as isize + xa_i as isize;
        if base >= 0 {
            let i = base as usize;
            if xb_i <= xa_i + 1 {
                let xmf = 0.5 * (x + xnext) - xa_f;
                a[i] += d - d * xmf;
                a[i + 1] += d * xmf;
            } else {
                let s = 1.0 / (xb - xa);
                let xaf = xa - xa_f;
                let a0 = 0.5 * s * (1.0 - xaf) * (1.0 - xaf);
                let xbf = xb - xb_c + 1.0;
                let am = 0.5 * s * xbf * xbf;
                a[i] += d * a0;
                if xb_i == xa_i + 2 {
                    a[i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - xaf);
                    a[i + 1] += d * (a1 - a0);
                    for xi in (xa_i + 2)..(xb_i - 1) {
                        a[ls + xi as usize] += d * s;
                    }
                    let a2 = a1 + (xb_i - xa_i - 3) as f32 * s;
                    a[ls + (xb_i - 1) as usize] += d * (1.0 - a2 - am);
                }
                a[ls + xb_i as usize] += d * am;
            }
        }
        x = xnext;
    }
}

fn quad(a: &mut [f32], w: usize, h: usize, p0: (f32, f32), c: (f32, f32), p1: (f32, f32)) {
    const N: usize = 8;
    let mut prev = p0;
    for k in 1..=N {
        let t = k as f32 / N as f32;
        let u = 1.0 - t;
        let x = u * u * p0.0 + 2.0 * u * t * c.0 + t * t * p1.0;
        let y = u * u * p0.1 + 2.0 * u * t * c.1 + t * t * p1.1;
        line(a, w, h, prev, (x, y));
        prev = (x, y);
    }
}

/// Один замкнутый контур: неявные середины между двумя внеконтурными точками.
fn contour(src: &[P], s: f32, ox: f32, oy: f32, a: &mut [f32], w: usize, h: usize) {
    let n = src.len();
    if n < 2 {
        return;
    }
    let mut items: Vec<(f32, f32, bool)> = Vec::with_capacity(n * 2);
    for i in 0..n {
        let p = src[i];
        let q = src[(i + 1) % n];
        items.push((p.x * s - ox, -p.y * s - oy, p.on));
        if !p.on && !q.on {
            items.push((((p.x + q.x) * 0.5) * s - ox, -((p.y + q.y) * 0.5) * s - oy, true));
        }
    }
    let Some(k) = items.iter().position(|it| it.2) else { return };
    items.rotate_left(k);
    let m = items.len();
    let mut cur = items[0];
    let mut i = 1;
    while i <= m {
        let it = items[i % m];
        if it.2 {
            line(a, w, h, (cur.0, cur.1), (it.0, it.1));
            cur = it;
            i += 1;
        } else {
            let nx = items[(i + 1) % m];
            quad(a, w, h, (cur.0, cur.1), (it.0, it.1), (nx.0, nx.1));
            cur = nx;
            i += 2;
        }
    }
}

/// Растеризация контуров в маску. Возвращает (x0, y0, w, h, маска, ymin, ymax).
fn raster(pts: &[P], ends: &[usize], s: f32) -> (i32, i32, i32, i32, Vec<u8>, f32, f32) {
    if pts.is_empty() {
        return (0, 0, 0, 0, Vec::new(), 0.0, 0.0);
    }
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        let (x, y) = (p.x * s, -p.y * s);
        minx = fmin(minx, x);
        maxx = fmax(maxx, x);
        miny = fmin(miny, y);
        maxy = fmax(maxy, y);
    }
    let ox = floor_i(minx);
    let oy = floor_i(miny);
    let w = (ceil_i(maxx) - ox + 2).max(1) as usize;
    let h = (ceil_i(maxy) - oy + 2).max(1) as usize;
    let stride = w + 2;
    let mut acc = vec![0f32; stride * h + 4];
    let mut start = 0;
    for &end in ends {
        if end > start && end <= pts.len() {
            contour(&pts[start..end], s, ox as f32, oy as f32, &mut acc, stride, h);
        }
        start = end;
    }
    let mut cov = vec![0u8; w * h];
    for y in 0..h {
        let mut sum = 0f32;
        for x in 0..w {
            sum += acc[y * stride + x];
            let v = if sum < 0.0 { -sum } else { sum };
            let v = fmin(v, 1.0);
            cov[y * w + x] = (v * 255.0 + 0.5) as u8;
        }
    }
    (ox, oy, w as i32, h as i32, cov, miny, maxy)
}

fn make_glyph(r: R, f: &Face, gid: u16) -> Glyph {
    let mut pts = Vec::new();
    let mut ends = Vec::new();
    if outline(r, f, gid, 0, &mut pts, &mut ends).is_none() {
        pts.clear();
        ends.clear();
    }
    // хинтинг по Y: считаем в пикселях, возвращаем в единицы шрифта для растеризатора
    if f.hint.n > 0 {
        for p in pts.iter_mut() {
            p.y = f.hint.map(p.y * f.scale) / f.scale;
        }
    }
    let (x0, y0, w, h, cov, ymin, ymax) = raster(&pts, &ends, f.scale);
    let idx = (gid as u32).min(f.nhm.saturating_sub(1));
    let adv = r.u16(f.hmtx + 4 * idx).map_or(0.0, |v| v as f32 * f.scale);
    Glyph { x0, y0, w, h, cov, adv, ymin, ymax, lift: f.lift }
}
