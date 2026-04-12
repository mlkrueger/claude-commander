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

use std::collections::VecDeque;
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

/// Bounded per-session store of [`StoredTurn`]s.
///
/// Eviction policy: oldest-first while `total_bytes > budget_bytes`
/// **and** `len > min_retain`. The floor takes precedence so a single
/// oversized turn can never push the store below `min_retain`.
pub struct ResponseStore {
    turns: VecDeque<StoredTurn>,
    total_bytes: usize,
    budget_bytes: usize,
    min_retain: usize,
}

impl ResponseStore {
    /// Construct a `ResponseStore` with the default budget
    /// ([`DEFAULT_BUDGET_BYTES`]) and default minimum retention floor
    /// ([`DEFAULT_MIN_RETAIN`]).
    pub fn new() -> Self {
        Self::with_budget(DEFAULT_BUDGET_BYTES, DEFAULT_MIN_RETAIN)
    }

    /// Construct a `ResponseStore` with custom byte budget and
    /// minimum retention floor. The store evicts oldest-first while
    /// `total_bytes > budget_bytes` AND `len > min_retain` — the
    /// `min_retain` floor takes precedence so an oversized turn never
    /// causes the store to drop below the floor.
    pub fn with_budget(budget_bytes: usize, min_retain: usize) -> Self {
        Self {
            turns: VecDeque::new(),
            total_bytes: 0,
            budget_bytes,
            min_retain,
        }
    }

    /// Look up a completed turn by id. Returns `None` if the turn
    /// has been evicted, never existed, or is still in progress.
    #[allow(dead_code)] // first production caller is Phase 3 Task 5 (get_response accessor)
    pub fn get(&self, turn_id: TurnId) -> Option<&StoredTurn> {
        self.turns.iter().find(|t| t.turn_id == turn_id)
    }

    /// Return the most recently completed turn, if any. Useful for
    /// the "I subscribed late and want to see the latest response"
    /// recovery path described in the design spec §2.
    #[allow(dead_code)] // first production caller is Phase 3 Task 5 (get_latest_response accessor)
    pub fn latest(&self) -> Option<&StoredTurn> {
        self.turns.back()
    }

    /// Number of stored turns. Useful for tests asserting eviction
    /// behavior.
    #[allow(dead_code)] // exposed for Phase 3 Task 2 store tests
    pub fn len(&self) -> usize {
        self.turns.len()
    }

