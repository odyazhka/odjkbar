//! D-Bus без std: кодирование и разбор сообщений над байтами.
//! Сокет и аутентификацию делает бинарь. Только little-endian.

use alloc::{string::{String, ToString}, vec, vec::Vec};

pub fn align(i: usize, n: usize) -> usize {
    (i + n - 1) & !(n - 1)
}

fn u32le(b: &[u8]) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(..4)?.try_into().ok()?))
}

/// Полная длина сообщения по первым 16 байтам заголовка. Бинарь читает столько байт.
pub fn msg_len(h: &[u8]) -> Option<usize> {
    if h.len() < 16 || h[0] != b'l' {
        return None;
    }
    let blen = u32le(&h[4..8])? as usize;
    let flen = u32le(&h[12..16])? as usize;
    Some(align(16 + flen, 8) + blen)
}

/// Входящее сообщение (только нужные поля).
pub struct Msg {
    pub mtype: u8, // 1 method_call, 2 method_return, 3 error, 4 signal
    pub serial: u32,
    pub reply: u32,
    pub path: String,
    pub iface: String,
    pub member: String,
    pub sender: String,
    pub body: Vec<u8>,
}

/// Разбор одного сообщения целиком (заголовок + тело).
pub fn parse(m: &[u8]) -> Option<Msg> {
    let total = msg_len(m)?;
    if m.len() < total {
        return None;
    }
    let blen = u32le(&m[4..8])? as usize;
    let serial = u32le(&m[8..12])?;
    let flen = u32le(&m[12..16])? as usize;
    let f = m.get(16..16 + flen)?;
    let bstart = align(16 + flen, 8);
    let body = m.get(bstart..bstart + blen)?.to_vec();
    let mut out = Msg {
        mtype: m[1],
        serial,
        reply: 0,
        path: String::new(),
        iface: String::new(),
        member: String::new(),
        sender: String::new(),
        body,
    };
    let mut i = 0usize;
    while i < f.len() {
        i = align(i, 8);
        if i >= f.len() {
            break;
        }
        let code = f[i];
        let slen = *f.get(i + 1)? as usize;
        let t = *f.get(i + 2)?;
        i += 3 + slen;
        let val: &[u8] = match t {
            b'o' | b's' => {
                i = align(i, 4);
                let n = u32le(f.get(i..)?)? as usize;
                i += 4;
                let v = f.get(i..i + n)?;
                i += n + 1;
                v
            }
            b'g' => {
                let n = *f.get(i)? as usize;
                i += 1;
                let v = f.get(i..i + n)?;
                i += n + 1;
                v
            }
            b'u' => {
                i = align(i, 4);
                let v = f.get(i..i + 4)?;
                i += 4;
                v
            }
            _ => return None,
        };
        let text = String::from_utf8_lossy(val).to_string();
        match code {
            1 => out.path = text,
            2 => out.iface = text,
            3 => out.member = text,
            5 => out.reply = u32le(val)?,
            7 => out.sender = text,
            _ => {}
        }
    }
    Some(out)
}

/// Значение поля заголовка при отправке.
pub enum F<'a> {
    O(&'a str),
    S(&'a str),
    U(u32),
    G(&'a str),
}

fn pad(m: &mut Vec<u8>, n: usize) {
    while m.len() % n != 0 {
        m.push(0);
    }
}

pub fn put_u32(m: &mut Vec<u8>, v: u32) {
    pad(m, 4);
    m.extend_from_slice(&v.to_le_bytes());
}

pub fn put_str(m: &mut Vec<u8>, s: &str) {
    put_u32(m, s.len() as u32);
    m.extend_from_slice(s.as_bytes());
    m.push(0);
}

fn put_sig(m: &mut Vec<u8>, s: &str) {
    m.push(s.len() as u8);
    m.extend_from_slice(s.as_bytes());
    m.push(0);
}

fn encode(mtype: u8, serial: u32, fields: &[(u8, F)], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(64 + body.len());
    m.extend_from_slice(&[b'l', mtype, 0, 1]);
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(&serial.to_le_bytes());
    m.extend_from_slice(&[0; 4]); // длина массива полей, запишем ниже
    let start = m.len();
    for (code, f) in fields {
        pad(&mut m, 8);
        m.push(*code);
        match f {
            F::O(s) => {
                m.extend_from_slice(&[1, b'o', 0]);
                put_str(&mut m, s);
            }
            F::S(s) => {
                m.extend_from_slice(&[1, b's', 0]);
                put_str(&mut m, s);
            }
            F::U(v) => {
                m.extend_from_slice(&[1, b'u', 0]);
                put_u32(&mut m, *v);
            }
            F::G(s) => {
                m.extend_from_slice(&[1, b'g', 0]);
                put_sig(&mut m, s);
            }
        }
    }
    let flen = (m.len() - start) as u32;
    m[12..16].copy_from_slice(&flen.to_le_bytes());
    pad(&mut m, 8);
    m.extend_from_slice(body);
    m
}

/// Отправитель сообщений: считает serial и возвращает готовые байты.
#[derive(Default)]
pub struct Bus {
    serial: u32,
}

impl Bus {
    fn next(&mut self) -> u32 {
        self.serial += 1;
        self.serial
    }

    /// Hello: без него шина не начнёт обмен. Возвращает serial для поиска ответа.
    pub fn hello(&mut self) -> (u32, Vec<u8>) {
        self.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello", "", &[])
    }

    pub fn request_name(&mut self, name: &str) -> Vec<u8> {
        let mut b = Vec::new();
        put_str(&mut b, name);
        put_u32(&mut b, 0);
        self.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName", "su", &b).1
    }

    /// Вызов метода: (serial, байты сообщения).
    pub fn call(&mut self, dest: &str, path: &str, iface: &str, member: &str, sig: &str, body: &[u8]) -> (u32, Vec<u8>) {
        let serial = self.next();
        let mut f: Vec<(u8, F)> = vec![
            (1, F::O(path)),
            (2, F::S(iface)),
            (3, F::S(member)),
            (6, F::S(dest)),
        ];
        if !sig.is_empty() {
            f.push((8, F::G(sig)));
        }
        (serial, encode(1, serial, &f, body))
    }

    /// Ответ на вызов `to`: байты сообщения.
    pub fn reply(&mut self, to: &Msg, sig: &str, body: &[u8]) -> Vec<u8> {
        let serial = self.next();
        let mut f: Vec<(u8, F)> = vec![(5, F::U(to.serial)), (6, F::S(to.sender.as_str()))];
        if !sig.is_empty() {
            f.push((8, F::G(sig)));
        }
        encode(2, serial, &f, body)
    }
}

/// Читатель значений из тела сообщения (little-endian).
pub struct Rd<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, i: 0 }
    }

    pub fn align(&mut self, n: usize) {
        self.i = align(self.i, n);
    }

    pub fn u32(&mut self) -> Option<u32> {
        self.align(4);
        let v = u32le(self.b.get(self.i..)?)?;
        self.i += 4;
        Some(v)
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    pub fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = String::from_utf8_lossy(self.b.get(self.i..self.i + n)?).to_string();
        self.i += n + 1;
        Some(s)
    }

    pub fn sig(&mut self) -> Option<String> {
        let n = *self.b.get(self.i)? as usize;
        self.i += 1;
        let s = String::from_utf8_lossy(self.b.get(self.i..self.i + n)?).to_string();
        self.i += n + 1;
        Some(s)
    }
}
