// Temporary facade while P2 splits moku-core: storage now lives in
// moku-store, and the encryption policy in moku-config. Removed once every
// crate imports them directly.
pub use moku_config::resolve_encryption;
pub use moku_store::{
    CURRENT_KEY_SCHEME, CURRENT_SCHEMA_VERSION, EncryptionStatus, KeyScheme, MigrationReport,
    StorageEnvelope, StorageManager, StorageType,
};
