//! Phase 6 Tasks 3 + 6 — end-to-end integration tests for the
//! `spawn_session` MCP tool and the driver-kill policy on `kill_session`.
//!
//! These tests use a hand-rolled `ureq` + SSE client (same shape as
//! `tests/mcp_write.rs`) with the `X-Ccom-Caller` header so they can
//! impersonate a driver caller and exercise the caller-scope logic.
//!
//! The driver fixture uses `Session::dummy_exited(...).with_role(...)`
//! pushed via `SessionManager::push_for_test` so no real Claude
//! binary is required to construct the driver itself. For the
//! `spawn_session` child, the tests set the `CCOM_TEST_SPAWN_CMD`
//! environment variable to `/bin/cat` — the handler reads this
//! override to stand in for the real Claude launcher during testing.
//!
//! **Serialization:** every test in this file uses the env var and
//! must NOT run in parallel with another test that depends on its
//! value being unset. Rust's `cargo test` runs integration tests in
//! parallel by default, but env-var-based test seams inside a single
//! test binary are shared process state. Because every test in this
//! file sets the same value (`/bin/cat`) and never unsets it, parallel
//! execution is safe for THIS file alone. Do not add tests that
//! depend on the override being absent.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use ccom::event::MonitoredSender;
use ccom::mcp::{ConfirmRequest, ConfirmResponse, ConfirmTool, McpServer};
use ccom::session::{EventBus, Session, SessionManager, SessionRole, SpawnPolicy};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Process-wide env var setup. `std::env::set_var` is not thread-safe
// in general, but a `Once` makes the first-touch idempotent.
// ---------------------------------------------------------------------------

fn ensure_test_spawn_cmd() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        // Safety: set exactly once before any test body runs.
        // `/bin/cat` is the same stand-in used by `tests/mcp_write.rs`'s
        // `CatFixture`.
        unsafe {
            std::env::set_var("CCOM_TEST_SPAWN_CMD", "/bin/cat");
        }
    });
}

// ---------------------------------------------------------------------------
// MCP client helper with X-Ccom-Caller support (copied from the spike).
// ---------------------------------------------------------------------------

struct McpClient {
    base_url: String,
    session_id: String,
    caller_header: Option<String>,
}

impl McpClient {
    fn initialize(port: u16, caller_header: Option<&str>) -> Self {
        let base_url = format!("http://127.0.0.1:{port}/mcp");
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "ccom-driver-it", "version": "0"}
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
            .expect("no Mcp-Session-Id")
            .to_string();
        let _ = resp.into_string();
        Self {
            base_url,
            session_id,
            caller_header: caller_header.map(|s| s.to_string()),
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
        let raw = resp.into_string().expect("body read");
        parse_sse_jsonrpc(&raw).unwrap_or_else(|| panic!("parse failed: {raw}"))
    }

    fn call_tool(&self, id: u64, tool: &str, arguments: Value) -> Value {
        self.call(
            id,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        )
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
        .unwrap_or_else(|| panic!("missing text: {resp}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Driver fixture: pushes a driver-role Session into the manager via
// `push_for_test` and starts an MCP server wired up to both a confirm
// bridge and an event_tx. Returns everything the test needs.
// ---------------------------------------------------------------------------

struct DriverFixture {
    sessions: Arc<Mutex<SessionManager>>,
    #[allow(dead_code)]
    bus: Arc<EventBus>,
    driver_id: usize,
    server: Option<McpServer>,
    confirm_rx: Option<std::sync::mpsc::Receiver<ConfirmRequest>>,
    #[allow(dead_code)]
    event_rx: std::sync::mpsc::Receiver<ccom::event::Event>,
}

impl DriverFixture {
    fn build(policy: SpawnPolicy, budget: u32) -> Self {
        ensure_test_spawn_cmd();
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
        let driver_id = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(driver_id, "orch").with_role(
            SessionRole::Driver {
                spawn_budget: budget,
                spawn_policy: policy,
            },
        ));
        let sessions = Arc::new(Mutex::new(mgr));

        let (raw_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx = MonitoredSender::wrap(raw_tx);

        let (server, confirm_rx) = McpServer::start_with_confirm_and_event_tx(
            Arc::clone(&sessions),
            Arc::clone(&bus),
            event_tx,
        )
        .expect("server start");

        Self {
            sessions,
            bus,
            driver_id,
            server: Some(server),
            confirm_rx: Some(confirm_rx),
            event_rx,
        }
    }

    fn port(&self) -> u16 {
        self.server.as_ref().unwrap().port()
    }

    fn stop(mut self) {
        if let Some(s) = self.server.take() {
            s.stop();
        }
    }
}

fn spawn_auto_responder(
    rx: std::sync::mpsc::Receiver<ConfirmRequest>,
    response: ConfirmResponse,
) -> std::thread::JoinHandle<Vec<(ConfirmTool, usize)>> {
    std::thread::spawn(move || {
        let mut observed: Vec<(ConfirmTool, usize)> = Vec::new();
        while let Ok(req) = rx.recv() {
            observed.push((req.tool, req.session_id));
            let _ = req.resp_tx.send(response);
        }
        observed
    })
}

// ---------------------------------------------------------------------------
// Task 3 — spawn_session tests.
// ---------------------------------------------------------------------------

#[test]
fn driver_with_budget_2_spawns_silently_then_asks_on_third() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Budget, 2);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    // Responder denies on confirmation — once budget is exhausted
    // the third call should be asked, then denied.
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Deny);

    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    // Call #1 + #2: silent (budget 2 → 1 → 0).
    for i in 0..2 {
        let resp = client.call_tool(
            10 + i,
            "spawn_session",
            json!({"label": format!("child-{}", i), "working_dir": "/tmp"}),
        );
        assert!(
            !tool_is_error(&resp),
            "silent spawn {i} should succeed: {resp}"
        );
    }

    // Call #3: budget exhausted → confirmation → denial.
    let resp = client.call_tool(
        20,
        "spawn_session",
        json!({"label": "child-2", "working_dir": "/tmp"}),
    );
    assert!(tool_is_error(&resp), "third spawn must be rejected: {resp}");
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("denied"),
        "error should mention denied: {resp}"
    );

