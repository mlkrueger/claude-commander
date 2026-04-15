//! Phase 7 Tasks 1+2+3+4+5 — integration tests for hook infrastructure and
//! approval routing.
//!
//! These tests verify:
//! - `Session::spawn` creates the Unix socket file in the hook dir
//! - `pretooluse_settings.json` has the correct JSON shape
//! - `respond_to_tool_approval` MCP handler resolves pending approvals
//! - approvals_state helpers read/write/match allow-always rules
//! - coordinator routes requests: solo→passthrough, child→driver, dynamic-attach→driver
//!
//! We use `CCOM_TEST_SPAWN_CMD=/bin/cat` and
//! `CCOM_TEST_PRETOOLUSE_HOOK_CMD=echo` to avoid needing the real
//! Claude binary or the real hook binary.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
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

// ---------------------------------------------------------------------------
// Task 5 — coordinator routing tests
// ---------------------------------------------------------------------------

/// Helper: send a fake hook request directly to `socket_path` and return
/// the decision string ("allow" | "deny" | "passthrough").
async fn send_fake_hook_request(
    socket_path: &std::path::Path,
    ccom_session_id: usize,
    tool_name: &str,
) -> String {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;

    let request = json!({
        "session_id": format!("fake-uuid-{ccom_session_id}"),
        "ccom_session_id": ccom_session_id,
        "tool_name": tool_name,
        "tool_input": {"command": "ls"},
        "cwd": "/tmp",
        "tool_use_id": format!("fake-tool-use-{ccom_session_id}"),
        "nonce": 99u64,
    });
    let path = socket_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let stream = UnixStream::connect(&path).expect("connect to approval.sock");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut line = serde_json::to_string(&request).unwrap();
        line.push('\n');
        writer.write_all(line.as_bytes()).unwrap();
        writer.flush().unwrap();
        let reader = std::io::BufReader::new(stream);
        reader
            .lines()
            .next()
            .and_then(|l| l.ok())
            .unwrap_or_default()
    })
    .await
    .expect("blocking task")
}

/// Spawn a session with `install_hook: true` inside a tokio context,
/// return (mgr, session_id, hook_dir, approval_socket_rx).
fn spawn_hook_session(mgr: &mut SessionManager) -> (usize, PathBuf) {
    let id = mgr
        .spawn(SpawnConfig {
            label: format!("hook-test-{}", std::process::id()),
            working_dir: PathBuf::from("/tmp"),
            command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
            args: vec![],
            event_tx: make_event_tx(),
            cols: 80,
            rows: 24,
            install_hook: true,
            mcp_port: None,
        })
        .expect("spawn");
    let hook_dir = mgr
        .get(id)
        .and_then(|s| s.hook_dir())
        .map(|p| p.to_path_buf())
        .expect("hook_dir");
    (id, hook_dir)
}

/// Task 5 test 1 — every Claude session gets a hook dir and approval socket.
#[cfg(unix)]
#[tokio::test]
async fn solo_session_gets_hook_installed() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let (id, hook_dir) = spawn_hook_session(&mut mgr);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        hook_dir.join("approval.sock").exists(),
        "approval.sock must exist for hook session"
    );
    assert!(
        mgr.get(id).and_then(|s| s.hook_dir()).is_some(),
        "session must have hook_dir set"
    );
    mgr.kill(id);
}

/// Task 5 test 2 — solo session (no driver) gets passthrough from the coordinator.
#[cfg(unix)]
#[tokio::test]
async fn solo_session_hook_replies_passthrough() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
    let (id, hook_dir) = spawn_hook_session(&mut mgr);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    let raw = send_fake_hook_request(&socket_path, id, "Bash").await;
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON response");
    assert_eq!(
        parsed["decision"].as_str(),
        Some("passthrough"),
        "solo session must passthrough; got: {raw}"
    );

    sessions.lock().unwrap().kill(id);
}

