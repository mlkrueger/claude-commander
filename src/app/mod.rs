mod keys;
mod render;

use crate::event::MonitoredSender;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::claude::launcher;
use crate::claude::rate_limit::{self, RateLimitInfo};
use crate::event::Event;
use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::pty::detector::PromptDetector;
use crate::session::{EventBus, SessionManager, SessionStatus, SpawnConfig};
use crate::setup::{self, SetupItem};
use crate::ui::panels::editor::EditorState;
use crate::ui::theme::{Theme, ThemeName};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Dashboard,
    SessionView(usize),
    SessionPicker(usize),
    Editor,
    RenamePrompt,
    NewSessionModal,
    SendFilePrompt,
    Setup,
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
    pub focused: usize,
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
    #[allow(dead_code)]
    pub(crate) event_bus: Arc<EventBus>,
    pub mode: AppMode,
    pub focus: PanelFocus,
    pub file_tree: FileTree,
    pub file_tree_scroll: usize,
    pub should_quit: bool,
    pub event_tx: MonitoredSender,
    pub detector: PromptDetector,
    pub input_buffer: String,
    pub working_dir: PathBuf,
    pub last_attention_check: Instant,
    pub terminal_cols: u16,
    pub terminal_rows: u16,
    pub session_view_scroll: usize,
    pub user_scrolled: bool,
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
    pub toggle_mouse_capture: bool,
    pub new_session: Option<NewSessionState>,
    pub theme: Theme,
    pub tick_count: u64,
    pub picker_selected: usize,
}

const ATTENTION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const USAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const PTY_COL_OVERHEAD: u16 = 34;
const PTY_ROW_OVERHEAD: u16 = 3;

impl App {
    pub fn new(event_tx: MonitoredSender, working_dir: PathBuf) -> Self {
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
                self.sessions.feed_pty_data(session_id, &data);
                if let AppMode::SessionView(id) = self.mode
                    && id == session_id
                    && !self.user_scrolled
                {
                    self.session_view_scroll = 0;
                }
            }
            Event::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
                if self.last_attention_check.elapsed() > ATTENTION_CHECK_INTERVAL {
                    self.check_all_attention();
                    self.last_attention_check = Instant::now();
                }
                if self.last_git_refresh.elapsed() > GIT_REFRESH_INTERVAL {
                    self.git_status = git::get_git_status(&self.file_tree.root.path);
                    self.last_git_refresh = Instant::now();
                    self.sessions.refresh_contexts();
                }
                if self.last_usage_refresh.elapsed() > USAGE_REFRESH_INTERVAL {
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
                if let AppMode::SessionView(id) = self.mode
                    && let Some(session) = self.sessions.get_mut(id)
                {
                    let inner_rows = rows.saturating_sub(PTY_ROW_OVERHEAD);
                    let inner_cols = cols.saturating_sub(2);
                    session.try_resize(inner_cols, inner_rows);
                }
            }
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

        let cols = self.terminal_cols.saturating_sub(PTY_COL_OVERHEAD).max(40);
        let rows = self.terminal_rows.saturating_sub(PTY_ROW_OVERHEAD).max(10);

        let install_hook = matches!(kind, SessionKind::Claude);
        let config = SpawnConfig {
            label,
            working_dir,
            command: cmd,
            args,
            event_tx: self.event_tx.clone(),
            cols,
            rows,
            install_hook,
        };

        match self.sessions.spawn(config) {
            Ok(_id) => {
                self.update_file_tree_for_selected();
            }
            Err(e) => {
                log::error!("Failed to spawn session: {e}");
                self.status_message = Some(format!("Failed to spawn session: {e}"));
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

        let prompt = missing.join("\n\n---\n\n");
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
        self.sessions.check_hook_signals();
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
        let visible_height = self.terminal_rows.saturating_sub(4) as usize;
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
}

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
