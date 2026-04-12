//! Phase 4 Task 8 — End-to-end integration test for the embedded MCP
//! server.
//!
//! Spins up a real `McpServer` on loopback, connects over plain HTTP
//! using `ureq` (a tiny sync client) with hand-rolled MCP Streamable
//! HTTP framing, and exercises the three read-only tools end-to-end.
//!
//! The MCP wire protocol used here is:
//!   * `POST /mcp` with `Accept: application/json, text/event-stream`
//!     and `Content-Type: application/json`
//!   * The initialize response carries an `Mcp-Session-Id` header
//!     that must be echoed on subsequent requests.
//!   * Bodies are SSE-framed — the JSON-RPC payload lives on a line
//!     beginning with `data: `.
//!
//! See `docs/plans/notes/rmcp-spike.md` §"Test script used for
//! verification" for the empirically verified request shapes this
//! test is built against.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ccom::mcp::McpServer;
use ccom::session::{EventBus, Session, SessionManager};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// HTTP/SSE helper
// ---------------------------------------------------------------------------

/// A live MCP session against a loopback server. Holds the port and
/// the `Mcp-Session-Id` allocated by `initialize` so subsequent
/// `call` invocations can reuse it.
struct McpClient {
    base_url: String,
    session_id: String,
}

impl McpClient {
    /// Send `initialize` and capture the session id the server allocates.
    /// Returns the parsed JSON-RPC `result` object on success.
    fn initialize(port: u16) -> (Self, Value) {
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

        // ureq lowercases header names internally; try both spellings.
        let session_id = resp
            .header("mcp-session-id")
            .or_else(|| resp.header("Mcp-Session-Id"))
            .expect("server did not return Mcp-Session-Id header")
            .to_string();

        let raw = resp.into_string().expect("initialize body read");
        let value = parse_sse_jsonrpc(&raw).expect("parse initialize SSE body");
        let result = value
            .get("result")
            .cloned()
            .expect("initialize response had no result");

        (
            Self {
                base_url,
                session_id,
            },
            result,
        )
    }

    /// Send an arbitrary JSON-RPC method on the existing MCP session.
    /// Returns the full decoded JSON-RPC envelope (caller picks out
    /// `result` / `error` as needed).
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

/// Extract the last `data:` JSON payload from an SSE-framed response body.
/// rmcp's `StreamableHttpService` emits envelopes like:
///
/// ```text
/// event: message
/// data: {"jsonrpc":"2.0","id":1,"result":{...}}
/// ```
fn parse_sse_jsonrpc(raw: &str) -> Option<Value> {
    // Walk lines in reverse so we prefer the final payload if the
    // server emitted a prelude line first.
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
    // Fallback: maybe the body is raw JSON (json_response mode).
    serde_json::from_str::<Value>(raw.trim()).ok()
}

// ---------------------------------------------------------------------------
// Fixture builders
// ---------------------------------------------------------------------------

fn empty_manager() -> (Arc<Mutex<SessionManager>>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new());
    let mgr = SessionManager::with_bus(Arc::clone(&bus));
    (Arc::new(Mutex::new(mgr)), bus)
}

fn manager_with_two_dummies() -> (Arc<Mutex<SessionManager>>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let id_a = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(id_a, "alpha"));
    let id_b = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(id_b, "beta"));
    (Arc::new(Mutex::new(mgr)), bus)
}

fn manager_with_one_dummy() -> (Arc<Mutex<SessionManager>>, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(id, "solo"));
    (Arc::new(Mutex::new(mgr)), bus)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn server_starts_and_exposes_three_tools() {
    let (sessions, bus) = empty_manager();
    let server = McpServer::start_with(sessions, bus).expect("server start");
    assert!(server.port() > 0, "expected bound port");

    let (client, init_result) = McpClient::initialize(server.port());
    assert!(
        init_result.get("serverInfo").is_some(),
        "initialize result missing serverInfo: {init_result}"
    );

    let resp = client.call(2, "tools/list", json!({}));
    assert!(
        resp.get("error").is_none(),
        "tools/list returned error: {resp}"
    );
    let tools = resp
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("tools/list missing /result/tools: {resp}"));
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .collect();
    for expected in ["list_sessions", "read_response", "subscribe"] {
        assert!(
            names.contains(&expected),
            "tools/list missing {expected}: got {names:?}"
        );
    }

    server.stop();
}

#[test]
fn list_sessions_returns_empty_for_empty_manager() {
    let (sessions, bus) = empty_manager();
    let server = McpServer::start_with(sessions, bus).expect("server start");
    let (client, _) = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({"name": "list_sessions", "arguments": {}}),
    );
    assert!(
        resp.get("error").is_none(),
        "list_sessions returned error: {resp}"
    );

    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("list_sessions: missing content text: {resp}"));
    let parsed: Value = serde_json::from_str(text).expect("list_sessions text is JSON");
    let arr = parsed.as_array().expect("list_sessions text is array");
    assert!(arr.is_empty(), "expected empty list, got {parsed}");

    server.stop();
}

#[test]
fn list_sessions_returns_populated_sessions() {
    let (sessions, bus) = manager_with_two_dummies();
    let server = McpServer::start_with(sessions, bus).expect("server start");
    let (client, _) = McpClient::initialize(server.port());

    let resp = client.call(
        2,
        "tools/call",
        json!({"name": "list_sessions", "arguments": {}}),
    );
    assert!(
        resp.get("error").is_none(),
        "list_sessions returned error: {resp}"
    );

    let text = resp
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .expect("list_sessions content text");
    let parsed: Value = serde_json::from_str(text).expect("list_sessions JSON");
    let arr = parsed.as_array().expect("array");
    assert_eq!(arr.len(), 2, "expected 2 sessions, got {parsed}");
    let labels: Vec<&str> = arr
        .iter()
        .filter_map(|s| s.get("label").and_then(Value::as_str))
        .collect();
    assert!(labels.contains(&"alpha"), "labels={labels:?}");
    assert!(labels.contains(&"beta"), "labels={labels:?}");

    server.stop();
}

#[test]
fn read_response_timeout_for_unknown_turn() {
    let (sessions, bus) = manager_with_one_dummy();
    let server = McpServer::start_with(sessions, bus).expect("server start");
    let (client, _) = McpClient::initialize(server.port());

    // The dummy session has id 0 and no stored turns. Ask for turn 99
    // with a 1-second timeout — the handler should long-poll briefly
    // and then return an internal_error.
    let resp = client.call(
        2,
        "tools/call",
        json!({
            "name": "read_response",
            "arguments": {
                "session_id": 0,
                "turn_id": 99,
                "timeout_secs": 1
            }
        }),
    );

    // rmcp surfaces tool errors via either the JSON-RPC `error`
    // envelope or a `result` with `isError: true` — accept both.
    let has_jsonrpc_error = resp.get("error").is_some();
    let has_tool_error = resp
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    assert!(
        has_jsonrpc_error || has_tool_error,
        "expected error for unknown turn, got: {resp}"
    );

    // The message should mention timeout or internal_error somewhere
    // in the envelope so we catch regressions in the error path.
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("timeout") || blob.contains("internal_error") || blob.contains("internal"),
        "error message lacked timeout/internal marker: {resp}"
    );

    server.stop();
}
