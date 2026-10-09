use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum StorageType {
    Embedded,
    External,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum EncryptionStatus {
    Plaintext,
    Encrypted,
}

/// Which storage/key-derivation scheme encrypted this envelope's
/// `payload`, when `status == Encrypted` — meaningless (and left at its
/// default) for `Plaintext` envelopes. `#[serde(default)]` means every
/// envelope written before this field existed deserializes as `V0`,
/// which is exactly correct: it really was encrypted directly under the
/// raw vault master key, the only scheme that existed at the time.
///
/// `#[serde(rename = ...)]` on each variant keeps the on-disk JSON tags
/// (`"Legacy"` / `"PerModuleV1"`) stable even though the Rust-side names
/// were renamed to a numbered scheme (`V0`/`V1`) — every already-written
/// record, including already-migrated production data, must keep
/// deserializing correctly with zero action required.
///
/// This is the first link of a permanent migration chain. Adding a
/// future `V2` means: (1) a new variant here with its own `version()`
/// entry, (2) a `resolve_read_key` match arm in `storage::manager`,
/// (3) bumping `CURRENT_KEY_SCHEME` below, and (4) — only if `V2`
/// changes the decrypted JSON shape, not just the key derivation — a
/// `match` arm in `data_transform_for_hop`. `StorageManager::
/// migrate_key_scheme_to_latest` walks any record from whatever version
/// it's currently on straight to `CURRENT_KEY_SCHEME`; a caller never
/// needs to know or care about intermediate versions.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyScheme {
    /// v0 — encrypted directly under the raw vault master key
    /// (pre-migration behavior, never used for new writes anymore).
    #[default]
    #[serde(rename = "Legacy")]
    V0,
    /// v1 — encrypted under `HKDF(master, "moku-core/storage/<module_id>/v1")`
    /// (see `storage::keys::derive_module_storage_key`) — never the raw
    /// master key directly. Current scheme for every new write.
    #[serde(rename = "PerModuleV1")]
    V1,
}

impl KeyScheme {
    /// Numeric position in the migration chain — `V0` = 0, `V1` = 1, a
    /// future `V2` = 2, and so on. Lets `StorageManager` detect "needs
    /// migration" as a plain `version()` comparison instead of matching
    /// on every past variant everywhere that cares.
    pub const fn version(self) -> u16 {
        match self {
            KeyScheme::V0 => 0,
            KeyScheme::V1 => 1,
        }
    }
}

/// The scheme every new encrypted write uses, and the target every
/// `StorageManager::migrate_key_scheme_to_latest` call upgrades toward.
pub const CURRENT_KEY_SCHEME: KeyScheme = KeyScheme::V1;

/// Applied, in order, to every intermediate version a record passes
/// through on its way to `CURRENT_KEY_SCHEME` — for a hop whose upgrade
/// isn't just "re-encrypt under a new key" but actually changes the
/// decrypted JSON payload's shape. `from_version` is the version being
/// upgraded *from* (e.g. `1` for a `V1 -> V2` hop). No hop needs one
/// today — `V0 -> V1` only changed key derivation, not the plaintext
/// shape — so this always returns `None` for now; add a `match` arm
/// here (and use `from_version` instead of `_from_version`) when a real
/// data-shape change is introduced.
pub type DataTransform = fn(serde_json::Value) -> anyhow::Result<serde_json::Value>;

pub fn data_transform_for_hop(_from_version: u16) -> Option<DataTransform> {
    None
}

#[derive(Serialize, Deserialize)]
pub struct StorageEnvelope {
    #[serde(default = "default_schema_version")]
    pub schema_version: u16,
    pub status: EncryptionStatus,
    pub storage_type: StorageType,
    pub payload: Vec<u8>,
    pub hash: Option<String>,
    #[serde(default)]
    pub key_scheme: KeyScheme,
}

fn default_schema_version() -> u16 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The on-disk JSON tag must stay exactly as it was before the `V0`/
    /// `V1` rename — this is what lets every already-written envelope
    /// (including a real user's already-migrated production data) keep
    /// deserializing correctly.
    #[test]
    fn test_key_scheme_serde_tags_are_stable_across_the_rename() {
        assert_eq!(serde_json::to_string(&KeyScheme::V0).unwrap(), "\"Legacy\"");
        assert_eq!(
            serde_json::to_string(&KeyScheme::V1).unwrap(),
            "\"PerModuleV1\""
        );
        assert_eq!(
            serde_json::from_str::<KeyScheme>("\"Legacy\"").unwrap(),
            KeyScheme::V0
        );
        assert_eq!(
            serde_json::from_str::<KeyScheme>("\"PerModuleV1\"").unwrap(),
            KeyScheme::V1
        );
    }

    #[test]
    fn test_key_scheme_version_ordering() {
        assert_eq!(KeyScheme::V0.version(), 0);
        assert_eq!(KeyScheme::V1.version(), 1);
        assert!(KeyScheme::V0.version() < CURRENT_KEY_SCHEME.version());
        assert_eq!(KeyScheme::V1.version(), CURRENT_KEY_SCHEME.version());
    }

    #[test]
    fn test_default_key_scheme_is_v0() {
        assert_eq!(KeyScheme::default(), KeyScheme::V0);
    }

    #[test]
    fn test_no_data_transform_registered_yet() {
        assert!(data_transform_for_hop(0).is_none());
        assert!(data_transform_for_hop(1).is_none());
    }
}
