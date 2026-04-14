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
    /// Phase 6 Task 3: event channel the `spawn_session` handler
    /// hands to newly-spawned sessions via their `SpawnConfig`. Same
    /// sender App uses for its own spawns — so PTY output from
    /// MCP-created sessions reaches the main TUI event loop. `None`
    /// in test contexts that never exercise `spawn_session`.
    #[allow(dead_code)]
    pub event_tx: Option<crate::event::MonitoredSender>,
}

/// Phase 6 MCP caller scope — the set of session ids a tool call
/// is allowed to see and touch. Resolved by [`McpCtx::caller_scope`]
/// from the caller's [`SessionRole`], the `spawned_by` parent
/// pointers in the manager, and the shared TUI attachment map.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Scope {
    /// Solo caller — may observe and mutate every session in the
    /// manager. This is the Phase 1–5 default and is also what
    /// an unknown caller id resolves to (legacy MCP clients that
    /// don't set the `X-Ccom-Caller` header).
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

    /// Resolve a caller ccom session id to its [`Scope`] — the set
    /// of sessions this caller is allowed to observe and mutate.
    ///
    /// - Solo caller → [`Scope::Full`] (the Phase 1–5 default).
    /// - Driver caller → [`Scope::Restricted`] containing the driver
    ///   itself, every session with `spawned_by == Some(caller_id)`,
    ///   and every id in the shared attachment map under the
    ///   driver's entry (populated by the TUI in Task 5).
    /// - Unknown caller id → [`Scope::Full`]. This is the legacy
    ///   pre-Phase-6 path for MCP clients that don't set the
    ///   `X-Ccom-Caller` header (direct HTTP clients, unit tests) —
    ///   they continue to see every session as before.
    pub fn caller_scope(&self, caller_id: usize) -> Scope {
        let mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let Some(caller) = mgr.get(caller_id) else {
            return Scope::Full;
        };
        match &caller.role {
            SessionRole::Solo => Scope::Full,
            SessionRole::Driver { .. } => {
                // Driver owns every session it spawned plus itself.
                // Spawned-by is the ground-truth parent pointer set
                // atomically in `SessionManager::spawn_with_role`.
                let mut scope: HashSet<usize> = mgr
                    .iter()
                    .filter(|s| s.spawned_by == Some(caller_id))
                    .map(|s| s.id)
                    .collect();
                scope.insert(caller_id);
                // Merge in any explicit attachments from the TUI side
                // (Task 5). The attachment map is the shared
                // `Arc<Mutex<_>>` owned by `App` — writes happen only
                // on the main thread; we just read a snapshot here.
                if let Some(attached) = self
                    .attachments
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&caller_id)
                {
                    scope.extend(attached.iter().copied());
                }
                Scope::Restricted(scope)
            }
        }
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
    use crate::session::{Session, SessionManager, SpawnPolicy};

    fn driver_ctx_with(sessions: SessionManager) -> Arc<McpCtx> {
        let bus = Arc::new(EventBus::new());
        Arc::new(McpCtx {
            sessions: Arc::new(Mutex::new(sessions)),
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(HashMap::new())),
            event_tx: None,
        })
    }

    #[test]
    fn list_sessions_empty_returns_empty_vec() {
        let bus = Arc::new(EventBus::new());
        let mgr = SessionManager::with_bus(Arc::clone(&bus));
        let ctx = McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(HashMap::new())),
            event_tx: None,
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
            event_tx: None,
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

    // ------------------------------------------------------------------
    // Phase 6 Task 4 — `caller_scope` unit tests.
    //
    // Every test pushes sessions via `push_for_test` (which bumps
    // `next_id` past the pushed id) and then consults `caller_scope`
    // directly. The attachment map is reached through `McpCtx`.
    // ------------------------------------------------------------------

    #[test]
    fn caller_scope_returns_full_for_solo_caller() {
        let mut mgr = SessionManager::new();
        mgr.push_for_test(Session::dummy_exited(1, "solo"));
        let ctx = driver_ctx_with(mgr);
        assert_eq!(ctx.caller_scope(1), Scope::Full);
    }

    #[test]
    fn caller_scope_returns_full_for_unknown_caller_id() {
        // Legacy path: no header, or an id the manager doesn't know.
        let ctx = driver_ctx_with(SessionManager::new());
        assert_eq!(ctx.caller_scope(999), Scope::Full);
    }

    #[test]
    fn caller_scope_returns_restricted_for_driver_caller() {
        let mut mgr = SessionManager::new();
        mgr.push_for_test(
            Session::dummy_exited(1, "orch").with_role(SessionRole::Driver {
                spawn_budget: 2,
                spawn_policy: SpawnPolicy::Budget,
            }),
        );
        let ctx = driver_ctx_with(mgr);
        match ctx.caller_scope(1) {
            Scope::Restricted(set) => assert!(set.contains(&1)),
            other => panic!("expected Restricted, got {other:?}"),
        }
    }

    #[test]
    fn caller_scope_includes_driver_itself() {
        let mut mgr = SessionManager::new();
        mgr.push_for_test(
            Session::dummy_exited(7, "orch").with_role(SessionRole::Driver {
                spawn_budget: 0,
                spawn_policy: SpawnPolicy::Ask,
            }),
        );
        let ctx = driver_ctx_with(mgr);
        let scope = ctx.caller_scope(7);
        assert!(
            scope.permits(7),
            "driver must be in its own scope: {scope:?}"
        );
    }

    #[test]
    fn caller_scope_includes_spawned_by_children() {
        let mut mgr = SessionManager::new();
        mgr.push_for_test(
            Session::dummy_exited(1, "orch").with_role(SessionRole::Driver {
                spawn_budget: 5,
                spawn_policy: SpawnPolicy::Trust,
            }),
        );
        mgr.push_for_test(Session::dummy_exited(2, "child-a").with_spawned_by(1));
        mgr.push_for_test(Session::dummy_exited(3, "child-b").with_spawned_by(1));
        // Unrelated peer, NOT spawned by the driver.
        mgr.push_for_test(Session::dummy_exited(4, "stranger"));

        let ctx = driver_ctx_with(mgr);
        let scope = ctx.caller_scope(1);
        assert!(scope.permits(1));
        assert!(scope.permits(2));
        assert!(scope.permits(3));
        assert!(
            !scope.permits(4),
            "stranger must not be in scope: {scope:?}"
        );
    }

    #[test]
    fn caller_scope_includes_attachments() {
        let mut mgr = SessionManager::new();
        mgr.push_for_test(
            Session::dummy_exited(1, "orch").with_role(SessionRole::Driver {
                spawn_budget: 0,
                spawn_policy: SpawnPolicy::Ask,
            }),
        );
        mgr.push_for_test(Session::dummy_exited(9, "attached-peer"));
        let ctx = driver_ctx_with(mgr);
        // Simulate the TUI attaching session 9 to driver 1.
        {
            let mut map = ctx.attachments.lock().unwrap();
            map.entry(1).or_default().insert(9);
        }
        let scope = ctx.caller_scope(1);
        assert!(scope.permits(9));
    }
}
