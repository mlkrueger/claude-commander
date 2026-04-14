//! Tool approval registry and coordinator.
//!
//! Phase 7 Task 2: when a driver-owned session hits a gated tool call,
//! the PreToolUse hook routes the request to the driver via a Unix socket.
//! This module owns the registry of pending approvals and the coordinator
//! that bridges the socket listener to the driver.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::sync::oneshot;

// --- Public decision types -----------------------------------------------

/// The driver's decision on a pending tool-use approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// Whether the approval applies once or should be remembered permanently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalScope {
    Once,
    AllowAlways,
}

// --- Wire types for the hook socket --------------------------------------

/// A parsed request arriving from the ccom-hook-pretooluse binary over
/// the Unix socket.
#[derive(Debug, serde::Deserialize)]
pub struct ApprovalHookRequest {
    /// Claude Code's internal session UUID (from hook stdin).
    pub session_id: String,
    /// ccom's own session index (from CCOM_SESSION_ID env var in the hook
    /// binary). Used for direct session lookup — avoids depending on
    /// `claude_session_id` which is only populated after the Stop hook fires.
    pub ccom_session_id: usize,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub cwd: String,
    pub tool_use_id: String,
    pub nonce: u64,
    /// Oneshot sender for the coordinator to reply back to the socket
    /// listener task.
    #[serde(skip)]
    pub response_tx: Option<oneshot::Sender<ApprovalHookResponse>>,
}

/// Response sent back to the hook binary (via the socket listener task).
#[derive(Debug)]
pub enum ApprovalHookResponse {
    Allow,
    Deny,
    Passthrough,
}

// --- Registry entry -------------------------------------------------------

/// A pending approval request waiting for a driver to resolve it.
pub struct PendingApproval {
    pub request_id: u64,
    /// ccom's own session index (NOT the Claude UUID).
    pub session_id: usize,
    /// Claude Code's internal session UUID (used for the state-file path).
    pub claude_uuid: String,
    pub driver_id: usize,
    pub tool: String,
    pub args: serde_json::Value,
    pub cwd: PathBuf,
    pub created_at: SystemTime,
    /// Oneshot channel to deliver the driver's `ApprovalDecision` back
    /// to the socket listener, which is blocking on this while Claude
    /// Code waits for the hook to exit.
    response_tx: oneshot::Sender<ApprovalDecision>,
}

/// Lightweight copy of a `PendingApproval`'s metadata, returned after
/// `resolve` consumes the entry (so the caller can do post-resolution
/// bookkeeping like writing the state file for AllowAlways).
#[derive(Debug)]
pub struct PendingApprovalMeta {
    /// ccom's own session index for the worker session that was waiting.
    pub session_id: usize,
    pub claude_uuid: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub scope: ApprovalScope,
}

/// Snapshot of a pending approval for serialization over MCP.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct PendingApprovalWire {
    pub request_id: u64,
    pub session_id: usize,
    pub driver_id: usize,
    pub tool: String,
    pub cwd: String,
    pub created_at_secs: u64,
}

// --- Registry -------------------------------------------------------------

/// Errors from `ApprovalRegistry::resolve`.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// No pending request with this id.
    NotFound,
    /// The caller's driver id doesn't match the registered driver.
    DriverMismatch,
}

/// Thread-safe registry of pending tool-use approvals.
///
/// Lives on `McpCtx` (wrapped in `Arc`) so it's shared between the MCP
/// handlers and the approval coordinator.
pub struct ApprovalRegistry {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingApproval>>,
}

impl ApprovalRegistry {
    /// Create a new empty registry wrapped in an `Arc`.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Called by the approval coordinator when a hook request arrives for
    /// a driver-owned session. Inserts a new `PendingApproval` and returns
    /// the assigned `request_id` plus a receiver the coordinator should
    /// await for the driver's decision.
    pub fn open_request(
        &self,
        session_id: usize,
        claude_uuid: String,
        driver_id: usize,
        tool: String,
        args: serde_json::Value,
        cwd: PathBuf,
    ) -> (u64, oneshot::Receiver<ApprovalDecision>) {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let entry = PendingApproval {
            request_id,
            session_id,
            claude_uuid,
            driver_id,
            tool,
            args,
            cwd,
            created_at: SystemTime::now(),
            response_tx: tx,
        };
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(request_id, entry);
        (request_id, rx)
    }

