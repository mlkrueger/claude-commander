//! Hook-based response boundary detection infrastructure.
//!
//! Phase 3.5: Claude Code's Stop hook fires when a response completes
//! and sends structured JSON on stdin. We install a per-session hook
//! that forwards this JSON to a named pipe (FIFO), which ccom's
//! sidecar reader thread consumes.
//!
//! See `docs/plans/phase-3.5-hook-boundary.md` for the full design.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

/// Parsed content of a Stop hook fire for one session.
///
/// Populated from the JSON that Claude Code writes to the hook's
/// stdin. Only the fields we currently use are extracted; the full
/// stdin may contain more (see `hook-spike.md` for examples).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookStopSignal {
    /// ccom's session id (from the `CCOM_SESSION_ID` env var injected
    /// at spawn time).
    pub ccom_session_id: usize,
    /// Claude Code's internal session UUID (from the `session_id`
    /// field in the stdin JSON).
    pub claude_session_id: String,
    /// The full text of the most recent assistant response. Used as
    /// the `StoredTurn::body` for hook-based sessions, replacing the
    /// ANSI-stripped PTY byte capture used by the regex detector.
    pub last_assistant_message: String,
    /// Optional path to the session transcript JSONL file. Retained
    /// for future features (transcript replay, context diffs).
    pub transcript_path: Option<String>,
}

/// Parse a single hook stdin JSON blob into a [`HookStopSignal`].
///
/// The caller supplies the ccom session id out-of-band (it's already
/// known from how the hook was installed — we don't trust the stdin
/// to identify it). Returns `None` if the JSON is malformed or
/// missing `last_assistant_message`.
pub fn parse_hook_stdin(json: &str, ccom_session_id: usize) -> Option<HookStopSignal> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;

    let last_assistant_message = obj
        .get("last_assistant_message")
        .and_then(|v| v.as_str())
        .map(String::from)?;

    let claude_session_id = obj
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    let transcript_path = obj
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .map(String::from);

    Some(HookStopSignal {
        ccom_session_id,
        claude_session_id,
        last_assistant_message,
        transcript_path,
    })
}

/// Create the per-session hook directory and write the `.claude/settings.json`
/// containing our Stop hook. Returns the root directory path (not the
/// `.claude` subdirectory).
///
/// Layout:
/// ```text
/// /tmp/ccom-<pid>-<session_id>/
///   .claude/
///     settings.json           ← our Stop hook config
///     <other-files>           ← symlinked from ~/.claude/
///   stop.fifo                 ← created separately by create_stop_fifo
/// ```
///
/// To preserve Claude Code's authentication state, we symlink every
/// entry in the user's real `~/.claude/` (resolved via `dirs::home_dir`)
/// into our `.claude/` *except* `settings.json` and `settings.local.json`
/// — those we override with our own hook-only config. This means
/// `CLAUDE_CONFIG_DIR` can safely point at our temp dir without losing
/// credentials, session history, or plugins.
///
/// If `HOME` is missing or `~/.claude` doesn't exist, the function
/// falls back to a non-symlinked layout — Claude Code will prompt for
/// login in that case, which is surfaced to the user via the spawn
/// failure path.
pub fn create_hook_dir(session_id: usize) -> std::io::Result<PathBuf> {
    let pid = process::id();
    let root = std::env::temp_dir().join(format!("ccom-{pid}-{session_id}"));
    // If a previous run with the same pid+id leaked a dir, clean it
    // first. Otherwise `symlink` calls below would fail with EEXIST.
    cleanup_hook_dir(&root);
    let claude_dir = root.join(".claude");
    fs::create_dir_all(&claude_dir)?;

    // Symlink the user's real Claude config into our per-session dir,
    // skipping the settings files we override.
    if let Some(home) = dirs::home_dir() {
        let user_claude = home.join(".claude");
        if user_claude.is_dir() {
            for entry in fs::read_dir(&user_claude)?.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == "settings.json" || name_str == "settings.local.json" {
                    continue;
                }
                let target = entry.path();
                let link = claude_dir.join(&name);
                #[cfg(unix)]
                {
                    if let Err(e) = std::os::unix::fs::symlink(&target, &link) {
                        log::warn!(
                            "failed to symlink {} → {}: {e}",
                            target.display(),
                            link.display()
                        );
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = (&target, &link);
                }
            }
        }
    }

    let fifo_path = root.join("stop.fifo");
    let settings = build_hook_settings(&fifo_path);
    fs::write(claude_dir.join("settings.json"), settings)?;

    Ok(root)
}

/// Build the `.claude/settings.json` contents for our Stop hook.
///
/// The hook command reads stdin (the JSON blob from Claude Code) and
/// appends it as a single line to the FIFO. We use `cat` + shell
/// redirection to keep the hook portable — no ccom-stop-hook helper
/// binary needed.
fn build_hook_settings(fifo_path: &Path) -> String {
    let fifo_str = fifo_path.display().to_string();
    // The hook command: read stdin, append one line (with trailing newline)
    // to the FIFO. `cat` is POSIX, available everywhere.
    let command = format!("cat >> {fifo_str}; printf '\\n' >> {fifo_str}");
    serde_json::json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": command,
                            "timeout": 5
                        }
                    ]
                }
            ]
        }
    })
    .to_string()
}

