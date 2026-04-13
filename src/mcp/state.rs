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

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::confirm::ConfirmBridge;
// `SessionRole` will be consumed by Task 4 when `caller_scope` gains
// the real body; kept on the import line + suppressed so the MCP
// subagent doesn't have to add it back.
#[allow(unused_imports)]
use crate::session::{EventBus, SessionManager, SessionRole, StoredTurn, TurnId};

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
    /// Phase 6 prelude: shared driver-attachment map. Keyed by
    /// driver session id → set of session ids the user has manually
    /// attached to that driver via the TUI (Task 5). Read by
    /// `caller_scope` when resolving a driver caller's visible set;
    /// written only by the main TUI thread. Same `Arc<Mutex<_>>` as
    /// `App::attachment_map` — the shared-pointer contract is
    /// load-bearing.
    ///
    /// `#[allow(dead_code)]` because the bin-target reachability
    /// graph doesn't see the real reader until Task 4's
    /// `caller_scope` body lands. Test constructors and tests in
    /// `state.rs` already reference it, but those don't satisfy
    /// the bin lint on their own.
    #[allow(dead_code)]
    pub attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>>,
}

/// Phase 6 prelude: MCP caller scope — the set of session ids a
/// tool call is allowed to see and touch.
///
/// Resolved by [`McpCtx::caller_scope`]. Currently returns
/// [`Scope::Full`] for every caller (the full-fledged role-based
/// logic lands in Task 4, which reads `Session::role`, unions in
/// `spawned_by` children, and adds explicit attachments). Stubbed
/// now so the type surface is in place for the MCP-side subagent
/// to consume without the TUI-side subagent needing coordinated
/// edits.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Scope {
    /// Solo caller — may observe and mutate every session in the
    /// manager (the Phase 1–5 default, and currently the only
    /// path the stub returns).
    Full,
    /// Driver caller — restricted to the explicit set of session
    /// ids this driver owns (via `spawned_by`) or has attached
    /// (via the shared attachment map).
    Restricted(HashSet<usize>),
}

#[allow(dead_code)]
impl Scope {
    /// Convenience: does this scope permit access to `session_id`?
    /// `Full` always permits; `Restricted` checks membership.
    pub fn permits(&self, session_id: usize) -> bool {
        match self {
            Self::Full => true,
            Self::Restricted(set) => set.contains(&session_id),
        }
    }
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

    /// Phase 6 prelude — STUB. Resolve a caller ccom session id to
    /// the `Scope` of sessions it may see and touch.
    ///
    /// Current behavior: returns [`Scope::Full`] for every caller.
    /// This is the type-surface placeholder — Task 4's subagent will
    /// replace the body with the real role-based logic:
    ///
    /// - Solo caller (or caller not found in the manager) → `Full`
    /// - Driver caller → `Restricted(own_children ∪ attachments)`
    ///
    /// Keeping the stub in place now lets the MCP-side subagent's
    /// scope-filter work (gated on this return value) compile and
    /// pass tests before the real logic lands — tests that need
    /// actual filtering will construct `Scope::Restricted(..)`
    /// directly in fixtures and bypass this helper.
    ///
    /// `caller_id` is currently ignored; `_` to silence the lint.
    #[allow(dead_code)]
    pub fn caller_scope(&self, _caller_id: usize) -> Scope {
        // Task 4: read caller's `SessionRole`, branch on
        // Solo → Full, Driver → Restricted(..); for now, punt.
        Scope::Full
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
            attachments: Arc::new(Mutex::new(HashMap::new())),
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
            attachments: Arc::new(Mutex::new(HashMap::new())),
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
