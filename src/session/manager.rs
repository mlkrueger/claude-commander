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

    /// Test-only helper: append a stub `Session` that does not fork a real
    /// PTY. Used by unit/property tests to exercise `SessionManager`
    /// invariants without the cost of spawning real processes.
    #[cfg(test)]
    pub(crate) fn spawn_dummy(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        let session = test_support::make_dummy_session(id);
        self.sessions.push(session);
        self.selected = self.sessions.len() - 1;
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
        }
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
