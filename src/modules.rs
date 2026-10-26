//! Модули панели: что рисовать и что делать по клику. Без std.

use crate::clock::Tm;
use crate::config::{hex, Config};
use crate::tray::{Icon, Tray};
use alloc::{collections::BTreeMap, format, rc::Rc, string::{String, ToString}, vec, vec::Vec};

pub const MARGIN: i32 = 8;
const RU_WD: [&str; 7] = ["вс", "пн", "вт", "ср", "чт", "пт", "сб"];
const RU_MON: [&str; 12] = [
    "января", "февраля", "марта", "апреля", "мая", "июня",
    "июля", "августа", "сентября", "октября", "ноября", "декабря",
];
const EN_WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const EN_MON: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

#[derive(Clone)]
pub enum Action {
    None,
    Cmd(String),
    ToggleDate,
    ToggleBattery,
    Desktop(String),
    ToggleTime,
    Layout(usize),
    Activate(String, String),
}

pub enum Seg {
    /// flex: ширина подгоняется под свободное место (название окна), см. layout в бинаре
    Text { s: String, fg: u32, act: Action, flex: bool },
    /// рабочий стол: звёздочка (если выбран) в ведущем слоте, затем название
    Desk { name: String, star: bool, fg: u32, act: Action, last: bool },
    /// иконка трея: квадрат со стороной в высоту панели
    Image { icon: Rc<Icon>, act: Action },
    Sep { w: i32, block: i32, color: u32 },
}

#[derive(Default)]
pub struct State {
    pub title: String,
    pub layout: usize,
    /// bspwm: имя монитора -> [(имя десктопа, состояние)]
    pub desktops: BTreeMap<String, Vec<(String, char)>>,
    pub battery: Option<(u32, bool)>,
    pub battery_collapsed: bool,
    pub date_numeric: bool,
    /// 0 -- 24 часа, 1 -- 12 с AM/PM
    pub time_mode: u8,
    pub tz: crate::clock::Tz,
    pub tray: Tray,
}

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub st: &'a State,
    pub mon: &'a str,
    pub fg: u32,
    pub primary: u32,
    pub alert: u32,
    pub charging: u32,
    pub now: Tm,
}

