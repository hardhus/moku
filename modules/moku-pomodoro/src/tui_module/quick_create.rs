//! Creating a brand-new profile: a name prompt, then the round-1 7-field
//! "simple" form (now navigated with `Up`/`Down`/`j`/`k` — never `Tab`).
//! Submitting builds the initial `Plan` and hands it back to
//! `tui_module.rs`, which saves the new profile and opens the Tree Editor
//! on it — this form is a fast starting point, never a dead end.

use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Block, Borders, Paragraph},
};

use moku_core::{AppContext, Command, MokuTheme, resolve_event};
use moku_pomodoro_daemon::Plan;

use crate::engine::{CLASSIC_PROFILE_NAME, build_plan_from_simple_form};
use crate::model::{SettingsField, SettingsFormState, SimplePresetForm};

pub struct NameState {
    pub name_input: String,
    pub error: Option<String>,
}

impl NameState {
    pub fn new() -> Self {
        Self {
            name_input: String::new(),
            error: None,
        }
    }
}

pub(super) enum NameOutcome {
    None,
    Redraw,
    Cancel,
    Proceed(String),
}

pub(super) fn handle_name_event(state: &mut NameState, event: &Event, existing_names: &[String]) -> NameOutcome {
    let Event::Key(key) = event else { return NameOutcome::None };
    if key.kind != KeyEventKind::Press {
        return NameOutcome::None;
    }
    match key.code {
        KeyCode::Esc => NameOutcome::Cancel,
        KeyCode::Enter => {
            let trimmed = state.name_input.trim();
            if trimmed.is_empty() {
                state.error = Some("Name cannot be empty.".to_string());
                NameOutcome::Redraw
            } else if trimmed == CLASSIC_PROFILE_NAME {
                state.error = Some(format!("'{CLASSIC_PROFILE_NAME}' is reserved."));
                NameOutcome::Redraw
            } else if existing_names.iter().any(|n| n == trimmed) {
                state.error = Some("A profile with that name already exists.".to_string());
                NameOutcome::Redraw
            } else {
                NameOutcome::Proceed(trimmed.to_string())
            }
        }
        KeyCode::Char(c) => {
            state.name_input.push(c);
            state.error = None;
            NameOutcome::Redraw
        }
        KeyCode::Backspace => {
            state.name_input.pop();
            state.error = None;
            NameOutcome::Redraw
        }
        _ => NameOutcome::None,
    }
}

pub(super) fn draw_name(frame: &mut Frame, area: Rect, theme: &MokuTheme, state: &NameState) {
    let chunks = Layout::vertical([Constraint::Percentage(40), Constraint::Length(5), Constraint::Percentage(40)]).split(area);
    let box_area = Layout::horizontal([Constraint::Percentage(20), Constraint::Percentage(60), Constraint::Percentage(20)]).split(chunks[1])[1];

    let mut lines = vec![Line::raw(format!("Profile name: {}_", state.name_input))];
    if let Some(err) = &state.error {
        lines.push(Line::styled(err.clone(), Style::default().fg(theme.error)));
    }
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .title(" New Profile ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.info)),
        )
        .style(Style::default().bg(theme.base_bg).fg(theme.base_fg));
    frame.render_widget(para, box_area);
}

pub struct FormState {
    pub name: String,
    pub form: SettingsFormState,
}

impl FormState {
    pub fn new(name: String) -> Self {
        Self {
            name,
            form: SettingsFormState::from_form(&SimplePresetForm::default()),
        }
    }
}

pub(super) enum FormOutcome {
    None,
    Redraw,
    Cancel,
    Created { name: String, plan: Plan },
}

fn target_field_buffer(form: &mut SettingsFormState) -> Option<&mut String> {
    match form.focus {
        SettingsField::WorkMinutes => Some(&mut form.work_minutes),
        SettingsField::BreakMinutes => Some(&mut form.break_minutes),
        SettingsField::CyclesBeforeLongBreak if form.enable_long_break => Some(&mut form.cycles_before_long_break),
        SettingsField::LongBreakMinutes if form.enable_long_break => Some(&mut form.long_break_minutes),
        SettingsField::TotalRepeats if !form.infinite => Some(&mut form.total_repeats),
        _ => None,
    }
}

pub(super) fn handle_form_event(state: &mut FormState, event: &Event, ctx: &AppContext) -> FormOutcome {
    let Event::Key(key) = event else { return FormOutcome::None };
    if key.kind != KeyEventKind::Press {
        return FormOutcome::None;
    }

    // Field-to-field movement goes through the shared Up/Down resolver
    // (k/j/arrows, any user override) — not Tab.
    let command = resolve_event(event, &ctx.config.load().keys, None);
    let form = &mut state.form;
    match command {
        Command::Up => {
            form.focus = form.focus.prev(form.enable_long_break, form.infinite);
            return FormOutcome::Redraw;
        }
        Command::Down => {
            form.focus = form.focus.next(form.enable_long_break, form.infinite);
            return FormOutcome::Redraw;
        }
        Command::Back => return FormOutcome::Cancel,
        _ => {}
    }

    match key.code {
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
            if matches!(form.focus, SettingsField::EnableLongBreak | SettingsField::Infinite) =>
        {
            match form.focus {
                SettingsField::EnableLongBreak => form.enable_long_break = !form.enable_long_break,
                SettingsField::Infinite => form.infinite = !form.infinite,
                _ => unreachable!(),
            }
            form.error = None;
            FormOutcome::Redraw
        }
        KeyCode::Enter => match form.try_build() {
            Ok(simple) => {
                let plan = build_plan_from_simple_form(&simple);
                FormOutcome::Created {
                    name: state.name.clone(),
                    plan,
                }
            }
            Err(e) => {
                state.form.error = Some(e);
                FormOutcome::Redraw
            }
        },
        KeyCode::Char(c) if c.is_ascii_digit() => {
            if let Some(buf) = target_field_buffer(form) {
                buf.push(c);
                form.error = None;
            }
            FormOutcome::Redraw
        }
        KeyCode::Backspace => {
            if let Some(buf) = target_field_buffer(form) {
                buf.pop();
                form.error = None;
            }
            FormOutcome::Redraw
        }
        _ => FormOutcome::None,
    }
}

