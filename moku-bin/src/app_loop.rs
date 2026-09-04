use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossterm::event::EventStream;
use futures::StreamExt;

use moku_core::{
    AppContext, ModuleId, MokuConfig, Router, SecurityManager, StorageManager, ToastManager,
    TuiRegistry, VaultSession, keys_match,
};

#[derive(Clone, Copy)]
enum AppState {
    Unlocked,
    Locked { after_unlock: ModuleId },
}

pub async fn run(
    config: Arc<ArcSwap<MokuConfig>>,
    session: Arc<VaultSession>,
    security: Arc<SecurityManager>,
    storage: Arc<StorageManager>,
    mut registry: TuiRegistry,
    initial_target: ModuleId,
) -> Result<()> {
    let mut terminal = crate::tui::init()?;
    let mut toasts = ToastManager::new();
    let mut router = Router::new(ModuleId::LAUNCHER);
    let mut ctx = AppContext::new(config, session, security, storage);

    let mut state = enter_module(&mut registry, &mut router, &mut ctx, initial_target)
        .await
        .map_err(|e| eyre!(e))?;

    let mut events = EventStream::new();
    let mut toast_tick = tokio::time::interval(Duration::from_millis(500));
    let mut auto_lock_tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_activity = Instant::now();
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw(|f| {
                let cfg_guard = ctx.config.load();
                let cfg: &MokuConfig = &cfg_guard;
                let theme = cfg.get_active_theme();
                let area = f.area();

                match state {
                    AppState::Locked { .. } => {
                        if let Some(m) = registry.get_mut(ModuleId::LOCK_SCREEN) {
                            m.draw(f, area, &theme);
                        }
                    }
                    AppState::Unlocked => {
                        router.draw(&mut registry, f, area, &theme);
                    }
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
                            state = enter_module(&mut registry, &mut router, &mut ctx, after_unlock)
                                .await
                                .map_err(|e| eyre!(e))?;
                            dirty = true;
                        }
                    }
                    AppState::Unlocked => {
                        // Global lock hotkey, intercepted before normal
                        // dispatch so it works from any screen.
                        let lock_key = ctx.config.load().keys.lock_vault.clone();
                        if let crossterm::event::Event::Key(key) = &ev
                            && ctx.session.is_unlocked()
                            && keys_match(*key, &lock_key)
                        {
                            ctx.session.lock();
                            state = enter_module(&mut registry, &mut router, &mut ctx, ModuleId::LAUNCHER)
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
            _ = toast_tick.tick() => {
                let before = toasts.len();
                toasts.update();
                if toasts.len() != before {
                    dirty = true;
                }
            }
            _ = auto_lock_tick.tick() => {
                let timeout = ctx.config.load().storage.auto_lock_timeout;
                if timeout > 0
                    && ctx.session.is_unlocked()
                    && last_activity.elapsed() >= Duration::from_secs(timeout)
                {
                    ctx.session.lock();
                    state = enter_module(&mut registry, &mut router, &mut ctx, ModuleId::LAUNCHER)
                        .await
                        .map_err(|e| eyre!(e))?;
                    dirty = true;
                }
            }
        }

        for (msg, kind) in ctx.drain_toasts() {
            toasts.add(msg, kind);
            dirty = true;
        }

        if ctx.should_quit() {
            break;
        }

        if let Some(target) = ctx.take_navigation() {
            state = enter_module(&mut registry, &mut router, &mut ctx, target)
                .await
                .map_err(|e| eyre!(e))?;
            dirty = true;
        }
    }

    crate::tui::restore()?;
    Ok(())
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

    // The module's own opinion on whether it owns encrypted storage at all
    // (Launcher/Dashboard/Settings/etc. always say no — they don't call
    // StorageManager::save, so gating their entry on vault unlock would be
    // wrong regardless of config). Falls back to `true` if the module
    // isn't registered (shouldn't normally happen), matching ModuleMeta's
    // own default.
    let module_default_encrypt = registry
        .get_mut(target)
        .map(|m| m.encrypt_by_default())
        .unwrap_or(true);
    let needs_vault =
        moku_core::resolve_encryption(&ctx.config.load(), target.as_str(), module_default_encrypt);

    if needs_vault && !ctx.session.is_unlocked() {
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
    router.switch_to(target);
    Ok(AppState::Unlocked)
}
