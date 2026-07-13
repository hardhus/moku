use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

use moku_core::{Command, MokuConfig, resolve_event};

#[derive(Deserialize, Default, PartialEq, Debug)]
#[serde(default)]
struct TestModuleConfig {
    enable_feature_x: bool,
    retry_count: u32,
}

#[test]
fn test_complex_config_and_theme_integration() {
    let raw_toml = r##"
    [general]
    theme = "custom_dark"

    [themes.custom_dark]
    base_fg = "#FF00FF" # Magenta
    base_bg = "Black"
    border = "White"
    selection_fg = "Yellow"
    selection_bg = "Reset"
    info = "Blue"
    warning = "Yellow"
    error = "Red"

    [modules.plugin_a]
    enable_feature_x = true
    "##;

    let config: MokuConfig = toml::from_str(raw_toml).expect("TOML parse error");

    // 1. THEME INTEGRATION TEST: Is HEX -> RGB conversion correct?
    let theme = config.get_active_theme();
    assert_eq!(theme.base_fg, ratatui::style::Color::Rgb(255, 0, 255));
    assert_eq!(theme.base_bg, ratatui::style::Color::Black);

    // 2. MODULARITY TEST
    let conf: TestModuleConfig = config.resolve_module_config("plugin_a");
    assert!(conf.enable_feature_x);
}

#[test]
fn test_nested_key_priority_flow() {
    // In globals UP='k', but let UP='u' in Todo module.
    let raw_toml = r#"
    [keys]
    up = "k"
    confirm = "enter"

    [modules.todo.keys]
    up = "u"
    # confirm is not defined here, should come from globals
    "#;

    let config: MokuConfig = toml::from_str(raw_toml).unwrap();

    #[derive(Deserialize, Default)]
    struct KeyMap {
        keys: HashMap<String, String>,
    }
    let todo_keys: KeyMap = config.resolve_module_config("todo");

    // Scenario A: Inside the module, pressed 'u' -> should be UP (Override)
    let event_u =
        crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::empty()));
    let cmd_u = resolve_event(&event_u, &config.keys, Some(&todo_keys.keys));
    assert_eq!(cmd_u, Command::Up);

    // Scenario B: Inside the module, pressed 'k' (Global UP) -> should be UP (Global Fallback)
    let event_k =
        crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::empty()));
    let cmd_k = resolve_event(&event_k, &config.keys, Some(&todo_keys.keys));
    assert_eq!(cmd_k, Command::Up);

    // Scenario C: Inside the module, pressed Enter -> should be Confirm (Global Fallback)
    let event_enter =
        crossterm::event::Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()));
    let cmd_enter = resolve_event(&event_enter, &config.keys, Some(&todo_keys.keys));
    assert_eq!(cmd_enter, Command::Confirm);
}
