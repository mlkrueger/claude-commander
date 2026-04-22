use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Widget};

use crate::claude::rate_limit::RateLimitInfo;
use crate::ui::theme::{self, Theme};

/// Number of discrete blocks in the usage bar. Each block = 5%.
const BLOCKS: u16 = 20;
/// Filled block character.
const FILLED: char = '\u{2588}'; // █
/// Empty block character.
const EMPTY: char = '\u{2591}'; // ░

/// Color zone boundaries (block index, 0-based).
/// Blocks 0–15  (0–80%):   status_ok (green)
/// Blocks 16–17 (80–90%):  status_orange
/// Blocks 18–19 (90–100%): status_err (red)
fn block_color(block_idx: u16, th: &Theme) -> Color {
    if block_idx >= 18 {
        th.status_err
    } else if block_idx >= 16 {
        th.status_orange
    } else {
        th.status_ok
    }
}

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

        if inner.width < 10 || inner.height < 2 {
            return;
        }

        let mut y = inner.y;

        if let Some(rl) = &self.rate_limit {
            // Current session (5-hour window)
            y = render_usage_row(
                buf,
                inner.x,
                y,
                inner.width,
                inner.bottom(),
                "Sess",
                rl.session_resets.as_deref(),
                rl.session_pct,
                th,
            );

            // Current week (7-day window)
            if y < inner.bottom() {
                y = render_usage_row(
                    buf,
                    inner.x,
                    y,
                    inner.width,
                    inner.bottom(),
                    "Week",
                    rl.weekly_resets.as_deref(),
                    rl.weekly_pct,
                    th,
                );
            }

            // Session cost
            if y < inner.bottom()
                && let Some(cost) = rl.cost_usd
            {
                render_text(
                    buf,
                    inner.x,
                    y,
                    inner.width,
                    &format!("${cost:.2}"),
                    Style::default().fg(th.dim),
                );
            }
        } else {
            render_text(
                buf,
                inner.x,
                y,
                inner.width,
                "No data",
                Style::default().fg(th.dim),
            );
        }
    }
}

/// Render one usage section: a label + 20-block bar on one line,
/// then a reset/percentage info line below it.
/// Returns the y position after this section (with a blank separator).
#[allow(clippy::too_many_arguments)]
fn render_usage_row(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    max_y: u16,
    label: &str,
    resets: Option<&str>,
    pct: Option<f64>,
    th: &Theme,
) -> u16 {
    if y >= max_y {
        return y;
    }

    let fill_pct = pct.unwrap_or(0.0).clamp(0.0, 100.0);
    let empty_color = Color::Rgb(50, 50, 50);

    // --- Row 1: label + blocks ---
    //
    // Pre-budget layout so the row never overflows regardless of pct value:
    //   label_width  = 5  ("Sess ")
    //   pct_width    = 4  ("100%" worst case — no leading space, fixed width)
    //   bar_blocks   = min(BLOCKS, width - label_width - pct_width)
    //
    // This keeps the row exactly `width` chars at any percentage.
    const LABEL_W: u16 = 5;
    const PCT_W: u16 = 4; // "100%" is the widest value
    let bar_blocks = BLOCKS.min(width.saturating_sub(LABEL_W + PCT_W));

    // Recompute filled count against the actual bar width we'll display.
    let filled_blocks = if bar_blocks > 0 {
        ((fill_pct / 100.0) * bar_blocks as f64).round() as u16
    } else {
        0
    };

    let mut cx = x;

    // Label (e.g. "Sess ")
    let label_display = format!("{label:<4} ");
    for ch in label_display.chars() {
        if cx < x + width {
            buf[(cx, y)]
                .set_char(ch)
                .set_style(Style::default().fg(th.dim).add_modifier(Modifier::BOLD));
            cx += 1;
        }
    }

    // Block bar
    for i in 0..bar_blocks {
        if cx >= x + width {
            break;
        }
        // Map this block's index into the full 0–BLOCKS range for colour lookup.
        let zone_idx = if bar_blocks > 0 {
            i * BLOCKS / bar_blocks
        } else {
            i
        };
        let (ch, color) = if pct.is_none() {
            ('\u{2500}', th.dim)
        } else if i < filled_blocks {
            (FILLED, block_color(zone_idx, th))
        } else {
            (EMPTY, empty_color)
        };
        buf[(cx, y)]
            .set_char(ch)
            .set_style(Style::default().fg(color));
        cx += 1;
    }

    // Percentage label — always exactly PCT_W chars wide, right-aligned.
    let pct_str = match pct {
        Some(p) => format!("{p:>3.0}%"), // e.g. "  0%", " 62%", "100%"
        None => "  — ".to_string(),
    };
    let pct_color = match pct {
        Some(p) if p >= 90.0 => th.status_err,
        Some(p) if p >= 80.0 => th.status_orange,
        _ => th.dim,
    };
    for ch in pct_str.chars() {
        if cx < x + width {
            buf[(cx, y)]
                .set_char(ch)
                .set_style(Style::default().fg(pct_color));
            cx += 1;
        }
    }

    // --- Row 2: reset time ---
    let mut cy = y + 1;
    if cy < max_y {
        let info = match resets {
            Some(r) => format!("  resets {r}"),
            None => String::new(),
        };
        if !info.is_empty() {
            render_text(buf, x, cy, width, &info, Style::default().fg(th.dim));
        }
        cy += 1;
    }

    // Blank separator
    cy + 1
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
