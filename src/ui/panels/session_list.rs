use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Row, Table, Widget};

use crate::pty::session::{Session, SessionStatus};
use crate::ui::theme;

pub struct SessionListPanel<'a> {
    sessions: &'a [Session],
    selected: usize,
    focused: bool,
}

impl<'a> SessionListPanel<'a> {
    pub fn new(sessions: &'a [Session], selected: usize, focused: bool) -> Self {
        Self {
            sessions,
            selected,
            focused,
        }
    }
}

impl Widget for SessionListPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let border_style = if self.focused {
            theme::BORDER_FOCUSED
        } else {
            theme::BORDER
        };

        let block = Block::default()
            .title(" Sessions ")
            .borders(Borders::ALL)
            .border_style(border_style);

        let header = Row::new(vec!["#", "Name", "Directory", "Status", "Last"])
            .style(theme::HEADER)
            .bottom_margin(1);

        let rows: Vec<Row> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, session)| {
                let status_str = match &session.status {
                    SessionStatus::Running => "working".to_string(),
                    SessionStatus::WaitingForApproval(kind) => format!("⚡ {kind}"),
                    SessionStatus::Idle => "idle".to_string(),
                    SessionStatus::Exited(code) => format!("exited ({code})"),
                };

                let status_style = match &session.status {
                    SessionStatus::Running => theme::RUNNING,
                    SessionStatus::WaitingForApproval(_) => theme::ATTENTION,
                    SessionStatus::Idle => theme::IDLE,
                    SessionStatus::Exited(_) => theme::EXITED,
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
                    theme::SELECTED
                } else {
                    Style::default()
                };

                Row::new(vec![
                    Cell::from(format!("{}", session.id)),
                    Cell::from(session.label.clone()),
                    Cell::from(dir),
                    Cell::from(Span::styled(status_str, status_style)),
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
            Constraint::Length(6),
        ];

        let table = Table::new(rows, widths).header(header).block(block);

        Widget::render(table, area, buf);
    }
}
