use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Available color themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeName {
    Default,
    Green,
    Tron,
    Amber,
    Ocean,
    HotPink,
    Rainbow,
}

impl ThemeName {
    pub const ALL: &[ThemeName] = &[
        ThemeName::Default,
        ThemeName::Green,
        ThemeName::Tron,
        ThemeName::Amber,
        ThemeName::Ocean,
        ThemeName::HotPink,
        ThemeName::Rainbow,
    ];

    pub fn next(self) -> ThemeName {
        let idx = ThemeName::ALL.iter().position(|&t| t == self).unwrap_or(0);
        ThemeName::ALL[(idx + 1) % ThemeName::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeName::Default => "Default",
            ThemeName::Green => "Green Terminal",
            ThemeName::Tron => "Tron",
            ThemeName::Amber => "Amber",
            ThemeName::Ocean => "Ocean",
            ThemeName::HotPink => "Hot Pink",
            ThemeName::Rainbow => "Rainbow",
        }
    }
}

/// Runtime theme that provides all styles used across the UI.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: ThemeName,
    /// Primary accent color (focused borders, shortcut keys, highlights)
    pub accent: Color,
    /// Secondary/dimmed color (unfocused borders, descriptions)
    pub dim: Color,
    /// Text color for headers and labels
    pub text: Color,
    /// Selected item background
    pub selected_bg: Color,
    /// Selected item foreground
    pub selected_fg: Color,
    /// Running/OK status
    pub status_ok: Color,
    /// Warning/attention status
    pub status_warn: Color,
    /// Error/exited status
    pub status_err: Color,
    /// Idle/muted status
    pub status_idle: Color,
}

impl Theme {
    pub fn new(name: ThemeName) -> Self {
        match name {
            ThemeName::Default => Self {
                name,
                accent: Color::Cyan,
                dim: Color::DarkGray,
                text: Color::White,
                selected_bg: Color::Cyan,
                selected_fg: Color::Black,
                status_ok: Color::Green,
                status_warn: Color::Yellow,
                status_err: Color::Red,
                status_idle: Color::DarkGray,
            },
            ThemeName::Green => Self {
                name,
                accent: Color::Green,
                dim: Color::Rgb(0, 80, 0),
                text: Color::Rgb(0, 255, 0),
                selected_bg: Color::Green,
                selected_fg: Color::Black,
                status_ok: Color::Rgb(0, 255, 0),
                status_warn: Color::Rgb(180, 255, 0),
                status_err: Color::Rgb(255, 80, 0),
                status_idle: Color::Rgb(0, 80, 0),
            },
            ThemeName::Tron => Self {
                name,
                accent: Color::Rgb(0, 255, 255),   // bright cyan
                dim: Color::Rgb(0, 60, 80),
                text: Color::Rgb(200, 240, 255),
                selected_bg: Color::Rgb(255, 160, 0), // tron orange
                selected_fg: Color::Black,
                status_ok: Color::Rgb(0, 255, 255),
                status_warn: Color::Rgb(255, 160, 0),
                status_err: Color::Rgb(255, 50, 50),
                status_idle: Color::Rgb(0, 60, 80),
            },
            ThemeName::Amber => Self {
                name,
                accent: Color::Rgb(255, 176, 0),
                dim: Color::Rgb(120, 80, 0),
                text: Color::Rgb(255, 200, 80),
                selected_bg: Color::Rgb(255, 176, 0),
                selected_fg: Color::Black,
                status_ok: Color::Rgb(255, 200, 80),
                status_warn: Color::Rgb(255, 255, 100),
                status_err: Color::Rgb(255, 60, 0),
                status_idle: Color::Rgb(120, 80, 0),
            },
            ThemeName::Ocean => Self {
                name,
                accent: Color::Rgb(60, 140, 255),
                dim: Color::Rgb(30, 50, 100),
                text: Color::Rgb(180, 210, 255),
                selected_bg: Color::Rgb(60, 140, 255),
                selected_fg: Color::White,
                status_ok: Color::Rgb(80, 200, 255),
                status_warn: Color::Rgb(255, 220, 80),
                status_err: Color::Rgb(255, 80, 80),
                status_idle: Color::Rgb(30, 50, 100),
            },
            ThemeName::HotPink => Self {
                name,
                accent: Color::Rgb(255, 20, 147),
                dim: Color::Rgb(100, 10, 60),
                text: Color::Rgb(255, 180, 220),
                selected_bg: Color::Rgb(255, 20, 147),
                selected_fg: Color::White,
                status_ok: Color::Rgb(255, 100, 200),
                status_warn: Color::Rgb(255, 200, 100),
                status_err: Color::Rgb(255, 50, 50),
                status_idle: Color::Rgb(100, 10, 60),
            },
            ThemeName::Rainbow => Self {
                name,
                accent: Color::White,
                dim: Color::DarkGray,
                text: Color::White,
                selected_bg: Color::Magenta,
                selected_fg: Color::White,
                status_ok: Color::Green,
                status_warn: Color::Yellow,
                status_err: Color::Red,
                status_idle: Color::DarkGray,
            },
        }
    }

