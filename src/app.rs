use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::claude::launcher;
use crate::claude::rate_limit::{self, RateLimitInfo};
use crate::event::Event;
use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::pty::detector::PromptDetector;
use crate::session::{EventBus, SessionManager, SessionStatus, SpawnConfig, lock_parser};
use crate::setup::{self, SetupItem};
use crate::ui::layout::AppLayout;
use crate::ui::panels::command_bar::{self, CommandBar, CommandBarMode};
use crate::ui::panels::editor::{EditorPanel, EditorState};
use crate::ui::panels::file_tree::FileTreePanel;
use crate::ui::panels::session_list::SessionListPanel;
use crate::ui::panels::session_picker::SessionPickerPanel;
use crate::ui::panels::session_view::SessionViewPanel;
use crate::ui::panels::usage_graph::UsageGraphPanel;
use crate::ui::theme::{Theme, ThemeName};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Dashboard,
    SessionView(usize),
    SessionPicker(usize), // quick-pick overlay; usize = session we came from
    Editor,
    RenamePrompt,
    NewSessionModal,
    SendFilePrompt, // choose which session to send file path to
    Setup,          // setup/onboarding: show missing configs, offer to fix
    QuitConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Claude,
    Terminal,
}

impl SessionKind {
    fn toggle(&self) -> Self {
        match self {
            SessionKind::Claude => SessionKind::Terminal,
            SessionKind::Terminal => SessionKind::Claude,
        }
    }
}

pub struct NewSessionState {
    pub kind: SessionKind,
    pub dir_input: String,
    pub flags_input: String,
    pub focused: usize, // 0 = type, 1 = directory, 2 = flags
    pub status_message: Option<String>,
}

impl NewSessionState {
    fn new() -> Self {
        Self {
            kind: SessionKind::Claude,
            dir_input: String::new(),
            flags_input: String::new(),
            focused: 1,
            status_message: None,
        }
    }

    fn with_dir(dir: String) -> Self {
        let mut s = Self::new();
        s.dir_input = dir;
        s
    }

    fn field_count(&self) -> usize {
        3
    }

