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

/// Which key encrypted this envelope's `payload`, when `status ==
/// Encrypted` — meaningless (and left at its default) for `Plaintext`
/// envelopes. `#[serde(default)]` means every envelope written before this
/// field existed deserializes as `Legacy`, which is exactly correct: it
/// really was encrypted directly under the raw vault master key, the only
/// scheme that existed at the time. This is what lets `StorageManager`
/// keep reading every pre-existing user's data forever with zero action
/// required, while every new write moves to `PerModuleV1`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyScheme {
    /// Encrypted directly under the raw vault master key (pre-migration
    /// behavior — never used for new writes anymore).
    #[default]
    Legacy,
    /// Encrypted under `HKDF(master, "moku-core/storage/<module_id>/v1")`
    /// (see `storage::keys::derive_module_storage_key`) — never the raw
    /// master key directly.
    PerModuleV1,
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