    // Clean up any spawned children before shutdown.
    {
        let mut mgr = fixture.sessions.lock().unwrap();
        let ids: Vec<usize> = mgr
            .iter()
            .filter(|s| s.spawned_by == Some(fixture.driver_id))
            .map(|s| s.id)
            .collect();
        for id in ids {
            mgr.kill(id);
        }
    }

    fixture.stop();
    let observed = responder.join().unwrap();
    assert_eq!(
        observed.len(),
        1,
        "only the third call should ask for confirmation: {observed:?}"
    );
    assert_eq!(observed[0].0, ConfirmTool::SpawnSession);
}

#[test]
fn driver_with_trust_policy_never_asks() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Deny);

    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    // Five silent spawns — Trust never asks regardless of budget.
    for i in 0..5 {
        let resp = client.call_tool(
            30 + i,
            "spawn_session",
            json!({"label": format!("trusted-{}", i), "working_dir": "/tmp"}),
        );
        assert!(!tool_is_error(&resp), "trust spawn {i} failed: {resp}");
    }

    {
        let mut mgr = fixture.sessions.lock().unwrap();
        let ids: Vec<usize> = mgr
            .iter()
            .filter(|s| s.spawned_by == Some(fixture.driver_id))
            .map(|s| s.id)
            .collect();
        for id in ids {
            mgr.kill(id);
        }
    }
    fixture.stop();
    let observed = responder.join().unwrap();
    assert!(
        observed.is_empty(),
        "Trust policy must never ask: {observed:?}"
    );
}

#[test]
fn driver_with_ask_policy_asks_every_time() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Ask, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    for i in 0..3 {
        let resp = client.call_tool(
            40 + i,
            "spawn_session",
            json!({"label": format!("ask-{}", i), "working_dir": "/tmp"}),
        );
        assert!(!tool_is_error(&resp), "ask spawn {i} failed: {resp}");
    }

    {
        let mut mgr = fixture.sessions.lock().unwrap();
        let ids: Vec<usize> = mgr
            .iter()
            .filter(|s| s.spawned_by == Some(fixture.driver_id))
            .map(|s| s.id)
            .collect();
        for id in ids {
            mgr.kill(id);
        }
    }
    fixture.stop();
    let observed = responder.join().unwrap();
    assert_eq!(
        observed.len(),
        3,
        "Ask must prompt every time: {observed:?}"
    );
    for entry in &observed {
        assert_eq!(entry.0, ConfirmTool::SpawnSession);
    }
}

#[test]
fn solo_caller_cannot_use_spawn_session() {
    ensure_test_spawn_cmd();
    // Push a solo session, not a driver.
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let solo_id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(solo_id, "solo"));
    let sessions = Arc::new(Mutex::new(mgr));

    let (raw_tx, _event_rx) = std::sync::mpsc::channel();
    let event_tx = MonitoredSender::wrap(raw_tx);
    let (server, _confirm_rx) =
        McpServer::start_with_confirm_and_event_tx(sessions, Arc::clone(&bus), event_tx)
            .expect("start");

    let client = McpClient::initialize(server.port(), Some(&solo_id.to_string()));
    let resp = client.call_tool(
        50,
        "spawn_session",
        json!({"label": "nope", "working_dir": "/tmp"}),
    );
    assert!(tool_is_error(&resp), "solo caller must be rejected: {resp}");
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("not a driver") || blob.contains("driver"),
        "error should mention driver: {resp}"
    );

    server.stop();
}

