use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// Daily usage summary
#[derive(Debug, Clone, Default)]
pub struct DailyUsage {
    pub date: String,
    pub messages: u64,
    pub output_tokens: u64,
}

/// Compute daily usage from all Claude Code session JSONL files.
/// Returns last `days` days of usage, sorted by date.
pub fn get_daily_usage(days: usize) -> Vec<DailyUsage> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let projects_dir = home.join(".claude").join("projects");
    let Ok(entries) = fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    let mut daily: BTreeMap<String, DailyUsage> = BTreeMap::new();

    // Compute the cutoff date
    let cutoff = chrono::Local::now()
        .date_naive()
        .checked_sub_days(chrono::Days::new(days as u64))
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    for project_entry in entries.flatten() {
        if !project_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = fs::read_dir(project_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                // Only process files modified recently
                if let Ok(meta) = path.metadata() {
                    if let Ok(modified) = meta.modified() {
                        let age = modified.elapsed().unwrap_or_default();
                        if age.as_secs() > (days as u64 + 1) * 86400 {
                            continue;
                        }
                    }
                }
                parse_jsonl_usage(&path, &cutoff, &mut daily);
            }
        }
    }

    // Fill in missing days with zero
    let today = chrono::Local::now().date_naive();
    for i in 0..days {
        let date = today
            .checked_sub_days(chrono::Days::new(i as u64))
            .map(|d| d.format("%Y-%m-%d").to_string());
        if let Some(date_str) = date {
            daily.entry(date_str.clone()).or_insert(DailyUsage {
                date: date_str,
                messages: 0,
                output_tokens: 0,
            });
        }
    }

    daily
        .into_values()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(days)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn parse_jsonl_usage(path: &PathBuf, cutoff: &str, daily: &mut BTreeMap<String, DailyUsage>) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);

    for line in reader.lines().map_while(Result::ok) {
        // Quick check: skip lines without "usage"
        if !line.contains("\"usage\"") {
            continue;
        }

        // Extract timestamp
        let date = extract_date(&line);
        let Some(date) = date else { continue };
        if date.as_str() < cutoff {
            continue;
        }

        // Extract output tokens from usage
        let Some(output_tokens) = extract_usage_number(&line, "\"output_tokens\":") else {
            continue;
        };

        let entry = daily.entry(date.clone()).or_insert(DailyUsage {
            date,
            messages: 0,
            output_tokens: 0,
        });
        entry.messages += 1;
        entry.output_tokens += output_tokens;
    }
}

/// Extract date (YYYY-MM-DD) from a timestamp field in the line
fn extract_date(line: &str) -> Option<String> {
    // Look for "timestamp":"2026-..." pattern
    let idx = line.find("\"timestamp\":\"")?;
    let start = idx + "\"timestamp\":\"".len();
    if start + 10 > line.len() {
        return None;
    }
    let date = &line[start..start + 10];
    // Validate it looks like a date
    if date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' {
        Some(date.to_string())
    } else {
        None
    }
}

fn extract_usage_number(line: &str, key: &str) -> Option<u64> {
    let usage_idx = line.find("\"usage\"")?;
    let usage_section = &line[usage_idx..];
    let idx = usage_section.find(key)?;
    let start = idx + key.len();
    let rest = usage_section[start..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_date() {
        let line = r#"{"timestamp":"2026-04-09T12:00:00Z","message":{}}"#;
        assert_eq!(extract_date(line), Some("2026-04-09".to_string()));

        let line_no_ts = r#"{"type":"snapshot"}"#;
        assert_eq!(extract_date(line_no_ts), None);
    }

    #[test]
    fn test_extract_usage_number() {
        let line = r#"{"message":{"usage":{"output_tokens":5000,"input_tokens":100}}}"#;
        assert_eq!(
            extract_usage_number(line, "\"output_tokens\":"),
            Some(5000)
        );
        assert_eq!(
            extract_usage_number(line, "\"input_tokens\":"),
            Some(100)
        );
    }
}
