//! `seal`/`open`: the AEAD every new moku format uses.
//!
//! Output is self-describing: `alg (1) || nonce (12 or 24) || ct || tag (16)`.
//! New formats use XChaCha20-Poly1305 (random 192-bit nonces, no
//! per-key message limit to track; audit KN-3.1). AAD is mandatory: an
//! `Aad` can only be built from a domain plus context fields.

use aes_gcm::Aes256Gcm;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use zeroize::Zeroizing;

use crate::wipe::WipeOnDrop;
use crate::{Aad, AeadKey, CryptoError, Key, random};

/// AEAD algorithm id, the first byte of a `seal` output.
///
/// Ids are permanent. A future cascade mode (several ciphers in sequence,
/// chosen in config) gets new ids here as "suites"; old ids keep their
/// meaning forever.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    Aes256Gcm = 1,
    XChaCha20Poly1305 = 2,
}

pub const TAG_LEN: usize = 16;

impl Alg {
    pub fn id(self) -> u8 {
        self as u8
    }

    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            1 => Some(Alg::Aes256Gcm),
            2 => Some(Alg::XChaCha20Poly1305),
            _ => None,
        }
    }

    pub fn nonce_len(self) -> usize {
        match self {
            Alg::Aes256Gcm => 12,
            Alg::XChaCha20Poly1305 => 24,
        }
    }
}

/// Cipher states are boxed and wiped after use (`WipeOnDrop`).
pub(crate) fn gcm(key: &[u8; 32]) -> WipeOnDrop<Aes256Gcm> {
    WipeOnDrop::new(Aes256Gcm::new(GenericArray::from_slice(key)))
}

fn xchacha(key: &[u8; 32]) -> WipeOnDrop<XChaCha20Poly1305> {
    WipeOnDrop::new(XChaCha20Poly1305::new(GenericArray::from_slice(key)))
}

/// Encrypts `plaintext` under `key`, bound to `aad`.
pub fn seal<K: AeadKey>(
    key: &Key<K>,
    alg: Alg,
    aad: &Aad,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut nonce = [0u8; 24];
    let nonce = &mut nonce[..alg.nonce_len()];
    random::fill_random(nonce);
    seal_raw(key.expose(), alg, nonce, aad.as_bytes(), plaintext)
}

/// Decrypts a `seal` output. Wrong key, wrong AAD or any modified byte is
/// `AuthFailed`; an unknown algorithm id is `Unsupported`; input too short
/// to hold a nonce and tag is `Malformed`.
pub fn open<K: AeadKey>(
    key: &Key<K>,
    aad: &Aad,
    sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    open_raw(key.expose(), aad.as_bytes(), sealed)
}

fn seal_raw(
    key: &[u8; 32],
    alg: Alg,
    nonce: &[u8],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let payload = Payload {
        msg: plaintext,
        aad,
    };
    let ct = match alg {
        Alg::Aes256Gcm => gcm(key).encrypt(GenericArray::from_slice(nonce), payload),
        Alg::XChaCha20Poly1305 => xchacha(key).encrypt(GenericArray::from_slice(nonce), payload),
    }
    .map_err(|_| CryptoError::EncryptFailed)?;
    let mut out = Vec::with_capacity(1 + nonce.len() + ct.len());
    out.push(alg.id());
    out.extend_from_slice(nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open_raw(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    let (&id, rest) = sealed
        .split_first()
        .ok_or(CryptoError::Malformed("empty ciphertext"))?;
    let alg = Alg::from_id(id).ok_or(CryptoError::Unsupported(id))?;
    if rest.len() < alg.nonce_len() + TAG_LEN {
        return Err(CryptoError::Malformed("shorter than its nonce and tag"));
    }
    let (nonce, ct) = rest.split_at(alg.nonce_len());
    let payload = Payload { msg: ct, aad };
    let pt = match alg {
        Alg::Aes256Gcm => gcm(key).decrypt(GenericArray::from_slice(nonce), payload),
        Alg::XChaCha20Poly1305 => xchacha(key).decrypt(GenericArray::from_slice(nonce), payload),
    }
    .map_err(|_| CryptoError::AuthFailed)?;
    Ok(Zeroizing::new(pt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExportKey, ModuleStorageKey};

    fn key() -> Key<ExportKey> {
        Key::from_slice(&[9u8; 32]).unwrap()
    }

    #[test]
    fn test_roundtrip_both_algorithms() {
        let aad = Aad::new("moku/test/v1").field(b"rec-1");
        for alg in [Alg::Aes256Gcm, Alg::XChaCha20Poly1305] {
            let sealed = seal(&key(), alg, &aad, b"foo bar").unwrap();
            assert_eq!(sealed[0], alg.id());
            assert_eq!(sealed.len(), 1 + alg.nonce_len() + 7 + TAG_LEN);
            assert_eq!(&open(&key(), &aad, &sealed).unwrap()[..], b"foo bar");
        }
    }

    #[test]
    fn test_nonces_are_fresh() {
        let aad = Aad::new("moku/test/v1");
        let a = seal(&key(), Alg::XChaCha20Poly1305, &aad, b"same").unwrap();
        let b = seal(&key(), Alg::XChaCha20Poly1305, &aad, b"same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn test_wrong_key_aad_or_modified_bytes_fail_auth() {
        let aad = Aad::new("moku/test/v1").field(b"rec-1");
        let sealed = seal(&key(), Alg::XChaCha20Poly1305, &aad, b"foo").unwrap();
        let other: Key<ModuleStorageKey> = Key::from_slice(&[8u8; 32]).unwrap();
        assert_eq!(open(&other, &aad, &sealed), Err(CryptoError::AuthFailed));
        let wrong_aad = Aad::new("moku/test/v1").field(b"rec-2");
        assert_eq!(open(&key(), &wrong_aad, &sealed), Err(CryptoError::AuthFailed));
        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert_eq!(open(&key(), &aad, &flipped), Err(CryptoError::AuthFailed));
    }

    #[test]
    fn test_unknown_alg_and_short_input() {
        let aad = Aad::new("moku/test/v1");
        assert_eq!(open(&key(), &aad, &[7u8; 64]), Err(CryptoError::Unsupported(7)));
        assert!(matches!(open(&key(), &aad, &[]), Err(CryptoError::Malformed(_))));
        assert!(matches!(open(&key(), &aad, &[2u8; 30]), Err(CryptoError::Malformed(_))));
    }

    /// draft-arciszewski-xchacha-03 Appendix A.1 (the vector the
    /// chacha20poly1305 crate itself tests against).
    #[test]
    fn test_xchacha20poly1305_draft_vector() {
        let key: [u8; 32] = std::array::from_fn(|i| 0x80 + i as u8);
        let nonce: [u8; 24] = std::array::from_fn(|i| 0x40 + i as u8);
        let aad = [0x50, 0x51, 0x52, 0x53, 0xc0, 0xc1, 0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7];
        let pt = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let expected_ct_tag = "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b4522f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52ec0875924c1c7987947deafd8780acf49";
        let sealed = seal_raw(&key, Alg::XChaCha20Poly1305, &nonce, &aad, pt).unwrap();
        let hex: String = sealed[25..].iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected_ct_tag);
        assert_eq!(&open_raw(&key, &aad, &sealed).unwrap()[..], pt);
    }
}
