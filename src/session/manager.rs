//! Collection manager for live `Session`s.
//!
//! `SessionManager` owns the `Vec<Session>` and the selected-index cursor.
//! Callers go through its methods — the vec itself is private and never
//! exposed, so direct indexing into sessions cannot desynchronize `selected`
//! from the underlying storage.
//!
//! ## Invariant
//!
//! `sessions.is_empty() || selected < sessions.len()`
//!
//! This holds on construction and is re-established after every mutation
//! (`spawn`, `kill`, `select_prev`, `select_next`, `retain_alive`). In debug
//! builds it is checked by `debug_assert!` at the end of each mutating
//! method.
//!
//! ## Id allocation
//!
//! `next_id` increases monotonically and is never reused — killing a session
//! with id `N` does not free `N` for future spawns. Callers can rely on id
//! uniqueness across the lifetime of an `App`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;

use crate::event::Event;
use crate::pty::detector::PromptDetector;

use super::events::{EventBus, SessionEvent, TurnId};
use super::types::{Session, SessionStatus};

/// Byte sequence appended after a prompt's payload to submit it to the
/// underlying interactive runner. Currently `\r` (carriage return),
/// matching what `crate::app::key_event_to_bytes` emits for
/// `KeyCode::Enter` and what the existing `App::approve_selected`
/// already writes directly. Centralized here so
/// `SessionManager::send_prompt` and any future caller share one
/// definition; if Claude Code's submit chord ever changes, this is
/// the only line to update — and the `// MUST match SUBMIT_SEQUENCE`
/// comment in `crate::app::key_event_to_bytes` is the cross-reference.
//
// `#[allow(dead_code)]` because the binary's reachability analysis
// (`cargo build`) starts from `main` and doesn't reach `send_prompt`
// yet — its first production caller arrives in Council Phase 2. Tests
// reference this constant via the test target, which doesn't satisfy
// the binary lint. Pinned by `submit_sequence_is_carriage_return`.
#[allow(dead_code)]
pub(crate) const SUBMIT_SEQUENCE: &[u8] = b"\r";

/// Arguments for spawning a new `Session` through [`SessionManager::spawn`].
pub struct SpawnConfig<'a> {
    pub label: String,
    pub working_dir: PathBuf,
    pub command: &'a str,
    pub args: Vec<String>,
    pub event_tx: mpsc::Sender<Event>,
    pub cols: u16,
    pub rows: u16,
}

/// Outcome of a [`SessionManager::broadcast`] call. Reports which
/// session ids the broadcast attempted to write to and which ids were
/// not found in the manager. Order within each Vec matches input
/// order. Note that `sent` reports *attempts*, not delivery — see
/// the doc on [`SessionManager::broadcast`] for the `try_write`
/// failure caveat.
//
// `#[allow(dead_code)]` until the first production caller (Council
// Phase 2). The binary's lint pass doesn't see test references.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastResult {
    pub sent: Vec<usize>,
    pub not_found: Vec<usize>,
}

pub struct SessionManager {
    sessions: Vec<Session>,
    selected: usize,
    next_id: usize,
    bus: Arc<EventBus>,
}

// `impl Default for SessionManager` was removed in the PR #7
// post-review pass: `Default::default` is unconditionally `pub` via the
// trait, which would have provided an external escape hatch around the
// `pub(crate) fn new()` restriction below. Use `SessionManager::with_bus`
// from production and `SessionManager::new()` from tests.

