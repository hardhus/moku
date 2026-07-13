use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::MokuConfig;
use crate::module::ModuleId;
use crate::security::{SecurityManager, VaultSession};
use crate::storage::StorageManager;
use crate::toast::ToastType;

/// Shared state passed to every TUI module.
/// Uses lock-free config reads (`ctx.config.load()`). Call `.load()` inline
/// instead of storing it to avoid borrow-checker conflicts.
pub struct AppContext {
    pub config: Arc<ArcSwap<MokuConfig>>,
    pub session: Arc<VaultSession>,
    pub security: Arc<SecurityManager>,
    pub storage: Arc<StorageManager>,

    should_quit: bool,
    pending_navigation: Option<ModuleId>,
    pending_toasts: Vec<(String, ToastType)>,
}

impl AppContext {
    pub fn new(
        config: Arc<ArcSwap<MokuConfig>>,
        session: Arc<VaultSession>,
        security: Arc<SecurityManager>,
        storage: Arc<StorageManager>,
    ) -> Self {
        Self {
            config,
            session,
            security,
            storage,
            should_quit: false,
            pending_navigation: None,
            pending_toasts: Vec::new(),
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    pub fn update_config<F>(&self, mutate: F)
    where
        F: FnOnce(&mut MokuConfig),
    {
        let mut new_config = (**self.config.load()).clone();
        mutate(&mut new_config);
        self.config.store(Arc::new(new_config));
    }

    pub fn navigate_to(&mut self, id: ModuleId) {
        self.pending_navigation = Some(id);
    }

    pub fn show_info(&mut self, msg: impl Into<String>) {
        self.pending_toasts.push((msg.into(), ToastType::Info));
    }

    pub fn show_warning(&mut self, msg: impl Into<String>) {
        self.pending_toasts.push((msg.into(), ToastType::Warning));
    }

    pub fn show_error(&mut self, msg: impl Into<String>) {
        self.pending_toasts.push((msg.into(), ToastType::Error));
    }

    // --- Internal accessors for moku-bin app_loop ---

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub fn take_navigation(&mut self) -> Option<ModuleId> {
        self.pending_navigation.take()
    }

    pub fn drain_toasts(&mut self) -> Vec<(String, ToastType)> {
        std::mem::take(&mut self.pending_toasts)
    }
}

/// Context for one-off CLI commands. No event loop, no Arc/ArcSwap wrapper required.
pub struct CliContext {
    pub config: MokuConfig,
    pub storage: Option<Arc<StorageManager>>,
}

/// Context for periodic tasks in the moku-daemon.
pub struct DaemonContext {
    pub config: Arc<ArcSwap<MokuConfig>>,
    pub storage: Arc<StorageManager>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityManager;
    use tempfile::tempdir;

    async fn test_ctx() -> AppContext {
        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let temp = tempdir().unwrap();
        let security = Arc::new(SecurityManager::new_with_root(temp.path().to_path_buf()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), temp.path().to_path_buf())
                .await
                .unwrap(),
        );
        // Keep temp directory alive until test ends (prevent auto-deletion).
        std::mem::forget(temp);
        AppContext::new(config, session, security, storage)
    }

    #[tokio::test]
    async fn test_context_navigation() {
        let mut ctx = test_ctx().await;
        ctx.navigate_to(ModuleId::TODO);
        assert_eq!(ctx.take_navigation(), Some(ModuleId::TODO));
        assert_eq!(
            ctx.take_navigation(),
            None,
            "should be cleared on second call"
        );
    }

    #[tokio::test]
    async fn test_context_toasts() {
        let mut ctx = test_ctx().await;
        ctx.show_info("Test");
        let toasts = ctx.drain_toasts();
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].1, ToastType::Info);
    }

    #[tokio::test]
    async fn test_context_quit() {
        let mut ctx = test_ctx().await;
        assert!(!ctx.should_quit());
        ctx.quit();
        assert!(ctx.should_quit());
    }
}
