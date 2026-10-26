//! Конфиг без std, serde и toml: собственный разбор нужного подмножества TOML.
//! Поддержано: [таблицы] и [a.b], ключи (в т.ч. в кавычках и через точку), строки
//! ("..", '..', """..""", '''..'''), целые, массивы (в т.ч. многострочные), комментарии.
//! Остальное (числа с точкой, даты, булевы, inline-таблицы) читается и пропускается.

use alloc::{collections::BTreeMap, string::{String, ToString}, vec::Vec};
use core::fmt;

#[derive(Clone)]
pub struct Colors {
    pub background: String,
    pub foreground: String,
    pub primary: String,
    pub alert: String,
    pub charging: String,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: "#000000".into(),
            foreground: "#ffffff".into(),
            primary: "#fc00ef".into(),
            alert: "#f53c3c".into(),
            charging: "#26a65b".into(),
        }
    }
}

/// Поля секции бара; None означает "взять из [bar.default]".
#[derive(Clone, Default)]
pub struct BarCfg {
    pub height: Option<u32>,
    /// "ПУТЬ:size=PT;ПОДЪЁМ", например "/usr/share/fonts/x.ttf:size=18;3".
    pub fonts: Option<Vec<String>>,
    pub left: Option<Vec<String>>,
    pub center: Option<Vec<String>>,
    pub right: Option<Vec<String>>,
}

#[derive(Clone, Default)]
pub struct ModCfg {
    pub text: Option<String>,
    pub click: Option<String>,
    pub width: Option<i32>,
    pub color: Option<String>,
    pub max_len: Option<usize>,
    pub layouts: Option<Vec<String>>,
}

#[derive(Default)]
pub struct Config {
    pub colors: Colors,
    pub bar: BTreeMap<String, BarCfg>,
    pub modules: BTreeMap<String, ModCfg>,
    /// "ru" или "en" (для даты). Пусто -- ru.
    pub locale: String,
    /// Фиксированное смещение от UTC в минутах (Москва: 180). Если не задано --
    /// берётся локальный часовой пояс системы (TZ или /etc/localtime).
    pub utc_offset_min: Option<i32>,
}

pub struct Bar {
    pub height: u32,
    pub fonts: Vec<String>,
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
}

/// Ошибка разбора: номер строки (с 1) и причина.
#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub msg: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "строка {}: {}", self.line, self.msg)
    }
}

enum Val {
    Str(String),
    Int(i64),
    Arr(Vec<Val>),
    Other,
}

fn strs(v: Val) -> Option<Vec<String>> {
    match v {
        Val::Arr(a) => Some(
            a.into_iter()
                .filter_map(|x| if let Val::Str(s) = x { Some(s) } else { None })
                .collect(),
        ),
        _ => None,
    }
}

fn string(v: Val) -> Option<String> {
    if let Val::Str(s) = v { Some(s) } else { None }
}

fn int(v: &Val) -> Option<i64> {
    if let Val::Int(n) = v { Some(*n) } else { None }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
    line: usize,
}

type R<T> = Result<T, ParseError>;

