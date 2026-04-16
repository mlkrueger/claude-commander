use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::ui::theme::{self, Theme};

pub struct SessionDetailPanel<'a> {
    /// Shared vt100 parser for the selected session. Locked during render.
    parser: Option<Arc<Mutex<vt100::Parser>>>,
    theme: &'a Theme,
    tick: u64,
}

impl<'a> SessionDetailPanel<'a> {
    pub fn new(theme: &'a Theme, tick: u64) -> Self {
        Self {
            parser: None,
            theme,
            tick,
        }
    }

    pub fn with_parser(mut self, parser: Option<Arc<Mutex<vt100::Parser>>>) -> Self {
        self.parser = parser;
        self
    }
}

impl Widget for SessionDetailPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;
        let block = Block::default()
            .title(" Detail ")
            .borders(Borders::ALL)
            .border_style(th.border());

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        if th.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }

        let Some(parser_arc) = self.parser else {
            let style = Style::default().fg(th.dim);
            render_str(
                buf,
                inner.x,
                inner.y,
                inner.width,
                "No session selected",
                style,
            );
            return;
        };

        let mut parser = parser_arc.lock().unwrap_or_else(|p| p.into_inner());
        // Always show the live terminal bottom, not any leftover scrollback
        // from a previous session-view scroll.
        parser.screen_mut().set_scrollback(0);
        let screen = parser.screen();
        let (screen_rows, screen_cols) = screen.size();

        if inner.height == 0 || inner.width == 0 || screen_rows == 0 {
            return;
        }

        // Find the last non-blank row in the screen so we don't waste
        // space rendering empty terminal lines at the bottom.
        let last_content_row = last_nonempty_row(screen, screen_rows);

        // Show the bottom `inner.height` rows of content, anchored so
        // the most recent output is at the bottom of the panel.
        let panel_rows = inner.height as usize;
        let end_row = (last_content_row + 1).min(screen_rows as usize);
        let start_row = end_row.saturating_sub(panel_rows);

        for (panel_y, screen_y) in (start_row..end_row).enumerate() {
            let buf_y = inner.y + panel_y as u16;
            if buf_y >= inner.bottom() {
                break;
            }

            for screen_x in 0..screen_cols.min(inner.width) {
                let buf_x = inner.x + screen_x;
                if buf_x >= inner.right() {
                    break;
                }

                let Some(cell) = screen.cell(screen_y as u16, screen_x) else {
                    continue;
                };

                let ch = cell.contents().chars().next().unwrap_or(' ');
                let style = cell_style(cell);
                buf[(buf_x, buf_y)].set_char(ch).set_style(style);
            }
        }
    }
}

/// Return the index of the last row that contains at least one non-space char.
fn last_nonempty_row(screen: &vt100::Screen, screen_rows: u16) -> usize {
    for row in (0..screen_rows as usize).rev() {
        for col in 0..screen.size().1 {
            if let Some(cell) = screen.cell(row as u16, col) {
                let ch = cell.contents().chars().next().unwrap_or(' ');
                if ch != ' ' {
                    return row;
                }
            }
        }
    }
    0
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(convert_color(cell.fgcolor()))
        .bg(convert_color(cell.bgcolor()));
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(idx) => Color::Indexed(idx),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn render_str(buf: &mut Buffer, x: u16, y: u16, max_width: u16, text: &str, style: Style) {
    for (i, ch) in text.chars().enumerate() {
        let cx = x + i as u16;
        if cx >= x + max_width {
            break;
        }
        buf[(cx, y)].set_char(ch).set_style(style);
    }
}
