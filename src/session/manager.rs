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
use std::sync::mpsc;

use crate::event::Event;
use crate::pty::detector::PromptDetector;

use super::types::{Session, SessionStatus};

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

pub struct SessionManager {
    sessions: Vec<Session>,
    selected: usize,
    next_id: usize,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            selected: 0,
            next_id: 1,
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
        self.sessions.remove(idx);

        if self.sessions.is_empty() {
            self.selected = 0;
        } else if idx < self.selected {
            self.selected -= 1;
        } else if idx == self.selected && self.selected >= self.sessions.len() {
            self.selected = self.sessions.len() - 1;
        }

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

    /// Run the attention detector against every live session.
    pub fn check_attention(&mut self, detector: &PromptDetector) {
        for session in &mut self.sessions {
            if !matches!(session.status, SessionStatus::Exited(_)) {
                session.check_attention(detector);
            }
        }
    }

    /// Poll all live sessions for child exit status and mark any that have
    /// exited on their own since the last tick.
    pub fn reap_exited(&mut self) {
        for session in &mut self.sessions {
            if !matches!(session.status, SessionStatus::Exited(_))
                && let Ok(Some(status)) = session.child.try_wait()
            {
                session.status = SessionStatus::Exited(status.exit_code() as i32);
            }
        }
    }

    /// Test-only helper that appends an already-built `Session` directly,
    /// bypassing real PTY creation. Bumps `next_id` past the pushed id so
    /// the monotonic-id invariant survives subsequent test spawns.
    #[cfg(test)]
    pub(crate) fn push_for_test(&mut self, session: Session) -> usize {
        let id = session.id;
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
        if id >= self.next_id {
            self.next_id = id + 1;
        }
        self.assert_invariant();
        id
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
}