impl<'a> P<'a> {
    fn err<T>(&self, msg: &'static str) -> R<T> {
        Err(ParseError { line: self.line, msg })
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn starts(&self, pat: &[u8]) -> bool {
        self.s[self.i..].starts_with(pat)
    }

    /// Пробелы и табы.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.i += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            while !matches!(self.peek(), None | Some(b'\n')) {
                self.i += 1;
            }
        }
    }

    /// Пробелы, переводы строк и комментарии.
    fn skip_all(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r') => self.i += 1,
                Some(b'\n') => {
                    self.i += 1;
                    self.line += 1;
                }
                Some(b'#') => self.skip_comment(),
                _ => break,
            }
        }
    }

    fn end_of_line(&mut self) -> R<()> {
        self.skip_ws();
        self.skip_comment();
        match self.peek() {
            None => Ok(()),
            Some(b'\r') => {
                self.i += 1;
                self.end_of_line()
            }
            Some(b'\n') => {
                self.i += 1;
                self.line += 1;
                Ok(())
            }
            _ => self.err("лишние символы после значения"),
        }
    }

    fn expect(&mut self, c: u8, msg: &'static str) -> R<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            self.err(msg)
        }
    }

    fn key_path(&mut self) -> R<Vec<String>> {
        let mut path = Vec::new();
        loop {
            self.skip_ws();
            let key = match self.peek() {
                Some(b'"') => self.basic_string()?,
                Some(b'\'') => self.literal_string()?,
                Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' => {
                    let st = self.i;
                    while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
                        self.i += 1;
                    }
                    String::from_utf8_lossy(&self.s[st..self.i]).into_owned()
                }
                _ => return self.err("ожидался ключ"),
            };
            path.push(key);
            self.skip_ws();
            if self.peek() == Some(b'.') {
                self.i += 1;
            } else {
                return Ok(path);
            }
        }
    }

    fn literal_string(&mut self) -> R<String> {
        let multi = self.starts(b"'''");
        self.i += if multi { 3 } else { 1 };
        if multi && self.peek() == Some(b'\r') {
            self.i += 1;
        }
        if multi && self.peek() == Some(b'\n') {
            self.i += 1;
            self.line += 1;
        }
        let st = self.i;
        loop {
            match self.peek() {
                None => return self.err("не закрыта строка"),
                Some(b'\n') if !multi => return self.err("не закрыта строка"),
                Some(b'\n') => {
                    self.line += 1;
                    self.i += 1;
                }
                Some(b'\'') if !multi => break,
                Some(b'\'') if self.starts(b"'''") => break,
                _ => self.i += 1,
            }
        }
        let v = String::from_utf8_lossy(&self.s[st..self.i]).into_owned();
        self.i += if multi { 3 } else { 1 };
        Ok(v)
    }

    fn basic_string(&mut self) -> R<String> {
        let multi = self.starts(b"\"\"\"");
        self.i += if multi { 3 } else { 1 };
        if multi && self.peek() == Some(b'\r') {
            self.i += 1;
        }
        if multi && self.peek() == Some(b'\n') {
            self.i += 1;
            self.line += 1;
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else { return self.err("не закрыта строка") };
            match c {
                b'"' if !multi => {
                    self.i += 1;
                    break;
                }
                b'"' if self.starts(b"\"\"\"") => {
                    self.i += 3;
                    break;
                }
                b'\n' if !multi => return self.err("не закрыта строка"),
                b'\n' => {
                    self.line += 1;
                    out.push(c);
                    self.i += 1;
                }
                b'\\' => {
                    self.i += 1;
                    let Some(e) = self.peek() else { return self.err("обрыв escape-последовательности") };
                    self.i += 1;
                    match e {
                        b'b' => out.push(8),
                        b't' => out.push(b'\t'),
                        b'n' => out.push(b'\n'),
                        b'f' => out.push(12),
                        b'r' => out.push(b'\r'),
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'u' | b'U' => {
                            let n = if e == b'u' { 4 } else { 8 };
                            let Some(h) = self.s.get(self.i..self.i + n) else {
                                return self.err("короткий \\u");
                            };
                            let cp = core::str::from_utf8(h).ok().and_then(|t| u32::from_str_radix(t, 16).ok());
                            let Some(ch) = cp.and_then(char::from_u32) else {
                                return self.err("неверный \\u");
                            };
                            self.i += n;
                            let mut b = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                        }
                        b' ' | b'\t' | b'\r' | b'\n' if multi => {
                            // обратная косая в конце строки: съедает пробелы и переводы строк
                            self.i -= 1;
                            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                                if self.peek() == Some(b'\n') {
                                    self.line += 1;
                                }
                                self.i += 1;
                            }
                        }
                        _ => return self.err("неизвестный escape"),
                    }
                }
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn value(&mut self) -> R<Val> {
        match self.peek() {
            Some(b'"') => Ok(Val::Str(self.basic_string()?)),
            Some(b'\'') => Ok(Val::Str(self.literal_string()?)),
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                loop {
                    self.skip_all();
                    if self.peek() == Some(b']') {
                        self.i += 1;
                        break;
                    }
                    a.push(self.value()?);
                    self.skip_all();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            break;
                        }
                        _ => return self.err("ожидалась , или ] в массиве"),
                    }
                }
                Ok(Val::Arr(a))
            }
            Some(b'{') => {
                self.i += 1;
                loop {
                    self.skip_all();
                    if self.peek() == Some(b'}') {
                        self.i += 1;
                        break;
                    }
                    self.key_path()?;
                    self.skip_ws();
                    self.expect(b'=', "ожидалось = в inline-таблице")?;
                    self.skip_ws();
                    self.value()?;
                    self.skip_all();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return self.err("ожидалась , или } в inline-таблице"),
                    }
                }
                Ok(Val::Other)
            }
            Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.') => {
                let st = self.i;
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.' | b'_' | b':')) {
                    self.i += 1;
                }
                let tok = core::str::from_utf8(&self.s[st..self.i]).unwrap_or("");
                let clean: String = tok.chars().filter(|&c| c != '_').collect();
                let num = if let Some(h) = clean.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).ok()
                } else if let Some(o) = clean.strip_prefix("0o") {
                    i64::from_str_radix(o, 8).ok()
                } else if let Some(b) = clean.strip_prefix("0b") {
                    i64::from_str_radix(b, 2).ok()
                } else {
                    clean.parse::<i64>().ok()
                };
                Ok(num.map_or(Val::Other, Val::Int))
            }
            _ => self.err("ожидалось значение"),
        }
    }
}