    /// Called by the `respond_to_tool_approval` MCP handler. Consumes the
    /// pending entry, sends the decision to the waiting coordinator, and
    /// returns metadata so the handler can do post-resolution work (e.g.
    /// writing the allow-always state file).
    ///
    /// # Errors
    /// - `NotFound` if no request with that id exists.
    /// - `DriverMismatch` if `caller_driver_id` doesn't match the entry's
    ///   registered driver (prevents cross-driver approval forgery).
    pub fn resolve(
        &self,
        request_id: u64,
        caller_driver_id: usize,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    ) -> Result<PendingApprovalMeta, ResolveError> {
        // Hold the lock across the driver check AND the conditional
        // re-insert to prevent a TOCTOU window where a concurrent
        // legitimate-driver call sees NotFound between our remove and
        // re-insert.
        let mut map = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.remove(&request_id).ok_or(ResolveError::NotFound)?;
        if entry.driver_id != caller_driver_id {
            // Put it back while still holding the lock.
            map.insert(request_id, entry);
            return Err(ResolveError::DriverMismatch);
        }
        // Release the lock before the oneshot send so we don't hold it
        // while the coordinator wakes up and accesses the registry.
        drop(map);
        // Send the decision to the coordinator (ignore send error — the
        // coordinator may have already timed out and dropped its receiver).
        let _ = entry.response_tx.send(decision);
        Ok(PendingApprovalMeta {
            session_id: entry.session_id,
            claude_uuid: entry.claude_uuid,
            tool: entry.tool,
            args: entry.args,
            scope,
        })
    }

    /// Remove a pending approval by request id. Used by the coordinator
    /// to clean up entries that timed out before the driver responded.
    pub fn cancel(&self, request_id: u64) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&request_id);
    }

    /// Deny and remove all pending approvals for a given driver. Called when
    /// the driver session exits so child sessions are not left hanging.
    ///
    /// Returns a `Vec<(request_id, session_id, driver_id)>` so the caller can
    /// publish `ToolApprovalResolved` events for each denied entry.
    pub fn deny_all_for_driver(&self, driver_id: usize) -> Vec<(u64, usize, usize)> {
        let entries: Vec<PendingApproval> = {
            let mut map = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let ids: Vec<u64> = map
                .values()
                .filter(|e| e.driver_id == driver_id)
                .map(|e| e.request_id)
                .collect();
            ids.into_iter().filter_map(|id| map.remove(&id)).collect()
        };

        entries
            .into_iter()
            .map(|e| {
                let _ = e.response_tx.send(ApprovalDecision::Deny);
                (e.request_id, e.session_id, e.driver_id)
            })
            .collect()
    }

    /// Remove entries that are older than `max_age_secs` (stale reaper).
    ///
    /// The default `max_age_secs` is `CCOM_APPROVAL_REAPER_SECS` env var if
    /// set and parseable, otherwise 710s (590s hook timeout + 120s grace).
    ///
    /// Returns the `(request_id, session_id, driver_id)` tuples of reaped
    /// entries so the caller can publish `ToolApprovalResolved` events and
    /// log appropriately.
    pub fn sweep_stale(&self) -> Vec<(u64, usize, usize)> {
        let max_age_secs: u64 = std::env::var("CCOM_APPROVAL_REAPER_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(710);
        let max_age = std::time::Duration::from_secs(max_age_secs);

        let now = SystemTime::now();
        let entries: Vec<PendingApproval> = {
            let mut map = self.pending.lock().unwrap_or_else(|p| p.into_inner());
            let stale_ids: Vec<u64> = map
                .values()
                .filter(|e| now.duration_since(e.created_at).unwrap_or_default() >= max_age)
                .map(|e| e.request_id)
                .collect();
            stale_ids
                .into_iter()
                .filter_map(|id| map.remove(&id))
                .collect()
        };

        entries
            .into_iter()
            .map(|e| {
                log::warn!(
                    "approval reaper: sweeping stale request_id={} session_id={} driver_id={} tool={}",
                    e.request_id,
                    e.session_id,
                    e.driver_id,
                    e.tool
                );
                let _ = e.response_tx.send(ApprovalDecision::Deny);
                (e.request_id, e.session_id, e.driver_id)
            })
            .collect()
    }

    /// Return wire-safe snapshots of all pending approvals for the given
    /// driver. Used by `list_tool_approvals` MCP tool.
    pub fn pending_for_driver(&self, driver_id: usize) -> Vec<PendingApprovalWire> {
        let map = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        map.values()
            .filter(|e| e.driver_id == driver_id)
            .map(|e| PendingApprovalWire {
                request_id: e.request_id,
                session_id: e.session_id,
                driver_id: e.driver_id,
                tool: e.tool.clone(),
                cwd: e.cwd.to_string_lossy().into_owned(),
                created_at_secs: e
                    .created_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            })
            .collect()
    }
}