#[test]
fn spawn_session_sanitizes_label() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let _responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);
    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    // ANSI + emoji + control chars — all should be stripped.
    let dirty = "\u{1b}[31mhot\u{1b}[0m\u{1f600}\u{01}-child";
    let resp = client.call_tool(
        60,
        "spawn_session",
        json!({"label": dirty, "working_dir": "/tmp"}),
    );
    assert!(!tool_is_error(&resp), "sanitize spawn failed: {resp}");
    let text = tool_result_text(&resp);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    let label = parsed["label"].as_str().unwrap();
    assert_eq!(
        label, "hot-child",
        "label should be sanitized to ASCII whitelist: got {label}"
    );

    {
        let mut mgr = fixture.sessions.lock().unwrap();
        let ids: Vec<usize> = mgr
            .iter()
            .filter(|s| s.spawned_by == Some(fixture.driver_id))
            .map(|s| s.id)
            .collect();
        for id in ids {
            mgr.kill(id);
        }
    }
    fixture.stop();
}

#[test]
fn spawn_session_empty_label_rejected() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let _confirm_rx = fixture.confirm_rx.take();
    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    let resp = client.call_tool(70, "spawn_session", json!({"label": ""}));
    assert!(
        tool_is_error(&resp),
        "empty label should be rejected: {resp}"
    );
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(blob.contains("empty"), "error should mention empty: {resp}");

    // Also: all-non-whitelist characters should be rejected.
    let resp2 = client.call_tool(71, "spawn_session", json!({"label": "\u{1f600}\u{1f601}"}));
    assert!(
        tool_is_error(&resp2),
        "all-emoji label should be rejected: {resp2}"
    );

    fixture.stop();
}

// ---------------------------------------------------------------------------
// Task 6 — driver-kill policy tests.
// ---------------------------------------------------------------------------

#[test]
fn driver_kill_own_child_is_silent() {
    // Build a driver with Trust so the initial spawn is silent, then
    // call kill_session on the resulting child — it should succeed
    // without raising a modal.
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Deny);
    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    let spawn_resp = client.call_tool(
        80,
        "spawn_session",
        json!({"label": "kill-me", "working_dir": "/tmp"}),
    );
    assert!(!tool_is_error(&spawn_resp), "spawn failed: {spawn_resp}");
    let text = tool_result_text(&spawn_resp);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    let child_id = parsed["session_id"].as_u64().unwrap() as usize;

    let kill_resp = client.call_tool(81, "kill_session", json!({"session_id": child_id}));
    assert!(
        !tool_is_error(&kill_resp),
        "driver kill child should succeed silently: {kill_resp}"
    );
    let kill_text = tool_result_text(&kill_resp);
    assert!(kill_text.contains("killed"), "unexpected body: {kill_text}");

    // Child should be gone from the manager.
    {
        let mgr = fixture.sessions.lock().unwrap();
        assert!(mgr.get(child_id).is_none());
    }

    fixture.stop();
    let observed = responder.join().unwrap();
    // No SpawnSession confirmations (Trust) and no KillSession
    // confirmations (silent driver kill).
    assert!(
        observed.is_empty(),
        "driver child kill must not raise a modal: {observed:?}"
    );
}

#[test]
fn driver_kill_out_of_scope_returns_not_found() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    // Push an unrelated session the driver does NOT own.
    let stranger_id = {
        let mut mgr = fixture.sessions.lock().unwrap();
        let id = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id, "stranger"));
        id
    };

    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));
    let resp = client.call_tool(90, "kill_session", json!({"session_id": stranger_id}));
    assert!(
        tool_is_error(&resp),
        "out-of-scope kill must be NotFound: {resp}"
    );
    let blob = resp.to_string().to_ascii_lowercase();
    assert!(
        blob.contains("not found"),
        "error should say not found: {resp}"
    );

    // Stranger must still be alive.
    {
        let mgr = fixture.sessions.lock().unwrap();
        assert!(mgr.get(stranger_id).is_some());
    }

    fixture.stop();
    let observed = responder.join().unwrap();
    assert!(
        observed.is_empty(),
        "out-of-scope kill must never raise a modal: {observed:?}"
    );
}

