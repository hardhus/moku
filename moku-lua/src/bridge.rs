use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub enum ToastKind {
    Info,
    Warning,
    Error,
}

#[derive(Default)]
pub struct LuaBridge {
    pub navigate_to: Option<String>,
    pub toasts: Vec<(String, ToastKind)>,
    pub quit: bool,
}

/// Registers the `moku.*` Lua API. Each function only writes to `bridge` —
/// the actual effect on `AppContext` happens on the Rust side, after the
/// Lua call finishes, inside `LuaModule::drain_bridge`.
pub fn register_api(lua: &mlua::Lua, bridge: Arc<Mutex<LuaBridge>>) -> mlua::Result<()> {
    let moku_table = lua.create_table()?;

    {
        let bridge = Arc::clone(&bridge);
        let f = lua.create_function(move |_, id: String| {
            bridge.lock().unwrap().navigate_to = Some(id);
            Ok(())
        })?;
        moku_table.set("navigate_to", f)?;
    }
    {
        let bridge = Arc::clone(&bridge);
        let f = lua.create_function(move |_, msg: String| {
            bridge.lock().unwrap().toasts.push((msg, ToastKind::Info));
            Ok(())
        })?;
        moku_table.set("show_info", f)?;
    }
    {
        let bridge = Arc::clone(&bridge);
        let f = lua.create_function(move |_, msg: String| {
            bridge.lock().unwrap().toasts.push((msg, ToastKind::Warning));
            Ok(())
        })?;
        moku_table.set("show_warning", f)?;
    }
    {
        let bridge = Arc::clone(&bridge);
        let f = lua.create_function(move |_, msg: String| {
            bridge.lock().unwrap().toasts.push((msg, ToastKind::Error));
            Ok(())
        })?;
        moku_table.set("show_error", f)?;
    }
    {
        let bridge = Arc::clone(&bridge);
        let f = lua.create_function(move |_, ()| {
            bridge.lock().unwrap().quit = true;
            Ok(())
        })?;
        moku_table.set("quit", f)?;
    }

    lua.globals().set("moku", moku_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bridge_captures_show_info() {
        let lua = mlua::Lua::new();
        let bridge = Arc::new(Mutex::new(LuaBridge::default()));
        register_api(&lua, Arc::clone(&bridge)).unwrap();

        lua.load(r#"moku.show_info("hello")"#).exec().unwrap();

        let b = bridge.lock().unwrap();
        assert_eq!(b.toasts.len(), 1);
        assert_eq!(b.toasts[0].0, "hello");
    }

    #[test]
    fn test_bridge_captures_navigate_and_quit() {
        let lua = mlua::Lua::new();
        let bridge = Arc::new(Mutex::new(LuaBridge::default()));
        register_api(&lua, Arc::clone(&bridge)).unwrap();

        lua.load(r#"moku.navigate_to("launcher"); moku.quit()"#).exec().unwrap();

        let b = bridge.lock().unwrap();
        assert_eq!(b.navigate_to.as_deref(), Some("launcher"));
        assert!(b.quit);
    }
}
