use anyhow::Result;
use arboard::Clipboard;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState},
};

use moku_core::{AppContext, Command, ModuleId, ModuleMeta, MokuTheme, TuiModule, resolve_event};

use crate::engine::{FeedItem, RssEngine};

pub struct RssTuiModule {
    items: Vec<FeedItem>,
    state: ListState,
}

impl RssTuiModule {
    pub fn new() -> Self {
        Self { items: Vec::new(), state: ListState::default() }
    }

    fn next(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => if i >= self.items.len() - 1 { 0 } else { i + 1 },
            None => 0,
        };
        self.state.select(Some(i));
        true
    }

    fn previous(&mut self) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let i = match self.state.selected() {
            Some(i) => if i == 0 { self.items.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.state.select(Some(i));
        true
    }

    fn copy_selected_link(&self, ctx: &mut AppContext) {
        if let Some(i) = self.state.selected() {
            let link = self.items[i].link.clone();
            match Clipboard::new().and_then(|mut c| c.set_text(link.clone())) {
                Ok(_) => ctx.show_info(format!("Copied: {link}")),
                Err(e) => ctx.show_error(format!("Clipboard error: {e}")),
            }
        }
    }
}

impl Default for RssTuiModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleMeta for RssTuiModule {
    fn id(&self) -> ModuleId {
        ModuleId::RSS
    }
    fn title(&self) -> &'static str {
        ModuleId::RSS.title()
    }
}

#[async_trait]
impl TuiModule for RssTuiModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.items = RssEngine::load_items(&ctx.storage).await;
        if !self.items.is_empty() {
            self.state.select(Some(0));
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let command = resolve_event(event, &ctx.config.load().keys, None);

        let mut changed = match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                true
            }
            Command::Up => self.previous(),
            Command::Down => self.next(),
            _ => false,
        };

        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('c') => {
                        self.copy_selected_link(ctx);
                        changed = true;
                    }
                    // NOTE: 'r' starts a synchronous/blocking network request here.
                    // Since it is triggered by the user it is acceptable, but the TUI will
                    // appear frozen during the fetch (a few seconds) — background task + channel notifications could be added later.
                    KeyCode::Char('r') => {
                        match RssEngine::fetch_all(&ctx.storage).await {
                            Ok(new_items) => {
                                ctx.show_info(format!("{} new items", new_items.len()));
                                self.items = RssEngine::load_items(&ctx.storage).await;
                                if !self.items.is_empty() {
                                    self.state.select(Some(0));
                                }
                            }
                            Err(e) => ctx.show_error(format!("Refresh error: {e}")),
                        }
                        changed = true;
                    }
                    _ => {}
                }
            }
        }
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let items: Vec<ListItem> = self
            .items
            .iter()
            .map(|i| ListItem::new(format!("[{}] {}", i.feed_title, i.title)))
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .title(" 📡 RSS | [r] Refresh | [c] Copy Link ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.border))
                    .style(Style::default().bg(theme.base_bg)),
            )
            .style(Style::default().fg(theme.base_fg))
            .highlight_style(
                Style::default().fg(theme.selection_fg).bg(theme.selection_bg).add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, area, &mut self.state);
    }
}
