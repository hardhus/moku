use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Frame, layout::Rect};
use tempfile::tempdir;

use moku_core::security::{SecurityManager, VaultSession};
use moku_core::{
    AppContext, ModuleId, ModuleMeta, MokuConfig, MokuTheme, StorageManager, ToastType, TuiModule,
    TuiRegistry,
};

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

struct MockModule;

impl ModuleMeta for MockModule {
    fn id(&self) -> ModuleId {
        ModuleId::new("mock")
    }
    fn title(&self) -> &'static str {
        "Mock"
    }
}

#[async_trait]
impl TuiModule for MockModule {
    async fn handle_event(&mut self, event: &Event, ctx: &mut AppContext) -> anyhow::Result<bool> {
        if let Event::Key(key) = event {
            match key.code {
                KeyCode::Char('n') => {
                    ctx.navigate_to(ModuleId::new("TargetModule"));
                    return Ok(true);
                }
                KeyCode::Char('t') => {
                    ctx.show_info("Test Toast");
                    return Ok(true);
                }
                KeyCode::Char('q') => {
                    ctx.quit();
                    return Ok(true);
                }
                _ => {}
            }
        }
        Ok(false)
    }

    fn draw(&mut self, _frame: &mut Frame, _area: Rect, _theme: &MokuTheme) {}
}

#[tokio::test]
async fn test_context_communication_flow() {
    let mut module = MockModule;
    let mut ctx = create_test_context().await;

    let event_n = Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()));
    module.handle_event(&event_n, &mut ctx).await.unwrap();
    assert_eq!(
        ctx.take_navigation().unwrap().as_str(),
        "TargetModule",
        "Incorrect navigation target!"
    );

    let event_t = Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::empty()));
    module.handle_event(&event_t, &mut ctx).await.unwrap();
    let toasts = ctx.drain_toasts();
    assert_eq!(toasts.len(), 1);
    assert_eq!(toasts[0].0, "Test Toast");
    assert_eq!(toasts[0].1, ToastType::Info);

    let event_q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::empty()));
    module.handle_event(&event_q, &mut ctx).await.unwrap();
    assert!(ctx.should_quit(), "Quit signal was not sent!");
}

#[test]
fn test_module_id_registration_and_safety() {
    let id1 = ModuleId::new("todo");
    let id2 = ModuleId::new("TODO");
    assert_ne!(
        id1, id2,
        "ModuleIds should not be case-insensitive (for security)"
    );

    let builtin_todo = ModuleId::TODO;
    assert_eq!(id1, builtin_todo);
}

#[tokio::test]
async fn test_registry_dispatch_simulation() {
    let mut registry = TuiRegistry::new();
    registry.insert(Box::new(MockModule));

    let mut ctx = create_test_context().await;
    let event = Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::empty()));

    if let Some(m) = registry.get_mut(ModuleId::new("mock")) {
        m.handle_event(&event, &mut ctx).await.unwrap();
    }
    assert!(ctx.take_navigation().is_some());
}
