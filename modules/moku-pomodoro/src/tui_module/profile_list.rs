//! The Profile List screen — every saved plan is a named *template*; this
//! screen is how the user sees several at once and picks one. Starting a
//! different profile than the one currently running just replaces it
//! (same daemon, same single active run — never "several timers at
//! once"), confirmed first if something is actively running.
//!
//! `TuiModule::draw` gets no `AppContext`, so `profiles`/`default_profile`
//! are cached on `ProfileListState` itself (populated on entry, kept in
//! sync after every mutation here) rather than re-read from config at
//! draw time.

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use moku_core::{AppContext, Command, ConfirmDeleteKey, MokuTheme, resolve_confirm_delete_key, resolve_event};

use crate::daemon_client;
use crate::engine::{PomodoroConfig, Profile};

pub struct ProfileListState {
    pub profiles: Vec<Profile>,
    pub default_profile: Option<String>,
    pub list_state: ListState,
    pub confirm_delete: Option<usize>,
    pub confirm_switch: Option<usize>,
}

impl ProfileListState {
    pub fn new(config: &PomodoroConfig) -> Self {
        let mut list_state = ListState::default();
        if !config.profiles.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            profiles: config.profiles.clone(),
            default_profile: config.default_profile.clone(),
            list_state,
            confirm_delete: None,
            confirm_switch: None,
        }
    }

    /// Re-syncs the cached display data after a mutation this screen just
    /// made (and persisted) to `config`.
    fn sync_from(&mut self, config: &PomodoroConfig) {
        self.profiles = config.profiles.clone();
        self.default_profile = config.default_profile.clone();
    }
}

pub(super) enum Outcome {
    None,
    Redraw,
    Back,
    OpenQuickCreate,
    OpenTreeEditor(String),
    Started,
}

pub(super) async fn handle_event(
    state: &mut ProfileListState,
    event: &Event,
    ctx: &mut AppContext,
    currently_running: bool,
) -> Result<Outcome> {
    if let Some(idx) = state.confirm_delete {
        return Ok(match resolve_confirm_delete_key(event) {
            ConfirmDeleteKey::Confirm => {
                let config = delete_profile_at(ctx, idx).await;
                state.confirm_delete = None;
                if let Some(config) = config {
                    state.sync_from(&config);
                    clamp_selection(state, state.profiles.len());
                }
                Outcome::Redraw
            }
            ConfirmDeleteKey::Cancel => {
                state.confirm_delete = None;
                Outcome::Redraw
            }
            ConfirmDeleteKey::Other => Outcome::None,
        });
    }
    if let Some(idx) = state.confirm_switch {
        return Ok(match resolve_confirm_delete_key(event) {
            ConfirmDeleteKey::Confirm => {
                state.confirm_switch = None;
                start_profile(state, idx, ctx).await
            }
            ConfirmDeleteKey::Cancel => {
                state.confirm_switch = None;
                Outcome::Redraw
            }
            ConfirmDeleteKey::Other => Outcome::None,
        });
    }

    let Event::Key(key) = event else { return Ok(Outcome::None) };
    if key.kind != KeyEventKind::Press {
        return Ok(Outcome::None);
    }

    let command = resolve_event(event, &ctx.config.load().keys, None);
    match command {
        Command::Up => {
            move_selection(state, -1);
            return Ok(Outcome::Redraw);
        }
        Command::Down => {
            move_selection(state, 1);
            return Ok(Outcome::Redraw);
        }
        Command::Back | Command::Quit => return Ok(Outcome::Back),
        Command::Confirm => {
            let Some(idx) = state.list_state.selected() else {
                return Ok(Outcome::None);
            };
            if idx >= state.profiles.len() {
                return Ok(Outcome::None);
            }
            if currently_running {
                state.confirm_switch = Some(idx);
                return Ok(Outcome::Redraw);
            }
            return Ok(start_profile(state, idx, ctx).await);
        }
        _ => {}
    }

    match key.code {
        KeyCode::Char('E') => {
            let Some(idx) = state.list_state.selected() else {
                return Ok(Outcome::None);
            };
            match state.profiles.get(idx) {
                Some(p) => Ok(Outcome::OpenTreeEditor(p.name.clone())),
                None => Ok(Outcome::None),
            }
        }
        KeyCode::Char('a') => Ok(Outcome::OpenQuickCreate),
        KeyCode::Char('d') => {
            if state.list_state.selected().is_some() {
                state.confirm_delete = state.list_state.selected();
                Ok(Outcome::Redraw)
            } else {
                Ok(Outcome::None)
            }
        }
        KeyCode::Char('D') => {
            if let Some(idx) = state.list_state.selected() {
                let config = delete_profile_at(ctx, idx).await;
                if let Some(config) = config {
                    state.sync_from(&config);
                    clamp_selection(state, state.profiles.len());
                }
                Ok(Outcome::Redraw)
            } else {
                Ok(Outcome::None)
            }
        }
        KeyCode::Char('x') => {
            let Some(idx) = state.list_state.selected() else {
                return Ok(Outcome::None);
            };
            let Some(name) = state.profiles.get(idx).map(|p| p.name.clone()) else {
                return Ok(Outcome::None);
            };
            let mut config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
            config.default_profile = Some(name.clone());
            super::save_config(ctx, &config).await;
            state.sync_from(&config);
            ctx.show_info(format!("'{name}' set as default."));
            Ok(Outcome::Redraw)
        }
        _ => Ok(Outcome::None),
    }
}

