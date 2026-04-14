//! Phase 7 Tasks 1+2 — integration tests for hook infrastructure and
//! approval routing.
//!
//! These tests verify:
//! - `Session::spawn` creates the Unix socket file in the hook dir
//! - `pretooluse_settings.json` has the correct JSON shape
//!
//! We use `CCOM_TEST_SPAWN_CMD=/bin/cat` and
//! `CCOM_TEST_PRETOOLUSE_HOOK_CMD=echo` to avoid needing the real
//! Claude binary or the real hook binary.

use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use ccom::event::MonitoredSender;
use ccom::session::{EventBus, SessionManager, SpawnConfig};

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
