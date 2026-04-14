//! Standalone PreToolUse hook binary for ccom.
//!
//! Invoked by Claude Code's PreToolUse hook mechanism. Reads a JSON blob
//! from stdin, checks allow-always state, then either:
//!   - Immediately allows (allow-always match)
//!   - Routes to the driver via Unix socket (approval required)
//!   - Passes through (no driver / socket not present)
//!
//! Decision protocol: `hookSpecificOutput.permissionDecision: "allow"|"deny"`
//! No output → passthrough to Claude Code's own permission system.
//!
//! IMPORTANT: This binary has NO imports from the ccom library. Only std,
//! serde, serde_json, and sha2 are allowed.

use serde_json::Value;
use std::io::{BufRead, Write};
use std::time::Duration;

// --- JSON types (no ccom lib imports) ------------------------------------

#[derive(serde::Deserialize, Debug)]
struct HookInput {
    /// Claude Code's internal session UUID
    session_id: String,
    tool_name: String,
    tool_input: Value,
    #[allow(dead_code)]
    cwd: Option<String>,
    tool_use_id: Option<String>,
}

#[derive(serde::Deserialize, Debug, Default)]
struct ApprovalsState {
    #[serde(default)]
    allow_always: Vec<AllowAlwaysEntry>,
}

#[derive(serde::Deserialize, Debug)]
struct AllowAlwaysEntry {
    tool: String,
    /// Empty string = match all inputs for this tool.
    /// Non-empty = SHA-256 hex of canonicalized tool_input JSON.
    #[serde(default)]
    input_fingerprint: String,
}

#[derive(serde::Deserialize, Debug)]
struct SocketResponse {
    /// "allow" | "deny" | "passthrough"
    decision: String,
}

#[derive(serde::Serialize)]
struct SocketRequest<'a> {
    /// Claude Code's internal session UUID (from hook stdin).
    session_id: &'a str,
    /// ccom's own session index (from CCOM_SESSION_ID env var).
    /// Allows the coordinator to look up the session without waiting
    /// for the Stop hook to populate `claude_session_id`.
    ccom_session_id: usize,
    tool_name: &'a str,
    tool_input: &'a Value,
    cwd: &'a str,
    tool_use_id: &'a str,
    nonce: u64,
}

// --- Output helpers -------------------------------------------------------

fn print_allow() {
    println!(r#"{{"hookSpecificOutput":{{"permissionDecision":"allow"}}}}"#);
}

fn print_deny(reason: &str) {
    let escaped = reason.replace('\\', "\\\\").replace('"', "\\\"");
    println!(
        r#"{{"hookSpecificOutput":{{"permissionDecision":"deny","permissionDecisionReason":"{escaped}"}}}}"#
    );
}

// --- Fingerprint ----------------------------------------------------------

/// Compute a canonical SHA-256 fingerprint of a JSON value.
/// Keys are sorted, no extra whitespace.
fn fingerprint(value: &Value) -> String {
    use sha2::Digest;
    let canonical = canonical_json(value);
    let hash = sha2::Sha256::digest(canonical.as_bytes());
    format!("sha256:{}", hex_encode(&hash))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Produce a deterministic JSON string with object keys sorted.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut pairs: Vec<_> = map.iter().collect();
            pairs.sort_by_key(|(k, _)| k.as_str());
            let inner: Vec<String> = pairs
                .into_iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical_json(v)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// --- State file path ------------------------------------------------------

fn approvals_path(claude_session_id: &str) -> std::path::PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            std::path::PathBuf::from(home).join(".local/state")
        });
    base.join("ccom")
        .join("sessions")
        .join(claude_session_id)
        .join("approvals.json")
}

// --- Allow-always check ---------------------------------------------------

fn check_allow_always(state: &ApprovalsState, tool_name: &str, tool_input: &Value) -> bool {
    let fp = fingerprint(tool_input);
    for entry in &state.allow_always {
        if entry.tool == tool_name {
            if entry.input_fingerprint.is_empty() || entry.input_fingerprint == fp {
                return true;
            }
        }
    }
    false
}

// --- Socket I/O -----------------------------------------------------------

/// Connect to the approval socket and exchange a request/response.
/// Returns `Some("allow"|"deny"|"passthrough")` or `None` on failure.
fn ask_via_socket(
    socket_path: &std::path::Path,
    request: &SocketRequest<'_>,
    timeout: Duration,
) -> Option<String> {
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;

    let stream = UnixStream::connect(socket_path).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;

    let mut writer = stream.try_clone().ok()?;
    let mut line = serde_json::to_string(request).ok()?;
    line.push('\n');
    writer.write_all(line.as_bytes()).ok()?;
    writer.flush().ok()?;

    let reader = BufReader::new(stream);
    let mut response_line = String::new();
    reader.lines().next().and_then(|l| l.ok()).map(|l| {
        response_line = l;
    });

    let resp: SocketResponse = serde_json::from_str(&response_line).ok()?;
    Some(resp.decision)
}

// --- Main -----------------------------------------------------------------

fn main() {
    // 1. Read stdin JSON
    let mut stdin_buf = String::new();
    {
        let stdin = std::io::stdin();
        let mut lock = stdin.lock();
        lock.read_line(&mut stdin_buf).unwrap_or(0);
    }
    let stdin_buf = stdin_buf.trim();

    let input: HookInput = match serde_json::from_str(stdin_buf) {
        Ok(v) => v,
        Err(_) => {
            // Malformed input → passthrough (no output)
            return;
        }
    };

    let tool_name = &input.tool_name;
    let tool_input = &input.tool_input;
    let claude_session_id = &input.session_id;
    let cwd = input.cwd.as_deref().unwrap_or("");
    let tool_use_id = input.tool_use_id.as_deref().unwrap_or("");

    // 2. Get hook dir and ccom session id from env
    let hook_dir = match std::env::var("CCOM_HOOK_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => {
            // No hook dir configured → passthrough
            return;
        }
    };
    let ccom_session_id: usize = match std::env::var("CCOM_SESSION_ID")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(id) => id,
        None => {
            // Missing or unparseable → passthrough
            return;
        }
    };

    // 3. Check allow-always state file
    let approvals_path = approvals_path(claude_session_id);
    if approvals_path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&approvals_path) {
            if let Ok(state) = serde_json::from_str::<ApprovalsState>(&contents) {
                if check_allow_always(&state, tool_name, tool_input) {
                    print_allow();
                    return;
                }
            }
        }
    }

    // 4. Connect to Unix socket
    let socket_path = hook_dir.join("approval.sock");

    // 5. Build request with random nonce
    let nonce = {
        // Use /dev/urandom for randomness (no rand crate)
        let mut buf = [0u8; 8];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            use std::io::Read;
            let _ = f.read_exact(&mut buf);
        }
        u64::from_le_bytes(buf)
    };

    let request = SocketRequest {
        session_id: claude_session_id,
        ccom_session_id,
        tool_name,
        tool_input,
        cwd,
        tool_use_id,
        nonce,
    };

    // 6. Send request and read response (590s timeout — just under Claude's 600s default)
    let timeout = Duration::from_secs(590);
    match ask_via_socket(&socket_path, &request, timeout) {
        Some(decision) if decision == "allow" => {
            print_allow();
        }
        Some(decision) if decision == "deny" => {
            print_deny("ccom: driver denied");
        }
        Some(_) | None => {
            // passthrough or socket not present / error
            // If socket connection failed entirely, also passthrough
            if !socket_path.exists() {
                // Socket not present → passthrough
                return;
            }
            // "passthrough" decision → no output
        }
    }
}
