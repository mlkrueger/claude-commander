//! Phase 7 Tasks 1+2+3+4 — integration tests for hook infrastructure and
//! approval routing.
//!
//! These tests verify:
//! - `Session::spawn` creates the Unix socket file in the hook dir
//! - `pretooluse_settings.json` has the correct JSON shape
//! - `respond_to_tool_approval` MCP handler resolves pending approvals
//! - approvals_state helpers read/write/match allow-always rules
//!
//! We use `CCOM_TEST_SPAWN_CMD=/bin/cat` and
//! `CCOM_TEST_PRETOOLUSE_HOOK_CMD=echo` to avoid needing the real
//! Claude binary or the real hook binary.

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use ccom::approvals::ApprovalRegistry;
use ccom::event::MonitoredSender;
use ccom::mcp::McpServer;
use ccom::session::{EventBus, Session, SessionManager, SessionRole, SpawnConfig, SpawnPolicy};
use serde_json::json;

// ---------------------------------------------------------------------------
// Process-wide env var setup and serialization
// ---------------------------------------------------------------------------

/// Mutex to serialize tests that share hook dir path space (same PID,
/// same session id = same /tmp/ccom-<pid>-<id> path).
static TEST_MUTEX: Mutex<()> = Mutex::new(());

fn ensure_test_env() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        unsafe {
            std::env::set_var("CCOM_TEST_SPAWN_CMD", "/bin/cat");
            // Use `/bin/echo` as a stand-in for the pretooluse hook — it
            // immediately exits (allowing Claude to proceed), which is
            // correct "passthrough" behavior for tests.
            std::env::set_var("CCOM_TEST_PRETOOLUSE_HOOK_CMD", "/bin/echo");
        }
    });
}

/// Shared event channel for tests that need to observe PTY events.
fn make_event_tx() -> MonitoredSender {
    let (tx, _rx) = std::sync::mpsc::channel();
    MonitoredSender::wrap(tx)
}

// ---------------------------------------------------------------------------
// Test: socket file is created in hook dir
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn session_spawn_creates_approval_socket() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

        let id = mgr
            .spawn(SpawnConfig {
                label: "test-hook-socket".to_string(),
                working_dir: std::path::PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            })
            .expect("spawn succeeded");

        // Yield to the runtime so the socket listener task can run and bind.
        tokio::time::sleep(Duration::from_millis(100)).await;

        {
            let session = mgr.get(id).expect("session exists");
            let hook_dir = session
                .hook_dir()
                .expect("hook_dir is set for hook sessions");
            let socket_path = hook_dir.join("approval.sock");
            assert!(
                socket_path.exists(),
                "approval.sock must exist in hook dir: {}",
                socket_path.display()
            );
        }

        // Kill the session to clean up.
        mgr.kill(id);
    });
}

// ---------------------------------------------------------------------------
// Test: pretooluse_settings.json has the correct shape
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn session_spawn_writes_pretooluse_hook_entry() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

        let id = mgr
            .spawn(SpawnConfig {
                label: "test-pretooluse-settings".to_string(),
                working_dir: std::path::PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            })
            .expect("spawn succeeded");

        {
            let session = mgr.get(id).expect("session exists");
            let hook_dir = session
                .hook_dir()
                .expect("hook_dir is set for hook sessions");

            let settings_path = hook_dir.join("pretooluse_settings.json");
            assert!(
                settings_path.exists(),
                "pretooluse_settings.json must exist: {}",
                settings_path.display()
            );

            let contents =
                std::fs::read_to_string(&settings_path).expect("read pretooluse_settings.json");
            let parsed: serde_json::Value = serde_json::from_str(&contents).expect("valid JSON");

            // Verify the structure: hooks.PreToolUse[0].hooks[0].type == "command"
            let hook_type = &parsed["hooks"]["PreToolUse"][0]["hooks"][0]["type"];
            assert_eq!(
                hook_type.as_str().unwrap_or(""),
                "command",
                "PreToolUse hook type must be 'command'"
            );

            // Verify timeout is 600.
            let timeout = &parsed["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"];
            assert_eq!(
                timeout.as_u64().unwrap_or(0),
                600,
                "PreToolUse hook timeout must be 600"
            );

            // Verify command is set (not empty).
            let command = &parsed["hooks"]["PreToolUse"][0]["hooks"][0]["command"];
            assert!(
                command.as_str().map(|s| !s.is_empty()).unwrap_or(false),
                "PreToolUse hook command must be non-empty"
            );
        }

        mgr.kill(id);
    });
}