impl SessionManager {
    /// Construct a `SessionManager` with a fresh internal `EventBus`.
    /// **Test-only.** Production code (`App::new`) must use
    /// [`SessionManager::with_bus`] so the top-level `App` owns the
    /// shared bus and can hand subscriptions to future Phase 2+
    /// consumers (Council, MCP server, stats panel).
    ///
    /// Gated to `#[cfg(test)]` and `pub(crate)` in the PR #7
    /// post-review pass: previously `pub`, which let external code
    /// (and `Default::default`) construct a `SessionManager` whose
    /// bus no one outside could ever subscribe to. The cfg gate makes
    /// the production constraint structural — `with_bus` is now the
    /// *only* externally-reachable constructor.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::with_bus(Arc::new(EventBus::new()))
    }

    /// Construct a `SessionManager` that publishes `SessionEvent`s onto
    /// the provided shared bus. Use this from production where the
    /// `App` owns the top-level bus and wants a single shared instance.
    pub fn with_bus(bus: Arc<EventBus>) -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            next_id: 1,
            bus,
        }
    }

    /// Return a shared reference to the event bus so callers (e.g. the
    /// main event loop, the MCP server in later phases, tests) can
    /// subscribe to `SessionEvent`s without holding a reference to the
    /// manager itself. Phase 1 only consumes this from tests; Phase 2+
    /// uses it from production code.
    #[allow(dead_code)]
    pub fn bus(&self) -> Arc<EventBus> {
        Arc::clone(&self.bus)
    }

    /// Compare each `(id, prior_status)` entry in `before` against the
    /// current session status and publish a `StatusChanged` event for
    /// any session whose status has actually changed. Additionally,
    /// when a session newly enters `WaitingForApproval`, publishes
    /// `PromptPending` once for that transition.
    ///
    /// Sessions present in `before` but no longer in the manager (e.g.
    /// killed mid-tick) are ignored.
    ///
    /// Private (`fn`, no `pub(crate)`): the only legitimate caller is
    /// `check_attention`. The child `mod tests` can still call this
    /// directly because Rust lets child modules reach private items of
    /// their parent. Tightened from `pub(crate)` in the PR #7
    /// post-review pass.
    fn publish_status_diffs(&self, before: &[(usize, SessionStatus)]) {
        for (id, old_status) in before {
            let Some(session) = self.sessions.iter().find(|s| s.id == *id) else {
                continue;
            };
            if session.status == *old_status {
                continue;
            }

            self.bus.publish(SessionEvent::StatusChanged {
                session_id: *id,
                status: session.status.clone(),
            });

            // PromptPending fires only on the *transition into*
            // WaitingForApproval — going WaitingForApproval(A) ->
            // WaitingForApproval(B) is still a status change, but we
            // already attention-flagged the session and don't want a
            // second nudge.
            if !matches!(old_status, SessionStatus::WaitingForApproval(_))
                && let SessionStatus::WaitingForApproval(kind) = &session.status
            {
                self.bus.publish(SessionEvent::PromptPending {
                    session_id: *id,
                    kind: kind.clone(),
                });
            }
        }
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Preview the id that will be assigned by the next `spawn()` call.
    /// Useful for building labels before spawning.
    pub fn peek_next_id(&self) -> usize {
        self.next_id
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn as_slice(&self) -> &[Session] {
        &self.sessions
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Session> {
        self.sessions.iter_mut()
    }

    /// Lookup a session by its id (NOT its vec index).
    pub fn get(&self, id: usize) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// Mutable lookup by id.
    pub fn get_mut(&mut self, id: usize) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// Submit `text` to the session identified by `id`, allocating a
    /// fresh `TurnId` and publishing `SessionEvent::PromptSubmitted` on
    /// the bus. Writes the prompt text followed by `SUBMIT_SEQUENCE` to
    /// the session's PTY.
    ///
    /// Returns `Err` if the id does not match any live session; in that
    /// case no turn id is allocated and no event is published.
    ///
    /// **PTY write failures are not detected.** [`Session::try_write`]
    /// is fire-and-forget — it logs failures and bumps
    /// `consecutive_write_failures`, but never returns an error to its
    /// caller. So if either the prompt-text write or the submit-byte
    /// write fails (broken pipe, dead child, etc.), `send_prompt`
    /// will still:
    ///   - allocate the `TurnId`
    ///   - publish `PromptSubmitted` on the bus
    ///   - return `Ok(turn_id)`
    /// even though no bytes reached the underlying process. Callers
    /// that need write-failure visibility should consult
    /// `Session::consecutive_write_failures` directly; the existing
    /// logic transitions a session to `Exited(-3)` after three
    /// consecutive failures, so a persistently broken session will
    /// surface via `SessionEvent::Exited` on the next `reap_exited`
    /// pass.
    ///
    /// PR #8 review item D1 made this limitation explicit. A future
    /// refactor may switch `try_write` to return a `Result` and
    /// propagate failures through `send_prompt` / `broadcast`; until
    /// then, treat the bus's `PromptSubmitted` event as "we attempted
    /// to submit," not "the runner has the bytes."
    //
    // `#[allow(dead_code)]` until the first production caller (Council
    // Phase 3 synthesizer). The binary's lint pass doesn't see test
    // references.
    #[allow(dead_code)]
    pub fn send_prompt(&mut self, id: usize, text: &str) -> anyhow::Result<TurnId> {
        let turn_id = {
            let session = self
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("session {id} not found"))?;
            let turn_id = session.allocate_turn_id();
            session.try_write(text.as_bytes());
            session.try_write(SUBMIT_SEQUENCE);
            turn_id
        };
        self.bus.publish(SessionEvent::PromptSubmitted {
            session_id: id,
            turn_id,
        });
        Ok(turn_id)
    }

    /// Write raw bytes to every session in `ids`, in order. Does not
    /// allocate a `TurnId`, does not publish `SessionEvent`s, does not
    /// dedupe `ids`. See `BroadcastResult` for the return shape and the
    /// Phase 2 design doc §3 for the rationale.
    ///
    /// **`sent` reports attempts, not delivery.** Same caveat as
    /// [`SessionManager::send_prompt`]: [`Session::try_write`] is
    /// fire-and-forget, so a session id appears in `result.sent` as
    /// long as `try_write` was called — even if the underlying PTY
    /// write actually failed. Callers needing per-session delivery
    /// visibility should consult `Session::consecutive_write_failures`
    /// after the broadcast. See PR #8 review item D1.
    //
    // `#[allow(dead_code)]` until the first production caller (Council
    // Phase 2 broadcast dispatch). The binary's lint pass doesn't see
    // test references.
    #[allow(dead_code)]
    pub fn broadcast(&mut self, ids: &[usize], bytes: &[u8]) -> BroadcastResult {
        let mut sent = Vec::new();
        let mut not_found = Vec::new();
        for &id in ids {
            match self.get_mut(id) {
                Some(session) => {
                    session.try_write(bytes);
                    sent.push(id);
                }
                None => not_found.push(id),
            }
        }
        BroadcastResult { sent, not_found }
    }

    /// Current selected index, or `None` when empty.
    pub fn selected_index(&self) -> Option<usize> {
        if self.sessions.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn selected(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    pub fn selected_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.selected)
    }

    /// Move selection to the previous session, wrapping at the start.
    pub fn select_prev(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.sessions.len() - 1);
        self.assert_invariant();
    }

    /// Move selection to the next session, wrapping at the end.
    pub fn select_next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.sessions.len();
        self.assert_invariant();
    }

    /// Move selection down by `n`, clamped to the last valid index.
    pub fn select_down_by(&mut self, n: usize) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + n).min(self.sessions.len() - 1);
        self.assert_invariant();
    }

    /// Move selection up by `n`, clamped to 0.
    pub fn select_up_by(&mut self, n: usize) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(n).min(self.sessions.len() - 1);
        self.assert_invariant();
    }

    /// Directly set the selected index; clamps to the last valid index.
    pub fn set_selected(&mut self, idx: usize) {
        if self.sessions.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = idx.min(self.sessions.len() - 1);
        self.assert_invariant();
    }

    /// Spawn a new session, append it, and select it. Returns the new id.
    pub fn spawn(&mut self, config: SpawnConfig<'_>) -> anyhow::Result<usize> {
        let id = self.next_id;
        self.next_id += 1;

        let arg_refs: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        let label_for_event = config.label.clone();

        let session = Session::spawn(
            id,
            config.label,
            config.working_dir,
            config.command,
            &arg_refs,
            config.event_tx,
            config.cols,
            config.rows,
        )?;

        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        self.bus.publish(SessionEvent::Spawned {
            session_id: id,
            label: label_for_event,
        });
        self.assert_invariant();
        Ok(id)
    }

    /// Kill the session with the given id and remove it from the collection.
    /// Returns `true` if a session was found and killed.
    ///
    /// Selection fix-up:
    /// - if the killed index was `< selected`, decrement `selected`
    /// - if it was `== selected`, clamp to the new last valid index
    /// - if it was `> selected`, leave `selected` unchanged
    pub fn kill(&mut self, id: usize) -> bool {
        let Some(idx) = self.sessions.iter().position(|s| s.id == id) else {
            return false;
        };
        self.sessions[idx].kill();
        // After `Session::kill`, status is `Exited(<code>)`. Capture
        // the code before removing the session so the published event
        // reflects what the killer set.
        let exit_code = match self.sessions[idx].status {
            SessionStatus::Exited(code) => code,
            _ => -1,
        };
        self.sessions.remove(idx);

        if self.sessions.is_empty() {
            self.selected = 0;
        } else if idx < self.selected {
            self.selected -= 1;
        } else if idx == self.selected && self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }

        self.bus.publish(SessionEvent::Exited {
            session_id: id,
            code: exit_code,
        });
        self.assert_invariant();
        true
    }

    /// Remove all sessions that have exited. Re-clamps `selected`.
    pub fn retain_alive(&mut self) {
        self.sessions
            .retain(|s| !matches!(s.status, SessionStatus::Exited(_)));
        if self.sessions.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }
        self.assert_invariant();
    }

    /// Refresh context-usage percentage for every live session.
    pub fn refresh_contexts(&mut self) {
        for session in &mut self.sessions {
            session.refresh_context();
        }
    }

    /// Run the attention detector against every live session and
    /// publish `StatusChanged` / `PromptPending` for any session whose
    /// status changes as a result.
    pub fn check_attention(&mut self, detector: &PromptDetector) {
        // Snapshot statuses before mutating so we can diff afterwards.
        let before: Vec<(usize, SessionStatus)> = self
            .sessions
            .iter()
            .map(|s| (s.id, s.status.clone()))
            .collect();

        for session in &mut self.sessions {
            if !matches!(session.status, SessionStatus::Exited(_)) {
                session.check_attention(detector);
            }
        }

        self.publish_status_diffs(&before);
    }

    /// Poll all live sessions for child exit status and mark any that have
    /// exited on their own since the last tick. Publishes
    /// `SessionEvent::Exited` for each session that transitioned in
    /// this call.
    pub fn reap_exited(&mut self) {
        // Collect transitions inside the mutable loop, then publish
        // afterwards. This sidesteps the borrow conflict between
        // `&mut self.sessions` and `&self.bus`.
        let mut transitioned = Vec::new();
        for session in &mut self.sessions {
            if !matches!(session.status, SessionStatus::Exited(_))
                && let Ok(Some(status)) = session.child.try_wait()
            {
                let code = status.exit_code() as i32;
                session.status = SessionStatus::Exited(code);
                transitioned.push((session.id, code));
            }
        }
        for (session_id, code) in transitioned {
            self.bus.publish(SessionEvent::Exited { session_id, code });
        }
    }

    /// Test-only helper that appends an already-built `Session` directly,
    /// bypassing real PTY creation. Bumps `next_id` past the pushed id so
    /// the monotonic-id invariant survives subsequent test spawns.
    /// Publishes `Spawned` on the bus for parity with the production
    /// `spawn` path.
    #[cfg(test)]
    pub(crate) fn push_for_test(&mut self, session: Session) -> usize {
        let id = session.id;
        let label = session.label.clone();
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        if id >= self.next_id {
            self.next_id = id + 1;
        }
        self.bus.publish(SessionEvent::Spawned {
            session_id: id,
            label,
        });
        self.assert_invariant();
        id
    }

    /// Test-only helper: append a stub `Session` that does not fork a real
    /// PTY. Used by the property test to exercise `SessionManager`
    /// invariants without the cost of spawning real processes. Publishes
    /// `Spawned` via `push_for_test`.
    #[cfg(test)]
    pub(crate) fn spawn_dummy(&mut self) -> usize {
        let id = self.next_id;
        let session = test_support::make_dummy_session(id);
        self.push_for_test(session)
    }

    #[inline]
    fn assert_invariant(&self) {
        debug_assert!(
            self.sessions.is_empty() || self.selected < self.sessions.len(),
            "SessionManager invariant violated: selected={} len={}",
            self.selected,
            self.sessions.len()
        );
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `SessionManager` invariants (REFACTOR_PLAN.md §3.1).
    //!
    //! These tests use `Session::dummy_exited` + `SessionManager::push_for_test`
    //! so nothing here forks a real process or opens a PTY — the whole file
    //! runs offline in CI.

    use super::*;

    /// Allocate a fresh dummy session through the manager, threading the
    /// manager's monotonic id counter so ids match what `spawn` would have
    /// produced.
    fn push(manager: &mut SessionManager, label: &str) -> usize {
        let id = manager.peek_next_id();
        let session = Session::dummy_exited(id, label);
        manager.push_for_test(session)
    }

    #[test]
    fn new_manager_is_empty() {
        let m = SessionManager::new();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert!(m.selected().is_none());
        assert!(m.selected_index().is_none());
    }

    #[test]
    fn spawn_assigns_monotonic_ids_and_keeps_selected_valid() {
        let mut m = SessionManager::new();
        let id_a = push(&mut m, "a");
        let id_b = push(&mut m, "b");
        let id_c = push(&mut m, "c");

        assert!(
            id_a < id_b && id_b < id_c,
            "ids must be strictly increasing"
        );
        assert_eq!(m.len(), 3);

        // Selected should always land on the most recently pushed session.
        assert_eq!(m.selected_index(), Some(2));
        assert_eq!(m.selected().map(|s| s.id), Some(id_c));

        // Killing and re-pushing must not reuse a freed id.
        m.kill(id_b);
        let id_d = push(&mut m, "d");
        assert!(id_d > id_c, "freed ids must not be reused");
    }

    #[test]
    fn get_mut_unknown_id_returns_none() {
        let mut m = SessionManager::new();
        let _ = push(&mut m, "a");
        assert!(m.get_mut(9999).is_none());
        assert!(m.get(9999).is_none());
    }

    #[test]
    fn selected_mut_clamps_after_killing_selected_tail() {
        let mut m = SessionManager::new();
        let id_a = push(&mut m, "a");
        let _id_b = push(&mut m, "b");
        let id_c = push(&mut m, "c");

        // selected points at tail (c) after the last push; kill it and the
        // selection should clamp to the new last session (b).
        assert_eq!(m.selected().map(|s| s.id), Some(id_c));
        m.kill(id_c);
        assert_eq!(m.len(), 2);
        let sel = m.selected_mut().expect("selected should still be Some");
        assert_eq!(
            sel.id,
            id_a + 1,
            "selection should clamp to the new last index"
        );
    }

    #[test]
    fn selected_mut_returns_none_when_last_session_removed() {
        let mut m = SessionManager::new();
        let id = push(&mut m, "only");
        m.kill(id);
        assert!(m.is_empty());
        assert!(m.selected_mut().is_none());
        assert!(m.selected_index().is_none());
    }

    #[test]
    fn select_next_and_prev_wrap_but_stay_in_bounds() {
        let mut m = SessionManager::new();
        push(&mut m, "a");
        push(&mut m, "b");
        push(&mut m, "c");
        // After pushing, selected == 2 (the tail).
        assert_eq!(m.selected_index(), Some(2));

        // select_next wraps to 0.
        m.select_next();
        assert_eq!(m.selected_index(), Some(0));

        // select_prev from 0 wraps to the tail — still a valid index.
        m.select_prev();
        assert_eq!(m.selected_index(), Some(2));

        // Many nexts in a row never walk out of bounds.
        for _ in 0..50 {
            m.select_next();
            assert!(m.selected_index().unwrap() < m.len());
        }
        for _ in 0..50 {
            m.select_prev();
            assert!(m.selected_index().unwrap() < m.len());
        }

        // select_up_by / select_down_by saturate at the ends.
        m.set_selected(1);
        m.select_up_by(100);
        assert_eq!(m.selected_index(), Some(0));
        m.select_down_by(100);
        assert_eq!(m.selected_index(), Some(m.len() - 1));
    }

    #[test]
    fn kill_before_selected_decrements_selected() {
        let mut m = SessionManager::new();
        let id_a = push(&mut m, "a");
        let _id_b = push(&mut m, "b");
        let id_c = push(&mut m, "c");

        // Select the tail explicitly.
        m.set_selected(2);
        assert_eq!(m.selected().map(|s| s.id), Some(id_c));

        // Killing the head (index 0, which is < selected) must decrement
        // selected so it still points at c.
        m.kill(id_a);
        assert_eq!(m.len(), 2);
        assert_eq!(m.selected_index(), Some(1));
        assert_eq!(m.selected().map(|s| s.id), Some(id_c));
    }

    #[test]
    fn kill_after_selected_leaves_selected_unchanged() {
        let mut m = SessionManager::new();
        let _id_a = push(&mut m, "a");
        let id_b = push(&mut m, "b");
        let id_c = push(&mut m, "c");

        m.set_selected(1);
        assert_eq!(m.selected().map(|s| s.id), Some(id_b));

        // Killing c (index 2, > selected) must not move selected.
        m.kill(id_c);
        assert_eq!(m.len(), 2);
        assert_eq!(m.selected_index(), Some(1));
        assert_eq!(m.selected().map(|s| s.id), Some(id_b));
    }

    #[test]
    fn kill_last_remaining_leaves_manager_empty() {
        let mut m = SessionManager::new();
        let id = push(&mut m, "only");
        assert!(m.kill(id));
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.selected_mut().is_none());
        assert!(m.selected().is_none());
        assert!(m.selected_index().is_none());

        // kill() of an unknown id on an empty manager returns false without
        // panicking.
        assert!(!m.kill(id));
    }

    // ---------------- Bus publishing (Phase 1 Task 4) ----------------
    //
    // These tests exercise the SessionManager → EventBus contract. Each
    // test constructs a fresh `EventBus`, passes it into
    // `SessionManager::with_bus`, subscribes *before* the action under
    // test, and asserts on the events that arrive.

    use crate::session::events::{EventBus, SessionEvent};

    fn manager_with_bus() -> (SessionManager, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new());
        let manager = SessionManager::with_bus(Arc::clone(&bus));
        (manager, bus)
    }

    /// Push a dummy session into the manager that starts in
    /// `SessionStatus::Running` so status-transition tests have a
    /// starting point that can change.
    fn push_running(m: &mut SessionManager, label: &str) -> usize {
        let id = m.peek_next_id();
        let mut session = Session::dummy_exited(id, label);
        session.status = SessionStatus::Running;
        m.push_for_test(session)
    }

    #[test]
    fn spawn_dummy_publishes_spawned_event() {
        let (mut m, bus) = manager_with_bus();
        let rx = bus.subscribe();
        let id = m.spawn_dummy();
        match rx.try_recv().expect("Spawned should have been published") {
            SessionEvent::Spawned { session_id, label } => {
                assert_eq!(session_id, id);
                assert_eq!(label, format!("dummy-{id}"));
            }
            other => panic!("expected Spawned, got {other:?}"),
        }
    }

    #[test]
    fn push_for_test_publishes_spawned_event() {
        // Test parity: the test-only push helper publishes Spawned too,
        // so tests that use `push_for_test` directly can still exercise
        // downstream consumers that react to Spawned.
        let (mut m, bus) = manager_with_bus();
        let rx = bus.subscribe();
        let id = m.peek_next_id();
        m.push_for_test(Session::dummy_exited(id, "abc"));
        match rx.try_recv().unwrap() {
            SessionEvent::Spawned { session_id, label } => {
                assert_eq!(session_id, id);
                assert_eq!(label, "abc");
            }
            other => panic!("expected Spawned, got {other:?}"),
        }
    }

    #[test]
    fn kill_publishes_exited_event() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let rx = bus.subscribe(); // subscribe AFTER push so we don't see Spawned
        assert!(m.kill(id));
        // `Session::kill` sets status to `Exited(-1)`, so the published
        // exit code is -1.
        match rx.try_recv().expect("Exited should have been published") {
            SessionEvent::Exited { session_id, code } => {
                assert_eq!(session_id, id);
                assert_eq!(code, -1);
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[test]
    fn kill_unknown_id_publishes_nothing() {
        let (mut m, bus) = manager_with_bus();
        let rx = bus.subscribe();
        assert!(!m.kill(9999));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn reap_exited_publishes_exited_on_transition() {
        let (mut m, bus) = manager_with_bus();
        // A session whose child reports Ok(Some(7)) from try_wait.
        let id = m.peek_next_id();
        m.push_for_test(test_support::make_exiting_session(id, 7));
        let rx = bus.subscribe();

        m.reap_exited();

        match rx.try_recv().expect("reap_exited should have published") {
            SessionEvent::Exited { session_id, code } => {
                assert_eq!(session_id, id);
                assert_eq!(code, 7);
            }
            other => panic!("expected Exited, got {other:?}"),
        }
    }

    #[test]
    fn reap_exited_does_not_refire_for_already_exited_sessions() {
        let (mut m, bus) = manager_with_bus();
        // `Session::dummy_exited` starts in Exited(0) — reap_exited must
        // leave it alone and publish nothing.
        let id = m.peek_next_id();
        m.push_for_test(Session::dummy_exited(id, "z"));
        let rx = bus.subscribe();

        m.reap_exited();

        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn reap_exited_is_idempotent_after_first_transition() {
        let (mut m, bus) = manager_with_bus();
        let id = m.peek_next_id();
        m.push_for_test(test_support::make_exiting_session(id, 0));
        let rx = bus.subscribe();

        m.reap_exited(); // should publish
        m.reap_exited(); // session is now Exited — should NOT publish again

        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::Exited { .. }
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn status_diff_fires_status_changed_when_status_changes() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        // Snapshot current status BEFORE we mutate.
        let before = vec![(id, SessionStatus::Running)];
        // Mutate session status directly (simulating what
        // `Session::check_attention` would do).
        m.get_mut(id).unwrap().status = SessionStatus::Idle;
        let rx = bus.subscribe();

        m.publish_status_diffs(&before);

        match rx.try_recv().expect("StatusChanged should have fired") {
            SessionEvent::StatusChanged { session_id, status } => {
                assert_eq!(session_id, id);
                assert_eq!(status, SessionStatus::Idle);
            }
            other => panic!("expected StatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn status_diff_fires_prompt_pending_on_transition_to_waiting() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let before = vec![(id, SessionStatus::Running)];
        m.get_mut(id).unwrap().status = SessionStatus::WaitingForApproval("AllowOnce".into());
        let rx = bus.subscribe();

        m.publish_status_diffs(&before);

        // Expect both StatusChanged and PromptPending (order: StatusChanged
        // first, then PromptPending, per the implementation).
        let first = rx.try_recv().unwrap();
        let second = rx.try_recv().unwrap();
        assert!(matches!(
            first,
            SessionEvent::StatusChanged {
                session_id,
                status: SessionStatus::WaitingForApproval(_),
            } if session_id == id
        ));
        match second {
            SessionEvent::PromptPending { session_id, kind } => {
                assert_eq!(session_id, id);
                assert_eq!(kind, "AllowOnce");
            }
            other => panic!("expected PromptPending, got {other:?}"),
        }
    }

    #[test]
    fn status_diff_does_not_refire_prompt_pending_within_waiting() {
        // Transitioning from WaitingForApproval(A) -> WaitingForApproval(B)
        // should NOT re-fire PromptPending — the session is already
        // waiting, and the kind change alone doesn't warrant a new
        // attention signal. It should still fire StatusChanged because
        // the status value did change.
        let (mut m, bus) = manager_with_bus();
        let id = m.peek_next_id();
        let mut session = Session::dummy_exited(id, "a");
        session.status = SessionStatus::WaitingForApproval("AllowOnce".into());
        m.push_for_test(session);
        let before = vec![(id, SessionStatus::WaitingForApproval("AllowOnce".into()))];
        m.get_mut(id).unwrap().status = SessionStatus::WaitingForApproval("YesNo".into());
        let rx = bus.subscribe();

        m.publish_status_diffs(&before);

        // Exactly one StatusChanged, no PromptPending.
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::StatusChanged { .. }
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn status_diff_publishes_nothing_when_unchanged() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let before = vec![(id, SessionStatus::Running)];
        // Do NOT mutate the session.
        let rx = bus.subscribe();

        m.publish_status_diffs(&before);

        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn bus_accessor_returns_a_shared_handle() {
        // `manager.bus()` gives out an Arc that's observably the same
        // bus: subscribing via the accessor sees events the manager
        // publishes.
        let m = SessionManager::new();
        let rx = m.bus().subscribe();
        m.bus().publish(SessionEvent::Exited {
            session_id: 42,
            code: 0,
        });
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 42, .. }
        ));
    }

    #[test]
    fn submit_sequence_is_carriage_return() {
        // Pin the submit byte sequence so an unintended change to
        // `SUBMIT_SEQUENCE` (e.g. switching to `\n` or a multi-byte
        // chord) is caught immediately. The value must match what the
        // production keyboard handler writes for `KeyCode::Enter` —
        // see `crate::app::key_event_to_bytes` and the existing
        // `App::approve_selected` call site (`b"\r"`).
        assert_eq!(super::SUBMIT_SEQUENCE, b"\r");
    }

    #[test]
    fn check_attention_publishes_via_real_detector() {
        // PR #7 review item D2: closes the one wiring gap that
        // `publish_status_diffs` direct tests can't see — verifies
        // that `check_attention` actually flows from a real
        // `PromptDetector` match through `Session::check_attention`
        // and out to the bus as `StatusChanged` + `PromptPending`.
        //
        // Uses a dummy session (no real PTY) but a real
        // `vt100::Parser`, into which we inject bytes that match the
        // detector's `YesNo` pattern. No fork, no /bin/sh, runs
        // offline.
        use crate::pty::detector::PromptDetector;
        use crate::session::lock_parser;

        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "real-detector");

        // Inject a Y/n prompt into the session's parser. This is the
        // same shape `Session::check_attention` reads via its own
        // `lock_parser(&self.parser).screen()` call.
        //
        // `PromptDetector::check` only scans the last 15 rows of the
        // screen, so we use the vt100 cursor-position escape
        // (`ESC[20;1H`) to land the bytes on row 20 — well within the
        // detector's scan window for the dummy session's 24x80 screen.
        {
            let session = m.get(id).expect("session must exist");
            let mut parser = lock_parser(&session.parser);
            parser.process(b"\x1b[20;1HDo you want to proceed? Y/n");
        }

        // Subscribe AFTER mutating the parser so the only events on
        // the channel are the ones produced by the detector run.
        let rx = bus.subscribe();
        let detector = PromptDetector::new();
        m.check_attention(&detector);

        // Drain everything the call published. Order is
        // implementation-defined: `publish_status_diffs` fires
        // `StatusChanged` first then `PromptPending`, but the test
        // asserts on presence rather than ordering so future
        // refactors don't bind us to a specific sequence.
        let events: Vec<SessionEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

        let status_changed_to_waiting = events.iter().any(|ev| {
            matches!(
                ev,
                SessionEvent::StatusChanged {
                    session_id,
                    status: SessionStatus::WaitingForApproval(_),
                } if *session_id == id
            )
        });
        assert!(
            status_changed_to_waiting,
            "expected StatusChanged → WaitingForApproval, saw {events:?}"
        );

        let prompt_pending = events.iter().any(|ev| {
            matches!(
                ev,
                SessionEvent::PromptPending { session_id, .. }
                    if *session_id == id
            )
        });
        assert!(
            prompt_pending,
            "expected PromptPending fired by detector hit, saw {events:?}"
        );
    }

    // ---------------- send_prompt (Phase 2 Task 2) ----------------

    #[test]
    fn send_prompt_returns_first_turn_id_as_zero() {
        let (mut m, _bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let turn = m.send_prompt(id, "hi").expect("send_prompt should succeed");
        assert_eq!(turn, TurnId::new(0));
    }

    #[test]
    fn send_prompt_returns_monotonic_turn_ids() {
        let (mut m, _bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let first = m.send_prompt(id, "one").expect("first send_prompt");
        let second = m.send_prompt(id, "two").expect("second send_prompt");
        assert_eq!(first, TurnId::new(0));
        assert_eq!(second, TurnId::new(1));
    }

    #[test]
    fn send_prompt_publishes_prompt_submitted_with_matching_turn_id() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let rx = bus.subscribe();
        let turn = m.send_prompt(id, "hi").expect("send_prompt should succeed");

        let events: Vec<SessionEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        let submitted: Vec<_> = events
            .iter()
            .filter_map(|ev| match ev {
                SessionEvent::PromptSubmitted {
                    session_id,
                    turn_id,
                } => Some((*session_id, *turn_id)),
                _ => None,
            })
            .collect();

        assert_eq!(
            submitted,
            vec![(id, turn)],
            "expected exactly one PromptSubmitted matching the returned turn, saw {events:?}"
        );
    }

    #[test]
    fn send_prompt_writes_text_then_submit_sequence() {
        let (mut m, _bus) = manager_with_bus();
        let next_id = m.peek_next_id();
        let (session, recording) = super::test_support::make_recording_session(next_id);
        let id = m.push_for_test(session);
        m.send_prompt(id, "hello")
            .expect("send_prompt should succeed");
        assert_eq!(recording.captured(), b"hello\r");
    }

    #[test]
    fn send_prompt_writes_unicode_text_correctly() {
        let (mut m, _bus) = manager_with_bus();
        let next_id = m.peek_next_id();
        let (session, recording) = super::test_support::make_recording_session(next_id);
        let id = m.push_for_test(session);
        m.send_prompt(id, "héllo")
            .expect("send_prompt should succeed");
        assert_eq!(recording.captured(), "héllo\r".as_bytes());
    }

    #[test]
    fn send_prompt_unknown_id_returns_err() {
        let (mut m, _bus) = manager_with_bus();
        let err = m
            .send_prompt(999, "x")
            .expect_err("send_prompt on unknown id must fail");
        assert!(
            err.to_string().contains("999"),
            "error message should mention the missing id, got {err}"
        );
    }

    #[test]
    fn send_prompt_unknown_id_publishes_nothing() {
        let (mut m, bus) = manager_with_bus();
        let rx = bus.subscribe();
        let _ = m.send_prompt(999, "x");
        let events: Vec<SessionEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events.is_empty(),
            "failed send_prompt must publish nothing, saw {events:?}"
        );
    }

    #[test]
    fn send_prompt_unknown_id_does_not_allocate_turn_id() {
        let (mut m, _bus) = manager_with_bus();
        let real_id = push_running(&mut m, "a");
        let _ = m.send_prompt(999, "x");
        let turn = m
            .send_prompt(real_id, "y")
            .expect("send_prompt on real id should succeed");
        assert_eq!(
            turn,
            TurnId::new(0),
            "a failed send_prompt must not bump any counter"
        );
    }

    // ---------------- broadcast (Phase 2 Task 3) ----------------
    //
    // These tests exercise the `SessionManager::broadcast` contract:
    // raw multi-session writes that do NOT allocate a TurnId and do
    // NOT publish any `SessionEvent`. See docs/designs/session-management.md §3.
    //
    // `TurnId` is already in scope via the `use super::*;` at the top
    // of `mod tests` (file-level `use super::events::{..., TurnId}`),
    // so no extra import is needed here.

    #[test]
    fn broadcast_to_empty_ids_returns_empty_result() {
        let mut m = SessionManager::new();
        let result = m.broadcast(&[], b"hi");
        assert!(result.sent.is_empty());
        assert!(result.not_found.is_empty());
    }

    #[test]
    fn broadcast_to_single_session_records_in_sent() {
        let (mut m, _bus) = manager_with_bus();
        let id = push_running(&mut m, "a");
        let result = m.broadcast(&[id], b"x");
        assert_eq!(result.sent, vec![id]);
        assert!(result.not_found.is_empty());
    }

    #[test]
    fn broadcast_to_multiple_sessions_preserves_input_order() {
        let (mut m, _bus) = manager_with_bus();
        let id_a = push_running(&mut m, "a");
        let id_b = push_running(&mut m, "b");
        let id_c = push_running(&mut m, "c");

        // Deliberately not in storage order: c, a, b.
        let result = m.broadcast(&[id_c, id_a, id_b], b"x");
        assert_eq!(result.sent, vec![id_c, id_a, id_b]);
        assert!(result.not_found.is_empty());
    }

    #[test]
    fn broadcast_to_unknown_id_records_in_not_found() {
        let (mut m, _bus) = manager_with_bus();
        let real_id = push_running(&mut m, "a");
        let result = m.broadcast(&[real_id, 9999], b"x");
        assert_eq!(result.sent, vec![real_id]);
        assert_eq!(result.not_found, vec![9999]);
    }

    #[test]
    fn broadcast_writes_bytes_to_each_target() {
        let (mut m, _bus) = manager_with_bus();
        let id_a = m.peek_next_id();
        let (sess_a, rec_a) = super::test_support::make_recording_session(id_a);
        m.push_for_test(sess_a);
        let id_b = m.peek_next_id();
        let (sess_b, rec_b) = super::test_support::make_recording_session(id_b);
        m.push_for_test(sess_b);

        let result = m.broadcast(&[id_a, id_b], b"hi");
        assert_eq!(result.sent, vec![id_a, id_b]);
        assert!(result.not_found.is_empty());

        // No submit sequence or other framing — broadcast sends raw bytes.
        assert_eq!(rec_a.captured(), b"hi");
        assert_eq!(rec_b.captured(), b"hi");
    }

    #[test]
    fn broadcast_does_not_publish_any_session_event() {
        let (mut m, bus) = manager_with_bus();
        let id = push_running(&mut m, "a");

        // Subscribe AFTER push so the Spawned event isn't on this rx.
        let rx = bus.subscribe();

        let _ = m.broadcast(&[id], b"payload");

        // Drain — must be empty.
        let events: Vec<SessionEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(
            events.is_empty(),
            "broadcast must not publish any SessionEvent, saw {events:?}"
        );
    }

    #[test]
    fn broadcast_does_not_allocate_turn_id() {
        let (mut m, _bus) = manager_with_bus();
        let id = push_running(&mut m, "a");

        let _ = m.broadcast(&[id], b"x");

        // If broadcast had allocated a TurnId, the per-session counter
        // would now be at 1 and this next allocation would return
        // TurnId::new(1). Assert it's still at 0.
        let session = m.get_mut(id).expect("session must exist");
        assert_eq!(session.allocate_turn_id(), TurnId::new(0));
    }

    #[test]
    fn broadcast_with_duplicate_ids_writes_each_time() {
        let (mut m, _bus) = manager_with_bus();
        let id = m.peek_next_id();
        let (sess, rec) = super::test_support::make_recording_session(id);
        m.push_for_test(sess);

        let result = m.broadcast(&[id, id, id], b"x");
        assert_eq!(result.sent, vec![id, id, id]);
        assert!(result.not_found.is_empty());
        assert_eq!(rec.captured(), b"xxx");
    }
}

#[cfg(test)]
mod test_support {
    //! Minimal stub PTY/Child impls so tests can construct a `Session`
    //! without forking a real child process. Every trait method that could
    //! actually talk to a live PTY panics — the property test only
    //! exercises `SessionManager`'s collection logic (spawn / kill /
    //! select), never `Session::write`, `Session::resize`, or child I/O,
    //! so these stubs should never be touched at runtime.
    use std::io::{Result as IoResult, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};

    use super::super::types::{Session, SessionStatus};

    #[derive(Debug)]
    pub(super) struct DummyPty;

    impl MasterPty for DummyPty {
        fn resize(&self, _size: PtySize) -> Result<(), anyhow::Error> {
            Ok(())
        }
        fn get_size(&self) -> Result<PtySize, anyhow::Error> {
            Ok(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
        }
        fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
            unreachable!("DummyPty::try_clone_reader should not be called in tests")
        }
        fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
            unreachable!("DummyPty::take_writer should not be called in tests")
        }
        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<i32> {
            None
        }
        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
            None
        }
        #[cfg(unix)]
        fn tty_name(&self) -> Option<PathBuf> {
            None
        }
    }

    pub(super) struct DummyWriter;

    impl Write for DummyWriter {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }

    /// `Write` impl that captures every byte written to it. Used by
    /// `send_prompt` / `broadcast` tests to assert exactly what bytes
    /// `Session::try_write` produced. The buffer is shared via
    /// `Arc<Mutex<>>` so the test can hold one handle and the
    /// `Session` (via its `Box<dyn Write + Send>`) holds another.
    #[derive(Debug, Clone)]
    pub(super) struct RecordingWriter(pub(super) Arc<Mutex<Vec<u8>>>);

    impl RecordingWriter {
        pub(super) fn new() -> Self {
            Self(Arc::new(Mutex::new(Vec::new())))
        }

        /// Snapshot the captured bytes. Cloning the inner Vec is fine
        /// for tests; production code never sees this type.
        pub(super) fn captured(&self) -> Vec<u8> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }

    /// Build a dummy `Session` whose `writer` is a `RecordingWriter`,
    /// returning both the session and a handle on the recording so the
    /// test can assert on captured bytes after the session is moved
    /// into the manager via `push_for_test`.
    pub(super) fn make_recording_session(id: usize) -> (Session, RecordingWriter) {
        let writer = RecordingWriter::new();
        let mut session = make_dummy_session(id);
        session.writer = Box::new(writer.clone());
        (session, writer)
    }

    #[derive(Debug)]
    pub(super) struct DummyChild;

    impl ChildKiller for DummyChild {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(DummyChild)
        }
    }

    impl Child for DummyChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    pub(super) fn make_dummy_session(id: usize) -> Session {
        Session {
            id,
            label: format!("dummy-{id}"),
            claude_session_id: None,
            working_dir: PathBuf::from("/"),
            status: SessionStatus::Running,
            master: Box::new(DummyPty),
            writer: Box::new(DummyWriter),
            child: Box::new(DummyChild),
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
            last_activity: Instant::now(),
            needs_attention: false,
            pty_size: PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            context_percent: None,
            consecutive_write_failures: 0,
            next_turn_id: 0,
        }
    }

    /// Child stub whose `try_wait` reports a completed exit with the
    /// given code on every call. Used by `reap_exited` tests so the
    /// session's status transitions from `Running` to `Exited(code)`.
    #[derive(Debug)]
    pub(super) struct ExitedChild {
        code: i32,
    }

    impl ExitedChild {
        /// Construct an `ExitedChild` with the given exit code.
        ///
        /// `code` must be non-negative because `try_wait` round-trips
        /// it through `ExitStatus::with_exit_code(u32)`. PR #7
        /// post-review pass added this `debug_assert` so a test
        /// passing a negative code (which would silently wrap to a
        /// large positive number) trips in debug builds instead of
        /// producing surprising assertion failures downstream.
        pub(super) fn new(code: i32) -> Self {
            debug_assert!(
                code >= 0,
                "ExitedChild only supports non-negative exit codes; got {code}"
            );
            Self { code }
        }
    }

    impl ChildKiller for ExitedChild {
        fn kill(&mut self) -> IoResult<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(ExitedChild::new(self.code))
        }
    }

    impl Child for ExitedChild {
        fn try_wait(&mut self) -> IoResult<Option<ExitStatus>> {
            // portable_pty's `with_exit_code` takes a u32, so negative
            // codes round-trip via `as u32` / `as i32` — we only need
            // non-negative codes in tests.
            Ok(Some(ExitStatus::with_exit_code(self.code as u32)))
        }
        fn wait(&mut self) -> IoResult<ExitStatus> {
            Ok(ExitStatus::with_exit_code(self.code as u32))
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    /// Build a dummy `Session` whose `child.try_wait()` immediately
    /// reports the given exit code. The session itself still starts in
    /// `SessionStatus::Running` so `reap_exited` will observe the
    /// `Running -> Exited(code)` transition and fire the bus event.
    pub(super) fn make_exiting_session(id: usize, code: i32) -> Session {
        let mut session = make_dummy_session(id);
        session.child = Box::new(ExitedChild::new(code));
        session
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Spawn,
        /// Kill a session by vec index (generator space: 0..16, modulo len
        /// at application time).
        Kill(usize),
        SelectNext,
        SelectPrev,
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Spawn),
            (0usize..16).prop_map(Op::Kill),
            Just(Op::SelectNext),
            Just(Op::SelectPrev),
        ]
    }

    proptest! {
        #[test]
        fn manager_invariant_holds(
            ops in prop::collection::vec(op_strategy(), 0..100),
        ) {
            let mut mgr = SessionManager::new();
            let mut seen_ids: Vec<usize> = Vec::new();

            for op in ops {
                match op {
                    Op::Spawn => {
                        let id = mgr.spawn_dummy();
                        // Ids must be globally unique for the lifetime of
                        // the manager — killing must never free an id.
                        prop_assert!(
                            !seen_ids.contains(&id),
                            "id {id} was reused"
                        );
                        seen_ids.push(id);
                    }
                    Op::Kill(idx) => {
                        if !mgr.is_empty() {
                            let real_idx = idx % mgr.len();
                            let target_id =
                                mgr.iter().nth(real_idx).unwrap().id;
                            prop_assert!(mgr.kill(target_id));
                        }
                    }
                    Op::SelectNext => mgr.select_next(),
                    Op::SelectPrev => mgr.select_prev(),
                }

                // Core invariant: empty OR selected < len.
                let ok = mgr.is_empty()
                    || mgr
                        .selected_index()
                        .map(|i| i < mgr.len())
                        .unwrap_or(false);
                prop_assert!(
                    ok,
                    "invariant broken: len={}, selected={:?}",
                    mgr.len(),
                    mgr.selected_index()
                );
            }
        }
    }
}