    // --- Style accessors ---

    pub fn header(&self) -> Style {
        Style::new().fg(self.text).add_modifier(Modifier::BOLD)
    }

    pub fn selected(&self) -> Style {
        Style::new().fg(self.selected_fg).bg(self.selected_bg)
    }

    pub fn attention(&self) -> Style {
        Style::new().fg(self.status_warn).add_modifier(Modifier::BOLD)
    }

    pub fn running(&self) -> Style {
        Style::new().fg(self.status_ok)
    }

    pub fn idle(&self) -> Style {
        Style::new().fg(self.status_idle)
    }

    pub fn exited(&self) -> Style {
        Style::new().fg(self.status_err)
    }

    pub fn border(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn border_focused(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn shortcut_key(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn shortcut_desc(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn is_rainbow(&self) -> bool {
        self.name == ThemeName::Rainbow
    }
}

// --- Rainbow border rendering ---

/// Convert HSL (h: 0..360, s: 0..1, l: 0..1) to RGB.
fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h2 = h / 60.0;
    let x = c * (1.0 - (h2 % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match h2 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (
        ((r1 + m) * 255.0) as u8,
        ((g1 + m) * 255.0) as u8,
        ((b1 + m) * 255.0) as u8,
    )
}

/// Rainbow color for a given position along the border perimeter.
fn rainbow_color(position: usize, total: usize, tick: u64) -> Color {
    let hue = ((position as f64 / total as f64) * 360.0
        + (tick as f64 * 8.0)) // speed of rotation
        % 360.0;
    let (r, g, b) = hsl_to_rgb(hue, 1.0, 0.55);
    Color::Rgb(r, g, b)
}

/// Paint rainbow colors over the border cells of a rendered Block.
/// Call this AFTER rendering the Block widget so the border chars are already in place.
pub fn paint_rainbow_border(buf: &mut Buffer, area: Rect, tick: u64) {
    if area.width < 2 || area.height < 2 {
        return;
    }

    let w = area.width as usize;
    let h = area.height as usize;
    // Perimeter: top + right (minus corner) + bottom + left (minus corner)
    let perimeter = 2 * (w + h) - 4;

    let mut pos: usize = 0;

    // Top edge: left to right
    for x in 0..w {
        let color = rainbow_color(pos, perimeter, tick);
        buf[(area.x + x as u16, area.y)].set_style(Style::new().fg(color));
        pos += 1;
    }

    // Right edge: top+1 to bottom-1
    for y in 1..h.saturating_sub(1) {
        let color = rainbow_color(pos, perimeter, tick);
        buf[(area.x + area.width - 1, area.y + y as u16)].set_style(Style::new().fg(color));
        pos += 1;
    }

    // Bottom edge: right to left
    for x in (0..w).rev() {
        let color = rainbow_color(pos, perimeter, tick);
        buf[(area.x + x as u16, area.y + area.height - 1)].set_style(Style::new().fg(color));
        pos += 1;
    }

    // Left edge: bottom-1 to top+1
    for y in (1..h.saturating_sub(1)).rev() {
        let color = rainbow_color(pos, perimeter, tick);
        buf[(area.x, area.y + y as u16)].set_style(Style::new().fg(color));
        pos += 1;
    }
}
