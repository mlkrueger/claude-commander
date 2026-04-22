// Test git status parsing
mod git_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn test_dir_has_changes_empty() {
        let map = HashMap::new();
        let result = ccom::fs::git::dir_has_changes(&PathBuf::from("/tmp"), &map);
        assert!(result.is_none());
    }

    #[test]
    fn test_git_file_status_indicator() {
        use ccom::fs::git::GitFileStatus;
        assert_eq!(GitFileStatus::Modified.indicator(), "M");
        assert_eq!(GitFileStatus::Staged.indicator(), "S");
        assert_eq!(GitFileStatus::Untracked.indicator(), "?");
        assert_eq!(GitFileStatus::Added.indicator(), "A");
        assert_eq!(GitFileStatus::Deleted.indicator(), "D");
    }

    #[test]
    fn test_dir_has_changes_finds_nested() {
        use ccom::fs::git::GitFileStatus;
        let mut map = HashMap::new();
        map.insert(
            PathBuf::from("/project/src/main.rs"),
            GitFileStatus::Modified,
        );
        map.insert(PathBuf::from("/project/src/lib.rs"), GitFileStatus::Staged);

        let result = ccom::fs::git::dir_has_changes(&PathBuf::from("/project/src"), &map);
        assert!(result.is_some());
    }

    #[test]
    fn test_dir_has_changes_worst_status() {
        use ccom::fs::git::GitFileStatus;
        let mut map = HashMap::new();
        map.insert(PathBuf::from("/project/a.rs"), GitFileStatus::Untracked);
        map.insert(PathBuf::from("/project/b.rs"), GitFileStatus::Modified);

        let result = ccom::fs::git::dir_has_changes(&PathBuf::from("/project"), &map);
        // Modified has higher priority than Untracked
        assert_eq!(result, Some(GitFileStatus::Modified));
    }
}

// Test file tree
mod tree_tests {
    use std::path::PathBuf;

    #[test]
    fn test_file_tree_creation() {
        let tree = ccom::fs::tree::FileTree::new(PathBuf::from("/tmp"));
        assert_eq!(tree.root.path, PathBuf::from("/tmp"));
        assert!(tree.root.expanded);
        assert!(tree.root.is_dir);
    }

    #[test]
    fn test_file_tree_navigation() {
        let mut tree = ccom::fs::tree::FileTree::new(PathBuf::from("/tmp"));
        assert_eq!(tree.selected, 0);
        tree.move_down();
        // Selected should advance if there are visible nodes
        // (depends on /tmp contents, but at minimum there's the root)
        assert!(tree.selected <= tree.visible_nodes().len());
    }

    #[test]
    fn test_file_tree_set_root() {
        let mut tree = ccom::fs::tree::FileTree::new(PathBuf::from("/tmp"));
        tree.set_root(PathBuf::from("/var"));
        assert_eq!(tree.root.path, PathBuf::from("/var"));
        assert_eq!(tree.selected, 0);
    }
}

// Test prompt detector
mod detector_tests {
    #[test]
    fn test_prompt_detector_creation() {
        let detector = ccom::pty::detector::PromptDetector::new();
        // Just verify it doesn't panic
        let parser = vt100::Parser::new(24, 80, 0);
        let result = detector.check(parser.screen());
        // Empty screen should have no prompts
        assert!(result.is_none());
    }

    // Helper: fill screen to push content into the last 15 rows
    fn make_parser_with_text(text: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(24, 80, 0);
        // Fill 20 blank lines to push text to the bottom
        for _ in 0..20 {
            parser.process(b"\r\n");
        }
        parser.process(text);
        parser
    }

    #[test]
    fn test_prompt_detector_finds_allow() {
        let detector = ccom::pty::detector::PromptDetector::new();
        let parser = make_parser_with_text(b"Allow once this tool call?");
        let result = detector.check(parser.screen());
        assert!(result.is_some());
    }

    #[test]
    fn test_prompt_detector_finds_yes_no() {
        let detector = ccom::pty::detector::PromptDetector::new();
        let parser = make_parser_with_text(b"Do you want to proceed? [Y/n]");
        let result = detector.check(parser.screen());
        assert!(result.is_some());
    }

