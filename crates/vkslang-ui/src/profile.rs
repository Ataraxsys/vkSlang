//! Named profiles: a whole look saved under a name, rather than a `.slangp`.
//!
//! A profile holds what the UI can set on a running game: the preset chain,
//! the parameters the user changed, the source and display geometry, the HDR
//! uniforms and the subframes. They live as one JSON file each in
//! `~/.config/vkSlang/profiles`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use vkslang_ipc::{HdrSettings, SourceSettings, State};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub presets: Vec<String>,
    pub source: SourceSettings,
    pub hdr: HdrSettings,
    pub subframes: u32,
    pub subframe_black: bool,
    /// Only the parameters that differ from the preset's own values.
    pub params: BTreeMap<String, f32>,
}

impl Profile {
    /// Captures what is currently running.
    pub fn from_state(name: &str, state: &State) -> Profile {
        Profile {
            name: name.trim().to_string(),
            presets: state.presets.clone(),
            source: state.source.clone(),
            hdr: state.hdr,
            subframes: state.subframes.max(1),
            subframe_black: state.subframe_black,
            params: state
                .params
                .iter()
                .filter(|p| !p.is_header() && p.is_modified())
                .map(|p| (p.name.clone(), p.value))
                .collect(),
        }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("vkSlang").join("profiles")
}

/// Keeps a name usable as a file name, without surprising the user with a
/// path of their own making.
pub fn file_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || " -_.".contains(c) { c } else { '_' })
        .collect();
    format!("{}.json", cleaned.trim())
}

pub fn save_in(dir: &Path, profile: &Profile) -> std::io::Result<PathBuf> {
    if profile.name.trim().is_empty() {
        return Err(std::io::Error::other("a profile needs a name"));
    }
    let path = dir.join(file_name(&profile.name));
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, serde_json::to_string_pretty(profile)?)?;
    Ok(path)
}

pub fn save(profile: &Profile) -> std::io::Result<PathBuf> {
    save_in(&dir(), profile)
}

pub fn load(path: &Path) -> std::io::Result<Profile> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn delete_in(dir: &Path, profile: &Profile) -> std::io::Result<()> {
    std::fs::remove_file(dir.join(file_name(&profile.name)))
}

pub fn delete(profile: &Profile) -> std::io::Result<()> {
    delete_in(&dir(), profile)
}

/// Every saved profile, by name.
pub fn list() -> Vec<Profile> {
    list_in(&dir())
}

pub fn list_in(dir: &Path) -> Vec<Profile> {
    let mut profiles: Vec<Profile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| load(&e.path()).ok())
        .collect();
    profiles.sort_by_key(|p| p.name.to_lowercase());
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_safe_file_names() {
        assert_eq!(file_name("Ma config 4:3"), "Ma config 4_3.json");
        // Slashes are the only real danger: the result is one file name.
        assert_eq!(file_name("  ../escape  "), ".._escape.json");
        assert!(!file_name("a/b/c").contains('/'));
    }

    fn sample_state() -> State {
        let param = |name: &str, initial, value| vkslang_ipc::Param {
            name: name.into(),
            description: name.into(),
            initial,
            minimum: 0.0,
            maximum: 10.0,
            step: 0.1,
            value,
        };
        State {
            presets: vec!["/a.slangp".into(), "/b.slangp".into()],
            subframes: 3,
            params: vec![param("KEPT", 1.0, 2.0), param("SAME", 1.0, 1.0)],
            ..Default::default()
        }
    }

    #[test]
    fn saved_then_listed_and_deleted() {
        let dir = std::env::temp_dir().join(format!("vkslang-profiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let profile = Profile::from_state("Amiga 4:3", &sample_state());
        save_in(&dir, &profile).unwrap();

        let found = list_in(&dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0], profile, "a profile survives the round trip untouched");
        assert_eq!(found[0].subframes, 3);

        delete_in(&dir, &profile).unwrap();
        assert!(list_in(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_profile_keeps_only_changed_parameters() {
        let profile = Profile::from_state("  test  ", &sample_state());
        assert_eq!(profile.name, "test");
        assert_eq!(profile.presets.len(), 2);
        assert_eq!(profile.params.len(), 1);
        assert_eq!(profile.params["KEPT"], 2.0);
    }
}
