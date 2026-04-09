use ratatui::style::{Color, Modifier, Style};

pub const HEADER: Style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);

pub const SELECTED: Style = Style::new().fg(Color::Black).bg(Color::Cyan);

pub const ATTENTION: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

pub const RUNNING: Style = Style::new().fg(Color::Green);

pub const IDLE: Style = Style::new().fg(Color::DarkGray);

pub const EXITED: Style = Style::new().fg(Color::Red);

pub const BORDER: Style = Style::new().fg(Color::DarkGray);

pub const BORDER_FOCUSED: Style = Style::new().fg(Color::Cyan);

pub const SHORTCUT_KEY: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

pub const SHORTCUT_DESC: Style = Style::new().fg(Color::DarkGray);

#[allow(dead_code)]
pub const STATUS_BAR: Style = Style::new().fg(Color::White).bg(Color::DarkGray);
