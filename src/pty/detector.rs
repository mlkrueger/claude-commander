use std::sync::LazyLock;

use regex::Regex;

#[derive(Debug, Clone)]
pub enum PromptKind {
    AllowOnce,
    YesNo,
    PressEnter,
    AcceptEdits,
    Unknown,
}

struct PatternEntry {
    regex: &'static LazyLock<Regex>,
    kind: PromptKind,
}

static RE_ALLOW_ONCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Allow once|Allow always|allow tool|Don't allow)")
        .expect("RE_ALLOW_ONCE is a valid regex")
});

// Keep this set to strings that actually frame a prompt dialog.
// Bare words like `approve` / `deny` were here once and false-matched
// on any prose about permissions — e.g. a Claude response containing
// "allow/ask/deny" or "deny wins globally" tripped the detector and
// pinned the session to WaitingForApproval even though no dialog was
// open. If Claude Code adds a new prompt phrasing, add the full
// phrase here rather than a bare verb.
static RE_YES_NO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(Do you want to|Yes/No|Y/n|y/N|\[Y/n\]|\[y/N\])")
        .expect("RE_YES_NO is a valid regex")
});

static RE_PRESS_ENTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(press enter|Press Enter)").expect("RE_PRESS_ENTER is a valid regex")
});

static RE_ACCEPT_EDITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(accept edits)").expect("RE_ACCEPT_EDITS is a valid regex"));

// `⎕` is a Claude Code dialog glyph. Bare `permission` and `Reject`
// used to live here and false-matched on any Claude response talking
// about permissions or rejection — so they're gone. Add full dialog
// phrases if new prompt variants show up.
static RE_UNKNOWN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"⎕").expect("RE_UNKNOWN is a valid regex"));

pub struct PromptDetector {
    patterns: Vec<PatternEntry>,
}

impl Default for PromptDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptDetector {
    pub fn new() -> Self {
        let patterns = vec![
            PatternEntry {
                regex: &RE_ALLOW_ONCE,
                kind: PromptKind::AllowOnce,
            },
            PatternEntry {
                regex: &RE_YES_NO,
                kind: PromptKind::YesNo,
            },
            PatternEntry {
                regex: &RE_PRESS_ENTER,
                kind: PromptKind::PressEnter,
            },
            PatternEntry {
                regex: &RE_ACCEPT_EDITS,
                kind: PromptKind::AcceptEdits,
            },
            PatternEntry {
                regex: &RE_UNKNOWN,
                kind: PromptKind::Unknown,
            },
        ];
        Self { patterns }
    }

    pub fn check(&self, screen: &vt100::Screen) -> Option<PromptKind> {
        let (_rows, cols) = screen.size();
        let all_rows: Vec<String> = screen.rows(0, cols).collect();
        let total_rows = all_rows.len();
        // Check last 15 rows
        let start = total_rows.saturating_sub(15);
        let text: String = all_rows[start..].join("\n");

        for entry in &self.patterns {
            if entry.regex.is_match(&text) {
                return Some(entry.kind.clone());
            }
        }
        None
    }
}
