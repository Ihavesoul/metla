//! TUI shell: layout (header / body / status + modal overlay) and dispatch.

pub mod app;
pub mod theme;
mod views;

use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::Block;
use ratatui::Frame;

pub use app::{App, AppOptions};

pub fn render(f: &mut Frame, app: &mut App) {
    f.render_widget(
        Block::new().style(Style::new().bg(app.pal.bg).fg(app.pal.fg)),
        f.area(),
    );
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(f.area());

    views::header(f, app, chunks[0]);
    if app.arena.is_none() {
        views::scanning(f, app, chunks[1]);
    } else {
        match app.screen {
            app::Screen::Browse => views::browse(f, app, chunks[1]),
            app::Screen::Hogs => views::hogs(f, app, chunks[1]),
            app::Screen::Suggestions => views::suggestions(f, app, chunks[1]),
            app::Screen::Help => views::help(f, app, chunks[1]),
        }
    }
    views::status(f, app, chunks[2]);
    if !matches!(app.modal, app::Modal::None) {
        views::modal(f, app, f.area());
    }
}
