use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Cell, Clear, Row, Table, Widget};

use crate::pty::session::{Session, SessionStatus};
use crate::ui::theme::Theme;

pub struct SessionPickerPanel<'a> {
    sessions: &'a [Session],
    selected: usize,
    theme: &'a Theme,
}

impl<'a> SessionPickerPanel<'a> {
    pub fn new(sessions: &'a [Session], selected: usize, theme: &'a Theme) -> Self {
        Self {
            sessions,
            selected,
            theme,
        }
    }
}

impl Widget for SessionPickerPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Size the popup: width = 60% of area, height = number of sessions + 4 (border + header + margin)
        let popup_width = (area.width * 60 / 100).max(40).min(area.width);
        let popup_height = (self.sessions.len() as u16 + 4).min(area.height);

        // Center it
        let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let popup_area = Rect::new(x, y, popup_width, popup_height);

        // Clear the area behind the popup
        Clear.render(popup_area, buf);

        let block = Block::default()
            .title(" Switch Session (C-S-s) ")
            .borders(Borders::ALL)
            .border_style(self.theme.border_focused());

        let header = Row::new(vec!["#", "Name", "Directory", "Status"])
            .style(self.theme.header())
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
                    SessionStatus::Running => self.theme.running(),
                    SessionStatus::WaitingForApproval(_) => self.theme.attention(),
                    SessionStatus::Idle => self.theme.idle(),
                    SessionStatus::Exited(_) => self.theme.exited(),
                };

                let dir = session.working_dir.to_string_lossy().replace(
                    dirs::home_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .as_ref(),
                    "~",
                );

                let row_style = if i == self.selected {
                    self.theme.selected()
                } else {
                    Style::default()
                };

                Row::new(vec![
                    Cell::from(format!("{}", session.id)),
                    Cell::from(session.label.clone()),
                    Cell::from(dir),
                    Cell::from(Span::styled(status_str, status_style)),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Length(3),
            Constraint::Length(15),
            Constraint::Min(10),
            Constraint::Length(15),
        ];

        let table = Table::new(rows, widths).header(header).block(block);

        Widget::render(table, popup_area, buf);
    }
}
