use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::theme;

pub enum CommandBarMode {
    Dashboard,
    FileTree,
    SessionView,
    SessionPicker,
    Editor,
    SendFile(Vec<String>), // session labels
}

pub struct CommandBar {
    mode: CommandBarMode,
}

impl CommandBar {
    pub fn new(mode: CommandBarMode) -> Self {
        Self { mode }
    }
}

impl Widget for CommandBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let shortcuts = match self.mode {
            CommandBarMode::Dashboard => vec![
                ("n", "new"),
                ("Enter", "view"),
                ("a", "approve"),
                ("d", "deny"),
                ("c", "commit"),
                ("K", "kill"),
                ("x", "clear"),
                ("r", "rename"),
                ("\u{2191}\u{2193}", "nav"),
                ("Tab", "files"),
                ("q", "quit"),
            ],
            CommandBarMode::FileTree => vec![
                ("\u{2191}\u{2193}", "navigate"),
                ("Enter/\u{2192}", "expand"),
                ("\u{2190}", "collapse"),
                ("e", "edit file"),
                ("n", "new session here"),
                ("R", "refresh"),
                ("Tab", "sessions"),
                ("q", "quit"),
            ],
            CommandBarMode::SessionView => vec![
                ("Ctrl+O", "back to dashboard"),
                ("Alt+S", "switch session"),
                ("All keys", "forwarded to session"),
            ],
            CommandBarMode::SessionPicker => vec![
                ("\u{2191}\u{2193}/jk", "navigate"),
                ("Enter", "switch"),
                ("Esc", "cancel"),
            ],
            CommandBarMode::Editor => vec![
                ("Ctrl+S", "save"),
                ("Ctrl+P", "send to claude"),
                ("Ctrl+O", "close"),
                ("Arrows", "navigate"),
            ],
            CommandBarMode::SendFile(ref labels) => {
                let _shortcuts: Vec<(&str, &str)> = vec![("Esc", "cancel")];
                // We can't easily return borrowed strs from labels here,
                // so this is handled specially below
                return render_send_file(area, buf, labels);
            }
        };

        let spans: Vec<Span> = shortcuts
            .iter()
            .enumerate()
            .flat_map(|(i, (key, desc))| {
                let mut s = vec![
                    Span::styled(format!("[{key}]"), theme::SHORTCUT_KEY),
                    Span::styled(format!(" {desc}"), theme::SHORTCUT_DESC),
                ];
                if i < shortcuts.len() - 1 {
                    s.push(Span::raw("  "));
                }
                s
            })
            .collect();

        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);
    }
}

fn render_send_file(area: Rect, buf: &mut Buffer, labels: &[String]) {
    let mut spans = vec![Span::styled("Send file to: ", theme::SHORTCUT_DESC)];
    for (i, label) in labels.iter().enumerate() {
        spans.push(Span::styled(format!("[{i}]"), theme::SHORTCUT_KEY));
        spans.push(Span::styled(format!(" {label}"), theme::SHORTCUT_DESC));
        if i < labels.len() - 1 {
            spans.push(Span::raw("  "));
        }
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled("[Esc]", theme::SHORTCUT_KEY));
    spans.push(Span::styled(" cancel", theme::SHORTCUT_DESC));

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}