// ---------------------------------------------------------------------------
// Task 3 — respond_to_tool_approval MCP handler
// ---------------------------------------------------------------------------

/// Minimal HTTP client for the MCP server (mirrors tests/mcp_write.rs).
struct McpClient {
    base_url: String,
    session_id: String,
}

impl McpClient {
    fn initialize(port: u16) -> Self {
        let base_url = format!("http://127.0.0.1:{port}/mcp");
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
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
            .timeout(std::time::Duration::from_secs(5))
            .send_string(&body)
            .expect("initialize POST");
        let session_id = resp
            .header("mcp-session-id")
            .or_else(|| resp.header("Mcp-Session-Id"))
            .expect("server did not return Mcp-Session-Id")
            .to_string();
        let _ = resp.into_string();
        Self {
            base_url,
            session_id,
        }
    }

    fn call(
        &self,
        id: u64,
        method: &str,
        params: serde_json::Value,
        caller_id: Option<usize>,
    ) -> serde_json::Value {
        let body = json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })
        .to_string();
        let mut req = ureq::post(&self.base_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("Mcp-Session-Id", &self.session_id)
            .timeout(std::time::Duration::from_secs(10));
        if let Some(cid) = caller_id {
            req = req.set("X-Ccom-Caller", &cid.to_string());
        }
        let resp = req.send_string(&body).expect("POST failed");
        let raw = resp.into_string().expect("body read");
        parse_sse_jsonrpc(&raw).unwrap_or_else(|| json!({"_raw": raw}))
    }
}

fn parse_sse_jsonrpc(raw: &str) -> Option<serde_json::Value> {
    for line in raw.lines().rev() {
        if let Some(rest) = line.strip_prefix("data:") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
                return Some(v);
            }
        }
    }
    None
}

fn tool_result_text(resp: &serde_json::Value) -> String {
    resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn is_tool_error(resp: &serde_json::Value) -> bool {
    resp["result"]["isError"].as_bool().unwrap_or(false)
}

fn make_driver_mgr() -> (SessionManager, Arc<EventBus>, usize, usize) {
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );
    let child_id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(child_id, "child").with_spawned_by(driver_id));
    (mgr, bus, driver_id, child_id)
}

#[test]
fn driver_allows_once_child_proceeds() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let (mgr, bus, driver_id, child_id) = make_driver_mgr();
    let registry = ApprovalRegistry::new();
    let (request_id, _decision_rx) = registry.open_request(
        child_id,
        "fake-uuid-allow-once".to_string(),
        driver_id,
        "Bash".to_string(),
        json!({"command": "ls"}),
        std::path::PathBuf::from("/tmp"),
    );

    let (server, _) =
        McpServer::start_with_approvals(Arc::new(Mutex::new(mgr)), bus, Arc::clone(&registry))
            .expect("server start");

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": request_id, "decision": "allow", "scope": "once"
        }}),
        Some(driver_id),
    );
    assert!(!is_tool_error(&resp), "expected success; got: {resp}");
    let text = tool_result_text(&resp);
    let result: serde_json::Value = serde_json::from_str(&text).expect("parse result");
    assert_eq!(result["decision"], "allow");
    assert_eq!(result["scope"], "once");
    // Registry must be consumed.
    assert!(registry.pending_for_driver(driver_id).is_empty());
    server.stop();
}

#[test]
fn driver_denies_child_aborts() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let (mgr, bus, driver_id, child_id) = make_driver_mgr();
    let registry = ApprovalRegistry::new();
    let (request_id, _rx) = registry.open_request(
        child_id,
        "fake-uuid-deny".to_string(),
        driver_id,
        "Edit".to_string(),
        json!({}),
        std::path::PathBuf::from("/tmp"),
    );

    let (server, _) =
        McpServer::start_with_approvals(Arc::new(Mutex::new(mgr)), bus, Arc::clone(&registry))
            .expect("server start");

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": request_id, "decision": "deny"
        }}),
        Some(driver_id),
    );
    assert!(!is_tool_error(&resp), "expected success; got: {resp}");
    let text = tool_result_text(&resp);
    let result: serde_json::Value = serde_json::from_str(&text).expect("parse result");
    assert_eq!(result["decision"], "deny");
    assert!(registry.pending_for_driver(driver_id).is_empty());
    server.stop();
}

