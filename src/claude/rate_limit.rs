use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

/// Rate limit info written by the statusline hook
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitFile {
    pub five_hour: Option<RateLimitWindow>,
    pub seven_day: Option<RateLimitWindow>,
    pub cost: Option<CostInfo>,
    #[serde(default)]
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitWindow {
    pub used_percentage: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CostInfo {
    pub total_cost_usd: Option<f64>,
}

/// Displayable rate limit info for the UI
#[derive(Debug, Clone, Default)]
pub struct RateLimitInfo {
    /// 5-hour window usage percentage (0.0 - 100.0), if known
    pub session_pct: Option<f64>,
    /// 5-hour window reset time as human-readable string
    pub session_resets: Option<String>,
    /// Weekly window usage percentage (0.0 - 100.0), if known
    pub weekly_pct: Option<f64>,
    /// Weekly window reset time as human-readable string
    pub weekly_resets: Option<String>,
    /// Session cost in USD
    pub cost_usd: Option<f64>,
}

/// Path to the rate limit file written by the statusline hook
fn rate_limit_file_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude").join("ccom-rate-limits.json"))
}

/// Format a unix timestamp as a human-readable reset time
fn format_reset_time(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    let dt = match Local.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => dt,
        _ => return format!("at {ts}"),
    };
    let now = Local::now();
    let today = now.date_naive();
    let reset_date = dt.date_naive();

    if reset_date == today {
        // Same day: "Resets 10:09am"
        dt.format("%-I:%M%P").to_string()
    } else {
        // Different day: "Apr 13 at 8am"
        dt.format("%b %-d at %-I%P").to_string()
    }
}

/// Read rate limit info from the file written by the statusline hook.
pub fn get_rate_limit_info() -> Option<RateLimitInfo> {
    let path = rate_limit_file_path()?;
    let contents = fs::read_to_string(path).ok()?;
    let file: RateLimitFile = serde_json::from_str(&contents).ok()?;

    let mut info = RateLimitInfo::default();

    if let Some(window) = &file.five_hour {
        info.session_pct = window.used_percentage;
        info.session_resets = window.resets_at.map(format_reset_time);
    }

    if let Some(window) = &file.seven_day {
        info.weekly_pct = window.used_percentage;
        info.weekly_resets = window.resets_at.map(format_reset_time);
    }

    if let Some(cost) = &file.cost {
        info.cost_usd = cost.total_cost_usd;
    }

    Some(info)
}

/// Read rate limit info from the stream-json rate_limit_event data.
/// Falls back to checking the telemetry file for the latest event.
pub fn get_rate_limit_from_telemetry() -> Option<RateLimitInfo> {
    let home = dirs::home_dir()?;
    let telemetry_dir = home.join(".claude").join("telemetry");
    let entries = fs::read_dir(telemetry_dir).ok()?;

    let mut latest_status = None;
    let mut latest_resets_at = None;
    let mut latest_timestamp = String::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "json") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
            if !line.contains("limits_status_changed") {
                continue;
            }
            // Extract timestamp and rate limit info
            if let Some(ts) = extract_json_string_val(&line, "client_timestamp") {
                if ts > latest_timestamp {
                    latest_timestamp = ts;
                    if let Some(meta) = extract_json_string_val(&line, "additional_metadata") {
                        // Parse the nested JSON string
                        if let Ok(meta_obj) = serde_json::from_str::<serde_json::Value>(&meta) {
                            latest_status = meta_obj
                                .get("status")
                                .and_then(|s| s.as_str())
                                .map(String::from);
                            latest_resets_at = meta_obj
                                .get("hoursTillReset")
                                .and_then(|h| h.as_u64())
                                .map(|h| format!("~{}h", h));
                        }
                    }
                }
            }
        }
    }

    if latest_status.is_some() {
        Some(RateLimitInfo {
            session_pct: match latest_status.as_deref() {
                Some("allowed") => None, // we know it's under limit but not exact %
                Some("rate_limited") => Some(100.0),
                _ => None,
            },
            session_resets: latest_resets_at,
            weekly_pct: None,
            weekly_resets: None,
            cost_usd: None,
        })
    } else {
        None
    }
}

fn extract_json_string_val(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let idx = json.find(&pattern)?;
    let start = idx + pattern.len();
    let rest = &json[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
