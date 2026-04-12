use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
}

/// Try to discover the Claude session ID for a given process PID.
/// Claude writes session info to ~/.claude/sessions/{pid}.json
pub fn discover_session_id(pid: u32) -> Option<String> {
    let home = dirs::home_dir()?;
    let session_file = home
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));

    let content = std::fs::read_to_string(&session_file).ok()?;
    let parsed: SessionFile = serde_json::from_str(&content).ok()?;
    parsed.session_id
}

/// List all known Claude sessions from ~/.claude/sessions/
pub fn list_claude_sessions() -> Vec<(u32, String)> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let sessions_dir = home.join(".claude").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(parsed) = serde_json::from_str::<SessionFile>(&content)
            && let Some(sid) = parsed.session_id
        {
            results.push((pid, sid));
        }
    }
    results
}