/// Create the Stop FIFO (named pipe) at the given path.
#[cfg(unix)]
pub fn create_stop_fifo(fifo_path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // mkfifo via libc — std doesn't expose it directly.
    let c_path = std::ffi::CString::new(fifo_path.as_os_str().as_encoded_bytes())
        .map_err(|e| std::io::Error::other(format!("invalid fifo path: {e}")))?;
    let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Ensure the permissions are as expected (mkfifo respects umask).
    fs::set_permissions(fifo_path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn create_stop_fifo(_fifo_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "hook-based boundary detection is only supported on Unix",
    ))
}

/// Spawn a thread that reads lines from the FIFO and forwards parsed
/// [`HookStopSignal`]s to the returned receiver.
///
/// The thread exits when the FIFO is closed (all writers gone) or
/// when the receiver is dropped. The FIFO file itself is NOT removed
/// by this thread — cleanup is the caller's responsibility via
/// [`cleanup_hook_dir`].
pub fn spawn_fifo_reader(
    fifo_path: PathBuf,
    ccom_session_id: usize,
) -> std::io::Result<(JoinHandle<()>, mpsc::Receiver<HookStopSignal>)> {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        // Opening a FIFO for reading blocks until a writer appears.
        // That's fine — the writer is Claude Code's hook invocation,
        // which happens on every Stop event. When all writers close,
        // read() returns EOF and we loop back to reopen.
        loop {
            let file = match fs::File::open(&fifo_path) {
                Ok(f) => f,
                Err(e) => {
                    log::warn!("hook fifo open failed for session {ccom_session_id}: {e}");
                    break;
                }
            };
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match parse_hook_stdin(&line, ccom_session_id) {
                    Some(signal) => {
                        if tx.send(signal).is_err() {
                            return; // receiver dropped, exit thread
                        }
                    }
                    None => {
                        log::warn!(
                            "hook fifo for session {ccom_session_id} received unparseable line: {}",
                            line.chars().take(200).collect::<String>()
                        );
                    }
                }
            }
            // Writer closed; loop to wait for next writer.
        }
    });
    Ok((handle, rx))
}

/// Recursively remove the hook directory. Best-effort; logs on
/// failure. Called from `Session::kill` / `reap_exited`.
pub fn cleanup_hook_dir(hook_dir: &Path) {
    if let Err(e) = fs::remove_dir_all(hook_dir) {
        // ENOENT is fine — already cleaned up.
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!("failed to remove hook dir {}: {e}", hook_dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stdin_extracts_fields() {
        let json = r#"{
            "session_id": "abc-123",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/tmp",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "pong"
        }"#;
        let signal = parse_hook_stdin(json, 42).expect("should parse");
        assert_eq!(signal.ccom_session_id, 42);
        assert_eq!(signal.claude_session_id, "abc-123");
        assert_eq!(signal.last_assistant_message, "pong");
        assert_eq!(signal.transcript_path.as_deref(), Some("/tmp/t.jsonl"));
    }

    #[test]
    fn parse_stdin_rejects_missing_last_message() {
        let json = r#"{"session_id":"x"}"#;
        assert!(parse_hook_stdin(json, 1).is_none());
    }

    #[test]
    fn parse_stdin_rejects_malformed_json() {
        assert!(parse_hook_stdin("{not json", 1).is_none());
        assert!(parse_hook_stdin("", 1).is_none());
    }

    #[test]
    fn build_hook_settings_contains_command_referencing_fifo() {
        let fifo = Path::new("/tmp/test-fifo");
        let settings = build_hook_settings(fifo);
        let parsed: serde_json::Value = serde_json::from_str(&settings).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("/tmp/test-fifo"));
    }

    #[test]
    fn create_hook_dir_writes_settings() {
        let dir = create_hook_dir(999_999_001).expect("should create dir");
        let settings_path = dir.join(".claude/settings.json");
        assert!(settings_path.exists());
        let contents = fs::read_to_string(&settings_path).unwrap();
        assert!(contents.contains("Stop"));
        assert!(contents.contains("stop.fifo"));
        cleanup_hook_dir(&dir);
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_round_trip() {
        let dir = create_hook_dir(999_999_002).expect("create dir");
        let fifo_path = dir.join("stop.fifo");
        create_stop_fifo(&fifo_path).expect("create fifo");

        let (_handle, rx) = spawn_fifo_reader(fifo_path.clone(), 7).expect("spawn reader");

        // Write a fake hook signal to the FIFO.
        let json = r#"{"session_id":"test-uuid","last_assistant_message":"hello"}"#;
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .open(&fifo_path)
                .expect("open fifo for writing");
            writeln!(f, "{json}").unwrap();
        });

        let signal = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("should receive signal");
        assert_eq!(signal.ccom_session_id, 7);
        assert_eq!(signal.last_assistant_message, "hello");

        cleanup_hook_dir(&dir);
    }
}
