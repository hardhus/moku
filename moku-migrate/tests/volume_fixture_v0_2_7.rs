//! Volume hop 0 -> 1 against the frozen 0.2.7 volume fixtures (copied to
//! a temp dir first).

use std::path::{Path, PathBuf};

use moku_migrate::MigrateError;
use moku_migrate::volume::{CURRENT_VOLUME_FORMAT, VOLUME_FILE, upgrade_volume, volume_format};
use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../test-fixtures/v0.2.7/volumes")
        .join(name)
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

fn json(dir: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join(VOLUME_FILE)).unwrap()).unwrap()
}

#[test]
fn fixture_volumes_get_their_format_recorded_and_nothing_else_changes() {
    for name in ["default-foo", "custom-bar"] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(name);
        copy_dir(&fixture(name), &dir);
        let before = json(&dir);
        assert_eq!(volume_format(&dir).unwrap(), 0, "{name}");

        assert_eq!(
            upgrade_volume(&dir).unwrap(),
            Some((0, CURRENT_VOLUME_FORMAT))
        );
        let mut after = json(&dir);
        assert_eq!(after["format"], 1, "{name}");
        after.as_object_mut().unwrap().remove("format");
        assert_eq!(after, before, "{name}: only `format` was added");

        // Idempotent; data/ untouched.
        assert_eq!(upgrade_volume(&dir).unwrap(), None);
        assert_eq!(
            std::fs::read(dir.join("data/.moku_dir_iv")).unwrap(),
            std::fs::read(fixture(name).join("data/.moku_dir_iv")).unwrap()
        );
    }
}

#[test]
fn a_missing_volume_json_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        upgrade_volume(tmp.path()),
        Err(MigrateError::Io { .. })
    ));
}
