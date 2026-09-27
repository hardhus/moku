//! Verifies that today's code can still open the frozen moku-bin 0.2.7
//! volume fixtures in `test-fixtures/v0.2.7/volumes/` (see the README
//! there): a Default-mode volume whose key is derived from the fixture's
//! app vault, and a Custom-mode volume with its own vault. The files are
//! copied to a temporary directory first, so the committed fixtures are
//! never modified. This also covers the MKV1 content format (K8): multi-
//! block files, a subdirectory with its own dir IV, and encrypted names.

use std::path::{Path, PathBuf};

use moku_core::security::SecurityManager;
use moku_volume_daemon::registry::{MountSecret, load_config, resolve_volume_master_key};
use moku_volume_fs::{FileKind, VirtualPath, VolumeEngine, derive_volume_keys};
use zeroize::Zeroizing;

const VAULT_PASSWORD: &str = "foo-test-password";
const CUSTOM_VOLUME_PASSWORD: &str = "bar-volume-password";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../test-fixtures/v0.2.7")
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

fn read_all(engine: &VolumeEngine, path: &str) -> Vec<u8> {
    let path = VirtualPath::parse(path);
    let size = engine.getattr(&path).unwrap().size as usize;
    let fh = engine.open(&path).unwrap();
    let mut buf = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = engine.read(fh, done as u64, &mut buf[done..]).unwrap();
        assert!(n > 0, "unexpected EOF in {path:?}");
        done += n;
    }
    engine.release(fh).unwrap();
    buf
}

fn assert_expected_tree(engine: &VolumeEngine) {
    let mut root: Vec<(String, FileKind)> = engine
        .read_dir(&VirtualPath::root())
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect();
    root.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        root,
        vec![
            ("bar.txt".to_string(), FileKind::File),
            ("docs".to_string(), FileKind::Directory),
        ]
    );
    assert_eq!(read_all(engine, "/bar.txt"), b"bar\n");
    assert_eq!(read_all(engine, "/docs/foo.md"), b"# Foo\n\nbar baz\n");
    let big: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(read_all(engine, "/docs/big.bin"), big);
}

async fn open(dir: &Path, secret: MountSecret) -> VolumeEngine {
    let cfg = load_config(dir).await.unwrap();
    let master = resolve_volume_master_key(dir, &cfg, secret).await.unwrap();
    VolumeEngine::open_volume(
        dir.join("data"),
        derive_volume_keys(&master),
        dir.join("usage.json"),
        cfg.size_limit_bytes,
    )
    .unwrap()
}

#[tokio::test]
async fn v0_2_7_default_volume_opens_with_the_app_vault_key() {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&fixture_root(), tmp.path());

    let app_key = SecurityManager::new_with_root(tmp.path().join("datadir"))
        .unlock_vault(Zeroizing::new(VAULT_PASSWORD.into()))
        .await
        .unwrap();
    let engine = open(
        &tmp.path().join("volumes/default-foo"),
        MountSecret::Key(app_key),
    )
    .await;
    assert_expected_tree(&engine);
}

#[tokio::test]
async fn v0_2_7_custom_volume_opens_with_its_own_password() {
    let tmp = tempfile::tempdir().unwrap();
    copy_dir(&fixture_root().join("volumes/custom-bar"), tmp.path());

    let engine = open(
        tmp.path(),
        MountSecret::Password(CUSTOM_VOLUME_PASSWORD.into()),
    )
    .await;
    assert_expected_tree(&engine);
}
