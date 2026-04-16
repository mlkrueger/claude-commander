use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Height of the bottom row (usage panel + session detail panel).
/// Both panels share this height so their top edges align.
const BOTTOM_HEIGHT: u16 = 9;

pub struct AppLayout {
    pub file_tree: Rect,
    pub usage_graph: Rect,
    pub main: Rect,
    pub session_detail: Rect,
    pub command_bar: Rect,
}

impl AppLayout {
    pub fn new(area: Rect) -> Self {
        // Outer: content area on top, command bar on bottom.
        let vert = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),    // content area
                Constraint::Length(1), // command bar
            ])
            .split(area);

        // Content area: left column | right column.
        let horiz = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(30), // left column (file tree + usage)
                Constraint::Min(40),    // right column (sessions + detail)
            ])
            .split(vert[0]);

        // Left column: file tree on top, usage panel on bottom.
        let left_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(BOTTOM_HEIGHT)])
            .split(horiz[0]);

        // Right column: session list on top, session detail on bottom.
        let right_split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(BOTTOM_HEIGHT)])
            .split(horiz[1]);

        Self {
            file_tree: left_split[0],
            usage_graph: left_split[1],
            main: right_split[0],
            session_detail: right_split[1],
            command_bar: vert[1],
        }
    }

    /// Layout without file tree (for session view full-screen).
    pub fn session_view(area: Rect) -> (Rect, Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(1)])
            .split(area);
        (chunks[0], chunks[1])
    }
}
