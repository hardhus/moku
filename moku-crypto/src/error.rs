/// Why a cryptographic operation failed. Deliberately coarse: in
/// particular `AuthFailed` never says whether the key was wrong or the
/// data was tampered with — an AEAD can't tell the two apart, and callers
/// that can (the vault's canary check) give it meaning themselves.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("decryption failed: wrong key, or the data was modified")]
    AuthFailed,
    #[error("invalid key length")]
    InvalidKeyLength,
    #[error("malformed ciphertext: {0}")]
    Malformed(&'static str),
    #[error("encryption failed")]
    EncryptFailed,
    #[error("key derivation failed: {0}")]
    Kdf(String),
    /// The key isn't available: the vault is locked (`KeyCell` wiped).
    #[error("the key is locked")]
    Locked,
    /// A self-describing ciphertext names an algorithm this build doesn't
    /// know.
    #[error("unsupported algorithm id {0}")]
    Unsupported(u8),
}
