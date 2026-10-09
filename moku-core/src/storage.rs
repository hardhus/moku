mod envelope;
mod manager;

pub use envelope::{
    CURRENT_KEY_SCHEME, CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, StorageEnvelope,
    StorageType,
};
pub use manager::{MigrationReport, StorageManager};
// Temporary facade (P2 split): the policy moved to moku-config with the
// config it reads.
pub use moku_config::resolve_encryption;