    /// Total bytes currently held in the store (sum of every
    /// `StoredTurn::body.len()`). Useful for tests asserting budget
    /// enforcement.
    #[allow(dead_code)] // exposed for Phase 3 Task 2 store tests
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

impl Default for ResponseStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnSink for ResponseStore {
    fn push_turn(&mut self, turn: StoredTurn) {
        self.total_bytes += turn.body.len();
        self.turns.push_back(turn);

        while self.total_bytes > self.budget_bytes && self.turns.len() > self.min_retain {
            if let Some(evicted) = self.turns.pop_front() {
                self.total_bytes -= evicted.body.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_of_size(id: u64, body_bytes: usize) -> StoredTurn {
        StoredTurn {
            turn_id: TurnId::new(id),
            started_at: Instant::now(),
            completed_at: Some(Instant::now()),
            body: "x".repeat(body_bytes),
        }
    }

    #[test]
    fn new_store_is_empty() {
        let store = ResponseStore::new();
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
        assert!(store.latest().is_none());
    }

    #[test]
    fn with_budget_zero_min_retain_is_allowed() {
        let store = ResponseStore::with_budget(100, 0);
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn push_turn_appends_and_increases_total_bytes() {
        let mut store = ResponseStore::new();
        let turn = turn_of_size(1, 50);
        store.push_turn(turn.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), 50);
        assert_eq!(store.latest(), Some(&turn));
        assert_eq!(store.get(TurnId::new(1)), Some(&turn));
    }

    #[test]
    fn get_unknown_turn_id_returns_none() {
        let mut store = ResponseStore::new();
        store.push_turn(turn_of_size(1, 10));
        assert!(store.get(TurnId::new(42)).is_none());
    }

    #[test]
    fn latest_returns_most_recently_pushed() {
        let mut store = ResponseStore::new();
        store.push_turn(turn_of_size(0, 10));
        store.push_turn(turn_of_size(1, 10));
        store.push_turn(turn_of_size(2, 10));
        assert_eq!(store.latest().unwrap().turn_id, TurnId::new(2));
    }

    #[test]
    fn push_below_budget_does_not_evict() {
        let mut store = ResponseStore::with_budget(1000, 0);
        for id in 0..3 {
            store.push_turn(turn_of_size(id, 100));
        }
        assert_eq!(store.len(), 3);
        assert_eq!(store.total_bytes(), 300);
        for id in 0..3 {
            assert!(store.get(TurnId::new(id)).is_some());
        }
    }

    #[test]
    fn push_over_budget_evicts_oldest_first() {
        let mut store = ResponseStore::with_budget(200, 0);
        for id in 0..4 {
            store.push_turn(turn_of_size(id, 100));
        }
        assert_eq!(store.len(), 2);
        assert_eq!(store.total_bytes(), 200);
        assert!(store.get(TurnId::new(0)).is_none());
        assert!(store.get(TurnId::new(1)).is_none());
        assert!(store.get(TurnId::new(2)).is_some());
        assert!(store.get(TurnId::new(3)).is_some());
    }

    #[test]
    fn min_retain_floor_protects_against_budget() {
        let mut store = ResponseStore::with_budget(100, 3);
        for id in 0..4 {
            store.push_turn(turn_of_size(id, 100));
        }
        assert_eq!(store.len(), 3);
        assert_eq!(store.total_bytes(), 300);
        assert!(store.get(TurnId::new(0)).is_none());
        assert!(store.get(TurnId::new(1)).is_some());
        assert!(store.get(TurnId::new(2)).is_some());
        assert!(store.get(TurnId::new(3)).is_some());
    }

    #[test]
    fn single_oversized_turn_is_retained_when_below_min_retain() {
        let mut store = ResponseStore::with_budget(100, 3);
        store.push_turn(turn_of_size(7, 500));
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), 500);
        assert!(store.get(TurnId::new(7)).is_some());
    }

    #[test]
    fn min_retain_three_with_one_huge_turn_keeps_huge_plus_two_more() {
        let mut store = ResponseStore::with_budget(100, 3);
        store.push_turn(turn_of_size(0, 10));
        store.push_turn(turn_of_size(1, 10));
        store.push_turn(turn_of_size(2, 1000));
        store.push_turn(turn_of_size(3, 10));
        store.push_turn(turn_of_size(4, 10));
        assert_eq!(store.len(), 3);
        assert_eq!(store.total_bytes(), 1020);
        assert!(store.get(TurnId::new(0)).is_none());
        assert!(store.get(TurnId::new(1)).is_none());
        assert!(store.get(TurnId::new(2)).is_some());
        assert!(store.get(TurnId::new(3)).is_some());
        assert!(store.get(TurnId::new(4)).is_some());
    }

    #[test]
    fn zero_min_retain_evicts_below_budget_aggressively() {
        let mut store = ResponseStore::with_budget(50, 0);
        for id in 0..3 {
            store.push_turn(turn_of_size(id, 100));
        }
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn total_bytes_matches_sum_of_bodies_after_evictions() {
        let mut store = ResponseStore::with_budget(500, 0);
        let sizes = [30, 70, 50, 120, 40, 80, 60, 90, 110, 25];
        for (id, size) in sizes.iter().enumerate() {
            store.push_turn(turn_of_size(id as u64, *size));
        }

        let reported = store.total_bytes();
        let mut summed = 0usize;
        for id in 0..sizes.len() as u64 {
            if let Some(t) = store.get(TurnId::new(id)) {
                summed += t.body.len();
            }
        }
        assert_eq!(reported, summed);
        assert!(reported <= 500);

        let before = store.total_bytes();
        let before_len = store.len();
        store.push_turn(turn_of_size(100, 40));
        // After pushing a new 40-byte turn, total_bytes must equal
        // (before + 40) minus any evicted bytes. Recompute from scratch:
        let mut fresh = 0usize;
        for id in 0..=100u64 {
            if let Some(t) = store.get(TurnId::new(id)) {
                fresh += t.body.len();
            }
        }
        assert_eq!(store.total_bytes(), fresh);
        assert!(store.total_bytes() <= 500);
        // Sanity: either we grew by 40 (no eviction) or we stayed flat /
        // shrank (eviction triggered).
        let _ = (before, before_len);
    }

    #[test]
    fn turn_sink_impl_delegates_to_push_turn() {
        let mut store = ResponseStore::new();
        {
            let sink: &mut dyn TurnSink = &mut store;
            sink.push_turn(turn_of_size(9, 20));
        }
        assert_eq!(store.len(), 1);
        assert_eq!(store.latest().unwrap().turn_id, TurnId::new(9));
    }

    #[test]
    fn default_matches_new() {
        let store = ResponseStore::default();
        assert_eq!(store.len(), 0);
        assert_eq!(store.total_bytes(), 0);
        assert!(store.latest().is_none());
    }
}
