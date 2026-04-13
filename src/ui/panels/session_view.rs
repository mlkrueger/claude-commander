use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Widget};

use crate::session::{Session, SessionRole, SpawnPolicy, lock_parser};
use crate::ui::theme::{self, Theme};
use crate::ui::widgets::terminal::TerminalWidget;

pub struct SessionViewPanel<'a> {
    session: &'a Session,
    scroll_offset: usize,
    theme: &'a Theme,
    tick: u64,
}

impl<'a> SessionViewPanel<'a> {
    pub fn new(session: &'a Session, theme: &'a Theme, tick: u64) -> Self {
        Self {
            session,
            scroll_offset: 0,
            theme,
            tick,
        }
    }

    pub fn with_scroll(mut self, offset: usize) -> Self {
        self.scroll_offset = offset;
        self
    }
}

impl Widget for SessionViewPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Phase 6 Task 7: append a `[driver · …]` suffix to the title
        // bar when viewing a driver so the user always knows which
        // role they're interacting with. Non-driver sessions get the
        // original title unchanged.
        let driver_suffix = match &self.session.role {
            SessionRole::Driver {
                spawn_budget,
                spawn_policy,
            } => match spawn_policy {
                SpawnPolicy::Budget => format!(" [driver · budget {spawn_budget}]"),
                SpawnPolicy::Ask => " [driver · ask]".to_string(),
                SpawnPolicy::Trust => " [driver · trust]".to_string(),
            },
            SessionRole::Solo => String::new(),
        };
        let title = format!(
            " {} \u{2014} {}{} ",
            self.session.label,
            self.session.working_dir.display(),
            driver_suffix
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(self.theme.border_focused());

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        if self.theme.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }

        let mut parser = lock_parser(&self.session.parser);
        parser.screen_mut().set_scrollback(self.scroll_offset);
        let screen = parser.screen();
        let terminal = TerminalWidget::new(screen, 0);
        Widget::render(terminal, inner, buf);
    }
}
