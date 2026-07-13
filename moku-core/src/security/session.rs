use std::sync::Arc;

use arc_swap::ArcSwapOption;
use secrecy::SecretBox;

use crate::security::manager::SafeKey;

/// Thread-safe, global source of truth for the vault's unlock state.
/// Shares the key pointer via `Arc` without copying the underlying secret.
pub struct VaultSession {
    key: ArcSwapOption<SecretBox<SafeKey>>,
}

impl VaultSession {
    pub fn new() -> Self {
        Self {
            key: ArcSwapOption::from(None),
        }
    }

    pub fn is_unlocked(&self) -> bool {
        self.key.load().is_some()
    }

    pub fn unlock(&self, key: SecretBox<SafeKey>) {
        self.key.store(Some(Arc::new(key)));
    }

    pub fn lock(&self) {
        self.key.store(None);
    }

    pub fn current(&self) -> Option<Arc<SecretBox<SafeKey>>> {
        self.key.load_full()
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
    use crate::security::manager::SecurityManager;

    #[tokio::test]
    async fn test_vault_session_lifecycle() {
        let session = VaultSession::new();
        assert!(!session.is_unlocked());
        assert!(session.current().is_none());

        let key = SecurityManager::derive_key("pass", &[0u8; 16])
            .await
            .unwrap();
        session.unlock(key);

        assert!(session.is_unlocked());
        assert!(session.current().is_some());

        session.lock();
        assert!(!session.is_unlocked());
    }
}