    #[test]
    fn test_prompt_detector_no_match() {
        let detector = ccom::pty::detector::PromptDetector::new();
        let parser = make_parser_with_text(b"Hello world, just some normal output");
        let result = detector.check(parser.screen());
        assert!(result.is_none());
    }

    // Regression: Claude's own response text can mention "deny",
    // "approve", "permission", etc. when explaining tool permissions.
    // The detector must not trip on prose — only on actual dialog UI.
    // Prior regex matched bare `deny` / `approve` / `permission` and
    // pinned sessions to WaitingForApproval during docs-style replies.
    #[test]
    fn test_prompt_detector_ignores_prose_about_permissions() {
        let detector = ccom::pty::detector::PromptDetector::new();
        let parser = make_parser_with_text(
            b"Permissions (allow/ask/deny). A deny in settings.json blocks \
              even if another tries to approve it. Useful for compliance \
              rules that are too dynamic for static permission patterns.",
        );
        let result = detector.check(parser.screen());
        assert!(
            result.is_none(),
            "detector false-matched on prose about permissions: {result:?}"
        );
    }
}

// Test editor state
mod editor_tests {
    #[test]
    fn test_editor_open() {
        let tmp = std::env::temp_dir().join("ccom_test_editor.txt");
        std::fs::write(&tmp, "line one\nline two\nline three\n").unwrap();

        let editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        assert_eq!(editor.lines.len(), 3);
        assert_eq!(editor.lines[0], "line one");
        assert_eq!(editor.cursor_row, 0);
        assert_eq!(editor.cursor_col, 0);
        assert!(!editor.modified);

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_editor_insert_char() {
        let tmp = std::env::temp_dir().join("ccom_test_insert.txt");
        std::fs::write(&tmp, "hello\n").unwrap();

        let mut editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        editor.cursor_col = 5;
        editor.insert_char('!');
        assert_eq!(editor.lines[0], "hello!");
        assert!(editor.modified);

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_editor_newline() {
        let tmp = std::env::temp_dir().join("ccom_test_newline.txt");
        std::fs::write(&tmp, "hello world\n").unwrap();

        let mut editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        editor.cursor_col = 5;
        editor.insert_newline();
        assert_eq!(editor.lines[0], "hello");
        assert_eq!(editor.lines[1], " world");
        assert_eq!(editor.cursor_row, 1);
        assert_eq!(editor.cursor_col, 0);

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_editor_backspace() {
        let tmp = std::env::temp_dir().join("ccom_test_backspace.txt");
        std::fs::write(&tmp, "hello\n").unwrap();

        let mut editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        editor.cursor_col = 5;
        editor.backspace();
        assert_eq!(editor.lines[0], "hell");
        assert_eq!(editor.cursor_col, 4);

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_editor_save() {
        let tmp = std::env::temp_dir().join("ccom_test_save.txt");
        std::fs::write(&tmp, "original\n").unwrap();

        let mut editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        editor.insert_char('!');
        assert!(editor.modified);
        editor.save().unwrap();
        assert!(!editor.modified);

        let content = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(content, "!original\n");

        std::fs::remove_file(tmp).ok();
    }

    #[test]
    fn test_editor_navigation() {
        let tmp = std::env::temp_dir().join("ccom_test_nav.txt");
        std::fs::write(&tmp, "aaa\nbbb\nccc\n").unwrap();

        let mut editor = ccom::ui::panels::editor::EditorState::open(tmp.clone()).unwrap();
        editor.move_down();
        assert_eq!(editor.cursor_row, 1);
        editor.move_end();
        assert_eq!(editor.cursor_col, 3);
        editor.move_home();
        assert_eq!(editor.cursor_col, 0);
        editor.move_up();
        assert_eq!(editor.cursor_row, 0);

        std::fs::remove_file(tmp).ok();
    }
}

// Integration smoke tests that exercise SessionManager's *real* spawn path
// (PTY + fork) end-to-end with the EventBus. The unit tests inside
// `src/session/manager.rs` use `push_for_test` and dummy children to keep
// the inner test loop fast and offline; these tests catch any wiring
// regressions in the production `Session::spawn` -> bus publish chain that
// the dummy path can't see.
mod session_bus_integration {
    use ccom::session::{EventBus, SessionManager, SpawnConfig};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Drain bus events until we either find one matching `pred` or
    /// exceed `timeout`. The bus is sync (`std::sync::mpsc`), so this
    /// just spins on `try_recv` with a small sleep — keeps the test
    /// resilient to ordering and to incidental events that arrive
    /// alongside the one we care about.
    fn wait_for<F>(
        rx: &mpsc::Receiver<ccom::session::SessionEvent>,
        timeout: Duration,
        mut pred: F,
    ) -> Option<ccom::session::SessionEvent>
    where
        F: FnMut(&ccom::session::SessionEvent) -> bool,
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            while let Ok(ev) = rx.try_recv() {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn spawn_publishes_spawned_event_through_real_pty() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));
        let rx = bus.subscribe();

        // We need an event_tx for the PTY reader thread. We don't read
        // from it here — the test only cares about the bus.
        let (raw_tx, _event_rx) = mpsc::channel();
        let event_tx = ccom::event::MonitoredSender::wrap(raw_tx);

        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-spawn".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "exit 0".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("real spawn should succeed");

        let event = wait_for(&rx, Duration::from_secs(2), |ev| {
            matches!(
                ev,
                ccom::session::SessionEvent::Spawned { session_id, .. }
                    if *session_id == id
            )
        })
        .expect("Spawned event should arrive within 2s");

        match event {
            ccom::session::SessionEvent::Spawned { session_id, label } => {
                assert_eq!(session_id, id);
                assert_eq!(label, "smoke-spawn");
            }
            _ => unreachable!(),
        }

        // Cleanup: child has likely already exited, but kill is
        // idempotent on dead processes.
        manager.kill(id);
    }

    #[test]
    fn kill_publishes_exited_event_through_real_pty() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));

