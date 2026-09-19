//! Minimal stderr logger controlled by `VKSLANG_LOG=error|warn|info|debug`.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

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

/// Optional log file (`VKSLANG_LOG_FILE`), for games whose stderr is hard to
/// reach (Steam, Proton).
pub fn file() -> Option<&'static Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::var_os("VKSLANG_LOG_FILE")?;
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => Some(Mutex::new(f)),
            Err(e) => {
                eprintln!("vkSlang error: cannot open {}: {e}", path.to_string_lossy());
                None
            }
        }
    })
    .as_ref()
}

/// Writes one already formatted line to stderr and to the log file.
pub fn emit(line: &str) {
    eprintln!("{line}");
    if let Some(file) = file() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "[{}] {line}", std::process::id());
        }
    }
}

#[macro_export]
macro_rules! log_at {
    ($lvl:expr, $tag:literal, $($arg:tt)*) => {
        if $lvl <= $crate::log::max_level() {
            $crate::log::emit(&format!(concat!("vkSlang ", $tag, ": {}"), format_args!($($arg)*)));
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
