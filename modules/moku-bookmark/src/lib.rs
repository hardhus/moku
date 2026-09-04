use anyhow::Result;
use arboard::Clipboard;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use ratatui::{Frame, layout::Rect, widgets::ListState};

pub mod engine;
pub mod filter;
pub mod io;
pub mod model;
pub mod ui;

use moku_core::{
    AppContext, Command, ModuleId, ModuleMeta, ModuleStatus, MokuTheme, TuiModule, resolve_event,
};

use crate::engine::BookmarkEngine;
use crate::filter::BookmarkFilter;
use crate::io::BookmarkIO;
use crate::model::{
    Bookmark, MODE_CONFIRM_DELETE, MODE_DOMAIN_FILTER_PREFIX, MODE_INPUT, MODE_NORMAL, MODE_SEARCH,
};
use crate::ui::BookmarkUi;

#[derive(PartialEq)]
enum AppMode {
    Normal,
    Input,
    Search,
    DomainFilter(String),
    /// Waiting for the user to confirm deleting the currently selected
    /// bookmark. Plain `d` enters this instead of deleting right away;
    /// `Shift+D` (`moku_core::is_delete_bypass`) still deletes immediately.
    ConfirmDelete,
}

pub struct BookmarkModule {
    items: Vec<Bookmark>,
    filtered_items: Vec<Bookmark>,
    state: ListState,
    input_buffer: String,
    mode: AppMode,
}

impl Default for BookmarkModule {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            filtered_items: Vec::new(),
            state: ListState::default(),
            input_buffer: String::new(),
            mode: AppMode::Normal,
        }
    }
}

impl BookmarkModule {
    pub fn new() -> Self {
        Self::default()
    }

    fn refresh_filter(&mut self) {
        match &self.mode {
            AppMode::Search => {
                self.filtered_items = BookmarkFilter::fuzzy(&self.items, &self.input_buffer);
            }
            AppMode::DomainFilter(domain) => {
                self.filtered_items = BookmarkFilter::by_domain(&self.items, domain);
            }
            _ => {
                self.filtered_items = self.items.clone();
            }
        }

        if self.filtered_items.is_empty() {
            self.state.select(None);
        } else {
            self.state.select(Some(0));
        }
    }

    fn copy_selected_to_clipboard(&self, ctx: &mut AppContext) {
        if let Some(i) = self.state.selected() {
            let url = &self.filtered_items[i].url;
            match Clipboard::new() {
                Ok(mut clipboard) => {
                    if let Err(e) = clipboard.set_text(url.clone()) {
                        ctx.show_error(format!("Clipboard copy error: {}", e));
                    } else {
                        ctx.show_info(format!("Copied to clipboard: {}", url));
                    }
                }
                Err(e) => ctx.show_error(format!("No clipboard access: {}", e)),
            }
        }
    }

    async fn paste_and_save_from_clipboard(&mut self, ctx: &mut AppContext) -> Result<bool> {
        match Clipboard::new() {
            Ok(mut clipboard) => {
                if let Ok(text) = clipboard.get_text() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        let new_bm = if let Some((title, url)) = trimmed.rsplit_once(" | ") {
                            let mut bm = Bookmark::new(url.trim().to_string());
                            bm.name = Some(title.trim().to_string());
                            bm
                        } else {
                            Bookmark::new(trimmed.to_string())
                        };

                        self.items.push(new_bm);
                        BookmarkEngine::save_all(ctx, &self.items).await?;
                        ctx.show_info("Imported from clipboard 🔐");
                        self.refresh_filter();
                        return Ok(true);
                    } else {
                        ctx.show_warning("Clipboard is empty");
                    }
                }
            }
            Err(e) => ctx.show_error(format!("Clipboard error: {}", e)),
        }
        Ok(false)
    }

    /// Deletes the currently selected bookmark. Shared by the `Shift+D`
    /// bypass and the confirmation prompt's `y`/Enter so both paths run
    /// identical logic (matches the original `Command::Delete` arm's own
    /// error propagation via `?`, unchanged).
    async fn delete_selected(&mut self, ctx: &mut AppContext) -> Result<bool> {
        let Some(i) = self.state.selected() else {
            return Ok(false);
        };
        let target_url = self.filtered_items[i].url.clone();
        self.items.retain(|b| b.url != target_url);
        BookmarkEngine::save_all(ctx, &self.items).await?;
        ctx.show_info("Deleted");
        self.refresh_filter();
        Ok(true)
    }

    fn command_up(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.filtered_items.len().saturating_sub(1)
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn command_down(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.filtered_items.len().saturating_sub(1) {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }
}

impl ModuleMeta for BookmarkModule {
    fn id(&self) -> ModuleId {
        ModuleId::BOOKMARK
    }

    fn title(&self) -> &'static str {
        ModuleId::BOOKMARK.title()
    }
}

