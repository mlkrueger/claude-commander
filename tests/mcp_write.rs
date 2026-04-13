//! Phase 5 Task 1 — End-to-end integration tests for the
//! `send_prompt` MCP write tool.
//!
//! Uses the same hand-rolled `ureq` + SSE client helper as
//! `tests/mcp_readonly.rs` and exercises `send_prompt` against a real
//! `/bin/cat` PTY-backed session. The assertions verify:
//!
//! * Clean text reaches the PTY (cat echoes it back).
//! * ANSI escape sequences are stripped before the PTY write.
//! * C0 control chars are stripped before the PTY write.
//! * Unknown session ids are rejected with a tool-level error.
//! * Empty and oversized text are rejected before dispatch.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ccom::event::{Event, MonitoredSender};
use ccom::mcp::McpServer;
use ccom::session::{EventBus, SessionManager, SpawnConfig};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// HTTP/SSE helper (copy-paste from tests/mcp_readonly.rs — factoring
// into a shared helper would require a new tests/common/ crate and
// isn't worth it for two tests files).
// ---------------------------------------------------------------------------

struct McpClient {
    base_url: String,
    session_id: String,
}

impl McpClient {
    fn initialize(port: u16) -> Self {
        let base_url = format!("http://127.0.0.1:{port}/mcp");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ccom-it", "version": "0"}
            }
        })
        .to_string();

        let resp = ureq::post(&base_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .timeout(Duration::from_secs(5))
            .send_string(&body)
            .expect("initialize POST");

        let session_id = resp
            .header("mcp-session-id")
            .or_else(|| resp.header("Mcp-Session-Id"))
            .expect("server did not return Mcp-Session-Id header")
            .to_string();

        // Drain the response body so the connection can be reused.
        let _ = resp.into_string();

        Self {
            base_url,
            session_id,
        }
    }

    fn call(&self, id: u64, method: &str, params: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();

        let resp = ureq::post(&self.base_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Mcp-Session-Id", &self.session_id)
            .timeout(Duration::from_secs(10))
            .send_string(&body)
            .unwrap_or_else(|e| panic!("{method} POST failed: {e}"));

        let raw = resp
            .into_string()
            .unwrap_or_else(|e| panic!("{method} body read failed: {e}"));
        parse_sse_jsonrpc(&raw)
            .unwrap_or_else(|| panic!("{method}: could not parse SSE body: {raw:?}"))
    }
}

fn parse_sse_jsonrpc(raw: &str) -> Option<Value> {
    for line in raw.lines().rev() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let trimmed = rest.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            return Some(v);
        }
    }
    serde_json::from_str::<Value>(raw.trim()).ok()
}

// ---------------------------------------------------------------------------
// Fixture: a real `/bin/cat` session owned by a live SessionManager.
// ---------------------------------------------------------------------------

/// Handle returned by [`spawn_cat_fixture`]. Owns the `SessionManager`
/// under an `Arc<Mutex<…>>` so it can be both handed to the MCP server
/// and locked by the test to drain events / kill the child at the end.
struct CatFixture {
    sessions: Arc<Mutex<SessionManager>>,
    bus: Arc<EventBus>,
    event_rx: std::sync::mpsc::Receiver<Event>,
    session_id: usize,
}

