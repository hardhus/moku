use moku_crypto::{BLOCK_NONCE_LEN, BlockAead, CryptoError};
use zeroize::Zeroizing;

use crate::types::{VResult as Result, VolumeFsError};

pub const BLOCK_SIZE: usize = 4096;
pub const NONCE_LEN: usize = BLOCK_NONCE_LEN;
pub const TAG_LEN: usize = moku_crypto::TAG_LEN;
pub const FULL_BLOCK_DISK_SIZE: u64 = (NONCE_LEN + BLOCK_SIZE + TAG_LEN) as u64;

/// The MKV1 block layout on top of the volume's `BlockAead` (keyed once
/// when the volume opens): a block on disk is `nonce(12) || ct || tag`,
/// and its AAD is `file_id(16) || block_idx (u64 BE)`.
pub struct BlockCipher<'a> {
    aead: &'a BlockAead,
}

impl<'a> BlockCipher<'a> {
    pub fn new(aead: &'a BlockAead) -> Self {
        Self { aead }
    }

    fn aad(file_id: &[u8; 16], block_idx: u64) -> [u8; 24] {
        let mut aad = [0u8; 24];
        aad[..16].copy_from_slice(file_id);
        aad[16..].copy_from_slice(&block_idx.to_be_bytes());
        aad
    }

    /// Encrypts one block's plaintext (<=4096 bytes) into its on-disk form.
    /// AAD binds the block to its owning file and position, so a block can
    /// never be spliced into another file or moved to a different offset
    /// undetected.
    pub fn encrypt_block(&self, file_id: &[u8; 16], block_idx: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
        debug_assert!(plaintext.len() <= BLOCK_SIZE);
        Ok(self.aead.seal_block(&Self::aad(file_id, block_idx), plaintext)?)
    }

    /// Decrypts one on-disk block back into plaintext.
    pub fn decrypt_block(
        &self,
        file_id: &[u8; 16],
        block_idx: u64,
        disk_bytes: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>> {
        match self.aead.open_block(&Self::aad(file_id, block_idx), disk_bytes) {
            Ok(plain) => Ok(plain),
            Err(CryptoError::Malformed(_)) => {
                Err(VolumeFsError::Corrupt("content block shorter than nonce + tag".into()))
            }
            Err(CryptoError::AuthFailed) => Err(VolumeFsError::Integrity("file content block")),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moku_crypto::{Key, VolumeContentKey};

    fn aead() -> BlockAead {
        BlockAead::new(&Key::<VolumeContentKey>::from_slice(&[9u8; 32]).unwrap())
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let a = aead();
        let cipher = BlockCipher::new(&a);
        let file_id = [1u8; 16];
        let plaintext = b"hello moku vault";

        let disk = cipher.encrypt_block(&file_id, 0, plaintext).unwrap();
        assert_eq!(disk.len(), NONCE_LEN + plaintext.len() + TAG_LEN);

        let back = cipher.decrypt_block(&file_id, 0, &disk).unwrap();
        assert_eq!(&back[..], plaintext);
    }

    #[test]
    fn test_wrong_block_idx_fails_aad_check() {
        let a = aead();
        let cipher = BlockCipher::new(&a);
        let file_id = [1u8; 16];
        let disk = cipher.encrypt_block(&file_id, 3, b"data").unwrap();
        assert!(matches!(
            cipher.decrypt_block(&file_id, 4, &disk),
            Err(VolumeFsError::Integrity(_))
        ));
    }

    #[test]
    fn test_wrong_file_id_fails_aad_check() {
        let a = aead();
        let cipher = BlockCipher::new(&a);
        let disk = cipher.encrypt_block(&[1u8; 16], 0, b"data").unwrap();
        assert!(matches!(
            cipher.decrypt_block(&[2u8; 16], 0, &disk),
            Err(VolumeFsError::Integrity(_))
        ));
    }

    #[test]
    fn test_short_block_is_corrupt() {
        let a = aead();
        let cipher = BlockCipher::new(&a);
        assert!(matches!(
            cipher.decrypt_block(&[1u8; 16], 0, &[0u8; 10]),
            Err(VolumeFsError::Corrupt(_))
        ));
    }

    #[test]
    fn test_two_encryptions_use_different_nonces() {
        let a = aead();
        let cipher = BlockCipher::new(&a);
        let file_id = [1u8; 16];
        let x = cipher.encrypt_block(&file_id, 0, b"same plaintext!!").unwrap();
        let y = cipher.encrypt_block(&file_id, 0, b"same plaintext!!").unwrap();
        assert_ne!(x, y, "nonce reuse would make ciphertexts identical");
    }
}