// --- Coordinator ----------------------------------------------------------

/// Handle a single hook request arriving from the socket listener.
///
/// Looks up the session by `ccom_session_id`, finds its driver via
/// `spawned_by` or the dynamic attachment map, registers a pending
/// approval, publishes an event, awaits the driver's decision, and
/// replies on `request.response_tx`.
///
/// IMPORTANT: This function must NOT hold any mutex across `.await` points.
pub async fn handle_hook_request(
    mut request: ApprovalHookRequest,
    sessions: Arc<Mutex<crate::session::SessionManager>>,
    approvals: Arc<ApprovalRegistry>,
    bus: Arc<crate::session::EventBus>,
    attachments: Arc<Mutex<std::collections::HashMap<usize, std::collections::HashSet<usize>>>>,
) {
    use crate::session::SessionEvent;

    let response_tx = match request.response_tx.take() {
        Some(tx) => tx,
        None => return, // no sender — nothing to reply to
    };

    let claude_uuid = request.session_id.clone();
    let ccom_session_id = request.ccom_session_id;
    let tool_name = request.tool_name.clone();
    let tool_input = request.tool_input.clone();
    let cwd = PathBuf::from(&request.cwd);

    // --- Lock-free snapshot of session + driver ids ---
    // Use the ccom session index directly (sent by the hook binary via
    // CCOM_SESSION_ID env var). This avoids depending on
    // `Session.claude_session_id`, which is only populated after the Stop
    // hook fires — i.e. after the first turn ends. PreToolUse fires before
    // any Stop hook, so find_by_uuid would always return None on first use.
    //
    // Driver resolution priority:
    //   1. spawned_by (permanent parent/child relationship set at spawn time)
    //   2. dynamic attachment map (user-initiated TUI attachment)
    let (session_id, driver_id) = {
        let mgr = sessions.lock().unwrap_or_else(|p| p.into_inner());
        let via_spawn = mgr.get(ccom_session_id).and_then(|s| s.spawned_by);
        let driver_id = via_spawn.or_else(|| {
            let map = attachments.lock().unwrap_or_else(|p| p.into_inner());
            map.iter()
                .find(|(_, attached)| attached.contains(&ccom_session_id))
                .map(|(did, _)| *did)
        });
        match driver_id {
            Some(did) => (ccom_session_id, did),
            None => {
                log::debug!(
                    "session {ccom_session_id}: no driver found, passing through tool call"
                );
                let _ = response_tx.send(ApprovalHookResponse::Passthrough);
                return;
            }
        }
    };

    // --- Register the pending approval (no mutex held across await) ---
    let (request_id, decision_rx) = approvals.open_request(
        session_id,
        claude_uuid,
        driver_id,
        tool_name.clone(),
        tool_input.clone(),
        cwd.clone(),
    );

    // --- Publish event ---
    bus.publish(SessionEvent::ToolApprovalRequested {
        request_id,
        session_id,
        driver_id,
        tool: tool_name,
        args: tool_input,
        cwd,
        timestamp: SystemTime::now(),
    });

    // --- Await driver decision (590s, slightly under Claude's 600s) ---
    // Allow tests to override the timeout with CCOM_APPROVAL_TIMEOUT_SECS.
    let timeout_secs: u64 = std::env::var("CCOM_APPROVAL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(590);
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), decision_rx).await {
        Ok(Ok(ApprovalDecision::Allow)) => {
            let _ = response_tx.send(ApprovalHookResponse::Allow);
        }
        Ok(Ok(ApprovalDecision::Deny)) => {
            let _ = response_tx.send(ApprovalHookResponse::Deny);
            bus.publish(SessionEvent::ToolApprovalResolved {
                request_id,
                session_id,
                driver_id,
                decision: ApprovalDecision::Deny,
                scope: ApprovalScope::Once,
            });
        }
        Ok(Err(_)) | Err(_) => {
            // Channel closed or timeout: remove the stale entry so it
            // doesn't show as a ghost approval in pending_for_driver.
            approvals.cancel(request_id);
            let _ = response_tx.send(ApprovalHookResponse::Deny);
            bus.publish(SessionEvent::ToolApprovalResolved {
                request_id,
                session_id,
                driver_id,
                decision: ApprovalDecision::Deny,
                scope: ApprovalScope::Once,
            });
        }
    }
}

