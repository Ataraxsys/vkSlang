//! Persisting the live settings: a RetroArch-compatible `.slangp` that
//! `#reference`s the running preset, or the vkSlang.conf file.

use std::io;
use std::path::{Path, PathBuf};
use vkslang_ipc::{Filter, State};

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("VKSLANG_CONFIG") {
        return PathBuf::from(p);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("vkSlang")
        .join("vkSlang.conf")
}

/// Suggested location for a user preset derived from `preset`.
pub fn default_preset_path(preset: Option<&str>) -> PathBuf {
    let stem = preset
        .and_then(|p| Path::new(p).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "preset".into());
    config_path().with_file_name("presets").join(format!("{stem}-custom.slangp"))
}

/// Parameters the user changed (value differs from the preset's).
fn changed_params(state: &State) -> impl Iterator<Item = (&str, f32)> {
    state
        .params
        .iter()
        .filter(|p| !p.is_header() && p.is_modified())
        .map(|p| (p.name.as_str(), p.value))
}

pub fn slangp_text(state: &State) -> Option<String> {
    let preset = state.preset.as_ref()?;
    let mut text = format!("#reference \"{preset}\"\n");
    for (name, value) in changed_params(state) {
        text.push_str(&format!("{name} = \"{value}\"\n"));
    }
    Some(text)
}

pub fn save_slangp(state: &State, path: &Path) -> io::Result<()> {
    let text = slangp_text(state).ok_or_else(|| io::Error::other("no preset running"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

/// Rewrites the keys vkSlang manages, keeping the user's other lines and
/// comments.
pub fn update_config_text(existing: &str, state: &State) -> String {
    let managed = |key: &str| {
        matches!(
            key,
            "preset"
                | "source_res"
                | "source_filter"
                | "source_rect"
                | "brightness_nits"
                | "expand_gamut"
                | "subframes"
                | "subframe_mode"
        ) || key.starts_with("param.")
    };
    let mut out: Vec<String> = existing
        .lines()
        .filter(|line| {
            let content = line.split('#').next().unwrap_or("");
            match content.split_once('=') {
                Some((key, _)) => !managed(key.trim()),
                None => true,
            }
        })
        .map(str::to_string)
        .collect();
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if !out.is_empty() {
        out.push(String::new());
    }
    out.push("# --- written by vkslang-ui ---".into());
    if let Some(preset) = &state.preset {
        out.push(format!("preset = {preset}"));
    }
    let src = &state.source;
    out.push(format!(
        "source_res = {}",
        src.res.map_or("native".to_string(), |[w, h]| format!("{w}x{h}"))
    ));
    out.push(format!(
        "source_filter = {}",
        if src.filter == Filter::Linear { "linear" } else { "nearest" }
    ));
    out.push(format!("source_rect = {}", src.rect));
    out.push(format!("brightness_nits = {}", state.hdr.brightness_nits));
    out.push(format!("expand_gamut = {}", state.hdr.expand_gamut));
    out.push(format!("subframes = {}", state.subframes));
    out.push(format!("subframe_mode = {}", if state.subframe_black { "black" } else { "shader" }));
    for (name, value) in changed_params(state) {
        out.push(format!("param.{name} = {value}"));
    }
    out.push(String::new());
    out.join("\n")
}

pub fn save_config(state: &State) -> io::Result<PathBuf> {
    let path = config_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, update_config_text(&existing, state))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vkslang_ipc::{Param, SourceSettings};

    fn state() -> State {
        let p = |name: &str, initial, value, min, max| Param {
            name: name.into(),
            description: name.into(),
            initial,
            minimum: min,
            maximum: max,
            step: 0.1,
            value,
        };
        State {
            preset: Some("/s/crt.slangp".into()),
            source: SourceSettings { res: Some([320, 240]), filter: Filter::Nearest, rect: "4:3".into() },
            params: vec![
                p("HEADER", 0.0, 0.0, 0.0, 0.0),
                p("GAMMA", 2.2, 2.4, 1.0, 3.0),
                p("MASK", 1.0, 1.0, 0.0, 3.0),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn slangp_only_changed() {
        assert_eq!(slangp_text(&state()).unwrap(), "#reference \"/s/crt.slangp\"\nGAMMA = \"2.4\"\n");
    }

    #[test]
    fn config_keeps_user_lines() {
        let old = "# my conf\nprocess = gamescope\npreset = /old.slangp\nparam.X = 1\n";
        let new = update_config_text(old, &state());
        assert!(new.contains("process = gamescope"));
        assert!(new.contains("# my conf"));
        assert!(!new.contains("/old.slangp"));
        assert!(!new.contains("param.X"));
        assert!(new.contains("preset = /s/crt.slangp"));
        assert!(new.contains("source_res = 320x240"));
        assert!(new.contains("source_rect = 4:3"));
        assert!(new.contains("brightness_nits = 200"));
        assert!(new.contains("expand_gamut = 0"));
        assert!(new.contains("subframes = 1"));
        assert!(new.contains("param.GAMMA = 2.4"));
        assert!(!new.contains("param.MASK"));
    }
}
