use std::sync::Arc;

use moku_config::{ConfigHandle, MokuConfig};
use moku_store::StorageManager;
use moku_vault::{SecurityManager, VaultSession};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::module::ModuleId;
use crate::toast::ToastType;

/// A request to suspend the TUI, run an external command with inherited
/// stdio, wait for it to exit, then restore the TUI — e.g. handing off to
/// the user's editor at a specific line. Drained by
/// `moku-bin/src/app_loop.rs` alongside `pending_navigation`/`pending_toasts`.
#[derive(Debug, Clone)]
pub struct SuspendRequest {
    pub command: String,
    pub args: Vec<String>,
}

/// Shared state passed to every TUI module.
/// Uses lock-free config reads (`ctx.config.load()`). Call `.load()` inline
/// instead of storing it to avoid borrow-checker conflicts. Config changes
/// go through `update_config`/`update_module_config` (saved immediately);
/// a module's own settings are read with `module_config`.
pub struct AppContext {
    pub config: Arc<ConfigHandle>,
    pub session: Arc<VaultSession>,
    pub security: Arc<SecurityManager>,
    pub storage: Arc<StorageManager>,

    should_quit: bool,
    pending_navigation: Option<ModuleId>,
    pending_toasts: Vec<(String, ToastType)>,
    pending_suspend: Option<SuspendRequest>,
}

impl AppContext {
    pub fn new(
        config: Arc<ConfigHandle>,
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
            pending_suspend: None,
        }
    }

    pub fn quit(&mut self) {
        self.should_quit = true;
    }

    /// Changes the config and saves it to config.toml right away (only the
    /// changed keys; comments are kept). Returns whether it was saved; if
    /// not (read-only config, a write-locked section, a disk error), the
    /// change is not applied in memory either and the reason is shown as an
    /// error toast.
    pub fn update_config<F>(&mut self, mutate: F) -> bool
    where
        F: FnOnce(&mut MokuConfig),
    {
        let result = self.config.update(mutate);
        self.report_config_write(result)
    }

    /// Like `update_config`, for `[modules.<module_id>]` read as `T`: only
    /// the fields `mutate` changed are written, unknown keys in the table
    /// are kept, and a table that doesn't currently match `T` is never
    /// overwritten (see `ConfigHandle::update_module`).
    pub fn update_module_config<T>(&mut self, module_id: &str, mutate: impl FnOnce(&mut T)) -> bool
    where
        T: Serialize + DeserializeOwned + Default + Clone,
    {
        let result = self.config.update_module(module_id, mutate);
        self.report_config_write(result)
    }

    /// `[modules.<module_id>]` as `T`; on a type mismatch, `T`'s defaults
    /// (the problem is reported to the user once, as a warning).
    pub fn module_config<T>(&self, module_id: &str) -> T
    where
        T: DeserializeOwned + Default,
    {
        self.config.module_config(module_id)
    }

    fn report_config_write(&mut self, result: Result<(), moku_config::ConfigError>) -> bool {
        match result {
            Ok(()) => true,
            Err(e) => {
                self.show_error(format!(
                    "Setting not saved: {}",
                    moku_sys::util::error_chain(&e)
                ));
                false
            }
        }
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

    /// Requests that `moku-bin`'s app loop suspend the TUI, run `command
    /// args` to completion with inherited stdio, then restore the TUI —
    /// e.g. handing a file off to the user's editor. See
    /// `moku-bin/src/app_loop.rs`'s drain point for what actually runs it.
    pub fn request_suspend(&mut self, command: impl Into<String>, args: Vec<String>) {
        self.pending_suspend = Some(SuspendRequest {
            command: command.into(),
            args,
        });
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

    pub fn take_pending_suspend(&mut self) -> Option<SuspendRequest> {
        self.pending_suspend.take()
    }
}

/// Context for one-off CLI commands. No event loop, no Arc/ArcSwap wrapper required.
pub struct CliContext {
    pub config: MokuConfig,
    pub storage: Option<Arc<StorageManager>>,
    /// Present for CLI modules that may need to prompt for the vault
    /// password (e.g. an `encrypt_by_default` module's first read/write in
    /// a given invocation) — mirrors `moku-bin/src/config_cmd.rs`'s
    /// `ensure_unlocked_if_needed` pattern, generalized so any CLI module
    /// can do the same instead of only the `config` command.
    pub session: Option<Arc<VaultSession>>,
    pub security: Option<Arc<SecurityManager>>,
}

/// Context for periodic tasks in the moku-daemon.
pub struct DaemonContext {
    pub config: Arc<ConfigHandle>,
    pub storage: Arc<StorageManager>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use moku_vault::SecurityManager;
    use tempfile::tempdir;

    async fn test_ctx() -> AppContext {
        let config = Arc::new(ConfigHandle::detached(MokuConfig::default()));
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

    #[tokio::test]
    async fn test_context_pending_suspend() {
        let mut ctx = test_ctx().await;
        assert!(ctx.take_pending_suspend().is_none());
        ctx.request_suspend("hx", vec!["+3".to_string(), "note.md".to_string()]);
        let req = ctx.take_pending_suspend().expect("should be set");
        assert_eq!(req.command, "hx");
        assert_eq!(req.args, vec!["+3".to_string(), "note.md".to_string()]);
        assert!(
            ctx.take_pending_suspend().is_none(),
            "should be cleared on second call"
        );
    }
}
