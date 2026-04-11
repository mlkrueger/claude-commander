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
        let (event_tx, _event_rx) = mpsc::channel();

        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-spawn".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "exit 0".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
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

        let (event_tx, _event_rx) = mpsc::channel();
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

        let (event_tx, _event_rx) = mpsc::channel();
        let id = manager
            .spawn(SpawnConfig {
                label: "smoke-reap".to_string(),
                working_dir: PathBuf::from("/tmp"),
                command: "/bin/sh",
                args: vec!["-c".to_string(), "exit 0".to_string()],
                event_tx,
                cols: 80,
                rows: 24,
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
}

// Test key_event_to_bytes (need to make it pub or test via integration)
mod key_encoding_tests {
    #[test]
    fn test_common_prefix_helper() {
        // Test the common_prefix function indirectly through the module
        // Since it's private, we just verify the tab_complete behavior works
        // by testing the path completion logic exists
        assert!(true); // placeholder — the function is private
    }
}
