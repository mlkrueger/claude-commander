//! Bounded per-session store of completed prompt/response turns.
//!
//! Phase 3 of session-management. Written by the response boundary
//! detector at [`crate::pty::response_boundary`], read by
//! [`crate::session::SessionManager::get_response`] /
//! `get_latest_response` (added later in Phase 3).
//!
//! ## Design
//!
//! Marker-only events on the [`crate::session::EventBus`] carry only
//! `TurnId` — bodies live here. Pull-on-demand: subscribers that need
//! the body call `get_response(session_id, turn_id)` after seeing a
//! `ResponseComplete` event.
//!
//! The store is **bounded by total bytes per session** with a
//! **minimum retention floor of N turns regardless of size**, so a
//! single oversized response can't blow out all the useful recent
//! history. Default budget: 256 KB. Default min retention: 3 turns.
//!
//! See `docs/designs/session-management.md` §4 for the full rationale.
//!
//! ## Phase 3 Task 0 (this skeleton)
//!
//! `StoredTurn`, `TurnSink`, and the `ResponseStore` skeleton with
//! `todo!()` bodies are pre-staged on the phase branch so the
//! parallel Tasks 1 (real `ResponseStore` impl) and 3 (detector) can
//! be developed in separate worktrees without stepping on each other.
//! Task 1 replaces all `todo!()` calls in this file.

use std::time::Instant;

use crate::session::events::TurnId;

/// Default per-session response budget. 256 KB strikes a balance
/// between holding enough recent turns to be useful and not hoarding
/// multi-megabyte history per session. Override via
/// [`ResponseStore::with_budget`].
pub const DEFAULT_BUDGET_BYTES: usize = 256 * 1024;

/// Default minimum retention floor: always keep at least this many
/// turns regardless of size. Prevents a single oversized response
/// from evicting all useful recent history.
pub const DEFAULT_MIN_RETAIN: usize = 3;

/// One completed prompt/response round-trip captured by the response
/// boundary detector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTurn {
    /// Correlation key paired with `SessionEvent::PromptSubmitted` and
    /// `SessionEvent::ResponseComplete`.
    pub turn_id: TurnId,
    /// When the prompt was submitted.
    pub started_at: Instant,
    /// When the response boundary detector observed the turn complete.
    pub completed_at: Option<Instant>,
    /// ANSI-normalized response body. The detector strips control
    /// sequences before storing so consumers can render the body as
    /// plain text without re-processing.
    pub body: String,
}

/// Sink for completed turns. The response boundary detector writes
/// completed turns through this trait so its tests can use a
/// recording sink without spinning up a real [`ResponseStore`].
///
/// Production: `ResponseStore` implements this trait and is owned per
/// session. Tests: a `Vec<StoredTurn>`-backed mock implements this
/// trait so detector tests can assert on the exact sequence of pushes.
#[allow(dead_code)] // first method-call lands in Phase 3 Task 3 (detector)
pub trait TurnSink {
    /// Append a completed turn. Implementations may evict older
    /// entries to stay within their own budget; callers should not
    /// assume the pushed turn remains retrievable forever.
    fn push_turn(&mut self, turn: StoredTurn);
}

/// Bounded per-session store of [`StoredTurn`]s. Phase 3 Task 1
/// (parallel subagent) will replace this skeleton with the real
/// implementation.
#[allow(dead_code)] // first production caller is Phase 3 Task 5 (get_response accessor)
pub struct ResponseStore {
    // Skeleton placeholder. Phase 3 Task 1 replaces this with real
    // fields (a `VecDeque<StoredTurn>`, total_bytes counter, budget
    // params, etc.). Marked `pub(super)` so the same-module
    // implementation can mutate, but no caller outside the module
    // touches it directly — all access goes through the methods
    // below.
    pub(super) _phase_3_task_1_placeholder: (),
}

impl ResponseStore {
    /// Construct a `ResponseStore` with the default budget
    /// ([`DEFAULT_BUDGET_BYTES`]) and default minimum retention floor
    /// ([`DEFAULT_MIN_RETAIN`]).
    #[allow(dead_code)] // first production caller is Phase 3 Task 4 (PTY reader hook)
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_BUDGET_BYTES, DEFAULT_MIN_RETAIN)
    }

    /// Construct a `ResponseStore` with custom byte budget and
    /// minimum retention floor. The store evicts oldest-first while
    /// `total_bytes > budget_bytes` AND `len > min_retain` — the
    /// `min_retain` floor takes precedence so an oversized turn never
    /// causes the store to drop below the floor.
    #[allow(dead_code)] // first production caller is Phase 3 Task 4 (PTY reader hook)
    pub fn with_budget(budget_bytes: usize, min_retain: usize) -> Self {
        let _ = (budget_bytes, min_retain);
        todo!("Phase 3 Task 1 — ResponseStore::with_budget")
    }

    /// Look up a completed turn by id. Returns `None` if the turn
    /// has been evicted, never existed, or is still in progress.
    #[allow(dead_code)] // first production caller is Phase 3 Task 5 (get_response accessor)
    pub fn get(&self, turn_id: TurnId) -> Option<&StoredTurn> {
        let _ = turn_id;
        todo!("Phase 3 Task 1 — ResponseStore::get")
    }

    /// Return the most recently completed turn, if any. Useful for
    /// the "I subscribed late and want to see the latest response"
    /// recovery path described in the design spec §2.
    #[allow(dead_code)] // first production caller is Phase 3 Task 5 (get_latest_response accessor)
    pub fn latest(&self) -> Option<&StoredTurn> {
        todo!("Phase 3 Task 1 — ResponseStore::latest")
    }

    /// Number of stored turns. Useful for tests asserting eviction
    /// behavior.
    #[allow(dead_code)] // exposed for Phase 3 Task 2 store tests
    pub fn len(&self) -> usize {
        todo!("Phase 3 Task 1 — ResponseStore::len")
    }

    /// Total bytes currently held in the store (sum of every
    /// `StoredTurn::body.len()`). Useful for tests asserting budget
    /// enforcement.
    #[allow(dead_code)] // exposed for Phase 3 Task 2 store tests
    pub fn total_bytes(&self) -> usize {
        todo!("Phase 3 Task 1 — ResponseStore::total_bytes")
    }
}

impl Default for ResponseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnSink for ResponseStore {
    fn push_turn(&mut self, turn: StoredTurn) {
        let _ = turn;
        todo!("Phase 3 Task 1 — TurnSink for ResponseStore")
    }
}
