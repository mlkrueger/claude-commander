use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::claude::launcher;
use crate::event::Event;
use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::pty::detector::PromptDetector;
use crate::pty::session::{Session, SessionStatus};
use crate::ui::layout::AppLayout;
use crate::ui::panels::command_bar::{CommandBar, CommandBarMode};
use crate::ui::panels::editor::{EditorPanel, EditorState};
use crate::ui::panels::file_tree::FileTreePanel;
use crate::ui::panels::session_list::SessionListPanel;
use crate::ui::panels::session_view::SessionViewPanel;

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Dashboard,
    SessionView(usize),
    Editor,
    RenamePrompt,
    NewSessionPrompt,
    SendFilePrompt, // choose which session to send file path to
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelFocus {
    FileTree,
    SessionList,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub mode: AppMode,
    pub focus: PanelFocus,
    pub file_tree: FileTree,
    pub file_tree_scroll: usize,
    pub should_quit: bool,
    pub event_tx: mpsc::Sender<Event>,
    pub detector: PromptDetector,
    pub next_session_id: usize,
    pub input_buffer: String,
    pub working_dir: PathBuf,
    pub last_attention_check: Instant,
    pub terminal_cols: u16,
    pub terminal_rows: u16,
    pub status_message: Option<String>,
    pub editor: Option<EditorState>,
    pub git_status: Option<GitStatusMap>,
    pub last_git_refresh: Instant,
}

