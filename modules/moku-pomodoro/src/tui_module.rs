//! The Pomodoro TUI: a Status screen (live countdown + quick controls), a
//! Profile List (see several saved plans at once, pick one to start or
//! edit), a Quick Create flow (name + a fast simple-shape starting
//! point), and a full Tree Editor (arbitrary `Block` nesting — see
//! `tui_module/tree_editor.rs`). Split the way `modules/moku-rss/src/
//! tui_module/{view_split,...}.rs` already splits independent flows into
//! their own files.
//!
//! `tick_interval`/`on_tick` only ask for a redraw tick while a phase is
//! actively counting down, at a cadence driven by the selected
//! `DetailLevel` — the module costs nothing extra whenever it's idle or
//! not focused (per the project's "0 CPU while idle" requirement).

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    widgets::{Block, Borders, Paragraph},
};

use moku_core::{
    AppContext, Command, ConfirmDeleteKey, ModuleId, ModuleMeta, ModuleStatus, MokuTheme,
    TuiModule, keys_match, resolve_confirm_delete_key, resolve_event,
};
use moku_pomodoro_daemon::{Block as PlanBlock, Plan, StatusSnapshot};

use crate::daemon_client;
use crate::engine::{DetailLevel, PomodoroConfig, Profile, resolve_start_plan};
use crate::model::{PomodoroKeyConfig, flatten_plan};

mod profile_list;
mod quick_create;
mod tree_editor;

pub(crate) enum PomodoroView {
    Status,
    ProfileList(profile_list::ProfileListState),
    QuickCreateName(quick_create::NameState),
    QuickCreateForm(quick_create::FormState),
    TreeEditor(tree_editor::TreeEditorState),
}

