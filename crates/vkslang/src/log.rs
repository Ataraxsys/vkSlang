//! Minimal stderr logger controlled by `VKSLANG_LOG=error|warn|info|debug`.

use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

pub fn max_level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VKSLANG_LOG").as_deref() {
        Ok("error") => Level::Error,
        Ok("warn") => Level::Warn,
        Ok("debug") | Ok("trace") => Level::Debug,
        _ => Level::Info,
    })
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $tag:literal, $($arg:tt)*) => {
        if $lvl <= $crate::log::max_level() {
            eprintln!(concat!("vkSlang ", $tag, ": {}"), format_args!($($arg)*));
        }
    };
}
#[macro_export]
macro_rules! log_error { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Error, "error", $($arg)*) }; }
#[macro_export]
macro_rules! log_warn { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Warn, "warn", $($arg)*) }; }
#[macro_export]
macro_rules! log_info { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Info, "info", $($arg)*) }; }
#[macro_export]
macro_rules! log_debug { ($($arg:tt)*) => { $crate::log_at!($crate::log::Level::Debug, "debug", $($arg)*) }; }
