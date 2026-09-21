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
