// Temporary facade while P2 splits moku-core: the vault now lives in
// moku-vault. Removed once every crate imports moku_vault directly.
pub use moku_vault::{SafeKey, SecurityManager, StorageMetadata, VaultSession, cli};