impl CatFixture {
    fn spawn() -> Self {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

        let (raw_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx = MonitoredSender::wrap(raw_tx);
        let session_id = mgr
            .spawn(SpawnConfig {
                label: "phase5-send-prompt".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn cat");

        Self {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            event_rx,
            session_id,
        }
    }

    /// Drain `event_rx` for up to `timeout` waiting for `PtyOutput`
    /// bytes on `self.session_id` whose concatenation contains
    /// `needle`. Returns the accumulated output on success, or panics
    /// with the full buffer contents on timeout.
    fn wait_for_pty_contains(&self, needle: &str, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut buf: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            while let Ok(ev) = self.event_rx.try_recv() {
                if let Event::PtyOutput { session_id, data } = ev
                    && session_id == self.session_id
                {
                    buf.extend_from_slice(&data);
                }
            }
            if twoway_contains(&buf, needle.as_bytes()) {
                return buf;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out after {:?} waiting for {:?} in PTY output; got {:?}",
            timeout,
            needle,
            String::from_utf8_lossy(&buf)
        );
    }

    /// Drain `event_rx` for `settle` duration and return every
    /// `PtyOutput` byte seen. Useful for verifying a forbidden byte is
    /// NOT present.
    fn drain_pty_for(&self, settle: Duration) -> Vec<u8> {
        let deadline = Instant::now() + settle;
        let mut buf: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            while let Ok(ev) = self.event_rx.try_recv() {
                if let Event::PtyOutput { session_id, data } = ev
                    && session_id == self.session_id
                {
                    buf.extend_from_slice(&data);
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        buf
    }

    fn kill(&self) {
        let mut mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        mgr.kill(self.session_id);
    }
}

fn twoway_contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn tool_is_error(resp: &Value) -> bool {
    resp.get("error").is_some()
        || resp
            .pointer("/result/isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn tool_result_text(resp: &Value) -> String {
    resp.pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing /result/content/0/text: {resp}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn send_prompt_delivers_bytes_to_cat_session() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": fixture.session_id,
                "text": "ccom-phase5-smoke"
            }
        }),
    );
    assert!(!tool_is_error(&resp), "send_prompt errored: {resp}");

    // Parse the wire payload — must be `{"turn_id": <u64>}`.
    let text = tool_result_text(&resp);
    let parsed: Value = serde_json::from_str(&text).expect("send_prompt text is JSON");
    assert!(
        parsed.get("turn_id").and_then(Value::as_u64).is_some(),
        "expected turn_id in payload, got {parsed}"
    );

    // Cat echoes input → PTY reader → Event::PtyOutput → event_rx.
    fixture.wait_for_pty_contains("ccom-phase5-smoke", Duration::from_secs(3));

    fixture.kill();
    server.stop();
}

#[test]
fn send_prompt_strips_ansi_escapes_before_write() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    // Raw ESC byte (0x1b) embedded in a JSON string. serde_json emits
    // `\u001b` which rmcp deserializes back to 0x1b before the tool
    // body runs — giving us a fully end-to-end sanitizer check.
    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": fixture.session_id,
                "text": "hello\u{1b}[31mred\u{1b}[0m"
            }
        }),
    );
    assert!(!tool_is_error(&resp), "send_prompt errored: {resp}");

    // Wait until the echoed "hellored" lands, then assert no ESC byte
    // ever showed up in the accumulated buffer.
    let seen = fixture.wait_for_pty_contains("hellored", Duration::from_secs(3));
    assert!(
        !seen.contains(&0x1b),
        "ESC byte leaked into PTY output: {seen:?}"
    );

    fixture.kill();
    server.stop();
}

#[test]
fn send_prompt_strips_control_chars_before_write() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": fixture.session_id,
                "text": "hello\u{01}\u{02}world"
            }
        }),
    );
    assert!(!tool_is_error(&resp), "send_prompt errored: {resp}");

    let seen = fixture.wait_for_pty_contains("helloworld", Duration::from_secs(3));
    assert!(
        !seen.contains(&0x01) && !seen.contains(&0x02),
        "control bytes leaked into PTY output: {seen:?}"
    );

    fixture.kill();
    server.stop();
}

#[test]
fn send_prompt_not_found_for_unknown_session() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": 999,
                "text": "ping"
            }
        }),
    );
    assert!(
        tool_is_error(&resp),
        "expected error for unknown session: {resp}"
    );
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("not found"),
        "error blob lacked 'not found': {resp}"
    );

    // And no bytes should have reached the real session.
    let seen = fixture.drain_pty_for(Duration::from_millis(200));
    assert!(
        !twoway_contains(&seen, b"ping"),
        "unexpected bytes delivered: {seen:?}"
    );

    fixture.kill();
    server.stop();
}

#[test]
fn send_prompt_rejects_empty_text() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": fixture.session_id,
                "text": ""
            }
        }),
    );
    assert!(
        tool_is_error(&resp),
        "expected error for empty text: {resp}"
    );
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(blob.contains("empty"), "error blob lacked 'empty': {resp}");

    fixture.kill();
    server.stop();
}

#[test]
fn send_prompt_rejects_oversized_text() {
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("server start");
    let client = McpClient::initialize(server.port());

    // 20 KB of 'a'. The sanitizer caps at 16 KB.
    let big = "a".repeat(20 * 1024);
    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "send_prompt",
            "arguments": {
                "session_id": fixture.session_id,
                "text": big,
            }
        }),
    );
    assert!(
        tool_is_error(&resp),
        "expected error for oversized text: {resp}"
    );
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("too large"),
        "error blob lacked 'too large': {resp}"
    );

    fixture.kill();
    server.stop();
}

// ---------------------------------------------------------------------------
// Phase 5 Task 3: `kill_session` tool — exercises the cross-thread
// ConfirmBridge. Each test spawns a helper thread that simulates the
// main TUI's modal-drain loop, answering each ConfirmRequest with the
// specified response.
// ---------------------------------------------------------------------------

use ccom::mcp::{ConfirmRequest, ConfirmResponse, ConfirmTool};

