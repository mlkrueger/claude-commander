//! Shared-state adapter for MCP tool handlers.
//!
//! Handlers receive an `Arc<ReadOnlyCtx>` that gives them snapshot
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

use crate::session::{EventBus, SessionManager, StoredTurn, TurnId};

/// Read-only context handed to each MCP tool handler instance.
///
/// The `sessions` field wraps the top-level `SessionManager` in an
/// `Arc<Mutex<…>>` so it can be shared between the main TUI thread
/// and the `ccom-mcp` thread. The `bus` is already `Arc<EventBus>`
/// from Phase 1 — no extra wrapping.
#[derive(Clone)]
pub struct ReadOnlyCtx {
    pub sessions: Arc<Mutex<SessionManager>>,
    pub bus: Arc<EventBus>,
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

impl ReadOnlyCtx {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionManager};

    #[test]
    fn list_sessions_empty_returns_empty_vec() {
        let bus = Arc::new(EventBus::new());
        let mgr = SessionManager::with_bus(Arc::clone(&bus));
        let ctx = ReadOnlyCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
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

        let ctx = ReadOnlyCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
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
