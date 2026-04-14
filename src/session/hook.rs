//! Hook-based response boundary detection infrastructure.
//!
//! Phase 3.5: Claude Code's Stop hook fires when a response completes
//! and sends structured JSON on stdin. We install a per-session hook
//! that forwards this JSON to a named pipe (FIFO), which ccom's
//! sidecar reader thread consumes.
//!
//! See `docs/plans/phase-3.5-hook-boundary.md` for the full design.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::BufRead;
#[cfg(unix)]
use std::process;

/// Maximum bytes we will read for a single hook stdin line before
/// skipping it with a warning. Claude Code's assistant messages can
/// be large (tool outputs inlined, base64 blobs, etc.) but 16 MB is
/// well above any realistic response and bounds reader memory growth
/// if a writer pathologically never emits a newline.
///
/// See review issue C4 in `docs/pr-review-pr13.md`.
pub const MAX_HOOK_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Handle to a sidecar FIFO reader thread. Wraps the join handle
/// with an [`AtomicBool`] stop flag the reader checks on each loop
/// iteration, so `Session::cleanup_hook_artifacts` can request
/// shutdown cleanly without relying solely on the write-poke trick
/// for unblocking `File::open`.
///
/// See review issue C3 in `docs/pr-review-pr13.md`.
pub struct SidecarHandle {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SidecarHandle {
    /// Set the stop flag. The reader thread checks this on each
    /// outer-loop iteration (between `File::open` calls and between
    /// inner reads). Callers must still write-poke the FIFO if the
    /// reader is currently blocked inside `File::open`.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// True when the reader thread has exited.
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Block up to `timeout` waiting for the reader thread to
    /// exit, then join it. Returns `Ok(())` on clean exit or a
    /// descriptive error if the timeout expires (the thread is
    /// then orphaned; caller should log-error with full context).
    pub fn join_with_timeout(&mut self, timeout: Duration) -> std::io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let start = Instant::now();
        while !handle.is_finished() {
            if start.elapsed() >= timeout {
                // Put it back so a later call can still observe it.
                self.handle = Some(handle);
                return Err(std::io::Error::other(
                    "sidecar reader thread did not exit within timeout",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        handle
            .join()
            .map_err(|_| std::io::Error::other("sidecar reader thread panicked"))
    }
}

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
    /// field in the stdin JSON). `None` if the payload lacks the
    /// field (older Claude Code versions / malformed hook input) —
    /// consumers treat absence as "no change" rather than a clear.
    pub claude_session_id: Option<String>,
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

    let last_assistant_message = match obj.get("last_assistant_message") {
        Some(v) => match v.as_str() {
            Some(s) => s.to_string(),
            None => {
                let ty = json_type_name(v);
                log::warn!(
                    "hook stdin for session {ccom_session_id} has last_assistant_message of wrong type: expected string, got {ty}"
                );
                return None;
            }
        },
        None => return None,
    };

    // Empty string is treated the same as missing so a bogus payload
    // can't clobber a previously-captured UUID downstream.
    let claude_session_id = obj
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);

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

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// POSIX-safe single-quote escape. Wraps `s` in `'...'`, turning any
/// embedded `'` into `'\''`. Safe for use inside a shell command.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Create the per-session hook directory and write the flat
/// `settings.json` containing our Stop hook. Returns the root
/// directory path.
///
/// Layout (flat — no `.claude/` subdir, no symlinks):
/// ```text
/// /tmp/ccom-<pid>-<session_id>/
///   settings.json    ← our Stop hook config (loaded via `--settings`)
///   .mcp.json        ← MCP server config (loaded via `--mcp-config`,
///                       written by write_mcp_config)
///   stop.fifo        ← created separately by create_stop_fifo
/// ```
///
/// **History / important**: an earlier Phase 3.5 approach created a
/// `.claude/` subdir, symlinked the user's real `~/.claude/*` into
/// it, and pointed Claude Code at the result via `CLAUDE_CONFIG_DIR`.
/// That broke **macOS Keychain authentication**: Claude Code binds
/// its OAuth credential entry to the config-dir path, so changing
/// `CLAUDE_CONFIG_DIR` invalidates the Keychain binding and forces
/// a fresh login every session. Symlinking the credential subdirs
/// didn't help because the Keychain ACL is path-based, not
/// filesystem-based.
///
/// The fix: don't touch `CLAUDE_CONFIG_DIR`. Use Claude Code's
/// `--settings <file>` CLI flag to layer our hook config on top of
/// the user's real config, and `--mcp-config <file>` for the MCP
/// server. `Session::spawn` injects both flags into the command
/// line. The user's `~/.claude/` stays the source of truth for
/// credentials, history, plugins, and everything else.
///
/// Directory is created with mode 0700 and `settings.json` with
/// mode 0600 via `create_new` to refuse following a pre-existing
/// symlink (TOCTOU review issue H1).
#[cfg(unix)]
pub fn create_hook_dir(session_id: usize) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let pid = process::id();
    let root = std::env::temp_dir().join(format!("ccom-{pid}-{session_id}"));
    // If a previous run with the same pid+id leaked a dir, clean it
    // first. Otherwise `create_new` on settings.json would fail.
    cleanup_hook_dir(&root);

    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(&root)?;

    let fifo_path = root.join("stop.fifo");
    let settings = build_hook_settings(&fifo_path)?;
    let settings_path = root.join("settings.json");
    // `create_new(true)` refuses to follow any pre-existing symlink
    // (returns EEXIST) — this closes the TOCTOU window (issue H1).
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&settings_path)?;
    f.write_all(settings.as_bytes())?;

