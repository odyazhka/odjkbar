//! Кодирование запросов протокола X11 (little-endian). Без std.
//! Каждая функция возвращает готовые байты запроса. Ответы и события разбираются отдельно.

use alloc::{vec, vec::Vec};

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn pad4(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

/// Заголовок запроса: код, байт данных, длина (записывается в finish).
fn start(opcode: u8, data: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.push(opcode);
    b.push(data);
    b.extend_from_slice(&[0, 0]);
    b
}

/// Дополняет до 4 байт и записывает длину запроса в словах.
fn finish(mut b: Vec<u8>) -> Vec<u8> {
    pad4(&mut b);
    let words = (b.len() / 4) as u16;
    b[2..4].copy_from_slice(&words.to_le_bytes());
    b
}

/// Атрибуты окна (CreateWindow / ChangeWindowAttributes). None -- не задано.
#[derive(Default, Clone, Copy)]
pub struct WinAttrs {
    pub background_pixel: Option<u32>,
    pub event_mask: Option<u32>,
    pub cursor: Option<u32>,
}

/// Маска и значения в порядке возрастания битов (так требует протокол).
fn win_values(a: &WinAttrs) -> (u32, Vec<u32>) {
    let mut mask = 0u32;
    let mut v = Vec::new();
    if let Some(x) = a.background_pixel {
        mask |= 0x0002;
        v.push(x);
    }
    if let Some(x) = a.event_mask {
        mask |= 0x0800;
        v.push(x);
    }
    if let Some(x) = a.cursor {
        mask |= 0x4000;
        v.push(x);
    }
    (mask, v)
}

/// CreateWindow (код 1). class: 1 = InputOutput.
pub fn create_window(
    depth: u8,
    wid: u32,
    parent: u32,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    border: u16,
    class: u16,
    visual: u32,
    a: &WinAttrs,
) -> Vec<u8> {
    let mut b = start(1, depth);
    put32(&mut b, wid);
    put32(&mut b, parent);
    put16(&mut b, x as u16);
    put16(&mut b, y as u16);
    put16(&mut b, w);
    put16(&mut b, h);
    put16(&mut b, border);
    put16(&mut b, class);
    put32(&mut b, visual);
    let (mask, vals) = win_values(a);
    put32(&mut b, mask);
    for v in vals {
        put32(&mut b, v);
    }
    finish(b)
}

/// ChangeWindowAttributes (код 2).
pub fn change_window_attributes(window: u32, a: &WinAttrs) -> Vec<u8> {
    let mut b = start(2, 0);
    put32(&mut b, window);
    let (mask, vals) = win_values(a);
    put32(&mut b, mask);
    for v in vals {
        put32(&mut b, v);
    }
    finish(b)
}

/// MapWindow (код 8).
pub fn map_window(window: u32) -> Vec<u8> {
    let mut b = start(8, 0);
    put32(&mut b, window);
    finish(b)
}

/// InternAtom (код 16).
pub fn intern_atom(only_if_exists: bool, name: &[u8]) -> Vec<u8> {
    let mut b = start(16, only_if_exists as u8);
    put16(&mut b, name.len() as u16);
    put16(&mut b, 0);
    b.extend_from_slice(name);
    finish(b)
}

/// ChangeProperty (код 18). mode: 0 Replace, 1 Prepend, 2 Append. format: 8, 16 или 32.
pub fn change_property(mode: u8, window: u32, property: u32, ty: u32, format: u8, data: &[u8]) -> Vec<u8> {
    let mut b = start(18, mode);
    put32(&mut b, window);
    put32(&mut b, property);
    put32(&mut b, ty);
    b.push(format);
    b.extend_from_slice(&[0, 0, 0]);
    put32(&mut b, (data.len() / (format as usize / 8)) as u32);
    b.extend_from_slice(data);
    finish(b)
}

/// GetProperty (код 20).
pub fn get_property(delete: bool, window: u32, property: u32, ty: u32, long_offset: u32, long_length: u32) -> Vec<u8> {
    let mut b = start(20, delete as u8);
    put32(&mut b, window);
    put32(&mut b, property);
    put32(&mut b, ty);
    put32(&mut b, long_offset);
    put32(&mut b, long_length);
    finish(b)
}

/// OpenFont (код 45).
pub fn open_font(fid: u32, name: &[u8]) -> Vec<u8> {
    let mut b = start(45, 0);
    put32(&mut b, fid);
    put16(&mut b, name.len() as u16);
    put16(&mut b, 0);
    b.extend_from_slice(name);
    finish(b)
}

/// CreateGlyphCursor (код 94). Цвета -- 16-битные RGB.
pub fn create_glyph_cursor(
    cid: u32,
    source_font: u32,
    mask_font: u32,
    source_char: u16,
    mask_char: u16,
    fore: (u16, u16, u16),
    back: (u16, u16, u16),
) -> Vec<u8> {
    let mut b = start(94, 0);
    put32(&mut b, cid);
    put32(&mut b, source_font);
    put32(&mut b, mask_font);
    put16(&mut b, source_char);
    put16(&mut b, mask_char);
    put16(&mut b, fore.0);
    put16(&mut b, fore.1);
    put16(&mut b, fore.2);
    put16(&mut b, back.0);
    put16(&mut b, back.1);
    put16(&mut b, back.2);
    finish(b)
}

/// CreateGC (код 55) с foreground и background.
pub fn create_gc(cid: u32, drawable: u32, fg: Option<u32>, bg: Option<u32>) -> Vec<u8> {
    let mut b = start(55, 0);
    put32(&mut b, cid);
    put32(&mut b, drawable);
    let mut mask = 0u32;
    let mut vals = Vec::new();
    if let Some(x) = fg {
        mask |= 0x0004;
        vals.push(x);
    }
    if let Some(x) = bg {
        mask |= 0x0008;
        vals.push(x);
    }
    put32(&mut b, mask);
    for v in vals {
        put32(&mut b, v);
    }
    finish(b)
}

/// Заголовок PutImage (код 72), формат ZPixmap (2). Данные (data_len байт, кратно 4)
/// отправляются следом отдельным записью: копии запроса не делается.
pub fn put_image_head(depth: u8, drawable: u32, gc: u32, w: u16, h: u16, dst_x: i16, dst_y: i16, data_len: usize) -> Vec<u8> {
    let mut b = start(72, 2);
    put32(&mut b, drawable);
    put32(&mut b, gc);
    put16(&mut b, w);
    put16(&mut b, h);
    put16(&mut b, dst_x as u16);
    put16(&mut b, dst_y as u16);
    b.push(0); // left-pad
    b.push(depth);
    put16(&mut b, 0);
    let words = ((b.len() + data_len) / 4) as u16;
    b[2..4].copy_from_slice(&words.to_le_bytes());
    b
}

/// QueryExtension (код 98).
pub fn query_extension(name: &[u8]) -> Vec<u8> {
    let mut b = start(98, 0);
    put16(&mut b, name.len() as u16);
    put16(&mut b, 0);
    b.extend_from_slice(name);
    finish(b)
}

/// RandR GetMonitors: major -- код расширения из QueryExtension, минорный код 42.
pub fn randr_get_monitors(major: u8, window: u32, get_active: bool) -> Vec<u8> {
    let mut b = start(major, 42);
    put32(&mut b, window);
    b.push(get_active as u8);
    finish(b)
}

/// XKB UseExtension: минорный код 0.
pub fn xkb_use_extension(major: u8, wanted_major: u16, wanted_minor: u16) -> Vec<u8> {
    let mut b = start(major, 0);
    put16(&mut b, wanted_major);
    put16(&mut b, wanted_minor);
    finish(b)
}

/// XKB SelectEvents (минорный код 1): подписка на события, bits -- маска типов.
pub fn xkb_select_events(major: u8, device: u16, bits: u16) -> Vec<u8> {
    let mut b = start(major, 1);
    put16(&mut b, device);
    put16(&mut b, bits); // affect_which
    put16(&mut b, 0); // clear
    put16(&mut b, bits); // select_all
    put16(&mut b, 0); // affect_map
    put16(&mut b, 0); // map
    finish(b)
}

/// XKB GetState (минорный код 4).
pub fn xkb_get_state(major: u8, device: u16) -> Vec<u8> {
    let mut b = start(major, 4);
    put16(&mut b, device);
    put16(&mut b, 0);
    finish(b)
}

/// XKB LatchLockState (минорный код 5): переключает группу раскладки.
pub fn xkb_latch_lock_state(major: u8, device: u16, lock_group: bool, group: u8) -> Vec<u8> {
    let mut b = start(major, 5);
    put16(&mut b, device);
    b.push(0); // affect_mod_locks
    b.push(0); // mod_locks
    b.push(lock_group as u8);
    b.push(group);
    b.push(0); // affect_mod_latches
    b.push(0); // pad
    b.push(0); // latch_group
    put16(&mut b, 0); // group_latch
    finish(b)
}

/// Первый запрос: рукопожатие. Код байтового порядка 'l' (little-endian).
pub fn setup_request(auth_name: &[u8], auth_data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + auth_name.len() + auth_data.len());
    b.push(b'l');
    b.push(0);
    put16(&mut b, 11); // major
    put16(&mut b, 0); // minor
    put16(&mut b, auth_name.len() as u16);
    put16(&mut b, auth_data.len() as u16);
    put16(&mut b, 0);
    b.extend_from_slice(auth_name);
    pad4(&mut b);
    b.extend_from_slice(auth_data);
    pad4(&mut b);
    b
}

