use std::fmt;
use std::marker::PhantomData;

use zeroize::Zeroize;

use crate::{CryptoError, random};

mod sealed {
    pub trait Sealed {}
}

/// What a key is for. A role is a type parameter of `Key`, so a key meant
/// only as HKDF input (`RootKey`, `PasswordKey`, `VolumeMasterKey`) can't
/// be handed to `seal`/`open` by mistake: only `AeadKey` roles can.
/// Sealed: new roles are added here, next to the derivation that makes them.
pub trait KeyRole: sealed::Sealed + Send + Sync + 'static {
    /// Shown by `Debug` instead of the key bytes.
    const NAME: &'static str;
}

/// Roles that may key an AEAD (`seal`/`open`, `BlockAead`).
pub trait AeadKey: KeyRole {}

macro_rules! roles {
    ($($(#[$doc:meta])* $name:ident $(: $aead:ident)?;)*) => {$(
        $(#[$doc])*
        pub enum $name {}
        impl sealed::Sealed for $name {}
        impl KeyRole for $name {
            const NAME: &'static str = stringify!($name);
        }
        $(impl $aead for $name {})?
    )*};
}

roles! {
    /// The app vault's master key (P5: the DEK). Only ever HKDF input,
    /// never a cipher key (old V0 records and the canary are the
    /// `format_v1` exception).
    RootKey;
    /// Raw Argon2id output. Only HKDF input (or adopted as a root key by
    /// the explicitly named `format_v1` conversions).
    PasswordKey;
    /// Wraps a DEK in a keyslot (P5).
    Kek: AeadKey;
    /// Per-module storage key (`Purpose::ModuleStorageV1`).
    ModuleStorageKey: AeadKey;
    /// One volume's master key. Only HKDF input for the volume's content
    /// and name keys.
    VolumeMasterKey;
    /// A volume's AES-256-GCM block key.
    VolumeContentKey: AeadKey;
    /// A volume's AES-256-SIV filename key pair (64 bytes).
    VolumeNameKey;
    /// Key of a secrets export file (MSXP v2).
    ExportKey: AeadKey;
    /// Recovery-key entropy (P5); only HKDF input for a keyslot KEK.
    RecoverySecret;
}

/// The vault's data-encryption key from P5 on; today the same value as
/// the root key.
pub type Dek = Key<RootKey>;

/// Fixed-size secret key material of role `P`.
///
/// - The bytes live on the heap, so moving a `Key` never leaves a copy on
///   the stack, and are wiped on drop.
/// - No `Clone`: a copy is made only through the visible `duplicate()`.
/// - `Debug` prints the role, never the bytes.
pub struct Key<P: KeyRole, const N: usize = 32> {
    bytes: Box<[u8; N]>,
    _role: PhantomData<P>,
}

/// Every `Key` allocation goes through here. P9 (KN-3.3) switches this
/// one place to locked, non-pageable memory.
fn alloc_key_box<const N: usize>() -> Box<[u8; N]> {
    Box::new([0u8; N])
}

impl<P: KeyRole, const N: usize> Key<P, N> {
    fn zeroed() -> Self {
        Self {
            bytes: alloc_key_box(),
            _role: PhantomData,
        }
    }

    /// Builds a key by letting `fill` write straight into its heap buffer.
    /// On error the partially written buffer is wiped like any other key.
    pub(crate) fn from_fn(
        fill: impl FnOnce(&mut [u8; N]) -> Result<(), CryptoError>,
    ) -> Result<Self, CryptoError> {
        let mut key = Self::zeroed();
        fill(&mut key.bytes)?;
        Ok(key)
    }

    /// A fresh key from the OS random number generator.
    pub fn random() -> Self {
        let mut key = Self::zeroed();
        random::fill_random(&mut key.bytes[..]);
        key
    }

    /// Copies key material received over a trusted channel (the mount
    /// worker's stdin, a test vector) straight into a new key.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != N {
            return Err(CryptoError::InvalidKeyLength);
        }
        let mut key = Self::zeroed();
        key.bytes.copy_from_slice(bytes);
        Ok(key)
    }

    /// The raw bytes, for the few places that must hand them on: a cipher
    /// constructor inside moku-crypto, the worker stdin frame, a KAT.
    pub fn expose(&self) -> &[u8; N] {
        &self.bytes
    }

    /// An explicit, visible copy of this key.
    pub fn duplicate(&self) -> Self {
        let mut key = Self::zeroed();
        key.bytes.copy_from_slice(&self.bytes[..]);
        key
    }

    /// Relabels the key as another role without copying it. Only the
    /// explicitly named conversions in `format_v1` use this.
    pub(crate) fn into_role<Q: KeyRole>(mut self) -> Key<Q, N> {
        let bytes = std::mem::replace(&mut self.bytes, alloc_key_box());
        Key {
            bytes,
            _role: PhantomData,
        }
    }
}

impl<P: KeyRole, const N: usize> Drop for Key<P, N> {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl<P: KeyRole, const N: usize> fmt::Debug for Key<P, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key<{}>(***REDACTED***)", P::NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_slice_checks_the_length() {
        assert!(Key::<RootKey>::from_slice(&[1u8; 31]).is_err());
        let key = Key::<RootKey>::from_slice(&[1u8; 32]).unwrap();
        assert_eq!(key.expose(), &[1u8; 32]);
        assert!(Key::<VolumeNameKey, 64>::from_slice(&[2u8; 64]).is_ok());
    }

    #[test]
    fn test_duplicate_keeps_the_bytes() {
        let key = Key::<PasswordKey>::from_slice(&[3u8; 32]).unwrap();
        assert_eq!(key.duplicate().expose(), key.expose());
    }

    #[test]
    fn test_random_keys_differ() {
        assert_ne!(
            Key::<RootKey>::random().expose(),
            Key::<RootKey>::random().expose()
        );
    }

    #[test]
    fn test_debug_never_prints_the_bytes() {
        let key = Key::<ModuleStorageKey>::from_slice(&[0xab; 32]).unwrap();
        let shown = format!("{key:?}");
        assert_eq!(shown, "Key<ModuleStorageKey>(***REDACTED***)");
    }
}
