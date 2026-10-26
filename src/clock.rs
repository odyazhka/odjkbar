//! Календарь без std: из unix-времени (секунды UTC) и смещения в минутах.

#[derive(Clone, Copy, Default)]
pub struct Tm {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    /// 0 = воскресенье
    pub wday: usize,
}

pub fn civil(unix: i64, offset_min: i32) -> Tm {
    let t = unix + offset_min as i64 * 60;
    let days = t.div_euclid(86400);
    let secs = t.rem_euclid(86400);
    // алгоритм даты по дням от эпохи (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    Tm {
        year,
        month: m as u32,
        day,
        hour: (secs / 3600) as u32,
        min: ((secs % 3600) / 60) as u32,
        wday: (days + 4).rem_euclid(7) as usize,
    }
}

// ---------- локальный часовой пояс (TZif + POSIX TZ), без libc ----------

use alloc::{string::String, vec::Vec};

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_len(y: i64, m: u32) -> u32 {
    match m {
        2 => if is_leap(y) { 29 } else { 28 },
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Момент перехода внутри года: Mm.w.d, Jn или n, плюс время суток в секундах.
#[derive(Clone, Copy)]
enum Day {
    /// месяц, неделя (5 = последняя), день недели (0 = воскресенье)
    M(u32, u32, u32),
    /// 1..=365, 29 февраля не считается
    J(u32),
    /// 0..=365, 29 февраля считается
    N(u32),
}

#[derive(Clone, Copy)]
struct Trans {
    day: Day,
    time: i32,
}

impl Trans {
    /// Локальные секунды от эпохи (без поправки на пояс) для года y.
    fn local_secs(&self, y: i64) -> i64 {
        let days = match self.day {
            Day::M(m, w, d) => {
                let wd = (days_from_civil(y, m, 1) + 4).rem_euclid(7) as u32;
                let mut day = 1 + (d + 7 - wd) % 7 + 7 * (w - 1);
                while day > month_len(y, m) {
                    day -= 7;
                }
                days_from_civil(y, m, day)
            }
            Day::J(n) => days_from_civil(y, 1, 1) + n as i64 - 1 + if is_leap(y) && n >= 60 { 1 } else { 0 },
            Day::N(n) => days_from_civil(y, 1, 1) + n as i64,
        };
        days * 86400 + self.time as i64
    }
}

/// Правило вида "CET-1CEST,M3.5.0,M10.5.0/3". Смещения в секундах к востоку от UTC.
#[derive(Clone, Copy)]
struct Rule {
    std: i32,
    dst: Option<(i32, Trans, Trans)>,
}

impl Rule {
    fn offset(&self, unix: i64) -> i32 {
        let Some((dst, start, end)) = self.dst else { return self.std };
        let y = civil(unix, (self.std / 60) as i32).year;
        let s = start.local_secs(y) - self.std as i64;
        let e = end.local_secs(y) - dst as i64;
        let in_dst = if s < e { unix >= s && unix < e } else { !(unix >= e && unix < s) };
        if in_dst { dst } else { self.std }
    }
}

struct Cur<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn name(&mut self) -> Option<()> {
        if self.eat(b'<') {
            while self.peek()? != b'>' {
                self.i += 1;
            }
            self.i += 1;
            return Some(());
        }
        let st = self.i;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.i += 1;
        }
        if self.i - st >= 3 { Some(()) } else { None }
    }
    fn num(&mut self) -> Option<i32> {
        let st = self.i;
        let mut n = 0i32;
        while let Some(c) = self.peek().filter(|c| c.is_ascii_digit()) {
            n = n.checked_mul(10)?.checked_add((c - b'0') as i32)?;
            self.i += 1;
        }
        if self.i > st { Some(n) } else { None }
    }
    /// [+-]hh[:mm[:ss]] в секундах со знаком.
    fn hms(&mut self) -> Option<i32> {
        let neg = if self.eat(b'-') {
            true
        } else {
            self.eat(b'+');
            false
        };
        let mut t = self.num()? * 3600;
        if self.eat(b':') {
            t += self.num()? * 60;
            if self.eat(b':') {
                t += self.num()?;
            }
        }
        Some(if neg { -t } else { t })
    }
    fn trans(&mut self) -> Option<Trans> {
        let day = if self.eat(b'M') {
            let m = self.num()? as u32;
            self.eat(b'.').then_some(())?;
            let w = self.num()? as u32;
            self.eat(b'.').then_some(())?;
            let d = self.num()? as u32;
            if !(1..=12).contains(&m) || !(1..=5).contains(&w) || d > 6 {
                return None;
            }
            Day::M(m, w, d)
        } else if self.eat(b'J') {
            Day::J(self.num()? as u32)
        } else {
            Day::N(self.num()? as u32)
        };
        let time = if self.eat(b'/') { self.hms()? } else { 7200 };
        Some(Trans { day, time })
    }
}

