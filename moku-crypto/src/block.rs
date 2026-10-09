//! Low-level primitives for the volume format (MKV1). The on-disk layout
//! (block AAD = `file_id || block_idx`, name AD = parent directory IV)
//! stays in moku-volume-fs; these only hold the keyed ciphers.

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::aead::{Aead, Payload};
use aes_siv::KeyInit;
use aes_siv::siv::Aes256Siv;
use zeroize::Zeroizing;

use crate::seal::{TAG_LEN, gcm};
use crate::wipe::WipeOnDrop;
use crate::{CryptoError, Key, VolumeContentKey, VolumeNameKey, random};

pub const BLOCK_NONCE_LEN: usize = 12;

/// AES-256-GCM for volume content blocks, keyed once per opened volume
/// (not once per block) and wiped when the volume closes.
///
/// A block on disk is `nonce (12) || ct || tag (16)`, byte-compatible
/// with every MKV1 volume written so far.
pub struct BlockAead {
    cipher: WipeOnDrop<Aes256Gcm>,
}

impl BlockAead {
    pub fn new(key: &Key<VolumeContentKey>) -> Self {
        Self {
            cipher: gcm(key.expose()),
        }
    }

    pub fn seal_block(&self, aad: &[u8; 24], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let nonce = random::random_array::<BLOCK_NONCE_LEN>();
        let ct = self
            .cipher
            .encrypt(
                GenericArray::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| CryptoError::EncryptFailed)?;
        let mut out = Vec::with_capacity(BLOCK_NONCE_LEN + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open_block(
        &self,
        aad: &[u8; 24],
        disk: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        if disk.len() < BLOCK_NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Malformed("content block shorter than nonce + tag"));
        }
        let (nonce, ct) = disk.split_at(BLOCK_NONCE_LEN);
        let pt = self
            .cipher
            .decrypt(GenericArray::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| CryptoError::AuthFailed)?;
        Ok(Zeroizing::new(pt))
    }
}

/// Deterministic AES-256-SIV for volume file names, with the parent
/// directory's IV as associated data. `Aes256Siv` needs `&mut self`, so the
/// cipher is built per call (as before; names are not a hot path) inside
/// `WipeOnDrop`.
pub struct NameSiv {
    key: Key<VolumeNameKey, 64>,
}

impl NameSiv {
    pub fn new(key: Key<VolumeNameKey, 64>) -> Self {
        Self { key }
    }

    fn cipher(&self) -> WipeOnDrop<Aes256Siv> {
        WipeOnDrop::new(Aes256Siv::new(GenericArray::from_slice(self.key.expose())))
    }

    pub fn seal(&self, dir_iv: &[u8; 16], name: &[u8]) -> Result<Vec<u8>, CryptoError> {
        self.cipher()
            .encrypt([dir_iv.as_slice()], name)
            .map_err(|_| CryptoError::EncryptFailed)
    }

    pub fn open(&self, dir_iv: &[u8; 16], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
        self.cipher()
            .decrypt([dir_iv.as_slice()], sealed)
            .map(Zeroizing::new)
            .map_err(|_| CryptoError::AuthFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aad(idx: u64) -> [u8; 24] {
        let mut a = [0u8; 24];
        a[..16].copy_from_slice(&[1u8; 16]);
        a[16..].copy_from_slice(&idx.to_be_bytes());
        a
    }

    #[test]
    fn test_block_roundtrip_and_aad_binding() {
        let key = Key::<VolumeContentKey>::from_slice(&[9u8; 32]).unwrap();
        let block = BlockAead::new(&key);
        let disk = block.seal_block(&aad(3), b"hello").unwrap();
        assert_eq!(disk.len(), BLOCK_NONCE_LEN + 5 + TAG_LEN);
        assert_eq!(&block.open_block(&aad(3), &disk).unwrap()[..], b"hello");
        assert_eq!(block.open_block(&aad(4), &disk), Err(CryptoError::AuthFailed));
        assert!(matches!(block.open_block(&aad(3), &[0u8; 27]), Err(CryptoError::Malformed(_))));
    }

    /// The pre-P3 volume code encrypted each block with a fresh
    /// `Aes256Gcm` and the same nonce/AAD layout; `BlockAead` must open
    /// such blocks and vice versa.
    #[test]
    fn test_block_layout_matches_a_plain_aes_gcm_block() {
        let key_bytes = [9u8; 32];
        let key = Key::<VolumeContentKey>::from_slice(&key_bytes).unwrap();
        let block = BlockAead::new(&key);
        let plain = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        let nonce = [7u8; 12];
        let ct = plain
            .encrypt(GenericArray::from_slice(&nonce), Payload { msg: b"old", aad: &aad(0) })
            .unwrap();
        let disk = [nonce.as_slice(), &ct].concat();
        assert_eq!(&block.open_block(&aad(0), &disk).unwrap()[..], b"old");

        let new_disk = block.seal_block(&aad(1), b"new").unwrap();
        let (n, c) = new_disk.split_at(12);
        let back = plain
            .decrypt(GenericArray::from_slice(n), Payload { msg: c, aad: &aad(1) })
            .unwrap();
        assert_eq!(back, b"new");
    }

    #[test]
    fn test_name_siv_is_deterministic_and_bound_to_the_dir_iv() {
        let siv = NameSiv::new(Key::from_slice(&[5u8; 64]).unwrap());
        let a = siv.seal(&[1u8; 16], b"todo.md").unwrap();
        assert_eq!(a, siv.seal(&[1u8; 16], b"todo.md").unwrap());
        assert_ne!(a, siv.seal(&[2u8; 16], b"todo.md").unwrap());
        assert_eq!(&siv.open(&[1u8; 16], &a).unwrap()[..], b"todo.md");
        assert_eq!(siv.open(&[2u8; 16], &a), Err(CryptoError::AuthFailed));
    }
}
