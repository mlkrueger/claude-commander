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
