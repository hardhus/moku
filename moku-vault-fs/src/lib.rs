//! Platform-agnostic encrypted-volume engine.
//!
//! This crate knows nothing about FUSE or WinFsp — it only implements the
//! on-disk format (block-encrypted file content, AES-SIV-encrypted names,
//! per-directory IVs) and a synchronous [`engine::VolumeEngine`] API that a
//! thin OS-specific mount shim (`moku-vault-mount`) drives. See the plan
//! doc "Moku: Şifreli Sürücü (FUSE/WinFsp) + satz Not Modülü", Bölüm A.

pub mod block_cipher;
pub mod content;
pub mod engine;
pub mod keys;
pub mod names;
pub mod pathmap;
pub mod quota;
pub mod types;

pub use engine::VolumeEngine;
pub use keys::{VolumeKeys, derive_volume_keys};
pub use types::{Attr, DirEntry, FileKind, VResult, VaultFsError, VirtualPath};