// ---------- разбор ответов и событий ----------

pub fn u16_at(p: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(p.get(o..o + 2)?.try_into().ok()?))
}

pub fn u32_at(p: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(p.get(o..o + 4)?.try_into().ok()?))
}

fn i16_at(p: &[u8], o: usize) -> Option<i16> {
    u16_at(p, o).map(|v| v as i16)
}

/// Общая длина пакета (ответ или событие) по его заголовку: 32 байта + 4 * длина_в_словах для ответа.
pub fn reply_len(p: &[u8]) -> Option<usize> {
    Some(32 + 4 * u32_at(p, 4)? as usize)
}

/// Разобранный пакет от сервера.
pub enum Ev {
    Error,
    Reply { seq: u16 },
    Expose,
    /// ButtonPress: detail -- номер кнопки
    Button { detail: u8, window: u32, x: i16, y: i16 },
    Motion { window: u32, x: i16, y: i16 },
    Property { window: u32, atom: u32 },
    Other(u8),
}

pub fn parse_packet(p: &[u8]) -> Option<Ev> {
    let t = *p.first()?;
    Some(match t {
        0 => Ev::Error,
        1 => Ev::Reply { seq: u16_at(p, 2)? },
        code => match code & 0x7f {
            4 => Ev::Button { detail: *p.get(1)?, window: u32_at(p, 12)?, x: i16_at(p, 24)?, y: i16_at(p, 26)? },
            6 => Ev::Motion { window: u32_at(p, 12)?, x: i16_at(p, 24)?, y: i16_at(p, 26)? },
            12 => Ev::Expose,
            28 => Ev::Property { window: u32_at(p, 4)?, atom: u32_at(p, 8)? },
            c => Ev::Other(c),
        },
    })
}