/// (action name, default key) — checked after any user-configured
/// override for the same action name. `resume` deliberately has no
/// default (see `resolve_pomodoro_action`'s doc comment); every other
/// action uses a plain letter that doesn't collide with
/// `moku_core::keys::check_hardcoded`'s reserved set (a/d/space/r//) or
/// `KeyBindings::default()` (q/esc/enter/k/j/ctrl-l).
const DEFAULT_KEYS: &[(&str, &str)] = &[
    ("start", "s"),
    ("pause", "p"),
    ("reset", "x"),
    ("skip", "n"),
    ("toggle_detail", "t"),
    ("profiles", "e"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PomodoroAction {
    Start,
    /// Bound to `pause`'s key (default `p`) — toggles based on the
    /// module's own cached `paused` state, so a single key does both
    /// jobs out of the box with no ambiguity.
    PauseResume,
    /// Only reachable via an explicit user-configured `resume` override.
    Resume,
    Reset,
    Skip,
    ToggleDetail,
    Profiles,
}

fn resolve_pomodoro_action(event: &Event, module_keys: &std::collections::HashMap<String, String>) -> Option<PomodoroAction> {
    let Event::Key(key) = event else { return None };
    if key.kind != KeyEventKind::Press {
        return None;
    }

    if let Some(resume_key) = module_keys.get("resume")
        && keys_match(*key, resume_key)
    {
        return Some(PomodoroAction::Resume);
    }

    for (action, default) in DEFAULT_KEYS {
        let configured = module_keys.get(*action).map(String::as_str).unwrap_or(default);
        if keys_match(*key, configured) {
            return Some(match *action {
                "start" => PomodoroAction::Start,
                "pause" => PomodoroAction::PauseResume,
                "reset" => PomodoroAction::Reset,
                "skip" => PomodoroAction::Skip,
                "toggle_detail" => PomodoroAction::ToggleDetail,
                "profiles" => PomodoroAction::Profiles,
                _ => unreachable!("DEFAULT_KEYS and this match must stay in sync"),
            });
        }
    }
    None
}

pub struct PomodoroModule {
    view: PomodoroView,
    /// `None` = daemon unreachable or not yet queried this session.
    last_status: Option<StatusSnapshot>,
    confirm_reset: bool,
    /// Cached from config — `tick_interval` has no `AppContext` access, so
    /// this is refreshed in `init` and whenever it changes.
    detail: DetailLevel,
}

impl PomodoroModule {
    pub fn new() -> Self {
        Self {
            view: PomodoroView::Status,
            last_status: None,
            confirm_reset: false,
            detail: DetailLevel::default(),
        }
    }
}

/// Writes `config` back into `[modules.pomodoro]` and saves — shared by
/// every sub-view (`super::save_config`).
async fn save_config(ctx: &mut AppContext, config: &PomodoroConfig) {
    let value = match toml::Value::try_from(config) {
        Ok(v) => v,
        Err(e) => {
            ctx.show_error(format!("Pomodoro: failed to encode settings: {e}"));
            return;
        }
    };
    ctx.update_config(|cfg| {
        cfg.modules.insert(ModuleId::POMODORO.as_str().to_string(), value);
    });
    let snapshot = (**ctx.config.load()).clone();
    if let Err(e) = moku_core::ConfigManager::save(&snapshot).await {
        ctx.show_error(format!("Pomodoro: failed to save settings: {e}"));
    }
}

/// A short, always-accurate one-line description of a plan for the
/// Profile List — total phase count, rather than trying to pattern-match
/// "is this one of the simple-form shapes" (which would miss anything
/// hand-edited or built in the Tree Editor).
fn plan_summary(plan: &Plan) -> String {
    fn count_phases(blocks: &[PlanBlock]) -> usize {
        blocks
            .iter()
            .map(|b| match b {
                PlanBlock::Phase { .. } => 1,
                PlanBlock::Repeat { blocks, .. } => count_phases(blocks),
            })
            .sum()
    }
    let n = count_phases(plan);
    format!("{n} phase{}", if n == 1 { "" } else { "s" })
}

async fn refresh_status(ctx: &mut AppContext) -> Option<StatusSnapshot> {
    match daemon_client::query_status().await {
        Ok(status) => status,
        Err(e) => {
            ctx.show_error(format!("Pomodoro: {e}"));
            None
        }
    }
}

async fn handle_action(
    action: PomodoroAction,
    last_status: &mut Option<StatusSnapshot>,
    confirm_reset: &mut bool,
    detail: &mut DetailLevel,
    view: &mut PomodoroView,
    ctx: &mut AppContext,
) -> bool {
    match action {
        PomodoroAction::Start => {
            let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
            match resolve_start_plan(&config, None) {
                Ok((name, plan)) => match daemon_client::start(plan, config.notify).await {
                    Ok(()) => ctx.show_info(format!("Started '{name}'.")),
                    Err(e) => ctx.show_error(format!("Pomodoro: {e}")),
                },
                Err(e) => ctx.show_error(format!("Pomodoro: {e}")),
            }
            *last_status = refresh_status(ctx).await;
            true
        }
        PomodoroAction::PauseResume => {
            let currently_paused = last_status.as_ref().is_some_and(|s| s.paused);
            let result = if currently_paused {
                daemon_client::resume().await
            } else {
                daemon_client::pause().await
            };
            if let Err(e) = result {
                ctx.show_error(format!("Pomodoro: {e}"));
            }
            *last_status = refresh_status(ctx).await;
            true
        }
        PomodoroAction::Resume => {
            if let Err(e) = daemon_client::resume().await {
                ctx.show_error(format!("Pomodoro: {e}"));
            }
            *last_status = refresh_status(ctx).await;
            true
        }
        PomodoroAction::Reset => {
            *confirm_reset = true;
            true
        }
        PomodoroAction::Skip => {
            if let Err(e) = daemon_client::skip().await {
                ctx.show_error(format!("Pomodoro: {e}"));
            }
            *last_status = refresh_status(ctx).await;
            true
        }
        PomodoroAction::ToggleDetail => {
            let mut config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
            config.detail = config.detail.next();
            *detail = config.detail;
            save_config(ctx, &config).await;
            true
        }
        PomodoroAction::Profiles => {
            let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
            *view = PomodoroView::ProfileList(profile_list::ProfileListState::new(&config));
            true
        }
    }
}

async fn handle_status_event(
    last_status: &mut Option<StatusSnapshot>,
    confirm_reset: &mut bool,
    detail: &mut DetailLevel,
    view: &mut PomodoroView,
    event: &Event,
    ctx: &mut AppContext,
) -> Result<bool> {
    if *confirm_reset {
        return Ok(match resolve_confirm_delete_key(event) {
            ConfirmDeleteKey::Confirm => {
                *confirm_reset = false;
                if let Err(e) = daemon_client::reset().await {
                    ctx.show_error(format!("Pomodoro: {e}"));
                }
                *last_status = refresh_status(ctx).await;
                true
            }
            ConfirmDeleteKey::Cancel => {
                *confirm_reset = false;
                true
            }
            ConfirmDeleteKey::Other => false,
        });
    }

    let module_config: PomodoroKeyConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
    if let Some(action) = resolve_pomodoro_action(event, &module_config.keys) {
        return Ok(handle_action(action, last_status, confirm_reset, detail, view, ctx).await);
    }

    let command = resolve_event(event, &ctx.config.load().keys, None);
    Ok(match command {
        Command::Back | Command::Quit => {
            ctx.navigate_to(ModuleId::LAUNCHER);
            true
        }
        _ => false,
    })
}

fn format_mmss(total_secs: u32) -> String {
    format!("{:02}:{:02}", total_secs / 60, total_secs % 60)
}

fn draw_status(frame: &mut Frame, area: Rect, theme: &MokuTheme, status: &Option<StatusSnapshot>, confirm_reset: bool, detail: DetailLevel) {
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(3)]).split(area);

    let body = match status {
        None => "Pomodoro daemon is not running.\n\nPress [s] to start it with your saved plan.".to_string(),
        Some(s) => match &s.current_phase {
            None => "Pomodoro daemon is running (no plan started yet).\n\nPress [s] to start.".to_string(),
            Some(phase) => {
                let remaining = detail.format(phase.remaining_secs);
                let state = if s.paused { " (paused)" } else { "" };
                format!("{}{}\n\n{}", phase.kind.label(), state, remaining)
            }
        },
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Pomodoro ")
        .title_alignment(Alignment::Center)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.base_bg));
    let para = Paragraph::new(body)
        .style(Style::default().fg(theme.base_fg))
        .alignment(Alignment::Center)
        .block(block);
    frame.render_widget(para, chunks[0]);

    let bottom = if confirm_reset {
        Paragraph::new("Reset the current plan to its first phase? [y] Yes  [n] No")
            .style(Style::default().fg(theme.error))
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(theme.error)),
            )
    } else {
        Paragraph::new(" [s] Start  [p] Pause/Resume  [n] Skip  [x] Reset  [t] Detail  [e] Profiles  [Esc] Back ")
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

impl Default for PomodoroModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for PomodoroModule {
    fn id(&self) -> ModuleId {
        ModuleId::POMODORO
    }
    fn title(&self) -> &'static str {
        ModuleId::POMODORO.title()
    }
    /// No encrypted data is ever touched (the plan/profile config lives
    /// in plaintext `config.toml`, and the daemon has no vault access at
    /// all) — see `moku-rss`'s identical override for the same reason.
    /// Without this, opening the tab for the first time could route
    /// through the vault-unlock screen for nothing.
    fn encrypt_by_default(&self) -> bool {
        false
    }
}

