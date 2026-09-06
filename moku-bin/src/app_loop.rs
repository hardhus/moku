use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use color_eyre::Result;
use color_eyre::eyre::eyre;
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Text;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use moku_core::{
    AppContext, ModuleId, MokuConfig, Router, SecurityManager, StorageManager, ToastManager,
    TuiRegistry, VaultSession, keys_match,
};

/// Below this many versions behind, a key-scheme upgrade is applied
/// silently in the background right after unlock (small, cheap, no need
/// to bother the user) — at or above it, `SchemaUpgradePrompt` asks
/// first. Matches the user's own example threshold.
const SCHEMA_UPGRADE_PROMPT_THRESHOLD: u16 = 2;

#[derive(Clone, Copy)]
enum AppState {
    Unlocked,
    Locked { after_unlock: ModuleId },
}

/// Shown full-pane (replacing the router's normal draw, same convention
/// as e.g. `moku-todo`'s delete-confirmation view) once the vault is
/// unlocked and the data directory turns out to be `versions_behind`
/// versions behind `CURRENT_KEY_SCHEME`. Enter/`y` runs the upgrade now;
/// Esc/`n` dismisses it for this run only — nothing is persisted, so a
/// declined upgrade is offered again on the next launch.
struct SchemaUpgradePrompt {
    versions_behind: u16,
}

/// `Some(true)` = confirm, `Some(false)` = cancel, `None` = keep waiting
/// (any other key while the prompt is open).
fn schema_prompt_response(ev: &Event) -> Option<bool> {
    let Event::Key(key) = ev else { return None };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => Some(true),
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => Some(false),
        _ => None,
    }
}

