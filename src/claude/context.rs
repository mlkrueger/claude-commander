use std::fs;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::PathBuf;

/// Reads the context usage percentage for a Claude Code session identified by its PID.
///
/// Flow: PID → ~/.claude/sessions/<PID>.json → sessionId
///       → ~/.claude/projects/<project-path>/<sessionId>.jsonl → last usage.input_tokens
///       → percentage of context window
pub fn get_context_percent(pid: u32) -> Option<f64> {
    let session_id = read_session_id(pid)?;
    let jsonl_path = find_session_jsonl(&session_id)?;
    let (input_tokens, model, is_claude_ai) = read_last_usage(&jsonl_path)?;
    let window_size = context_window_for_model(&model, is_claude_ai);
    Some((input_tokens as f64 / window_size as f64) * 100.0)
}

/// Read the Claude session ID from ~/.claude/sessions/<PID>.json
fn read_session_id(pid: u32) -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    let contents = fs::read_to_string(path).ok()?;
    // Simple JSON extraction — avoid pulling in a full JSON parser just for this
    extract_json_string(&contents, "sessionId")
}

/// Find the JSONL transcript file for a session ID.
/// Searches all project directories under ~/.claude/projects/
fn find_session_jsonl(session_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude").join("projects");
    let filename = format!("{session_id}.jsonl");

    for entry in fs::read_dir(projects_dir).ok()?.flatten() {
        if entry.file_type().ok()?.is_dir() {
            let candidate = entry.path().join(&filename);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Read the last assistant message's total context usage from the JSONL file.
/// Returns (total_context_tokens, model_name, is_claude_ai_auth).
/// Total = input_tokens + cache_read_input_tokens + cache_creation_input_tokens
///
/// Reads only the tail of the file (last 64KB) for performance on large transcripts.
fn read_last_usage(path: &PathBuf) -> Option<(u64, String, bool)> {
    let mut file = fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();

    // Read last 64KB (or entire file if smaller)
    let read_from = file_len.saturating_sub(65_536);
    if read_from > 0 {
        file.seek(SeekFrom::Start(read_from)).ok()?;
        // Skip partial first line after seeking
        let mut reader = std::io::BufReader::new(&mut file);
        let mut discard = String::new();
        reader.read_line(&mut discard).ok()?;
        return parse_last_usage_from_reader(reader);
    }

    let reader = std::io::BufReader::new(file);
    parse_last_usage_from_reader(reader)
}

fn parse_last_usage_from_reader<R: BufRead>(reader: R) -> Option<(u64, String, bool)> {
    let mut last_total: Option<u64> = None;
    let mut last_model = String::from("claude-sonnet-4-20250514");
    let mut is_claude_ai = false;

    for line in reader.lines().map_while(Result::ok) {
        if let Some(total) = extract_total_context_tokens(&line) {
            last_total = Some(total);
            if let Some(model) = extract_json_string(&line, "model") {
                last_model = model;
            }
        }
        // Detect Claude.ai auth from userType field (appears in JSONL entries)
        if !is_claude_ai && line.contains("\"userType\":\"external\"") {
            is_claude_ai = true;
        }
    }

    last_total.map(|t| (t, last_model, is_claude_ai))
}

/// Extract total context tokens from a JSONL line.
/// Total = input_tokens + cache_read_input_tokens + cache_creation_input_tokens
fn extract_total_context_tokens(line: &str) -> Option<u64> {
    let usage_idx = line.find("\"usage\"")?;
    let usage_section = &line[usage_idx..];

    let input = extract_number(usage_section, "\"input_tokens\":")?;
    let cache_read = extract_number(usage_section, "\"cache_read_input_tokens\":").unwrap_or(0);
    let cache_create =
        extract_number(usage_section, "\"cache_creation_input_tokens\":").unwrap_or(0);

    Some(input + cache_read + cache_create)
}

/// Extract a numeric value following a key pattern in a string
fn extract_number(text: &str, key: &str) -> Option<u64> {
    let idx = text.find(key)?;
    let start = idx + key.len();
    let rest = text[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Simple JSON string value extractor — finds "key":"value" patterns
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let idx = json.find(&pattern)?;
    let start = idx + pattern.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Return context window size (in tokens) for a given model.
/// Claude.ai auth users get 1M context for Opus models.
fn context_window_for_model(model: &str, is_claude_ai: bool) -> u64 {
    if is_claude_ai && model.contains("opus") {
        1_000_000
    } else {
        200_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_string() {
        let json = r#"{"sessionId":"abc-123","cwd":"/tmp"}"#;
        assert_eq!(
            extract_json_string(json, "sessionId"),
            Some("abc-123".to_string())
        );
        assert_eq!(extract_json_string(json, "cwd"), Some("/tmp".to_string()));
        assert_eq!(extract_json_string(json, "missing"), None);
    }

    #[test]
    fn test_extract_total_context_tokens() {
        let line = r#"{"message":{"usage":{"input_tokens":1,"cache_read_input_tokens":80000,"cache_creation_input_tokens":5000,"output_tokens":500}}}"#;
        assert_eq!(extract_total_context_tokens(line), Some(85001));

        let line_no_cache = r#"{"message":{"usage":{"input_tokens":42000,"output_tokens":500}}}"#;
        assert_eq!(extract_total_context_tokens(line_no_cache), Some(42000));

        let line_no_usage = r#"{"type":"file-history-snapshot"}"#;
        assert_eq!(extract_total_context_tokens(line_no_usage), None);
    }

    #[test]
    fn test_context_window_for_model() {
        // Claude.ai auth: Opus gets 1M context
        assert_eq!(context_window_for_model("claude-opus-4-6", true), 1_000_000);
        // API auth: Opus gets 200k
        assert_eq!(context_window_for_model("claude-opus-4-6", false), 200_000);
        // Non-opus models always 200k
        assert_eq!(context_window_for_model("claude-sonnet-4-6", true), 200_000);
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001", false),
            200_000
        );
    }
}