/// Task 5 test 3 — child session (spawned_by driver) routes to driver.
#[cfg(unix)]
#[tokio::test]
async fn driver_owned_child_routes_to_driver() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    // Add a dummy driver session.
    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );

    // Spawn a real child with spawned_by = driver_id.
    let child_id = {
        mgr.spawn_with_role(
            SpawnConfig {
                label: format!("child-hook-{}", std::process::id()),
                working_dir: PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            },
            None,
            Some(driver_id),
        )
        .expect("spawn child")
    };
    tokio::time::sleep(Duration::from_millis(50)).await;

    let hook_dir = mgr
        .get(child_id)
        .and_then(|s| s.hook_dir())
        .map(|p| p.to_path_buf())
        .expect("hook_dir");
    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(child_id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    // Send request — coordinator should open a registry entry for the driver.
    // We resolve it immediately so the socket gets a response.
    let approvals_clone = Arc::clone(&approvals);
    let resolve_task = tokio::spawn(async move {
        // Poll until a pending approval appears.
        for _ in 0..50 {
            let pending = approvals_clone.pending_for_driver(driver_id);
            if let Some(entry) = pending.first() {
                let request_id = entry.request_id;
                approvals_clone
                    .resolve(
                        request_id,
                        driver_id,
                        ccom::approvals::ApprovalDecision::Allow,
                        ccom::approvals::ApprovalScope::Once,
                    )
                    .ok();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    });

    let raw = send_fake_hook_request(&socket_path, child_id, "Bash").await;
    let resolved = resolve_task.await.unwrap_or(false);
    assert!(resolved, "pending approval must have appeared in registry");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON response");
    assert_eq!(
        parsed["decision"].as_str(),
        Some("allow"),
        "driver-owned child must get allow; got: {raw}"
    );

    sessions.lock().unwrap().kill(child_id);
}

/// Task 5 test 4 — dynamically-attached session routes to driver after
/// attachment, even though it was spawned solo.
#[cfg(unix)]
#[tokio::test]
async fn attached_session_routes_to_driver_dynamically() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    // Add a dummy driver.
    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );

    // Spawn a solo session (no spawned_by).
    let (solo_id, hook_dir) = spawn_hook_session(&mut mgr);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(solo_id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    // Dynamically attach solo → driver via the attachment map.
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    attachments
        .lock()
        .unwrap()
        .entry(driver_id)
        .or_default()
        .insert(solo_id);

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    // Send request — coordinator must find driver via attachment map.
    let approvals_clone = Arc::clone(&approvals);
    let resolve_task = tokio::spawn(async move {
        for _ in 0..50 {
            let pending = approvals_clone.pending_for_driver(driver_id);
            if let Some(entry) = pending.first() {
                approvals_clone
                    .resolve(
                        entry.request_id,
                        driver_id,
                        ccom::approvals::ApprovalDecision::Deny,
                        ccom::approvals::ApprovalScope::Once,
                    )
                    .ok();
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    });

    let raw = send_fake_hook_request(&socket_path, solo_id, "Edit").await;
    let resolved = resolve_task.await.unwrap_or(false);
    assert!(
        resolved,
        "pending approval must appear via dynamic attachment"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON response");
    assert_eq!(
        parsed["decision"].as_str(),
        Some("deny"),
        "dynamically-attached session must route to driver; got: {raw}"
    );

    sessions.lock().unwrap().kill(solo_id);
}
/// Task 10 test 6 — two concurrent approvals from the same child are
/// handled independently: each gets the decision it was resolved with.
#[cfg(unix)]
#[tokio::test]
async fn two_concurrent_approvals_from_same_child_serialize_correctly() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );

    let child_id = mgr
        .spawn_with_role(
            SpawnConfig {
                label: format!("child-concurrent-{}", std::process::id()),
                working_dir: PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            },
            None,
            Some(driver_id),
        )
        .expect("spawn child");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let hook_dir = mgr
        .get(child_id)
        .and_then(|s| s.hook_dir())
        .map(|p| p.to_path_buf())
        .expect("hook_dir");
    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(child_id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    // Send two concurrent hook requests with different tool names.
    let sp1 = socket_path.clone();
    let task_bash =
        tokio::spawn(async move { send_fake_hook_request(&sp1, child_id, "Bash").await });
    let sp2 = socket_path.clone();
    let task_edit =
        tokio::spawn(async move { send_fake_hook_request(&sp2, child_id, "Edit").await });

    // Wait for both pending approvals to appear in the registry.
    for _ in 0..100 {
        if approvals.pending_for_driver(driver_id).len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pending = approvals.pending_for_driver(driver_id);
    assert_eq!(pending.len(), 2, "both approvals must appear in registry");

    // Resolve: Bash → Allow, Edit → Deny.
    for entry in &pending {
        let decision = if entry.tool == "Bash" {
            ccom::approvals::ApprovalDecision::Allow
        } else {
            ccom::approvals::ApprovalDecision::Deny
        };
        approvals
            .resolve(
                entry.request_id,
                driver_id,
                decision,
                ccom::approvals::ApprovalScope::Once,
            )
            .ok();
    }

    let raw_bash = task_bash.await.expect("bash task");
    let raw_edit = task_edit.await.expect("edit task");

    let bash_resp: serde_json::Value = serde_json::from_str(&raw_bash).expect("valid JSON");
    let edit_resp: serde_json::Value = serde_json::from_str(&raw_edit).expect("valid JSON");
    assert_eq!(
        bash_resp["decision"].as_str(),
        Some("allow"),
        "Bash must be allowed; got: {raw_bash}"
    );
    assert_eq!(
        edit_resp["decision"].as_str(),
        Some("deny"),
        "Edit must be denied; got: {raw_edit}"
    );

    sessions.lock().unwrap().kill(child_id);
}

/// Task 10 test 7 — when a driver session exits, all its pending
/// approvals are immediately denied so children are not left hanging.
#[cfg(unix)]
#[tokio::test]
async fn driver_exit_denies_all_pending_approvals_for_its_children() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    let bus = Arc::new(EventBus::new());
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );

    let child_id = mgr
        .spawn_with_role(
            SpawnConfig {
                label: format!("child-driver-exit-{}", std::process::id()),
                working_dir: PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            },
            None,
            Some(driver_id),
        )
        .expect("spawn child");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let hook_dir = mgr
        .get(child_id)
        .and_then(|s| s.hook_dir())
        .map(|p| p.to_path_buf())
        .expect("hook_dir");
    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(child_id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    // Send a hook request without resolving it, simulating a blocked child.
    let sp = socket_path.clone();
    let hook_task =
        tokio::spawn(async move { send_fake_hook_request(&sp, child_id, "Bash").await });

    // Wait for the pending approval to appear.
    for _ in 0..50 {
        if !approvals.pending_for_driver(driver_id).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !approvals.pending_for_driver(driver_id).is_empty(),
        "pending approval must appear before driver exit"
    );

    // Simulate driver exit: cancel all its pending approvals.
    // The coordinator receives a closed channel and sends Deny.
    approvals.deny_all_for_driver(driver_id);

    let raw = hook_task.await.expect("hook task completed");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["decision"].as_str(),
        Some("deny"),
        "driver exit must deny all pending approvals; got: {raw}"
    );
    // Registry must be empty — no ghost entries.
    assert!(approvals.pending_for_driver(driver_id).is_empty());

    sessions.lock().unwrap().kill(child_id);
}

/// Task 10 test 8 — when the coordinator times out waiting for a driver
/// decision, the blocked hook receives a deny response.
///
/// Uses `CCOM_APPROVAL_TIMEOUT_SECS=1` to avoid a 590-second wait.
#[cfg(unix)]
#[tokio::test]
async fn hook_timeout_denies_and_surfaces_to_tui_log() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    ensure_test_env();
    // Short timeout so the test doesn't wait 590 seconds.
    unsafe { std::env::set_var("CCOM_APPROVAL_TIMEOUT_SECS", "1") };

    let bus = Arc::new(EventBus::new());
    let bus_rx = bus.subscribe();
    let mut mgr = SessionManager::with_bus(Arc::clone(&bus));

    let driver_id = mgr.peek_next_id();
    mgr.push_for_test(
        Session::dummy_exited(driver_id, "driver").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        }),
    );

    let child_id = mgr
        .spawn_with_role(
            SpawnConfig {
                label: format!("child-timeout-{}", std::process::id()),
                working_dir: PathBuf::from("/tmp"),
                command: &std::env::var("CCOM_TEST_SPAWN_CMD").unwrap_or("/bin/cat".into()),
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            },
            None,
            Some(driver_id),
        )
        .expect("spawn child");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let hook_dir = mgr
        .get(child_id)
        .and_then(|s| s.hook_dir())
        .map(|p| p.to_path_buf())
        .expect("hook_dir");
    let socket_path = hook_dir.join("approval.sock");
    let rx = mgr
        .get_mut(child_id)
        .and_then(|s| s.take_approval_rx())
        .expect("approval_socket_rx");

    let sessions = Arc::new(Mutex::new(mgr));
    let approvals = ApprovalRegistry::new();
    let attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(ccom::approvals::run_coordinator(
        rx,
        Arc::clone(&sessions),
        Arc::clone(&approvals),
        Arc::clone(&bus),
        Arc::clone(&attachments),
    ));

    // Send a hook request but intentionally do NOT resolve it.
    // The coordinator must time out after 1s and send deny.
    let sp = socket_path.clone();
    let hook_task =
        tokio::spawn(async move { send_fake_hook_request(&sp, child_id, "Bash").await });

    // Wait for pending approval to appear, then just wait for the timeout.
    for _ in 0..50 {
        if !approvals.pending_for_driver(driver_id).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Wait for the 1s timeout + a little margin.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let raw = hook_task.await.expect("hook task completed");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
    assert_eq!(
        parsed["decision"].as_str(),
        Some("deny"),
        "coordinator timeout must deny; got: {raw}"
    );

    // After timeout the coordinator publishes ToolApprovalResolved so the
    // TUI status-line counter can clear.
    use ccom::session::SessionEvent;
    let resolved_seen = {
        let mut found = false;
        while let Ok(evt) = bus_rx.try_recv() {
            if let SessionEvent::ToolApprovalResolved { decision, .. } = evt {
                if decision == ccom::approvals::ApprovalDecision::Deny {
                    found = true;
                }
            }
        }
        found
    };
    assert!(
        resolved_seen,
        "ToolApprovalResolved(Deny) must be published by coordinator on timeout"
    );

    // Registry must be empty — no ghost entries after timeout.
    assert!(approvals.pending_for_driver(driver_id).is_empty());

    unsafe { std::env::remove_var("CCOM_APPROVAL_TIMEOUT_SECS") };
    sessions.lock().unwrap().kill(child_id);
}

