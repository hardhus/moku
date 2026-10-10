use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossterm::event::EventStream;
use futures::StreamExt;

use moku_config::{ConfigHandle, MokuConfig};
use moku_core::{AppContext, ModuleId, MokuTheme, Router, ToastManager, TuiRegistry, keys_match};
use moku_store::StorageManager;
use moku_vault::{SecurityManager, VaultSession};

use crate::data_upgrade::{self, DataDirStatus, UpgradeEvent, UpgradeScreen};

#[derive(Clone, Copy)]
enum AppState {
    Unlocked,
    Locked { after_unlock: ModuleId },
}

/// Drives the currently-focused module's own opt-in redraw tick
/// (`TuiModule::tick_interval`/`on_tick`) — pending forever (no wakeup at
/// all) whenever that module doesn't currently want one, so an idle
/// module costs nothing. Mirrors `worker.rs`'s `deadline_sleep` in the
/// pomodoro daemon crate: same "own a plain `Option`, never hold a
/// borrow across the await" shape.
async fn focus_tick_wait(interval: Option<&mut tokio::time::Interval>) {
    match interval {
        Some(iv) => {
            iv.tick().await;
        }
        None => std::future::pending::<()>().await,
    }
}

pub async fn run(
    config: Arc<ConfigHandle>,
    session: Arc<VaultSession>,
    security: Arc<SecurityManager>,
    storage: Arc<StorageManager>,
    mut registry: TuiRegistry,
    initial_target: ModuleId,
    data: DataDirStatus,
) -> Result<()> {
    let mut terminal = crate::tui::init()?;
    let mut toasts = ToastManager::new();
    let mut router = Router::new(ModuleId::LAUNCHER);
    let mut ctx = AppContext::new(config, session, security, storage);

    // Set at startup from the data directory's FORMAT (no unlock needed,
    // see main.rs) and cleared once an upgrade succeeds. While it is set,
    // the upgrade prompt is offered on the first unlock and every
    // vault-gated module opens it instead of itself (audit KN-4.1).
    let DataDirStatus {
        dir: data_dir,
        pending_upgrade,
    } = data;
    let mut pending_upgrade = pending_upgrade;
    let mut upgrade_screen: Option<UpgradeScreen> = None;

    let mut state = navigate(
        &mut registry,
        &mut router,
        &mut ctx,
        initial_target,
        pending_upgrade.is_some(),
        &mut upgrade_screen,
    )
    .await
    .map_err(|e| eyre!(e))?;

    let mut events = EventStream::new();
    let mut toast_tick = tokio::time::interval(Duration::from_millis(500));
    let mut auto_lock_tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_activity = Instant::now();
    let mut dirty = true;

    // The focused module's own opt-in tick (see `focus_tick_wait` above) —
    // rebuilt whenever the focused module or its requested interval
    // changes, so a module that stops wanting ticks (e.g. a pomodoro
    // phase finishing) genuinely drops back to zero extra wakeups instead
    // of an idle no-op firing forever.
    let mut focus_tick_module: Option<ModuleId> = None;
    let mut focus_tick_interval: Option<Duration> = None;
    let mut focus_tick: Option<tokio::time::Interval> = None;

    loop {
        if let AppState::Unlocked = state {
            let desired = registry
                .get_mut(router.focused())
                .and_then(|m| m.tick_interval());
            if desired != focus_tick_interval || focus_tick_module != Some(router.focused()) {
                focus_tick = desired.map(tokio::time::interval);
                focus_tick_interval = desired;
                focus_tick_module = Some(router.focused());
            }
        } else if focus_tick.is_some() {
            focus_tick = None;
            focus_tick_interval = None;
            focus_tick_module = None;
        }

        if dirty {
            terminal.draw(|f| {
                let cfg_guard = ctx.config.load();
                let cfg: &MokuConfig = &cfg_guard;
                let theme = MokuTheme::from_config(cfg);
                let area = f.area();

                match state {
                    AppState::Locked { .. } => {
                        if let Some(m) = registry.get_mut(ModuleId::LOCK_SCREEN) {
                            m.draw(f, area, &theme);
                        }
                    }
                    AppState::Unlocked => match (&upgrade_screen, pending_upgrade) {
                        (Some(UpgradeScreen::Prompt { daemon_running, .. }), Some(from)) => {
                            data_upgrade::draw_prompt(f, area, &theme, from, &data_dir, *daemon_running);
                        }
                        (Some(UpgradeScreen::Running { progress, .. }), _) => {
                            data_upgrade::draw_running(f, area, &theme, &progress.borrow());
                        }
                        _ => router.draw(&mut registry, f, area, &theme),
                    },
                }

                toasts.draw(f, area, cfg);
            })?;
            dirty = false;
        }

        tokio::select! {
            maybe_event = events.next() => {
                let ev = match maybe_event {
                    Some(Ok(ev)) => ev,
                    Some(Err(_)) => continue,
                    None => break,
                };
                last_activity = Instant::now();

                // No module's handle_event marks itself dirty for a
                // terminal resize (most only match Event::Key), so without
                // this the screen would just sit at its old size/content
                // until the next unrelated redraw. ratatui's Terminal::draw
                // already re-queries the backend's size on every call, so
                // forcing one is all that's needed here.
                if matches!(ev, crossterm::event::Event::Resize(_, _)) {
                    dirty = true;
                }

                match state {
                    AppState::Locked { after_unlock } => {
                        let was_unlocked = ctx.session.is_unlocked();

                        if let Some(m) = registry.get_mut(ModuleId::LOCK_SCREEN) {
                            dirty |= m.handle_event(&ev, &mut ctx).await.map_err(|e| eyre!(e))?;
                        }

                        if !was_unlocked && ctx.session.is_unlocked() {
                            if pending_upgrade.is_some() {
                                // First unlock with an upgrade pending: ask
                                // before entering anything (the target
                                // likely needs the upgraded data).
                                upgrade_screen = Some(UpgradeScreen::Prompt {
                                    after: after_unlock,
                                    daemon_running: moku_daemon::status::is_running(),
                                });
                                state = AppState::Unlocked;
                            } else {
                                state = navigate(
                                    &mut registry,
                                    &mut router,
                                    &mut ctx,
                                    after_unlock,
                                    pending_upgrade.is_some(),
                                    &mut upgrade_screen,
                                )
                                .await
                                .map_err(|e| eyre!(e))?;
                            }
                            dirty = true;
                        }
                    }
                    AppState::Unlocked => {
                        if let Some(UpgradeScreen::Running { .. }) = &upgrade_screen {
                            // Keys are ignored while the upgrade runs.
                        } else if let Some(&UpgradeScreen::Prompt {
                            after,
                            daemon_running,
                        }) = upgrade_screen.as_ref()
                        {
                            match data_upgrade::prompt_response(&ev) {
                                Some(true) => {
                                    let stopped = if daemon_running {
                                        moku_daemon::status::stop_daemon_and_wait(Duration::from_secs(5))
                                            .map(|_| ())
                                    } else {
                                        Ok(())
                                    };
                                    match stopped {
                                        Ok(()) => {
                                            // The upgrade opens the module DBs
                                            // itself; release this process's
                                            // sled locks first.
                                            ctx.storage.close_all_dbs().await;
                                            let (progress, task) = data_upgrade::start(
                                                data_dir.clone(),
                                                Arc::clone(&ctx.session),
                                            );
                                            upgrade_screen =
                                                Some(UpgradeScreen::Running { after, progress, task });
                                        }
                                        Err(e) => {
                                            ctx.show_error(format!("Couldn't stop the daemon: {e:#}"));
                                        }
                                    }
                                    dirty = true;
                                }
                                Some(false) => {
                                    upgrade_screen = None;
                                    state = skip_upgrade(
                                        &mut registry,
                                        &mut router,
                                        &mut ctx,
                                        after,
                                        &mut upgrade_screen,
                                    )
                                    .await
                                    .map_err(|e| eyre!(e))?;
                                    dirty = true;
                                }
                                None => {}
                            }
                        } else {
                            // Global lock hotkey, intercepted before normal
                            // dispatch so it works from any screen.
                            let lock_key = ctx.config.load().keys.lock_vault.clone();
                            if let crossterm::event::Event::Key(key) = &ev
                                && ctx.session.is_unlocked()
                                && keys_match(*key, &lock_key)
                            {
                                ctx.session.lock();
                                state = navigate(
                                    &mut registry,
                                    &mut router,
                                    &mut ctx,
                                    ModuleId::LAUNCHER,
                                    pending_upgrade.is_some(),
                                    &mut upgrade_screen,
                                )
                                .await
                                .map_err(|e| eyre!(e))?;
                                dirty = true;
                            } else {
                                dirty |= router
                                    .dispatch_event(&mut registry, &ev, &mut ctx)
                                    .await
                                    .map_err(|e| eyre!(e))?;
                            }
                        }
                    }
                }
            }
            _ = toast_tick.tick() => {
                let before = toasts.len();
                toasts.update();
                if toasts.len() != before {
                    dirty = true;
                }
            }
            _ = auto_lock_tick.tick() => {
                let timeout = ctx.config.load().storage.auto_lock_timeout;
                // Never lock in the middle of a running upgrade: its hops
                // borrow the root key.
                let upgrading = matches!(upgrade_screen, Some(UpgradeScreen::Running { .. }));
                if timeout > 0
                    && !upgrading
                    && ctx.session.is_unlocked()
                    && last_activity.elapsed() >= Duration::from_secs(timeout)
                {
                    ctx.session.lock();
                    // An open prompt is moot once locked; it comes back on
                    // the next unlock-gated entry.
                    upgrade_screen = None;
                    state = navigate(
                        &mut registry,
                        &mut router,
                        &mut ctx,
                        ModuleId::LAUNCHER,
                        pending_upgrade.is_some(),
                        &mut upgrade_screen,
                    )
                    .await
                    .map_err(|e| eyre!(e))?;
                    dirty = true;
                }
            }
            event = data_upgrade::next_event(&mut upgrade_screen) => {
                match event {
                    UpgradeEvent::Progress => {}
                    UpgradeEvent::Done(result) => {
                        let after = match upgrade_screen.take() {
                            Some(UpgradeScreen::Running { after, .. }) => after,
                            _ => ModuleId::LAUNCHER,
                        };
                        let target = match result {
                            Ok(Ok(report)) => {
                                pending_upgrade = None;
                                ctx.show_info(report.summary());
                                after
                            }
                            Ok(Err(e)) => {
                                ctx.show_error(format!(
                                    "Data format upgrade failed: {:#}",
                                    anyhow::Error::from(e)
                                ));
                                ModuleId::LAUNCHER
                            }
                            Err(e) => {
                                ctx.show_error(format!("Data format upgrade task failed: {e}"));
                                ModuleId::LAUNCHER
                            }
                        };
                        state = navigate(
                            &mut registry,
                            &mut router,
                            &mut ctx,
                            target,
                            pending_upgrade.is_some(),
                            &mut upgrade_screen,
                        )
                        .await
                        .map_err(|e| eyre!(e))?;
                    }
                }
                dirty = true;
            }
            _ = focus_tick_wait(focus_tick.as_mut()) => {
                if let AppState::Unlocked = state
                    && let Some(m) = registry.get_mut(router.focused())
                {
                    match m.on_tick(&mut ctx).await {
                        Ok(true) => dirty = true,
                        Ok(false) => {}
                        Err(e) => return Err(eyre!(e)),
                    }
                }
            }
        }

        // Config problems found at startup or while a module read its
        // settings (an invalid section, a read-only config.toml): each one
        // is shown once, as a warning, instead of being swallowed.
        for issue in ctx.config.take_issues() {
            ctx.show_warning(issue.to_string());
        }

        for (msg, kind) in ctx.drain_toasts() {
            toasts.add(msg, kind);
            dirty = true;
        }

        if let Some(req) = ctx.take_pending_suspend() {
            crate::tui::restore()?;
            // tokio::process::Command (not std::process::Command) so this
            // .status().await doesn't block the whole async runtime for the
            // entire child process's lifetime — inherits stdin/stdout/stderr
            // by default, same as std::process::Command's own default.
            let status = tokio::process::Command::new(&req.command)
                .args(&req.args)
                .status()
                .await;
            if let Err(e) = status {
                ctx.show_error(format!("Failed to launch '{}': {e:#}", req.command));
            }
            terminal = crate::tui::init()?;
            dirty = true;
        }

        if ctx.should_quit() {
            break;
        }

        if let Some(target) = ctx.take_navigation() {
            state = navigate(
                &mut registry,
                &mut router,
                &mut ctx,
                target,
                pending_upgrade.is_some(),
                &mut upgrade_screen,
            )
            .await
            .map_err(|e| eyre!(e))?;
            dirty = true;
        }
    }

    crate::tui::restore()?;
    Ok(())
}

