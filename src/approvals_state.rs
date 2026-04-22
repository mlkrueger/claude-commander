//! Per-session tool approval state file.
//!
//! Stores allow-always rules as JSON at
//! `$XDG_STATE_HOME/ccom/sessions/<claude-uuid>/approvals.json`.
//!
//! The hook binary (`ccom-hook-pretooluse`) reads this file on every
//! invocation to short-circuit tool calls that the driver has permanently
//! allowed without blocking on a socket roundtrip.
//!
//! Writes are atomic (tmp → rename on POSIX) and serialized within a
//! single process via a module-level `Mutex`. Cross-process writers
//! (rare in practice) are protected from corruption; a concurrent
//! read-modify-write from another process could lose one rule, which is
//! harmless — the next approval request will re-present it.

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

/// In-process write serialization.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Root state struct stored in `approvals.json`.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default, Clone)]
pub struct ApprovalsState {
    #[serde(default)]
    pub allow_always: Vec<AllowAlwaysEntry>,
}

/// One allow-always rule.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AllowAlwaysEntry {
    pub tool: String,
    /// SHA-256 fingerprint of the canonical tool input JSON.
    /// Empty string = wildcard (matches all inputs for this tool).
    #[serde(default)]
    pub input_fingerprint: String,
}

/// Absolute path to `approvals.json` for a given Claude session UUID.
pub fn state_file_path(claude_uuid: &str) -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".local/state")
        });
    base.join("ccom")
        .join("sessions")
        .join(claude_uuid)
        .join("approvals.json")
}

/// Read the current approvals state for a session.
/// Returns an empty `ApprovalsState` if the file does not exist.
#[allow(dead_code)]
pub fn read_approvals(claude_uuid: &str) -> io::Result<ApprovalsState> {
    let path = state_file_path(claude_uuid);
    if !path.exists() {
        return Ok(ApprovalsState::default());
    }
    let contents = std::fs::read_to_string(&path)?;
    serde_json::from_str(&contents).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Add an allow-always rule to the state file, creating parent
/// directories if needed.
///
/// - `input_fingerprint = None` → wildcard (matches all inputs for this
///   tool).
/// - `input_fingerprint = Some(fp)` → matches only inputs whose SHA-256
///   fingerprint equals `fp`.
///
/// Duplicate rules (same tool + fingerprint) are silently ignored.
/// Writes are atomic: the file is written to a temp path and renamed.
pub fn add_allow_always(
    claude_uuid: &str,
    tool: &str,
    input_fingerprint: Option<&str>,
) -> io::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let path = state_file_path(claude_uuid);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Read current state; treat parse errors as empty.
    let mut state = if path.exists() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ApprovalsState>(&s).ok())
            .unwrap_or_default()
    } else {
        ApprovalsState::default()
    };

    let fp = input_fingerprint.unwrap_or("").to_string();

    let already_present = state
        .allow_always
        .iter()
        .any(|e| e.tool == tool && e.input_fingerprint == fp);
    if !already_present {
        state.allow_always.push(AllowAlwaysEntry {
            tool: tool.to_string(),
            input_fingerprint: fp,
        });
    }

    let json = serde_json::to_string_pretty(&state).map_err(io::Error::other)?;

    // Atomic write: temp file → rename.
    let tmp_path = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), rand_nonce()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &path)?;

    // Best-effort: fsync the directory entry so the rename is durable.
    if let Some(parent) = path.parent() {
        let _ = std::fs::File::open(parent).and_then(|f| f.sync_all());
    }

    Ok(())
}

/// Compute the SHA-256 fingerprint of a JSON value's canonical form
/// (object keys sorted, no extra whitespace). Returns `"sha256:<hex>"`.
pub fn input_fingerprint(tool_input: &serde_json::Value) -> String {
    use sha2::Digest;
    let canonical = canonical_json(tool_input);
    let hash = sha2::Sha256::digest(canonical.as_bytes());
    format!("sha256:{}", hex_encode(&hash))
}