#[async_trait]
impl TuiModule for PomodoroModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
        self.detail = config.detail;
        self.last_status = refresh_status(ctx).await;
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let PomodoroModule {
            view,
            last_status,
            confirm_reset,
            detail,
        } = self;

        match view {
            PomodoroView::Status => handle_status_event(last_status, confirm_reset, detail, view, event, ctx).await,
            PomodoroView::ProfileList(state) => {
                let currently_running = last_status.as_ref().is_some_and(|s| s.current_phase.is_some());
                let outcome = profile_list::handle_event(state, event, ctx, currently_running).await?;
                Ok(match outcome {
                    profile_list::Outcome::None => false,
                    profile_list::Outcome::Redraw => true,
                    profile_list::Outcome::Back => {
                        *view = PomodoroView::Status;
                        true
                    }
                    profile_list::Outcome::OpenQuickCreate => {
                        *view = PomodoroView::QuickCreateName(quick_create::NameState::new());
                        true
                    }
                    profile_list::Outcome::OpenTreeEditor(name) => {
                        let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
                        if let Some(p) = config.profiles.iter().find(|p| p.name == name) {
                            *view = PomodoroView::TreeEditor(tree_editor::TreeEditorState::new(p.name.clone(), flatten_plan(&p.plan)));
                        }
                        true
                    }
                    profile_list::Outcome::Started => {
                        *view = PomodoroView::Status;
                        *last_status = refresh_status(ctx).await;
                        true
                    }
                })
            }
            PomodoroView::QuickCreateName(state) => {
                let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
                let existing: Vec<String> = config.profiles.iter().map(|p| p.name.clone()).collect();
                let outcome = quick_create::handle_name_event(state, event, &existing);
                Ok(match outcome {
                    quick_create::NameOutcome::None => false,
                    quick_create::NameOutcome::Redraw => true,
                    quick_create::NameOutcome::Cancel => {
                        *view = PomodoroView::ProfileList(profile_list::ProfileListState::new(&config));
                        true
                    }
                    quick_create::NameOutcome::Proceed(name) => {
                        *view = PomodoroView::QuickCreateForm(quick_create::FormState::new(name));
                        true
                    }
                })
            }
            PomodoroView::QuickCreateForm(state) => {
                let outcome = quick_create::handle_form_event(state, event, ctx);
                match outcome {
                    quick_create::FormOutcome::None => Ok(false),
                    quick_create::FormOutcome::Redraw => Ok(true),
                    quick_create::FormOutcome::Cancel => {
                        let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
                        *view = PomodoroView::ProfileList(profile_list::ProfileListState::new(&config));
                        Ok(true)
                    }
                    quick_create::FormOutcome::Created { name, plan } => {
                        let mut config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
                        config.profiles.push(Profile {
                            name: name.clone(),
                            plan: plan.clone(),
                        });
                        save_config(ctx, &config).await;
                        ctx.show_info(format!("Profile '{name}' created."));
                        *view = PomodoroView::TreeEditor(tree_editor::TreeEditorState::new(name, flatten_plan(&plan)));
                        Ok(true)
                    }
                }
            }
            PomodoroView::TreeEditor(state) => {
                let outcome = tree_editor::handle_event(state, event, ctx).await?;
                Ok(match outcome {
                    tree_editor::Outcome::None => false,
                    tree_editor::Outcome::Redraw => true,
                    tree_editor::Outcome::Back => {
                        let config: PomodoroConfig = ctx.config.load().resolve_module_config(ModuleId::POMODORO.as_str());
                        *view = PomodoroView::ProfileList(profile_list::ProfileListState::new(&config));
                        true
                    }
                })
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        match &mut self.view {
            PomodoroView::Status => draw_status(frame, area, theme, &self.last_status, self.confirm_reset, self.detail),
            PomodoroView::ProfileList(state) => profile_list::draw(frame, area, theme, state),
            PomodoroView::QuickCreateName(state) => quick_create::draw_name(frame, area, theme, state),
            PomodoroView::QuickCreateForm(state) => quick_create::draw_form(frame, area, theme, state),
            PomodoroView::TreeEditor(state) => tree_editor::draw(frame, area, theme, state),
        }
    }

    async fn dashboard_summary(&self, _ctx: &AppContext) -> Option<ModuleStatus> {
        let status = daemon_client::query_status().await.ok().flatten();
        let text = match status.and_then(|s| s.current_phase.map(|p| (s.paused, p))) {
            Some((paused, phase)) => {
                let remaining = format_mmss(phase.remaining_secs.max(0.0) as u32);
                if paused {
                    format!("{} (paused) {remaining}", phase.kind.label())
                } else {
                    format!("{} {remaining} remaining", phase.kind.label())
                }
            }
            None => "Not running".to_string(),
        };
        Some(ModuleStatus::normal(text))
    }

    fn tick_interval(&self) -> Option<Duration> {
        // Only the Status screen shows a live countdown; other views (the
        // Profile List, the editors) don't need a redraw tick at all.
        if !matches!(self.view, PomodoroView::Status) {
            return None;
        }
        let status = self.last_status.as_ref()?;
        if status.paused || status.current_phase.is_none() {
            return None;
        }
        Some(self.detail.tick_interval())
    }

    async fn on_tick(&mut self, ctx: &mut AppContext) -> Result<bool> {
        let before = self.last_status.clone();
        self.last_status = refresh_status(ctx).await;
        Ok(self.last_status != before)
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn key(code: crossterm::event::KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[test]
    fn test_default_keys_resolve_with_no_overrides() {
        let overrides = std::collections::HashMap::new();
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('s')), &overrides),
            Some(PomodoroAction::Start)
        );
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('p')), &overrides),
            Some(PomodoroAction::PauseResume)
        );
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('x')), &overrides),
            Some(PomodoroAction::Reset)
        );
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('n')), &overrides),
            Some(PomodoroAction::Skip)
        );
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('t')), &overrides),
            Some(PomodoroAction::ToggleDetail)
        );
        assert_eq!(
            resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char('e')), &overrides),
            Some(PomodoroAction::Profiles)
        );
    }

    #[test]
    fn test_resume_has_no_default_binding() {
        let overrides = std::collections::HashMap::new();
        for c in ['r', 'o', 'u'] {
            assert_ne!(
                resolve_pomodoro_action(&key(crossterm::event::KeyCode::Char(c)), &overrides),
                Some(PomodoroAction::Resume)
            );
        }
    }

    #[test]
    fn test_plan_summary_counts_all_phases_including_nested() {
        use moku_pomodoro_daemon::{PhaseKind, RepeatCount};
        let plan: Plan = vec![PlanBlock::Repeat {
            count: RepeatCount::Infinite,
            blocks: vec![
                PlanBlock::Repeat {
                    count: RepeatCount::Finite(3),
                    blocks: vec![
                        PlanBlock::Phase {
                            kind: PhaseKind::Work,
                            minutes: 45,
                            label: None,
                        },
                        PlanBlock::Phase {
                            kind: PhaseKind::Break,
                            minutes: 15,
                            label: None,
                        },
                    ],
                },
                PlanBlock::Phase {
                    kind: PhaseKind::LongBreak,
                    minutes: 30,
                    label: None,
                },
            ],
        }];
        assert_eq!(plan_summary(&plan), "3 phases");
    }

    fn rendered(module: &mut PomodoroModule) -> String {
        let (width, height) = (70u16, 24u16);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = MokuTheme::default();
        terminal
            .draw(|frame| module.draw(frame, Rect::new(0, 0, width, height), &theme))
            .unwrap();
        terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn test_draw_status_shows_start_prompt_when_daemon_unreachable() {
        let mut module = PomodoroModule::new();
        let content = rendered(&mut module);
        assert!(content.contains("not running"));
    }

    #[test]
    fn test_draw_status_shows_countdown_when_a_phase_is_active() {
        let mut module = PomodoroModule::new();
        module.detail = DetailLevel::Seconds;
        module.last_status = Some(StatusSnapshot {
            paused: false,
            current_phase: Some(moku_pomodoro_daemon::PhaseSnapshot {
                kind: moku_pomodoro_daemon::PhaseKind::Work,
                label: None,
                total_secs: 2700,
                remaining_secs: 754.0,
            }),
            plan_complete: false,
        });
        let content = rendered(&mut module);
        assert!(content.contains("Work"));
        assert!(content.contains("12:34"));
    }

    #[test]
    fn test_encrypt_by_default_is_false() {
        assert!(!PomodoroModule::new().encrypt_by_default());
    }

    #[test]
    fn test_tick_interval_none_outside_status_view() {
        let mut module = PomodoroModule::new();
        module.last_status = Some(StatusSnapshot {
            paused: false,
            current_phase: Some(moku_pomodoro_daemon::PhaseSnapshot {
                kind: moku_pomodoro_daemon::PhaseKind::Work,
                label: None,
                total_secs: 60,
                remaining_secs: 30.0,
            }),
            plan_complete: false,
        });
        assert!(module.tick_interval().is_some(), "active phase on Status view should tick");
        module.view = PomodoroView::ProfileList(profile_list::ProfileListState::new(&PomodoroConfig::default()));
        assert!(module.tick_interval().is_none(), "no tick needed outside the Status view");
    }

    #[test]
    fn test_tick_interval_matches_detail_level_cadence() {
        let mut module = PomodoroModule::new();
        module.last_status = Some(StatusSnapshot {
            paused: false,
            current_phase: Some(moku_pomodoro_daemon::PhaseSnapshot {
                kind: moku_pomodoro_daemon::PhaseKind::Work,
                label: None,
                total_secs: 60,
                remaining_secs: 30.0,
            }),
            plan_complete: false,
        });
        for detail in [DetailLevel::Coarse, DetailLevel::Seconds, DetailLevel::Centiseconds, DetailLevel::Full] {
            module.detail = detail;
            assert_eq!(module.tick_interval(), Some(detail.tick_interval()));
        }
    }
}