fn move_selection(state: &mut ProfileListState, delta: i32) {
    let len = state.profiles.len();
    if len == 0 {
        state.list_state.select(None);
        return;
    }
    let i = match state.list_state.selected() {
        Some(i) => ((i as i32 + delta).rem_euclid(len as i32)) as usize,
        None => 0,
    };
    state.list_state.select(Some(i));
}

fn clamp_selection(state: &mut ProfileListState, len: usize) {
    if len == 0 {
        state.list_state.select(None);
        return;
    }
    match state.list_state.selected() {
        Some(i) if i >= len => state.list_state.select(Some(len - 1)),
        None => state.list_state.select(Some(0)),
        _ => {}
    }
}

/// Removes the profile at `idx` and saves, returning the resulting config
/// (`None` if `idx` was already out of range).
async fn delete_profile_at(ctx: &mut AppContext, idx: usize) -> Option<PomodoroConfig> {
    let mut config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
    if idx >= config.profiles.len() {
        return None;
    }
    let removed = config.profiles.remove(idx);
    if config.default_profile.as_deref() == Some(removed.name.as_str()) {
        config.default_profile = None;
    }
    super::save_config(ctx, &config).await;
    ctx.show_info(format!("Deleted profile '{}'.", removed.name));
    Some(config)
}

async fn start_profile(state: &ProfileListState, idx: usize, ctx: &mut AppContext) -> Outcome {
    let Some(p) = state.profiles.get(idx) else {
        return Outcome::None;
    };
    let name = p.name.clone();
    let config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
    match daemon_client::start(p.plan.clone(), config.notify).await {
        Ok(()) => {
            ctx.show_info(format!("Started '{name}'."));
            Outcome::Started
        }
        Err(e) => {
            ctx.show_error(format!("Pomodoro: {e}"));
            Outcome::Redraw
        }
    }
}