/// Check whether `state` contains an allow-always rule that matches
/// `tool` + `tool_input`.
#[allow(dead_code)]
pub fn matches_allow_always(
    state: &ApprovalsState,
    tool: &str,
    tool_input: &serde_json::Value,
) -> bool {
    let fp = input_fingerprint(tool_input);
    state
        .allow_always
        .iter()
        .any(|e| e.tool == tool && (e.input_fingerprint.is_empty() || e.input_fingerprint == fp))
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn rand_nonce() -> u64 {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut buf);
    }
    u64::from_le_bytes(buf)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
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
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;

    /// Serializes tests that mutate XDG_STATE_HOME (process-global env var).
    static XDG_MUTEX: Mutex<()> = Mutex::new(());

    fn unique_uuid() -> String {
        format!("test-{:016x}", rand_nonce())
    }

    /// Run `f(uuid)` with XDG_STATE_HOME pointing at a throw-away temp
    /// directory. Cleans up the session dir afterward.
    fn with_temp_xdg<R, F: FnOnce(&str) -> R>(f: F) -> R {
        let _guard = XDG_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = std::env::temp_dir().join("ccom-test-approvals-state");
        std::fs::create_dir_all(&tmp).expect("create temp base");
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.to_str().unwrap());
        }
        let uuid = unique_uuid();
        let result = f(&uuid);
        // Best-effort cleanup.
        let session_dir = tmp.join("ccom").join("sessions").join(&uuid);
        let _ = std::fs::remove_dir_all(&session_dir);
        result
    }

    #[test]
    fn state_file_creates_dirs_if_missing() {
        with_temp_xdg(|uuid| {
            let path = state_file_path(uuid);
            assert!(!path.exists());
            add_allow_always(uuid, "Bash", None).expect("add_allow_always");
            assert!(path.exists(), "approvals.json must have been created");
        });
    }

    #[test]
    fn state_file_add_and_match_exact_fingerprint() {
        with_temp_xdg(|uuid| {
            let args = serde_json::json!({"command": "ls -la"});
            let fp = input_fingerprint(&args);
            add_allow_always(uuid, "Bash", Some(&fp)).expect("add");

            let state = read_approvals(uuid).expect("read");
            assert!(matches_allow_always(&state, "Bash", &args));
            // Different args must NOT match.
            let other = serde_json::json!({"command": "rm -rf /"});
            assert!(!matches_allow_always(&state, "Bash", &other));
        });
    }

    #[test]
    fn state_file_wildcard_matches_any_input() {
        with_temp_xdg(|uuid| {
            // None = wildcard.
            add_allow_always(uuid, "Edit", None).expect("add");
            let state = read_approvals(uuid).expect("read");

            let any_args = serde_json::json!({"path": "/foo", "content": "hello"});
            assert!(matches_allow_always(&state, "Edit", &any_args));
            assert!(matches_allow_always(&state, "Edit", &serde_json::json!({})));
            // Different tool must NOT match.
            assert!(!matches_allow_always(&state, "Bash", &any_args));
        });
    }

    #[test]
    fn state_file_is_idempotent_for_duplicate_rules() {
        with_temp_xdg(|uuid| {
            let fp = "sha256:aabbcc";
            add_allow_always(uuid, "Read", Some(fp)).expect("first add");
            add_allow_always(uuid, "Read", Some(fp)).expect("second add");

            let state = read_approvals(uuid).expect("read");
            let count = state
                .allow_always
                .iter()
                .filter(|e| e.tool == "Read" && e.input_fingerprint == fp)
                .count();
            assert_eq!(count, 1, "duplicate rule must not be stored twice");
        });
    }

    #[test]
    fn state_file_atomic_rename_preserves_existing_on_parse_error() {
        with_temp_xdg(|uuid| {
            // First, write a valid rule.
            add_allow_always(uuid, "Write", None).expect("initial add");

            // Corrupt the file (simulates a partial write from another process).
            let path = state_file_path(uuid);
            std::fs::write(&path, b"{ invalid json }").expect("corrupt");

            // add_allow_always must recover and produce a valid file
            // with our new rule (the corrupt file is treated as empty state).
            add_allow_always(uuid, "Bash", None).expect("add after corrupt");

            let state = read_approvals(uuid).expect("read after recovery");
            // The corrupt file was treated as empty, so only Bash is present.
            assert!(
                matches_allow_always(&state, "Bash", &serde_json::json!({})),
                "Bash rule must be present"
            );
        });
    }

    #[test]
    fn state_file_handles_concurrent_writers() {
        // Two threads write different rules to the same state file.
        // Both rules must be present afterward.
        //
        // WRITE_LOCK serializes the writes within the process, so the
        // last write wins without losing either rule — each thread reads
        // the current state before appending.
        with_temp_xdg(|uuid| {
            // Capture the uuid for the threads (which can't borrow).
            let uuid1 = uuid.to_string();
            let uuid2 = uuid.to_string();

            let h1 = thread::spawn(move || {
                for _ in 0..5 {
                    add_allow_always(&uuid1, "Bash", None).expect("h1 add");
                }
            });
            let h2 = thread::spawn(move || {
                for _ in 0..5 {
                    add_allow_always(&uuid2, "Edit", None).expect("h2 add");
                }
            });
            h1.join().expect("h1 join");
            h2.join().expect("h2 join");

            let state = read_approvals(uuid).expect("read");
            assert!(
                matches_allow_always(&state, "Bash", &serde_json::json!({})),
                "Bash rule must be present"
            );
            assert!(
                matches_allow_always(&state, "Edit", &serde_json::json!({})),
                "Edit rule must be present"
            );
        });
    }
}
