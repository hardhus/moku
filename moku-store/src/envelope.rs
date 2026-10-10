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

/// The key-scheme tag on an envelope. The runtime decrypts only `V1`;
/// this is a tag, not a migration chain (that is moku-migrate's `FORMAT`
/// and its hops, audit P4).
///
/// `V0` (`"Legacy"`) stays a variant because two kinds of record carry it:
/// every *plaintext* record (the tag is meaningless there and has always
/// been written as the default), and encrypted records a data directory
/// still in format 0 holds. Reading an encrypted `V0` record returns
/// `StoreError::NeedsUpgrade`; the code that decrypts it lives only in
/// moku-migrate's hop 0 -> 1.
///
/// `#[serde(rename = ...)]` pins the on-disk JSON tags (`"Legacy"` /
/// `"PerModuleV1"`) forever. `#[serde(default)]` on the field means an
/// envelope from before the field existed reads as `V0`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyScheme {
    /// Plaintext records, or encrypted under the raw vault root key
    /// (format 0; never written for encrypted records anymore).
    #[default]
    #[serde(rename = "Legacy")]
    V0,
    /// Encrypted under `module_storage_key(root, module_id)`
    /// (`HKDF(root, "moku-core/storage/<module_id>/v1")`).
    #[serde(rename = "PerModuleV1")]
    V1,
}

/// The scheme every encrypted write uses, and the only one read.
pub const CURRENT_KEY_SCHEME: KeyScheme = KeyScheme::V1;

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
    fn test_default_key_scheme_is_v0() {
        assert_eq!(KeyScheme::default(), KeyScheme::V0);
    }
}
