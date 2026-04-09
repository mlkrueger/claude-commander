use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, Widget};

use crate::pty::session::{Session, SessionStatus};
use crate::ui::theme::{self, Theme};

pub struct SessionListPanel<'a> {
    sessions: &'a [Session],
    selected: usize,
    focused: bool,
    theme: &'a Theme,
    tick: u64,
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
        }
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

        let rows: Vec<Row> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, session)| {
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
                    Cell::from(session.label.clone()),
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
