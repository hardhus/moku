use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::config::KeyBindings;

/// Standard commands used throughout the application.
#[derive(Debug, PartialEq, Clone)]
pub enum Command {
    None,
    Quit,
    Confirm,
    Back,
    Up,
    Down,
    Left,
    Right,

    Add,
    Delete,
    Toggle,
    Refresh,
    Search,

    Custom(String),
}

/// Resolves a raw terminal event into a semantic Command.
///
/// Priority Order:
/// 1. Module Specific Overrides (defined in [modules.x.keys])
/// 2. Global Key Bindings (defined in [keys])
/// 3. Hardcoded Defaults (Arrow keys, Esc, Enter)
pub fn resolve_event(
    event: &Event,
    global_keys: &KeyBindings,
    module_overrides: Option<&HashMap<String, String>>,
) -> Command {
    if let Event::Key(key) = event {
        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat {
            if let Some(cmd) = check_overrides(key, module_overrides) {
                return cmd;
            }

            if let Some(cmd) = check_globals(key, global_keys) {
                return cmd;
            }

            return check_hardcoded(key);
        }
    }
    Command::None
}

/// Checks if the key matches any module-specific override.
fn check_overrides(key: &KeyEvent, overrides: Option<&HashMap<String, String>>) -> Option<Command> {
    let overrides = overrides?;

    let mut clean_key = *key;
    clean_key.modifiers = clean_modifiers(key.modifiers);

    for (action, key_str) in overrides {
        if keys_match(clean_key, key_str) {
            return match action.to_lowercase().as_str() {
                // Navigation
                "up" => Some(Command::Up),
                "down" => Some(Command::Down),
                "left" => Some(Command::Left),
                "right" => Some(Command::Right),
                "confirm" | "select" => Some(Command::Confirm),
                "back" | "menu" => Some(Command::Back),
                "quit" => Some(Command::Quit),

                // Common Actions
                "add" => Some(Command::Add),
                "delete" => Some(Command::Delete),
                "toggle" => Some(Command::Toggle),
                "refresh" => Some(Command::Refresh),
                "search" => Some(Command::Search),

                custom_action => Some(Command::Custom(custom_action.to_string())),
            };
        }
    }
    None
}

