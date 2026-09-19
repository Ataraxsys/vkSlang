//! Process-wide control state shared between the IPC thread (writes the
//! desired settings) and the present hook (applies them, publishes status).
//!
//! Every setting carries a generation counter: each device runtime compares
//! it with the generation it last applied, so changes are applied exactly
//! once per device, on its own presenting thread.
//!
//! Lock order: `DeviceData::runtime` -> `control()`. The IPC thread only ever
//! takes `control()`.

use crate::config::{self, Source};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, MutexGuard};
use vkslang_ipc::{ColorSpace, HdrSettings, Output, Param, State, PROTOCOL_VERSION};

pub struct Control {
    // ---- desired (written by IPC) ----
    pub enabled: bool,
    pub preset: Option<PathBuf>,
    pub preset_gen: u64,
    /// User overrides on top of the preset's own values.
    pub overrides: BTreeMap<String, f32>,
    pub params_gen: u64,
    pub source: Source,
    pub source_gen: u64,
    /// Read every frame, no generation needed.
    pub hdr: HdrSettings,

    // ---- published (written by the runtime) ----
    pub running_preset: Option<PathBuf>,
    pub loading: bool,
    pub error: Option<String>,
    pub params: Vec<Param>,
    pub preset_color_space: Option<ColorSpace>,
    pub outputs: Vec<Output>,
}

static CONTROL: LazyLock<Mutex<Control>> = LazyLock::new(|| {
    let cfg = config::get();
    Mutex::new(Control {
        enabled: true,
        preset: cfg.preset.clone(),
        preset_gen: 0,
        overrides: cfg.params.iter().cloned().collect(),
        params_gen: 0,
        source: cfg.source.clone(),
        source_gen: 0,
        hdr: cfg.hdr,
        running_preset: None,
        loading: false,
        error: None,
        params: Vec::new(),
        preset_color_space: None,
        outputs: Vec::new(),
    })
});

pub fn control() -> MutexGuard<'static, Control> {
    CONTROL.lock().unwrap_or_else(|e| e.into_inner())
}

impl Control {
    pub fn snapshot(&self) -> State {
        State {
            protocol: PROTOCOL_VERSION,
            pid: std::process::id(),
            process: config::exe_name(),
            enabled: self.enabled,
            preset: self.running_preset.as_ref().map(|p| p.display().to_string()),
            loading: self.loading,
            error: self.error.clone(),
            source: self.source.to_ipc(),
            outputs: self.outputs.clone(),
            preset_color_space: self.preset_color_space,
            hdr: self.hdr,
            params: self.params.clone(),
        }
    }

    /// Value a parameter should have: user override, else preset value.
    pub fn param_value(&self, name: &str, initial: f32) -> f32 {
        self.overrides.get(name).copied().unwrap_or(initial)
    }
}
