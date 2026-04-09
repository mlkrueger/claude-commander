use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Widget};
use std::path::PathBuf;

use crate::ui::theme::{self, Theme};

pub struct EditorState {
    pub file_path: PathBuf,
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll_offset: usize,
    pub modified: bool,
    pub message: Option<String>,
}

impl EditorState {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(&path)?;
        let lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
        let lines = if lines.is_empty() {
            vec![String::new()]
        } else {
            lines
        };

        Ok(Self {
            file_path: path,
            lines,
            cursor_row: 0,
            cursor_col: 0,
            scroll_offset: 0,
            modified: false,
            message: None,
        })
    }

    pub fn save(&mut self) -> anyhow::Result<()> {
        let content = self.lines.join("\n");
        // Add trailing newline if file had content
        let content = if content.is_empty() {
            content
        } else {
            content + "\n"
        };
        std::fs::write(&self.file_path, &content)?;
        self.modified = false;
        self.message = Some("Saved.".to_string());
        Ok(())
    }

    pub fn insert_char(&mut self, c: char) {
        if self.cursor_row >= self.lines.len() {
            self.lines.push(String::new());
        }
        let line = &mut self.lines[self.cursor_row];
        let byte_pos = char_to_byte_pos(line, self.cursor_col);
        line.insert(byte_pos, c);
        self.cursor_col += 1;
        self.modified = true;
        self.message = None;
    }

    pub fn insert_newline(&mut self) {
        if self.cursor_row >= self.lines.len() {
            self.lines.push(String::new());
        }
        let line = &mut self.lines[self.cursor_row];
        let byte_pos = char_to_byte_pos(line, self.cursor_col);
        let remainder = line[byte_pos..].to_string();
        line.truncate(byte_pos);
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.lines.insert(self.cursor_row, remainder);
        self.modified = true;
        self.message = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let byte_pos = char_to_byte_pos(line, self.cursor_col - 1);
            let end_pos = char_to_byte_pos(line, self.cursor_col);
            line.replace_range(byte_pos..end_pos, "");
            self.cursor_col -= 1;
            self.modified = true;
        } else if self.cursor_row > 0 {
            // Join with previous line
            let current_line = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&current_line);
            self.modified = true;
        }
        self.message = None;
    }

    pub fn delete(&mut self) {
        if self.cursor_row >= self.lines.len() {
            return;
        }
        let line_len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < line_len {
            let line = &mut self.lines[self.cursor_row];
            let byte_pos = char_to_byte_pos(line, self.cursor_col);
            let end_pos = char_to_byte_pos(line, self.cursor_col + 1);
            line.replace_range(byte_pos..end_pos, "");
            self.modified = true;
        } else if self.cursor_row + 1 < self.lines.len() {
            // Join with next line
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
            self.modified = true;
        }
        self.message = None;
    }

    pub fn move_up(&mut self) {
        if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.clamp_cursor_col();
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let line_len = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        if self.cursor_col < line_len {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    pub fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor_col = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
    }

    pub fn page_up(&mut self, page_size: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(page_size);
        self.clamp_cursor_col();
    }

    pub fn page_down(&mut self, page_size: usize) {
        self.cursor_row = (self.cursor_row + page_size).min(self.lines.len().saturating_sub(1));
        self.clamp_cursor_col();
    }

    fn clamp_cursor_col(&mut self) {
        let line_len = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        self.cursor_col = self.cursor_col.min(line_len);
    }

    pub fn ensure_cursor_visible(&mut self, visible_rows: usize) {
        if self.cursor_row < self.scroll_offset {
            self.scroll_offset = self.cursor_row;
        } else if self.cursor_row >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.cursor_row - visible_rows + 1;
        }
    }
}

fn char_to_byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

pub struct EditorPanel<'a> {
    state: &'a EditorState,
    theme: &'a Theme,
    tick: u64,
}

impl<'a> EditorPanel<'a> {
    pub fn new(state: &'a EditorState, theme: &'a Theme, tick: u64) -> Self {
        Self { state, theme, tick }
    }
}

impl Widget for EditorPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;
        let modified_marker = if self.state.modified { " [+]" } else { "" };
        let title = format!(
            " {}{modified_marker} ",
            self.state
                .file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(th.border_focused());

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        if th.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }

        let gutter_width = 4u16; // line number width
        let text_area_width = inner.width.saturating_sub(gutter_width + 1); // +1 for separator

        for (i, line) in self
            .state
            .lines
            .iter()
            .enumerate()
            .skip(self.state.scroll_offset)
            .take(inner.height as usize)
        {
            let screen_y = inner.y + (i - self.state.scroll_offset) as u16;
            if screen_y >= inner.bottom() {
                break;
            }

            // Line number gutter
            let line_num = format!("{:>3} ", i + 1);
            let gutter_style = Style::default().fg(Color::DarkGray);
            buf.set_string(inner.x, screen_y, &line_num, gutter_style);

            // Separator
            buf.set_string(inner.x + gutter_width, screen_y, "\u{2502}", gutter_style);

            // Plain text
            let text_x = inner.x + gutter_width + 1;
            let truncated: String = line.chars().take(text_area_width as usize).collect();
            buf.set_string(text_x, screen_y, &truncated, Style::default());

            // Cursor
            if i == self.state.cursor_row {
                let cursor_x = text_x + self.state.cursor_col as u16;
                if cursor_x < inner.right() && screen_y < inner.bottom() {
                    buf[(cursor_x, screen_y)]
                        .set_style(Style::default().fg(Color::Black).bg(Color::White));
                }
            }
        }
    }
}