pub(super) fn draw_form(frame: &mut Frame, area: Rect, theme: &MokuTheme, state: &FormState) {
    let form = &state.form;
    let marker = |focused: bool| if focused { ">" } else { " " };
    let field_style = |focused: bool| {
        if focused {
            Style::default().fg(theme.selection_fg)
        } else {
            Style::default().fg(theme.base_fg)
        }
    };

    let mut lines = vec![
        Line::styled(
            format!("{} Work minutes:  {}", marker(form.focus == SettingsField::WorkMinutes), form.work_minutes),
            field_style(form.focus == SettingsField::WorkMinutes),
        ),
        Line::styled(
            format!("{} Break minutes: {}", marker(form.focus == SettingsField::BreakMinutes), form.break_minutes),
            field_style(form.focus == SettingsField::BreakMinutes),
        ),
        Line::styled(
            format!(
                "{} Long break:    {}  (Space to toggle)",
                marker(form.focus == SettingsField::EnableLongBreak),
                if form.enable_long_break { "on" } else { "off" }
            ),
            field_style(form.focus == SettingsField::EnableLongBreak),
        ),
    ];
    if form.enable_long_break {
        lines.push(Line::styled(
            format!(
                "{} Cycles before long break: {}",
                marker(form.focus == SettingsField::CyclesBeforeLongBreak),
                form.cycles_before_long_break
            ),
            field_style(form.focus == SettingsField::CyclesBeforeLongBreak),
        ));
        lines.push(Line::styled(
            format!(
                "{} Long break minutes: {}",
                marker(form.focus == SettingsField::LongBreakMinutes),
                form.long_break_minutes
            ),
            field_style(form.focus == SettingsField::LongBreakMinutes),
        ));
    }
    lines.push(Line::styled(
        format!(
            "{} Repeat forever: {}  (Space to toggle)",
            marker(form.focus == SettingsField::Infinite),
            if form.infinite { "on" } else { "off" }
        ),
        field_style(form.focus == SettingsField::Infinite),
    ));
    if !form.infinite {
        lines.push(Line::styled(
            format!("{} Total repeats: {}", marker(form.focus == SettingsField::TotalRepeats), form.total_repeats),
            field_style(form.focus == SettingsField::TotalRepeats),
        ));
    }
    if let Some(err) = &form.error {
        lines.push(Line::styled(format!("  {err}"), Style::default().fg(theme.error)));
    }

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(2)]).split(area);
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!(" New Profile: {} ", state.name))
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.info)),
        )
        .style(Style::default().bg(theme.base_bg));
    frame.render_widget(para, chunks[0]);

    let hint = Paragraph::new("[Up/Down/j/k] Switch field  [Space] Toggle  [Enter] Create  [Esc] Cancel")
        .alignment(Alignment::Center)
        .style(Style::default().fg(theme.base_fg));
    frame.render_widget(hint, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    #[test]
    fn test_name_empty_shows_error_and_stays() {
        let mut state = NameState::new();
        let outcome = handle_name_event(&mut state, &key(KeyCode::Enter), &[]);
        assert!(matches!(outcome, NameOutcome::Redraw));
        assert!(state.error.is_some());
    }

    #[test]
    fn test_name_reserved_classic_rejected() {
        let mut state = NameState::new();
        state.name_input = "classic".to_string();
        let outcome = handle_name_event(&mut state, &key(KeyCode::Enter), &[]);
        assert!(matches!(outcome, NameOutcome::Redraw));
        assert!(state.error.unwrap().contains("reserved"));
    }

    #[test]
    fn test_name_duplicate_rejected() {
        let mut state = NameState::new();
        state.name_input = "basic".to_string();
        let outcome = handle_name_event(&mut state, &key(KeyCode::Enter), &["basic".to_string()]);
        assert!(matches!(outcome, NameOutcome::Redraw));
    }

    #[test]
    fn test_name_valid_proceeds() {
        let mut state = NameState::new();
        state.name_input = "kitap".to_string();
        let outcome = handle_name_event(&mut state, &key(KeyCode::Enter), &["basic".to_string()]);
        assert!(matches!(outcome, NameOutcome::Proceed(n) if n == "kitap"));
    }

    #[test]
    fn test_name_esc_cancels() {
        let mut state = NameState::new();
        let outcome = handle_name_event(&mut state, &key(KeyCode::Esc), &[]);
        assert!(matches!(outcome, NameOutcome::Cancel));
    }
}