#[test]
fn solo_kill_still_prompts() {
    // Phase 5 regression guard: a solo (or unknown) caller killing
    // a session still goes through the confirmation modal.
    ensure_test_spawn_cmd();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    // Spawn a real /bin/cat so kill_session has something to kill.
    let (raw_tx, _event_rx) = std::sync::mpsc::channel();
    let event_tx = MonitoredSender::wrap(raw_tx);
    let target_id = mgr
        .spawn(ccom::session::SpawnConfig {
            label: "solo-target".to_string(),
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
    let sessions = Arc::new(Mutex::new(mgr));

    let (raw_tx2, _rx2) = std::sync::mpsc::channel();
    let event_tx2 = MonitoredSender::wrap(raw_tx2);
    let (server, confirm_rx) = McpServer::start_with_confirm_and_event_tx(
        Arc::clone(&sessions),
        Arc::clone(&bus),
        event_tx2,
    )
    .expect("start");

    // Responder Allows — verify the request actually reached it.
    let responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    // No X-Ccom-Caller header → Scope::Full → modal path.
    let client = McpClient::initialize(server.port(), None);
    let resp = client.call_tool(100, "kill_session", json!({"session_id": target_id}));
    assert!(!tool_is_error(&resp), "solo kill allow failed: {resp}");
    // Target must be gone.
    {
        let mgr = sessions.lock().unwrap();
        assert!(mgr.get(target_id).is_none());
    }

    server.stop();
    let observed = responder.join().unwrap();
    assert_eq!(
        observed.len(),
        1,
        "solo kill must go through the modal: {observed:?}"
    );
    assert_eq!(observed[0].0, ConfirmTool::KillSession);
    assert_eq!(observed[0].1, target_id);
}

// ---------------------------------------------------------------------------
// Task 9 — gap fills: plan matrix entries #5, #6, #9.
// ---------------------------------------------------------------------------

/// Plan #5 — `spawn_session` MUST produce Solo children, never another
/// Driver. The MCP handler calls `SessionManager::spawn_with_role` with
/// `role = None` which defaults to Solo; this test verifies that
/// end-to-end by inspecting the child's role in the manager after a
/// successful spawn. This is the "nesting cap": a driver's spawned
/// child can't recursively act as a driver itself.
#[test]
fn driver_cannot_spawn_driver_nesting_cap() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let _responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);
    let client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));

    let resp = client.call_tool(
        200,
        "spawn_session",
        json!({"label": "nested", "working_dir": "/tmp"}),
    );
    assert!(!tool_is_error(&resp), "spawn failed: {resp}");
    let text = tool_result_text(&resp);
    let parsed: Value = serde_json::from_str(&text).unwrap();
    let child_id = parsed["session_id"].as_u64().unwrap() as usize;

    {
        let mgr = fixture.sessions.lock().unwrap();
        let child = mgr.get(child_id).expect("child should exist in manager");
        assert_eq!(
            child.role,
            SessionRole::Solo,
            "nesting cap: spawned child must be Solo, not Driver: got {:?}",
            child.role
        );
        assert_eq!(
            child.spawned_by,
            Some(fixture.driver_id),
            "child must record driver as its parent"
        );
    }

    {
        let mut mgr = fixture.sessions.lock().unwrap();
        mgr.kill(child_id);
    }
    fixture.stop();
}