fn parse_rule(s: &str) -> Option<Rule> {
    let mut c = Cur { s: s.as_bytes(), i: 0 };
    c.name()?;
    // в POSIX положительное смещение означает запад от UTC
    let std = -c.hms()?;
    if c.peek().is_none() {
        return Some(Rule { std, dst: None });
    }
    c.name()?;
    let dst = if matches!(c.peek(), Some(b',') | None) { std + 3600 } else { -c.hms()? };
    let (start, end) = if c.eat(b',') {
        let a = c.trans()?;
        c.eat(b',').then_some(())?;
        (a, c.trans()?)
    } else {
        // правило по умолчанию (США)
        (Trans { day: Day::M(3, 2, 0), time: 7200 }, Trans { day: Day::M(11, 1, 0), time: 7200 })
    };
    Some(Rule { std, dst: Some((dst, start, end)) })
}

/// Часовой пояс: таблица переходов из TZif и правило для времени после последнего перехода.
/// По умолчанию -- UTC.
#[derive(Default)]
pub struct Tz {
    trans: Vec<i64>,
    idx: Vec<u8>,
    /// смещение в секундах к востоку от UTC
    types: Vec<i32>,
    rule: Option<Rule>,
}

fn be32(b: &[u8], o: usize) -> Option<i64> {
    Some(i32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as i64)
}

fn be64(b: &[u8], o: usize) -> Option<i64> {
    Some(i64::from_be_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

impl Tz {
    /// Только POSIX-строка, например значение TZ="CET-1CEST,M3.5.0,M10.5.0/3".
    pub fn from_posix(s: &str) -> Option<Tz> {
        Some(Tz { rule: Some(parse_rule(s)?), ..Default::default() })
    }

    /// Файл TZif (например /etc/localtime), версии 1-4.
    pub fn from_tzif(d: &[u8]) -> Option<Tz> {
        if d.get(..4)? != b"TZif" {
            return None;
        }
        let counts = |o: usize| -> Option<[usize; 6]> {
            let mut c = [0usize; 6];
            for (k, v) in c.iter_mut().enumerate() {
                *v = be32(d, o + 20 + k * 4)? as usize;
            }
            Some(c) // isut, isstd, leap, time, type, char
        };
        let mut off = 0;
        let mut c = counts(0)?;
        let mut ts = 4;
        let v2 = d[4] >= b'2';
        if v2 {
            off = 44 + c[3] * 4 + c[3] + c[4] * 6 + c[5] + c[2] * 8 + c[1] + c[0];
            d.get(..off + 44)?;
            c = counts(off)?;
            ts = 8;
        }
        let mut p = off + 44;
        let mut trans = Vec::with_capacity(c[3]);
        for k in 0..c[3] {
            trans.push(if ts == 8 { be64(d, p + k * 8)? } else { be32(d, p + k * 4)? });
        }
        p += c[3] * ts;
        let idx = d.get(p..p + c[3])?.to_vec();
        p += c[3];
        let mut types = Vec::with_capacity(c[4]);
        for k in 0..c[4] {
            types.push(be32(d, p + k * 6)? as i32);
        }
        if idx.iter().any(|&i| i as usize >= types.len()) {
            return None;
        }
        p += c[4] * 6 + c[5] + c[2] * (ts + 4) + c[1] + c[0];
        let mut rule = None;
        if v2 && d.get(p) == Some(&b'\n') {
            let rest = &d[p + 1..];
            if let Some(end) = rest.iter().position(|&b| b == b'\n') {
                rule = core::str::from_utf8(&rest[..end]).ok().and_then(parse_rule);
            }
        }
        if types.is_empty() && rule.is_none() {
            return None;
        }
        Some(Tz { trans, idx, types, rule })
    }

    /// Смещение от UTC в секундах для момента unix.
    pub fn offset_secs(&self, unix: i64) -> i32 {
        let after_last = self.trans.last().map_or(true, |&t| unix >= t);
        if after_last {
            if let Some(r) = &self.rule {
                return r.offset(unix);
            }
        }
        let n = self.trans.partition_point(|&t| t <= unix);
        match n {
            0 => self.types.first().copied().unwrap_or(0),
            n => self.types[self.idx[n - 1] as usize],
        }
    }

    /// То же в минутах (для `civil`).
    pub fn offset_min(&self, unix: i64) -> i32 {
        self.offset_secs(unix).div_euclid(60)
    }
}

/// Имя файла зоны из TZ: ":Europe/Amsterdam" и "Europe/Amsterdam" дают один путь.
pub fn tz_path(tz: &str) -> String {
    let n = tz.trim_start_matches(':');
    if n.starts_with('/') {
        String::from(n)
    } else {
        alloc::format!("/usr/share/zoneinfo/{n}")
    }
}
