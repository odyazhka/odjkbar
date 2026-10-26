//! odjkbar: бинарь поверх odjkbar_core. Без libc и без std: прямые системные вызовы (см. sys.rs),
//! один поток и один цикл poll. Свой клиент X11, D-Bus, bspc, файлы шрифтов и батареи.

#![no_std]
#![no_main]

extern crate alloc;

mod sys;

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use odjkbar_core::canvas::{Canvas, Prim};
use odjkbar_core::clock;
use odjkbar_core::config::{hex, Config};
use odjkbar_core::dbus::{self, Msg as BusMsg};
use odjkbar_core::modules::{self, Action, Ctx, Seg, State};
use odjkbar_core::text::{parse_spec, round_i, Text};
use odjkbar_core::tray;
use odjkbar_core::x11::{self as x, Ev, WinAttrs};
use sys::{die, lossy, PollFd, POLLERR, POLLHUP, POLLIN};

const BAT: &str = "/sys/class/power_supply/BAT0";
const WM_NAME: u32 = 39;
const WINDOW: u32 = 33;
const EV_EXPOSE: u32 = 0x8000;
const EV_BUTTON: u32 = 0x0004;
const EV_MOTION: u32 = 0x0040;
const EV_PROPERTY: u32 = 0x40_0000;
const XKB_CORE_KBD: u16 = 0x0100;
const XKB_STATE_NOTIFY: u16 = 0x0004;

struct Mon {
    name: String,
    x: i16,
    y: i16,
    w: u16,
}

struct Hit {
    x0: i32,
    x1: i32,
    act: Action,
}

struct BarWin {
    id: u32,
    gc: u32,
    w: u16,
    h: u16,
    mon: String,
    text: Text,
    left: Vec<String>,
    center: Vec<String>,
    right: Vec<String>,
    hits: Vec<Hit>,
    cv: Canvas,
    /// сейчас показан курсор-рука
    hand: bool,
}

struct Look {
    bg: u32,
    fg: u32,
    primary: u32,
    alert: u32,
    charging: u32,
    depth: u8,
}

/// Сессия D-Bus: состояние шины, сокет и буфер недочитанных байт.
struct Sess {
    bus: dbus::Bus,
    fd: i32,
    rbuf: Vec<u8>,
}

impl Sess {
    /// Следующее целое сообщение из буфера. Err -- поток испорчен, соединение надо бросить.
    fn next_msg(&mut self) -> Result<Option<BusMsg>, ()> {
        loop {
            if self.rbuf.len() < 16 {
                return Ok(None);
            }
            let total = dbus::msg_len(&self.rbuf[..16]).ok_or(())?;
            if self.rbuf.len() < total {
                return Ok(None);
            }
            let raw: Vec<u8> = self.rbuf.drain(..total).collect();
            if let Some(m) = dbus::parse(&raw) {
                return Ok(Some(m));
            }
        }
    }
}

/// Ожидаемый ответ X по номеру запроса.
#[derive(Clone, Copy)]
enum Pend {
    Active,
    TitleNet(u32),
    TitleWm(u32),
    Group,
}

/// Атомы, которые нужны в работе.
#[derive(Clone, Copy)]
struct Atoms {
    active: u32,
    net_name: u32,
    utf8: u32,
    wm_type: u32,
    dock: u32,
    strut: u32,
    strut_old: u32,
}

/// Соединение с X-сервером.
struct X {
    fd: i32,
    seq: u16,
    base: u32,
    mask: u32,
    ctr: u32,
    root: u32,
    root_visual: u32,
    root_depth: u8,
    width: u16,
    /// недочитанные байты
    rbuf: Vec<u8>,
}