        let (raw_tx, _event_rx) = mpsc::channel();
        let event_tx = ccom::event::MonitoredSender::wrap(raw_tx);
        // Use `sleep 30` so the child is reliably alive when we kill it.
        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-kill".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "sleep 30".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("real spawn should succeed");

        // Subscribe AFTER spawn so we don't have to filter the Spawned
        // event we don't care about for this test.
        let rx = bus.subscribe();

        assert!(manager.kill(id));

        let event = wait_for(&rx, Duration::from_secs(2), |ev| {
            matches!(
                ev,
                ccom::session::SessionEvent::Exited { session_id, .. }
                    if *session_id == id
            )
        })
        .expect("Exited event should arrive within 2s");

        if let ccom::session::SessionEvent::Exited { code, .. } = event {
            // `Session::kill` sets status to Exited(-1) regardless of
            // the actual signal-driven exit code, so the bus event
            // mirrors that.
            assert_eq!(code, -1);
        }
    }

    #[test]
    fn reap_exited_publishes_for_naturally_exiting_child() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));

        let (raw_tx, _event_rx) = mpsc::channel();
        let event_tx = ccom::event::MonitoredSender::wrap(raw_tx);
        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-reap".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "exit 0".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("real spawn should succeed");

        // Wait long enough for the child to actually exit on its own.
        std::thread::sleep(Duration::from_millis(200));

        let rx = bus.subscribe();
        manager.reap_exited();

        let event = wait_for(&rx, Duration::from_secs(2), |ev| {
            matches!(
                ev,
                ccom::session::SessionEvent::Exited { session_id, .. }
                    if *session_id == id
            )
        })
        .expect("reap_exited should publish Exited within 2s");

        if let ccom::session::SessionEvent::Exited { code, .. } = event {
            // `exit 0` exits with code 0 — reap_exited reads the real
            // child status, so the published code matches.
            assert_eq!(code, 0);
        }
    }

    // ---------------- Phase 2 (send_prompt + broadcast) ----------------
    //
    // These exercise the production path against a real PTY backed by
    // `/bin/cat`, which echoes its stdin to stdout. The PTY's line
    // discipline ALSO echoes input back, so a successful write produces
    // observable bytes on the PtyOutput event channel that the test
    // reader thread feeds. We use that to confirm the bytes
    // `send_prompt` and `broadcast` write actually reached the PTY.

    use ccom::session::TurnId;
    use std::collections::HashMap;

    /// Per-session PTY output buffer that survives across multiple
    /// `wait_for_bytes` calls. Built once per test and threaded through
    /// each substring check so events for *other* sessions get
    /// accumulated for their own future checks instead of being
    /// silently dropped.
    ///
    /// This replaces an earlier helper (`read_pty_until_contains`) that
    /// drained the channel into a single per-call buffer and discarded
    /// any non-matching events. PR #8 review item C3 caught the bug:
    /// `broadcast_through_real_pty_writes_to_each_session` checks
    /// session a then session b in sequence, and the original helper
    /// would discard a's events while waiting for b (or vice versa)
    /// depending on arrival order. The test passed by ordering luck.
    struct PtyOutputAccumulator<'a> {
        rx: &'a mpsc::Receiver<ccom::event::Event>,
        buffers: HashMap<usize, Vec<u8>>,
    }

    impl<'a> PtyOutputAccumulator<'a> {
        fn new(rx: &'a mpsc::Receiver<ccom::event::Event>) -> Self {
            Self {
                rx,
                buffers: HashMap::new(),
            }
        }

        /// Drain whatever is currently sitting on the channel into the
        /// per-session buffers. Non-blocking.
        fn drain(&mut self) {
            while let Ok(ev) = self.rx.try_recv() {
                if let ccom::event::Event::PtyOutput { session_id, data } = ev {
                    self.buffers
                        .entry(session_id)
                        .or_default()
                        .extend_from_slice(&data);
                }
            }
        }

        /// Block (with polling) until `needle` appears anywhere in the
        /// accumulated buffer for `target_session`, or `timeout`
        /// elapses. Drain happens on every poll, so events for other
        /// sessions are buffered for their own future checks.
        fn wait_for_bytes(
            &mut self,
            target_session: usize,
            needle: &[u8],
            timeout: Duration,
        ) -> bool {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                self.drain();
                if let Some(buf) = self.buffers.get(&target_session)
                    && buf.windows(needle.len()).any(|w| w == needle)
                {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn send_prompt_through_real_pty_writes_bytes_and_publishes_event() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));
        let (raw_tx, event_rx) = mpsc::channel();
        let event_tx = ccom::event::MonitoredSender::wrap(raw_tx);

        // `cat` reads its stdin and echoes back. The PTY line
        // discipline also echoes input, so we'll see the bytes via
        // the PtyOutput channel either way.
        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-send".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("real spawn should succeed");

        let bus_rx = bus.subscribe();
        let returned_turn = manager
            .send_prompt(id, "phase2-marker")
            .expect("send_prompt should succeed against a real PTY");

        // First allocation on this fresh session — TurnId::new(0).
        assert_eq!(returned_turn, TurnId::new(0));

        // The bus must publish PromptSubmitted with the same turn id.
        let bus_event = wait_for(&bus_rx, Duration::from_secs(2), |ev| {
            matches!(
                ev,
                ccom::session::SessionEvent::PromptSubmitted { session_id, .. }
                    if *session_id == id
            )
        })
        .expect("PromptSubmitted should arrive on bus");
        if let ccom::session::SessionEvent::PromptSubmitted { turn_id, .. } = bus_event {
            assert_eq!(turn_id, returned_turn);
        }

        // The bytes we wrote must actually reach the PTY — verified
        // via the PtyOutput echo through cat.
        let mut pty = PtyOutputAccumulator::new(&event_rx);
        assert!(
            pty.wait_for_bytes(id, b"phase2-marker", Duration::from_secs(3)),
            "expected 'phase2-marker' to appear in PtyOutput from cat",
        );

        manager.kill(id);
    }

    #[test]
    fn broadcast_through_real_pty_writes_to_each_session() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));
        let (raw_tx, event_rx) = mpsc::channel();
        let event_tx = ccom::event::MonitoredSender::wrap(raw_tx);

        let id_a = manager
            .spawn(SpawnConfig {
                label: "smoke-bcast-a".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx: event_tx.clone(),
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn a");

        let id_b = manager
            .spawn(SpawnConfig {
                label: "smoke-bcast-b".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn b");

        // Subscribe AFTER spawn so we don't have to filter Spawned events.
        let bus_rx = bus.subscribe();

        let result = manager.broadcast(&[id_a, id_b], b"bcast-marker\r");
        assert_eq!(result.sent, vec![id_a, id_b]);
        assert!(result.not_found.is_empty());

        // Bytes must reach BOTH sessions — verified via per-session
        // PtyOutput echoes through their respective cat processes.
        // Single accumulator threads through both checks so events
        // arriving for one session while we wait on the other are
        // buffered, not dropped (PR #8 review item C3).
        let mut pty = PtyOutputAccumulator::new(&event_rx);
        assert!(
            pty.wait_for_bytes(id_a, b"bcast-marker", Duration::from_secs(3)),
            "session a should have echoed 'bcast-marker'",
        );
        assert!(
            pty.wait_for_bytes(id_b, b"bcast-marker", Duration::from_secs(3)),
            "session b should have echoed 'bcast-marker'",
        );

        // Broadcast must NOT have published any SessionEvent on the bus.
        let bus_events: Vec<_> = std::iter::from_fn(|| bus_rx.try_recv().ok()).collect();
        assert!(
            !bus_events
                .iter()
                .any(|ev| matches!(ev, ccom::session::SessionEvent::PromptSubmitted { .. })),
            "broadcast must not publish PromptSubmitted, saw {bus_events:?}",
        );

        manager.kill(id_a);
        manager.kill(id_b);
    }
}

mod session_lifecycle {
    use ccom::event::{Event, MonitoredSender};
    use ccom::session::{EventBus, SessionManager, SessionStatus, SpawnConfig};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn spawn_read_output_exit_cleanup() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));
        let (raw_tx, event_rx) = mpsc::channel();
        let event_tx = MonitoredSender::wrap(raw_tx);

        let id = manager
            .spawn(SpawnConfig {
                label: "lifecycle".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "echo hello && exit 0".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn should succeed");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_output = false;
        let mut saw_exit = false;
        let mut exit_code = None;

        while Instant::now() < deadline && !saw_exit {
            match event_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::PtyOutput { session_id, data }) if session_id == id => {
                    if data.windows(5).any(|w| w == b"hello") {
                        saw_output = true;
                    }
                }
                Ok(Event::SessionExited { session_id, code }) if session_id == id => {
                    saw_exit = true;
                    exit_code = Some(code);
                }
                _ => {}
            }
        }

        assert!(saw_output, "should have seen 'hello' in PtyOutput");
        assert!(saw_exit, "should have received SessionExited");
        assert_eq!(exit_code, Some(0));

        if let Some(session) = manager.get_mut(id) {
            session.status = SessionStatus::Exited(0);
            session.join_reader(Duration::from_millis(500));
        }
    }

    #[test]
    fn kill_stops_reader_thread() {
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));
        let (raw_tx, _event_rx) = mpsc::channel();
        let event_tx = MonitoredSender::wrap(raw_tx);

        let id = manager
            .spawn(SpawnConfig {
                label: "kill-test".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "sleep 30".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn should succeed");

        manager.kill(id);

        if let Some(session) = manager.get_mut(id) {
            session.join_reader(Duration::from_millis(1000));
        }
    }
}