fn clean_modifiers(m: KeyModifiers) -> KeyModifiers {
    m & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// Checks if the key matches global configuration.
fn check_globals(key: &KeyEvent, global_keys: &KeyBindings) -> Option<Command> {
    let mut clean_key = *key;
    clean_key.modifiers = clean_modifiers(key.modifiers);

    if keys_match(clean_key, &global_keys.up) {
        return Some(Command::Up);
    }
    if keys_match(clean_key, &global_keys.down) {
        return Some(Command::Down);
    }
    if keys_match(clean_key, &global_keys.select) {
        return Some(Command::Confirm);
    }
    if keys_match(clean_key, &global_keys.menu) {
        return Some(Command::Back);
    }
    if keys_match(clean_key, &global_keys.quit) {
        return Some(Command::Quit);
    }
    None
}

/// Standard fallback keys (Arrow keys, Enter, Esc).
fn check_hardcoded(key: &KeyEvent) -> Command {
    let relevant = clean_modifiers(key.modifiers);

    match key.code {
        KeyCode::Up => Command::Up,
        KeyCode::Down => Command::Down,
        KeyCode::Left => Command::Left,
        KeyCode::Right => Command::Right,
        KeyCode::Enter => Command::Confirm,
        KeyCode::Esc => Command::Back,
        KeyCode::Char('a') if relevant.is_empty() => Command::Add,
        KeyCode::Char('d') if relevant.is_empty() => Command::Delete,
        KeyCode::Char(' ') if relevant.is_empty() => Command::Toggle,
        KeyCode::Char('r') if relevant.is_empty() => Command::Refresh,
        // Some layouts (e.g. Turkish Q) produce '/' via Shift+7. On Windows,
        // crossterm reports the physical Shift bit even though it was
        // already consumed to produce the '/' character itself, so
        // `relevant.is_empty()` alone misses that case — accept Shift too.
        KeyCode::Char('/') if relevant.is_empty() || relevant == KeyModifiers::SHIFT => {
            Command::Search
        }
        _ => Command::None,
    }
}

pub fn keys_match(key_event: KeyEvent, config_str: &str) -> bool {
    let config_lower = config_str.to_lowercase();
    let mut parts: Vec<&str> = config_lower.split('-').collect();

    let key_code_str = parts.pop().unwrap_or("");

    let mut required_modifiers = KeyModifiers::empty();
    for part in parts {
        match part {
            "ctrl" => required_modifiers.insert(KeyModifiers::CONTROL),
            "alt" => required_modifiers.insert(KeyModifiers::ALT),
            "shift" => required_modifiers.insert(KeyModifiers::SHIFT),
            _ => {}
        }
    }

    let relevant_modifiers =
        key_event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
    if relevant_modifiers != required_modifiers {
        return false;
    }

    match key_event.code {
        KeyCode::Char(c) => c.to_lowercase().to_string() == key_code_str,
        KeyCode::Esc => key_code_str == "esc" || key_code_str == "escape",
        KeyCode::Enter => key_code_str == "enter" || key_code_str == "return",
        KeyCode::Backspace => key_code_str == "backspace",
        KeyCode::Tab => key_code_str == "tab",
        KeyCode::Up => key_code_str == "up",
        KeyCode::Down => key_code_str == "down",
        KeyCode::Left => key_code_str == "left",
        KeyCode::Right => key_code_str == "right",
        KeyCode::F(n) => key_code_str == format!("f{}", n),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::KeyBindings;

    // Helper to create a KeyEvent easily
    fn make_key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        })
    }

    #[test]
    fn test_keys_match_simple() {
        let key = KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT);
        assert!(keys_match(key, "shift-q"));
    }

    #[test]
    fn test_resolve_global_defaults() {
        let defaults = KeyBindings::default();
        let overrides = None;
        let k_press = make_key(KeyCode::Char('k'), KeyModifiers::empty());
        assert_eq!(resolve_event(&k_press, &defaults, overrides), Command::Up);
    }

    #[test]
    fn test_resolve_priority() {
        let defaults = KeyBindings::default();

        // 1. Hardcoded Check
        let arrow_up = make_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(resolve_event(&arrow_up, &defaults, None), Command::Up);

        // 2. Override Check
        let mut overrides = HashMap::new();
        overrides.insert("up".to_string(), "u".to_string());

        let u_key = make_key(KeyCode::Char('u'), KeyModifiers::empty());
        assert_eq!(
            resolve_event(&u_key, &defaults, Some(&overrides)),
            Command::Up
        );
    }

    #[test]
    fn test_custom_module_action() {
        let defaults = KeyBindings::default();
        let mut overrides = HashMap::new();

        overrides.insert("sort".to_string(), "ctrl-s".to_string());

        let event = make_key(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let command = resolve_event(&event, &defaults, Some(&overrides));

        assert_eq!(command, Command::Custom("sort".to_string()));
    }

    #[test]
    fn test_slash_via_shift_also_triggers_search() {
        // Turkish Q layout produces '/' via Shift+7; Windows/crossterm can
        // report the physical Shift bit even though it already produced the
        // shifted character, so both the bare and Shift-modified '/' must
        // resolve to Command::Search.
        let defaults = KeyBindings::default();
        let plain = make_key(KeyCode::Char('/'), KeyModifiers::empty());
        assert_eq!(resolve_event(&plain, &defaults, None), Command::Search);

        let shifted = make_key(KeyCode::Char('/'), KeyModifiers::SHIFT);
        assert_eq!(resolve_event(&shifted, &defaults, None), Command::Search);
    }

    #[test]
    fn test_modifier_masking_resilience() {
        let global_keys = KeyBindings::default();

        let event = Event::Key(KeyEvent {
            code: KeyCode::Char('j'),
            modifiers: KeyModifiers::from_bits(0x0008).unwrap(),
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        });

        let cmd = resolve_event(&event, &global_keys, None);
        assert_eq!(
            cmd,
            Command::Down,
            "Noise modifiers like NumLock should not break key resolution!"
        );
    }
}
