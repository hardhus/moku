use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    widgets::ListState,
};

pub mod input;
pub mod list;
pub mod status;

use moku_core::MokuTheme;

use crate::model::Bookmark;

pub struct BookmarkUi;

impl BookmarkUi {
    pub fn draw(
        frame: &mut Frame,
        area: Rect,
        theme: &MokuTheme,
        items: &[Bookmark],
        state: &mut ListState,
        input_buffer: &str,
        input_mode: bool,
        search_mode: bool,
        mode_name: &str,
    ) {
        let show_input = input_mode || search_mode;

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(if show_input { 3 } else { 0 }),
                Constraint::Length(1),
            ])
            .split(area);

        list::draw_list(
            frame,
            chunks[0],
            theme,
            items,
            state,
            " 🔒 Bookmark Manager (Encrypted) ",
        );

        if show_input {
            input::draw_input(frame, chunks[1], theme, input_buffer, mode_name);
        }

        status::draw_status_bar(frame, chunks[2], theme, mode_name);
    }
}
