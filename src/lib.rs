//! Ядро odjkbar без std: конфиг, шрифты, текст, канва, D-Bus, трей, модули, календарь.
//! Сокеты, файлы и X11 живут в бинаре.
#![no_std]
extern crate alloc;

pub mod canvas;
pub mod clock;
pub mod config;
pub mod dbus;
pub mod modules;
pub mod text;
pub mod tray;
pub mod x11;

pub use canvas::Canvas;
pub use config::{hex, Config};
pub use text::{parse_spec, Text};