/// Spawn a background thread that drains `confirm_rx` and answers
/// every request with the given response. Returns a `JoinHandle`
/// the test can join after `server.stop()`.
fn spawn_auto_responder(
    rx: std::sync::mpsc::Receiver<ConfirmRequest>,
    response: ConfirmResponse,
) -> std::thread::JoinHandle<Vec<(ConfirmTool, usize)>> {
    std::thread::spawn(move || {
        let mut observed: Vec<(ConfirmTool, usize)> = Vec::new();
        // Loop until the sender side is dropped (server shutdown).
        while let Ok(req) = rx.recv() {
            observed.push((req.tool, req.session_id));
            let _ = req.resp_tx.send(response);
        }
        observed
    })
}

#[test]
fn kill_session_waits_for_confirmation_and_allow_kills_session() {
    let fixture = CatFixture::spawn();
    let (server, confirm_rx) =
        McpServer::start_with_confirm(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
            .expect("start");

    // Simulate main-thread modal: answer every request with Allow.
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        10,
        "tools/call",
        json!({
            "name": "kill_session",
            "arguments": {"session_id": fixture.session_id}
        }),
    );

    assert!(!tool_is_error(&resp), "expected success: {resp}");
    let text = tool_result_text(&resp);
    assert!(text.contains("killed"), "unexpected success body: {text}");

    // `SessionManager::kill` removes the session entirely, not just
    // marks it exited. Verify it's gone from the manager.
    {
        let mgr = fixture.sessions.lock().unwrap_or_else(|p| p.into_inner());
        assert!(
            mgr.get(fixture.session_id).is_none(),
            "session {} should have been removed from the manager after kill_session",
            fixture.session_id
        );
    }

    // Shut down the server so `confirm_rx`'s sender drops and the
    // responder thread exits.
    server.stop();
    let observed = responder.join().expect("responder thread");
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].0, ConfirmTool::KillSession);
    assert_eq!(observed[0].1, fixture.session_id);
}

#[test]
fn kill_session_denied_leaves_session_alive() {
    let fixture = CatFixture::spawn();
    let (server, confirm_rx) =
        McpServer::start_with_confirm(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
            .expect("start");

    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Deny);

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        11,
        "tools/call",
        json!({
            "name": "kill_session",
            "arguments": {"session_id": fixture.session_id}
        }),
    );

    assert!(tool_is_error(&resp), "expected tool error on deny: {resp}");
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("denied"),
        "error blob lacked 'denied': {resp}"
    );

    // Verify the session is still alive (Running, not Exited).
    {
        let mgr = fixture.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let session = mgr.get(fixture.session_id).expect("still tracked");
        assert!(
            !matches!(session.status, ccom::session::SessionStatus::Exited(_)),
            "session should NOT be Exited after deny, got {:?}",
            session.status
        );
    }

    // Clean up: kill the live session, then stop the server.
    fixture.kill();
    server.stop();
    let _ = responder.join();
}

#[test]
fn kill_session_not_found_for_unknown_session() {
    let fixture = CatFixture::spawn();
    let (server, confirm_rx) =
        McpServer::start_with_confirm(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
            .expect("start");

    // Responder should never be asked anything — unknown sessions are
    // rejected before confirmation.
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        12,
        "tools/call",
        json!({
            "name": "kill_session",
            "arguments": {"session_id": 9_999_999usize}
        }),
    );

    assert!(tool_is_error(&resp), "expected NotFound error: {resp}");
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("not found"),
        "error blob lacked 'not found': {resp}"
    );

    fixture.kill();
    server.stop();
    let observed = responder.join().expect("responder");
    assert!(
        observed.is_empty(),
        "responder should never have been asked for unknown session, got: {observed:?}"
    );
}

#[test]
fn kill_session_no_bridge_auto_denies() {
    // Sanity check the fallback path: `McpServer::start_with` builds
    // an McpCtx with `confirm: None`, so kill_session should
    // auto-deny without hanging.
    let fixture = CatFixture::spawn();
    let server = McpServer::start_with(Arc::clone(&fixture.sessions), Arc::clone(&fixture.bus))
        .expect("start");

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        13,
        "tools/call",
        json!({
            "name": "kill_session",
            "arguments": {"session_id": fixture.session_id}
        }),
    );

    assert!(tool_is_error(&resp), "expected deny without bridge: {resp}");
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("denied") || blob.contains("no confirm bridge"),
        "unexpected error body: {resp}"
    );

    // Session should still be alive.
    {
        let mgr = fixture.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let session = mgr.get(fixture.session_id).expect("still tracked");
        assert!(!matches!(
            session.status,
            ccom::session::SessionStatus::Exited(_)
        ));
    }

    fixture.kill();
    server.stop();
}