    fn extra_args(&self) -> Vec<String> {
        self.flags_input
            .split_whitespace()
            .map(|s| s.to_string())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelFocus {
    FileTree,
    SessionList,
}

pub struct App {
    pub(crate) sessions: SessionManager,
    /// Top-level event bus for high-level `SessionEvent`s. Owned at the
    /// `App` level so future consumers (Council controller, MCP server,
    /// stats panel) can subscribe without reaching through
    /// `SessionManager`. Holds the same `Arc` that `SessionManager`
    /// publishes to. Read by Phase 2+ consumers.
    #[allow(dead_code)]
    pub(crate) event_bus: Arc<EventBus>,
    pub mode: AppMode,
    pub focus: PanelFocus,
    pub file_tree: FileTree,
    pub file_tree_scroll: usize,
    pub should_quit: bool,
    pub event_tx: mpsc::Sender<Event>,
    pub detector: PromptDetector,
    pub input_buffer: String,
    pub working_dir: PathBuf,
    pub last_attention_check: Instant,
    pub terminal_cols: u16,
    pub terminal_rows: u16,
    pub session_view_scroll: usize,
    pub user_scrolled: bool, // true when user has scrolled up from bottom
    pub show_help: bool,
    pub status_message: Option<String>,
    pub editor: Option<EditorState>,
    pub git_status: Option<GitStatusMap>,
    pub last_git_refresh: Instant,
    pub last_usage_refresh: Instant,
    pub rate_limit: Option<RateLimitInfo>,
    pub setup_items: Vec<SetupItem>,
    pub setup_selected: usize,
    pub setup_banner_dismissed: bool,
    pub mouse_captured: bool,
    pub toggle_mouse_capture: bool, // signal to main loop to flip terminal mouse mode
    pub new_session: Option<NewSessionState>,
    pub theme: Theme,
    pub tick_count: u64,
    pub picker_selected: usize,
}

impl App {
    pub fn new(event_tx: mpsc::Sender<Event>, working_dir: PathBuf) -> Self {
        let file_tree = FileTree::new(working_dir.clone());
        let git_status = git::get_git_status(&working_dir);
        let rate_limit =
            rate_limit::get_rate_limit_info().or_else(rate_limit::get_rate_limit_from_telemetry);
        let setup_items = setup::missing_items();
        let initial_mode = if setup::is_first_launch() && !setup_items.is_empty() {
            AppMode::Setup
        } else {
            AppMode::Dashboard
        };
        let event_bus = Arc::new(EventBus::new());
        Self {
            sessions: SessionManager::with_bus(Arc::clone(&event_bus)),
            event_bus,
            mode: initial_mode,
            focus: PanelFocus::SessionList,
            file_tree,
            file_tree_scroll: 0,
            should_quit: false,
            event_tx,
            detector: PromptDetector::new(),
            input_buffer: String::new(),
            working_dir,
            last_attention_check: Instant::now(),
            terminal_cols: 80,
            terminal_rows: 24,
            session_view_scroll: 0,
            user_scrolled: false,
            show_help: false,
            status_message: None,
            editor: None,
            git_status,
            last_git_refresh: Instant::now(),
            last_usage_refresh: Instant::now() - Duration::from_secs(60),
            rate_limit,
            setup_items,
            setup_selected: 0,
            setup_banner_dismissed: false,
            mouse_captured: true,
            toggle_mouse_capture: false,
            new_session: None,
            theme: Theme::new(ThemeName::Default),
            tick_count: 0,
            picker_selected: 0,
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::PtyOutput { session_id, data } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.last_activity = Instant::now();
                }
                // Phase 3: feed bytes to the response boundary detector
                // so it can accumulate the active turn's body and fire
                // ResponseComplete on the next tick.
                self.sessions.feed_pty_data(session_id, &data);
                // Auto-scroll to bottom only when user hasn't manually scrolled up
                if let AppMode::SessionView(id) = self.mode {
                    if id == session_id && !self.user_scrolled {
                        self.session_view_scroll = 0;
                    }
                }
            }
            Event::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
                if self.last_attention_check.elapsed() > Duration::from_secs(1) {
                    self.check_all_attention();
                    self.last_attention_check = Instant::now();
                }
                // Refresh git status and context usage every 5 seconds
                if self.last_git_refresh.elapsed() > Duration::from_secs(5) {
                    self.git_status = git::get_git_status(&self.file_tree.root.path);
                    self.last_git_refresh = Instant::now();
                    self.sessions.refresh_contexts();
                }
                // Refresh usage graph and rate limits every 30 seconds
                if self.last_usage_refresh.elapsed() > Duration::from_secs(30) {
                    self.rate_limit = rate_limit::get_rate_limit_info()
                        .or_else(rate_limit::get_rate_limit_from_telemetry);
                    self.last_usage_refresh = Instant::now();
                }
                self.sessions.reap_exited();
            }
            Event::SessionExited { session_id, code } => {
                if let Some(session) = self.sessions.get_mut(session_id) {
                    session.status = SessionStatus::Exited(code);
                }
                if self.mode == AppMode::SessionView(session_id) {
                    self.mode = AppMode::Dashboard;
                }
            }
            Event::Resize(cols, rows) => {
                self.terminal_cols = cols;
                self.terminal_rows = rows;
                if let AppMode::SessionView(id) = self.mode {
                    if let Some(session) = self.sessions.get_mut(id) {
                        let inner_rows = rows.saturating_sub(3);
                        let inner_cols = cols.saturating_sub(2);
                        session.try_resize(inner_cols, inner_rows);
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Only handle key press events (crossterm 0.29 also sends Release/Repeat)
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Ctrl+C quits from dashboard/editor, but is forwarded to the session in SessionView
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            match &self.mode {
                AppMode::SessionView(_) | AppMode::SessionPicker(_) => {
                    // Fall through to mode-specific handler (forwarded to PTY)
                }
                _ => {
                    self.should_quit = true;
                    return;
                }
            }
        }

        // Alt+M: toggle mouse capture (allows native text selection when off)
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
            self.toggle_mouse_capture = true;
            return;
        }

        // t: cycle color theme (available in dashboard modes)
        if key.code == KeyCode::Char('t') && matches!(self.mode, AppMode::Dashboard) {
            let next = self.theme.name.next();
            self.theme = Theme::new(next);
            self.status_message = Some(format!("Theme: {}", next.label()));
            return;
        }

        match &self.mode {
            AppMode::Dashboard => self.handle_dashboard_key(key),
            AppMode::SessionView(id) => {
                let id = *id;
                self.handle_session_view_key(key, id);
            }
            AppMode::SessionPicker(from_id) => {
                let from_id = *from_id;
                self.handle_session_picker_key(key, from_id);
            }
            AppMode::Editor => self.handle_editor_key(key),
            AppMode::RenamePrompt => self.handle_rename_key(key),
            AppMode::NewSessionModal => self.handle_new_session_modal_key(key),
            AppMode::SendFilePrompt => self.handle_send_file_key(key),
            AppMode::Setup => self.handle_setup_key(key),
            AppMode::QuitConfirm => self.handle_quit_confirm_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let scroll_lines: usize = 3;
        match mouse.kind {
            MouseEventKind::ScrollUp => match &self.mode {
                AppMode::Dashboard => match self.focus {
                    PanelFocus::SessionList => {
                        if !self.sessions.is_empty() {
                            self.sessions.select_up_by(scroll_lines);
                            self.update_file_tree_for_selected();
                        }
                    }
                    PanelFocus::FileTree => {
                        for _ in 0..scroll_lines {
                            self.file_tree.move_up();
                        }
                        self.adjust_file_tree_scroll();
                    }
                },
                AppMode::SessionView(id) => {
                    let id = *id;
                    if let Some(session) = self.sessions.get(id) {
                        // Probe max scrollback by setting a large value and reading back
                        let mut parser = lock_parser(&session.parser);
                        parser.screen_mut().set_scrollback(usize::MAX);
                        let max_scroll = parser.screen().scrollback();
                        let desired = self.session_view_scroll + scroll_lines;
                        self.session_view_scroll = desired.min(max_scroll);
                        self.user_scrolled = self.session_view_scroll > 0;
                        // Reset so rendering sets it properly
                        parser.screen_mut().set_scrollback(0);
                    }
                }
                AppMode::Editor => {
                    if let Some(editor) = &mut self.editor {
                        for _ in 0..scroll_lines {
                            editor.move_up();
                        }
                        let visible = self.terminal_rows.saturating_sub(4) as usize;
                        editor.ensure_cursor_visible(visible);
                    }
                }
                _ => {}
            },
            MouseEventKind::ScrollDown => match &self.mode {
                AppMode::Dashboard => match self.focus {
                    PanelFocus::SessionList => {
                        if !self.sessions.is_empty() {
                            self.sessions.select_down_by(scroll_lines);
                            self.update_file_tree_for_selected();
                        }
                    }
                    PanelFocus::FileTree => {
                        for _ in 0..scroll_lines {
                            self.file_tree.move_down();
                        }
                        self.adjust_file_tree_scroll();
                    }
                },
                AppMode::SessionView(_) => {
                    self.session_view_scroll =
                        self.session_view_scroll.saturating_sub(scroll_lines);
                    self.user_scrolled = self.session_view_scroll > 0;
                }
                AppMode::Editor => {
                    if let Some(editor) = &mut self.editor {
                        for _ in 0..scroll_lines {
                            editor.move_down();
                        }
                        let visible = self.terminal_rows.saturating_sub(4) as usize;
                        editor.ensure_cursor_visible(visible);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        // If help modal is showing, close it on Esc or ?
        if self.show_help {
            if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                self.show_help = false;
            }
            return;
        }

        // Tab switches panel focus
        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                PanelFocus::FileTree => PanelFocus::SessionList,
                PanelFocus::SessionList => PanelFocus::FileTree,
            };
            // Update file tree root to selected session's dir
            if self.focus == PanelFocus::FileTree {
                if let Some(session) = self.sessions.selected() {
                    let dir = session.working_dir.clone();
                    if dir != self.file_tree.root.path {
                        self.file_tree.set_root(dir);
                    }
                }
            }
            return;
        }

        match self.focus {
            PanelFocus::SessionList => self.handle_session_list_key(key),
            PanelFocus::FileTree => self.handle_file_tree_key(key),
        }
    }

    fn handle_session_list_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.mode = AppMode::QuitConfirm,
            KeyCode::Down => {
                if !self.sessions.is_empty() {
                    self.sessions.select_next();
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Up => {
                if !self.sessions.is_empty() {
                    self.sessions.select_prev();
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Enter => {
                let inner_rows = self.terminal_rows.saturating_sub(3);
                let inner_cols = self.terminal_cols.saturating_sub(2);
                if let Some(session) = self.sessions.selected_mut() {
                    let id = session.id;
                    session.try_resize(inner_cols, inner_rows);
                    self.session_view_scroll = 0;
                    self.user_scrolled = false;
                    self.mode = AppMode::SessionView(id);
                }
            }
            KeyCode::Char('n') => {
                self.new_session = Some(NewSessionState::new());
                self.mode = AppMode::NewSessionModal;
            }
            KeyCode::Char('a') => self.approve_selected(),
            KeyCode::Char('d') => self.deny_selected(),
            KeyCode::Char('r') => {
                if let Some(session) = self.sessions.selected() {
                    self.input_buffer = session.label.clone();
                    self.mode = AppMode::RenamePrompt;
                }
            }
            KeyCode::Char('K') => self.kill_selected(),
            KeyCode::Char('c') => self.send_commit_prompt(),
            KeyCode::Char('x') => self.clear_dead_sessions(),
            KeyCode::Char('S') => {
                self.setup_items = setup::missing_items();
                self.setup_selected = 0;
                self.mode = AppMode::Setup;
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            _ => {}
        }
    }

    fn handle_file_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.mode = AppMode::QuitConfirm,
            KeyCode::Down => {
                self.file_tree.move_down();
                self.adjust_file_tree_scroll();
            }
            KeyCode::Up => {
                self.file_tree.move_up();
                self.adjust_file_tree_scroll();
            }
            KeyCode::Enter | KeyCode::Right => {
                // If dir: toggle expand. If file: no-op for now (Phase 3 editor)
                if let Some(path) = self.file_tree.selected_path() {
                    if path.is_dir() {
                        self.file_tree.toggle_selected();
                    }
                }
            }
            KeyCode::Left => {
                // Collapse current dir
                if let Some(path) = self.file_tree.selected_path() {
                    if path.is_dir() {
                        self.file_tree.toggle_selected();
                    }
                }
            }
            KeyCode::Char('n') => {
                // Open new session modal pre-filled with selected directory
                if let Some(path) = self.file_tree.selected_path() {
                    let dir = if path.is_dir() {
                        path.to_path_buf()
                    } else {
                        path.parent().unwrap_or(path).to_path_buf()
                    };
                    self.new_session = Some(NewSessionState::with_dir(dir.display().to_string()));
                    self.mode = AppMode::NewSessionModal;
                    self.focus = PanelFocus::SessionList;
                }
            }
            KeyCode::Char('R') => {
                self.file_tree.refresh();
            }
            KeyCode::Char('e') => {
                // Open file in editor
                if let Some(path) = self.file_tree.selected_path() {
                    if path.is_file() {
                        self.open_editor(path.to_path_buf());
                    }
                }
            }
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
            }
            _ => {}
        }
    }

    fn handle_editor_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            // Ctrl+S: save
            KeyCode::Char('s') if ctrl => {
                if let Some(editor) = &mut self.editor {
                    if let Err(e) = editor.save() {
                        editor.message = Some(format!("Save failed: {e}"));
                    }
                }
            }
            // Alt+D: back to dashboard
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.editor = None;
                self.mode = AppMode::Dashboard;
            }
            // Ctrl+P: send file path to a session
            KeyCode::Char('p') if ctrl => {
                if !self.sessions.is_empty() {
                    self.mode = AppMode::SendFilePrompt;
                } else if let Some(editor) = &mut self.editor {
                    editor.message = Some("No sessions to send to.".to_string());
                }
            }
            // Navigation
            KeyCode::Up => {
                if let Some(editor) = &mut self.editor {
                    editor.move_up();
                }
            }
            KeyCode::Down => {
                if let Some(editor) = &mut self.editor {
                    editor.move_down();
                }
            }
            KeyCode::Left => {
                if let Some(editor) = &mut self.editor {
                    editor.move_left();
                }
            }
            KeyCode::Right => {
                if let Some(editor) = &mut self.editor {
                    editor.move_right();
                }
            }
            KeyCode::Home => {
                if let Some(editor) = &mut self.editor {
                    editor.move_home();
                }
            }
            KeyCode::End => {
                if let Some(editor) = &mut self.editor {
                    editor.move_end();
                }
            }
            KeyCode::PageUp => {
                if let Some(editor) = &mut self.editor {
                    let page = self.terminal_rows.saturating_sub(4) as usize;
                    editor.page_up(page);
                }
            }
            KeyCode::PageDown => {
                if let Some(editor) = &mut self.editor {
                    let page = self.terminal_rows.saturating_sub(4) as usize;
                    editor.page_down(page);
                }
            }
            // Editing
            KeyCode::Enter => {
                if let Some(editor) = &mut self.editor {
                    editor.insert_newline();
                }
            }
            KeyCode::Backspace => {
                if let Some(editor) = &mut self.editor {
                    editor.backspace();
                }
            }
            KeyCode::Delete => {
                if let Some(editor) = &mut self.editor {
                    editor.delete();
                }
            }
            KeyCode::Tab => {
                // Insert 4 spaces
                if let Some(editor) = &mut self.editor {
                    for _ in 0..4 {
                        editor.insert_char(' ');
                    }
                }
            }
            KeyCode::Char(c) => {
                if let Some(editor) = &mut self.editor {
                    editor.insert_char(c);
                }
            }
            _ => {}
        }

        // Keep cursor visible
        if let Some(editor) = &mut self.editor {
            let visible = self.terminal_rows.saturating_sub(4) as usize;
            editor.ensure_cursor_visible(visible);
        }
    }

