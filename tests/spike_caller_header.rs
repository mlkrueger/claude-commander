//! Phase 6 Task 0 — caller-id spike.
//!
//! Validates the approach the Phase 6 plan proposes for identifying
//! which ccom session is making an MCP tool call: a custom HTTP
//! header (`X-Ccom-Caller`) written into the spawned session's
//! `.mcp.json` by `Session::spawn`, propagated by Claude Code on
//! every tool-call POST, and read back inside the `#[tool]` handler
//! via `ctx.extensions.get::<http::request::Parts>()` (the pattern
//! documented in rmcp 1.4's `StreamableHttpService` rustdoc).
//!
//! The spike proves TWO things:
//!   1. rmcp 1.4 surfaces arbitrary custom request headers into
//!      tool-handler `RequestContext::extensions` — not just the
//!      well-known `Mcp-Session-Id` already exercised by Phase 4.
//!   2. A missing header produces the expected fallback sentinel
//!      (so the Phase 6 handler can distinguish "driver caller" from
//!      "unknown caller" without panicking).
//!
//! The spike does NOT exercise Claude Code's own propagation of
//! `.mcp.json` "headers" blocks — that is verified separately with a
//! manual smoke test (record the result in the same scratch doc the
//! rmcp spike used). The rmcp side is the part we can fully automate.
//!
//! If this test passes, Phase 6 Task 3's handler design is validated
//! and we can merge the spike + start Phase 6 for real.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ccom::mcp::McpServer;
use ccom::session::{EventBus, SessionManager};
use serde_json::{Value, json};

struct McpClient {
    base_url: String,
    session_id: String,
    caller_header: Option<String>,
}

impl McpClient {
    /// Initialize against the server, optionally passing the
    /// `X-Ccom-Caller` header on the initialize POST. The header is
    /// then re-sent on every subsequent `call` so every tool
    /// invocation carries it — matching what Claude Code would do
    /// once the header is declared in `.mcp.json`.
    fn initialize(port: u16, caller_header: Option<&str>) -> (Self, Value) {
        let base_url = format!("http://127.0.0.1:{port}/mcp");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ccom-spike", "version": "0"}
            }
        })
        .to_string();

        let mut req = ureq::post(&base_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .timeout(Duration::from_secs(5));
        if let Some(v) = caller_header {
            req = req.set("X-Ccom-Caller", v);
        }
        let resp = req.send_string(&body).expect("initialize POST");

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
                caller_header: caller_header.map(|s| s.to_string()),
            },
            result,
        )
    }

    fn call(&self, id: u64, method: &str, params: Value) -> Value {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();

        let mut req = ureq::post(&self.base_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Mcp-Session-Id", &self.session_id)
            .timeout(Duration::from_secs(10));
        if let Some(v) = self.caller_header.as_deref() {
            req = req.set("X-Ccom-Caller", v);
        }
        let resp = req
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

fn empty_server() -> McpServer {
    let bus = Arc::new(EventBus::new());
    let mgr = SessionManager::with_bus(Arc::clone(&bus));
    McpServer::start_with(Arc::new(Mutex::new(mgr)), bus).expect("server start")
}

/// Extract the text body out of a CallToolResult JSON-RPC `result`.
fn tool_text(result: &Value) -> String {
    let content = result
        .get("content")
        .and_then(|v| v.as_array())
        .expect("tool result had no content array");
    let first = content.first().expect("content array was empty");
    first
        .get("text")
        .and_then(|v| v.as_str())
        .expect("content entry had no text field")
        .to_string()
}

#[test]
fn custom_header_propagates_to_tool_handler_via_request_context_extensions() {
    let server = empty_server();
    let (client, _init) = McpClient::initialize(server.port(), Some("42"));

    let envelope = client.call(
        2,
        "tools/call",
        json!({
            "name": "_caller_probe",
            "arguments": {},
        }),
    );

    let result = envelope
        .get("result")
        .unwrap_or_else(|| panic!("no result in envelope: {envelope:?}"));
    let text = tool_text(result);
    assert_eq!(
        text, "42",
        "probe should echo the X-Ccom-Caller header verbatim; got {text:?}"
    );

    server.stop();
}

#[test]
fn missing_caller_header_returns_sentinel() {
    let server = empty_server();
    let (client, _init) = McpClient::initialize(server.port(), None);

    let envelope = client.call(
        2,
        "tools/call",
        json!({
            "name": "_caller_probe",
            "arguments": {},
        }),
    );

    let result = envelope
        .get("result")
        .unwrap_or_else(|| panic!("no result in envelope: {envelope:?}"));
    let text = tool_text(result);
    assert_eq!(
        text, "<missing>",
        "probe should return the missing sentinel; got {text:?}"
    );

    server.stop();
}

#[test]
fn multibyte_header_value_preserved() {
    // HTTP header values are supposed to be ISO-8859-1, but real
    // clients (including Claude Code) send UTF-8 in practice and
    // rmcp/http-1 tolerates it. This test pins the assumption that
    // a printable ASCII caller id — which is all Phase 6 will ever
    // write — survives the round-trip without mangling.
    let server = empty_server();
    let (client, _init) = McpClient::initialize(server.port(), Some("ccom-session-99"));

    let envelope = client.call(
        2,
        "tools/call",
        json!({
            "name": "_caller_probe",
            "arguments": {},
        }),
    );
    let result = envelope.get("result").expect("no result");
    assert_eq!(tool_text(result), "ccom-session-99");

    server.stop();
}
