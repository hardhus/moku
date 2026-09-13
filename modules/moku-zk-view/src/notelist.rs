use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use moku_core::MokuTheme;

use crate::jumplist::Ranker;
use crate::model::NoteEntry;

pub struct NoteListState {
    pub state: ListState,
    pub filter: String,
    pub filtering: bool,
    pub filtered: Vec<usize>,
}

impl NoteListState {
    pub fn new() -> Self {
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            state,
            filter: String::new(),
            filtering: false,
            filtered: Vec::new(),
        }
    }

    pub fn apply_filter(&mut self, notes: &[NoteEntry]) {
        if self.filter.is_empty() {
            self.filtered = (0..notes.len()).collect();
        } else {
            let mut ranker = Ranker::new(&self.filter);
            let mut scored: Vec<(usize, u32)> = notes
                .iter()
                .enumerate()
                .filter_map(|(i, n)| ranker.score(&n.rel_path).map(|s| (i, s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            self.filtered = scored.into_iter().map(|(i, _)| i).collect();
        }
        let out_of_range = self
            .state
            .selected()
            .map(|i| i >= self.filtered.len())
            .unwrap_or(true);
        if out_of_range {
            self.state.select(if self.filtered.is_empty() { None } else { Some(0) });
        }
    }

    pub fn selected_note_idx(&self) -> Option<usize> {
        self.state.selected().and_then(|i| self.filtered.get(i)).copied()
    }

    pub fn select_next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.filtered.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn select_previous(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + self.filtered.len() - 1) % self.filtered.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }
}

impl Default for NoteListState {
    fn default() -> Self {
        Self::new()
    }
}

pub enum NoteListAction {
    None,
    Open(usize),
    Back,
}

/// Two dispatch tiers, same idea as `tui_module.rs`'s Document-view
/// dispatch: while actively typing a filter (`filtering == true`) every
/// printable char/backspace feeds the filter; otherwise plain `j`/`k`/
/// arrows move the selection and `/` starts filtering.
pub fn handle_key(state: &mut NoteListState, notes: &[NoteEntry], key: &KeyEvent) -> NoteListAction {
    if state.filtering {
        match key.code {
            KeyCode::Esc => {
                state.filtering = false;
                state.filter.clear();
                state.apply_filter(notes);
                NoteListAction::None
            }
            KeyCode::Enter => {
                state.filtering = false;
                match state.selected_note_idx() {
                    Some(i) => NoteListAction::Open(i),
                    None => NoteListAction::None,
                }
            }
            KeyCode::Backspace => {
                state.filter.pop();
                state.apply_filter(notes);
                NoteListAction::None
            }
            KeyCode::Down => {
                state.select_next();
                NoteListAction::None
            }
            KeyCode::Up => {
                state.select_previous();
                NoteListAction::None
            }
            KeyCode::Char(c) => {
                state.filter.push(c);
                state.apply_filter(notes);
                NoteListAction::None
            }
            _ => NoteListAction::None,
        }
    } else {
        match key.code {
            KeyCode::Char('/') => {
                state.filtering = true;
                NoteListAction::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                state.select_next();
                NoteListAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                state.select_previous();
                NoteListAction::None
            }
            KeyCode::Enter => match state.selected_note_idx() {
                Some(i) => NoteListAction::Open(i),
                None => NoteListAction::None,
            },
            KeyCode::Esc | KeyCode::Char('q') => NoteListAction::Back,
            _ => NoteListAction::None,
        }
    }
}

pub fn draw_note_list(
    frame: &mut Frame,
    area: Rect,
    state: &mut NoteListState,
    notes: &[NoteEntry],
    error: Option<&str>,
    theme: &MokuTheme,
) {
    let items: Vec<ListItem> = if let Some(err) = error {
        vec![ListItem::new(format!("  Error: {err}")).style(Style::default().fg(theme.error))]
    } else if notes.is_empty() {
        vec![ListItem::new("  No notes found (or vault not configured).")]
    } else {
        state
            .filtered
            .iter()
            .filter_map(|&i| notes.get(i))
            .map(|n| ListItem::new(n.rel_path.clone()).style(Style::default().fg(theme.base_fg)))
            .collect()
    };
    let title = if state.filter.is_empty() {
        " Notes ".to_string()
    } else {
        format!(" Notes (filter: {}) ", state.filter)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(theme.border))
                .style(Style::default().bg(theme.base_bg)),
        )
        .highlight_style(
            Style::default()
                .fg(theme.selection_fg)
                .bg(theme.selection_bg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");
    frame.render_stateful_widget(list, area, &mut state.state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use satz_core::DocId;

    fn notes() -> Vec<NoteEntry> {
        vec![
            NoteEntry { id: DocId::new("a.md"), rel_path: "a.md".to_string() },
            NoteEntry { id: DocId::new("folder/b.md"), rel_path: "folder/b.md".to_string() },
        ]
    }

    #[test]
    fn test_apply_filter_empty_keeps_all() {
        let mut s = NoteListState::new();
        s.apply_filter(&notes());
        assert_eq!(s.filtered.len(), 2);
    }

    #[test]
    fn test_apply_filter_narrows_by_fuzzy_match() {
        let mut s = NoteListState::new();
        s.filter = "folder".to_string();
        s.apply_filter(&notes());
        assert_eq!(s.filtered, vec![1]);
    }

    #[test]
    fn test_select_next_wraps() {
        let mut s = NoteListState::new();
        s.apply_filter(&notes());
        s.state.select(Some(1));
        s.select_next();
        assert_eq!(s.state.selected(), Some(0));
    }
}