/// Успешное рукопожатие: поля, нужные для окон.
pub struct Setup {
    pub resource_base: u32,
    pub resource_mask: u32,
    pub root: u32,
    pub root_visual: u32,
    pub root_depth: u8,
    pub width: u16,
}

/// Разбор тела ответа на рукопожатие (без первых 8 байт заголовка).
pub fn parse_setup(body: &[u8]) -> Option<Setup> {
    let resource_base = u32_at(body, 4)?;
    let resource_mask = u32_at(body, 8)?;
    let vendor_len = u16_at(body, 16)? as usize;
    let nformats = *body.get(21)? as usize;
    // фиксированная часть 32 байта, затем имя поставщика, форматы (по 8 байт) и экраны
    let o = 32 + align4(vendor_len) + nformats * 8;
    // первый экран: корень, 5 полей по 4 байта, затем размеры, потом корневой визуал и глубина
    let root = u32_at(body, o)?;
    let width = u16_at(body, o + 20)?;
    let root_visual = u32_at(body, o + 32)?;
    let root_depth = *body.get(o + 38)?;
    Some(Setup { resource_base, resource_mask, root, root_visual, root_depth, width })
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// Ответ QueryExtension: (присутствует, код основного запроса, код первого события).
pub fn parse_query_extension(p: &[u8]) -> Option<(bool, u8, u8)> {
    Some((*p.get(8)? != 0, *p.get(9)?, *p.get(10)?))
}

/// Ответ InternAtom: номер атома.
pub fn parse_intern_atom(p: &[u8]) -> Option<u32> {
    u32_at(p, 8)
}

/// Ответ GetAtomName: имя (байты) внутри p.
pub fn parse_atom_name(p: &[u8]) -> Option<&[u8]> {
    let n = u16_at(p, 8)? as usize;
    p.get(32..32 + n)
}

/// Ответ GetProperty: (тип, значение байтами).
pub fn parse_get_property(p: &[u8]) -> Option<(u32, &[u8])> {
    let format = *p.get(1)? as usize;
    let ty = u32_at(p, 8)?;
    let len = u32_at(p, 16)? as usize;
    Some((ty, p.get(32..32 + len * format / 8)?))
}

/// Монитор из ответа RandR GetMonitors.
#[derive(Clone, Copy)]
pub struct Monitor {
    pub name: u32,
    pub primary: bool,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// Ответ RandR GetMonitors: список мониторов.
pub fn parse_monitors(p: &[u8]) -> Option<alloc::vec::Vec<Monitor>> {
    let n = u32_at(p, 12)? as usize;
    let mut out = alloc::vec::Vec::with_capacity(n);
    let mut o = 32;
    for _ in 0..n {
        let name = u32_at(p, o)?;
        let primary = *p.get(o + 4)? != 0;
        let n_out = u16_at(p, o + 6)? as usize;
        out.push(Monitor {
            name,
            primary,
            x: i16_at(p, o + 8)?,
            y: i16_at(p, o + 10)?,
            width: u16_at(p, o + 12)?,
            height: u16_at(p, o + 14)?,
        });
        o += 24 + 4 * n_out;
    }
    Some(out)
}

/// Ответ XKB GetState: эффективная группа раскладки.
pub fn parse_xkb_group(p: &[u8]) -> Option<u8> {
    p.get(12).copied()
}

/// GetAtomName (код 17).
pub fn get_atom_name(atom: u32) -> Vec<u8> {
    let mut b = start(17, 0);
    put32(&mut b, atom);
    finish(b)
}

/// Предопределённые типы свойств X11 (константы протокола, интернировать не нужно).
pub const ATOM_TY: u32 = 4;
pub const CARDINAL_TY: u32 = 6;

/// Атомы, которые бинарь интернирует один раз через `intern_atom`.
pub struct DockAtoms {
    pub strut: u32,
    pub strut_partial: u32,
    pub window_type: u32,
    pub type_dock: u32,
}

fn le_u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Говорит WM оставить сверху полосу высотой `bar_h` на мониторе (mon_x, mon_y, mon_w).
/// Координаты абсолютные, относительно всего экрана.
/// Возвращает запросы ChangeProperty, их нужно отправить до MapWindow.
pub fn reserve_top(window: u32, a: &DockAtoms, bar_h: u32, mon_x: i32, mon_y: i32, mon_w: u32) -> Vec<Vec<u8>> {
    let x0 = mon_x.max(0) as u32;
    let x1 = x0.saturating_add(mon_w).saturating_sub(1);
    let top = (mon_y.max(0) as u32).saturating_add(bar_h);
    // порядок: left, right, top, bottom, left_start_y, left_end_y, right_start_y, right_end_y,
    // top_start_x, top_end_x, bottom_start_x, bottom_end_x
    let partial = [0, 0, top, 0, 0, 0, 0, 0, x0, x1, 0, 0];
    // старый вариант для WM, которые не знают STRUT_PARTIAL
    let strut = [0, 0, top, 0];
    vec![
        change_property(0, window, a.window_type, ATOM_TY, 32, &le_u32s(&[a.type_dock])),
        change_property(0, window, a.strut_partial, CARDINAL_TY, 32, &le_u32s(&partial)),
        change_property(0, window, a.strut, CARDINAL_TY, 32, &le_u32s(&strut)),
    ]
}
