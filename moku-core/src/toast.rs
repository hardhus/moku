use std::time::{Duration, Instant};

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph},
};

use crate::config::MokuConfig;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ToastType {
    Info,
    Warning,
    Error,
}

#[derive(Debug)]
pub struct Toast {
    pub message: String,
    pub kind: ToastType,
    pub created_at: Instant,
    pub duration: Duration,
}

pub struct ToastManager {
    pub toasts: Vec<Toast>,
}

impl Default for ToastManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastManager {
    pub fn new() -> Self {
        Self { toasts: Vec::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    pub fn add(&mut self, message: impl Into<String>, kind: ToastType) {
        let duration = match kind {
            ToastType::Info => Duration::from_secs(2),
            ToastType::Warning => Duration::from_secs(4),
            ToastType::Error => Duration::from_secs(6),
        };

        self.toasts.push(Toast {
            message: message.into(),
            kind,
            created_at: Instant::now(),
            duration,
        });
    }

    pub fn update(&mut self) {
        let now = Instant::now();
        self.toasts
            .retain(|t| now.duration_since(t.created_at) < t.duration);
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, config: &MokuConfig) {
        let theme = config.get_active_theme();
        let width = std::cmp::min(area.width, 40);
        let mut y_offset = 2;

        for toast in self.toasts.iter().rev() {
            let height = 3;
            if y_offset + height > area.height {
                break;
            }

            let x = area.width.saturating_sub(width + 2);
            let toast_area = Rect::new(x, y_offset, width, height);

            let color = match toast.kind {
                ToastType::Info => theme.info,
                ToastType::Warning => theme.warning,
                ToastType::Error => theme.error,
            };

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .style(Style::default().bg(theme.base_bg));

            let p = Paragraph::new(toast.message.as_str()).block(block).style(
                Style::default()
                    .fg(theme.base_fg)
                    .add_modifier(Modifier::BOLD),
            );

            f.render_widget(p, toast_area);
            y_offset += height;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toast_addition_types() {
        let mut manager = ToastManager::new();
        manager.add("Info", ToastType::Info);
        manager.add("Error", ToastType::Error);

        assert_eq!(manager.toasts.len(), 2);
        assert_eq!(manager.toasts[0].duration, Duration::from_secs(2));
        assert_eq!(manager.toasts[1].duration, Duration::from_secs(6));
    }

    #[test]
    fn test_toast_cleanup_logic() {
        let mut manager = ToastManager::new();

        manager.toasts.push(Toast {
            message: "Expired".to_string(),
            kind: ToastType::Info,
            created_at: Instant::now() - Duration::from_secs(10),
            duration: Duration::from_secs(2),
        });

        manager.add("Active", ToastType::Info);
        assert_eq!(manager.toasts.len(), 2);

        manager.update();

        assert_eq!(manager.toasts.len(), 1);
        assert_eq!(manager.toasts[0].message, "Active");
    }

    #[test]
    fn test_toast_lifo_order() {
        let mut manager = ToastManager::new();
        manager.add("First", ToastType::Info);
        manager.add("Second", ToastType::Info);

        assert_eq!(manager.toasts[0].message, "First");
        assert_eq!(manager.toasts[1].message, "Second");
    }

    #[test]
    fn test_toast_is_empty_and_len() {
        let mut manager = ToastManager::new();
        assert!(manager.is_empty());
        manager.add("X", ToastType::Info);
        assert!(!manager.is_empty());
        assert_eq!(manager.len(), 1);
    }
}
