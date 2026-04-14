mod attach;
mod keys;
mod render;

use crate::event::MonitoredSender;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::claude::launcher;
use crate::claude::rate_limit::{self, RateLimitInfo};
use crate::event::Event;
use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::pty::detector::PromptDetector;
use crate::session::{EventBus, SessionManager, SessionRole, SessionStatus, SpawnConfig};
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
    /// Phase 5: a write-tool MCP handler is blocked on user
    /// confirmation. The pending request lives in
    /// `App::pending_confirm`.
    McpConfirm,
    /// Phase 6 Task 5: sub-picker overlay that lists live drivers so
    /// the user can attach a target session to one of them. Entered
    /// from `SessionPicker` via `a`; `target_session_id` is the
    /// session that will be added to the chosen driver's
    /// `attachment_map` entry.
    ///
    /// `drivers` is snapshotted at mode entry time
    /// (`open_attach_driver_picker`) so the key handler and render
    /// function don't each acquire `sessions_lock()` per interaction
    /// — a driver that exits while the picker is open still renders
    /// in the list until the user dismisses the overlay, but the
    /// `attach_session_to_driver` call will re-check liveness before
    /// committing. See pr-review-phase-6-tasks-3-to-7.md §D2.
    ///
    /// `restore_picker_selected` is the `App.picker_selected` value
    /// the user had in the originating `SessionPicker` before
    /// pressing `a`. Both the Esc and Enter arms of the key handler
    /// write it back before returning to `SessionPicker` so the
    /// highlight lands on the originally-selected row, not on 0.
    AttachDriverPicker {
        target_session_id: usize,
        drivers: Vec<(usize, String)>,
        restore_picker_selected: usize,
    },
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
    pub(crate) sessions: Arc<Mutex<SessionManager>>,
    #[allow(dead_code)]
    pub(crate) event_bus: Arc<EventBus>,
    /// Embedded MCP server running on loopback. `None` if startup
    /// failed — the TUI still works without it, just without the
    /// read-only tools. `take()`n in `main.rs` on shutdown so
    /// `McpServer::stop` can consume it.
    pub mcp: Option<crate::mcp::McpServer>,
    /// Receiver side of the `ConfirmBridge`. The MCP server sends
    /// `ConfirmRequest`s on this channel; we drain them on every
    /// `Event::Tick` and push into `pending_confirm` to switch the
    /// UI into `AppMode::McpConfirm`.
    pub(crate) confirm_rx: std::sync::mpsc::Receiver<crate::mcp::ConfirmRequest>,
    /// The currently-displayed confirmation request, if any. Holds
    /// the `oneshot::Sender` that will be resolved when the user
    /// answers `y`/`n`/`Esc`.
    pub(crate) pending_confirm: Option<crate::mcp::ConfirmRequest>,
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
    /// Phase 6 Task 2: pending driver role to apply to the very
    /// next Claude session spawned through `spawn_session_kind`.
    /// Populated by `main` from the resolved `DriverConfig` when
    /// `--driver` is passed. `take()`n on first use so subsequent
    /// spawns stay `Solo`.
    pub pending_driver_role: Option<SessionRole>,
    /// Phase 6 prelude: explicit driver-to-attached-session mapping.
    /// Keyed by driver session id → set of session ids the user
    /// has manually attached to that driver via the TUI's attach-to
    /// -driver action (Task 5). Separate from parent/child spawning:
    /// attachments are user-initiated and visible to the driver's
    /// MCP scope filter in addition to its own `spawned_by` children.
    ///
    /// Stored as `Arc<Mutex<..>>` rather than plain `HashMap` because
    /// the MCP server thread needs read access for `McpCtx::caller_scope`,
    /// and the TUI thread needs write access for attach/detach +
    /// on-driver-exit cleanup. The shared-pointer contract is
    /// load-bearing: App owns the only write path, the MCP thread
    /// observes snapshots. Same memory — no divergence possible.
    ///
    /// `#[allow(dead_code)]` until Task 5's attach-to-driver key
    /// handler lands — at that point the field becomes the TUI's
    /// write path. The `Arc` clone in `App::new` hands it to
    /// `McpCtx::attachments`, which already exists but is also
    /// unread until Task 4's `caller_scope` body replaces the stub.
    #[allow(dead_code)]
    pub(crate) attachment_map: Arc<Mutex<HashMap<usize, HashSet<usize>>>>,
}

