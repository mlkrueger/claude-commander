//! Shared-state adapter for MCP tool handlers.
//!
//! Handlers receive an `Arc<McpCtx>` that gives them snapshot
//! access to the bits of `SessionManager` and `EventBus` they need.
//! The ctx holds `Arc`s, not owned data — cloning is cheap, and
//! snapshots are taken at tool-call time under a short critical
//! section.
//!
//! **Critical constraint:** tool handlers MUST NOT hold
//! `sessions.lock()` across `.await` points. Snapshot into owned
//! `SessionSummary` / `StoredTurn` values, release the lock, then
//! return.

use std::sync::{Arc, Mutex};

use super::confirm::ConfirmBridge;
use crate::session::{EventBus, SessionManager, StoredTurn, TurnId};

/// Shared context handed to each MCP tool handler instance.
///
/// The `sessions` field wraps the top-level `SessionManager` in an
/// `Arc<Mutex<…>>` so it can be shared between the main TUI thread
/// and the `ccom-mcp` thread. The `bus` is already `Arc<EventBus>`
/// from Phase 1 — no extra wrapping.
///
/// Phase 5 adds write tools (`send_prompt`, `kill_session`) that
/// mutate session state; the earlier name `ReadOnlyCtx` became
/// misleading and was renamed to `McpCtx`. The contract still holds
/// that handlers must not hold `sessions.lock()` across `.await`
/// points — take snapshots and release the lock before returning.
///
/// `confirm` is the cross-thread confirmation bridge added in
/// Phase 5 Task 2. `kill_session` uses it to block on a user TUI
/// modal before the destructive operation goes through. `Option<>`
/// so tests and non-TUI harnesses can construct an `McpCtx` without
/// wiring up the full App-side machinery; production (`App::new`)
/// always provides it.
#[derive(Clone)]
pub struct McpCtx {
    pub sessions: Arc<Mutex<SessionManager>>,
    pub bus: Arc<EventBus>,
    pub confirm: Option<Arc<ConfirmBridge>>,
}

/// Snapshot of a single session for `list_sessions` MCP tool.
///
/// Cheap-to-serialize, owns no references. Constructed from
/// [`SessionManager::iter`] under a short critical section so the
/// tool handler returns after releasing the session lock.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    pub id: usize,
    pub label: String,
    pub working_dir: String,
    pub status: String,
    pub last_activity_secs: u64,
    pub context_percent: Option<f64>,
}

impl McpCtx {
    /// Snapshot every live session into a `Vec<SessionSummary>`.
    /// Called by the `list_sessions` MCP tool. The lock is held only
    /// for the duration of the iteration; the returned vec owns its
    /// contents so the caller is free to `.await` afterward.
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        mgr.iter()
            .map(|s| SessionSummary {
                id: s.id,
                label: s.label.clone(),
                working_dir: s.working_dir.to_string_lossy().into_owned(),
                status: format!("{:?}", s.status),
                last_activity_secs: s.last_activity.elapsed().as_secs(),
                context_percent: s.context_percent,
            })
            .collect()
    }

    /// Fetch a stored turn body. If `turn_id` is `Some`, looks up
    /// that specific turn; if `None`, returns the most recently
    /// completed turn for the session. Returns a clone so the caller
    /// doesn't hold a borrow into the session's store across the
    /// released lock.
    pub fn get_response(&self, session_id: usize, turn_id: Option<TurnId>) -> Option<StoredTurn> {
        let mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        match turn_id {
            Some(tid) => mgr.get_response(session_id, tid),
            None => mgr.get_latest_response(session_id),
        }
    }

    /// Scope-checked wrapper over [`SessionManager::send_prompt`].
    ///
    /// Returns [`SendPromptRejection::NotFound`] if the session id is
    /// unknown to the TUI's manager, *before* the text reaches the
    /// PTY. Caller is expected to have already sanitized `text` via
    /// [`super::sanitize::sanitize_prompt_text`].
    pub fn send_prompt(
        &self,
        session_id: usize,
        text: &str,
    ) -> Result<TurnId, SendPromptRejection> {
        let mut mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if mgr.get(session_id).is_none() {
            return Err(SendPromptRejection::NotFound);
        }
        // `SessionManager::send_prompt` only fails on unknown id
        // (which we just excluded) so remap any residual error back
        // to `NotFound` for a single uniform rejection shape.
        mgr.send_prompt(session_id, text)
            .map_err(|_| SendPromptRejection::NotFound)
    }
}

/// Reasons the `send_prompt` MCP tool may refuse to deliver a prompt.
///
/// Currently there is only one variant — the session isn't owned by
/// the TUI's `SessionManager`. Sanitization-level rejections (empty,
/// oversized, bad bytes) are returned by
/// [`super::sanitize::sanitize_prompt_text`] as `String`s and never
/// materialize into this enum.
#[derive(Debug)]
pub enum SendPromptRejection {
    /// No session with this id is currently owned by the TUI.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionManager};

    #[test]
    fn list_sessions_empty_returns_empty_vec() {
        let bus = Arc::new(EventBus::new());
        let mgr = SessionManager::with_bus(Arc::clone(&bus));
        let ctx = McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            confirm: None,
        };
        assert!(ctx.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions_snapshots_populated_manager() {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
        let id_a = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_a, "alpha"));
        let id_b = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_b, "beta"));

        let ctx = McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            confirm: None,
        };
        let summaries = ctx.list_sessions();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, id_a);
        assert_eq!(summaries[0].label, "alpha");
        assert_eq!(summaries[1].id, id_b);
        assert_eq!(summaries[1].label, "beta");
        // `Session::dummy_exited` starts in Exited(0), which the
        // summary renders via the `Debug` impl of `SessionStatus`.
        assert!(summaries[0].status.contains("Exited"));
    }
}