impl Config {
    /// Разбор TOML из памяти; чтение файла делает бинарь.
    pub fn parse(src: &str) -> Result<Self, ParseError> {
        let mut cfg = Config::default();
        let mut p = P { s: src.as_bytes(), i: 0, line: 1 };
        // BOM
        if p.starts(&[0xEF, 0xBB, 0xBF]) {
            p.i = 3;
        }
        let mut table: Vec<String> = Vec::new();
        loop {
            p.skip_all();
            let Some(c) = p.peek() else { break };
            if c == b'[' {
                p.i += 1;
                let array = p.peek() == Some(b'[');
                if array {
                    p.i += 1;
                }
                let path = p.key_path()?;
                p.skip_ws();
                p.expect(b']', "ожидалась ]")?;
                if array {
                    p.expect(b']', "ожидалась ]]")?;
                }
                table = path;
                p.end_of_line()?;
            } else {
                let mut key = p.key_path()?;
                p.skip_ws();
                p.expect(b'=', "ожидалось =")?;
                p.skip_ws();
                let v = p.value()?;
                p.end_of_line()?;
                let mut full = table.clone();
                full.append(&mut key);
                cfg.set(&full, v);
            }
        }
        Ok(cfg)
    }

    fn set(&mut self, path: &[String], v: Val) {
        let p: Vec<&str> = path.iter().map(|s| s.as_str()).collect();
        match p.as_slice() {
            ["locale"] => {
                if let Some(s) = string(v) {
                    self.locale = s;
                }
            }
            ["utc_offset_min"] => {
                if let Some(n) = int(&v) {
                    self.utc_offset_min = Some(n as i32);
                }
            }
            ["colors", k] => {
                let Some(s) = string(v) else { return };
                match *k {
                    "background" => self.colors.background = s,
                    "foreground" => self.colors.foreground = s,
                    "primary" => self.colors.primary = s,
                    "alert" => self.colors.alert = s,
                    "charging" => self.colors.charging = s,
                    _ => {}
                }
            }
            ["bar", name, k] => {
                let b = self.bar.entry(name.to_string()).or_default();
                match *k {
                    "height" => b.height = int(&v).and_then(|n| u32::try_from(n).ok()),
                    "fonts" => b.fonts = strs(v),
                    "left" => b.left = strs(v),
                    "center" => b.center = strs(v),
                    "right" => b.right = strs(v),
                    _ => {}
                }
            }
            ["modules", name, k] => {
                let m = self.modules.entry(name.to_string()).or_default();
                match *k {
                    "text" => m.text = string(v),
                    "click" => m.click = string(v),
                    "color" => m.color = string(v),
                    "width" => m.width = int(&v).and_then(|n| i32::try_from(n).ok()),
                    "max_len" => m.max_len = int(&v).and_then(|n| usize::try_from(n).ok()),
                    "layouts" => m.layouts = strs(v),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Бар для монитора: секция [bar.ИМЯ] (например [bar.HDMI-1]) с наложением на [bar.default].
    /// Если имя монитора неизвестно (нет RandR), используется секция [bar.ИНДЕКС].
    pub fn bar(&self, mon: &str, idx: usize) -> Bar {
        let d = self.bar.get("default").cloned().unwrap_or_default();
        let key = if mon.is_empty() { idx.to_string() } else { mon.to_string() };
        let o = self.bar.get(&key).cloned().unwrap_or_default();
        Bar {
            height: o.height.or(d.height).unwrap_or(24),
            fonts: o.fonts.or(d.fonts).unwrap_or_default(),
            left: o.left.or(d.left).unwrap_or_default(),
            center: o.center.or(d.center).unwrap_or_default(),
            right: o.right.or(d.right).unwrap_or_default(),
        }
    }

    pub fn module(&self, name: &str) -> ModCfg {
        self.modules.get(name).cloned().unwrap_or_default()
    }

    pub fn is_en(&self) -> bool {
        self.locale == "en"
    }
}

/// "#RRGGBB" -> 0xRRGGBB
pub fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0)
}