/// `enter_module`, gated on a pending data-format upgrade (audit KN-4.1):
/// with the vault unlocked, a module that needs it opens the upgrade
/// prompt instead of itself (its data may still be in the old format),
/// and stays on the current screen. Everything else is `enter_module`.
async fn navigate(
    registry: &mut TuiRegistry,
    router: &mut Router,
    ctx: &mut AppContext,
    target: ModuleId,
    upgrade_pending: bool,
    upgrade_screen: &mut Option<UpgradeScreen>,
) -> anyhow::Result<AppState> {
    // The launcher is where a skipped prompt sends the user: never gated.
    if upgrade_pending
        && target != ModuleId::LOCK_SCREEN
        && target != ModuleId::LAUNCHER
        && ctx.session.is_unlocked()
        && needs_vault(registry, ctx, target)
    {
        *upgrade_screen = Some(UpgradeScreen::Prompt {
            after: target,
            daemon_running: moku_daemon::status::is_running(),
        });
        return Ok(AppState::Unlocked);
    }
    enter_module(registry, router, ctx, target).await
}

/// "Not now" on the upgrade prompt: go where the user was heading, unless
/// that module is itself gated on the upgrade, in which case back to the
/// launcher with a warning (instead of reopening the prompt at once).
async fn skip_upgrade(
    registry: &mut TuiRegistry,
    router: &mut Router,
    ctx: &mut AppContext,
    after: ModuleId,
    upgrade_screen: &mut Option<UpgradeScreen>,
) -> anyhow::Result<AppState> {
    let target = if needs_vault(registry, ctx, after) {
        ctx.show_warning(format!("{} needs the data format update first", after.title()));
        ModuleId::LAUNCHER
    } else {
        after
    };
    navigate(registry, router, ctx, target, true, upgrade_screen).await
}

