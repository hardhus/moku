//! The config.toml format chain (audit P4): the same one-hop-per-change
//! model as moku-migrate's data chain, with its own counter
//! (`config_version`). It lives here because `ConfigHandle` is the only
//! owner of config.toml.
//!
//! The chain is empty today: the counter starts at 1 (a file without
//! `config_version` is version 1) and `CURRENT_CONFIG_VERSION` is 1. A
//! future config change adds one `ConfigHop` that edits the document in
//! place (comments and unknown keys survive, as with every config write)
//! and bumps `CURRENT_CONFIG_VERSION`. Like a data hop, a config hop is
//! frozen once shipped and works on the raw TOML document, not on
//! `MokuConfig`.

use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::schema::CURRENT_CONFIG_VERSION;

/// The version of a file without `config_version`.
pub const CONFIG_BASE_VERSION: u32 = 1;

/// One step of the config chain, from `from` to `to` (= `from + 1`).
pub struct ConfigHop {
    pub from: u32,
    pub to: u32,
    pub description: &'static str,
    pub run: fn(&mut DocumentMut) -> Result<(), String>,
}

/// The chain, in order. Empty until the first config format change.
pub static CONFIG_HOPS: &[ConfigHop] = &[];

/// The file's `config_version`, or the base version if it has none. A
/// value that isn't a number is left to the normal section parsing (it is
/// reported there), so it is treated as current here.
pub(crate) fn file_version(table: &toml::Table) -> u32 {
    match table.get("config_version") {
        None => CONFIG_BASE_VERSION,
        Some(v) => v
            .as_integer()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(CURRENT_CONFIG_VERSION),
    }
}

/// Brings an older config.toml up to `CURRENT_CONFIG_VERSION`: keeps a
/// `config.toml.pre-v<to>-<unix time>` copy, applies the pending hops to
/// the document, records the new `config_version` and writes the file
/// atomically. Returns the new document text, or `None` if nothing was
/// pending. Any failure is an `Err` and nothing is written (the caller
/// opens the file read-only, as for any config it can't handle).
pub(crate) fn upgrade_file(
    path: &Path,
    doc: &DocumentMut,
    from: u32,
) -> Result<Option<String>, String> {
    upgrade_with(path, doc, from, CONFIG_HOPS, CURRENT_CONFIG_VERSION)
}

fn upgrade_with(
    path: &Path,
    doc: &DocumentMut,
    from: u32,
    hops: &[ConfigHop],
    current: u32,
) -> Result<Option<String>, String> {
    if from >= current {
        return Ok(None);
    }
    let mut doc = doc.clone();
    let Some(start) = hops.iter().position(|h| h.from == from) else {
        return Err(format!(
            "config version {from} has no upgrade path to {current}"
        ));
    };
    for hop in &hops[start..] {
        (hop.run)(&mut doc)
            .map_err(|e| format!("config upgrade \"{}\" failed: {e}", hop.description))?;
    }
    doc["config_version"] = toml_edit::value(i64::from(current));

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut copy = path.as_os_str().to_owned();
    copy.push(format!(".pre-v{current}-{stamp}"));
    let copy = PathBuf::from(copy);
    std::fs::copy(path, &copy)
        .map_err(|e| format!("couldn't back up config.toml to {}: {e}", copy.display()))?;

    let text = doc.to_string();
    moku_sys::util::atomic_write(path, text.as_bytes())
        .map_err(|e| moku_sys::util::error_chain(&e))?;
    Ok(Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_hops_are_contiguous() {
        let mut version = CONFIG_BASE_VERSION;
        for hop in CONFIG_HOPS {
            assert_eq!(hop.from, version, "{}", hop.description);
            assert_eq!(hop.to, hop.from + 1, "{}", hop.description);
            version = hop.to;
        }
        assert_eq!(version, CURRENT_CONFIG_VERSION);
    }

    fn rename_key(doc: &mut DocumentMut) -> Result<(), String> {
        let old = doc["general"]["old_name"].clone();
        doc["general"]["new_name"] = old;
        doc["general"]
            .as_table_mut()
            .ok_or("no [general]")?
            .remove("old_name");
        Ok(())
    }

    fn add_key(doc: &mut DocumentMut) -> Result<(), String> {
        doc["general"]["added"] = toml_edit::value(true);
        Ok(())
    }

    static FAKE: &[ConfigHop] = &[
        ConfigHop {
            from: 1,
            to: 2,
            description: "rename",
            run: rename_key,
        },
        ConfigHop {
            from: 2,
            to: 3,
            description: "add",
            run: add_key,
        },
    ];

    #[test]
    fn test_hops_run_in_order_keep_comments_and_back_up_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# my comment\n[general]\nold_name = 5 # inline\n";
        std::fs::write(&path, original).unwrap();
        let doc: DocumentMut = original.parse().unwrap();

        let text = upgrade_with(&path, &doc, 1, FAKE, 3).unwrap().unwrap();
        assert!(text.contains("# my comment"));
        assert!(text.contains("new_name = 5"));
        assert!(!text.contains("old_name"));
        assert!(text.contains("added = true"));
        assert!(text.contains("config_version = 3"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("config.toml.pre-v3-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(&backups[0])).unwrap(),
            original
        );

        // From the middle of the chain only the rest runs; current is a no-op.
        let doc: DocumentMut = "[general]\n".parse().unwrap();
        let text = upgrade_with(&path, &doc, 2, FAKE, 3).unwrap().unwrap();
        assert!(text.contains("added = true"));
        assert!(upgrade_with(&path, &doc, 3, FAKE, 3).unwrap().is_none());
    }

    fn fail(_: &mut DocumentMut) -> Result<(), String> {
        Err("nope".to_string())
    }

    static FAILING: &[ConfigHop] = &[ConfigHop {
        from: 1,
        to: 2,
        description: "fails",
        run: fail,
    }];

    #[test]
    fn test_a_failing_hop_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x = 1\n").unwrap();
        let doc: DocumentMut = "x = 1\n".parse().unwrap();
        let err = upgrade_with(&path, &doc, 1, FAILING, 2).unwrap_err();
        assert!(err.contains("fails") && err.contains("nope"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x = 1\n");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no backup either"
        );
    }

    #[test]
    fn test_file_version() {
        let t = |s: &str| toml::from_str::<toml::Table>(s).unwrap();
        assert_eq!(file_version(&t("")), CONFIG_BASE_VERSION);
        assert_eq!(file_version(&t("config_version = 1")), 1);
        assert_eq!(file_version(&t("config_version = 7")), 7);
        assert_eq!(
            file_version(&t("config_version = \"x\"")),
            CURRENT_CONFIG_VERSION
        );
    }
}