pub(super) fn draw(frame: &mut Frame, area: Rect, theme: &MokuTheme, state: &mut ProfileListState) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

    let items: Vec<ListItem> = state
        .profiles
        .iter()
        .map(|p| {
            let is_default = state.default_profile.as_deref() == Some(p.name.as_str());
            let marker = if is_default { "*" } else { " " };
            let summary = super::plan_summary(&p.plan);
            ListItem::new(format!("{marker} {} — {summary}", p.name))
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Pomodoro Profiles ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.base_bg));

    if items.is_empty() {
        let para = Paragraph::new("No profiles yet. Press [a] to create one.")
            .alignment(Alignment::Center)
            .block(block)
            .style(Style::default().fg(theme.base_fg));
        frame.render_widget(para, chunks[0]);
    } else {
        let list = List::new(items)
            .block(block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.selection_fg)
                    .bg(theme.selection_bg),
            )
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, chunks[0], &mut state.list_state);
    }

    let bottom = if let Some(idx) = state.confirm_delete {
        let name = state.profiles.get(idx).map(|p| p.name.as_str()).unwrap_or("?");
        Paragraph::new(format!("Delete '{name}'? [y] Yes  [n] No"))
            .style(Style::default().fg(theme.error))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.error)),
            )
    } else if let Some(idx) = state.confirm_switch {
        let name = state.profiles.get(idx).map(|p| p.name.as_str()).unwrap_or("?");
        Paragraph::new(format!(
            "A plan is already running — start '{name}' instead? [y] Yes  [n] No"
        ))
        .style(Style::default().fg(theme.warning))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(theme.warning)),
        )
    } else {
        Paragraph::new(" [Enter] Start  [E] Edit  [a] New  [d] Delete  [x] Set default  [Esc] Back ")
            .style(Style::default().fg(theme.base_fg))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.border)),
            )
    };
    frame.render_widget(bottom, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_pomodoro_daemon::{Block, PhaseKind};
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
    use tempfile::tempdir;

    async fn create_test_context_with_profiles(profiles: Vec<Profile>) -> AppContext {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::mem::forget(temp);
        let mut mc = MokuConfig::default();
        mc.modules.insert(
            "pomodoro".to_string(),
            toml::Value::try_from(&PomodoroConfig {
                profiles,
                ..PomodoroConfig::default()
            })
            .unwrap(),
        );
        let config = Arc::new(ArcSwap::from_pointee(mc));
        let session = Arc::new(VaultSession::new());
        let security = Arc::new(SecurityManager::new_with_root(root.clone()));
        let storage = Arc::new(StorageManager::new_with_root(Arc::clone(&session), root).await.unwrap());
        AppContext::new(config, session, security, storage)
    }

    fn simple_profile(name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            plan: vec![Block::Phase {
                kind: PhaseKind::Work,
                minutes: 10,
                label: None,
            }],
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    async fn state_from(ctx: &AppContext) -> ProfileListState {
        let config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
        ProfileListState::new(&config)
    }

    #[tokio::test]
    async fn test_plain_d_opens_confirm_not_immediate_delete() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic")]).await;
        let mut state = state_from(&ctx).await;
        let outcome = handle_event(&mut state, &key(KeyCode::Char('d')), &mut ctx, false)
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Redraw));
        assert_eq!(state.confirm_delete, Some(0));
        assert_eq!(state.profiles.len(), 1, "plain d must not delete yet");
    }

    #[tokio::test]
    async fn test_shift_d_deletes_immediately_and_syncs_cache() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic")]).await;
        let mut state = state_from(&ctx).await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        handle_event(&mut state, &event, &mut ctx, false).await.unwrap();
        let config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
        assert!(config.profiles.is_empty());
        assert!(state.profiles.is_empty(), "the cached display list should reflect the deletion");
    }

    #[tokio::test]
    async fn test_confirm_delete_yes_deletes_and_clears_default_if_it_was_default() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic")]).await;
        ctx.update_config(|cfg| {
            let mut pc: PomodoroConfig = cfg.resolve_module_config("pomodoro");
            pc.default_profile = Some("basic".to_string());
            cfg.modules.insert("pomodoro".to_string(), toml::Value::try_from(&pc).unwrap());
        });
        let mut state = state_from(&ctx).await;
        state.confirm_delete = Some(0);
        handle_event(&mut state, &key(KeyCode::Char('y')), &mut ctx, false)
            .await
            .unwrap();
        let config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
        assert!(config.profiles.is_empty());
        assert!(config.default_profile.is_none(), "deleting the default profile should clear default_profile");
    }

    #[tokio::test]
    async fn test_x_sets_selected_profile_as_default() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic"), simple_profile("kitap")]).await;
        let mut state = state_from(&ctx).await;
        state.list_state.select(Some(1));
        handle_event(&mut state, &key(KeyCode::Char('x')), &mut ctx, false)
            .await
            .unwrap();
        let config: PomodoroConfig = ctx.config.load().resolve_module_config("pomodoro");
        assert_eq!(config.default_profile.as_deref(), Some("kitap"));
        assert_eq!(state.default_profile.as_deref(), Some("kitap"));
    }

    #[tokio::test]
    async fn test_enter_while_idle_starts_without_confirmation() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic")]).await;
        let mut state = state_from(&ctx).await;
        let outcome = handle_event(&mut state, &key(KeyCode::Enter), &mut ctx, false)
            .await
            .unwrap();
        // No daemon reachable in this unit test, so Outcome::Redraw (error toast) is
        // the realistic result rather than Outcome::Started — this asserts no
        // confirm prompt was opened, which is the behavior under test.
        assert!(state.confirm_switch.is_none());
        assert!(matches!(outcome, Outcome::Redraw | Outcome::Started));
    }

    #[tokio::test]
    async fn test_enter_while_running_opens_confirm_switch() {
        let mut ctx = create_test_context_with_profiles(vec![simple_profile("basic")]).await;
        let mut state = state_from(&ctx).await;
        let outcome = handle_event(&mut state, &key(KeyCode::Enter), &mut ctx, true)
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Redraw));
        assert_eq!(state.confirm_switch, Some(0));
    }

    #[test]
    fn test_move_selection_wraps() {
        let config = PomodoroConfig {
            profiles: vec![simple_profile("a"), simple_profile("b"), simple_profile("c")],
            ..PomodoroConfig::default()
        };
        let mut state = ProfileListState::new(&config);
        move_selection(&mut state, -1);
        assert_eq!(state.list_state.selected(), Some(2));
    }
}
