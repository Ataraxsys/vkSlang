//! French and English, picked from the system and switchable in the window.
//!
//! Texts are written in place as `(french, english)` pairs: a key table would
//! only move them away from where they are read.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Fr,
    En,
}

impl Lang {
    /// French when the system speaks it, English otherwise.
    pub fn from_system() -> Lang {
        let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default();
        if locale.starts_with("fr") {
            Lang::Fr
        } else {
            Lang::En
        }
    }

    pub fn t(self, fr: &'static str, en: &'static str) -> &'static str {
        match self {
            Lang::Fr => fr,
            Lang::En => en,
        }
    }
}

/// What the panel remembers between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prefs {
    pub lang: Lang,
}

fn path() -> PathBuf {
    vkslang_ipc::profile::dir().with_file_name("ui.json")
}

impl Prefs {
    pub fn load() -> Prefs {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Prefs { lang: Lang::from_system() })
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }
}