    fn handle_send_file_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::Editor;
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let idx = c.to_digit(10).unwrap() as usize;
                self.send_file_to_session(idx);
                self.mode = AppMode::Editor;
            }
            KeyCode::Enter => {
                // Send to first session
                if !self.sessions.is_empty() {
                    self.send_file_to_session(0);
                }
                self.mode = AppMode::Editor;
            }
            _ => {}
        }
    }

    fn open_editor(&mut self, path: PathBuf) {
        match EditorState::open(path) {
            Ok(state) => {
                self.editor = Some(state);
                self.mode = AppMode::Editor;
            }
            Err(e) => {
                self.status_message = Some(format!("Can't open file: {e}"));
            }
        }
    }

    fn send_file_to_session(&mut self, idx: usize) {
        let file_path = self.editor.as_ref().map(|e| e.file_path.clone());
        if let Some(path) = file_path {
            // idx here is a positional index into the session list, not an id.
            let session_id = self.sessions.iter().nth(idx).map(|s| s.id);
            if let Some(id) = session_id {
                if let Some(session) = self.sessions.get_mut(id) {
                    let msg = format!("Read the file at {}\n", path.display());
                    session.try_write(msg.as_bytes());
                    let label = session.label.clone();
                    if let Some(editor) = &mut self.editor {
                        editor.message = Some(format!("Sent to session {label}"));
                    }
                }
            } else if let Some(editor) = &mut self.editor {
                editor.message = Some(format!("No session at index {idx}"));
            }
        }
    }

    fn handle_session_view_key(&mut self, key: KeyEvent, session_id: usize) {
        // Alt+D: back to dashboard
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('d') {
            self.mode = AppMode::Dashboard;
            return;
        }

        // Alt+S: session quick-picker
        if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('s') {
            if self.sessions.len() > 1 {
                // Pre-select the current session in the picker
                self.picker_selected = self
                    .sessions
                    .iter()
                    .position(|s| s.id == session_id)
                    .unwrap_or(0);
                self.mode = AppMode::SessionPicker(session_id);
            }
            return;
        }

        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty() {
            if let Some(session) = self.sessions.get_mut(session_id) {
                session.try_write(&bytes);
            }
        }
    }

    fn handle_session_picker_key(&mut self, key: KeyEvent, from_session_id: usize) {
        match key.code {
            KeyCode::Esc => {
                self.mode = AppMode::SessionView(from_session_id);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.sessions.is_empty() {
                    self.picker_selected = (self.picker_selected + 1) % self.sessions.len();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if !self.sessions.is_empty() {
                    self.picker_selected = self
                        .picker_selected
                        .checked_sub(1)
                        .unwrap_or(self.sessions.len() - 1);
                }
            }
            KeyCode::Enter => {
                let picker_idx = self.picker_selected;
                let id = self.sessions.iter().nth(picker_idx).map(|s| s.id);
                if let Some(id) = id {
                    // Resize to fit the session view
                    if let Some(session) = self.sessions.get_mut(id) {
                        let inner_rows = self.terminal_rows.saturating_sub(3);
                        let inner_cols = self.terminal_cols.saturating_sub(2);
                        session.try_resize(inner_cols, inner_rows);
                    }
                    self.sessions.set_selected(picker_idx);
                    self.mode = AppMode::SessionView(id);
                }
            }
            _ => {}
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() {
                    if let Some(session) = self.sessions.selected_mut() {
                        session.label = self.input_buffer.clone();
                    }
                }
                self.input_buffer.clear();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Esc => {
                self.input_buffer.clear();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
    }

    fn handle_new_session_modal_key(&mut self, key: KeyEvent) {
        let focused = match &self.new_session {
            Some(s) => s.focused,
            None => return,
        };

        match key.code {
            KeyCode::Esc => {
                self.new_session = None;
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Enter => {
                self.spawn_from_modal();
            }
            KeyCode::Up => {
                if let Some(state) = &mut self.new_session {
                    if state.focused > 0 {
                        state.focused -= 1;
                        state.status_message = None;
                    }
                }
            }
            KeyCode::Down => {
                if let Some(state) = &mut self.new_session {
                    if state.focused < state.field_count() - 1 {
                        state.focused += 1;
                        state.status_message = None;
                    }
                }
            }
            KeyCode::Tab if focused == 1 => {
                self.tab_complete_path();
            }
            KeyCode::Left | KeyCode::Right if focused == 0 => {
                if let Some(state) = &mut self.new_session {
                    state.kind = state.kind.toggle();
                    state.status_message = None;
                }
            }
            KeyCode::Char(' ') if focused == 0 => {
                if let Some(state) = &mut self.new_session {
                    state.kind = state.kind.toggle();
                    state.status_message = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(state) = &mut self.new_session {
                    match focused {
                        1 => {
                            state.dir_input.pop();
                        }
                        2 => {
                            state.flags_input.pop();
                        }
                        _ => {}
                    }
                    state.status_message = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(state) = &mut self.new_session {
                    match focused {
                        1 => state.dir_input.push(c),
                        2 => state.flags_input.push(c),
                        _ => {}
                    }
                    state.status_message = None;
                }
            }
            _ => {}
        }
    }

    fn handle_quit_confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.should_quit = true;
            }
            _ => {
                self.mode = AppMode::Dashboard;
            }
        }
    }

    fn spawn_from_modal(&mut self) {
        let state = match self.new_session.take() {
            Some(s) => s,
            None => return,
        };

        let dir = if state.dir_input.is_empty() {
            self.working_dir.clone()
        } else {
            PathBuf::from(shellexpand::tilde(&state.dir_input).to_string())
        };

        if !dir.is_dir() {
            let msg = format!("Not a directory: {}", dir.display());
            self.new_session = Some(NewSessionState {
                status_message: Some(msg),
                ..state
            });
            return;
        }

        let extra_args = state.extra_args();
        let kind = state.kind;
        self.spawn_session_kind(kind, dir, extra_args, None);
        self.mode = AppMode::Dashboard;
    }

    fn tab_complete_path(&mut self) {
        let state = match &self.new_session {
            Some(s) => s,
            None => return,
        };

        let expanded = shellexpand::tilde(&state.dir_input).to_string();
        let path = PathBuf::from(&expanded);

        // Split into parent dir and partial name
        let (search_dir, prefix) = if path.is_dir() && state.dir_input.ends_with('/') {
            (path.clone(), String::new())
        } else {
            let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            let partial = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            (parent, partial)
        };

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return;
        };

        let mut matches: Vec<String> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                e.path().is_dir() && name.starts_with(&prefix) && !name.starts_with('.')
            })
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        matches.sort();

        if matches.is_empty() {
            return;
        }

        let state = self.new_session.as_mut().unwrap();
        if matches.len() == 1 {
            let completed = search_dir.join(&matches[0]);
            let home = dirs::home_dir().unwrap_or_default();
            let display = match completed.strip_prefix(&home).ok() {
                Some(rel) => format!("~{}/", rel.display()),
                None => format!("{}/", completed.display()),
            };
            state.dir_input = display;
            state.status_message = None;
        } else {
            let common = common_prefix(&matches);
            if common.len() > prefix.len() {
                let completed = search_dir.join(&common);
                let home = dirs::home_dir().unwrap_or_default();
                let display = match completed.strip_prefix(&home).ok() {
                    Some(rel) => format!("~{}", rel.display()),
                    None => format!("{}", completed.display()),
                };
                state.dir_input = display;
            }
            let display_matches: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
            state.status_message = Some(display_matches.join("  "));
        }
    }

    pub fn spawn_session(&mut self, working_dir: PathBuf) {
        self.spawn_session_with_prompt(working_dir, vec![], None);
    }

    pub fn spawn_session_with_prompt(
        &mut self,
        working_dir: PathBuf,
        extra_args: Vec<String>,
        initial_prompt: Option<String>,
    ) {
        self.spawn_session_kind(SessionKind::Claude, working_dir, extra_args, initial_prompt);
    }

    pub fn spawn_session_kind(
        &mut self,
        kind: SessionKind,
        working_dir: PathBuf,
        extra_args: Vec<String>,
        initial_prompt: Option<String>,
    ) {
        let next_id = self.sessions.peek_next_id();
        let (cmd_owned, args, label): (String, Vec<String>, String) = match kind {
            SessionKind::Claude => {
                let mut a: Vec<String> = launcher::claude_args()
                    .into_iter()
                    .map(String::from)
                    .collect();
                a.extend(extra_args);
                if let Some(prompt) = initial_prompt {
                    a.push(prompt);
                }
                (
                    launcher::claude_command().to_string(),
                    a,
                    format!("claude-{next_id}"),
                )
            }
            SessionKind::Terminal => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                (shell, extra_args, format!("term-{next_id}"))
            }
        };
        let cmd: &str = &cmd_owned;

        let cols = self.terminal_cols.saturating_sub(34).max(40); // account for file tree
        let rows = self.terminal_rows.saturating_sub(3).max(10);

        let config = SpawnConfig {
            label,
            working_dir,
            command: cmd,
            args,
            event_tx: self.event_tx.clone(),
            cols,
            rows,
        };

        match self.sessions.spawn(config) {
            Ok(_id) => {
                self.update_file_tree_for_selected();
            }
            Err(e) => {
                log::error!("Failed to spawn session: {e}");
            }
        }
    }

    fn approve_selected(&mut self) {
        if let Some(session) = self.sessions.selected_mut() {
            session.try_write(b"\r");
        }
    }

    fn deny_selected(&mut self) {
        if let Some(session) = self.sessions.selected_mut() {
            session.try_write(b"\x1b[B\x1b[B\r");
        }
    }

    fn kill_selected(&mut self) {
        let id = self.sessions.selected().map(|s| s.id);
        if let Some(id) = id {
            self.sessions.kill(id);
        }
    }

    fn send_commit_prompt(&mut self) {
        if let Some(session) = self.sessions.selected_mut() {
            session.try_write(b"/commit\n");
            let label = session.label.clone();
            self.status_message = Some(format!("Sent /commit to {label}"));
        }
    }

    fn clear_dead_sessions(&mut self) {
        self.sessions.retain_alive();
    }

    fn handle_setup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.setup_banner_dismissed = true;
                setup::mark_initialized();
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Up => {
                self.setup_selected = self.setup_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if !self.setup_items.is_empty() {
                    self.setup_selected =
                        (self.setup_selected + 1).min(self.setup_items.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                // Fix all missing items by spawning a setup session
                self.spawn_setup_session();
            }
            _ => {}
        }
    }

    fn spawn_setup_session(&mut self) {
        let missing: Vec<String> = self
            .setup_items
            .iter()
            .filter(|i| i.status == setup::SetupStatus::Missing)
            .map(|i| i.fix_prompt.clone())
            .collect();

        if missing.is_empty() {
            self.status_message = Some("All setup items are configured!".to_string());
            setup::mark_initialized();
            self.mode = AppMode::Dashboard;
            return;
        }

        // Build a combined prompt and pass it as an initial arg to claude
        let prompt = missing.join("\n\n---\n\n");

        // Spawn session in the home directory (settings are global)
        let home = dirs::home_dir().unwrap_or_else(|| self.working_dir.clone());
        self.spawn_session_with_prompt(home, vec![], Some(prompt));

        let session_id = self.sessions.selected().map(|s| s.id);

        if let Some(id) = session_id {
            setup::mark_initialized();
            self.mode = AppMode::SessionView(id);
        } else {
            self.status_message = Some("Failed to spawn setup session".to_string());
            self.mode = AppMode::Dashboard;
        }
    }

    fn check_all_attention(&mut self) {
        self.sessions.check_attention(&self.detector);
        // Phase 3: same cadence — check whether any session's active
        // turn has produced its idle marker. On a hit, the detector
        // pushes a `StoredTurn` into the session's response store
        // and publishes `ResponseComplete` on the bus.
        self.sessions.check_response_boundaries();
    }

    fn update_file_tree_for_selected(&mut self) {
        if let Some(session) = self.sessions.selected() {
            let dir = session.working_dir.clone();
            if dir != self.file_tree.root.path {
                self.file_tree.set_root(dir.clone());
                self.file_tree_scroll = 0;
                self.git_status = git::get_git_status(&dir);
                self.last_git_refresh = Instant::now();
            }
        }
    }

    fn adjust_file_tree_scroll(&mut self) {
        let visible_height = self.terminal_rows.saturating_sub(4) as usize; // borders + cmd bar
        if self.file_tree.selected < self.file_tree_scroll {
            self.file_tree_scroll = self.file_tree.selected;
        } else if self.file_tree.selected >= self.file_tree_scroll + visible_height {
            self.file_tree_scroll = self.file_tree.selected - visible_height + 1;
        }
    }

    fn session_dirs(&self) -> Vec<PathBuf> {
        self.sessions
            .iter()
            .filter(|s| !matches!(s.status, SessionStatus::Exited(_)))
            .map(|s| s.working_dir.clone())
            .collect()
    }

    fn sel_idx_or_zero(&self) -> usize {
        self.sessions.selected_index().unwrap_or(0)
    }

    pub fn draw(&self, frame: &mut Frame) {
        let th = &self.theme;
        let tick = self.tick_count;

        match &self.mode {
            AppMode::Editor | AppMode::SendFilePrompt => {
                let (main_area, cmd_area) = AppLayout::session_view(frame.area());

                if let Some(editor) = &self.editor {
                    let panel = EditorPanel::new(editor, th, tick);
                    frame.render_widget(panel, main_area);

                    if let Some(msg) = &editor.message {
                        let line = ratatui::text::Line::styled(
                            msg.clone(),
                            ratatui::style::Style::default().fg(th.status_warn),
                        );
                        frame.render_widget(line, cmd_area);
                    } else if self.mode == AppMode::SendFilePrompt {
                        let labels: Vec<String> =
                            self.sessions.iter().map(|s| s.label.clone()).collect();
                        let bar = CommandBar::new(CommandBarMode::SendFile(labels), th);
                        frame.render_widget(bar, cmd_area);
                    } else {
                        let bar = CommandBar::new(CommandBarMode::Editor, th);
                        frame.render_widget(bar, cmd_area);
                    }
                }
            }
            AppMode::Dashboard
            | AppMode::RenamePrompt
            | AppMode::NewSessionModal
            | AppMode::QuitConfirm => {
                let layout = AppLayout::new(frame.area());

                // File tree (left panel)
                let session_dirs = self.session_dirs();
                let tree_panel = FileTreePanel::new(
                    &self.file_tree,
                    self.focus == PanelFocus::FileTree,
                    &session_dirs,
                    th,
                    tick,
                )
                .with_scroll(self.file_tree_scroll)
                .with_git_status(self.git_status.as_ref());
                frame.render_widget(tree_panel, layout.file_tree);

                // Session list (main panel)
                let session_list = SessionListPanel::new(
                    self.sessions.as_slice(),
                    self.sel_idx_or_zero(),
                    self.focus == PanelFocus::SessionList,
                    th,
                    tick,
                );
                frame.render_widget(session_list, layout.main);

                // Setup banner
                let show_banner = !self.setup_banner_dismissed && !self.setup_items.is_empty();
                let usage_area = if show_banner && layout.usage_graph.height > 1 {
                    let banner = ratatui::text::Line::from(vec![
                        ratatui::text::Span::styled(
                            " Setup needed ",
                            ratatui::style::Style::default()
                                .fg(th.selected_fg)
                                .bg(th.status_warn),
                        ),
                        ratatui::text::Span::styled(
                            format!(" {} item(s) — press S to configure", self.setup_items.len()),
                            ratatui::style::Style::default().fg(th.status_warn),
                        ),
                    ]);
                    let banner_area = ratatui::layout::Rect {
                        x: layout.usage_graph.x,
                        y: layout.usage_graph.y,
                        width: layout.usage_graph.width,
                        height: 1,
                    };
                    frame.render_widget(banner, banner_area);
                    ratatui::layout::Rect {
                        x: layout.usage_graph.x,
                        y: layout.usage_graph.y + 1,
                        width: layout.usage_graph.width,
                        height: layout.usage_graph.height - 1,
                    }
                } else {
                    layout.usage_graph
                };

                // Usage graph (bottom panel)
                let usage_panel =
                    UsageGraphPanel::new(th, tick).with_rate_limit(self.rate_limit.as_ref());
                frame.render_widget(usage_panel, usage_area);

                // Command bar
                match &self.mode {
                    AppMode::RenamePrompt => {
                        let prompt = format!("Rename: {}_", self.input_buffer);
                        let line = ratatui::text::Line::raw(prompt);
                        frame.render_widget(line, layout.command_bar);
                    }
                    _ => {
                        let bar_mode = match self.focus {
                            PanelFocus::SessionList => CommandBarMode::Dashboard,
                            PanelFocus::FileTree => CommandBarMode::FileTree,
                        };
                        let command_bar = CommandBar::new(bar_mode, th);
                        frame.render_widget(command_bar, layout.command_bar);
                    }
                }

                // Modal overlays
                if self.show_help {
                    self.draw_help_modal(frame);
                } else if self.mode == AppMode::NewSessionModal {
                    self.draw_new_session_modal(frame);
                } else if self.mode == AppMode::QuitConfirm {
                    self.draw_quit_confirm(frame);
                }
            }
            AppMode::SessionView(id) | AppMode::SessionPicker(id) => {
                let (main_area, cmd_area) = AppLayout::session_view(frame.area());

                let context_pct = if let Some(session) = self.sessions.get(*id) {
                    let view = SessionViewPanel::new(session, th, tick)
                        .with_scroll(self.session_view_scroll);
                    frame.render_widget(view, main_area);
                    session.context_percent
                } else {
                    None
                };

                if matches!(self.mode, AppMode::SessionPicker(_)) {
                    // Render picker overlay
                    let picker =
                        SessionPickerPanel::new(self.sessions.as_slice(), self.picker_selected, th);
                    frame.render_widget(picker, main_area);

                    let command_bar = CommandBar::new(CommandBarMode::SessionPicker, th);
                    frame.render_widget(command_bar, cmd_area);
                } else {
                    let usage = command_bar::UsageStats {
                        context_pct,
                        session_pct: self.rate_limit.as_ref().and_then(|r| r.session_pct),
                        weekly_pct: self.rate_limit.as_ref().and_then(|r| r.weekly_pct),
                    };
                    let command_bar =
                        CommandBar::new(CommandBarMode::SessionView, th).with_usage(usage);
                    frame.render_widget(command_bar, cmd_area);
                }
            }
            AppMode::Setup => {
                let (main_area, cmd_area) = AppLayout::session_view(frame.area());
                self.draw_setup_screen(frame, main_area);

                let bar = CommandBar::new(CommandBarMode::Setup, th);
                frame.render_widget(bar, cmd_area);
            }
        }
    }

    fn draw_setup_screen(&self, frame: &mut Frame, area: Rect) {
        use ratatui::style::{Color, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Paragraph};
        let th = &self.theme;

        let block = Block::default()
            .title(" Setup ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());

        let inner = block.inner(area);
        frame.render_widget(block, area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), area, self.tick_count);
        }

        let mut lines = Vec::new();

        if self.setup_items.is_empty() {
            lines.push(Line::styled(
                "  All configurations are in place!",
                Style::default().fg(Color::Green),
            ));
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  Press Esc to return.",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            lines.push(Line::styled(
                "  The following configurations are needed for full functionality:",
                Style::default().fg(Color::Yellow),
            ));
            lines.push(Line::raw(""));

            for (i, item) in self.setup_items.iter().enumerate() {
                let marker = if i == self.setup_selected {
                    " > "
                } else {
                    "   "
                };
                let (icon, color) = match item.status {
                    setup::SetupStatus::Ok => ("OK", Color::Green),
                    setup::SetupStatus::Missing => ("MISSING", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, Style::default().fg(Color::Cyan)),
                    Span::styled(format!("[{icon}] "), Style::default().fg(color)),
                    Span::styled(&item.name, Style::default().fg(Color::White)),
                    Span::styled(
                        format!(" — {}", item.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }

            let missing_count = self
                .setup_items
                .iter()
                .filter(|i| i.status == setup::SetupStatus::Missing)
                .count();

            if missing_count > 0 {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    "  Press Enter or 'y' to fix — this will start a Claude session",
                    Style::default().fg(Color::Cyan),
                ));
                lines.push(Line::styled(
                    "  that configures the missing items for you.",
                    Style::default().fg(Color::Cyan),
                ));
            }
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }

    fn draw_new_session_modal(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear};
        let th = &self.theme;

        let state = match &self.new_session {
            Some(s) => s,
            None => return,
        };

        let area = frame.area();
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 13u16.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" New Session ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let mut row = inner.y;
        let field_width = inner.width.saturating_sub(4);

        // Type field
        let type_focused = state.focused == 0;
        let type_style = if type_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let type_label = Line::styled("  Type:", type_style);
        frame.render_widget(type_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;
        let claude_selected = state.kind == SessionKind::Claude;
        let term_selected = state.kind == SessionKind::Terminal;
        let sel_style = Style::default().fg(th.text);
        let unsel_style = Style::default().fg(th.dim);
        let type_line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if claude_selected {
                    "● Claude"
                } else {
                    "○ Claude"
                },
                if claude_selected {
                    sel_style
                } else {
                    unsel_style
                },
            ),
            Span::raw("   "),
            Span::styled(
                if term_selected {
                    "● Terminal"
                } else {
                    "○ Terminal"
                },
                if term_selected {
                    sel_style
                } else {
                    unsel_style
                },
            ),
        ]);
        frame.render_widget(type_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        // Directory field
        let dir_focused = state.focused == 1;
        let dir_style = if dir_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let dir_label = Line::styled("  Directory:", dir_style);
        frame.render_widget(dir_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;

        let dir_text = if state.dir_input.is_empty() {
            format!("{} (default)", self.working_dir.display())
        } else if dir_focused {
            format!("{}█", state.dir_input)
        } else {
            state.dir_input.clone()
        };
        let dir_display = if dir_text.len() > field_width as usize {
            let skip = dir_text.len() - field_width as usize + 1;
            format!("  …{}", &dir_text[skip..])
        } else {
            format!("  > {dir_text}")
        };
        let cursor_style = if dir_focused {
            Style::default().fg(th.text)
        } else {
            Style::default().fg(th.dim)
        };
        let dir_line = Line::styled(dir_display, cursor_style);
        frame.render_widget(dir_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        // Flags field
        let flags_focused = state.focused == 2;
        let flags_style = if flags_focused {
            Style::default().fg(th.accent)
        } else {
            Style::default().fg(th.dim)
        };
        let flags_label = Line::styled("  Flags:", flags_style);
        frame.render_widget(flags_label, Rect::new(inner.x, row, inner.width, 1));
        row += 1;

        let flags_text = if state.flags_input.is_empty() && !flags_focused {
            "(none)".to_string()
        } else if flags_focused {
            format!("{}█", state.flags_input)
        } else {
            state.flags_input.clone()
        };
        let flags_display = if flags_text.len() > field_width as usize {
            let skip = flags_text.len() - field_width as usize + 1;
            format!("  …{}", &flags_text[skip..])
        } else {
            format!("  > {flags_text}")
        };
        let fcursor_style = if flags_focused {
            Style::default().fg(th.text)
        } else {
            Style::default().fg(th.dim)
        };
        let flags_line = Line::styled(flags_display, fcursor_style);
        frame.render_widget(flags_line, Rect::new(inner.x, row, inner.width, 1));
        row += 2;

        if let Some(msg) = &state.status_message {
            let msg_display = if msg.len() + 2 > inner.width as usize {
                format!("  {}…", &msg[..inner.width as usize - 3])
            } else {
                format!("  {msg}")
            };
            let line = Line::styled(msg_display, Style::default().fg(th.status_warn));
            frame.render_widget(line, Rect::new(inner.x, row, inner.width, 1));
        } else {
            let help = Line::from(vec![
                Span::styled("  [Enter]", th.shortcut_key()),
                Span::styled(" Create ", th.shortcut_desc()),
                Span::styled("[Tab]", th.shortcut_key()),
                Span::styled(" Complete ", th.shortcut_desc()),
                Span::styled("[Esc]", th.shortcut_key()),
                Span::styled(" Cancel", th.shortcut_desc()),
            ]);
            frame.render_widget(help, Rect::new(inner.x, row, inner.width, 1));
        }
    }

    fn draw_quit_confirm(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear};
        let th = &self.theme;

        let area = frame.area();
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = 7u16.min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Quit ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(th.status_warn));
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let has_running = self
            .sessions
            .iter()
            .any(|s| !matches!(s.status, SessionStatus::Exited(_)));
        let msg = if has_running {
            "  Quit ccom? Running sessions will be killed."
        } else {
            "  Quit ccom?"
        };
        let line = Line::styled(msg, Style::default().fg(th.text));
        frame.render_widget(line, Rect::new(inner.x, inner.y + 1, inner.width, 1));

        let help = Line::from(vec![
            Span::styled("  [y]", Style::default().fg(th.status_warn)),
            Span::styled(" Yes  ", th.shortcut_desc()),
            Span::styled("[n/Esc]", th.shortcut_key()),
            Span::styled(" No", th.shortcut_desc()),
        ]);
        frame.render_widget(help, Rect::new(inner.x, inner.y + 3, inner.width, 1));
    }

    fn draw_help_modal(&self, frame: &mut Frame) {
        use ratatui::style::Style;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Block, Borders, Clear, Paragraph};
        let th = &self.theme;

        let sections: &[(&str, &[(&str, &str)])] = &[
            (
                "Session Management",
                &[
                    ("n", "New session"),
                    ("Enter", "View selected session"),
                    ("a", "Approve tool request"),
                    ("d", "Deny tool request"),
                    ("c", "Send commit prompt"),
                    ("K", "Kill session"),
                    ("x", "Clear dead sessions"),
                    ("r", "Rename session"),
                ],
            ),
            (
                "Navigation",
                &[
                    ("↑/↓", "Navigate list"),
                    ("Tab", "Switch panel (sessions/files)"),
                    ("S", "Open setup screen"),
                ],
            ),
            (
                "File Tree",
                &[
                    ("Enter/→", "Expand directory"),
                    ("←", "Collapse directory"),
                    ("e", "Edit file"),
                    ("n", "New session in directory"),
                    ("R", "Refresh tree"),
                ],
            ),
            (
                "General",
                &[
                    ("t", "Cycle color theme"),
                    ("C-S-m", "Toggle mouse capture"),
                    ("?", "Toggle this help"),
                    ("q", "Quit"),
                    ("Ctrl+C", "Force quit"),
                ],
            ),
        ];

        // Calculate height: 1 per section header + 1 per entry + 1 blank between sections + border
        let content_lines: u16 = sections
            .iter()
            .map(|(_, entries)| 1 + entries.len() as u16)
            .sum::<u16>()
            + (sections.len() as u16).saturating_sub(1); // blank lines between sections
        let height = (content_lines + 3).min(frame.area().height.saturating_sub(2)); // +3 for borders + bottom hint
        let width = 48u16.min(frame.area().width.saturating_sub(4));
        let area = frame.area();
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let modal_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .title(" Keyboard Shortcuts ")
            .borders(Borders::ALL)
            .border_style(th.border_focused());
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);
        if th.is_rainbow() {
            crate::ui::theme::paint_rainbow_border(frame.buffer_mut(), modal_area, self.tick_count);
        }

        let mut lines = Vec::new();
        for (i, (section, entries)) in sections.iter().enumerate() {
            if i > 0 {
                lines.push(Line::raw(""));
            }
            lines.push(Line::styled(
                format!(" {section}"),
                Style::default().fg(th.status_warn),
            ));
            for (key, desc) in *entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {key:>10}"), th.shortcut_key()),
                    Span::styled(format!("  {desc}"), Style::default().fg(th.text)),
                ]));
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(" Press ? or Esc to close", th.shortcut_desc()));

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

/// Convert a crossterm KeyEvent to the bytes the PTY expects.
fn key_event_to_bytes(key: &KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) if ctrl => {
            let byte = (c as u8).wrapping_sub(b'a').wrapping_add(1);
            if byte <= 26 { vec![byte] } else { vec![] }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        // MUST match `crate::session::manager::SUBMIT_SEQUENCE` (currently
        // `b"\r"`). The constant is `pub(crate)` and not re-exported from
        // `session::mod`, so the full path goes through `manager`.
        // If you change the byte sequence Enter produces, also update
        // `SUBMIT_SEQUENCE` in `src/session/manager.rs` — the
        // `submit_sequence_is_carriage_return` test will catch divergence
        // on the manager side, but the source of truth lives here.
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Find the longest common prefix of a list of strings
fn common_prefix(strings: &[String]) -> String {
    if strings.is_empty() {
        return String::new();
    }
    let first = &strings[0];
    let mut len = first.len();
    for s in &strings[1..] {
        len = len.min(s.len());
        for (i, (a, b)) in first.chars().zip(s.chars()).enumerate() {
            if a != b {
                len = len.min(i);
                break;
            }
        }
    }
    first[..len].to_string()
}
