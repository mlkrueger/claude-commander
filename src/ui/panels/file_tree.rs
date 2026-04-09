use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};
use std::path::PathBuf;

use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::ui::theme::{self, Theme};

pub struct FileTreePanel<'a> {
    tree: &'a FileTree,
    focused: bool,
    session_dirs: &'a [PathBuf],
    scroll_offset: usize,
    git_status: Option<&'a GitStatusMap>,
    theme: &'a Theme,
    tick: u64,
}

impl<'a> FileTreePanel<'a> {
    pub fn new(tree: &'a FileTree, focused: bool, session_dirs: &'a [PathBuf], theme: &'a Theme, tick: u64) -> Self {
        Self {
            tree,
            focused,
            session_dirs,
            scroll_offset: 0,
            git_status: None,
            theme,
            tick,
        }
    }

    pub fn with_scroll(mut self, offset: usize) -> Self {
        self.scroll_offset = offset;
        self
    }

    pub fn with_git_status(mut self, git_status: Option<&'a GitStatusMap>) -> Self {
        self.git_status = git_status;
        self
    }
}

impl Widget for FileTreePanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;
        let border_style = if self.focused {
            th.border_focused()
        } else {
            th.border()
        };

        let title = format!(
            " {} ",
            self.tree.root.path.to_string_lossy().replace(
                &dirs::home_dir()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                "~"
            )
        );

        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(border_style);

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        if th.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }

        let visible = self.tree.visible_nodes();
        let max_lines = inner.height as usize;

        for (i, (path, depth)) in visible
            .iter()
            .skip(self.scroll_offset)
            .enumerate()
            .take(max_lines)
        {
            let global_idx = i + self.scroll_offset;
            let is_selected = global_idx == self.tree.selected;
            let is_dir = path.is_dir();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());

            let has_session = self.session_dirs.iter().any(|d| d == path);

            let indent = "  ".repeat(*depth);
            let icon = if is_dir {
                let next_depth = visible.get(global_idx + 1).map(|(_, d)| *d).unwrap_or(0);
                if next_depth > *depth {
                    "\u{25be} " // ▾
                } else {
                    "\u{25b8} " // ▸
                }
            } else {
                "  "
            };

            let session_marker = if has_session { "\u{25cf} " } else { "" }; // ●

            // Determine git status color
            let git_file_status = self.git_status.and_then(|gs| {
                if is_dir {
                    git::dir_has_changes(path, gs)
                } else {
                    gs.get(path).copied()
                }
            });

            let git_indicator = git_file_status
                .map(|s| format!(" {}", s.indicator()))
                .unwrap_or_default();

            // Color: selection overrides, then git status, then default
            let base_style = if is_selected {
                th.selected()
            } else if let Some(gs) = git_file_status {
                if is_dir {
                    Style::default().fg(gs.color()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(gs.color())
                }
            } else if is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };

            let line_str = format!("{indent}{icon}{session_marker}{name}{git_indicator}");
            let truncated = if line_str.len() > inner.width as usize {
                &line_str[..inner.width as usize]
            } else {
                &line_str
            };

            let y = inner.y + i as u16;
            if y < inner.bottom() {
                buf.set_string(inner.x, y, truncated, base_style);
                if is_selected {
                    for x in (inner.x + truncated.len() as u16)..inner.right() {
                        buf[(x, y)].set_style(th.selected());
                    }
                }
            }
        }
    }
}
