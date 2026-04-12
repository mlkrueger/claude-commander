use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

/// Daily usage summary
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct DailyUsage {
    pub date: String,
    pub messages: u64,
    pub output_tokens: u64,
}

/// Compute daily usage from all Claude Code session JSONL files.
/// Returns last `days` days of usage, sorted by date.
#[allow(dead_code)]
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
        if !project_entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        let Ok(files) = fs::read_dir(project_entry.path()) else {
            continue;
        };
        for file_entry in files.flatten() {
            let path = file_entry.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                // Only process files modified recently
                if let Ok(meta) = path.metadata()
                    && let Ok(modified) = meta.modified()
                {
                    let age = modified.elapsed().unwrap_or_default();
                    if age.as_secs() > (days as u64 + 1) * 86400 {
                        continue;
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

#[allow(dead_code)]
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
#[allow(dead_code)]
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

#[allow(dead_code)]
fn extract_usage_number(line: &str, key: &str) -> Option<u64> {
    let usage_idx = line.find("\"usage\"")?;
    let usage_section = &line[usage_idx..];
    let idx = usage_section.find(key)?;
    let start = idx + key.len();
    let rest = usage_section[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
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
        assert_eq!(extract_usage_number(line, "\"output_tokens\":"), Some(5000));
        assert_eq!(extract_usage_number(line, "\"input_tokens\":"), Some(100));
    }

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn test_parse_jsonl_mixed_conversation() {
        // conversation_mixed.jsonl contains:
        //  - 1 plain user message (no "usage" field) -> skipped
        //  - assistant 2026-04-08 output_tokens=150
        //  - assistant 2026-04-08 output_tokens=300 (tool_use)
        //  - 1 tool_result user message (no "usage") -> skipped
        //  - assistant 2026-04-09 output_tokens=500
        //  - 1 malformed line (bad timestamp) -> skipped, not fatal
        //  - assistant 2026-04-09 output_tokens=75
        //  - 1 assistant line dated 2025-01-01 -> filtered by cutoff
        //
        // With cutoff "2026-01-01", expected:
        //   2026-04-08: 450 tokens / 2 messages
        //   2026-04-09: 575 tokens / 2 messages
        //   total:     1025 tokens / 4 messages
        let path = fixture_path("conversation_mixed.jsonl");
        let mut daily: BTreeMap<String, DailyUsage> = BTreeMap::new();
        parse_jsonl_usage(&path, "2026-01-01", &mut daily);

        assert_eq!(daily.len(), 2, "expected two dated buckets");

        let d08 = daily.get("2026-04-08").expect("missing 2026-04-08 bucket");
        assert_eq!(d08.output_tokens, 450);
        assert_eq!(d08.messages, 2);

        let d09 = daily.get("2026-04-09").expect("missing 2026-04-09 bucket");
        assert_eq!(d09.output_tokens, 575);
        assert_eq!(d09.messages, 2);

        let total_tokens: u64 = daily.values().map(|d| d.output_tokens).sum();
        let total_messages: u64 = daily.values().map(|d| d.messages).sum();
        assert_eq!(total_tokens, 1025);
        assert_eq!(total_messages, 4);
    }

    #[test]
    fn test_parse_jsonl_empty_file_is_not_an_error() {
        // An empty conversation.jsonl must yield zero totals, not a panic
        // or error. parse_jsonl_usage returns unit, so we assert the map
        // stays empty.
        let path = fixture_path("conversation_empty.jsonl");
        let mut daily: BTreeMap<String, DailyUsage> = BTreeMap::new();
        parse_jsonl_usage(&path, "2026-01-01", &mut daily);
        assert!(daily.is_empty(), "empty file should produce no entries");
    }

    #[test]
    fn test_parse_jsonl_skips_malformed_lines() {
        // Write a file with one malformed line sandwiched between two good
        // ones. The parser must surface both good lines and simply skip
        // the bad one (not abort, not zero-out).
        let mut path = std::env::temp_dir();
        path.push(format!("ccom_usage_malformed_{}.jsonl", std::process::id()));
        let contents = concat!(
            r#"{"type":"assistant","timestamp":"2026-04-09T08:00:00Z","message":{"usage":{"input_tokens":1,"output_tokens":42}}}"#,
            "\n",
            "this line is not json at all and has no usage field\n",
            r#"{"type":"assistant","timestamp":"2026-04-09T09:00:00Z","message":{"usage":{"input_tokens":2,"output_tokens":58}}}"#,
            "\n",
        );
        fs::write(&path, contents).expect("write temp fixture");

        let mut daily: BTreeMap<String, DailyUsage> = BTreeMap::new();
        parse_jsonl_usage(&path, "2026-01-01", &mut daily);

        // Clean up temp file before assertions in case they fail
        let _ = fs::remove_file(&path);

        assert_eq!(daily.len(), 1);
        let bucket = daily.get("2026-04-09").expect("missing bucket");
        assert_eq!(bucket.output_tokens, 100, "42 + 58 from the two good lines");
        assert_eq!(bucket.messages, 2);
    }
}