pub fn build(name: &str, c: &Ctx) -> Vec<Seg> {
    if name.starts_with("sep") {
        let m = c.cfg.module("sep");
        let block = m.width.unwrap_or(3);
        let color = m.color.as_deref().map(hex).unwrap_or(0xe61a99);
        return vec![Seg::Sep { w: block + 2 * MARGIN, block, color }];
    }
    let m = c.cfg.module(name);
    let text = |s: String, fg: u32, act: Action| vec![Seg::Text { s, fg, act, flex: false }];
    match name {
        "launcher" => text(
            m.text.unwrap_or_default(),
            c.fg,
            Action::Cmd(m.click.unwrap_or_else(|| "rofi -show drun -show-icons".to_string())),
        ),
        "xwindow" => {
            // название занимает всё свободное место и обрезается в раскладке (flex);
            // max_len, если задан в конфиге, только ограничивает сверху
            let s = match m.max_len {
                Some(max) if c.st.title.chars().count() > max => {
                    let mut s: String = c.st.title.chars().take(max).collect();
                    s.push('…');
                    s
                }
                _ => c.st.title.clone(),
            };
            vec![Seg::Text {
                s,
                fg: c.fg,
                act: Action::Cmd(m.click.unwrap_or_else(|| "rofi -show window -show-icons".to_string())),
                flex: true,
            }]
        }
        "time" => text(time_text(&c.now, c.st.time_mode), c.fg, Action::ToggleTime),
        "date" => text(date_text(&c.now, c.st.date_numeric, c.cfg.is_en()), c.fg, Action::ToggleDate),
        "stol" => c
            .st
            .desktops
            .get(c.mon)
            .map(|list| {
                let n = list.len();
                list.iter()
                    .enumerate()
                    .map(|(idx, (name, k))| {
                        let k = *k;
                        let focused = k.is_uppercase();
                        let fg = if matches!(k, 'U' | 'u') { c.alert } else { c.fg };
                        Seg::Desk {
                            name: name.clone(),
                            star: focused,
                            fg,
                            act: Action::Desktop(name.clone()),
                            last: idx + 1 == n,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
        "xkeyboard" => {
            let layouts = m
                .layouts
                .unwrap_or_else(|| vec!["us;ENG".to_string(), "ru;РУС".to_string()]);
            let i = c.st.layout % layouts.len().max(1);
            let label = layouts
                .get(i)
                .map(|e| e.split(';').nth(1).unwrap_or(e).to_string())
                .unwrap_or_default();
            text(label, c.fg, Action::Layout(layouts.len()))
        }
        "battery" => match c.st.battery {
            None => vec![],
            Some((pct, chg)) => {
                let fg = if chg {
                    c.charging
                } else if pct <= 10 {
                    c.alert
                } else {
                    c.fg
                };
                let label = if c.st.battery_collapsed { String::new() } else { format!(" {pct}%") };
                text(format!("{}{}", battery_icon(pct, chg), label), fg, Action::ToggleBattery)
            }
        },
        "tray" => c
            .st
            .tray
            .items
            .iter()
            .filter_map(|it| {
                it.icon.as_ref().map(|icon| Seg::Image {
                    icon: icon.clone(),
                    act: Action::Activate(it.service.clone(), it.path.clone()),
                })
            })
            .collect(),
        _ => text(
            m.text.unwrap_or_default(),
            c.fg,
            m.click.map_or(Action::None, Action::Cmd),
        ),
    }
}

/// 24 часа или 12 с AM/PM (режим 1).
fn time_text(t: &Tm, mode: u8) -> String {
    match mode {
        1 => {
            let h12 = match t.hour % 12 {
                0 => 12,
                h => h,
            };
            format!("{:02}:{:02} {}", h12, t.min, if t.hour < 12 { "AM" } else { "PM" })
        }
        _ => format!("{:02}:{:02}", t.hour, t.min),
    }
}

/// Числовая дата или "пт, 23 октябрь" (ru) / "Fri, 23 October" (en).
fn date_text(t: &Tm, numeric: bool, en: bool) -> String {
    if numeric {
        return format!("{:02}.{:02}.{}", t.day, t.month, t.year);
    }
    let m = (t.month as usize).clamp(1, 12) - 1;
    if en {
        format!("{}, {} {}", EN_WD[t.wday], t.day, EN_MON[m])
    } else {
        format!("{}, {} {}", RU_WD[t.wday], t.day, RU_MON[m])
    }
}

fn battery_icon(pct: u32, chg: bool) -> &'static str {
    let (i, ci) = match pct {
        0..=10 => ("󰁺", "󰁺󱐋"),
        11..=30 => ("󰁽", "󰁽󱐋"),
        31..=55 => ("󰁿", "󰁿󱐋"),
        56..=80 => ("󰂂", "󰂂󱐋"),
        _ => ("󰁹", "󰁹󱐋"),
    };
    if chg { ci } else { i }
}

/// Вывод `bspc subscribe report`, например "WMDP-1:Oweb:f2:Ltiled:..."
/// Заглавная буква = сфокусирован, U/u = urgent.
pub fn parse_report(line: &str, out: &mut BTreeMap<String, Vec<(String, char)>>) {
    out.clear();
    let mut cur = String::new();
    for tok in line.trim_start_matches('W').split(':') {
        let mut ch = tok.chars();
        let Some(k) = ch.next() else { continue };
        match k {
            'M' | 'm' => {
                cur = ch.collect();
                out.entry(cur.clone()).or_default();
            }
            'O' | 'o' | 'F' | 'f' | 'U' | 'u' => {
                out.entry(cur.clone()).or_default().push((ch.collect(), k));
            }
            _ => {}
        }
    }
}