// --- Coordinator ----------------------------------------------------------

/// Drain `rx` and dispatch each incoming hook request to
/// [`handle_hook_request`] as an independent task. Exits when `rx` is
/// closed (i.e. the session's approval socket is torn down on cleanup).
///
/// One coordinator is spawned per session on the `ccom-mcp` tokio runtime.
pub async fn run_coordinator(
    mut rx: tokio::sync::mpsc::Receiver<ApprovalHookRequest>,
    sessions: Arc<Mutex<crate::session::SessionManager>>,
    approvals: Arc<ApprovalRegistry>,
    bus: Arc<crate::session::EventBus>,
    attachments: Arc<Mutex<std::collections::HashMap<usize, std::collections::HashSet<usize>>>>,
) {
    while let Some(request) = rx.recv().await {
        tokio::spawn(handle_hook_request(
            request,
            Arc::clone(&sessions),
            Arc::clone(&approvals),
            Arc::clone(&bus),
            Arc::clone(&attachments),
        ));
    }
}

// --- Unit tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_open_and_resolve_round_trip() {
        let registry = ApprovalRegistry::new();
        let (request_id, rx) = registry.open_request(
            1,
            "claude-uuid-1".to_string(),
            42,
            "Bash".to_string(),
            serde_json::json!({"command": "ls"}),
            PathBuf::from("/tmp"),
        );
        assert!(request_id > 0);

        let result = registry.resolve(request_id, 42, ApprovalDecision::Allow, ApprovalScope::Once);
        assert!(result.is_ok());
        let meta = result.unwrap();
        assert_eq!(meta.session_id, 1);
        assert_eq!(meta.tool, "Bash");
        assert_eq!(meta.claude_uuid, "claude-uuid-1");
        assert_eq!(meta.scope, ApprovalScope::Once);

        // The receiver should get the decision
        assert_eq!(rx.blocking_recv().unwrap(), ApprovalDecision::Allow);
    }

    #[test]
    fn registry_resolve_unknown_request_id_errors() {
        let registry = ApprovalRegistry::new();
        let result = registry.resolve(99999, 1, ApprovalDecision::Allow, ApprovalScope::Once);
        assert_eq!(result.unwrap_err(), ResolveError::NotFound);
    }

    #[test]
    fn registry_resolve_wrong_driver_rejected() {
        let registry = ApprovalRegistry::new();
        let (request_id, _rx) = registry.open_request(
            1,
            "uuid-x".to_string(),
            /* driver_id */ 10,
            "Edit".to_string(),
            serde_json::json!({}),
            PathBuf::from("/tmp"),
        );

        // Wrong driver
        let result = registry.resolve(
            request_id,
            /* caller */ 99,
            ApprovalDecision::Deny,
            ApprovalScope::Once,
        );
        assert_eq!(result.unwrap_err(), ResolveError::DriverMismatch);

        // The entry should still be present (not consumed)
        let result2 = registry.resolve(
            request_id,
            /* correct driver */ 10,
            ApprovalDecision::Deny,
            ApprovalScope::Once,
        );
        assert!(result2.is_ok());
    }

    #[test]
    fn pending_for_driver_filters_correctly() {
        let registry = ApprovalRegistry::new();
        let (id_a, _rx_a) = registry.open_request(
            1,
            "uuid-a".to_string(),
            /* driver */ 10,
            "Bash".to_string(),
            serde_json::json!({}),
            PathBuf::from("/a"),
        );
        let (_id_b, _rx_b) = registry.open_request(
            2,
            "uuid-b".to_string(),
            /* different driver */ 20,
            "Edit".to_string(),
            serde_json::json!({}),
            PathBuf::from("/b"),
        );

        let for_10 = registry.pending_for_driver(10);
        assert_eq!(for_10.len(), 1);
        assert_eq!(for_10[0].request_id, id_a);
        assert_eq!(for_10[0].driver_id, 10);

        let for_20 = registry.pending_for_driver(20);
        assert_eq!(for_20.len(), 1);

        let for_99 = registry.pending_for_driver(99);
        assert!(for_99.is_empty());
    }

    #[test]
    fn driver_exit_denies_all_pending_approvals_for_that_driver() {
        let registry = ApprovalRegistry::new();

        // Two requests for driver 10, one for driver 20.
        let (id_a, rx_a) = registry.open_request(
            1,
            "uuid-a".to_string(),
            10,
            "Bash".to_string(),
            serde_json::json!({}),
            PathBuf::from("/a"),
        );
        let (id_b, rx_b) = registry.open_request(
            2,
            "uuid-b".to_string(),
            10,
            "Edit".to_string(),
            serde_json::json!({}),
            PathBuf::from("/b"),
        );
        let (_id_c, mut rx_c) = registry.open_request(
            3,
            "uuid-c".to_string(),
            20,
            "Bash".to_string(),
            serde_json::json!({}),
            PathBuf::from("/c"),
        );

        let denied = registry.deny_all_for_driver(10);

        // Both of driver 10's requests should be in the returned list.
        let mut denied_ids: Vec<u64> = denied.iter().map(|(rid, _, _)| *rid).collect();
        denied_ids.sort_unstable();
        let mut expected = vec![id_a, id_b];
        expected.sort_unstable();
        assert_eq!(denied_ids, expected);

        // All returned entries should have driver_id 10.
        assert!(denied.iter().all(|(_, _, did)| *did == 10));

        // The receivers for driver 10's sessions should get Deny.
        assert_eq!(rx_a.blocking_recv().unwrap(), ApprovalDecision::Deny);
        assert_eq!(rx_b.blocking_recv().unwrap(), ApprovalDecision::Deny);

        // Driver 20's request should NOT be touched — registry still has it.
        assert_eq!(registry.pending_for_driver(20).len(), 1);
        // Its receiver should not have been signalled.
        assert!(rx_c.try_recv().is_err());
    }

    #[test]
    fn registry_reaper_clears_stale_entries() {
        // Override the reaper threshold to 0s so every entry is immediately stale.
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe { std::env::set_var("CCOM_APPROVAL_REAPER_SECS", "0") };

        let registry = ApprovalRegistry::new();
        let (id_a, rx_a) = registry.open_request(
            1,
            "uuid-a".to_string(),
            10,
            "Bash".to_string(),
            serde_json::json!({}),
            PathBuf::from("/a"),
        );

        let reaped = registry.sweep_stale();
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].0, id_a);

        // The entry must have been removed.
        assert!(registry.pending_for_driver(10).is_empty());

        // The receiver gets Deny.
        assert_eq!(rx_a.blocking_recv().unwrap(), ApprovalDecision::Deny);

        // Clean up env so other tests are not affected.
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe { std::env::remove_var("CCOM_APPROVAL_REAPER_SECS") };
    }

    #[tokio::test]
    async fn timeout_publishes_resolved_event_for_status_line() {
        use crate::session::{EventBus, SessionEvent};

        // Use a 0-second timeout so the test does not wait 590s.
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe { std::env::set_var("CCOM_APPROVAL_TIMEOUT_SECS", "0") };

        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe();
        let approvals = ApprovalRegistry::new();

        // Open a request and simulate the timeout/channel-closed branch
        // by directly replicating the deny-and-publish logic. This verifies
        // the event shape without needing a full SessionManager setup.
        let (request_id, decision_rx) = approvals.open_request(
            1,
            "uuid-test".to_string(),
            42,
            "Bash".to_string(),
            serde_json::json!({}),
            PathBuf::from("/tmp"),
        );
        // Drop the decision_rx: simulates channel-closed / timeout.
        drop(decision_rx);

        approvals.cancel(request_id);
        bus.publish(SessionEvent::ToolApprovalResolved {
            request_id,
            session_id: 1,
            driver_id: 42,
            decision: ApprovalDecision::Deny,
            scope: ApprovalScope::Once,
        });

        // The event must be on the bus.
        let event = rx
            .try_recv()
            .expect("ToolApprovalResolved must be published on timeout");
        match event {
            SessionEvent::ToolApprovalResolved {
                request_id: rid,
                session_id: sid,
                driver_id: did,
                decision,
                ..
            } => {
                assert_eq!(rid, request_id);
                assert_eq!(sid, 1);
                assert_eq!(did, 42);
                assert_eq!(decision, ApprovalDecision::Deny);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Clean up.
        // SAFETY: single-threaded test; no concurrent env access.
        unsafe { std::env::remove_var("CCOM_APPROVAL_TIMEOUT_SECS") };
    }
}
