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
            Ok(func) => match func.call::<bool>(key_str) {
                Ok(changed) => changed,
                Err(e) => {
                    ctx.show_error(format!("Lua on_event error: {e}"));
                    false
                }
            },
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

    async fn build_test_context() -> AppContext {
        use std::sync::Arc;

        use arc_swap::ArcSwap;
        use moku_core::security::{SecurityManager, VaultSession};
        use moku_core::{MokuConfig, StorageManager};

        let temp = tempfile::tempdir().unwrap();
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

    fn empty_module() -> LuaModule {
        let script = write_temp_script("-- no handlers defined");
        LuaModule::load(ModuleId::new("test_lua_empty"), "Empty", script.path()).unwrap()
    }

    #[tokio::test]
    async fn test_drain_bridge_applies_navigate_to_launcher() {
        let module = empty_module();
        module.bridge.lock().unwrap().navigate_to = Some("launcher".to_string());
        let mut ctx = build_test_context().await;

        let changed = module.drain_bridge(&mut ctx);
        assert!(changed);
        assert_eq!(ctx.take_navigation(), Some(ModuleId::LAUNCHER));
    }

    #[tokio::test]
    async fn test_drain_bridge_warns_on_unknown_navigate_target() {
        let module = empty_module();
        module.bridge.lock().unwrap().navigate_to = Some("some-other-plugin".to_string());
        let mut ctx = build_test_context().await;

        module.drain_bridge(&mut ctx);
        assert!(ctx.take_navigation().is_none());
        let toasts = ctx.drain_toasts();
        assert_eq!(toasts.len(), 1);
        assert!(toasts[0].0.contains("some-other-plugin"));
    }

    #[tokio::test]
    async fn test_drain_bridge_applies_toasts_of_each_kind() {
        let module = empty_module();
        {
            let mut b = module.bridge.lock().unwrap();
            b.toasts.push(("info msg".to_string(), ToastKind::Info));
            b.toasts.push(("warn msg".to_string(), ToastKind::Warning));
            b.toasts.push(("err msg".to_string(), ToastKind::Error));
        }
        let mut ctx = build_test_context().await;

        let changed = module.drain_bridge(&mut ctx);
        assert!(changed);
        let toasts = ctx.drain_toasts();
        assert_eq!(toasts.len(), 3);
        assert_eq!(toasts[0].0, "info msg");
        assert_eq!(toasts[1].0, "warn msg");
        assert_eq!(toasts[2].0, "err msg");
    }

    #[tokio::test]
    async fn test_drain_bridge_applies_quit() {
        let module = empty_module();
        module.bridge.lock().unwrap().quit = true;
        let mut ctx = build_test_context().await;

        let changed = module.drain_bridge(&mut ctx);
        assert!(changed);
        assert!(ctx.should_quit());
    }

    #[tokio::test]
    async fn test_drain_bridge_reports_no_change_when_bridge_is_empty() {
        let module = empty_module();
        let mut ctx = build_test_context().await;

        assert!(!module.drain_bridge(&mut ctx));
    }

    #[test]
    fn test_key_event_to_string_plain_char() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        assert_eq!(key_event_to_string(&key), "q");
    }

    #[test]
    fn test_key_event_to_string_with_modifiers_in_ctrl_alt_shift_order() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(key_event_to_string(&key), "ctrl-alt-shift-s");
    }

    #[test]
    fn test_key_event_to_string_named_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert_eq!(
            key_event_to_string(&KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
            "enter"
        );
        assert_eq!(
            key_event_to_string(&KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            "esc"
        );
        assert_eq!(
            key_event_to_string(&KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
            "up"
        );
    }

    #[tokio::test]
    async fn test_handle_event_calls_on_event_and_returns_its_result() {
        let script = write_temp_script(
            r#"
            function on_event(key)
                return key == "q"
            end
            "#,
        );
        let mut module = LuaModule::load(ModuleId::new("test_lua_event"), "Event", script.path()).unwrap();
        let mut ctx = build_test_context().await;

        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        key.kind = KeyEventKind::Press;
        let changed = module.handle_event(&Event::Key(key), &mut ctx).await.unwrap();
        assert!(changed, "on_event returning true should report a change");

        let mut key2 = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::empty());
        key2.kind = KeyEventKind::Press;
        let changed2 = module.handle_event(&Event::Key(key2), &mut ctx).await.unwrap();
        assert!(!changed2, "on_event returning false should report no change");
    }

    #[tokio::test]
    async fn test_handle_event_surfaces_lua_runtime_error_via_show_error() {
        let script = write_temp_script(
            r#"
            function on_event(key)
                error("boom")
            end
            "#,
        );
        let mut module = LuaModule::load(ModuleId::new("test_lua_err"), "Err", script.path()).unwrap();
        let mut ctx = build_test_context().await;

        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let mut key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty());
        key.kind = KeyEventKind::Press;
        let changed = module.handle_event(&Event::Key(key), &mut ctx).await.unwrap();
        assert!(!changed);

        let toasts = ctx.drain_toasts();
        assert_eq!(toasts.len(), 1, "a Lua runtime error must surface as a toast, not vanish silently");
        assert!(toasts[0].0.contains("boom"));
    }
}