/// Task 10 test 10 — state file survives concurrent writes from multiple
/// goroutines: all rules must be present afterward.
#[test]
fn state_file_round_trip_under_concurrent_write() {
    let _lock = TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    let uuid = format!("concurrent-write-{}", std::process::id());
    let state_base = std::env::temp_dir().join("ccom-it-concurrent-write");
    std::fs::create_dir_all(&state_base).unwrap();
    let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
    unsafe { std::env::set_var("XDG_STATE_HOME", state_base.to_str().unwrap()) };

    // Two threads each write their own tool rule 5 times to the same file.
    let uuid1 = uuid.clone();
    let uuid2 = uuid.clone();
    let h1 = std::thread::spawn(move || {
        for _ in 0..5 {
            ccom::approvals_state::add_allow_always(&uuid1, "Bash", None).expect("h1 add");
        }
    });
    let h2 = std::thread::spawn(move || {
        for _ in 0..5 {
            ccom::approvals_state::add_allow_always(&uuid2, "Edit", None).expect("h2 add");
        }
    });
    h1.join().expect("h1 join");
    h2.join().expect("h2 join");

    let state = ccom::approvals_state::read_approvals(&uuid).expect("read approvals");
    assert!(
        ccom::approvals_state::matches_allow_always(&state, "Bash", &serde_json::json!({})),
        "Bash rule must be present after concurrent writes"
    );
    assert!(
        ccom::approvals_state::matches_allow_always(&state, "Edit", &serde_json::json!({})),
        "Edit rule must be present after concurrent writes"
    );

    // Cleanup.
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
}
