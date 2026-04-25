mod attach;
mod keys;
mod render;

use crate::event::MonitoredSender;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::claude::launcher;
use crate::claude::rate_limit::{self, RateLimitInfo};
use crate::event::Event;
use crate::fs::git::{self, GitStatusMap};
use crate::fs::tree::FileTree;
use crate::pty::detector::PromptDetector;
use crate::session::{
    EventBus, SessionEvent, SessionManager, SessionRole, SessionStatus, SpawnConfig,
};
use crate::setup::{self, SetupItem};
use crate::ui::theme::{Theme, ThemeName};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Dashboard,
    SessionView(usize),
    SessionPicker(usize),
    RenamePrompt,
    NewSessionModal,
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
    /// When `Some`, a file-picker overlay is open on top of the modal.
    /// The tree navigates directories starting at `$HOME`; selecting
    /// a directory writes its path into `dir_input` and closes the
    /// picker, returning focus to the modal.
    pub picker: Option<FileTree>,
    /// When opened from a session view, holds the originating session id
    /// so that after spawn we stay in SessionView rather than going to Dashboard.
    pub return_to_session: Option<usize>,
}

impl NewSessionState {
    fn new() -> Self {
        Self {
            kind: SessionKind::Claude,
            dir_input: String::new(),
            flags_input: String::new(),
            focused: 1,
            status_message: None,
            picker: None,
            return_to_session: None,
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
    pub input_paused: Arc<AtomicBool>,
    pub needs_full_redraw: bool,
    /// Scrollback length the last time we adjusted session_view_scroll.
    /// Used to compensate for new rows pushed into scrollback while
    /// user_scrolled is true, so the view stays at the same visual position.
    pub scroll_lock_sb_len: usize,
    pub git_status: Option<GitStatusMap>,
    pub last_git_refresh: Instant,
    pub last_usage_refresh: Instant,
    /// Phase 7 Task 9: timestamp of the last approval-registry reaper sweep.
    pub last_reaper_sweep: Instant,
    pub rate_limit: Option<RateLimitInfo>,
    pub setup_items: Vec<SetupItem>,
    pub setup_selected: usize,
    pub setup_banner_dismissed: bool,
    pub mouse_captured: bool,
    pub toggle_mouse_capture: bool,
    /// Set by the Resize handler so main.rs can re-assert EnableMouseCapture,
    /// which some terminals silently drop after a resize event.
    pub reapply_mouse_capture: bool,
    /// Tick count when the current status_message was set (for auto-expiry).
    pub status_message_tick: u64,
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
    /// Phase 7 Task 5: approval registry owned at App-level and shared
    /// with `McpCtx`. Allows the TUI thread to wire per-session
    /// approval coordinators after spawning, without going through MCP.
    pub(crate) approvals: Arc<crate::approvals::ApprovalRegistry>,
    /// Phase 7 Task 8: per-driver count of tool-approval requests that
    /// are currently pending (requested but not yet resolved). Keyed by
    /// driver session id. The TUI uses this to render the `▲ <n>` hint
    /// in the status line and the `▲` marker in the session list.
    pub(crate) pending_approvals_per_driver: HashMap<usize, u32>,
    /// Maps child `session_id` → `(request_id, driver_id)` for native
    /// Claude Code dialogs (YesNo / AllowOnce) that have been promoted to
    /// synthetic registry entries so the driver can resolve them via MCP.
    /// Entries are inserted when `WaitingForApproval` is detected and
    /// removed when the status leaves that state or the driver resolves it.
    pub(crate) pending_pty_approvals: HashMap<usize, (u64, usize)>,
    /// Phase 7 Task 8: subscriber for `ToolApprovalRequested` /
    /// `ToolApprovalResolved` events published by the approval
    /// coordinator. Drained each tick in `handle_event`.
    pub(crate) approval_event_rx: std::sync::mpsc::Receiver<SessionEvent>,

    // --- Update machinery ---
    /// Background channel that delivers the result of the GitHub release check.
    /// `None` once the result has been consumed.
    pub(crate) update_rx: Option<std::sync::mpsc::Receiver<Option<String>>>,
    /// Latest release tag (e.g. `"v0.4.0"`) if newer than the running version.
    pub update_available: Option<String>,
    /// True while a download-and-replace is running in the background.
    pub update_installing: bool,
    /// Channel for the install background thread's result. `None` when idle.
    pub(crate) update_install_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    /// True after a successful install — banner switches to "restart to apply".
    pub update_installed: bool,
    /// Cached at startup — true when running from a Homebrew Cellar path.
    pub homebrew_install: bool,
}

const ATTENTION_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const USAGE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
/// How often the approval-registry reaper sweeps for stale entries.
const APPROVAL_REAPER_INTERVAL: Duration = Duration::from_secs(60);
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

        // Phase 7 Task 5: create the registry at App-level so both the
        // MCP handlers (via McpCtx) and the TUI-thread coordinator-startup
        // path can share the same Arc.
        let approvals = crate::approvals::ApprovalRegistry::new();

        // Phase 7 Task 8: subscribe to the event bus for approval events
        // so the TUI can maintain per-driver pending-approval counts.
        // Must subscribe BEFORE MCP starts so we don't miss any early
        // events published during startup.
        let approval_event_rx = event_bus.subscribe();

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
                // Phase 7 Tasks 1+2+3+5: approval registry shared between
                // the socket listener tasks and the MCP handlers.
                approvals: Some(Arc::clone(&approvals)),
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

        let update_rx = Some(crate::update::spawn_update_check());

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
            input_paused: Arc::new(AtomicBool::new(false)),
            needs_full_redraw: false,
            scroll_lock_sb_len: 0,
            git_status,
            last_git_refresh: Instant::now(),
            last_usage_refresh: Instant::now() - Duration::from_secs(60),
            last_reaper_sweep: Instant::now(),
            rate_limit,
            setup_items,
            setup_selected: 0,
            setup_banner_dismissed: false,
            mouse_captured: true,
            toggle_mouse_capture: false,
            reapply_mouse_capture: false,
            status_message_tick: 0,
            new_session: None,
            theme: Theme::new(ThemeName::Default),
            tick_count: 0,
            picker_selected: 0,
            pending_driver_role: None,
            attachment_map,
            approvals,
            pending_approvals_per_driver: HashMap::new(),
            pending_pty_approvals: HashMap::new(),
            approval_event_rx,
            update_rx,
            update_available: None,
            update_installing: false,
            update_install_rx: None,
            update_installed: false,
            homebrew_install: crate::update::is_homebrew_install(),
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
                {
                    if self.user_scrolled {
                        // The scrollback offset is "rows from current bottom".
                        // As new rows arrive the bottom advances, so we must
                        // increase the offset by the same amount to keep the
                        // same visual content on screen.
                        let current_sb = {
                            let mgr = self.sessions_lock();
                            mgr.get(session_id).map_or(0, |session| {
                                let mut parser = crate::session::lock_parser(&session.parser);
                                parser.screen_mut().set_scrollback(usize::MAX);
                                let len = parser.screen().scrollback();
                                parser.screen_mut().set_scrollback(self.session_view_scroll);
                                len
                            })
                        };
                        let added = current_sb.saturating_sub(self.scroll_lock_sb_len);
                        self.session_view_scroll = self
                            .session_view_scroll
                            .saturating_add(added)
                            .min(current_sb);
                        self.scroll_lock_sb_len = current_sb;
                    } else {
                        self.session_view_scroll = 0;
                    }
                }
            }
            Event::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
                // Auto-expire status messages after ~3 seconds (15 ticks × 200ms).
                if self.status_message.is_some()
                    && self.tick_count.wrapping_sub(self.status_message_tick) > 15
                {
                    self.status_message = None;
                }
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
                // Phase 7 Task 8: drain approval bus events to keep the
                // per-driver pending-count map up to date.
                self.drain_approval_events();
                // Phase 7 Task 9: reap stale approval entries once per minute.
                if self.last_reaper_sweep.elapsed() > APPROVAL_REAPER_INTERVAL {
                    self.approvals.sweep_stale();
                    self.last_reaper_sweep = Instant::now();
                }
                // Drain update-check result (fires once after startup).
                {
                    let result = self.update_rx.as_ref().and_then(|rx| rx.try_recv().ok());
                    if let Some(version) = result {
                        self.update_available = version;
                        self.update_rx = None;
                    }
                }
                // Drain update-install result.
                {
                    let result = self
                        .update_install_rx
                        .as_ref()
                        .and_then(|rx| rx.try_recv().ok());
                    if let Some(outcome) = result {
                        self.update_installing = false;
                        self.update_install_rx = None;
                        match outcome {
                            Ok(()) => {
                                self.update_installed = true;
                            }
                            Err(e) => {
                                self.status_message = Some(format!("Update failed: {e}"));
                                self.status_message_tick = self.tick_count;
                            }
                        }
                    }
                }
            }
            Event::SessionExited { session_id, code } => {
                // Drain any queued approval events before removing the driver
                // entry so we don't re-insert a ghost entry on the next Tick.
                self.drain_approval_events();
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
                // Phase 7 Task 9: when a driver exits, deny all pending
                // approvals for its children so they are not left hanging.
                // Phase 7 Task 8: also drop the stale pending-approval count
                // for the exited driver.
                if was_driver {
                    self.approvals.deny_all_for_driver(session_id);
                    self.pending_approvals_per_driver.remove(&session_id);
                }
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
                // Re-assert mouse capture after resize — some terminals
                // (notably macOS Terminal and older iTerm2) silently drop
                // mouse tracking mode when the window is resized.
                self.reapply_mouse_capture = true;
            }
        }
    }

    /// Drain all queued [`SessionEvent`]s from `approval_event_rx` and update
    /// `pending_approvals_per_driver` accordingly.  Called both on every
    /// `Event::Tick` and at the *start* of `Event::SessionExited` (before the
    /// map entry is removed) so that a `ToolApprovalRequested` event that
    /// arrived between the last tick and the exit cannot re-insert a ghost
    /// entry on the following tick.
    fn drain_approval_events(&mut self) {
        while let Ok(ev) = self.approval_event_rx.try_recv() {
            match ev {
                SessionEvent::ToolApprovalRequested { driver_id, .. } => {
                    *self
                        .pending_approvals_per_driver
                        .entry(driver_id)
                        .or_insert(0) += 1;
                }
                SessionEvent::ToolApprovalResolved { driver_id, .. } => {
                    let cnt = self
                        .pending_approvals_per_driver
                        .entry(driver_id)
                        .or_insert(0);
                    *cnt = cnt.saturating_sub(1);
                }
                SessionEvent::StatusChanged {
                    session_id,
                    ref status,
                } => {
                    use crate::session::SessionStatus;
                    match status {
                        SessionStatus::WaitingForApproval(kind) => {
                            // Only promote to a synthetic approval if this
                            // session is driver-owned AND not already tracked.
                            if self.pending_pty_approvals.contains_key(&session_id) {
                                // Already registered — nothing to do.
                                continue;
                            }
                            let (driver_id, cwd) = {
                                let mgr = self.sessions_lock();
                                let session = match mgr.get(session_id) {
                                    Some(s) => s,
                                    None => continue,
                                };
                                let driver_id = match session.spawned_by {
                                    Some(did) => did,
                                    None => continue, // not a child session
                                };
                                (driver_id, session.working_dir.clone())
                            };
                            let request_id = self.approvals.open_pty_dialog_request(
                                session_id,
                                driver_id,
                                kind.clone(),
                                cwd,
                            );
                            self.pending_pty_approvals
                                .insert(session_id, (request_id, driver_id));
                            self.event_bus.publish(SessionEvent::ToolApprovalRequested {
                                request_id,
                                session_id,
                                driver_id,
                                tool: format!("NativeDialog({kind})"),
                                args: serde_json::json!({ "kind": kind }),
                                cwd: std::path::PathBuf::new(),
                                timestamp: std::time::SystemTime::now(),
                            });
                        }
                        _ => {
                            // Status left WaitingForApproval (or changed to a
                            // different kind). Cancel any tracked PTY approval
                            // so stale entries don't linger in the registry.
                            if let Some((request_id, driver_id)) =
                                self.pending_pty_approvals.remove(&session_id)
                                && self.approvals.cancel_if_pending(request_id)
                            {
                                // Entry was still open — the user resolved
                                // the dialog manually in the TUI. Publish
                                // Resolved so the driver badge updates.
                                // (If cancel_if_pending returns false, the driver
                                // already called respond_to_tool_approval — Resolved
                                // was already published there.)
                                self.event_bus.publish(SessionEvent::ToolApprovalResolved {
                                    request_id,
                                    session_id,
                                    driver_id,
                                    decision: crate::approvals::ApprovalDecision::Deny,
                                    scope: crate::approvals::ApprovalScope::Once,
                                });
                            }
                        }
                    }
                }
                _ => {}
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
        self.spawn_session_kind_labeled(kind, working_dir, extra_args, initial_prompt, None);
    }

    pub fn spawn_session_kind_labeled(
        &mut self,
        kind: SessionKind,
        working_dir: PathBuf,
        extra_args: Vec<String>,
        initial_prompt: Option<String>,
        label_override: Option<String>,
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
                    label_override.unwrap_or_else(|| format!("claude-{next_id}")),
                )
            }
            SessionKind::Terminal => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
                (
                    shell,
                    extra_args,
                    label_override.unwrap_or_else(|| format!("term-{next_id}")),
                )
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
            Ok(id) => {
                // Phase 7 Task 5: wire the approval coordinator for every
                // Claude session. The coordinator bridges the per-session
                // Unix socket to the ApprovalRegistry + event bus.
                if install_hook {
                    self.start_approval_coordinator(id);
                }
                self.update_file_tree_for_selected();
            }
            Err(e) => {
                log::error!("Failed to spawn session: {e}");
                self.status_message = Some(format!("Failed to spawn session: {e}"));
            }
        }
    }

    /// Resume a dead session in-place: spawn a new Claude session with
    /// `--resume <uuid>` using the same label and working directory,
    /// then remove the old exited entry so it doesn't linger.
    /// Only acts when the selected session is Exited and has a captured UUID.
    pub fn resume_selected(&mut self) {
        let info = {
            let mgr = self.sessions_lock();
            mgr.selected().and_then(|s| {
                if matches!(s.status, crate::session::SessionStatus::Exited(_)) {
                    s.claude_session_id
                        .as_ref()
                        .map(|uuid| (uuid.clone(), s.working_dir.clone(), s.label.clone(), s.id))
                } else {
                    None
                }
            })
        };
        let Some((uuid, working_dir, label, old_id)) = info else {
            self.status_message =
                Some("No resumable session selected (needs exited + UUID)".into());
            return;
        };
        let args = launcher::claude_resume_args(&uuid);
        self.spawn_session_kind_labeled(SessionKind::Claude, working_dir, args, None, Some(label));
        // Remove the old exited entry now that the replacement is live.
        self.sessions_lock().remove_exited(old_id);
    }

    /// Fork the selected session: spawn a new Claude session with
    /// `--resume <uuid> --fork-session`. Works on running or dead sessions
    /// as long as a UUID has been captured.
    pub fn fork_selected(&mut self) {
        let info = {
            let mgr = self.sessions_lock();
            mgr.selected().and_then(|s| {
                s.claude_session_id
                    .as_ref()
                    .map(|uuid| (uuid.clone(), s.working_dir.clone(), s.label.clone()))
            })
        };
        let Some((uuid, working_dir, label)) = info else {
            self.status_message = Some("No forkable session selected (needs captured UUID)".into());
            return;
        };
        let args = launcher::claude_fork_args(&uuid);
        let fork_label = format!("fork-{label}");
        self.spawn_session_kind_labeled(
            SessionKind::Claude,
            working_dir,
            args,
            None,
            Some(fork_label),
        );
    }

    /// Phase 7 Task 5: start the approval coordinator for session `id`.
    ///
    /// Called after every Claude session spawn. Uses the MCP server's
    /// tokio runtime handle to schedule the coordinator task, because the
    /// TUI main thread has no tokio runtime of its own.
    pub(crate) fn start_approval_coordinator(&mut self, session_id: usize) {
        let Some(mcp) = self.mcp.as_ref() else { return };
        let handle = mcp.runtime_handle();
        let rx = self
            .sessions_lock()
            .get_mut(session_id)
            .and_then(|s| s.ensure_approval_socket_running(handle));
        let Some(rx) = rx else {
            log::debug!("session {session_id}: no approval socket rx, skipping coordinator");
            return;
        };
        let sessions = Arc::clone(&self.sessions);
        let approvals = Arc::clone(&self.approvals);
        let bus = Arc::clone(&self.event_bus);
        let attachments = Arc::clone(&self.attachment_map);
        handle.spawn(crate::approvals::run_coordinator(
            rx,
            sessions,
            approvals,
            bus,
            attachments,
        ));
        log::debug!("session {session_id}: approval coordinator started");
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

    /// Phase 7 Task 8: return the pending-approval count for a driver
    /// session, or `0` if the session is not a driver or has no pending
    /// approvals.
    pub(crate) fn pending_approval_count(&self, session_id: usize) -> u32 {
        self.pending_approvals_per_driver
            .get(&session_id)
            .copied()
            .unwrap_or(0)
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
        use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
        use crossterm::{
            execute,
            terminal::{EnterAlternateScreen, LeaveAlternateScreen},
        };
        use std::io::Write;

        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| "vi".to_string());

        self.input_paused.store(true, Ordering::Relaxed);

        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        // Disable mouse tracking so the editor doesn't receive mouse events as garbage
        let _ = write!(stdout, "\x1b[?1000l\x1b[?1002l\x1b[?1006l");
        let _ = stdout.flush();
        let _ = execute!(stdout, LeaveAlternateScreen);

        let _ = std::process::Command::new(&editor).arg(&path).status();

        let _ = enable_raw_mode();
        let _ = execute!(stdout, EnterAlternateScreen);
        // Re-enable mouse tracking (matches main.rs setup)
        let _ = write!(stdout, "\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        let _ = stdout.flush();

        self.input_paused.store(false, Ordering::Relaxed);
        self.needs_full_redraw = true;
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
        let return_to = state.return_to_session;
        self.spawn_session_kind(kind, dir, extra_args, None);
        let new_id = self.sessions_lock().selected().map(|s| s.id).unwrap_or(0);
        self.mode = if return_to.is_some() {
            AppMode::SessionView(new_id)
        } else {
            AppMode::Dashboard
        };
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
                Some(rel) => format!("~/{}/", rel.display()),
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
                    Some(rel) => format!("~/{}", rel.display()),
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

#[cfg(test)]
mod pending_approval_tests {
    use super::*;

    // ---- helpers ----

    /// Minimal helper: build a `pending_approvals_per_driver` map and
    /// exercise the same increment/decrement logic used in handle_event.
    fn apply_requested(map: &mut HashMap<usize, u32>, driver_id: usize) {
        *map.entry(driver_id).or_insert(0) += 1;
    }

    fn apply_resolved(map: &mut HashMap<usize, u32>, driver_id: usize) {
        let cnt = map.entry(driver_id).or_insert(0);
        *cnt = cnt.saturating_sub(1);
    }

    // ---- status-line counter tests ----

    #[test]
    fn status_line_shows_pending_count_for_active_driver() {
        let mut map: HashMap<usize, u32> = HashMap::new();
        let driver_id = 3;

        apply_requested(&mut map, driver_id);
        apply_requested(&mut map, driver_id);

        assert_eq!(map[&driver_id], 2, "two requests → count should be 2");
    }

    #[test]
    fn status_line_clears_on_resolution() {
        let mut map: HashMap<usize, u32> = HashMap::new();
        let driver_id = 7;

        apply_requested(&mut map, driver_id);
        apply_requested(&mut map, driver_id);
        apply_resolved(&mut map, driver_id);
        apply_resolved(&mut map, driver_id);

        assert_eq!(
            map.get(&driver_id).copied().unwrap_or(0),
            0,
            "all requests resolved → count should be 0"
        );
    }

    #[test]
    fn saturating_sub_prevents_underflow() {
        let mut map: HashMap<usize, u32> = HashMap::new();
        let driver_id = 1;

        // Resolve without a preceding request: must not underflow.
        apply_resolved(&mut map, driver_id);
        assert_eq!(
            map.get(&driver_id).copied().unwrap_or(0),
            0,
            "saturating_sub must not underflow past 0"
        );
    }

    // ---- session-list marker test ----

    #[test]
    fn session_list_marker_tracks_pending_count() {
        let mut map: HashMap<usize, u32> = HashMap::new();
        let driver_a = 10;
        let driver_b = 20;

        apply_requested(&mut map, driver_a);
        apply_requested(&mut map, driver_a);
        apply_requested(&mut map, driver_b);

        // driver_a should show marker (count > 0)
        let a_count = map.get(&driver_a).copied().unwrap_or(0);
        assert!(a_count > 0, "driver_a should have a pending marker");

        // driver_b should also show marker
        let b_count = map.get(&driver_b).copied().unwrap_or(0);
        assert!(b_count > 0, "driver_b should have a pending marker");

        // Resolve both of driver_a's requests
        apply_resolved(&mut map, driver_a);
        apply_resolved(&mut map, driver_a);

        let a_count_after = map.get(&driver_a).copied().unwrap_or(0);
        assert_eq!(
            a_count_after, 0,
            "driver_a marker should clear after resolution"
        );

        // driver_b still has one pending
        let b_count_after = map.get(&driver_b).copied().unwrap_or(0);
        assert_eq!(b_count_after, 1, "driver_b should still have one pending");
    }

    #[test]
    fn driver_exit_drops_pending_count() {
        let mut map: HashMap<usize, u32> = HashMap::new();
        let driver_id = 5;

        apply_requested(&mut map, driver_id);
        apply_requested(&mut map, driver_id);

        // Simulate what handle_event does on SessionExited for a driver.
        map.remove(&driver_id);

        assert!(
            !map.contains_key(&driver_id),
            "pending count should be removed on driver exit"
        );
    }
}
