use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Widget};

use crate::pty::session::Session;
use crate::ui::theme;
use crate::ui::widgets::terminal::TerminalWidget;

pub struct SessionViewPanel<'a> {
    session: &'a Session,
}

impl<'a> SessionViewPanel<'a> {
    pub fn new(session: &'a Session) -> Self {
        Self { session }
    }
}

impl Widget for SessionViewPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title = format!(
            " {} — {} ",
            self.session.label,
            self.session.working_dir.display()
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(theme::BORDER_FOCUSED);

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        let parser = self.session.parser.lock().unwrap();
        let screen = parser.screen();
        let terminal = TerminalWidget::new(screen, 0);
        Widget::render(terminal, inner, buf);
    }
}
