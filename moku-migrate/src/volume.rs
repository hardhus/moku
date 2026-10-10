//! The per-volume format chain (audit P4), separate from the data
//! directory's: volumes live wherever the user put them (removable drives
//! included) and Custom volumes have their own password, so each one is
//! upgraded lazily, by the mount worker, right after it takes the
//! volume's `VolumeLock` and before it derives any key.
//!
//! The counter is `volume.json`'s `format` field (missing = 0). Hops here
//! follow the same rules as the data hops (frozen, own types, crash safe,
//! idempotent, fixture test): they read `volume.json` as plain JSON, not
//! through moku-volume-daemon's `VolumeConfig`.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::MigrateError;

/// The file each volume directory describes itself in.
pub const VOLUME_FILE: &str = "volume.json";

/// The volume format this build writes and upgrades to.
///
/// - `0`: no `format` field (every volume before moku-bin 0.2.12).
/// - `1`: `format` recorded; nothing else changed.
pub const CURRENT_VOLUME_FORMAT: u32 = 1;

/// One step of the volume chain.
pub struct VolumeHop {
    pub from: u32,
    pub to: u32,
    pub description: &'static str,
    pub run: fn(&Path) -> Result<(), MigrateError>,
}

/// The volume chain, in order.
pub static VOLUME_HOPS: &[VolumeHop] = &[VolumeHop {
    from: 0,
    to: 1,
    description: "Record the volume format in volume.json",
    run: hop_0000_record_format,
}];

/// A volume's format, read from its `volume.json` without unlocking.
pub fn volume_format(volume_dir: &Path) -> Result<u32, MigrateError> {
    let (_, json) = read_volume_json(volume_dir)?;
    format_of(&json, &volume_dir.join(VOLUME_FILE))
}

/// Upgrades one volume to `CURRENT_VOLUME_FORMAT`. The caller holds the
/// volume's `VolumeLock`. Returns `Some((from, to))` if anything ran.
/// A format newer than this build is `NewerVolumeFormat` (the volume must
/// not be opened).
pub fn upgrade_volume(volume_dir: &Path) -> Result<Option<(u32, u32)>, MigrateError> {
    upgrade_with(volume_dir, VOLUME_HOPS, CURRENT_VOLUME_FORMAT)
}

fn upgrade_with(
    volume_dir: &Path,
    hops: &[VolumeHop],
    current: u32,
) -> Result<Option<(u32, u32)>, MigrateError> {
    let from = volume_format(volume_dir)?;
    if from == current {
        return Ok(None);
    }
    if from > current {
        return Err(MigrateError::NewerVolumeFormat {
            path: volume_dir.to_path_buf(),
            found: from,
            supported: current,
        });
    }
    let Some(start) = hops.iter().position(|h| h.from == from) else {
        return Err(MigrateError::NoPath {
            from: crate::DataFormat(from),
        });
    };
    for hop in &hops[start..] {
        (hop.run)(volume_dir).map_err(|e| MigrateError::HopFailed {
            hop: hop.description,
            source: Box::new(e),
        })?;
    }
    Ok(Some((from, current)))
}

fn read_volume_json(volume_dir: &Path) -> Result<(PathBuf, Value), MigrateError> {
    let path = volume_dir.join(VOLUME_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(MigrateError::io("read a volume's volume.json", &path))?;
    let json = serde_json::from_str(&text).map_err(|e| MigrateError::FormatUnreadable {
        path: path.clone(),
        source: Box::new(e),
    })?;
    Ok((path, json))
}

fn format_of(json: &Value, path: &Path) -> Result<u32, MigrateError> {
    match json.get("format") {
        None => Ok(0),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| MigrateError::FormatUnreadable {
                path: path.to_path_buf(),
                source: format!("`format` is not a version number: {v}").into(),
            }),
    }
}

/// Volume hop 0 -> 1. FROZEN (shipped in moku-bin 0.2.12): adds
/// `"format": 1` to `volume.json`, keeping every other field as it is.
fn hop_0000_record_format(volume_dir: &Path) -> Result<(), MigrateError> {
    let (path, mut json) = read_volume_json(volume_dir)?;
    let Some(object) = json.as_object_mut() else {
        return Err(MigrateError::FormatUnreadable {
            path,
            source: "volume.json is not a JSON object".into(),
        });
    };
    object.insert("format".to_string(), Value::from(1u32));
    let text = serde_json::to_string_pretty(&json).map_err(|e| MigrateError::Io {
        what: "encode volume.json",
        path: path.clone(),
        source: io::Error::other(e),
    })?;
    moku_sys::util::atomic_write(&path, text.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(VOLUME_FILE), json).unwrap();
        dir
    }

    #[test]
    fn volume_hops_are_contiguous() {
        assert_eq!(VOLUME_HOPS.first().map(|h| h.from), Some(0));
        for pair in VOLUME_HOPS.windows(2) {
            assert_eq!(pair[0].to, pair[1].from);
        }
        for h in VOLUME_HOPS {
            assert_eq!(h.to, h.from + 1);
        }
        assert_eq!(
            VOLUME_HOPS.last().map(|h| h.to),
            Some(CURRENT_VOLUME_FORMAT)
        );
    }

    #[test]
    fn test_newer_volume_is_refused_and_untouched() {
        let dir = volume(r#"{"id":"x","format":9}"#);
        assert!(matches!(
            upgrade_volume(dir.path()),
            Err(MigrateError::NewerVolumeFormat { found: 9, .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(VOLUME_FILE)).unwrap(),
            r#"{"id":"x","format":9}"#
        );
    }

    #[test]
    fn test_bad_format_field_is_unreadable() {
        let dir = volume(r#"{"id":"x","format":"one"}"#);
        assert!(matches!(
            volume_format(dir.path()),
            Err(MigrateError::FormatUnreadable { .. })
        ));
    }
}
