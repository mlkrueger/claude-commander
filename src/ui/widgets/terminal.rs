use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;

/// Renders a vt100::Screen into a ratatui buffer.
pub struct TerminalWidget<'a> {
    screen: &'a vt100::Screen,
    scroll_offset: usize,
    /// Fallback fg used when a cell reports Color::Reset (theme text color).
    cursor_fg: Color,
    /// Fallback bg used when a cell reports Color::Reset (theme background).
    cursor_bg: Color,
}

impl<'a> TerminalWidget<'a> {
    pub fn new(screen: &'a vt100::Screen, scroll_offset: usize) -> Self {
        Self {
            screen,
            scroll_offset,
            cursor_fg: Color::White,
            cursor_bg: Color::Black,
        }
    }
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (screen_rows, screen_cols) = self.screen.size();

        for y in 0..area.height {
            let screen_row = y as usize + self.scroll_offset;
            if screen_row >= screen_rows as usize {
                break;
            }

            for x in 0..area.width {
                if x >= screen_cols {
                    break;
                }

                let cell = self.screen.cell(screen_row as u16, x);
                let (ch, style) = match cell {
                    Some(cell) => {
                        let contents = cell.contents();
                        let ch = contents.chars().next().unwrap_or(' ');
                        let style = vt100_color_to_ratatui(cell);
                        (ch, style)
                    }
                    None => (' ', Style::default()),
                };

                let buf_x = area.x + x;
                let buf_y = area.y + y;
                if buf_x < area.right() && buf_y < area.bottom() {
                    buf[(buf_x, buf_y)].set_char(ch).set_style(style);
                }
            }
        }

        // Render cursor: resolve Reset colors to real theme values before
        // swapping fg/bg so the cursor is always visible against the background.
        let cursor = self.screen.cursor_position();
        let cursor_y = cursor.0;
        let cursor_x = cursor.1;
        if cursor_y >= self.scroll_offset as u16
            && (cursor_y - self.scroll_offset as u16) < area.height
            && cursor_x < area.width
        {
            let buf_x = area.x + cursor_x;
            let buf_y = area.y + cursor_y - self.scroll_offset as u16;
            if buf_x < area.right() && buf_y < area.bottom() {
                let existing = buf[(buf_x, buf_y)].style();
                let cell_fg = resolve_reset(existing.fg, self.cursor_fg);
                let cell_bg = resolve_reset(existing.bg, self.cursor_bg);
                // Swap fg/bg explicitly; clear REVERSED so we don't double-flip.
                let cursor_style = existing
                    .fg(cell_bg)
                    .bg(cell_fg)
                    .remove_modifier(Modifier::REVERSED);
                buf[(buf_x, buf_y)].set_style(cursor_style);
            }
        }
    }
}

/// Replace `Color::Reset` (or `None`) with `fallback`.
fn resolve_reset(color: Option<Color>, fallback: Color) -> Color {
    match color {
        None | Some(Color::Reset) => fallback,
        Some(c) => c,
    }
}

fn vt100_color_to_ratatui(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();

    style = style.fg(convert_color(cell.fgcolor()));
    style = style.bg(convert_color(cell.bgcolor()));

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