const ATTENTION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const USAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const PTY_COL_OVERHEAD: u16 = 34;
const PTY_ROW_OVERHEAD: u16 = 3;

impl App {
    /// Lock the shared `SessionManager`, recovering transparently if a
    /// previous holder panicked (matches the `Arc<Mutex<EventBus>>`
    /// poison-recovery pattern in `src/session/events.rs`). App is
    /// single-threaded, so lock contention is impossible in practice —
    /// the mutex exists solely so the MCP thread can snapshot state.
    pub(crate) fn sessions_lock(&self) -> MutexGuard<'_, SessionManager> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

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
        let sessions = Arc::new(Mutex::new(SessionManager::with_bus(Arc::clone(&event_bus))));

        // Phase 6 prelude: shared driver-attachment map. App is the
        // only writer (attach/detach + on-driver-exit cleanup); the
        // `ccom-mcp` thread reads through `McpCtx::caller_scope` when
        // resolving a driver caller's visible session set. Same
        // `Arc<Mutex<_>>` on both sides, so there's no divergence.
        let attachment_map: Arc<Mutex<HashMap<usize, HashSet<usize>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Phase 5: cross-thread confirmation bridge for write tools.
        // The bridge's receiver is drained each tick by `handle_event`;
        // the sender side lives on `McpCtx` so tool handlers on the
        // `ccom-mcp` thread can request user confirmation.
        let (confirm_bridge, confirm_rx) = crate::mcp::ConfirmBridge::new();

        // Start the embedded MCP server. Failure is non-fatal — the
        // TUI remains functional, just without the read-only tools.
        let mcp = {
            let ctx = Arc::new(crate::mcp::McpCtx {
                sessions: Arc::clone(&sessions),
                bus: Arc::clone(&event_bus),
                confirm: Some(Arc::clone(&confirm_bridge)),
                attachments: Arc::clone(&attachment_map),
                // Phase 6 Task 3: hand the event sender to the MCP
                // ctx so `spawn_session` can spawn new sessions
                // whose PTY output reaches the main TUI event loop.
                event_tx: Some(event_tx.clone()),
            });
            match crate::mcp::McpServer::start(ctx) {
                Ok(server) => {
                    log::info!("ccom-mcp server listening on 127.0.0.1:{}", server.port());
                    Some(server)
                }
                Err(e) => {
                    log::error!("ccom-mcp server failed to start: {e}");
                    None
                }
            }
        };

