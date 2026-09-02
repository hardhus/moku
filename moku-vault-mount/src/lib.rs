//! Thin, platform-specific OS-mount shims over `moku-vault-fs`'s
//! platform-agnostic `VolumeEngine`. Each shim's job is only to translate
//! one OS filesystem-driver callback API into calls on the shared engine —
//! all crypto/path/quota logic lives in `moku-vault-fs` (plan §2).
//!
//! Windows-first: WinFsp is what's actually installed and testable in
//! this project's own dev environment. The FUSE/Unix shim is a later
//! pass and is not implemented yet.

#[cfg(windows)]
mod winfsp_shim;

#[cfg(windows)]
pub use winfsp_shim::mount_and_wait;

#[cfg(unix)]
mod fuse_shim;

#[cfg(unix)]
pub use fuse_shim::mount_and_wait;
