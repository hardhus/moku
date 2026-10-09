mod aad;
mod aead;
mod block;
mod error;
pub mod format_v1;
pub mod kdf;
mod key;
mod password;
pub mod purpose;
pub mod random;
mod seal;
mod secret;
mod subkey;
mod wipe;

pub use aad::Aad;
pub use aead::{decrypt, encrypt};
pub use block::{BLOCK_NONCE_LEN, BlockAead, NameSiv};
pub use error::CryptoError;
pub use key::{
    AeadKey, Dek, ExportKey, Kek, Key, KeyRole, ModuleStorageKey, PasswordKey, RecoverySecret,
    RootKey, SafeKey, VolumeContentKey, VolumeMasterKey, VolumeNameKey,
};
pub use kdf::{KdfParams, password_key};
pub use password::{argon2id_key, generate_salt};
pub use purpose::{default_volume_master, module_storage_key, volume_keys};
pub use seal::{Alg, TAG_LEN, open, seal};
pub use secret::SecretString;
pub use subkey::derive_module_storage_key;