/// The module's own opinion on whether it owns encrypted storage at all
/// (Launcher/Dashboard/Settings/etc. always say no — they don't call
/// StorageManager::save, so gating their entry on vault unlock would be
/// wrong regardless of config). Falls back to `true` if the module isn't
/// registered (shouldn't normally happen), matching ModuleMeta's own
/// default. An invalid `encrypt` override counts as encrypted (fail
/// closed); the module's save path reports it.
fn needs_vault(registry: &mut TuiRegistry, ctx: &AppContext, target: ModuleId) -> bool {
    let module_default_encrypt = registry
        .get_mut(target)
        .map(|m| m.encrypt_by_default())
        .unwrap_or(true);
    moku_config::resolve_encryption(&ctx.config.load(), target.as_str(), module_default_encrypt)
        .unwrap_or(true)
}

async fn enter_module(
    registry: &mut TuiRegistry,
    router: &mut Router,
    ctx: &mut AppContext,
    target: ModuleId,
) -> anyhow::Result<AppState> {
    // Vault Security is directly selectable from the launcher menu, but its
    // own `encrypt_by_default()` is hardcoded false (it must never gate on
    // its own unlock). That means the generic check below would never route
    // it through AppState::Locked, so a successful unlock would have
    // nowhere to "return" to — the module never navigates away on its own
    // (see its doc comment), so it would just keep re-showing the password
    // prompt. Route it through the same Locked/after_unlock machinery the
    // auto-triggered path already uses, targeting the launcher.
    if target == ModuleId::LOCK_SCREEN {
        if ctx.session.is_unlocked() {
            ctx.show_info("Vault is already unlocked.");
            router.switch_to(ModuleId::LAUNCHER);
            return Ok(AppState::Unlocked);
        }
        if let Some(m) = registry.get_mut(ModuleId::LOCK_SCREEN) {
            m.init(ctx).await?;
        }
        return Ok(AppState::Locked {
            after_unlock: ModuleId::LAUNCHER,
        });
    }

    if needs_vault(registry, ctx, target) && !ctx.session.is_unlocked() {
        if let Some(m) = registry.get_mut(ModuleId::LOCK_SCREEN) {
            m.init(ctx).await?;
        }
        return Ok(AppState::Locked {
            after_unlock: target,
        });
    }

    if let Some(m) = registry.get_mut(target) {
        m.init(ctx).await?;
    }

    // Dashboard has no way to reach other modules' live instances itself
    // (see `TuiModule::dashboard_summary`'s doc comment) — this is the one
    // place with access to both the registry and ctx, so it's the one spot
    // that collects and hands over summaries. A future module gaining a
    // dashboard row never needs a change here — only `dashboard_summary`
    // in its own crate.
    if target == ModuleId::DASHBOARD {
        let summaries = registry
            .collect_dashboard_summaries(ModuleId::DASHBOARD, ctx)
            .await;
        if let Some(m) = registry.get_mut(ModuleId::DASHBOARD)
            // `.as_mut()` derefs through the `Box` to `&mut dyn TuiModule`
            // first — calling `.as_any_mut()` directly on `&mut Box<dyn
            // TuiModule>` picks the blanket `AsAny` impl for `Box` itself
            // (it's `'static` too), type-erasing the *Box*, not the
            // concrete module inside it, so the downcast below always
            // failed silently.
            && let Some(dash) = m
                .as_mut()
                .as_any_mut()
                .downcast_mut::<moku_dashboard::DashboardModule>()
        {
            dash.set_summaries(summaries);
        }
    }

    router.switch_to(target);
    Ok(AppState::Unlocked)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moku_core::TuiModule;
    use moku_store::StorageManager;
    use moku_vault::{SecurityManager, VaultSession};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(moku_config::ConfigHandle::detached(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    async fn create_unlocked_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(moku_config::ConfigHandle::detached(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let key = SecurityManager::derive_key(&"test_pass".into(), &[1u8; 16])
            .await
            .unwrap();
        session.unlock(key);
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(
            StorageManager::new_with_root(Arc::clone(&session), root)
                .await
                .unwrap(),
        );

        AppContext::new(config, session, security, storage)
    }

    fn todo_registry() -> TuiRegistry {
        let mut registry = TuiRegistry::new();
        registry.insert(Box::new(moku_todo::TodoModule::new()));
        registry
    }

    /// KN-4.1: with an upgrade pending, an encrypted module opens the
    /// upgrade prompt instead of itself.
    #[tokio::test]
    async fn test_pending_upgrade_gates_encrypted_modules() {
        let mut registry = todo_registry();
        let mut router = Router::new(ModuleId::LAUNCHER);
        let mut ctx = create_unlocked_test_context().await;
        let mut screen = None;

        let state = navigate(&mut registry, &mut router, &mut ctx, ModuleId::TODO, true, &mut screen)
            .await
            .unwrap();
        assert!(matches!(state, AppState::Unlocked));
        assert!(matches!(
            screen,
            Some(UpgradeScreen::Prompt { after: ModuleId::TODO, .. })
        ));
        assert_eq!(router.focused(), ModuleId::LAUNCHER, "todo was not entered");
    }

    #[tokio::test]
    async fn test_no_pending_upgrade_enters_the_module() {
        let mut registry = todo_registry();
        let mut router = Router::new(ModuleId::LAUNCHER);
        let mut ctx = create_unlocked_test_context().await;
        let mut screen = None;

        navigate(&mut registry, &mut router, &mut ctx, ModuleId::TODO, false, &mut screen)
            .await
            .unwrap();
        assert!(screen.is_none());
        assert_eq!(router.focused(), ModuleId::TODO);
    }

    /// Locked: the lock screen comes first; the prompt follows the unlock.
    #[tokio::test]
    async fn test_pending_upgrade_with_locked_vault_goes_to_the_lock_screen() {
        let mut registry = todo_registry();
        let mut router = Router::new(ModuleId::LAUNCHER);
        let mut ctx = create_test_context().await;
        let mut screen = None;

        let state = navigate(&mut registry, &mut router, &mut ctx, ModuleId::TODO, true, &mut screen)
            .await
            .unwrap();
        assert!(matches!(state, AppState::Locked { after_unlock: ModuleId::TODO }));
        assert!(screen.is_none());
    }

    #[tokio::test]
    async fn test_skipping_the_upgrade_for_a_gated_module_goes_to_the_launcher() {
        let mut registry = todo_registry();
        let mut router = Router::new(ModuleId::TODO);
        let mut ctx = create_unlocked_test_context().await;
        let mut screen = None;

        skip_upgrade(&mut registry, &mut router, &mut ctx, ModuleId::TODO, &mut screen)
            .await
            .unwrap();
        assert!(screen.is_none(), "the prompt is not reopened at once");
        assert_eq!(router.focused(), ModuleId::LAUNCHER);
        assert!(!ctx.drain_toasts().is_empty(), "the user is told why");
    }

    // Regression test for a real bug: the downcast used to type-erase
    // `Box<dyn TuiModule>` itself (its blanket `AsAny` impl is picked over
    // deref-ing into the boxed module, since it needs zero autoderefs) and
    // always failed silently, so Dashboard never received any summaries —
    // "No module status available" no matter what. Fixed by explicitly
    // `.as_mut()`-ing through the `Box` before calling `.as_any_mut()`.
    #[tokio::test]
    async fn test_entering_dashboard_populates_summaries_from_other_modules() {
        let mut registry = TuiRegistry::new();
        registry.insert(Box::new(moku_dashboard::DashboardModule::new()));
        registry.insert(Box::new(moku_todo::TodoModule::new()));
        registry.insert(Box::new(moku_rss::RssTuiModule::new()));
        let mut router = Router::new(ModuleId::LAUNCHER);
        let mut ctx = create_test_context().await;

        let state = enter_module(&mut registry, &mut router, &mut ctx, ModuleId::DASHBOARD)
            .await
            .unwrap();
        assert!(matches!(state, AppState::Unlocked));

        let dash = registry
            .get_mut(ModuleId::DASHBOARD)
            .unwrap()
            .as_mut()
            .as_any_mut()
            .downcast_mut::<moku_dashboard::DashboardModule>()
            .expect("downcast should succeed");

        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = moku_core::MokuTheme::default();
        terminal
            .draw(|f| dash.draw(f, ratatui::layout::Rect::new(0, 0, 60, 20), &theme))
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            content.contains("feeds"),
            "expected RSS's never-lock-gated summary row to be present: {content}"
        );
        assert!(
            !content.contains("No module status available"),
            "Dashboard must not fall back to its empty state once summaries were collected"
        );
    }
}