impl X {
    /// Отправляет запрос; возвращает его номер (последовательный номер сервера).
    fn send(&mut self, req: &[u8]) -> u16 {
        let _ = sys::write_all(self.fd, req);
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Данные без заголовка и без номера запроса: продолжение PutImage.
    fn send_raw(&mut self, data: &[u8]) {
        let _ = sys::write_all(self.fd, data);
    }

    /// Один read в буфер. false -- соединение закрыто.
    fn fill(&mut self) -> bool {
        let mut tmp = [0u8; 4096];
        match sys::read(self.fd, &mut tmp) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.rbuf.extend_from_slice(&tmp[..n]);
                true
            }
        }
    }

    /// Целый пакет из буфера: 32 байта, у ответа и generic-события ещё хвост по длине.
    fn take_pkt(&mut self) -> Option<Vec<u8>> {
        if self.rbuf.len() < 32 {
            return None;
        }
        let t = self.rbuf[0];
        let need = if t == 1 || t & 0x7f == 35 { x::reply_len(&self.rbuf)? } else { 32 };
        if self.rbuf.len() < need {
            return None;
        }
        Some(self.rbuf.drain(..need).collect())
    }

    fn read_pkt_blocking(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(p) = self.take_pkt() {
                return Some(p);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    /// Запрос с ответом (только при старте): читает пакеты, пока не придёт ответ с нужным номером.
    fn call(&mut self, req: &[u8]) -> Option<Vec<u8>> {
        let want = self.send(req);
        loop {
            let p = self.read_pkt_blocking()?;
            match x::parse_packet(&p)? {
                Ev::Reply { seq } if seq == want => return Some(p),
                Ev::Error => {
                    if x::u16_at(&p, 2)? == want {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }

    fn id(&mut self) -> u32 {
        let v = self.base | (self.ctr & self.mask);
        self.ctr += 1;
        v
    }

    fn intern(&mut self, name: &str) -> u32 {
        self.call(&x::intern_atom(false, name.as_bytes()))
            .and_then(|p| x::parse_intern_atom(&p))
            .unwrap_or(0)
    }

    /// Расширение: (код основного запроса, код первого события) или None.
    fn extension(&mut self, name: &str) -> Option<(u8, u8)> {
        let p = self.call(&x::query_extension(name.as_bytes()))?;
        let (present, major, first_event) = x::parse_query_extension(&p)?;
        if present { Some((major, first_event)) } else { None }
    }
}

/// Читает запись MIT-MAGIC-COOKIE-1 для нужного дисплея из $XAUTHORITY или ~/.Xauthority.
fn xauth(display: u32) -> Option<(Vec<u8>, Vec<u8>)> {
    let path = match sys::getenv("XAUTHORITY") {
        Some(p) => p.to_string(),
        None => format!("{}/.Xauthority", sys::getenv("HOME").unwrap_or("")),
    };
    let d = sys::read_file(&path)?;
    let want = display.to_string();
    let mut i = 0usize;
    while i + 2 <= d.len() {
        i += 2; // семейство адреса
        let (_addr, ni) = read_counted(&d, i)?;
        let (number, ni) = read_counted(&d, ni)?;
        let (name, ni) = read_counted(&d, ni)?;
        let (data, ni) = read_counted(&d, ni)?;
        i = ni;
        if name == b"MIT-MAGIC-COOKIE-1" && (number.is_empty() || number == want.as_bytes()) {
            return Some((name.to_vec(), data.to_vec()));
        }
    }
    None
}

/// Строка с 2-байтной длиной в big-endian: (данные, позиция после них).
fn read_counted(d: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let n = u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]) as usize;
    Some((d.get(i + 2..i + 2 + n)?, i + 2 + n))
}

/// Подключение к X-серверу по $DISPLAY (локальный сокет), с рукопожатием.
fn x_connect() -> Option<X> {
    let disp = sys::getenv("DISPLAY")?;
    let num: u32 = disp.rsplit(':').next()?.split('.').next()?.parse().ok()?;
    let path = format!("/tmp/.X11-unix/X{num}");
    let fd = sys::connect_unix(&path, false).or_else(|| sys::connect_unix(&path, true))?;
    let (name, data) = xauth(num).unwrap_or_default();
    if !sys::write_all(fd, &x::setup_request(&name, &data)) {
        return None;
    }
    let mut h = [0u8; 8];
    if !sys::read_exact(fd, &mut h) || h[0] != 1 {
        return None;
    }
    let len = u16::from_le_bytes([h[6], h[7]]) as usize * 4;
    let mut body = vec![0u8; len];
    if !sys::read_exact(fd, &mut body) {
        return None;
    }
    let setup = x::parse_setup(&body)?;
    Some(X {
        fd,
        seq: 0,
        base: setup.resource_base,
        mask: setup.resource_mask,
        ctr: 0,
        root: setup.root,
        root_visual: setup.root_visual,
        root_depth: setup.root_depth,
        width: setup.width,
        rbuf: Vec::new(),
    })
}

fn unix_now() -> i64 {
    (sys::now_ms() / 1000) as i64
}

/// Локальный пояс: TZ (имя зоны, путь или POSIX-строка), иначе /etc/localtime; нет -- UTC.
fn load_tz() -> clock::Tz {
    let tz = match sys::getenv("TZ") {
        Some(t) if !t.is_empty() => sys::read_file(&clock::tz_path(t))
            .and_then(|d| clock::Tz::from_tzif(&d))
            .or_else(|| clock::Tz::from_posix(t.trim_start_matches(':'))),
        _ => sys::read_file("/etc/localtime").and_then(|d| clock::Tz::from_tzif(&d)),
    };
    tz.unwrap_or_default()
}

fn load_config() -> Config {
    let path = match sys::getenv("ODJKBAR_CONFIG") {
        Some(p) => p.to_string(),
        None => format!("{}/.config/odjkbar/config.toml", sys::getenv("HOME").unwrap_or("")),
    };
    let Some(raw) = sys::read_file(&path) else {
        die(&format!("odjkbar: не читается {path}"))
    };
    let src = lossy(&raw);
    match Config::parse(&src) {
        Ok(c) => c,
        Err(e) => die(&format!("odjkbar: ошибка в конфиге: {e}")),
    }
}

/// Шрифты: файл открывается, но не отображается в память. Парсер читает через pread
/// только заголовки и байты нужных глифов.
fn load_text(specs: &[String]) -> Option<Text> {
    let mut list = Vec::new();
    for raw in specs {
        let sp = parse_spec(raw);
        let fd = sys::open_ro(sp.path)?;
        list.push((fd, sp.px, sp.lift));
    }
    Text::new(&list, sys::pread)
}

/// Размер иконки в баре: вписывается в высоту минус отступы, пропорции сохраняются, не увеличивается.
fn icon_box(icon: &odjkbar_core::tray::Icon, h: i32) -> (i32, i32) {
    if icon.w <= 0 || icon.h <= 0 {
        return (0, 0);
    }
    let max = (h - 4).max(1);
    let (mut dw, mut dh) = (icon.w, icon.h);
    if dh > max {
        dw = dw * max / dh;
        dh = max;
    }
    if dw > max {
        dh = dh * max / dw;
        dw = max;
    }
    (dw.max(1), dh.max(1))
}

/// Подписка на состояние bspwm напрямую через сокет, без процесса bspc.
/// Путь: BSPWM_SOCKET или /tmp/bspwm_<дисплей>_<экран>-socket. Запрос: "subscribe\0report\0".
fn bspwm_subscribe() -> Option<i32> {
    let path = match sys::getenv("BSPWM_SOCKET") {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            let disp = sys::getenv("DISPLAY")?;
            let mut parts = disp.rsplit(':').next()?.split('.');
            let num = parts.next()?;
            let scr = parts.next().unwrap_or("0");
            format!("/tmp/bspwm_{num}_{scr}-socket")
        }
    };
    let fd = sys::connect_unix(&path, false)?;
    if !sys::write_all(fd, b"subscribe\0report\0") {
        sys::close(fd);
        return None;
    }
    Some(fd)
}

/// Буфер после разбора: если ёмкость выросла до пика, а данных мало, возвращаем память.
fn trim(v: &mut Vec<u8>) {
    if v.capacity() > 8192 && v.len() < 1024 {
        v.shrink_to(4096);
    }
}

fn cell_w(t: &mut Text, s: &str, h: i32) -> i32 {
    if s.trim().chars().count() == 1 { h } else { t.width(s) }
}

/// Слот под звёздочку = ширина пробела.
fn star_slot(t: &mut Text) -> i32 {
    t.width(" ")
}

fn seg_width(t: &mut Text, segs: &[Seg], h: i32) -> i32 {
    let mut w = 0;
    for s in segs {
        w += match s {
            Seg::Desk { name, last, .. } => {
                let slot = star_slot(t);
                slot + t.width(name) + if *last { 0 } else { slot }
            }
            Seg::Image { .. } => h,
            Seg::Text { s, .. } => cell_w(t, s, h),
            Seg::Sep { w, .. } => *w,
        };
    }
    w
}

/// Строка -> глифы с позициями. Одиночный символ центрируется по своему контуру.
fn draw_str(out: &mut Vec<Prim>, t: &mut Text, x: i32, h: i32, s: &str, color: u32) {
    let single = s.trim().chars().count() == 1;
    let (asc, desc) = t.metrics();
    let baseline = round_i((h as f32 - (asc - desc)) / 2.0 + asc);
    let mut pen = x as f32;
    for c in s.chars() {
        let idx = t.resolve(c);
        let g = t.glyph(idx);
        let y = if single {
            round_i((h as f32 - (g.ymax - g.ymin)) / 2.0 - g.ymin)
        } else {
            baseline - g.lift
        };
        out.push(Prim::Glyph { idx, x: round_i(pen), y, color });
        pen += g.adv;
    }
}

fn place(out: &mut Vec<Prim>, t: &mut Text, segs: &[Seg], mut x: i32, h: i32, hits: &mut Vec<Hit>) {
    for s in segs {
        match s {
            Seg::Text { s, fg, act, .. } => {
                let tw = t.width(s);
                let cw = cell_w(t, s, h);
                draw_str(out, t, x + (cw - tw) / 2, h, s, *fg);
                if !matches!(act, Action::None) {
                    hits.push(Hit { x0: x, x1: x + cw, act: act.clone() });
                }
                x += cw;
            }
            Seg::Desk { name, star, fg, act, last } => {
                let slot = star_slot(t);
                let nw = t.width(name);
                let total = slot + nw + if *last { 0 } else { slot };
                if *star {
                    let sw = t.width("*");
                    draw_str(out, t, x + (slot - sw) / 2, h, "*", *fg);
                }
                draw_str(out, t, x + slot, h, name, *fg);
                if !matches!(act, Action::None) {
                    hits.push(Hit { x0: x, x1: x + total, act: act.clone() });
                }
                x += total;
            }
            Seg::Image { icon, act } => {
                let cw = h;
                let (dw, dh) = icon_box(icon, h);
                out.push(Prim::Icon { x: x + (cw - dw) / 2, y: (h - dh) / 2, w: dw, h: dh, icon: icon.clone() });
                if !matches!(act, Action::None) {
                    hits.push(Hit { x0: x, x1: x + cw, act: act.clone() });
                }
                x += cw;
            }
            Seg::Sep { w, block, color } => {
                out.push(Prim::Rect { x: x + (w - block) / 2, y: 0, w: *block, h, color: *color });
                x += w;
            }
        }
    }
}

/// Ширины трёх групп (слева, по центру, справа).
fn group_widths(t: &mut Text, g: [&[Seg]; 3], h: i32) -> [i32; 3] {
    [seg_width(t, g[0], h), seg_width(t, g[1], h), seg_width(t, g[2], h)]
}

/// Зазор между соседними группами. Два разделителя на стыке совпадают полосками: зазор
/// отрицательный и равен их перекрытию, поэтому видна одна линия. Разделитель с одной стороны
/// уже несёт свой отступ, там дополнительного зазора нет. Иначе обычный зазор.
fn gap_between(a: &[Seg], b: &[Seg]) -> i32 {
    match (a.last(), b.first()) {
        (Some(Seg::Sep { w: wa, block: ba, .. }), Some(Seg::Sep { w: wb, block: bb, .. })) => {
            -((*wa + *ba) / 2 + (*wb - *bb) / 2)
        }
        (Some(Seg::Sep { .. }), _) | (_, Some(Seg::Sep { .. })) => 0,
        _ => modules::MARGIN,
    }
}

/// Нужная ширина: три группы плюс промежутки между ними.
fn needed(ws: [i32; 3], gl: i32, gr: i32) -> i32 {
    ws.iter().sum::<i32>() + gl + gr
}

/// Обрезает строку по ширине с многоточием. Пустая строка, если даже многоточие не влезает.
fn fit(t: &mut Text, s: &str, budget: i32, h: i32) -> String {
    if cell_w(t, s, h) <= budget {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    for n in (0..chars.len()).rev() {
        let cand = format!("{}…", chars[..n].iter().collect::<String>());
        if cell_w(t, &cand, h) <= budget {
            return cand;
        }
    }
    String::new()
}

/// Раскладка трёх групп без наложения: слева, по центру, справа.
/// Если места не хватает, сначала сужается название окна (flex), затем убираются промежутки.
/// Центр стоит по середине, но не заходит ни на левую, ни на правую группу, поэтому при
/// нехватке места он сдвигается к левому краю. Возвращает x начала каждой группы.
fn layout(t: &mut Text, left: &mut [Seg], center: &mut [Seg], right: &mut [Seg], w: i32, h: i32) -> (i32, i32, i32) {
    let left_off = if matches!(left.first(), Some(Seg::Sep { .. })) { -modules::MARGIN } else { 0 };
    let ne = [!left.is_empty(), !center.is_empty(), !right.is_empty()];
    // промежутки между соседними непустыми группами
    let (mut gl, mut gr) = if ne[1] {
        (
            if ne[0] { gap_between(left, center) } else { 0 },
            if ne[2] { gap_between(center, right) } else { 0 },
        )
    } else if ne[0] && ne[2] {
        (gap_between(left, right), 0)
    } else {
        (0, 0)
    };
    let mut ws = group_widths(t, [&*left, &*center, &*right], h);
    let mut excess = needed(ws, gl, gr) - w;

    if excess > 0 {
        // сужаем первое гибкое место (название окна) на недостающую величину
        let mut done = false;
        for g in [&mut *left, &mut *center, &mut *right] {
            for seg in g.iter_mut() {
                if done {
                    break;
                }
                if let Seg::Text { s, flex: true, .. } = seg {
                    let cur = cell_w(t, s.as_str(), h);
                    let new = fit(t, s.as_str(), (cur - excess).max(0), h);
                    *s = new;
                    done = true;
                }
            }
        }
        ws = group_widths(t, [&*left, &*center, &*right], h);
        excess = needed(ws, gl, gr) - w;
    }
    if excess > 0 {
        // если и без названия тесно, обычные зазоры уходят; совпавшие разделители остаются
        gl = gl.min(0);
        gr = gr.min(0);
    }

    let [lw, cw, rw] = ws;
    let lx = left_off;
    let rx = w - rw;
    let ideal = (w - cw) / 2;
    let min_c = if ne[0] { lx + lw + gl } else { 0 };
    let max_c = if ne[2] { rx - gr - cw } else { w - cw };
    let cx = ideal.min(max_c).max(min_c);
    (lx, cx, rx)
}

fn draw_bar(x: &mut X, b: &mut BarWin, look: &Look, st: &State, cfg: &Config) {
    let unix = unix_now();
    let off = cfg.utc_offset_min.unwrap_or_else(|| st.tz.offset_min(unix));
    let now = clock::civil(unix, off);
    let (w, h) = (b.w as i32, b.h as i32);
    let mon = b.mon.clone();
    let ctx = Ctx {
        cfg,
        st,
        mon: &mon,
        fg: look.fg,
        primary: look.primary,
        alert: look.alert,
        charging: look.charging,
        now,
    };
    let build = |names: &[String]| -> Vec<Seg> { names.iter().flat_map(|n| modules::build(n, &ctx)).collect() };
    let mut left = build(&b.left);
    let mut center = build(&b.center);
    let mut right = build(&b.right);

    let (lx, cx, rx) = layout(&mut b.text, &mut left, &mut center, &mut right, w, h);
    let mut prims = Vec::new();
    let mut hits = Vec::new();
    place(&mut prims, &mut b.text, &left, lx, h, &mut hits);
    place(&mut prims, &mut b.text, &center, cx, h, &mut hits);
    place(&mut prims, &mut b.text, &right, rx, h, &mut hits);
    b.hits = hits;

    // полосами: буфер около 16 КБ (≈128 px при высоте 32), данные уходят без копии
    let strip = (4_096 / (h.max(1) as usize * 4)).clamp(16, w.max(16) as usize) as i32;
    let mut x0 = 0i32;
    while x0 < w {
        let cw = strip.min(w - x0);
        b.cv.reset(x0, cw, h, look.bg);
        b.cv.paint(&prims, &b.text);
        let len = cw as usize * h as usize * 4;
        let hdr = x::put_image_head(look.depth, b.id, b.gc, cw as u16, h as u16, x0 as i16, 0, len);
        x.send(&hdr);
        x.send_raw(b.cv.as_bytes());
        x0 += cw;
    }

}

fn run_cmd(argv: &[&str]) {
    sys::spawn(argv, false);
}

fn read_battery() -> Option<(u32, bool)> {
    let rd = |f: &str| sys::read_file(&format!("{BAT}/{f}")).map(|b| lossy(&b));
    let pct = rd("capacity")?.trim().parse::<u32>().ok()?;
    let chg = rd("status").map(|s| s.trim() == "Charging").unwrap_or(false);
    Some((pct, chg))
}

/// Подключение к сессионной шине: AUTH EXTERNAL, Hello, RequestName.
fn dbus_connect() -> Option<Sess> {
    let addr = sys::getenv("DBUS_SESSION_BUS_ADDRESS")?;
    let rest = addr.split(';').next()?.strip_prefix("unix:")?;
    let fd = if let Some(p) = rest.split(',').find_map(|kv| kv.strip_prefix("path=")) {
        sys::connect_unix(p, false)?
    } else {
        let p = rest.split(',').find_map(|kv| kv.strip_prefix("abstract="))?;
        sys::connect_unix(p, true)?
    };

    let uid: String = sys::geteuid().to_string().bytes().map(|b| format!("{b:02x}")).collect();
    if !sys::write_all(fd, format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes()) {
        return None;
    }
    // ответ -- одна строка "OK <guid>"
    let mut line = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if !sys::read_exact(fd, &mut b) {
            return None;
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
    }
    if !lossy(&line).trim().starts_with("OK") {
        return None;
    }
    if !sys::write_all(fd, b"BEGIN\r\n") {
        return None;
    }

    let mut s = Sess { bus: dbus::Bus::default(), fd, rbuf: Vec::new() };
    let (hello, m) = s.bus.hello();
    if !sys::write_all(fd, &m) {
        return None;
    }
    'wait: loop {
        let mut tmp = [0u8; 2048];
        match sys::read(fd, &mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => s.rbuf.extend_from_slice(&tmp[..n]),
        }
        while let Ok(Some(msg)) = s.next_msg() {
            if msg.mtype == 2 && msg.reply == hello {
                break 'wait;
            }
        }
    }
    let name = s.bus.request_name("org.kde.StatusNotifierWatcher");
    if !sys::write_all(fd, &name) {
        return None;
    }
    Some(s)
}

/// Монитор: имя (через GetAtomName), координаты. Отсортированы слева направо.
fn monitors(x: &mut X, root: u32) -> Vec<Mon> {
    let Some((randr, _)) = x.extension("RANDR") else { return Vec::new() };
    let Some(p) = x.call(&x::randr_get_monitors(randr, root, true)) else { return Vec::new() };
    let Some(list) = x::parse_monitors(&p) else { return Vec::new() };
    let mut v: Vec<Mon> = list
        .iter()
        .map(|m| {
            let name = x
                .call(&x::get_atom_name(m.name))
                .and_then(|r| x::parse_atom_name(&r).map(lossy))
                .unwrap_or_default();
            Mon { name, x: m.x, y: m.y, w: m.width }
        })
        .collect();
    v.sort_by_key(|m| (m.x, m.y));
    v
}

fn create_bars(x: &mut X, depth: u8, visual: u32, cfg: &Config, mons: &[Mon], bg: u32, at: Atoms) -> Vec<BarWin> {
    let mut bars = Vec::new();
    for (i, m) in mons.iter().enumerate() {
        let bar = cfg.bar(&m.name, i);
        let h = bar.height * 96 / 72;
        let id = x.id();
        let attrs = WinAttrs { background_pixel: Some(bg), event_mask: Some(EV_EXPOSE | EV_BUTTON | EV_MOTION), cursor: None };
        x.send(&x::create_window(depth, id, x.root, m.x, m.y, m.w, h as u16, 0, 1, visual, &attrs));
        // док и верхний отступ равный высоте бара (до map_window)
        let dock = x::DockAtoms { strut: at.strut_old, strut_partial: at.strut, window_type: at.wm_type, type_dock: at.dock };
        for req in x::reserve_top(id, &dock, h as u32, m.x as i32, m.y as i32, m.w as u32) {
            x.send(&req);
        }
        let gc = x.id();
        x.send(&x::create_gc(gc, id, None, None));
        x.send(&x::map_window(id));
        let Some(text) = load_text(&bar.fonts) else {
            die("odjkbar: ни один шрифт не загружен (проверь пути в fonts)")
        };
        bars.push(BarWin {
            id,
            gc,
            w: m.w,
            h: h as u16,
            mon: m.name.clone(),
            text,
            left: bar.left,
            center: bar.center,
            right: bar.right,
            hits: Vec::new(),
            cv: Canvas::new(),
            hand: false,
        });
    }
    bars
}

/// WM_NAME: годится только тип STRING (Latin-1). COMPOUND_TEXT и прочее с ESC-последовательностями
/// (ISO-2022) не декодируем: пустая строка лучше мусора.
fn wm_name_text(ty: u32, v: &[u8]) -> String {
    if ty == 31 && !v.contains(&0x1b) {
        v.iter().map(|&b| b as char).collect()
    } else {
        String::new()
    }
}

/// Заголовок окна синхронно (при старте).
fn sync_title(x: &mut X, win: u32, at: Atoms) -> String {
    if win == 0 {
        return String::new();
    }
    let read = |x: &mut X, prop: u32, ty: u32| -> String {
        x.call(&x::get_property(false, win, prop, ty, 0, 256))
            .and_then(|p| x::parse_get_property(&p).map(|(_, v)| lossy(v)))
            .unwrap_or_default()
    };
    let t = read(x, at.net_name, at.utf8);
    if !t.is_empty() {
        return t;
    }
    x.call(&x::get_property(false, win, WM_NAME, 0, 0, 256))
        .and_then(|p| x::parse_get_property(&p).map(|(ty, v)| wm_name_text(ty, v)))
        .unwrap_or_default()
}

/// Окно в фокусе из _NET_ACTIVE_WINDOW корневого окна (0, если его нет).
fn active_window_sync(x: &mut X, at: Atoms) -> u32 {
    x.call(&x::get_property(false, x.root, at.active, WINDOW, 0, 1))
        .and_then(|p| x::parse_get_property(&p).and_then(|(_, v)| x::u32_at(v, 0)))
        .unwrap_or(0)
}

/// Подписка на изменение свойств окна: xterm меняет заголовок на лету.
fn watch_window(x: &mut X, win: u32) {
    if win != 0 {
        x.send(&x::change_window_attributes(win, &WinAttrs { background_pixel: None, event_mask: Some(EV_PROPERTY), cursor: None }));
    }
}

/// Курсор-рука из встроенного шрифта "cursor" (для кликабельных мест). Возвращает (рука, стрелка).
/// Стрелка = 0 (None): окно наследует курсор корня, который задаётся темой (xsetroot -xcf).
fn make_cursors(x: &mut X) -> (u32, u32) {
    let font = x.id();
    x.send(&x::open_font(font, b"cursor"));
    let hand = x.id();
    // XC_hand2 = 60; маска -- следующий символ шрифта
    x.send(&x::create_glyph_cursor(hand, font, font, 60, 61, (0, 0, 0), (0xffff, 0xffff, 0xffff)));
    (hand, 0)
}

/// Всё состояние работающего бара.
struct App {
    x: X,
    cfg: Config,
    look: Look,
    at: Atoms,
    bars: Vec<BarWin>,
    st: State,
    sess: Option<Sess>,
    pend: BTreeMap<u16, Pend>,
    active_win: u32,
    /// код основного запроса XKB (0 -- расширения нет) и код её событий
    xkb: u8,
    xkb_ev: u8,
    hand_cur: u32,
    arrow_cur: u32,
}

impl App {
    fn redraw(&mut self) {
        for b in self.bars.iter_mut() {
            draw_bar(&mut self.x, b, &self.look, &self.st, &self.cfg);
        }
    }

    fn write_bus(&mut self, msgs: &[Vec<u8>]) {
        if let Some(s) = self.sess.as_ref() {
            for m in msgs {
                let _ = sys::write_all(s.fd, m);
            }
        }
    }

    fn run(&mut self, act: Action) {
        match act {
            Action::None => {}
            Action::ToggleDate => self.st.date_numeric = !self.st.date_numeric,
            Action::ToggleTime => self.st.time_mode = (self.st.time_mode + 1) % 2,
            Action::ToggleBattery => self.st.battery_collapsed = !self.st.battery_collapsed,
            Action::Activate(service, path) => {
                if let Some(s) = self.sess.as_mut() {
                    let msg = tray::activate(&mut s.bus, &service, &path);
                    let _ = sys::write_all(s.fd, &msg);
                }
            }
            Action::Layout(n) => {
                let next = (self.st.layout + 1) % n.max(1);
                self.x.send(&x::xkb_latch_lock_state(self.xkb, XKB_CORE_KBD, true, next as u8));
            }
            Action::Cmd(c) => run_cmd(&["sh", "-c", &c]),
            Action::Desktop(n) => run_cmd(&["bspc", "desktop", "-f", &n]),
        }
    }

    fn get_prop(&mut self, win: u32, prop: u32, ty: u32, pend: Pend) {
        let s = self.x.send(&x::get_property(false, win, prop, ty, 0, 256));
        self.pend.insert(s, pend);
    }

    /// Пакет от X-сервера (событие или ответ). true -- перерисовать.
    fn on_x(&mut self, p: &[u8]) -> bool {
        let root = self.x.root;
        let at = self.at;
        match x::parse_packet(p) {
            Some(Ev::Expose) => true,
            Some(Ev::Property { window, atom }) if window == root && atom == at.active => {
                let s = self.x.send(&x::get_property(false, root, at.active, WINDOW, 0, 1));
                self.pend.insert(s, Pend::Active);
                false
            }
            Some(Ev::Property { window, atom }) if window == self.active_win && (atom == at.net_name || atom == WM_NAME) => {
                // источник один: _NET_WM_NAME; WM_NAME читается только как запасной (см. TitleNet)
                self.get_prop(window, at.net_name, at.utf8, Pend::TitleNet(window));
                false
            }
            Some(Ev::Error) => {
                if let Some(seq) = x::u16_at(p, 2) {
                    self.pend.remove(&seq);
                }
                false
            }
            Some(Ev::Reply { seq }) => match self.pend.remove(&seq) {
                Some(Pend::Active) => {
                    let w = x::parse_get_property(p).and_then(|(_, v)| x::u32_at(v, 0)).unwrap_or(0);
                    if w != self.active_win {
                        self.active_win = w;
                        watch_window(&mut self.x, w);
                        self.get_prop(w, at.net_name, at.utf8, Pend::TitleNet(w));
                    }
                    false
                }
                Some(Pend::TitleNet(w)) => {
                    let t = x::parse_get_property(p).map(|(_, v)| lossy(v)).unwrap_or_default();
                    if w != self.active_win {
                        false
                    } else if t.is_empty() {
                        self.get_prop(w, WM_NAME, 0, Pend::TitleWm(w));
                        false
                    } else {
                        self.st.title = t;
                        true
                    }
                }
                Some(Pend::TitleWm(w)) => {
                    if w == self.active_win {
                        self.st.title = x::parse_get_property(p).map(|(ty, v)| wm_name_text(ty, v)).unwrap_or_default();
                        true
                    } else {
                        false
                    }
                }
                Some(Pend::Group) => match x::parse_xkb_group(p) {
                    Some(g) if g as usize != self.st.layout => {
                        self.st.layout = g as usize;
                        true
                    }
                    _ => false,
                },
                None => false,
            },
            Some(Ev::Button { detail, window, x: ex, y: _ }) => {
                let on_bar = self.bars.iter().any(|b| b.id == window);
                match detail {
                    // колёсико над баром: 4 -- следующий стол, 5 -- предыдущий
                    4 | 5 if on_bar => {
                        let dir = if detail == 4 { "next.local" } else { "prev.local" };
                        run_cmd(&["bspc", "desktop", "-f", dir]);
                        false
                    }
                    1 => {
                        let ex = ex as i32;
                        let act = self
                            .bars
                            .iter()
                            .find(|b| b.id == window)
                            .and_then(|b| b.hits.iter().find(|h| ex >= h.x0 && ex < h.x1))
                            .map(|h| h.act.clone());
                        match act {
                            Some(a) => {
                                self.run(a);
                                true
                            }
                            None => false,
                        }
                    }
                    _ => false,
                }
            }
            Some(Ev::Motion { window, x: ex, y: _ }) => {
                if let Some(b) = self.bars.iter_mut().find(|b| b.id == window) {
                    let ex = ex as i32;
                    let over = b.hits.iter().any(|h| ex >= h.x0 && ex < h.x1);
                    if over != b.hand {
                        b.hand = over;
                        let cur = if over { self.hand_cur } else { self.arrow_cur };
                        self.x.send(&x::change_window_attributes(b.id, &WinAttrs { background_pixel: None, event_mask: None, cursor: Some(cur) }));
                    }
                }
                false
            }
            // любое событие XKB -> перечитываем группу раскладки
            Some(Ev::Other(c)) if self.xkb != 0 && c == self.xkb_ev => {
                let s = self.x.send(&x::xkb_get_state(self.xkb, XKB_CORE_KBD));
                self.pend.insert(s, Pend::Group);
                false
            }
            _ => false,
        }
    }

    fn on_bus(&mut self, m: BusMsg) -> bool {
        let Some(s) = self.sess.as_mut() else { return false };
        let mut out = Vec::new();
        let d = self.st.tray.handle(&mut s.bus, &m, &mut out);
        self.write_bus(&out);
        d
    }

    fn on_minute(&mut self) {
        self.st.battery = read_battery();
        if let Some(s) = self.sess.as_mut() {
            let mut out = Vec::new();
            self.st.tray.refresh(&mut s.bus, &mut out);
            self.write_bus(&out);
        }
    }
}

/// Вывод `bspc subscribe report`: читающий конец канала и недочитанная строка.
struct Sub {
    fd: i32,
    buf: Vec<u8>,
}

fn real_main() {
    sys::init_signals();
    let cfg = load_config();
    let Some(mut x) = x_connect() else {
        die("odjkbar: нет X-сервера или не подходит авторизация ($DISPLAY, ~/.Xauthority)")
    };
    let root = x.root;
    let depth = x.root_depth;
    let visual = x.root_visual;
    let screen_w = x.width;

    let look = Look {
        bg: hex(&cfg.colors.background),
        fg: hex(&cfg.colors.foreground),
        primary: hex(&cfg.colors.primary),
        alert: hex(&cfg.colors.alert),
        charging: hex(&cfg.colors.charging),
        depth,
    };

    let at = Atoms {
        active: x.intern("_NET_ACTIVE_WINDOW"),
        net_name: x.intern("_NET_WM_NAME"),
        utf8: x.intern("UTF8_STRING"),
        wm_type: x.intern("_NET_WM_WINDOW_TYPE"),
        dock: x.intern("_NET_WM_WINDOW_TYPE_DOCK"),
        strut: x.intern("_NET_WM_STRUT_PARTIAL"),
        strut_old: x.intern("_NET_WM_STRUT"),
    };
    let (xkb, xkb_ev) = x.extension("XKEYBOARD").unwrap_or((0, 0));
    let mut layout = 0usize;
    if xkb != 0 {
        x.call(&x::xkb_use_extension(xkb, 1, 0));
        // события раскладки приходят в то же соединение, что и события баров
        x.send(&x::xkb_select_events(xkb, XKB_CORE_KBD, XKB_STATE_NOTIFY));
        if let Some(g) = x.call(&x::xkb_get_state(xkb, XKB_CORE_KBD)).and_then(|p| x::parse_xkb_group(&p)) {
            layout = g as usize;
        }
    }
    x.send(&x::change_window_attributes(root, &WinAttrs { background_pixel: None, event_mask: Some(EV_PROPERTY), cursor: None }));
    let (hand_cur, arrow_cur) = make_cursors(&mut x);

    let mut mons = monitors(&mut x, root);
    if mons.is_empty() {
        mons.push(Mon { name: String::new(), x: 0, y: 0, w: screen_w });
    }
    let bars = create_bars(&mut x, depth, visual, &cfg, &mons, look.bg, at);

    let active_win = active_window_sync(&mut x, at);
    watch_window(&mut x, active_win);

    let sess = dbus_connect();
    let st = State {
        title: sync_title(&mut x, active_win, at),
        battery: read_battery(),
        layout,
        tz: load_tz(),
        ..Default::default()
    };

    // bspc остаётся подпроцессом: протокол сокета bspwm здесь не проверен
    let mut sub = bspwm_subscribe().map(|fd| Sub { fd, buf: Vec::new() });

    let mut app = App {
        x,
        cfg,
        look,
        at,
        bars,
        st,
        sess,
        pend: BTreeMap::new(),
        active_win,
        xkb,
        xkb_ev,
        hand_cur,
        arrow_cur,
    };
    app.redraw();

    let mut next_min = (sys::now_ms() / 60_000 + 1) * 60_000;
    loop {
        let now = sys::now_ms();
        let timeout = next_min.saturating_sub(now).min(60_000) as i32;
        let mut fds = [
            PollFd { fd: app.x.fd, events: POLLIN, revents: 0 },
            PollFd { fd: app.sess.as_ref().map_or(-1, |s| s.fd), events: POLLIN, revents: 0 },
            PollFd { fd: sub.as_ref().map_or(-1, |s| s.fd), events: POLLIN, revents: 0 },
        ];
        // timeout 0 после пробуждения -- минута наступила, ниже её обработка
        sys::poll(&mut fds, timeout);
        let mut dirty = false;
        let ready = POLLIN | POLLHUP | POLLERR;

        if fds[0].revents & ready != 0 {
            if !app.x.fill() {
                break;
            }
            while let Some(p) = app.x.take_pkt() {
                dirty |= app.on_x(&p);
            }
        }

        if fds[1].revents & ready != 0 {
            let mut alive = true;
            if let Some(s) = app.sess.as_mut() {
                let mut tmp = [0u8; 4096];
                match sys::read(s.fd, &mut tmp) {
                    Ok(0) | Err(_) => alive = false,
                    Ok(n) => s.rbuf.extend_from_slice(&tmp[..n]),
                }
            }
            while alive {
                let Some(s) = app.sess.as_mut() else { break };
                match s.next_msg() {
                    Ok(Some(m)) => dirty |= app.on_bus(m),
                    Ok(None) => break,
                    Err(()) => alive = false,
                }
            }
            if !alive {
                if let Some(s) = app.sess.take() {
                    sys::close(s.fd);
                }
            }
        }

        if fds[2].revents & ready != 0 {
            let mut done = false;
            if let Some(s) = sub.as_mut() {
                let mut tmp = [0u8; 2048];
                match sys::read(s.fd, &mut tmp) {
                    Ok(0) | Err(_) => done = true,
                    Ok(n) => {
                        s.buf.extend_from_slice(&tmp[..n]);
                        while let Some(nl) = s.buf.iter().position(|&b| b == b'\n') {
                            let line: Vec<u8> = s.buf.drain(..=nl).collect();
                            modules::parse_report(&lossy(&line[..nl]), &mut app.st.desktops);
                            dirty = true;
                        }
                    }
                }
            }
            if done {
                if let Some(s) = sub.take() {
                    sys::close(s.fd);
                }
            }
        }

        if sys::now_ms() >= next_min {
            next_min = (sys::now_ms() / 60_000 + 1) * 60_000;
            app.on_minute();
            dirty = true;
        }

        // буферы после разбора: их ёмкость держит пик, отдаём лишнее
        trim(&mut app.x.rbuf);
        if let Some(s) = app.sess.as_mut() {
            trim(&mut s.rbuf);
        }
        if let Some(s) = sub.as_mut() {
            trim(&mut s.buf);
        }

        if dirty {
            app.redraw();
        }
    }
}
