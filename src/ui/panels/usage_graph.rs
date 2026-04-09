use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::claude::rate_limit::RateLimitInfo;
use crate::ui::theme::{self, Theme};

pub struct UsageGraphPanel<'a> {
    rate_limit: Option<&'a RateLimitInfo>,
    theme: &'a Theme,
    tick: u64,
}

impl<'a> UsageGraphPanel<'a> {
    pub fn new(theme: &'a Theme, tick: u64) -> Self {
        Self {
            rate_limit: None,
            theme,
            tick,
        }
    }

    pub fn with_rate_limit(mut self, rate_limit: Option<&'a RateLimitInfo>) -> Self {
        self.rate_limit = rate_limit;
        self
    }
}

impl Widget for UsageGraphPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let th = self.theme;
        let block = Block::default()
            .title(" Usage ")
            .borders(Borders::ALL)
            .border_style(th.border());

        let inner = block.inner(area);
        Widget::render(block, area, buf);

        if th.is_rainbow() {
            theme::paint_rainbow_border(buf, area, self.tick);
        }

        if inner.width < 10 || inner.height < 3 {
            return;
        }

        let mut y = inner.y;
        let x = inner.x;
        let w = inner.width;

        if let Some(rl) = &self.rate_limit {
            // Current session (5-hour window)
            y = render_usage_section(
                buf,
                x,
                y,
                w,
                inner.bottom(),
                "Current session",
                rl.session_resets.as_deref(),
                rl.session_pct,
                th,
            );

            // Current week (7-day window)
            if y < inner.bottom() {
                y = render_usage_section(
                    buf,
                    x,
                    y,
                    w,
                    inner.bottom(),
                    "Current week",
                    rl.weekly_resets.as_deref(),
                    rl.weekly_pct,
                    th,
                );
            }

            // Session cost
            if y < inner.bottom() {
                if let Some(cost) = rl.cost_usd {
                    render_text(
                        buf,
                        x,
                        y,
                        w,
                        "Session cost",
                        Style::default().fg(th.text).add_modifier(Modifier::BOLD),
                    );
                    y += 1;
                    if y < inner.bottom() {
                        let cost_text = format!("${:.2}", cost);
                        render_text(
                            buf,
                            x + 1,
                            y,
                            w.saturating_sub(1),
                            &cost_text,
                            Style::default().fg(th.dim),
                        );
                    }
                }
            }
        } else {
            render_text(
                buf,
                x,
                y,
                w,
                "No usage data available",
                Style::default().fg(th.dim),
            );
        }
    }
}

/// Render a usage section (title, reset info, progress bar) and return the next y position.
fn render_usage_section(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    max_y: u16,
    title: &str,
    resets: Option<&str>,
    pct: Option<f64>,
    th: &Theme,
) -> u16 {
    if y >= max_y {
        return y;
    }

    // Title line
    render_text(
        buf,
        x,
        y,
        width,
        title,
        Style::default().fg(th.text).add_modifier(Modifier::BOLD),
    );
    let mut cy = y + 1;

    // Reset info line
    if cy < max_y {
        let reset_text = match resets {
            Some(r) => format!("Resets {r}"),
            None => String::new(),
        };
        if !reset_text.is_empty() {
            render_text(
                buf,
                x + 1,
                cy,
                width.saturating_sub(1),
                &reset_text,
                Style::default().fg(th.dim),
            );
        }
        cy += 1;
    }

    // Progress bar line
    if cy < max_y {
        render_progress_bar(buf, x + 1, cy, width.saturating_sub(2), pct, th);
        cy += 1;
    }

    // Blank line after section
    cy + 1
}

/// Render a horizontal progress bar with percentage label.
fn render_progress_bar(buf: &mut Buffer, x: u16, y: u16, width: u16, pct: Option<f64>, th: &Theme) {
    if width < 8 {
        return;
    }

    // Format the label that goes on the right side
    let label = match pct {
        Some(p) => format!("{:.0}% used", p),
        None => "ok".to_string(),
    };
    let label_width = label.len() as u16 + 1; // +1 for spacing

    let bar_width = width.saturating_sub(label_width);
    if bar_width < 4 {
        return;
    }

    let fill_pct = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let filled = ((fill_pct / 100.0) * bar_width as f64).round() as u16;

    let bar_color = if pct.is_none() {
        th.status_ok
    } else if fill_pct > 80.0 {
        th.status_err
    } else if fill_pct > 50.0 {
        th.status_warn
    } else {
        th.status_ok
    };

    let empty_color = Color::Rgb(60, 60, 60);

    // Draw the bar
    let mut cx = x;
    for i in 0..bar_width {
        if cx >= x + width {
            break;
        }
        let (ch, color) = if pct.is_none() {
            ('\u{2500}', th.dim) // ─ unknown state
        } else if i < filled {
            ('\u{2501}', bar_color) // ━
        } else {
            ('\u{2501}', empty_color) // ━
        };
        buf[(cx, y)]
            .set_char(ch)
            .set_style(Style::default().fg(color));
        cx += 1;
    }

    // Space before label
    if cx < x + width {
        cx += 1;
    }

    // Draw the label
    let label_color = if pct.is_none() {
        th.status_ok
    } else if fill_pct > 80.0 {
        th.status_err
    } else if fill_pct > 50.0 {
        th.status_warn
    } else {
        th.text
    };
    for ch in label.chars() {
        if cx < x + width {
            buf[(cx, y)]
                .set_char(ch)
                .set_style(Style::default().fg(label_color));
            cx += 1;
        }
    }
}

fn render_text(buf: &mut Buffer, x: u16, y: u16, max_width: u16, text: &str, style: Style) {
    for (i, ch) in text.chars().enumerate() {
        let cx = x + i as u16;
        if cx >= x + max_width {
            break;
        }
        buf[(cx, y)].set_char(ch).set_style(style);
    }
}
