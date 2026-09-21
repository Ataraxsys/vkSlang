//! Named profiles live in the shared crate, since the layer loads them too.

pub use vkslang_ipc::profile::*;
#[cfg(test)]
use vkslang_ipc::State;

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

    /// A profile saved before a setting existed must keep loading: the UI
    /// dropped such files silently, and they looked lost.
    #[test]
    fn a_profile_without_the_newer_fields_still_loads() {
        let dir = std::env::temp_dir().join(format!("vkslang-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let older = r#"{
            "name": "IBM PC LCD",
            "presets": ["/a.slangp", "/b.slangp"],
            "source": {
                "res": {"mode": "fixed", "size": [640, 360]},
                "filter": "nearest",
                "rect": "16:9",
                "display": "full",
                "display_scale": [1.0, 1.0]
            },
            "hdr": {"brightness_nits": 200.0, "expand_gamut": 0},
            "subframes": 1,
            "subframe_black": false,
            "params": {"LUT_Size1": 40.0}
        }"#;
        std::fs::write(dir.join("IBM PC LCD.json"), older).unwrap();

        let (profiles, failures) = read_dir(&dir);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "IBM PC LCD");
        assert_eq!(profiles[0].source.duplicate, [1, 1], "missing settings take their default");
        assert_eq!(profiles[0].params["LUT_Size1"], 40.0);
        let _ = std::fs::remove_dir_all(&dir);
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
