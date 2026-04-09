use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct AppLayout {
    pub file_tree: Rect,
    pub main: Rect,
    pub command_bar: Rect,
}

impl AppLayout {
    pub fn new(area: Rect) -> Self {
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // content area
                Constraint::Length(1), // command bar
            ])
            .split(area);

        let horiz = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(30), // file tree
                Constraint::Min(40),    // main area
            ])
            .split(vert[0]);

        Self {
            file_tree: horiz[0],
            main: horiz[1],
            command_bar: vert[1],
        }
    }

    /// Layout without file tree (for session view full-screen)
    pub fn session_view(area: Rect) -> (Rect, Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(1)])
            .split(area);
        (chunks[0], chunks[1])
    }
}