    Ok(root)
}

#[cfg(not(unix))]
pub fn create_hook_dir(_session_id: usize) -> std::io::Result<PathBuf> {
    Err(std::io::Error::other(
        "hook-based boundary detection is only supported on Unix",
    ))
}

/// Phase 4 Task 6: write a per-session `.mcp.json` in the hook dir
/// pointing the spawned Claude Code process at ccom's embedded MCP
/// server on `http://127.0.0.1:<port>/mcp`.
///
/// The file is loaded by passing `--mcp-config <hook_dir>/.mcp.json`
/// on the Claude command line (done in `Session::spawn`). This
/// avoids touching `CLAUDE_CONFIG_DIR` — see `create_hook_dir`'s doc
/// for why the config-dir approach was abandoned.
///
/// Schema (confirmed against Claude Code 2.1.x docs):
/// ```json
/// {
///   "mcpServers": {
///     "ccom": {
///       "type": "http",
///       "url": "http://127.0.0.1:<port>/mcp"
///     }
///   }
/// }
/// ```
///
/// File is written mode 0600 via `create_new` to avoid overwriting
/// anything pre-existing.
#[cfg(unix)]
pub fn write_mcp_config(
    hook_dir: &Path,
    port: u16,
    caller_id: Option<usize>,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = hook_dir.join(".mcp.json");
    let contents = build_mcp_config_json(port, caller_id);

    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

/// Phase 6 Task 3: build the `.mcp.json` body string. Split out
/// from the Unix-gated file writer so the JSON shape (especially
/// the optional `headers.X-Ccom-Caller` injection) can be unit
/// tested without touching the filesystem.
///
/// When `caller_id` is `Some`, a `"headers": { "X-Ccom-Caller": "<id>" }`
/// block is added to the `ccom` server entry. Claude Code propagates
/// the header on every tool-call POST, which lets the in-process
/// MCP server identify which ccom session is calling it — the
/// load-bearing mechanism behind driver-role scoping.
pub(crate) fn build_mcp_config_json(port: u16, caller_id: Option<usize>) -> String {
    let mut server = serde_json::Map::new();
    server.insert("type".to_string(), serde_json::json!("http"));
    server.insert(
        "url".to_string(),
        serde_json::json!(format!("http://127.0.0.1:{port}/mcp")),
    );
    if let Some(id) = caller_id {
        let mut headers = serde_json::Map::new();
        headers.insert(
            "X-Ccom-Caller".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        server.insert("headers".to_string(), serde_json::Value::Object(headers));
    }
    serde_json::json!({
        "mcpServers": {
            "ccom": serde_json::Value::Object(server),
        }
    })
    .to_string()
}

#[cfg(not(unix))]
pub fn write_mcp_config(
    _hook_dir: &Path,
    _port: u16,
    _caller_id: Option<usize>,
) -> std::io::Result<()> {
    Err(std::io::Error::other(
        ".mcp.json injection is only supported on Unix",
    ))
}

/// Build the `.claude/settings.json` contents for our Stop hook.
///
/// The hook command reads stdin (the JSON blob from Claude Code) and
/// appends it as a single line to the FIFO. We use `cat` + shell
/// redirection to keep the hook portable — no ccom-stop-hook helper
/// binary needed.
///
/// The fifo path is POSIX-single-quote-escaped, so paths containing
/// spaces, single quotes, `$`, backticks, etc. are handled safely
/// (issue C2).
///
/// Returns `Err` if the fifo path is not valid UTF-8. In that case
/// the hook command string could not be encoded into `settings.json`
/// without loss (review second-pass N2). Practically this only
/// happens on Unix with a non-UTF-8 `TMPDIR`, which is exceedingly
/// rare — the function fails loudly rather than silently producing
/// a settings.json whose quoted path doesn't match the real FIFO.
fn build_hook_settings(fifo_path: &Path) -> std::io::Result<String> {
    let fifo_str = fifo_path.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("hook fifo path is not valid UTF-8: {}", fifo_path.display()),
        )
    })?;
    let quoted = shell_single_quote(fifo_str);
    // The hook command: read stdin, append one line (with trailing newline)
    // to the FIFO. `cat` is POSIX, available everywhere.
    let command = format!("cat >> {quoted}; printf '\\n' >> {quoted}");
    Ok(serde_json::json!({
        "hooks": {
            "Stop": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": command,
                            "timeout": 30
                        }
                    ]
                }
            ]
        }
    })
    .to_string())
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
/// [`HookStopSignal`]s through the returned receiver. Returns a
/// [`SidecarHandle`] for coordinated shutdown plus the signal
/// receiver.
///
/// The thread exits when any of:
/// - The receiver is dropped ([`mpsc::Sender::send`] returns `Err`)
/// - The stop flag on [`SidecarHandle`] is set AND the reader next
///   checks it (between `File::open` calls or between lines)
/// - A hard open error occurs that persists past retry budget
///
/// Individual lines are bounded at [`MAX_HOOK_LINE_BYTES`]; oversized
/// lines are skipped with a warning (issue C4).
///
/// The FIFO file itself is NOT removed by this thread — cleanup is
/// the caller's responsibility via [`cleanup_hook_dir`].
#[cfg(unix)]
pub fn spawn_fifo_reader(
    fifo_path: PathBuf,
    ccom_session_id: usize,
) -> std::io::Result<(SidecarHandle, mpsc::Receiver<HookStopSignal>)> {
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        // Opening a FIFO for reading blocks until a writer appears.
        // That's fine — the writer is Claude Code's hook invocation,
        // which happens on every Stop event. When all writers close,
        // read() returns EOF and we loop back to reopen.
        'outer: loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            // Retry transient open failures up to 3 times with 50ms
            // backoff (issue M4).
            let file = 'open: {
                let mut last_err: Option<std::io::Error> = None;
                for attempt in 0..3 {
                    if stop_for_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    match std::fs::File::open(&fifo_path) {
                        Ok(f) => break 'open Some(f),
                        Err(e) => {
                            log::warn!(
                                "hook fifo open failed for session {ccom_session_id} (attempt {}): {e}",
                                attempt + 1
                            );
                            last_err = Some(e);
                            thread::sleep(Duration::from_millis(50));
                        }
                    }
                }
                log::warn!(
                    "hook fifo open for session {ccom_session_id} exhausted retries: {:?}",
                    last_err
                );
                None
            };
            let Some(file) = file else { break 'outer };

            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            let mut reader = std::io::BufReader::new(file);
            let mut buf: Vec<u8> = Vec::new();
            loop {
                if stop_for_thread.load(Ordering::Relaxed) {
                    return;
                }
                buf.clear();
                // Manual bounded read: read up to one line but cap at
                // MAX_HOOK_LINE_BYTES + 1 so we can detect overflow.
                let n = match read_line_bounded(&mut reader, &mut buf, MAX_HOOK_LINE_BYTES) {
                    Ok(ReadOutcome::Eof) => break, // writer closed; reopen
                    Ok(ReadOutcome::Line(n)) => n,
                    Ok(ReadOutcome::Oversized) => {
                        log::warn!(
                            "hook fifo for session {ccom_session_id} skipped oversized line (>{} bytes)",
                            MAX_HOOK_LINE_BYTES
                        );
                        continue;
                    }
                    Err(e) => {
                        log::warn!("hook fifo read error for session {ccom_session_id}: {e}");
                        break;
                    }
                };
                if n == 0 {
                    break;
                }
                // Trim trailing \n (and \r\n).
                while matches!(buf.last(), Some(&b'\n') | Some(&b'\r')) {
                    buf.pop();
                }
                if buf.is_empty() {
                    continue;
                }
                let line = match std::str::from_utf8(&buf) {
                    Ok(s) => s,
                    Err(_) => {
                        log::warn!(
                            "hook fifo for session {ccom_session_id} received non-UTF8 line; skipping"
                        );
                        continue;
                    }
                };
                match parse_hook_stdin(line, ccom_session_id) {
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
    Ok((
        SidecarHandle {
            stop,
            handle: Some(handle),
        },
        rx,
    ))
}

#[cfg(not(unix))]
pub fn spawn_fifo_reader(
    _fifo_path: PathBuf,
    _ccom_session_id: usize,
) -> std::io::Result<(SidecarHandle, mpsc::Receiver<HookStopSignal>)> {
    Err(std::io::Error::other(
        "hook-based boundary detection is only supported on Unix",
    ))
}

#[cfg(unix)]
enum ReadOutcome {
    Eof,
    Line(usize),
    Oversized,
}

/// Read one `\n`-terminated line into `buf`, capped at `max` bytes.
/// If the line would exceed `max`, drains the rest of the line
/// (up to the next newline) and returns `Oversized`.
#[cfg(unix)]
fn read_line_bounded<R: BufRead>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<ReadOutcome> {
    // Use read_until but in chunks so we can enforce the cap without
    // allocating unbounded memory.
    loop {
        let available = match reader.fill_buf() {
            Ok([]) => {
                if buf.is_empty() {
                    return Ok(ReadOutcome::Eof);
                } else {
                    return Ok(ReadOutcome::Line(buf.len()));
                }
            }
            Ok(b) => b,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let (done, used) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (true, i + 1),
            None => (false, available.len()),
        };
        // Enforce cap: if appending `used` bytes would exceed max, drain+discard.
        if buf.len().saturating_add(used) > max {
            reader.consume(used);
            // Keep draining this line until we hit a newline or EOF.
            loop {
                let avail = match reader.fill_buf() {
                    Ok([]) => return Ok(ReadOutcome::Oversized),
                    Ok(b) => b,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                };
                match avail.iter().position(|&b| b == b'\n') {
                    Some(i) => {
                        reader.consume(i + 1);
                        return Ok(ReadOutcome::Oversized);
                    }
                    None => {
                        let n = avail.len();
                        reader.consume(n);
                    }
                }
            }
        }
        buf.extend_from_slice(&available[..used]);
        reader.consume(used);
        if done {
            return Ok(ReadOutcome::Line(buf.len()));
        }
    }
}

/// Write a `pretooluse_settings.json` into `hook_dir` that installs the
/// `ccom-hook-pretooluse` binary as a PreToolUse hook for every tool.
///
/// The file is loaded by passing an additional `--settings pretooluse_settings.json`
/// flag on the Claude command line. Claude Code supports multiple `--settings`
/// flags; each one is layered on top of the previous.
///
/// The `cmd` argument is the full command string for the hook binary (normally
/// the absolute path to `ccom-hook-pretooluse`, but can be overridden in tests
/// via `CCOM_TEST_PRETOOLUSE_HOOK_CMD`).
///
/// File is written mode 0600 via `create_new` to match the security properties
/// of the main `settings.json`.
#[cfg(unix)]
pub fn write_pretooluse_settings(hook_dir: &Path, cmd: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let path = hook_dir.join("pretooluse_settings.json");
    let contents = build_pretooluse_settings_json(cmd);

    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
pub fn write_pretooluse_settings(_hook_dir: &Path, _cmd: &str) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "pretooluse hook is only supported on Unix",
    ))
}