#[async_trait]
impl TuiModule for BookmarkModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        match BookmarkEngine::load_all(ctx).await {
            Ok(loaded) => {
                self.items = loaded;
                self.refresh_filter();

                if !self.filtered_items.is_empty() {
                    self.state.select(Some(0));
                }
                Ok(())
            }
            Err(e) => {
                ctx.show_error(format!("Load failed: {}", e));
                Err(e)
            }
        }
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let mut changed = false;

        // --- INPUT ve SEARCH MODE ---
        if self.mode == AppMode::Input || self.mode == AppMode::Search {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Enter => {
                            if self.mode == AppMode::Input {
                                if let Ok(new_bm) =
                                    BookmarkEngine::create_bookmark(self.input_buffer.clone())
                                {
                                    self.items.push(new_bm);
                                    BookmarkEngine::save_all(ctx, &self.items).await?;
                                    ctx.show_info("Saved successfully 🔐");
                                }
                            }
                            self.mode = AppMode::Normal;
                            self.input_buffer.clear();
                            changed = true;
                        }
                        KeyCode::Esc => {
                            self.mode = AppMode::Normal;
                            self.input_buffer.clear();
                            self.refresh_filter();
                            changed = true;
                        }
                        KeyCode::Char(c) => {
                            self.input_buffer.push(c);
                            if self.mode == AppMode::Search {
                                self.refresh_filter();
                            }
                            changed = true;
                        }
                        KeyCode::Backspace => {
                            self.input_buffer.pop();
                            if self.mode == AppMode::Search {
                                self.refresh_filter();
                            }
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            return Ok(changed);
        }

        // --- DOMAIN FILTER MODE ---
        if let AppMode::DomainFilter(_) = self.mode {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('r') => {
                            self.mode = AppMode::Normal;
                            self.refresh_filter();
                            changed = true;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            self.command_down();
                            changed = true;
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            self.command_up();
                            changed = true;
                        }
                        KeyCode::Char('c') => {
                            self.copy_selected_to_clipboard(ctx);
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            return Ok(changed);
        }

        // --- CONFIRM DELETE MODE ---
        if self.mode == AppMode::ConfirmDelete {
            if let Event::Key(key) = event {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Enter | KeyCode::Char('y') => {
                            changed = self.delete_selected(ctx).await?;
                            self.mode = AppMode::Normal;
                        }
                        KeyCode::Esc | KeyCode::Char('n') => {
                            self.mode = AppMode::Normal;
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            return Ok(changed);
        }

        // Shift+D bypasses the confirmation prompt entirely and deletes
        // immediately — checked as a raw key before resolve_event, same
        // shape as other raw Shift-key checks in this app (e.g.
        // moku-http's Shift+R).
        if moku_core::is_delete_bypass(event) {
            changed = self.delete_selected(ctx).await?;
            return Ok(changed);
        }

        // --- NORMAL MODE ---
        let command = resolve_event(event, &ctx.config.load().keys, None);

        match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            Command::Up => {
                self.command_up();
                changed = true;
            }
            Command::Down => {
                self.command_down();
                changed = true;
            }
            Command::Delete => {
                if self.state.selected().is_some() {
                    self.mode = AppMode::ConfirmDelete;
                    changed = true;
                }
            }
            _ => {
                if let Event::Key(key) = event {
                    if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
                        match key.code {
                            KeyCode::Char('a') => {
                                self.mode = AppMode::Input;
                                self.input_buffer.clear();
                                changed = true;
                            }
                            KeyCode::Char('/') => {
                                self.mode = AppMode::Search;
                                self.input_buffer.clear();
                                self.refresh_filter();
                                changed = true;
                            }
                            KeyCode::Char('c') => {
                                self.copy_selected_to_clipboard(ctx);
                                changed = true;
                            }
                            KeyCode::Char('p') => {
                                if self.paste_and_save_from_clipboard(ctx).await? {
                                    changed = true;
                                }
                            }
                            KeyCode::Char('f') => {
                                if let Some(i) = self.state.selected() {
                                    let domain = self.filtered_items[i].domain.clone();
                                    self.mode = AppMode::DomainFilter(domain);
                                    self.refresh_filter();
                                    changed = true;
                                }
                            }
                            KeyCode::Char('e') => {
                                match BookmarkIO::export_json(&self.items, "moku_bookmarks.json") {
                                    Ok(_) => {
                                        ctx.show_info("Exported to moku_bookmarks.json 📤");
                                        changed = true;
                                    }
                                    Err(e) => {
                                        ctx.show_error(format!("Export failed: {}", e));
                                    }
                                }
                            }
                            KeyCode::Char('i') => {
                                let file_to_import =
                                    if std::path::Path::new("bookmarks.html").exists() {
                                        "bookmarks.html"
                                    } else {
                                        "moku_bookmarks.json"
                                    };

                                match BookmarkIO::import_file(file_to_import) {
                                    Ok(mut imported_items) => {
                                        self.items.append(&mut imported_items);
                                        BookmarkEngine::remove_duplicates(&mut self.items);
                                        let _ = BookmarkEngine::save_all(ctx, &self.items).await;
                                        self.refresh_filter();
                                        ctx.show_info(format!(
                                            "Imported and encrypted {} 📥",
                                            file_to_import
                                        ));
                                        changed = true;
                                    }
                                    Err(e) => {
                                        ctx.show_error(format!("Import failed: {}", e));
                                    }
                                }
                            }
                            KeyCode::Char('x') => {
                                let removed_count =
                                    BookmarkEngine::remove_duplicates(&mut self.items);
                                if removed_count > 0 {
                                    let _ = BookmarkEngine::save_all(ctx, &self.items).await;
                                    self.refresh_filter();
                                    ctx.show_info(format!(
                                        "Removed {} duplicate(s) 🧹",
                                        removed_count
                                    ));
                                    changed = true;
                                } else {
                                    ctx.show_warning("No duplicates found");
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        Ok(changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let (input_mode, search_mode) = match self.mode {
            AppMode::Input => (true, false),
            AppMode::Search => (false, true),
            _ => (false, false),
        };

        let mode_name = match &self.mode {
            AppMode::Input => MODE_INPUT.to_string(),
            AppMode::Search => MODE_SEARCH.to_string(),
            AppMode::DomainFilter(d) => format!("{}: {}", MODE_DOMAIN_FILTER_PREFIX, d),
            AppMode::Normal => MODE_NORMAL.to_string(),
            AppMode::ConfirmDelete => {
                let label = self
                    .state
                    .selected()
                    .and_then(|i| self.filtered_items.get(i))
                    .map(|b| b.name.clone().unwrap_or_else(|| b.url.clone()))
                    .unwrap_or_default();
                format!("{}: {}", MODE_CONFIRM_DELETE, label)
            }
        };

        BookmarkUi::draw(
            frame,
            area,
            theme,
            &self.filtered_items,
            &mut self.state,
            &self.input_buffer,
            input_mode,
            search_mode,
            &mode_name,
        );
    }

    async fn dashboard_summary(&self, ctx: &AppContext) -> Option<ModuleStatus> {
        let needs_vault =
            moku_core::resolve_encryption(&ctx.config.load(), ModuleId::BOOKMARK.as_str(), true);
        if needs_vault && !ctx.session.is_unlocked() {
            return Some(ModuleStatus::locked());
        }
        let items = BookmarkEngine::load_all(ctx).await.unwrap_or_default();
        Some(ModuleStatus::normal(format!("{} bookmarks", items.len())))
    }
}

#[cfg(test)]
mod confirm_delete_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use crossterm::event::{KeyEvent, KeyModifiers};
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
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

    async fn module_with_one_bookmark() -> (BookmarkModule, AppContext) {
        let mut module = BookmarkModule::new();
        let ctx = create_test_context().await;
        let key = SecurityManager::derive_key("test-pass", &[9u8; 16])
            .await
            .unwrap();
        ctx.session.unlock(key);
        module.items = vec![Bookmark::new("https://example.com".to_string())];
        module.refresh_filter();
        (module, ctx)
    }

    #[tokio::test]
    async fn test_plain_d_does_not_delete_and_shows_confirm_mode() {
        let (mut module, mut ctx) = module_with_one_bookmark().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(module.items.len(), 1, "plain 'd' must not delete anything");
        assert!(module.mode == AppMode::ConfirmDelete);
    }

    #[tokio::test]
    async fn test_shift_d_deletes_immediately() {
        let (mut module, mut ctx) = module_with_one_bookmark().await;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(module.items.is_empty(), "Shift+D should delete immediately");
    }

    #[tokio::test]
    async fn test_confirm_delete_yes_deletes() {
        let (mut module, mut ctx) = module_with_one_bookmark().await;
        module.mode = AppMode::ConfirmDelete;
        let event = Event::Key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert!(module.items.is_empty());
        assert!(module.mode == AppMode::Normal);
    }

    #[tokio::test]
    async fn test_confirm_delete_no_cancels() {
        let (mut module, mut ctx) = module_with_one_bookmark().await;
        module.mode = AppMode::ConfirmDelete;
        let event = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()));
        module.handle_event(&event, &mut ctx).await.unwrap();

        assert_eq!(module.items.len(), 1, "cancelling must not delete anything");
        assert!(module.mode == AppMode::Normal);
    }
}

#[cfg(test)]
mod dashboard_summary_tests {
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use moku_core::security::{SecurityManager, VaultSession};
    use moku_core::{MokuConfig, StorageManager};
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

    #[tokio::test]
    async fn test_dashboard_summary_locked_when_vault_not_unlocked() {
        let module = BookmarkModule::new();
        let ctx = create_test_context().await;
        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Locked);
    }

    #[tokio::test]
    async fn test_dashboard_summary_reports_count_when_unlocked() {
        let module = BookmarkModule::new();
        let ctx = create_test_context().await;
        let key = SecurityManager::derive_key("test-pass", &[3u8; 16])
            .await
            .unwrap();
        ctx.session.unlock(key);

        let items = vec![
            crate::model::Bookmark::new("https://a.example".to_string()),
            crate::model::Bookmark::new("https://b.example".to_string()),
        ];
        crate::engine::BookmarkEngine::save_all(&ctx, &items)
            .await
            .unwrap();

        let status = module.dashboard_summary(&ctx).await.unwrap();
        assert_eq!(status.tone, moku_core::StatusTone::Normal);
        assert_eq!(status.text, "2 bookmarks");
    }
}
