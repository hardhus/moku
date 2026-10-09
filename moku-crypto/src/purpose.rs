//! Every HKDF derivation in moku, and the only place its info strings are
//! built.
//!
//! **Rule (audit KN-3.2):** a new derivation is a new `Purpose` variant,
//! a named public function below that takes and returns the right roles,
//! and a known-answer test pinning its info bytes and output. The generic
//! `derive` stays crate-private, so callers can't invent ad-hoc
//! derivations or mix up roles.
//!
//! **Info strings:**
//! - The four `...V1` strings that existed before moku-crypto are
//!   permanent and byte-for-byte fixed, odd shapes included: three still
//!   say "vault" (they predate the vault -> volume rename) and the
//!   default-volume one has its version in the middle. Every existing
//!   record and volume is keyed by them.
//! - New strings follow `moku/<area>/<purpose>/v<N>`, followed by each
//!   context field as `len (u16 big-endian) || bytes`, which keeps them
//!   unambiguous. Bump `v<N>` (never edit a string) when a derivation must
//!   change.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::{
    Key, KeyRole, ModuleStorageKey, RootKey, VolumeContentKey, VolumeMasterKey, VolumeNameKey,
};

/// Which kind of keyslot a KEK opens (P5).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Password,
    Recovery,
}

impl SlotKind {
    fn label(self) -> &'static [u8] {
        match self {
            SlotKind::Password => b"password",
            SlotKind::Recovery => b"recovery",
        }
    }
}

/// What an HKDF output is for. Determines the info string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose<'a> {
    /// `moku-core/storage/<module_id>/v1` (32 B, from the root key).
    ModuleStorageV1 { module_id: &'a str },
    /// `moku-vault-daemon/default-volume/v1/<volume_id>` (32 B, from the
    /// root key).
    DefaultVolumeV1 { volume_id: &'a str },
    /// `moku-vault-fs/content-key/v1` (32 B, from a volume master key).
    VolumeContentV1,
    /// `moku-vault-fs/name-key/v1` (64 B, from a volume master key).
    VolumeNameV1,
    /// `moku/keyslot/kek/v1 || lp(kind)` (32 B, from a password key or
    /// recovery secret).
    KeyslotKek { kind: SlotKind },
    /// `moku/secrets/export/v2` (32 B, from an export password key).
    SecretsExportV2,
}

impl Purpose<'_> {
    pub fn info(&self) -> Vec<u8> {
        match *self {
            Purpose::ModuleStorageV1 { module_id } => {
                [b"moku-core/storage/", module_id.as_bytes(), b"/v1"].concat()
            }
            Purpose::DefaultVolumeV1 { volume_id } => {
                [b"moku-vault-daemon/default-volume/v1/", volume_id.as_bytes()].concat()
            }
            Purpose::VolumeContentV1 => b"moku-vault-fs/content-key/v1".to_vec(),
            Purpose::VolumeNameV1 => b"moku-vault-fs/name-key/v1".to_vec(),
            Purpose::KeyslotKek { kind } => new_style(b"moku/keyslot/kek/v1", &[kind.label()]),
            Purpose::SecretsExportV2 => new_style(b"moku/secrets/export/v2", &[]),
        }
    }
}

/// `label || (len_u16_be || field)*`.
fn new_style(label: &[u8], fields: &[&[u8]]) -> Vec<u8> {
    let mut info = label.to_vec();
    for field in fields {
        let len = u16::try_from(field.len()).expect("purpose context fields are short constants");
        info.extend_from_slice(&len.to_be_bytes());
        info.extend_from_slice(field);
    }
    info
}

/// One HKDF-SHA256 extract (no salt) and one expand.
pub(crate) fn derive<O: KeyRole, const N: usize>(ikm: &[u8], purpose: Purpose<'_>) -> Key<O, N> {
    const { assert!(N <= 255 * 32, "longer than HKDF-SHA256 can produce") };
    let info = purpose.info();
    let hk = Hkdf::<Sha256>::new(None, ikm);
    Key::from_fn(|out| {
        hk.expand(&info, out)
            .map_err(|_| crate::CryptoError::Kdf("invalid HKDF output length".into()))
    })
    .expect("N is a valid HKDF-SHA256 output length (checked above)")
}

/// A module's storage key (`KeyScheme::V1`).
pub fn module_storage_key(root: &Key<RootKey>, module_id: &str) -> Key<ModuleStorageKey> {
    derive(root.expose(), Purpose::ModuleStorageV1 { module_id })
}

/// A Default-mode volume's master key, derived from the app's root key so
/// that two volumes never share one.
pub fn default_volume_master(root: &Key<RootKey>, volume_id: &str) -> Key<VolumeMasterKey> {
    derive(root.expose(), Purpose::DefaultVolumeV1 { volume_id })
}

/// A volume's two data keys: AES-256-GCM content key and AES-256-SIV name
/// key pair.
pub fn volume_keys(
    master: &Key<VolumeMasterKey>,
) -> (Key<VolumeContentKey>, Key<VolumeNameKey, 64>) {
    (
        derive(master.expose(), Purpose::VolumeContentV1),
        derive(master.expose(), Purpose::VolumeNameV1),
    )
}
