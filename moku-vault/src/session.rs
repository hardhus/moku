use moku_crypto::{CryptoError, Key, KeyCell, RootKey};

/// Thread-safe, global source of truth for the vault's unlock state.
///
/// The root key lives in a `KeyCell`: it is borrowed for the length of a
/// closure (`with`), never handed out, so `lock()` really wipes it.
pub struct VaultSession {
    key: KeyCell<RootKey>,
}

impl VaultSession {
    pub fn new() -> Self {
        Self {
            key: KeyCell::new(),
        }
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.is_set()
    }

    pub fn unlock(&self, key: Key<RootKey>) {
        self.key.set(key);
    }

    /// Zeroizes the root key in place; waits for running `with` calls.
    pub fn lock(&self) {
        self.key.wipe();
    }

    /// Runs `f` with the root key, or `CryptoError::Locked`. Keep `f`
    /// short: derive what you need (a module or volume key) and return it.
    pub fn with<R>(&self, f: impl FnOnce(&Key<RootKey>) -> R) -> Result<R, CryptoError> {
        self.key.with(f)
    }

    /// Changes on every unlock and lock.
    pub fn generation(&self) -> u64 {
        self.key.generation()
    }
}

impl Default for VaultSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::SecurityManager;

    #[tokio::test]
    async fn test_vault_session_lifecycle() {
        let session = VaultSession::new();
        assert!(!session.is_unlocked());
        assert_eq!(session.with(|_| ()), Err(CryptoError::Locked));

        let key = SecurityManager::derive_key(&"pass".into(), &[0u8; 16])
            .await
            .unwrap();
        let expected = *key.expose();
        session.unlock(key);

        assert!(session.is_unlocked());
        assert_eq!(session.with(|k| *k.expose()), Ok(expected));

        session.lock();
        assert!(!session.is_unlocked());
        assert_eq!(session.with(|_| ()), Err(CryptoError::Locked));
    }
}
