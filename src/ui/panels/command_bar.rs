use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::ui::theme::Theme;

/// Optional usage stats to display right-aligned on the command bar.
pub struct UsageStats {
    pub context_pct: Option<f64>,
    pub session_pct: Option<f64>,
    pub weekly_pct: Option<f64>,
}

pub enum CommandBarMode {
    Dashboard,
    FileTree,
    SessionView,
    SessionPicker,
    Editor,
    SendFile(Vec<String>), // session labels
    Setup,
}

pub struct CommandBar<'a> {
    mode: CommandBarMode,
    usage: Option<UsageStats>,
    theme: &'a Theme,
    /// Phase 7 Task 8: when the session currently shown is a driver
    /// with pending approvals, this holds the count so the status
    /// line can render `" ▲ <n> pending approval(s)"`.
    pending_approvals: Option<u32>,
}

impl<'a> CommandBar<'a> {
    pub fn new(mode: CommandBarMode, theme: &'a Theme) -> Self {
        Self {
            mode,
            usage: None,
            theme,
            pending_approvals: None,
        }
    }

    pub fn with_usage(mut self, usage: UsageStats) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Phase 7 Task 8: attach a pending-approval count to the bar.
    /// Only rendered when count > 0.
    pub fn with_pending_approvals(mut self, count: u32) -> Self {
        if count > 0 {
            self.pending_approvals = Some(count);
        }
        self
    }
}

impl Widget for CommandBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;

        let shortcuts = match self.mode {
            CommandBarMode::Dashboard => vec![
                ("n", "new"),
                ("Enter", "view"),
                ("a", "approve"),
                ("d", "deny"),
                ("t", "theme"),
                ("\u{2191}\u{2193}", "nav"),
                ("Tab", "files"),
                ("?", "help"),
                ("q", "quit"),
            ],
            CommandBarMode::FileTree => vec![
                ("\u{2191}\u{2193}", "navigate"),
                ("Enter/\u{2192}", "expand"),
                ("\u{2190}", "collapse"),
                ("e", "edit"),
                ("n", "new here"),
                ("t", "theme"),
                ("Tab", "sessions"),
                ("?", "help"),
                ("q", "quit"),
            ],
            CommandBarMode::SessionView => vec![
                ("Alt+D", "dashboard"),
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
                ("Alt+D", "close"),
                ("Arrows", "navigate"),
            ],
            CommandBarMode::Setup => vec![
                ("Enter/y", "fix"),
                ("\u{2191}\u{2193}", "nav"),
                ("Esc", "back"),
            ],
            CommandBarMode::SendFile(ref labels) => {
                return render_send_file(area, buf, labels, th);
            }
        };

        // Phase 7 Task 8: prefix the hint when this is a driver view
        // with pending approvals.
        let mut spans: Vec<Span> = if let Some(count) = self.pending_approvals {
            let n = count;
            let label = if n == 1 {
                " \u{25b2} 1 pending approval  ".to_string()
            } else {
                format!(" \u{25b2} {n} pending approvals  ")
            };
            vec![Span::styled(
                label,
                ratatui::style::Style::default()
                    .fg(th.driver_color())
                    .add_modifier(ratatui::style::Modifier::DIM),
            )]
        } else {
            Vec::new()
        };

        spans.extend(shortcuts.iter().enumerate().flat_map(|(i, (key, desc))| {
            let mut s = vec![
                Span::styled(format!("[{key}]"), th.shortcut_key()),
                Span::styled(format!(" {desc}"), th.shortcut_desc()),
            ];
            if i < shortcuts.len() - 1 {
                s.push(Span::raw("  "));
            }
            s
        }));

        let line = Line::from(spans);
        buf.set_line(area.x, area.y, &line, area.width);

        // Render right-aligned usage stats if present
        if let Some(usage) = &self.usage {
            let mut parts: Vec<Span> = Vec::new();

            if let Some(ctx) = usage.context_pct {
                let color = pct_color(ctx, th);
                parts.push(Span::styled("ctx:", th.shortcut_desc()));
                parts.push(Span::styled(
                    format!("{:.0}%", ctx),
                    ratatui::style::Style::default().fg(color),
                ));
            }
            if let Some(s) = usage.session_pct {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = pct_color(s, th);
                parts.push(Span::styled("5h:", th.shortcut_desc()));
                parts.push(Span::styled(
                    format!("{:.0}%", s),
                    ratatui::style::Style::default().fg(color),
                ));
            }
            if let Some(w) = usage.weekly_pct {
                if !parts.is_empty() {
                    parts.push(Span::raw("  "));
                }
                let color = pct_color(w, th);
                parts.push(Span::styled("7d:", th.shortcut_desc()));
                parts.push(Span::styled(
                    format!("{:.0}%", w),
                    ratatui::style::Style::default().fg(color),
                ));
            }

            if !parts.is_empty() {
                let usage_line = Line::from(parts);
                let usage_width = usage_line.width() as u16;
                if usage_width < area.width {
                    let x = area.x + area.width - usage_width;
                    buf.set_line(x, area.y, &usage_line, usage_width);
                }
            }
        }
    }
}

fn pct_color(pct: f64, th: &Theme) -> ratatui::style::Color {
    if pct > 80.0 {
        th.status_err
    } else if pct > 50.0 {
        th.status_warn
    } else {
        th.status_ok
    }
}

fn render_send_file(area: Rect, buf: &mut Buffer, labels: &[String], th: &Theme) {
    let mut spans = vec![Span::styled("Send file to: ", th.shortcut_desc())];
    for (i, label) in labels.iter().enumerate() {
        spans.push(Span::styled(format!("[{i}]"), th.shortcut_key()));
        spans.push(Span::styled(format!(" {label}"), th.shortcut_desc()));
        if i < labels.len() - 1 {
            spans.push(Span::raw("  "));
        }
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled("[Esc]", th.shortcut_key()));
    spans.push(Span::styled(" cancel", th.shortcut_desc()));

    let line = Line::from(spans);
    buf.set_line(area.x, area.y, &line, area.width);
}
