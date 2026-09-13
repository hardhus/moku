use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};

use moku_core::MokuTheme;

use crate::model::{JumpItem, LinkResState, OpenDoc};

/// A small, dependency-free case-insensitive subsequence fuzzy scorer:
/// `None` if `target` doesn't contain the query's characters in order,
/// otherwise higher-is-better, rewarding contiguous runs and an early
/// match start. Self-contained rather than pulling in a fuzzy-matching
/// crate for this one small feature.
pub struct Ranker {
    needle: Vec<char>,
}

impl Ranker {
    pub fn new(query: &str) -> Self {
        Self {
            needle: query.to_lowercase().chars().collect(),
        }
    }

    pub fn score(&mut self, target: &str) -> Option<u32> {
        if self.needle.is_empty() {
            return Some(0);
        }
        let hay: Vec<char> = target.to_lowercase().chars().collect();
        let mut hi = 0usize;
        let mut score: u32 = 0;
        let mut run = 0u32;
        let mut first_match: Option<usize> = None;
        for &n in &self.needle {
            let mut found = false;
            while hi < hay.len() {
                if hay[hi] == n {
                    if first_match.is_none() {
                        first_match = Some(hi);
                    }
                    run += 1;
                    score += 1 + run;
                    hi += 1;
                    found = true;
                    break;
                }
                run = 0;
                hi += 1;
            }
            if !found {
                return None;
            }
        }
        let bonus = first_match.map(|i| 50u32.saturating_sub(i as u32)).unwrap_or(0);
        Some(score + bonus)
    }
}

fn item_label(item: &JumpItem) -> &str {
    match item {
        JumpItem::Link { label, .. } => label,
        JumpItem::Tag { name, .. } => name,
        JumpItem::Heading { text, .. } => text,
    }
}

pub fn collect_jump_items(open: &OpenDoc) -> Vec<JumpItem> {
    let mut items = Vec::new();
    for (i, l) in open.doc.links.iter().enumerate() {
        let label = l.display.clone().unwrap_or_else(|| {
            if l.target_doc.is_empty() {
                "(this document)".to_string()
            } else {
                l.target_doc.clone()
            }
        });
        let resolved = matches!(open.link_res.get(i), Some(LinkResState::Resolved { .. }));
        items.push(JumpItem::Link { link_idx: i, label, resolved });
    }
    for t in &open.doc.tags {
        items.push(JumpItem::Tag {
            name: t.name.clone(),
            byte_offset: t.range.start,
        });
    }
    for h in &open.doc.headings {
        items.push(JumpItem::Heading {
            level: h.level,
            text: h.text.clone(),
            byte_offset: h.range.start,
        });
    }
    items
}

pub struct JumpListState {
    pub query: String,
    pub items: Vec<JumpItem>,
    pub filtered: Vec<usize>,
    pub state: ListState,
}

impl JumpListState {
    pub fn new(open: &OpenDoc) -> Self {
        let items = collect_jump_items(open);
        let mut s = Self {
            query: String::new(),
            items,
            filtered: Vec::new(),
            state: ListState::default(),
        };
        s.apply_filter();
        s
    }

    pub fn apply_filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.items.len()).collect();
        } else {
            let mut ranker = Ranker::new(&self.query);
            let mut scored: Vec<(usize, u32)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, it)| ranker.score(item_label(it)).map(|s| (i, s)))
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

    pub fn selected_item(&self) -> Option<&JumpItem> {
        self.state
            .selected()
            .and_then(|i| self.filtered.get(i))
            .and_then(|&idx| self.items.get(idx))
    }
}

pub enum JumpAction {
    None,
    Close,
    JumpLink(usize),
    JumpOffset(usize),
}

pub fn handle_key(state: &mut JumpListState, key: &KeyEvent) -> JumpAction {
    match key.code {
        KeyCode::Esc => JumpAction::Close,
        KeyCode::Enter => match state.selected_item() {
            Some(JumpItem::Link { link_idx, .. }) => JumpAction::JumpLink(*link_idx),
            Some(JumpItem::Tag { byte_offset, .. }) => JumpAction::JumpOffset(*byte_offset),
            Some(JumpItem::Heading { byte_offset, .. }) => JumpAction::JumpOffset(*byte_offset),
            None => JumpAction::None,
        },
        KeyCode::Down => {
            state.select_next();
            JumpAction::None
        }
        KeyCode::Up => {
            state.select_previous();
            JumpAction::None
        }
        KeyCode::Backspace => {
            state.query.pop();
            state.apply_filter();
            JumpAction::None
        }
        KeyCode::Char(c) => {
            state.query.push(c);
            state.apply_filter();
            JumpAction::None
        }
        _ => JumpAction::None,
    }
}

pub fn draw_jump_list(frame: &mut Frame, area: Rect, state: &mut JumpListState, theme: &MokuTheme) {
    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .filter_map(|&i| state.items.get(i))
        .map(|it| {
            let (label, style) = match it {
                JumpItem::Link { label, resolved, .. } => (
                    format!("[link] {label}"),
                    Style::default().fg(if *resolved { theme.success } else { theme.error }),
                ),
                JumpItem::Tag { name, .. } => (format!("[tag] #{name}"), Style::default().fg(theme.info)),
                JumpItem::Heading { level, text, .. } => {
                    (format!("[h{level}] {text}"), Style::default().fg(theme.base_fg))
                }
            };
            ListItem::new(label).style(style)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Jump to (filter: {}) ", state.query))
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

    #[test]
    fn test_ranker_rejects_non_subsequence() {
        let mut r = Ranker::new("xyz");
        assert_eq!(r.score("abc"), None);
    }

    #[test]
    fn test_ranker_scores_earlier_and_contiguous_matches_higher() {
        let mut r1 = Ranker::new("ab");
        let mut r2 = Ranker::new("ab");
        let early = r1.score("abcdef").unwrap();
        let late = r2.score("xxxxxxab").unwrap();
        assert!(early > late);
    }

    #[test]
    fn test_ranker_empty_query_matches_everything_with_zero_score() {
        let mut r = Ranker::new("");
        assert_eq!(r.score("anything"), Some(0));
    }
}