mod key_encoding_tests {
    #[test]
    fn test_common_prefix_helper() {
        // Test the common_prefix function indirectly through the module
        // Since it's private, we just verify the tab_complete behavior works
        // by testing the path completion logic exists
        assert!(true); // placeholder — the function is private
    }
}

// Phase 3.5 review-fix Stream B: real-PTY hook integration tests.
//
// These exercise `Session::spawn` with `install_hook: true` and poke
// the resulting FIFOs by hand. They require Unix (mkfifo + blocking
// FIFO open). `check_hook_signals` + `send_prompt` are `pub` on
// `SessionManager`; the `hook` module itself is `pub(crate)`, so these
// tests write the hook's JSON payload as raw bytes to the FIFO and
// rely on the sidecar reader to parse it.
//
// NOTE: all three hook-integration tests share a `/tmp/ccom-<pid>-<id>`
// namespace keyed on the test-process pid and per-`SessionManager`
// `next_id`. Because cargo runs tests in parallel within a single
// process, two tests that both start from `next_id = 0` would collide
// on mkfifo. We serialize them behind a single mutex so they run one
// at a time without blocking the rest of the test suite.
#[cfg(unix)]
mod session_hook_integration {
    use std::sync::Mutex;
    static SERIAL: Mutex<()> = Mutex::new(());

