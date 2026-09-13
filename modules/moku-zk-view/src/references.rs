use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use satz_core::{DocId, Index};

use moku_core::MokuTheme;

pub struct ReferenceItem {
    pub doc_id: DocId,
    pub rel_path: String,
}

/// `gr` — "what links here": every document with a backlink to the
/// currently open one, via `Index::backlinks_of` (the same lookup
/// satz-lsp's `handlers/references.rs` uses for its own find-references).
pub struct ReferencesState {
    pub items: Vec<ReferenceItem>,
    pub state: ListState,
}

impl ReferencesState {
    pub fn new(index: &Index, doc_id: &DocId) -> Self {
        let mut items: Vec<ReferenceItem> = index
            .backlinks_of(doc_id)
            .filter_map(|id| {
                index.get_doc(id).map(|d| ReferenceItem {
                    doc_id: id.clone(),
                    rel_path: d.path.to_string_lossy().replace('\\', "/"),
                })
            })
            .collect();
        items.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let mut state = ListState::default();
        state.select(if items.is_empty() { None } else { Some(0) });
        Self { items, state }
    }

    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.items.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn select_previous(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => (i + self.items.len() - 1) % self.items.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn selected(&self) -> Option<&ReferenceItem> {
        self.state.selected().and_then(|i| self.items.get(i))
    }
}

pub enum ReferenceAction {
    None,
    Close,
    Open(DocId),
}

pub fn handle_key(state: &mut ReferencesState, key: &KeyEvent) -> ReferenceAction {
    match key.code {
        KeyCode::Esc => ReferenceAction::Close,
        KeyCode::Enter => match state.selected() {
            Some(item) => ReferenceAction::Open(item.doc_id.clone()),
            None => ReferenceAction::None,
        },
        KeyCode::Down | KeyCode::Char('j') => {
            state.select_next();
            ReferenceAction::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.select_previous();
            ReferenceAction::None
        }
        _ => ReferenceAction::None,
    }
}

pub fn draw_references(frame: &mut Frame, area: Rect, state: &mut ReferencesState, theme: &MokuTheme) {
    let items: Vec<ListItem> = if state.items.is_empty() {
        vec![ListItem::new("  Bu dokümana referans veren not yok.").style(Style::default().fg(theme.base_fg))]
    } else {
        state
            .items
            .iter()
            .map(|it| ListItem::new(it.rel_path.clone()).style(Style::default().fg(theme.base_fg)))
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" References (gr) ")
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
    use satz_core::{Index, parse_document};
    use std::path::Path;

    #[test]
    fn test_references_state_lists_backlinks_sorted() {
        let a = parse_document("[[c]]\n", Path::new("a.md"));
        let b = parse_document("[[c]]\n", Path::new("b.md"));
        let c = parse_document("# C\n", Path::new("c.md"));
        let index = Index::build(vec![a, b, c]);

        let state = ReferencesState::new(&index, &DocId::new("c.md"));
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].rel_path, "a.md");
        assert_eq!(state.items[1].rel_path, "b.md");
    }

    #[test]
    fn test_handle_key_j_and_k_move_selection() {
        let a = parse_document("[[c]]\n", Path::new("a.md"));
        let b = parse_document("[[c]]\n", Path::new("b.md"));
        let c = parse_document("# C\n", Path::new("c.md"));
        let index = Index::build(vec![a, b, c]);
        let mut state = ReferencesState::new(&index, &DocId::new("c.md"));
        assert_eq!(state.state.selected(), Some(0));

        let j = crossterm::event::KeyEvent::new(KeyCode::Char('j'), crossterm::event::KeyModifiers::empty());
        handle_key(&mut state, &j);
        assert_eq!(state.state.selected(), Some(1));

        let k = crossterm::event::KeyEvent::new(KeyCode::Char('k'), crossterm::event::KeyModifiers::empty());
        handle_key(&mut state, &k);
        assert_eq!(state.state.selected(), Some(0));
    }

    #[test]
    fn test_references_state_empty_when_no_backlinks() {
        let a = parse_document("# A\n", Path::new("a.md"));
        let index = Index::build(vec![a]);
        let state = ReferencesState::new(&index, &DocId::new("a.md"));
        assert!(state.items.is_empty());
        assert_eq!(state.selected().map(|i| i.rel_path.clone()), None);
    }
}