/// Build the `pretooluse_settings.json` body. Split from the file writer
/// so it can be unit tested without filesystem access.
pub(crate) fn build_pretooluse_settings_json(cmd: &str) -> String {
    serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {
                    "hooks": [
                        {
                            "type": "command",
                            "command": cmd,
                            "timeout": 600
                        }
                    ]
                }
            ]
        }
    })
    .to_string()
}

/// Recursively remove the hook directory. Best-effort; logs on
/// failure. Called from `Session::kill` / `reap_exited`.
///
/// **Symlink safety:** On Rust ≥ 1.70, `fs::remove_dir_all` does not
/// follow symlinks — it calls `unlinkat` on symlink entries rather
/// than recursing into their targets. A future refactor must NOT
/// re-introduce a pre-1.70-style hand-rolled recursive walker, which
/// would re-open the CVE-2022-21658 class of bug (symlink in the dir
/// pointing at a sensitive target). See review issue H2.
pub fn cleanup_hook_dir(hook_dir: &Path) {
    #[cfg(unix)]
    {
        if let Err(e) = std::fs::remove_dir_all(hook_dir) {
            // ENOENT is fine — already cleaned up.
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("failed to remove hook dir {}: {e}", hook_dir.display());
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = hook_dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretooluse_settings_has_correct_shape() {
        let json = build_pretooluse_settings_json("/usr/local/bin/ccom-hook-pretooluse");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

        // Must have hooks.PreToolUse array
        let hooks_arr = &parsed["hooks"]["PreToolUse"];
        assert!(hooks_arr.is_array(), "PreToolUse must be an array");
        let hook_entry = &hooks_arr[0]["hooks"][0];
        assert_eq!(hook_entry["type"].as_str().unwrap(), "command");
        assert_eq!(
            hook_entry["command"].as_str().unwrap(),
            "/usr/local/bin/ccom-hook-pretooluse"
        );
        assert_eq!(hook_entry["timeout"].as_u64().unwrap(), 600);
    }

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
        assert_eq!(signal.claude_session_id.as_deref(), Some("abc-123"));
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
    fn parse_stdin_rejects_wrong_type_last_message() {
        // last_assistant_message present but not a string → None,
        // and we log-warn (issue M6). Here we just check the None
        // return; log output is side-effectful but non-fatal.
        let json = r#"{"session_id":"x","last_assistant_message":42}"#;
        assert!(parse_hook_stdin(json, 1).is_none());
        let json = r#"{"session_id":"x","last_assistant_message":null}"#;
        assert!(parse_hook_stdin(json, 1).is_none());
        let json = r#"{"session_id":"x","last_assistant_message":["a","b"]}"#;
        assert!(parse_hook_stdin(json, 1).is_none());
    }

    #[test]
    fn build_hook_settings_contains_command_referencing_fifo() {
        let fifo = Path::new("/tmp/test-fifo");
        let settings = build_hook_settings(fifo).expect("utf8 path");
        let parsed: serde_json::Value = serde_json::from_str(&settings).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("/tmp/test-fifo"));
        // Bump from 5 to 30 (issue M2).
        assert_eq!(
            parsed["hooks"]["Stop"][0]["hooks"][0]["timeout"]
                .as_u64()
                .unwrap(),
            30
        );
    }

    #[test]
    fn shell_single_quote_handles_tricky_chars() {
        assert_eq!(shell_single_quote("simple"), "'simple'");
        assert_eq!(shell_single_quote("with space"), "'with space'");
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_single_quote("$x`y`"), "'$x`y`'");
    }

    #[test]
    fn build_hook_settings_escapes_tricky_paths() {
        // Path contains space, single quote, dollar, backtick.
        let p = PathBuf::from("/tmp/weird dir's $x`y`/stop.fifo");
        let settings = build_hook_settings(&p).expect("utf8 path");
        let parsed: serde_json::Value = serde_json::from_str(&settings).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();
        // Should not contain the raw unescaped `'`; check the quoted form is present.
        assert!(command.contains("'/tmp/weird dir'\\''s $x`y`/stop.fifo'"));
    }

    #[cfg(unix)]
    #[test]
    fn create_hook_dir_writes_settings() {
        let dir = create_hook_dir(999_999_001).expect("should create dir");
        let settings_path = dir.join("settings.json");
        assert!(settings_path.exists());
        let contents = std::fs::read_to_string(&settings_path).unwrap();
        assert!(contents.contains("Stop"));
        assert!(contents.contains("stop.fifo"));
        cleanup_hook_dir(&dir);
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_hook_dir_sets_secure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = create_hook_dir(999_999_010).expect("create dir");
        let root_mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(root_mode & 0o777, 0o700, "root dir should be 0700");
        let settings_mode = std::fs::metadata(dir.join("settings.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(settings_mode & 0o777, 0o600, "settings.json should be 0600");
        cleanup_hook_dir(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn create_hook_dir_does_not_touch_claude_config_dir() {
        // Regression guard for the `CLAUDE_CONFIG_DIR`-keychain bug:
        // the flat layout must NOT create a `.claude/` subdir, and
        // must not symlink anything from the user's real config dir.
        let dir = create_hook_dir(999_999_012).expect("create dir");
        assert!(
            !dir.join(".claude").exists(),
            "hook dir must not contain a .claude/ subdir (would force CLAUDE_CONFIG_DIR override)"
        );
        cleanup_hook_dir(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_preserves_symlink_targets() {
        // Regression test for review issue H2: fs::remove_dir_all
        // (Rust ≥1.70) must not follow symlinks inside the hook dir.
        let dir = create_hook_dir(999_999_011).expect("create dir");
        // Create a sensitive "outside" file.
        let outside =
            std::env::temp_dir().join(format!("ccom-h2-outside-{}-{}", process::id(), 999_999_011));
        std::fs::write(&outside, b"do not delete me").expect("write outside");
        // Create a symlink inside the hook dir pointing at it.
        let link = dir.join("danger-link");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        assert!(link.exists());

        cleanup_hook_dir(&dir);
        assert!(!dir.exists(), "hook dir should be gone");
        assert!(outside.exists(), "symlink target must survive cleanup");
        assert_eq!(std::fs::read(&outside).unwrap(), b"do not delete me");
        let _ = std::fs::remove_file(&outside);
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
            let mut f = std::fs::OpenOptions::new()
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

    #[test]
    fn parse_stdin_one_mb_body() {
        // Issue T6: round-trip a 1 MB last_assistant_message.
        let body = "x".repeat(1024 * 1024);
        let v = serde_json::json!({
            "session_id": "big",
            "last_assistant_message": body,
        });
        let json = v.to_string();
        let signal = parse_hook_stdin(&json, 123).expect("parse");
        assert_eq!(signal.ccom_session_id, 123);
        assert_eq!(signal.last_assistant_message.len(), 1024 * 1024);
        assert!(signal.last_assistant_message.chars().all(|c| c == 'x'));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_path_with_space_and_quote() {
        // Issue T7: verify the build_hook_settings escaping works
        // end-to-end by shell-execing the emitted command against a
        // real FIFO whose path contains a space and a single quote.
        let root = create_hook_dir(999_999_020).expect("create dir");
        let weird = root.join("weird 'dir");
        std::fs::create_dir(&weird).expect("mkdir weird");
        let fifo_path = weird.join("stop.fifo");
        create_stop_fifo(&fifo_path).expect("mkfifo");

        let (_handle, rx) = spawn_fifo_reader(fifo_path.clone(), 17).expect("spawn reader");

        // Build the command the hook would run.
        let settings_json = build_hook_settings(&fifo_path).expect("utf8 path");
        let parsed: serde_json::Value = serde_json::from_str(&settings_json).unwrap();
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_string();

        // Shell-exec it with piped stdin (the JSON blob).
        let json_blob = r#"{"session_id":"weird","last_assistant_message":"hi from weird"}"#;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        {
            use std::io::Write;
            let stdin = child.stdin.as_mut().unwrap();
            stdin.write_all(json_blob.as_bytes()).unwrap();
        }
        child.wait().expect("wait sh");

        let signal = rx
            .recv_timeout(Duration::from_secs(3))
            .expect("receive signal");
        assert_eq!(signal.ccom_session_id, 17);
        assert_eq!(signal.last_assistant_message, "hi from weird");

        cleanup_hook_dir(&root);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_skips_oversized_line_then_parses_next() {
        // Issue T8: write a >16 MB line, then a valid line; reader
        // should skip the first with a warning and still deliver the
        // second signal.
        let dir = create_hook_dir(999_999_030).expect("create dir");
        let fifo_path = dir.join("stop.fifo");
        create_stop_fifo(&fifo_path).expect("mkfifo");

        let (_handle, rx) = spawn_fifo_reader(fifo_path.clone(), 31).expect("spawn reader");

        let fifo_for_writer = fifo_path.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo_for_writer)
                .expect("open fifo for writing");
            // ~17 MB of garbage with NO newline until the end.
            let chunk = vec![b'A'; 1024 * 1024];
            for _ in 0..17 {
                f.write_all(&chunk).unwrap();
            }
            f.write_all(b"\n").unwrap();
            // Now a valid line.
            let good = br#"{"session_id":"ok","last_assistant_message":"recovered"}"#;
            f.write_all(good).unwrap();
            f.write_all(b"\n").unwrap();
        });

        let signal = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("should recover and receive second signal");
        assert_eq!(signal.ccom_session_id, 31);
        assert_eq!(signal.last_assistant_message, "recovered");

        cleanup_hook_dir(&dir);
    }

    // ------------------------------------------------------------------
    // Phase 6 Task 3 — `.mcp.json` JSON shape for the caller-id header.
    // ------------------------------------------------------------------

    #[test]
    fn mcp_config_without_caller_id_has_no_headers_block() {
        let json = build_mcp_config_json(1234, None);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let server = &parsed["mcpServers"]["ccom"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://127.0.0.1:1234/mcp");
        assert!(
            server.get("headers").is_none(),
            "headers block must be absent when caller_id is None: {json}"
        );
    }

    #[test]
    fn mcp_config_with_caller_id_injects_header() {
        let json = build_mcp_config_json(4321, Some(42));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let server = &parsed["mcpServers"]["ccom"];
        assert_eq!(server["url"], "http://127.0.0.1:4321/mcp");
        assert_eq!(server["headers"]["X-Ccom-Caller"], "42");
    }
}