        Self {
            sessions,
            event_bus,
            mcp,
            confirm_rx,
            pending_confirm: None,
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
            pending_driver_role: None,
            attachment_map,
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::PtyOutput { session_id, data } => {
                {
                    let mut mgr = self.sessions_lock();
                    if let Some(session) = mgr.get_mut(session_id) {
                        session.last_activity = Instant::now();
                    }
                    mgr.feed_pty_data(session_id, &data);
                }
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
                    self.sessions_lock().refresh_contexts();
                }
                if self.last_usage_refresh.elapsed() > USAGE_REFRESH_INTERVAL {
                    self.rate_limit = rate_limit::get_rate_limit_info()
                        .or_else(rate_limit::get_rate_limit_from_telemetry);
                    self.last_usage_refresh = Instant::now();
                }
                self.sessions_lock().reap_exited();
                // Phase 5: drain any pending MCP confirmation requests.
                // If one arrives while another is already pending we
                // immediately Deny the new one — the modal is strictly
                // one-at-a-time.
                while let Ok(req) = self.confirm_rx.try_recv() {
                    if self.pending_confirm.is_none() {
                        self.pending_confirm = Some(req);
                        self.mode = AppMode::McpConfirm;
                    } else {
                        let _ = req.resp_tx.send(crate::mcp::ConfirmResponse::Deny);
                    }
                }
            }
            Event::SessionExited { session_id, code } => {
                // Phase 6 Task 5: if the exited session was a driver,
                // drop its attachment set so stale entries don't
                // linger. Reads the role before mutating status so
                // the check still sees `Driver` regardless of the
                // exit order. Also scrubs the id from any OTHER
                // driver's attachment set — an attached session
                // going away should leave the attacher's list clean.
                let was_driver = self
                    .sessions_lock()
                    .get(session_id)
                    .map(|s| matches!(s.role, SessionRole::Driver { .. }))
                    .unwrap_or(false);
                self.scrub_attachments_for_exited(session_id, was_driver);
                if let Some(session) = self.sessions_lock().get_mut(session_id) {
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
                    let mut mgr = self.sessions_lock();
                    if let Some(session) = mgr.get_mut(id) {
                        let inner_rows = rows.saturating_sub(PTY_ROW_OVERHEAD);
                        let inner_cols = cols.saturating_sub(2);
                        session.try_resize(inner_cols, inner_rows);
                    }
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
        let next_id = self.sessions_lock().peek_next_id();
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
        // Only Claude sessions get a `.mcp.json` — Terminal sessions
        // don't know what to do with MCP. Mirrors the `install_hook`
        // gating.
        let mcp_port = if install_hook {
            self.mcp.as_ref().map(|s| s.port())
        } else {
            None
        };
        let config = SpawnConfig {
            label,
            working_dir,
            command: cmd,
            args,
            event_tx: self.event_tx.clone(),
            cols,
            rows,
            install_hook,
            mcp_port,
        };

        // Phase 6 prelude (pr-review-pr18 §Issue 1): route through
        // `SessionManager::spawn_with_role`, which performs the
        // session creation + role promotion as a single atomic
        // operation under one sessions-mutex acquisition. Fixes the
        // TOCTOU window where an MCP-thread observer could otherwise
        // snapshot the new session with `role = Solo` between a
        // `spawn()` and a subsequent `set_role()`.
        //
        // `pending_driver_role` is `take()`n BEFORE acquiring the
        // lock because `self.sessions_lock()` borrows `&self` for
        // the guard's lifetime. On spawn failure the role is put
        // back so a subsequent retry can still consume it.
        let pending_role = if matches!(kind, SessionKind::Claude) {
            self.pending_driver_role.take()
        } else {
            None
        };
        if let Some(role) = pending_role.as_ref() {
            log::info!("promoting next Claude spawn to driver role: {role:?}");
        }
        let spawn_res = self
            .sessions_lock()
            .spawn_with_role(config, pending_role.clone(), None);
        if spawn_res.is_err()
            && let Some(role) = pending_role
        {
            self.pending_driver_role = Some(role);
        }
        match spawn_res {
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
        if let Some(session) = self.sessions_lock().selected_mut() {
            session.try_write(b"\r");
        }
    }

    fn deny_selected(&mut self) {
        if let Some(session) = self.sessions_lock().selected_mut() {
            session.try_write(b"\x1b[B\x1b[B\r");
        }
    }

    // --- Phase 6 Task 5: driver attachment helpers ---

    /// Returns the set of alive drivers in manager order as
    /// `(id, label)` tuples. Used by the attach sub-picker for both
    /// the overlay listing and the Enter-commits path.
    pub(crate) fn live_drivers(&self) -> Vec<(usize, String)> {
        self.sessions_lock()
            .iter()
            .filter(|s| {
                matches!(s.role, SessionRole::Driver { .. })
                    && !matches!(s.status, SessionStatus::Exited(_))
            })
            .map(|s| (s.id, s.label.clone()))
            .collect()
    }

    /// Switch to the attach-driver sub-picker if any live drivers
    /// exist. If not, surface a status message and stay put. The
    /// driver list is snapshotted here and cached on the mode
    /// variant so subsequent key events and render frames don't
    /// re-lock `sessions`. See pr-review-phase-6-tasks-3-to-7.md §D2.
    ///
    /// Captures `self.picker_selected` before resetting it so the
    /// Esc/Enter return path can restore the originating session
    /// picker's highlight (pr-review-phase-6-tasks-3-to-7.md §B —
    /// finding 2 on PR #22).
    pub(crate) fn open_attach_driver_picker(&mut self, target_session_id: usize) {
        let drivers = self.live_drivers();
        if drivers.is_empty() {
            self.status_message = Some("No active drivers — launch ccom with --driver".to_string());
            return;
        }
        let restore_picker_selected = self.picker_selected;
        self.picker_selected = 0;
        self.mode = AppMode::AttachDriverPicker {
            target_session_id,
            drivers,
            restore_picker_selected,
        };
    }

    /// Add `target` to `driver`'s attachment set. Idempotent. No-op
    /// with a logged warning if `driver` doesn't point at a live
    /// driver session.
    pub(crate) fn attach_session_to_driver(&mut self, driver_id: usize, target: usize) {
        let is_live_driver = self
            .sessions_lock()
            .get(driver_id)
            .map(|s| {
                matches!(s.role, SessionRole::Driver { .. })
                    && !matches!(s.status, SessionStatus::Exited(_))
            })
            .unwrap_or(false);
        if !is_live_driver {
            log::warn!("attach_session_to_driver: {driver_id} is not a live driver");
            return;
        }
        let mut map = self
            .attachment_map
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        attach::attach(&mut map, driver_id, target);
    }

    /// Invoked from the `SessionExited` handler. If the exited session
    /// was a driver, drop its entry entirely; either way, scrub its
    /// id from any OTHER driver's attachment set so references don't
    /// linger after reaping.
    pub(crate) fn scrub_attachments_for_exited(&mut self, id: usize, was_driver: bool) {
        let mut map = self
            .attachment_map
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        attach::scrub_on_exit(&mut map, id, was_driver);
    }

    fn kill_selected(&mut self) {
        let mut mgr = self.sessions_lock();
        let id = mgr.selected().map(|s| s.id);
        if let Some(id) = id {
            mgr.kill(id);
        }
    }

    fn send_commit_prompt(&mut self) {
        let label = {
            let mut mgr = self.sessions_lock();
            match mgr.selected_mut() {
                Some(session) => {
                    session.try_write(b"/commit\n");
                    Some(session.label.clone())
                }
                None => None,
            }
        };
        if let Some(label) = label {
            self.status_message = Some(format!("Sent /commit to {label}"));
        }
    }

    fn clear_dead_sessions(&mut self) {
        self.sessions_lock().retain_alive();
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
            // Returns Some(label) if the write went through, None if the
            // idx didn't resolve to a live session.
            let sent_label: Option<String> = {
                let mut mgr = self.sessions_lock();
                let session_id = mgr.iter().nth(idx).map(|s| s.id);
                session_id.and_then(|id| {
                    mgr.get_mut(id).map(|session| {
                        let msg = format!("Read the file at {}\n", path.display());
                        session.try_write(msg.as_bytes());
                        session.label.clone()
                    })
                })
            };
            if let Some(label) = sent_label {
                if let Some(editor) = &mut self.editor {
                    editor.message = Some(format!("Sent to session {label}"));
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

        let session_id = self.sessions_lock().selected().map(|s| s.id);

        if let Some(id) = session_id {
            setup::mark_initialized();
            self.mode = AppMode::SessionView(id);
        } else {
            self.status_message = Some("Failed to spawn setup session".to_string());
            self.mode = AppMode::Dashboard;
        }
    }

    fn check_all_attention(&mut self) {
        let mut mgr = self.sessions_lock();
        mgr.check_attention(&self.detector);
        mgr.check_hook_signals();
        mgr.check_response_boundaries();
    }

    fn update_file_tree_for_selected(&mut self) {
        let dir = self
            .sessions_lock()
            .selected()
            .map(|session| session.working_dir.clone());
        if let Some(dir) = dir
            && dir != self.file_tree.root.path
        {
            self.file_tree.set_root(dir.clone());
            self.file_tree_scroll = 0;
            self.git_status = git::get_git_status(&dir);
            self.last_git_refresh = Instant::now();
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
        self.sessions_lock()
            .iter()
            .filter(|s| !matches!(s.status, SessionStatus::Exited(_)))
            .map(|s| s.working_dir.clone())
            .collect()
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