fn draw_schema_upgrade_prompt(
    f: &mut ratatui::Frame,
    area: Rect,
    theme: &moku_core::MokuTheme,
    versions_behind: u16,
) {
    let text = Text::from(format!(
        "Storage encryption scheme is {versions_behind} version(s) behind the latest.\n\n\
         Upgrade the data directory to the current scheme now?\n\n\
         [Enter/y] Upgrade now   [Esc/n] Not now (asked again next launch)"
    ));
    let block = Block::default()
        .title(" Storage scheme upgrade available ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warning));
    let para = Paragraph::new(text)
        .block(block)
        .style(Style::default().fg(theme.base_fg).bg(theme.base_bg))
        .wrap(Wrap { trim: true });
    f.render_widget(para, area);
}

/// Runs `StorageManager::migrate_all_key_schemes` for every module in
/// `module_ids` and turns the result into one human-readable summary
/// line for a toast — shared by both the silent-auto-upgrade path and
/// the confirm-prompt path below.
async fn run_key_scheme_migration(ctx: &AppContext, module_ids: &[&str]) -> anyhow::Result<String> {
    let reports = ctx.storage.migrate_all_key_schemes(module_ids).await?;
    let total_migrated: usize = reports.iter().map(|(_, r)| r.migrated).sum();
    let total_errors: usize = reports.iter().map(|(_, r)| r.errors.len()).sum();
    let detail = reports
        .iter()
        .map(|(module, r)| format!("{module}: {} migrated", r.migrated))
        .collect::<Vec<_>>()
        .join(", ");

    if total_errors > 0 {
        anyhow::bail!("{total_errors} record(s) failed to migrate ({detail})");
    }
    Ok(format!(
        "Storage encryption scheme updated to the latest version ({total_migrated} record(s)) — {detail}"
    ))
}

pub async fn run(
    config: Arc<ArcSwap<MokuConfig>>,
    session: Arc<VaultSession>,
    security: Arc<SecurityManager>,
    storage: Arc<StorageManager>,
    mut registry: TuiRegistry,
    initial_target: ModuleId,
    schema_versions_behind: u16,
) -> Result<()> {
    let mut terminal = crate::tui::init()?;
    let mut toasts = ToastManager::new();
    let mut router = Router::new(ModuleId::LAUNCHER);
    let mut ctx = AppContext::new(config, session, security, storage);

    let module_ids: Vec<&str> = crate::config_cmd::ENCRYPTABLE_MODULES
        .iter()
        .map(|(id, _)| *id)
        .collect();

    // Set once at startup from the data directory's own marker (no
    // unlock needed to read it — see `main.rs`); consumed the first time
    // the vault actually gets unlocked, below. `None` once handled
    // (either auto-applied or handed off to `schema_prompt`) so it's
    // never re-evaluated on a later unlock within the same run.
    let mut pending_key_scheme_gap: Option<u16> =
        (schema_versions_behind > 0).then_some(schema_versions_behind);
    let mut schema_prompt: Option<SchemaUpgradePrompt> = None;

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
                        if let Some(prompt) = &schema_prompt {
                            draw_schema_upgrade_prompt(f, area, &theme, prompt.versions_behind);
                        } else {
                            router.draw(&mut registry, f, area, &theme);
                        }
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

                            // First real unlock this run — decide what to
                            // do with the version gap detected at startup
                            // (see main.rs). A small gap is applied right
                            // away and just announced; a larger one asks
                            // first via `schema_prompt`.
                            if let Some(gap) = pending_key_scheme_gap.take() {
                                if gap >= SCHEMA_UPGRADE_PROMPT_THRESHOLD {
                                    schema_prompt = Some(SchemaUpgradePrompt { versions_behind: gap });
                                } else {
                                    match run_key_scheme_migration(&ctx, &module_ids).await {
                                        Ok(summary) => ctx.show_info(summary),
                                        Err(e) => ctx.show_error(format!(
                                            "Storage scheme upgrade failed: {e}"
                                        )),
                                    }
                                }
                            }
                        }
                    }
                    AppState::Unlocked => {
                        if let Some(prompt) = schema_prompt.take() {
                            match schema_prompt_response(&ev) {
                                Some(true) => {
                                    match run_key_scheme_migration(&ctx, &module_ids).await {
                                        Ok(summary) => ctx.show_info(summary),
                                        Err(e) => ctx.show_error(format!(
                                            "Storage scheme upgrade failed: {e}"
                                        )),
                                    }
                                    dirty = true;
                                }
                                Some(false) => {
                                    dirty = true;
                                }
                                None => {
                                    // Not a confirm/cancel key — keep the
                                    // prompt open and ignore the keypress.
                                    schema_prompt = Some(prompt);
                                }
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

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{StorageManager, TuiModule};
    use tempfile::tempdir;

    use super::*;

    async fn create_test_context() -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
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

        let config = Arc::new(ArcSwap::from_pointee(MokuConfig::default()));
        let session = Arc::new(VaultSession::new());
        let key = SecurityManager::derive_key("test_pass", &[1u8; 16])
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

    fn make_key(code: crossterm::event::KeyCode, modifiers: crossterm::event::KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        })
    }

    #[test]
    fn test_schema_prompt_response_confirm_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Enter, KeyModifiers::empty())),
            Some(true)
        );
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Char('y'), KeyModifiers::empty())),
            Some(true)
        );
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Char('Y'), KeyModifiers::empty())),
            Some(true)
        );
    }

    #[test]
    fn test_schema_prompt_response_cancel_keys() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Esc, KeyModifiers::empty())),
            Some(false)
        );
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Char('n'), KeyModifiers::empty())),
            Some(false)
        );
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Char('N'), KeyModifiers::empty())),
            Some(false)
        );
    }

    #[test]
    fn test_schema_prompt_response_other_keys_ignored() {
        use crossterm::event::{KeyCode, KeyModifiers};
        assert_eq!(
            schema_prompt_response(&make_key(KeyCode::Char('x'), KeyModifiers::empty())),
            None
        );
    }

    #[tokio::test]
    async fn test_run_key_scheme_migration_errors_when_vault_locked() {
        let ctx = create_test_context().await;
        let result = run_key_scheme_migration(&ctx, &["mod1"]).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_draw_schema_upgrade_prompt_renders_versions_behind() {
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let theme = moku_core::MokuTheme::default();
        terminal
            .draw(|f| {
                let area = f.area();
                draw_schema_upgrade_prompt(f, area, &theme, 3);
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(content.contains("3 version"));
        assert!(content.contains("Upgrade"));
    }

    #[tokio::test]
    async fn test_run_key_scheme_migration_summarizes_zero_records() {
        let ctx = create_unlocked_test_context().await;
        let summary = run_key_scheme_migration(&ctx, &["mod1", "mod2"])
            .await
            .unwrap();
        assert!(summary.contains("mod1: 0 migrated"));
        assert!(summary.contains("mod2: 0 migrated"));
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
