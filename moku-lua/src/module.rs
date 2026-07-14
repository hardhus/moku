use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mlua::Lua;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, List, ListItem},
};

use moku_core::{AppContext, ModuleId, ModuleMeta, MokuTheme, TuiModule};

use crate::bridge::{LuaBridge, ToastKind, register_api};

pub struct LuaModule {
    id: ModuleId,
    title: &'static str,
    lua: Lua,
    bridge: Arc<Mutex<LuaBridge>>,
}

impl LuaModule {
    /// `id` and `title` must be leaked as `'static` by the caller using `Box::leak`
    /// (see moku-bin/src/registry.rs — Phase 7.3).
    pub fn load(id: ModuleId, title: &'static str, script_path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(script_path)
            .with_context(|| format!("Failed to read Lua script: {:?}", script_path))?;

        let lua = Lua::new();
        let bridge = Arc::new(Mutex::new(LuaBridge::default()));
        register_api(&lua, Arc::clone(&bridge)).context("Lua API registration failed")?;

        lua.load(&source)
            .exec()
            .with_context(|| format!("Failed to execute Lua script: {:?}", script_path))?;

        Ok(Self { id, title, lua, bridge })
    }

    /// Applies accumulated actions from the Lua side to the actual AppContext
    /// and clears the bridge. Must be called AFTER each init/handle_event.
    fn drain_bridge(&self, ctx: &mut AppContext) -> bool {
        let mut b = self.bridge.lock().unwrap();
        let mut changed = false;

        if let Some(target) = b.navigate_to.take() {
            // v1: only native module IDs are supported (e.g. "launcher").
            // Inter-plugin navigation is an advanced feature for later.
            match target.as_str() {
                "launcher" => ctx.navigate_to(ModuleId::LAUNCHER),
                other => ctx.show_warning(format!("Unknown target: {other}")),
            }
            changed = true;
        }
        for (msg, kind) in b.toasts.drain(..) {
            match kind {
                ToastKind::Info => ctx.show_info(msg),
                ToastKind::Warning => ctx.show_warning(msg),
                ToastKind::Error => ctx.show_error(msg),
            }
            changed = true;
        }
        if b.quit {
            ctx.quit();
            changed = true;
        }
        changed
    }
}

impl ModuleMeta for LuaModule {
    fn id(&self) -> ModuleId {
        self.id
    }
    fn title(&self) -> &'static str {
        self.title
    }
}

#[async_trait]
impl TuiModule for LuaModule {
    async fn init(&mut self, ctx: &mut AppContext) -> Result<()> {
        if let Ok(func) = self.lua.globals().get::<mlua::Function>("on_init") {
            func.call::<()>(()).context("Lua on_init returned an error")?;
        }
        self.drain_bridge(ctx);
        Ok(())
    }

    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> Result<bool> {
        let Event::Key(key) = event else { return Ok(false) };
        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }
        let key_str = key_event_to_string(key);

        let lua_changed = match self.lua.globals().get::<mlua::Function>("on_event") {
            Ok(func) => func.call::<bool>(key_str).unwrap_or(false),
            Err(_) => false,
        };

        let bridge_changed = self.drain_bridge(ctx);
        Ok(lua_changed || bridge_changed)
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &MokuTheme) {
        let result: mlua::Result<(String, Vec<String>)> = self
            .lua
            .globals()
            .get::<mlua::Function>("on_draw")
            .and_then(|f| f.call::<mlua::Table>(()))
            .and_then(|t| {
                let title: String = t.get("title").unwrap_or_else(|_| self.title.to_string());
                let lines: Vec<String> = t.get("lines").unwrap_or_default();
                Ok((title, lines))
            });

        let (title, lines) =
            result.unwrap_or_else(|_| (self.title.to_string(), vec!["(on_draw not defined)".to_string()]));

        let items: Vec<ListItem> = lines.into_iter().map(ListItem::new).collect();
        let list = List::new(items).block(
            Block::default()
                .title(format!(" {} ", title))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .style(Style::default().bg(theme.base_bg).fg(theme.base_fg)),
        );
        frame.render_widget(list, area);
    }
}

/// Converts a crossterm KeyEvent into a string that uses the SAME dictionary
/// as the keybinding strings in config.toml (e.g., "ctrl-s", "up", "q") —
/// allowing Lua script authors to reuse the keybinding format they already know.
fn key_event_to_string(key: &crossterm::event::KeyEvent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("ctrl".to_string());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("alt".to_string());
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("shift".to_string());
    }

    let code = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        _ => "unknown".to_string(),
    };
    parts.push(code);
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp_script(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[test]
    fn test_lua_module_loads_and_parses() {
        let script = write_temp_script(
            r#"
            function on_draw()
                return { title = "T", lines = {"a", "b"} }
            end
            "#,
        );

        let module = LuaModule::load(ModuleId::new("test_lua"), "Test", script.path());
        assert!(module.is_ok(), "{:?}", module.err());
    }

    #[test]
    fn test_lua_module_rejects_syntax_error() {
        let script = write_temp_script("this is not valid lua ((((");
        let module = LuaModule::load(ModuleId::new("test_lua_bad"), "Bad", script.path());
        assert!(module.is_err());
    }
}
