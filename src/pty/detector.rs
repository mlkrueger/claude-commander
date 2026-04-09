use regex::Regex;

#[derive(Debug, Clone)]
pub enum PromptKind {
    AllowOnce,
    YesNo,
    PressEnter,
    AcceptEdits,
    Unknown,
}

pub struct PromptDetector {
    patterns: Vec<(Regex, PromptKind)>,
}

impl PromptDetector {
    pub fn new() -> Self {
        let patterns = vec![
            (
                Regex::new(r"(?i)(Allow once|Allow always|allow tool|Don't allow)").unwrap(),
                PromptKind::AllowOnce,
            ),
            (
                Regex::new(r"(?i)(Do you want to|Yes/No|Y/n|y/N|\[Y/n\]|\[y/N\]|approve|deny)")
                    .unwrap(),
                PromptKind::YesNo,
            ),
            (
                Regex::new(r"(?i)(press enter|Press Enter)").unwrap(),
                PromptKind::PressEnter,
            ),
            (
                Regex::new(r"(?i)(accept edits)").unwrap(),
                PromptKind::AcceptEdits,
            ),
            (
                Regex::new(r"(?i)(permission|⎕|Reject)").unwrap(),
                PromptKind::Unknown,
            ),
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

        for (pattern, kind) in &self.patterns {
            if pattern.is_match(&text) {
                return Some(kind.clone());
            }
        }
        None
    }
}
