use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Tabs},
};

mod tabs;

use moku_core::{
    AppContext, Command, ConfigManager, ModuleId, ModuleMeta, MokuConfig, MokuTheme, TuiModule,
    resolve_event,
};

use crate::tabs::{
    SettingsTab, context::ContextTab, general::GeneralTab, keybindings::KeybindingsTab,
};

pub struct SettingsModule {
    tabs: Vec<Box<dyn SettingsTab + Send + Sync>>,
    selected_tab: usize,
    config: Option<Arc<arc_swap::ArcSwap<MokuConfig>>>,
}

impl SettingsModule {
    pub fn new(config: &MokuConfig) -> Self {
        Self {
            tabs: vec![
                Box::new(GeneralTab::new(config)),
                Box::new(ContextTab::new(config)),
                Box::new(KeybindingsTab::new()),
            ],
            selected_tab: 0,
            config: None,
        }
    }

    fn next_tab(&mut self) {
        self.selected_tab = (self.selected_tab + 1) % self.tabs.len();
    }

    fn previous_tab(&mut self) {
        if self.selected_tab > 0 {
            self.selected_tab -= 1;
        } else {
            self.selected_tab = self.tabs.len() - 1;
        }
    }
}

impl ModuleMeta for SettingsModule {
    fn id(&self) -> ModuleId {
        ModuleId::SETTINGS
    }
    fn title(&self) -> &'static str {
        ModuleId::SETTINGS.title()
    }
}

#[async_trait]
impl TuiModule for SettingsModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        self.config = Some(Arc::clone(&ctx.config));
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let mut changed = false;
        let overrides = ctx
            .config
            .load()
            .get_module_keys(ModuleId::SETTINGS.as_str());

        let command = resolve_event(event, &ctx.config.load().keys, overrides.as_ref());

        // Main navigation (global commands before tab switching)
        match command {
            Command::Quit | Command::Back => {
                ctx.navigate_to(ModuleId::LAUNCHER);
                return Ok(true);
            }
            _ => {}
        }

        if let Event::Key(key) = event {
            if key.kind == crossterm::event::KeyEventKind::Press
                || key.kind == crossterm::event::KeyEventKind::Repeat
            {
                match key.code {
                    // TAB -> Next Tab
                    KeyCode::Tab => {
                        self.next_tab();
                        return Ok(true);
                    }
                    // SHIFT+TAB -> Previous Tab
                    KeyCode::BackTab => {
                        self.previous_tab();
                        return Ok(true);
                    }
                    // Ctrl+S -> Save (Global Shortcut)
                    KeyCode::Char('s')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        if let Err(e) = ConfigManager::save(&*ctx.config.load()).await {
                            ctx.show_error(format!("Save Failed: {}", e));
                        } else {
                            ctx.show_info("Settings Saved! 💾");
                        }
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        }

        if let Some(tab) = self.tabs.get_mut(self.selected_tab) {
            let before = ctx.config.load();
            tab.handle_event(event, ctx)?;
            let after = ctx.config.load();
            if !Arc::ptr_eq(&before, &after) {
                changed = true;
            }
        }

        let keys_to_change = matches!(
            command,
            Command::Up | Command::Down | Command::Left | Command::Right | Command::Confirm
        );

        Ok(changed || keys_to_change)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let Some(ref config_arc) = self.config else {
            return;
        };
        let config = config_arc.load();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Tab Bar
                Constraint::Min(0),    // Content
                Constraint::Length(3), // Footer
            ])
            .split(area);

        let tab_titles: Vec<String> = self.tabs.iter().map(|t| t.title().to_string()).collect();
        let tabs_widget = Tabs::new(tab_titles)
            .block(Block::default().borders(Borders::ALL).title(" Settings "))
            .highlight_style(
                Style::default()
                    .fg(theme.selection_fg)
                    .add_modifier(Modifier::BOLD),
            )
            .select(self.selected_tab)
            .divider(" | ");

        frame.render_widget(tabs_widget, chunks[0]);

        if let Some(tab) = self.tabs.get_mut(self.selected_tab) {
            let _ = tab.draw(frame, chunks[1], &config);
        }

        let footer =
            Paragraph::new("Tab: [TAB] | Change: [Arrows/Enter] | Save: [Ctrl+S] | Exit: [ESC]")
                .style(Style::default().fg(theme.base_fg))
                .alignment(ratatui::layout::Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::TOP)
                        .border_style(Style::default().fg(theme.border)),
                );

        frame.render_widget(footer, chunks[2]);
    }
}