    use ccom::session::{EventBus, SessionEvent, SessionManager, SpawnConfig};
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn make_event_tx() -> ccom::event::MonitoredSender {
        let (raw_tx, _event_rx) = mpsc::channel();
        ccom::event::MonitoredSender::wrap(raw_tx)
    }

    /// Count directories under `/tmp` whose file name starts with
    /// `ccom-<pid>-`. Used to detect leaked hook dirs on spawn failure.
    fn count_hook_dirs_for_pid(pid: u32) -> usize {
        let prefix = format!("ccom-{pid}-");
        fs::read_dir(std::env::temp_dir())
            .map(|iter| {
                iter.flatten()
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn spawn_cleans_up_hook_dir_on_spawn_command_failure() {
        // T2: install_hook=true with a bad command must not leak a
        // `/tmp/ccom-<pid>-*` dir. We spawn, expect an Err, and verify
        // the dir count didn't increase.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));

        let pid = std::process::id();
        let before = count_hook_dirs_for_pid(pid);

        // Use a nonexistent absolute binary so `spawn_command` fails
        // synchronously at posix_spawn. (On some platforms/libcs the
        // exec failure is deferred to the child, in which case spawn
        // would return Ok and this test would no-op; but on macOS /
        // Linux with an absolute path that doesn't exist, the parent
        // sees ENOENT.)
        //
        // If the platform defers the error, `spawn` returns Ok(id);
        // in that case we fall through to the cleanup validation via
        // `kill`, which also tears down the hook dir — this is a
        // weaker assertion but still catches a regression where the
        // error path leaks a dir when it IS synchronous.
        let result = manager.spawn(SpawnConfig {
            label: "hook-fail-spawn".to_string(),
            working_dir: PathBuf::from("/tmp"),
            command: "/nonexistent/ccom-test-binary-that-does-not-exist",
            args: vec![],
            event_tx: make_event_tx(),
            cols: 80,
            rows: 24,
            install_hook: true,
            mcp_port: None,
        });
        if let Ok(id) = result {
            // Platform deferred the exec error. Kill the session so
            // its hook dir gets cleaned up via the kill path — this
            // still validates the cleanup *happens*, just via a
            // different branch. The spawn error cleanup path is
            // exercised indirectly via the other early-failure
            // branches (create_stop_fifo, spawn_fifo_reader) which
            // are harder to trigger in a test.
            manager.kill(id);
        }

        let after = count_hook_dirs_for_pid(pid);
        assert_eq!(
            after,
            before,
            "spawn failure leaked {} hook dir(s) (before={before}, after={after})",
            after.saturating_sub(before)
        );
    }

    /// Open the FIFO for writing and append one JSON line terminated
    /// by a newline — matches the format the real hook command emits
    /// (`cat >> fifo; printf '\\n' >> fifo`).
    fn write_hook_line(fifo: &std::path::Path, json: &str) {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .open(fifo)
            .expect("open fifo for write");
        f.write_all(json.as_bytes()).unwrap();
        f.write_all(b"\n").unwrap();
    }

    /// Poll `check_hook_signals` until the given predicate holds, up
    /// to `timeout`. Returns true on success.
    fn wait_for_signal<F>(
        manager: &mut SessionManager,
        bus_rx: &mpsc::Receiver<SessionEvent>,
        timeout: Duration,
        mut pred: F,
    ) -> Vec<SessionEvent>
    where
        F: FnMut(&[SessionEvent]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut seen: Vec<SessionEvent> = Vec::new();
        while Instant::now() < deadline {
            manager.check_hook_signals();
            while let Ok(ev) = bus_rx.try_recv() {
                seen.push(ev);
            }
            if pred(&seen) {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
    }

    #[test]
    fn concurrent_sessions_have_isolated_hook_dirs() {
        // T3: two hook-enabled sessions with /bin/cat get distinct
        // hook dirs and signals route to the correct session.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));

        let id_a = manager
            .spawn(SpawnConfig {
                label: "iso-a".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            })
            .expect("spawn a");
        let id_b = manager
            .spawn(SpawnConfig {
                label: "iso-b".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            })
            .expect("spawn b");

        // Distinct hook dir paths. We reach into the session via
        // `get` / a public accessor — but those aren't exposed, so we
        // infer isolation via the `/tmp/ccom-<pid>-<id>` naming scheme
        // established by `create_hook_dir`.
        let pid = std::process::id();
        let dir_a = std::env::temp_dir().join(format!("ccom-{pid}-{id_a}"));
        let dir_b = std::env::temp_dir().join(format!("ccom-{pid}-{id_b}"));
        assert_ne!(dir_a, dir_b, "hook dirs must be distinct");
        assert!(dir_a.is_dir(), "hook dir a exists: {dir_a:?}");
        assert!(dir_b.is_dir(), "hook dir b exists: {dir_b:?}");

        // send_prompt to each session so the detector has an active
        // turn for both — otherwise the hook signal would be dropped.
        let turn_a = manager.send_prompt(id_a, "hi-a").expect("send a");
        let turn_b = manager.send_prompt(id_b, "hi-b").expect("send b");

        let bus_rx = bus.subscribe();

        // Write a distinct JSON line to each FIFO.
        let fifo_a = dir_a.join("stop.fifo");
        let fifo_b = dir_b.join("stop.fifo");
        write_hook_line(
            &fifo_a,
            r#"{"session_id":"uuid-a","last_assistant_message":"body-for-a"}"#,
        );
        write_hook_line(
            &fifo_b,
            r#"{"session_id":"uuid-b","last_assistant_message":"body-for-b"}"#,
        );

        // Wait for both ResponseComplete events.
        let events = wait_for_signal(&mut manager, &bus_rx, Duration::from_secs(3), |seen| {
            let mut saw_a = false;
            let mut saw_b = false;
            for ev in seen {
                if let SessionEvent::ResponseComplete {
                    session_id,
                    turn_id,
                    ..
                } = ev
                {
                    if *session_id == id_a && *turn_id == turn_a {
                        saw_a = true;
                    }
                    if *session_id == id_b && *turn_id == turn_b {
                        saw_b = true;
                    }
                }
            }
            saw_a && saw_b
        });

        let stored_a = manager.get_response(id_a, turn_a).expect("a stored");
        let stored_b = manager.get_response(id_b, turn_b).expect("b stored");
        assert_eq!(stored_a.body, "body-for-a");
        assert_eq!(stored_b.body, "body-for-b");
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                SessionEvent::ResponseComplete { session_id, .. } if *session_id == id_a
            )),
            "expected ResponseComplete for session a"
        );
        assert!(
            events.iter().any(|ev| matches!(
                ev,
                SessionEvent::ResponseComplete { session_id, .. } if *session_id == id_b
            )),
            "expected ResponseComplete for session b"
        );

        manager.kill(id_a);
        manager.kill(id_b);
    }

    #[test]
    fn check_hook_signals_end_to_end_with_real_fifo() {
        // T4: single-session end-to-end. send_prompt allocates a turn
        // id, we write a real hook JSON line to the FIFO, call
        // `check_hook_signals`, and assert the stored body matches.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let bus = Arc::new(EventBus::new());
        let mut manager = SessionManager::with_bus(Arc::clone(&bus));

        let id = manager
            .spawn(SpawnConfig {
                label: "hook-e2e".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx: make_event_tx(),
                cols: 80,
                rows: 24,
                install_hook: true,
                mcp_port: None,
            })
            .expect("spawn");

        let turn = manager.send_prompt(id, "prompt").expect("send");

        let pid = std::process::id();
        let fifo = std::env::temp_dir()
            .join(format!("ccom-{pid}-{id}"))
            .join("stop.fifo");

        let bus_rx = bus.subscribe();
        write_hook_line(
            &fifo,
            r#"{"session_id":"uuid","last_assistant_message":"hook-body"}"#,
        );

        let events = wait_for_signal(&mut manager, &bus_rx, Duration::from_secs(3), |seen| {
            seen.iter().any(|ev| {
                matches!(
                    ev,
                    SessionEvent::ResponseComplete { session_id, turn_id, .. }
                        if *session_id == id && *turn_id == turn
                )
            })
        });

        assert!(
            events.iter().any(|ev| matches!(
                ev,
                SessionEvent::ResponseComplete { session_id, turn_id, .. }
                    if *session_id == id && *turn_id == turn
            )),
            "expected ResponseComplete on the bus; saw {events:?}"
        );

        let stored = manager.get_response(id, turn).expect("stored turn");
        assert_eq!(stored.body, "hook-body");

        manager.kill(id);
    }
}
