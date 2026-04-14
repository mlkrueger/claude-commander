use std::collections::{HashMap, HashSet};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Row, Table, Widget};

use crate::session::{Session, SessionStatus};
use crate::ui::panels::driver_role_suffix;
use crate::ui::panels::session_tree::{TreeRow, build_session_tree};
use crate::ui::theme::{self, Theme};

pub struct SessionListPanel<'a> {
    sessions: &'a [Session],
    selected: usize,
    focused: bool,
    theme: &'a Theme,
    tick: u64,
    attachments: HashMap<usize, HashSet<usize>>,
}

impl<'a> SessionListPanel<'a> {
    pub fn new(
        sessions: &'a [Session],
        selected: usize,
        focused: bool,
        theme: &'a Theme,
        tick: u64,
    ) -> Self {
        Self {
            sessions,
            selected,
            focused,
            theme,
            tick,
            attachments: HashMap::new(),
        }
    }

    /// Phase 6 Task 7: pass the driver-attachment snapshot so the
    /// tree builder can group attached sessions under their driver.
    /// Caller snapshots `App.attachment_map` under the mutex and
    /// drops the lock before constructing the panel.
    pub fn with_attachments(mut self, attachments: HashMap<usize, HashSet<usize>>) -> Self {
        self.attachments = attachments;
        self
    }
}

impl Widget for SessionListPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;
        let border_style = if self.focused {
            th.border_focused()
        } else {
            th.border()
        };

        let block = Block::default()
            .title(" Sessions ")
            .borders(Borders::ALL)
            .border_style(border_style);

        let header = Row::new(vec!["#", "Name", "Directory", "Status", "Context", "Last"])
            .style(th.header())
            .bottom_margin(1);

        // Phase 6 Task 7: group sessions into driver-rooted subtrees
        // once per render. The attachments snapshot was already
        // cloned out from under the mutex by the caller.
        let tree = build_session_tree(self.sessions, &self.attachments);

        let rows: Vec<Row> = tree
            .iter()
            .map(|row| {
                // Unpack per-row tree info into the underlying slice
                // index + decoration decisions for the label cell.
                let (i, label_line) = match *row {
                    TreeRow::Driver { index } => {
                        let session = &self.sessions[index];
                        let suffix = driver_role_suffix(&session.role);
                        let line = Line::from(vec![
                            Span::styled(th.driver_icon(), Style::default().fg(th.driver_color())),
                            Span::styled(
                                session.label.clone(),
                                Style::default()
                                    .fg(th.driver_color())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(suffix, Style::default().fg(th.dim_color())),
                        ]);
                        (index, line)
                    }
                    TreeRow::Child {
                        parent_index,
                        index,
                        attached,
                    } => {
                        let session = &self.sessions[index];
                        let parent = &self.sessions[parent_index];
                        let icon = if attached {
                            th.attached_icon()
                        } else {
                            th.child_icon()
                        };
                        let line = Line::from(vec![
                            Span::raw("  "),
                            Span::styled(icon, Style::default().fg(th.dim_color())),
                            Span::raw(session.label.clone()),
                            Span::styled(
                                format!(" (parent: {})", parent.label),
                                Style::default().fg(th.dim_color()),
                            ),
                        ]);
                        (index, line)
                    }
                    TreeRow::Solo { index } => {
                        let session = &self.sessions[index];
                        (index, Line::raw(session.label.clone()))
                    }
                };
                let session = &self.sessions[i];
                let status_str = match &session.status {
                    SessionStatus::Running => "working".to_string(),
                    SessionStatus::WaitingForApproval(kind) => format!("\u{26a1} {kind}"),
                    SessionStatus::Idle => "idle".to_string(),
                    SessionStatus::Exited(code) => format!("exited ({code})"),
                };

                let status_style = match &session.status {
                    SessionStatus::Running => th.running(),
                    SessionStatus::WaitingForApproval(_) => th.attention(),
                    SessionStatus::Idle => th.idle(),
                    SessionStatus::Exited(_) => th.exited(),
                };

                let elapsed = session.elapsed_since_activity();
                let elapsed_str = if elapsed.as_secs() < 60 {
                    format!("{}s", elapsed.as_secs())
                } else if elapsed.as_secs() < 3600 {
                    format!("{}m", elapsed.as_secs() / 60)
                } else {
                    format!("{}h", elapsed.as_secs() / 3600)
                };

                let dir = session.working_dir.to_string_lossy().replace(
                    dirs::home_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                    "~",
                );

                let row_style = if i == self.selected {
                    th.selected()
                } else {
                    Style::default()
                };

                let context_str = match session.context_percent {
                    Some(pct) => format!("{pct:.0}%"),
                    None => "\u{2014}".to_string(),
                };

                let context_style = match session.context_percent {
                    Some(pct) if pct >= 80.0 => th.attention(),
                    Some(pct) if pct >= 50.0 => Style::default().fg(th.status_warn),
                    _ => Style::default(),
                };

                Row::new(vec![
                    Cell::from(format!("{}", session.id)),
                    Cell::from(label_line),
                    Cell::from(dir),
                    Cell::from(Span::styled(status_str, status_style)),
                    Cell::from(Span::styled(context_str, context_style)),
                    Cell::from(elapsed_str),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Length(3),
            Constraint::Length(15),
            Constraint::Min(20),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(6),
        ];

        let table = Table::new(rows, widths).header(header).block(block);

        Widget::render(table, area, buf);

        if th.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }
    }
}