#[test]
fn driver_allows_always_writes_state_file() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let uuid = format!("allow-always-{}", std::process::id());
    let state_base = std::env::temp_dir().join("ccom-it-approvals-state");
    std::fs::create_dir_all(&state_base).unwrap();
    let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
    unsafe {
        std::env::set_var("XDG_STATE_HOME", state_base.to_str().unwrap());
    }

    let (mgr, bus, driver_id, child_id) = make_driver_mgr();
    let registry = ApprovalRegistry::new();
    let tool_args = json!({"command": "ls -la"});
    let (request_id, _rx) = registry.open_request(
        child_id,
        uuid.clone(),
        driver_id,
        "Bash".to_string(),
        tool_args.clone(),
        std::path::PathBuf::from("/tmp"),
    );

    let (server, _) =
        McpServer::start_with_approvals(Arc::new(Mutex::new(mgr)), bus, Arc::clone(&registry))
            .expect("server start");

    let client = McpClient::initialize(server.port());
    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": request_id, "decision": "allow", "scope": "allow_always"
        }}),
        Some(driver_id),
    );
    assert!(!is_tool_error(&resp), "expected success; got: {resp}");

    std::thread::sleep(std::time::Duration::from_millis(100));

    let state = ccom::approvals_state::read_approvals(&uuid).expect("read approvals");
    assert!(
        ccom::approvals_state::matches_allow_always(&state, "Bash", &tool_args),
        "allow-always rule must be present; state={state:?}"
    );
    let _ = std::fs::remove_dir_all(
        ccom::approvals_state::state_file_path(&uuid)
            .parent()
            .unwrap(),
    );
    unsafe {
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
    server.stop();
}

#[test]
fn driver_cannot_answer_other_drivers_request() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let driver1 = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver1, "driver1").with_role(SessionRole::Driver {
            spawn_budget: 0,
            spawn_policy: SpawnPolicy::Ask,
        }),
    );
    let child1 = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(child1, "child1").with_spawned_by(driver1));
    let driver2 = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver2, "driver2").with_role(SessionRole::Driver {
            spawn_budget: 0,
            spawn_policy: SpawnPolicy::Ask,
        }),
    );

    let registry = ApprovalRegistry::new();
    let (request_id, _rx) = registry.open_request(
        child1,
        "uuid-c1".to_string(),
        driver1,
        "Bash".to_string(),
        json!({}),
        std::path::PathBuf::from("/tmp"),
    );

    let (server, _) = McpServer::start_with_approvals(
        Arc::new(Mutex::new(mgr)),
        Arc::clone(&bus),
        Arc::clone(&registry),
    )
    .expect("server start");
    let client = McpClient::initialize(server.port());

    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": request_id, "decision": "allow"
        }}),
        Some(driver2),
    );
    assert!(
        is_tool_error(&resp),
        "driver2 stealing driver1 request must error; got: {resp}"
    );
    let text = tool_result_text(&resp);
    assert!(
        text.contains("different driver"),
        "error must say 'different driver'; got: {text:?}"
    );
    assert_eq!(
        registry.pending_for_driver(driver1).len(),
        1,
        "request must still be pending"
    );
    server.stop();
}

#[test]
fn solo_caller_rejected_from_respond_tool() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let solo_id = mgr.peek_next_id();
    mgr.push_for_test(Session::dummy_exited(solo_id, "solo"));

    let registry = ApprovalRegistry::new();
    let (server, _) = McpServer::start_with_approvals(Arc::new(Mutex::new(mgr)), bus, registry)
        .expect("server start");
    let client = McpClient::initialize(server.port());
    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": 999, "decision": "allow"
        }}),
        Some(solo_id),
    );
    assert!(
        is_tool_error(&resp),
        "solo caller must be rejected; got: {resp}"
    );
    server.stop();
}

#[test]
fn unknown_request_id_returns_error() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let (mgr, bus, driver_id, _) = make_driver_mgr();
    let registry = ApprovalRegistry::new();

    let (server, _) = McpServer::start_with_approvals(Arc::new(Mutex::new(mgr)), bus, registry)
        .expect("server start");
    let client = McpClient::initialize(server.port());
    let resp = client.call(
        1,
        "tools/call",
        json!({"name": "respond_to_tool_approval", "arguments": {
            "request_id": 99999, "decision": "deny"
        }}),
        Some(driver_id),
    );
    assert!(
        is_tool_error(&resp),
        "unknown request_id must error; got: {resp}"
    );
    let text = tool_result_text(&resp);
    assert!(
        text.contains("not found"),
        "error must say 'not found'; got: {text:?}"
    );
    server.stop();
}
