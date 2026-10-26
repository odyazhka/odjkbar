//! Трей: хост StatusNotifierWatcher и иконки приложений (IconPixmap). Без std.

use crate::dbus::{put_str, put_u32, Bus, Msg, Rd};
use alloc::{collections::BTreeMap, format, rc::Rc, string::String, vec::Vec};

const WATCHER: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";

/// Иконка: пиксели в формате 0xAARRGGBB.
pub struct Icon {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
}

pub struct Item {
    pub service: String,
    pub path: String,
    pub icon: Option<Rc<Icon>>,
}

#[derive(Default)]
pub struct Tray {
    pub items: Vec<Item>,
    /// serial запроса IconPixmap -> имя сервиса приложения
    pending: BTreeMap<u32, String>,
}

/// Разбор варианта a(iiay): берёт самую большую иконку.
fn parse_icon(body: &[u8]) -> Option<Icon> {
    let mut r = Rd::new(body);
    if r.sig()? != "a(iiay)" {
        return None;
    }
    let len = r.u32()? as usize;
    r.align(8);
    let end = r.i + len;
    let mut best: Option<Icon> = None;
    while r.i < end {
        r.align(8);
        let w = r.i32()?;
        let h = r.i32()?;
        let n = r.u32()? as usize;
        let data = r.b.get(r.i..r.i + n)?;
        r.i += n;
        if w <= 0 || h <= 0 || data.len() != (w * h * 4) as usize {
            continue;
        }
        // данные приходят в сетевом порядке: A, R, G, B
        let px: Vec<u32> = data
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        if best.as_ref().map_or(true, |b| w > b.w) {
            best = Some(Icon { w, h, px });
        }
    }
    best
}

/// Тело варианта: сигнатура значения, затем само значение.
fn variant(sig: &str, val: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(sig.len() as u8);
    b.extend_from_slice(sig.as_bytes());
    b.push(0);
    val(&mut b);
    b
}

fn request_icon(bus: &mut Bus, t: &mut Tray, out: &mut Vec<Vec<u8>>, service: &str, path: &str) {
    let mut b = Vec::new();
    put_str(&mut b, ITEM_IFACE);
    put_str(&mut b, "IconPixmap");
    let (serial, m) = bus.call(service, path, PROPS_IFACE, "Get", "ss", &b);
    t.pending.insert(serial, String::from(service));
    out.push(m);
}

impl Tray {
    fn get_prop(&self, iface: &str, prop: &str) -> Option<Vec<u8>> {
        match (iface, prop) {
            (WATCHER_IFACE, "IsStatusNotifierHostRegistered") => Some(variant("b", |b| put_u32(b, 1))),
            (WATCHER_IFACE, "ProtocolVersion") => Some(variant("i", |b| put_u32(b, 0))),
            (WATCHER_IFACE, "RegisteredStatusNotifierItems") => {
                let names: Vec<String> = self.items.iter().map(|i| format!("{}{}", i.service, i.path)).collect();
                Some(variant("as", |b| {
                    let mut items = Vec::new();
                    for n in &names {
                        put_str(&mut items, n);
                    }
                    put_u32(b, items.len() as u32);
                    b.extend_from_slice(&items);
                }))
            }
            _ => None,
        }
    }

    fn method_call(&mut self, bus: &mut Bus, m: &Msg, out: &mut Vec<Vec<u8>>) -> bool {
        if m.path != WATCHER {
            return false;
        }
        match (m.iface.as_str(), m.member.as_str()) {
            (WATCHER_IFACE, "RegisterStatusNotifierItem") => {
                let arg = Rd::new(&m.body).str().unwrap_or_default();
                // аргумент -- путь объекта (тогда сервис = отправитель) или имя сервиса
                let (service, path) = if arg.starts_with('/') {
                    (m.sender.clone(), arg)
                } else {
                    (arg, String::from("/StatusNotifierItem"))
                };
                out.push(bus.reply(m, "", &[]));
                if !self.items.iter().any(|i| i.service == service && i.path == path) {
                    self.items.push(Item { service: service.clone(), path: path.clone(), icon: None });
                    request_icon(bus, self, out, &service, &path);
                }
                false
            }
            (WATCHER_IFACE, "RegisterStatusNotifierHost") => {
                out.push(bus.reply(m, "", &[]));
                false
            }
            (PROPS_IFACE, "Get") => {
                let mut r = Rd::new(&m.body);
                let iface = r.str().unwrap_or_default();
                let prop = r.str().unwrap_or_default();
                if let Some(body) = self.get_prop(&iface, &prop) {
                    out.push(bus.reply(m, "v", &body));
                }
                false
            }
            _ => false,
        }
    }

    /// Обрабатывает входящее сообщение. Ответы кладёт в `out`. true -- перерисовать.
    pub fn handle(&mut self, bus: &mut Bus, m: &Msg, out: &mut Vec<Vec<u8>>) -> bool {
        match m.mtype {
            1 => self.method_call(bus, m, out),
            2 => {
                // ответ на IconPixmap
                let Some(service) = self.pending.remove(&m.reply) else { return false };
                let icon = parse_icon(&m.body).map(Rc::new);
                match self.items.iter_mut().find(|i| i.service == service) {
                    Some(it) => {
                        it.icon = icon;
                        true
                    }
                    None => false,
                }
            }
            3 => {
                // ошибка: у приложения нет IconPixmap или оно уже закрыто
                let Some(service) = self.pending.remove(&m.reply) else { return false };
                match self.items.iter_mut().find(|i| i.service == service) {
                    Some(it) => {
                        let had = it.icon.is_some();
                        it.icon = None;
                        had
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// Раз в минуту запрашивает иконки всех приложений.
    pub fn refresh(&mut self, bus: &mut Bus, out: &mut Vec<Vec<u8>>) {
        let list: Vec<(String, String)> = self.items.iter().map(|i| (i.service.clone(), i.path.clone())).collect();
        for (s, p) in list {
            request_icon(bus, self, out, &s, &p);
        }
    }
}

/// Клик по иконке: вызов Activate у приложения. Возвращает байты сообщения.
pub fn activate(bus: &mut Bus, service: &str, path: &str) -> Vec<u8> {
    bus.call(service, path, ITEM_IFACE, "Activate", "ii", &[0u8; 8]).1
}