/// Plan #6 — `list_sessions` output is scope-filtered for driver
/// callers: the driver sees itself + its spawned children + any
/// attached sessions, never unrelated sessions. A solo/no-header
/// caller still sees every session (legacy Phase 1–5 behavior).
#[test]
fn list_sessions_filtered_for_driver_caller() {
    let mut fixture = DriverFixture::build(SpawnPolicy::Trust, 0);
    let confirm_rx = fixture.confirm_rx.take().unwrap();
    let _responder = spawn_auto_responder(confirm_rx, ConfirmResponse::Allow);

    // Seed one unrelated solo session directly in the manager. The
    // driver must NOT see this one.
    let stranger_id = {
        let mut mgr = fixture.sessions.lock().unwrap();
        let id = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id, "stranger"));
        id
    };

    // Driver spawns two children via MCP.
    let driver_client = McpClient::initialize(fixture.port(), Some(&fixture.driver_id.to_string()));
    let mut child_ids: Vec<usize> = Vec::new();
    for i in 0..2 {
        let resp = driver_client.call_tool(
            210 + i,
            "spawn_session",
            json!({"label": format!("owned-{}", i), "working_dir": "/tmp"}),
        );
        assert!(!tool_is_error(&resp), "spawn {i} failed: {resp}");
        let text = tool_result_text(&resp);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        child_ids.push(parsed["session_id"].as_u64().unwrap() as usize);
    }

    // Driver's list_sessions → only driver + 2 children (3 total),
    // stranger not visible.
    let driver_list = driver_client.call_tool(220, "list_sessions", json!({}));
    assert!(
        !tool_is_error(&driver_list),
        "driver list failed: {driver_list}"
    );
    let driver_text = tool_result_text(&driver_list);
    let driver_rows: Vec<Value> = serde_json::from_str(&driver_text).unwrap();
    let driver_ids: HashSet<usize> = driver_rows
        .iter()
        .map(|r| r["id"].as_u64().unwrap() as usize)
        .collect();
    let mut expected_driver: HashSet<usize> = child_ids.iter().copied().collect();
    expected_driver.insert(fixture.driver_id);
    assert_eq!(
        driver_ids, expected_driver,
        "driver scope: got {driver_ids:?}, expected {expected_driver:?}"
    );
    assert!(
        !driver_ids.contains(&stranger_id),
        "stranger must not appear in driver scope"
    );

    // Solo/no-header caller sees everything: driver + 2 children +
    // stranger = 4.
    let solo_client = McpClient::initialize(fixture.port(), None);
    let solo_list = solo_client.call_tool(221, "list_sessions", json!({}));
    assert!(!tool_is_error(&solo_list), "solo list failed: {solo_list}");
    let solo_text = tool_result_text(&solo_list);
    let solo_rows: Vec<Value> = serde_json::from_str(&solo_text).unwrap();
    let solo_ids: HashSet<usize> = solo_rows
        .iter()
        .map(|r| r["id"].as_u64().unwrap() as usize)
        .collect();
    let mut expected_solo = expected_driver.clone();
    expected_solo.insert(stranger_id);
    assert_eq!(
        solo_ids, expected_solo,
        "solo scope: got {solo_ids:?}, expected {expected_solo:?}"
    );

    {
        let mut mgr = fixture.sessions.lock().unwrap();
        for id in &child_ids {
            mgr.kill(*id);
        }
    }
    fixture.stop();
}

/// Plan #9 — a session placed into a driver's attachment map (without
/// being spawned by that driver) becomes visible in the driver's
/// `list_sessions` result. Validates the shared `Arc<Mutex<_>>`
/// attachment contract between `App` and `McpCtx::caller_scope`.
///
/// Uses a dedicated fixture path that passes a pre-built `attachments`
/// map into `McpServer::start_with_confirm_event_tx_and_attachments`
/// so the test can mutate it directly.
#[test]
fn attached_session_visible_in_driver_scope() {
    ensure_test_spawn_cmd();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    // Push a driver and an unrelated "attached" session.
    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "orch").with_role(SessionRole::Driver {
            spawn_budget: 0,
            spawn_policy: SpawnPolicy::Trust,
        }),
    );
    let attached_id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(attached_id, "attached"));
    // And an unrelated stranger the driver must NOT see.
    let stranger_id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(stranger_id, "stranger"));
    let sessions = Arc::new(Mutex::new(mgr));

    // Pre-seed the attachment map with { driver_id -> {attached_id} }.
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    {
        let mut att = attachments.lock().unwrap();
        let mut set = HashSet::new();
        set.insert(attached_id);
        att.insert(driver_id, set);
    }

    let (raw_tx, _event_rx) = std::sync::mpsc::channel();
    let event_tx = MonitoredSender::wrap(raw_tx);
    let (server, _confirm_rx) = McpServer::start_with_confirm_event_tx_and_attachments(
        Arc::clone(&sessions),
        Arc::clone(&bus),
        event_tx,
        Arc::clone(&attachments),
    )
    .expect("server start");

    let client = McpClient::initialize(server.port(), Some(&driver_id.to_string()));
    let resp = client.call_tool(300, "list_sessions", json!({}));
    assert!(!tool_is_error(&resp), "list_sessions failed: {resp}");
    let text = tool_result_text(&resp);
    let rows: Vec<Value> = serde_json::from_str(&text).unwrap();
    let ids: HashSet<usize> = rows
        .iter()
        .map(|r| r["id"].as_u64().unwrap() as usize)
        .collect();

    assert!(ids.contains(&driver_id), "driver must see itself: {ids:?}");
    assert!(
        ids.contains(&attached_id),
        "attached session must be visible to driver scope: {ids:?}"
    );
    assert!(
        !ids.contains(&stranger_id),
        "unrelated stranger must not leak into driver scope: {ids:?}"
    );
    assert_eq!(ids.len(), 2, "expected exactly driver + attached: {ids:?}");

    server.stop();
}