impl App {
    pub fn new(event_tx: mpsc::Sender<Event>, working_dir: PathBuf) -> Self {
        let file_tree = FileTree::new(working_dir.clone());
        let git_status = git::get_git_status(&working_dir);
        Self {
            sessions: Vec::new(),
            selected: 0,
            mode: AppMode::Dashboard,
            focus: PanelFocus::SessionList,
            file_tree,
            file_tree_scroll: 0,
            should_quit: false,
            event_tx,
            detector: PromptDetector::new(),
            next_session_id: 1,
            input_buffer: String::new(),
            working_dir,
            last_attention_check: Instant::now(),
            terminal_cols: 80,
            terminal_rows: 24,
            status_message: None,
            editor: None,
            git_status,
            last_git_refresh: Instant::now(),
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::PtyOutput { session_id, .. } => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    session.last_activity = Instant::now();
                }
            }
            Event::Tick => {
                if self.last_attention_check.elapsed() > Duration::from_secs(1) {
                    self.check_all_attention();
                    self.last_attention_check = Instant::now();
                }
                // Refresh git status every 5 seconds
                if self.last_git_refresh.elapsed() > Duration::from_secs(5) {
                    self.git_status = git::get_git_status(&self.file_tree.root.path);
                    self.last_git_refresh = Instant::now();
                }
                for session in &mut self.sessions {
                    if !matches!(session.status, SessionStatus::Exited(_)) {
                        if let Ok(Some(status)) = session.child.try_wait() {
                            session.status = SessionStatus::Exited(status.exit_code() as i32);
                        }
                    }
                }
            }
            Event::SessionExited { session_id, code } => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
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
                    if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                        let inner_rows = rows.saturating_sub(3);
                        let inner_cols = cols.saturating_sub(2);
                        let _ = session.resize(inner_cols, inner_rows);
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        match &self.mode {
            AppMode::Dashboard => self.handle_dashboard_key(key),
            AppMode::SessionView(id) => {
                let id = *id;
                self.handle_session_view_key(key, id);
            }
            AppMode::Editor => self.handle_editor_key(key),
            AppMode::RenamePrompt => self.handle_rename_key(key),
            AppMode::NewSessionPrompt => self.handle_new_session_key(key),
            AppMode::SendFilePrompt => self.handle_send_file_key(key),
        }
    }

    fn handle_dashboard_key(&mut self, key: KeyEvent) {
        // Tab switches panel focus
        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                PanelFocus::FileTree => PanelFocus::SessionList,
                PanelFocus::SessionList => PanelFocus::FileTree,
            };
            // Update file tree root to selected session's dir
            if self.focus == PanelFocus::FileTree {
                if let Some(session) = self.sessions.get(self.selected) {
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
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Down => {
                if !self.sessions.is_empty() {
                    self.selected = (self.selected + 1) % self.sessions.len();
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Up => {
                if !self.sessions.is_empty() {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(self.sessions.len() - 1);
                    self.update_file_tree_for_selected();
                }
            }
            KeyCode::Enter => {
                if let Some(session) = self.sessions.get(self.selected) {
                    let id = session.id;
                    if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                        let inner_rows = self.terminal_rows.saturating_sub(3);
                        let inner_cols = self.terminal_cols.saturating_sub(2);
                        let _ = session.resize(inner_cols, inner_rows);
                    }
                    self.mode = AppMode::SessionView(id);
                }
            }
            KeyCode::Char('n') => {
                self.input_buffer.clear();
                self.mode = AppMode::NewSessionPrompt;
            }
            KeyCode::Char('a') => self.approve_selected(),
            KeyCode::Char('d') => self.deny_selected(),
            KeyCode::Char('r') => {
                if !self.sessions.is_empty() {
                    self.input_buffer = self.sessions[self.selected].label.clone();
                    self.mode = AppMode::RenamePrompt;
                }
            }
            KeyCode::Char('K') => self.kill_selected(),
            KeyCode::Char('c') => self.send_commit_prompt(),
            KeyCode::Char('x') => self.clear_dead_sessions(),
            _ => {}
        }
    }

    fn handle_file_tree_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
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
                // Spawn new session at selected directory
                if let Some(path) = self.file_tree.selected_path() {
                    let dir = if path.is_dir() {
                        path.to_path_buf()
                    } else {
                        path.parent().unwrap_or(path).to_path_buf()
                    };
                    self.spawn_session(dir);
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
            // Ctrl+O: back to dashboard (same as session view)
            KeyCode::Char('o') if ctrl => {
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
            if let Some(session) = self.sessions.get_mut(idx) {
                let msg = format!("Read the file at {}\n", path.display());
                let _ = session.write(msg.as_bytes());
                if let Some(editor) = &mut self.editor {
                    editor.message = Some(format!("Sent to session {}", session.label));
                }
            } else if let Some(editor) = &mut self.editor {
                editor.message = Some(format!("No session at index {idx}"));
            }
        }
    }

    fn handle_session_view_key(&mut self, key: KeyEvent, session_id: usize) {
        // Ctrl+O returns to dashboard (Esc is forwarded to session)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.mode = AppMode::Dashboard;
            return;
        }

        let bytes = key_event_to_bytes(&key);
        if !bytes.is_empty() {
            if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                let _ = session.write(&bytes);
            }
        }
    }

    fn handle_rename_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                if !self.input_buffer.is_empty() && !self.sessions.is_empty() {
                    self.sessions[self.selected].label = self.input_buffer.clone();
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

    fn handle_new_session_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                let dir = if self.input_buffer.is_empty() {
                    self.working_dir.clone()
                } else {
                    PathBuf::from(shellexpand::tilde(&self.input_buffer).to_string())
                };
                if dir.is_dir() {
                    self.spawn_session(dir);
                    self.input_buffer.clear();
                    self.status_message = None;
                    self.mode = AppMode::Dashboard;
                } else {
                    self.status_message = Some(format!("Directory not found: {}", dir.display()));
                }
            }
            KeyCode::Esc => {
                self.input_buffer.clear();
                self.status_message = None;
                self.mode = AppMode::Dashboard;
            }
            KeyCode::Tab => {
                self.tab_complete_path();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
                self.status_message = None;
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
                self.status_message = None;
            }
            _ => {}
        }
    }

    fn tab_complete_path(&mut self) {
        let expanded = shellexpand::tilde(&self.input_buffer).to_string();
        let path = PathBuf::from(&expanded);

        // Split into parent dir and partial name
        let (search_dir, prefix) = if path.is_dir() && self.input_buffer.ends_with('/') {
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

        if matches.len() == 1 {
            // Single match: complete it
            let completed = search_dir.join(&matches[0]);
            // Convert back to tilde form if possible
            let home = dirs::home_dir().unwrap_or_default();
            let display = if completed.starts_with(&home) {
                format!("~{}/", completed.strip_prefix(&home).unwrap().display())
            } else {
                format!("{}/", completed.display())
            };
            self.input_buffer = display;
            self.status_message = None;
        } else {
            // Multiple matches: complete common prefix and show options
            let common = common_prefix(&matches);
            if common.len() > prefix.len() {
                let completed = search_dir.join(&common);
                let home = dirs::home_dir().unwrap_or_default();
                let display = if completed.starts_with(&home) {
                    format!("~{}", completed.strip_prefix(&home).unwrap().display())
                } else {
                    format!("{}", completed.display())
                };
                self.input_buffer = display;
            }
            // Show matches in status
            let display_matches: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
            self.status_message = Some(display_matches.join("  "));
        }
    }

    pub fn spawn_session(&mut self, working_dir: PathBuf) {
        let id = self.next_session_id;
        self.next_session_id += 1;
        let label = format!("claude-{id}");

        let cmd = launcher::claude_command();
        let args = launcher::claude_args();

        let cols = self.terminal_cols.saturating_sub(34).max(40); // account for file tree
        let rows = self.terminal_rows.saturating_sub(3).max(10);

        match Session::spawn(
            id,
            label,
            working_dir,
            cmd,
            &args,
            self.event_tx.clone(),
            cols,
            rows,
        ) {
            Ok(session) => {
                self.sessions.push(session);
                self.selected = self.sessions.len() - 1;
                self.update_file_tree_for_selected();
            }
            Err(e) => {
                log::error!("Failed to spawn session: {e}");
            }
        }
    }

    fn approve_selected(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            let _ = session.write(b"\r");
        }
    }

    fn deny_selected(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            let _ = session.write(b"\x1b[B\x1b[B\r");
        }
    }

    fn kill_selected(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            session.kill();
        }
    }

    fn send_commit_prompt(&mut self) {
        if let Some(session) = self.sessions.get_mut(self.selected) {
            let _ = session.write(b"/commit\n");
            self.status_message = Some(format!("Sent /commit to {}", session.label));
        }
    }

    fn clear_dead_sessions(&mut self) {
        self.sessions
            .retain(|s| !matches!(s.status, SessionStatus::Exited(_)));
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    fn check_all_attention(&mut self) {
        for session in &mut self.sessions {
            if !matches!(session.status, SessionStatus::Exited(_)) {
                session.check_attention(&self.detector);
            }
        }
    }

    fn update_file_tree_for_selected(&mut self) {
        if let Some(session) = self.sessions.get(self.selected) {
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

    pub fn draw(&self, frame: &mut Frame) {
        match &self.mode {
            AppMode::Editor | AppMode::SendFilePrompt => {
                let (main_area, cmd_area) = AppLayout::session_view(frame.area());

                if let Some(editor) = &self.editor {
                    let panel = EditorPanel::new(editor);
                    frame.render_widget(panel, main_area);

                    // Show editor message if any, otherwise command bar
                    if let Some(msg) = &editor.message {
                        let line = ratatui::text::Line::styled(
                            msg.clone(),
                            ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
                        );
                        frame.render_widget(line, cmd_area);
                    } else if self.mode == AppMode::SendFilePrompt {
                        let labels: Vec<String> =
                            self.sessions.iter().map(|s| s.label.clone()).collect();
                        let bar = CommandBar::new(CommandBarMode::SendFile(labels));
                        frame.render_widget(bar, cmd_area);
                    } else {
                        let bar = CommandBar::new(CommandBarMode::Editor);
                        frame.render_widget(bar, cmd_area);
                    }
                }
            }
            AppMode::Dashboard | AppMode::RenamePrompt | AppMode::NewSessionPrompt => {
                let layout = AppLayout::new(frame.area());

                // File tree (left panel)
                let session_dirs = self.session_dirs();
                let tree_panel = FileTreePanel::new(
                    &self.file_tree,
                    self.focus == PanelFocus::FileTree,
                    &session_dirs,
                )
                .with_scroll(self.file_tree_scroll)
                .with_git_status(self.git_status.as_ref());
                frame.render_widget(tree_panel, layout.file_tree);

                // Session list (main panel)
                let session_list = SessionListPanel::new(
                    &self.sessions,
                    self.selected,
                    self.focus == PanelFocus::SessionList,
                );
                frame.render_widget(session_list, layout.main);

                // Command bar
                match &self.mode {
                    AppMode::RenamePrompt => {
                        let prompt = format!("Rename: {}_", self.input_buffer);
                        let line = ratatui::text::Line::raw(prompt);
                        frame.render_widget(line, layout.command_bar);
                    }
                    AppMode::NewSessionPrompt => {
                        let prompt = if let Some(msg) = &self.status_message {
                            ratatui::text::Line::styled(
                                format!("Dir: {}_ | {msg}", self.input_buffer),
                                ratatui::style::Style::default().fg(ratatui::style::Color::Yellow),
                            )
                        } else {
                            let display_dir = if self.input_buffer.is_empty() {
                                format!("{} (default, Tab=complete)", self.working_dir.display())
                            } else {
                                format!("{}_ (Tab=complete)", self.input_buffer)
                            };
                            ratatui::text::Line::raw(format!("New session dir: {display_dir}"))
                        };
                        frame.render_widget(prompt, layout.command_bar);
                    }
                    _ => {
                        let bar_mode = match self.focus {
                            PanelFocus::SessionList => CommandBarMode::Dashboard,
                            PanelFocus::FileTree => CommandBarMode::FileTree,
                        };
                        let command_bar = CommandBar::new(bar_mode);
                        frame.render_widget(command_bar, layout.command_bar);
                    }
                }
            }
            AppMode::SessionView(id) => {
                let (main_area, cmd_area) = AppLayout::session_view(frame.area());

                if let Some(session) = self.sessions.iter().find(|s| s.id == *id) {
                    let view = SessionViewPanel::new(session);
                    frame.render_widget(view, main_area);
                }

                let command_bar = CommandBar::new(CommandBarMode::SessionView);
                frame.render_widget(command_bar, cmd_area);
            }
        }
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
