use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::claude::context;
use crate::event::{Event, MonitoredSender};
use crate::pty::detector::PromptDetector;
use crate::session::events::TurnId;
use crate::session::hook::{self, HookStopSignal, SidecarHandle};
use crate::session::response_store::ResponseStore;

pub fn lock_parser(p: &Mutex<vt100::Parser>) -> MutexGuard<'_, vt100::Parser> {
    p.lock().unwrap_or_else(|poisoned| {
        log::warn!("vt100 parser mutex poisoned, recovering");
        poisoned.into_inner()
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Running,
    WaitingForApproval(String),
    Idle,
    Exited(i32),
}

/// Session role — Phase 6. `Solo` is every Phase 1–5 session (no
/// spawning privileges, no scope restrictions). `Driver` is a session
/// that may call `spawn_session` via MCP, gated by a [`SpawnPolicy`].
///
/// The `spawn_budget` on `Driver` is a remaining-silent-spawns counter
/// used by `SpawnPolicy::Budget`: decremented on each silent spawn,
/// falls back to [`SpawnPolicy::Ask`] once it hits zero. Irrelevant
/// when the policy is `Ask` or `Trust`.
///
/// Nesting cap: v1 forbids drivers spawning drivers (`spawn_session`
/// always creates `Solo` children). See `docs/plans/phase-6-driver-role.md`
/// §Architecture for rationale.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
// `Driver` is only constructed by Task 2 (`driver_config`) onward,
// so rustc's per-binary reachability check flags it as dead code in
// the bin target until Task 2 lands. Same allow pattern used on
// `SUBMIT_SEQUENCE` in `manager.rs` for the same reason.
#[allow(dead_code)]
pub enum SessionRole {
    /// The default role — matches all Phase 1–5 behavior. Solo
    /// sessions can be managed by the user from the TUI but cannot
    /// call driver-only MCP tools (`spawn_session`).
    #[default]
    Solo,
    /// A fleet-orchestrator session. Can spawn, prompt, read, and
    /// kill its own children + explicitly attached sessions with no
    /// confirmation fatigue (gated by `spawn_policy`).
    Driver {
        /// Remaining silent spawns allowed under
        /// [`SpawnPolicy::Budget`]. Decrementing is the job of
        /// `spawn_session`'s handler, holding the sessions mutex.
        /// Meaningless for `Ask` / `Trust` — left at 0 in those cases
        /// by convention.
        spawn_budget: u32,
        /// Policy lever controlling whether each spawn prompts the
        /// user. See [`SpawnPolicy`] for the three modes.
        spawn_policy: SpawnPolicy,
    },
}

/// Driver spawn policy — Phase 6 §Architecture. Three modes from
/// strictest to loosest:
///
/// - `Ask`: modal on every spawn. Safe for untrusted drivers.
/// - `Budget`: pre-authorize N silent spawns, then fall back to `Ask`.
///   Recommended default.
/// - `Trust`: silent; opt-in per driver run.
///
/// These correspond 1:1 to the `--spawn-policy` CLI enum that Task 2
/// wires up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
// Same per-binary-reachability rationale as `SessionRole::Driver`:
// `Budget` and `Trust` are only referenced once Task 2's config
// surface lands.
#[allow(dead_code)]
pub enum SpawnPolicy {
    /// Modal confirmation on every `spawn_session` call. The safe
    /// default when no policy is specified.
    #[default]
    Ask,
    /// Silent until the driver's `spawn_budget` hits zero, then
    /// fall back to `Ask` behavior. The recommended default for
    /// interactive fleet orchestration.
    Budget,
    /// Never prompt — always silent. Use only for trusted drivers
    /// spun up in the current shell session.
    Trust,
}

pub struct Session {
    pub id: usize,
    pub label: String,
    #[allow(dead_code)]
    pub claude_session_id: Option<String>,
    pub working_dir: PathBuf,
    pub status: SessionStatus,
    pub master: Box<dyn MasterPty + Send>,
    pub writer: Box<dyn Write + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub last_activity: Instant,
    pub needs_attention: bool,
    pub pty_size: PtySize,
    pub context_percent: Option<f64>,
    pub consecutive_write_failures: u32,
    /// Per-session monotonic counter for `TurnId` allocation. Bumped
    /// by [`Session::allocate_turn_id`]; never reused. Phase 2 added
    /// this so `SessionManager::send_prompt` can correlate prompts
    /// with `ResponseComplete` events emitted by Phase 3's response
    /// boundary detector.
    ///
    /// `pub(super)` so the `session::manager::test_support` helpers
    /// that construct stub `Session` values directly can initialize
    /// the field. Outside the `session` module, treat as private —
    /// production code must allocate ids only via
    /// [`Session::allocate_turn_id`].
    pub(super) next_turn_id: u64,
    /// Bounded per-session store of completed prompt/response turns.
    /// Phase 3 added this; the response boundary detector pushes
    /// `StoredTurn`s into it on idle-marker detection, and
    /// `SessionManager::get_response` / `get_latest_response` read
    /// from it on demand.
    ///
    /// `pub(super)` for the same reason as `next_turn_id` — direct
    /// initialization in `make_dummy_session`.
    pub(super) response_store: ResponseStore,
    pub(super) reader_handle: Option<JoinHandle<()>>,
    /// Phase 6 session role. [`SessionRole::Solo`] for every Phase
    /// 1–5 session and for children spawned by drivers; only the
    /// distinguished driver session (created via `--driver` on the
    /// ccom CLI) carries `SessionRole::Driver { .. }`. Controls
    /// access to `spawn_session` and the scope filter on every
    /// other MCP tool.
    ///
    /// Mutable on the manager side because `SpawnPolicy::Budget`
    /// decrements `spawn_budget` on each silent spawn — the handler
    /// holds the sessions mutex across the check-and-decrement to
    /// avoid double-charging concurrent tool calls (Phase 6 Risk #4).
    pub role: SessionRole,
    /// Id of the session that spawned this one via `spawn_session`,
    /// if any. `None` for the top-level driver itself, for Solo
    /// sessions created from the TUI, and for sessions that were
    /// created before Phase 6. Used by the scope helper to build
    /// a driver's implicit "own children" set.
    pub spawned_by: Option<usize>,
    /// Per-session hook directory (Phase 3.5). `Some` for Claude
    /// sessions that have a Stop hook installed; `None` for Terminal
    /// sessions.
    pub(super) hook_dir: Option<PathBuf>,
    /// Receiver for parsed hook signals from the sidecar FIFO reader
    /// thread. `Some` iff `hook_dir` is `Some`.
    pub(super) hook_rx: Option<mpsc::Receiver<HookStopSignal>>,
    /// Handle for the sidecar FIFO reader thread. `Some` iff
    /// `hook_dir` is `Some`.
    pub(super) hook_reader_handle: Option<SidecarHandle>,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        id: usize,
        label: String,
        working_dir: PathBuf,
        command: &str,
        args: &[&str],
        event_tx: MonitoredSender,
        cols: u16,
        rows: u16,
        install_hook: bool,
        mcp_port: Option<u16>,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size)?;

        // Phase 3.5/4: set up the per-session hook directory BEFORE
        // building the command, so we can inject `--settings` and
        // `--mcp-config` flags pointing at files in the hook dir.
        //
        // Design history (the hard-won part):
        //
        // - The original Phase 3.5 approach set `CLAUDE_CONFIG_DIR`
        //   to a symlinked temp dir. This worked in the spike but
        //   broke in production because Claude Code stores
        //   credentials in the macOS Keychain bound to the config
        //   dir path — changing `CLAUDE_CONFIG_DIR` invalidates the
        //   Keychain binding and forces a fresh OAuth flow every
        //   session. Symlinking everything except `settings.json`
        //   didn't help because the Keychain ACL is path-based, not
        //   file-system-based.
        //
        // - The fix: use Claude Code's `--settings <file>` and
        //   `--mcp-config <file>` CLI flags, which load additional
        //   config from explicit paths WITHOUT moving
        //   `CLAUDE_CONFIG_DIR`. The user's real `~/.claude/` stays
        //   the source of truth for auth and everything else; our
        //   hook + MCP config merge on top via the CLI-flag-loaded
        //   files.
        //
        // M7: hook-based boundary detection is Unix-only (relies on
        // mkfifo + blocking open semantics).
        let (hook_dir, hook_rx, hook_reader_handle, extra_args) = if install_hook {
            #[cfg(unix)]
            {
                let hook_dir = hook::create_hook_dir(id)?;
                let fifo_path = hook_dir.join("stop.fifo");
                if let Err(e) = hook::create_stop_fifo(&fifo_path) {
                    hook::cleanup_hook_dir(&hook_dir);
                    return Err(e.into());
                }
                let (handle, rx) = match hook::spawn_fifo_reader(fifo_path, id) {
                    Ok(pair) => pair,
                    Err(e) => {
                        hook::cleanup_hook_dir(&hook_dir);
                        return Err(e.into());
                    }
                };

                // Collect extra CLI flags to pass to the Claude
                // command. `--settings` points at the hook
                // settings.json; `--mcp-config` points at the
                // .mcp.json if the MCP server is running.
                let mut flags: Vec<String> = Vec::new();
                let settings_path = hook_dir.join("settings.json");
                flags.push("--settings".to_string());
                flags.push(settings_path.display().to_string());

                // Phase 4 Task 6: if the MCP server is running, write
                // a `.mcp.json` in the hook dir and pass it via
                // `--mcp-config`. Best-effort — failure logs a
                // warning but does not fail the spawn.
                if let Some(port) = mcp_port {
                    // Phase 6 Task 3: inject the session's own id as
                    // the `X-Ccom-Caller` header so MCP handlers can
                    // identify the caller for scope resolution.
                    match hook::write_mcp_config(&hook_dir, port, Some(id)) {
                        Ok(()) => {
                            let mcp_config_path = hook_dir.join(".mcp.json");
                            flags.push("--mcp-config".to_string());
                            flags.push(mcp_config_path.display().to_string());
                        }
                        Err(e) => {
                            log::warn!(
                                "session {id} failed to write .mcp.json: {e} (MCP tools will be unavailable to this session)"
                            );
                        }
                    }
                }

                (Some(hook_dir), Some(rx), Some(handle), flags)
            }
            #[cfg(not(unix))]
            {
                return Err(anyhow::anyhow!(
                    "hook-based boundary detection requires Unix"
                ));
            }
        } else {
            (None, None, None, Vec::<String>::new())
        };

        let mut cmd = CommandBuilder::new(command);
        // Append the hook-related flags after the caller-supplied
        // args so they can be overridden if the caller already
        // passed `--settings` or `--mcp-config` (unlikely, but
        // don't silently stomp).
        cmd.args(args);
        for flag in &extra_args {
            cmd.arg(flag);
        }
        cmd.cwd(&working_dir);
        if install_hook {
            cmd.env("CCOM_SESSION_ID", id.to_string());
        }

        let child = match pair.slave.spawn_command(cmd) {
            Ok(child) => child,
            Err(e) => {
                if let Some(dir) = hook_dir.as_ref() {
                    hook::cleanup_hook_dir(dir);
                }
                return Err(e);
            }
        };
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let parser_clone = Arc::clone(&parser);
        let session_id = id;

        let handle = thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            if event_tx
                                .send(Event::SessionExited {
                                    session_id,
                                    code: 0,
                                })
                                .is_err()
                            {
                                return true; // receiver dropped
                            }
                            true
                        }
                        Ok(n) => {
                            let data = buf[..n].to_vec();
                            lock_parser(&parser_clone).process(&data);
                            if event_tx
                                .send(Event::PtyOutput { session_id, data })
                                .is_err()
                            {
                                return true; // receiver dropped
                            }
                            false
                        }
                        Err(_) => {
                            let _ = event_tx.send(Event::SessionExited {
                                session_id,
                                code: -1,
                            });
                            true
                        }
                    }
                }));
                match result {
                    Ok(true) => break,
                    Ok(false) => continue,
                    Err(payload) => {
                        let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                            (*s).to_string()
                        } else if let Some(s) = payload.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "<non-string panic payload>".to_string()
                        };
                        log::warn!("pty reader for session {session_id} panicked: {msg}");
                        let _ = event_tx.send(Event::SessionExited {
                            session_id,
                            code: -2,
                        });
                        break;
                    }
                }
            }
        });

        Ok(Self {
            id,
            label,
            claude_session_id: None,
            working_dir,
            status: SessionStatus::Running,
            master: pair.master,
            writer,
            child,
            parser,
            last_activity: Instant::now(),
            needs_attention: false,
            pty_size,
            context_percent: None,
            consecutive_write_failures: 0,
            next_turn_id: 0,
            response_store: ResponseStore::new(),
            reader_handle: Some(handle),
            // Phase 6 Task 1: every freshly spawned session is Solo
            // by default. Drivers are promoted after construction by
            // `SessionManager::set_role` (wired up in Task 2 via
            // `App::pending_driver_role` → `spawn_session_kind`), and
            // children created via `spawn_session` will have their
            // `spawned_by` set by the same MCP-side plumbing in
            // Task 3. Leaving the positional signature of
            // `Session::spawn` untouched keeps every existing call
            // site (including the 16 `SpawnConfig { .. }` literals
            // across tests) unchanged.
            role: SessionRole::Solo,
            spawned_by: None,
            hook_dir,
            hook_rx,
            hook_reader_handle,
        })
    }

    /// Allocate the next `TurnId` for this session. Returns the
    /// current value of `next_turn_id` wrapped in a `TurnId`, then
    /// increments the counter so the next call yields a fresh id.
    /// Monotonic for the lifetime of the `Session`; never reused.
    ///
    /// Called by `SessionManager::send_prompt` (Phase 2) before
    /// publishing `SessionEvent::PromptSubmitted`. The returned
    /// `TurnId` is the correlation key the response boundary
    /// detector (Phase 3) will pair with the matching
    /// `ResponseComplete`.
    pub(crate) fn allocate_turn_id(&mut self) -> TurnId {
        let id = TurnId::new(self.next_turn_id);
        self.next_turn_id += 1;
        id
    }

    pub fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn try_write(&mut self, bytes: &[u8]) {
        match self.write(bytes) {
            Ok(()) => {
                self.consecutive_write_failures = 0;
            }
            Err(e) => {
                log::warn!("session {} write failed: {e}", self.id);
                self.consecutive_write_failures += 1;
                if self.consecutive_write_failures >= 3 {
                    log::warn!(
                        "session {} exited after 3 consecutive write failures",
                        self.id
                    );
                    self.status = SessionStatus::Exited(-3);
                }
            }
        }
    }

    pub fn try_resize(&mut self, cols: u16, rows: u16) {
        if let Err(e) = self.resize(cols, rows) {
            log::warn!("session {} resize failed: {e}", self.id);
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master.resize(self.pty_size)?;
        lock_parser(&self.parser).screen_mut().set_size(rows, cols);
        Ok(())
    }

    pub fn check_attention(&mut self, detector: &PromptDetector) {
        if matches!(self.status, SessionStatus::Exited(_)) {
            return;
        }

        let parser = lock_parser(&self.parser);
        let screen = parser.screen();
        if let Some(kind) = detector.check(screen) {
            self.needs_attention = true;
            self.status = SessionStatus::WaitingForApproval(format!("{kind:?}"));
        } else if self.last_activity.elapsed() > std::time::Duration::from_secs(5) {
            self.needs_attention = false;
            self.status = SessionStatus::Idle;
        } else {
            self.needs_attention = false;
            self.status = SessionStatus::Running;
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        self.status = SessionStatus::Exited(-1);
        // Drained signals are discarded here — we're killing the
        // session, so there's nothing to publish.
        let _ = self.cleanup_hook_artifacts();
    }

    /// Test seam: install a channel receiver for hook signals
    /// directly on a session, bypassing `Session::spawn`. The returned
    /// `Sender` lets tests inject synthetic `HookStopSignal`s that
    /// `SessionManager::check_hook_signals` will consume.
    ///
    /// Also installs a dummy `hook_dir` path so the invariant
    /// "`hook_rx` is `Some` iff `hook_dir` is `Some`" documented on
    /// the fields holds for test-constructed sessions. The dummy path
    /// does not exist on disk and `cleanup_hook_artifacts` will
    /// harmlessly try (and fail) to remove it.
    #[cfg(test)]
    pub(crate) fn install_test_hook_channel(&mut self) -> mpsc::Sender<HookStopSignal> {
        let (tx, rx) = mpsc::channel();
        self.hook_rx = Some(rx);
        self.hook_dir = Some(PathBuf::from(format!(
            "/tmp/ccom-test-hook-dir-{}",
            self.id
        )));
        tx
    }

    /// Clean up hook-related resources and return any pending hook
    /// signals that were still in the channel.
    ///
    /// Order of operations (deliberate — see review C3 and the
    /// second-pass N1 follow-up in `docs/pr-review-pr13.md`):
    ///
    /// 1. Drain `hook_rx` into a `Vec` of pending signals BEFORE
    ///    dropping the receiver, so final-turn signals aren't lost.
    /// 2. Request the sidecar reader thread to stop via its
    ///    `SidecarHandle`'s atomic flag.
    /// 3. Best-effort write-poke the FIFO to unblock a pending
    ///    `File::open` in the reader (the stop flag alone can't wake
    ///    a blocked open).
    /// 4. Join the sidecar thread with a bounded timeout. On timeout,
    ///    log at error level with session id and context — the thread
    ///    is leaked but cleanup continues.
    /// 5. **Drain `hook_rx` a second time.** Between step 1 and the
    ///    join returning, the reader thread may have sent one or more
    ///    additional signals. After join returns the thread is gone,
    ///    so this second drain is race-free.
    /// 6. Drop the receiver.
    /// 7. Remove the hook dir from disk.
    ///
    /// Idempotent — safe to call on a session without hook artifacts,
    /// and safe to call a second time (returns an empty Vec).
    ///
    /// Callers in `Session::kill` discard the returned signals
    /// (the session is being killed; nothing to publish).
    /// `SessionManager::reap_exited` consumes them and pushes them
    /// through the boundary detector before publishing `Exited`, so
    /// a final `ResponseComplete` still reaches subscribers.
    pub(super) fn cleanup_hook_artifacts(&mut self) -> Vec<HookStopSignal> {
        // 1. Drain pending signals BEFORE dropping the receiver.
        let mut drained = Vec::new();
        if let Some(rx) = self.hook_rx.as_ref() {
            while let Ok(signal) = rx.try_recv() {
                drained.push(signal);
            }
        }

        // 2–4. Tell the sidecar to stop, unblock any pending
        //      File::open, then bounded-join.
        if let Some(mut handle) = self.hook_reader_handle.take() {
            handle.request_stop();
            // Fast path: if the reader already exited on its own
            // (e.g. the child closed the FIFO before we got here),
            // skip the write-poke and go straight to the join, which
            // will return immediately.
            if !handle.is_finished() {
                // Best-effort write-poke: open the fifo for writing
                // so a reader blocked inside `File::open` wakes up.
                if let Some(dir) = self.hook_dir.as_ref() {
                    let fifo = dir.join("stop.fifo");
                    if fifo.exists() {
                        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
                    }
                }
            }
            if let Err(e) = handle.join_with_timeout(Duration::from_millis(500)) {
                // Loud on leak — the thread is now orphaned.
                log::error!(
                    "session {} sidecar reader thread leaked during cleanup: {e}",
                    self.id
                );
            }
        }

        // 5. Second drain. Between step 1 and join returning, the
        //    reader may have pushed additional signals into the
        //    channel. After join the thread is gone, so this is
        //    race-free. Only runs if the receiver is still alive
        //    (i.e. this isn't a second call on an already-cleaned
        //    session).
        if let Some(rx) = self.hook_rx.as_ref() {
            while let Ok(signal) = rx.try_recv() {
                drained.push(signal);
            }
        }

        // 6. Drop the receiver.
        self.hook_rx = None;

        // 7. Remove the hook dir. Still attempted even if the join
        //    above timed out — the dir is on disk, not tied to the
        //    thread.
        if let Some(dir) = self.hook_dir.take() {
            hook::cleanup_hook_dir(&dir);
        }

        drained
    }

    pub fn join_reader(&mut self, timeout: Duration) {
        if let Some(handle) = self.reader_handle.take() {
            let start = Instant::now();
            while !handle.is_finished() {
                if start.elapsed() >= timeout {
                    log::warn!(
                        "session {} reader thread did not exit within timeout",
                        self.id
                    );
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = handle.join();
        }
    }

    pub fn refresh_context(&mut self) {
        if matches!(self.status, SessionStatus::Exited(_)) {
            return;
        }
        if let Some(pid) = self.child.process_id() {
            self.context_percent = context::get_context_percent(pid);
        }
    }

    pub fn elapsed_since_activity(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }

    /// Build a fake `Session` in the `Exited(0)` state for unit tests.
    ///
    /// No real PTY is opened and no process is spawned. The `master`,
    /// `writer`, and `child` fields are stub objects that panic if anything
    /// tries to drive them — tests that exercise lifecycle bookkeeping only
    /// (id/label/status/selection) should never touch them.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn dummy_exited(id: usize, label: &str) -> Self {
        use portable_pty::PtySize;
        use test_helpers::{DummyChild, DummyPty, DummyWriter};

        let pty_size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };

        Self {
            id,
            label: label.to_string(),
            claude_session_id: None,
            working_dir: PathBuf::from("/tmp"),
            status: SessionStatus::Exited(0),
            master: Box::new(DummyPty),
            writer: Box::new(DummyWriter),
            child: Box::new(DummyChild),
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 1000))),
            last_activity: Instant::now(),
            needs_attention: false,
            pty_size,
            context_percent: None,
            consecutive_write_failures: 0,
            next_turn_id: 0,
            response_store: ResponseStore::new(),
            reader_handle: None,
            role: SessionRole::Solo,
            spawned_by: None,
            hook_dir: None,
            hook_rx: None,
            hook_reader_handle: None,
        }
    }

    /// Phase 6 test helper: promote a `dummy_exited` session to a
    /// driver role in a single chained call. Returns `self` so
    /// fixtures can write
    /// `Session::dummy_exited(1, "orch").with_role(SessionRole::Driver { .. })`
    /// without the five-line struct-update dance.
    ///
    /// Production code should NOT use this — drivers are created via
    /// the Task 2/3 plumbing that routes through `SessionManager` so
    /// the spawned-by bookkeeping stays consistent. This helper exists
    /// purely so unit tests and integration fixtures can build
    /// non-default-role sessions without a real PTY.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn with_role(mut self, role: SessionRole) -> Self {
        self.role = role;
        self
    }

    /// Companion to [`Self::with_role`]: set the spawned-by pointer.
    /// Same test-only rationale — production spawn paths go through
    /// the manager.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn with_spawned_by(mut self, parent: usize) -> Self {
        self.spawned_by = Some(parent);
        self
    }
}

#[doc(hidden)]
#[allow(dead_code)]
pub mod test_helpers {
    use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
    use std::io::{Result as IoResult, Write};

    /// No-op stand-in for `MasterPty`. All methods panic — tests must not
    /// drive the pty.
    #[derive(Debug)]
    pub struct DummyPty;

    impl MasterPty for DummyPty {
        fn resize(&self, _size: PtySize) -> Result<(), anyhow::Error> {
            panic!("DummyPty::resize should not be called from tests");
        }

        fn get_size(&self) -> Result<PtySize, anyhow::Error> {
            panic!("DummyPty::get_size should not be called from tests");
        }

        fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
            panic!("DummyPty::try_clone_reader should not be called from tests");
        }

        fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
            panic!("DummyPty::take_writer should not be called from tests");
        }

        #[cfg(unix)]
        fn process_group_leader(&self) -> Option<i32> {
            None
        }

        #[cfg(unix)]
        fn as_raw_fd(&self) -> Option<portable_pty::unix::RawFd> {
            None
        }

        #[cfg(unix)]
        fn tty_name(&self) -> Option<std::path::PathBuf> {
            None
        }
    }

    /// No-op stand-in for the session's `Box<dyn Write + Send>`.
    #[derive(Debug)]
    pub struct DummyWriter;

    impl Write for DummyWriter {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> IoResult<()> {
            Ok(())
        }
    }

    /// No-op stand-in for `Box<dyn Child + Send + Sync>`. `try_wait` returns
    /// `Ok(None)` (still running) so `reap_exited` leaves the session alone
    /// if a test happens to call it on a dummy in a non-Exited state.
    #[derive(Debug)]
    pub struct DummyChild;

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
}

#[cfg(test)]
mod tests {
    //! Tests for `Session` state that doesn't require a real PTY.
    //! Phase 2 added the `next_turn_id` counter and `allocate_turn_id`
    //! method; the tests below pin its monotonic + per-session-isolated
    //! contract.

    use super::*;

    #[test]
    fn allocate_turn_id_starts_at_zero() {
        let mut s = Session::dummy_exited(1, "a");
        assert_eq!(s.allocate_turn_id(), TurnId::new(0));
    }

    #[test]
    fn allocate_turn_id_is_monotonic() {
        let mut s = Session::dummy_exited(1, "a");
        let ids: Vec<TurnId> = (0..5).map(|_| s.allocate_turn_id()).collect();
        assert_eq!(
            ids,
            vec![
                TurnId::new(0),
                TurnId::new(1),
                TurnId::new(2),
                TurnId::new(3),
                TurnId::new(4),
            ]
        );
    }

    #[test]
    fn allocate_turn_id_never_reuses_a_value() {
        // Stronger version of monotonic: across many allocations, no
        // id repeats. Catches a future "reset on overflow" or
        // "reuse after reap" regression.
        let mut s = Session::dummy_exited(1, "a");
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = s.allocate_turn_id();
            assert!(seen.insert(id), "TurnId {id:?} was reused");
        }
    }

    // -----------------------------------------------------------------
    // Phase 6 Task 1 — session role / spawned_by defaults.
    //
    // These pin the three invariants the Phase 6 plan calls out:
    // 1. Every freshly constructed session defaults to Solo role.
    // 2. Every freshly constructed session has spawned_by = None.
    // 3. The with_role/with_spawned_by builders on dummy_exited produce
    //    driver variants correctly for use as test fixtures downstream.
    //
    // If any of these break, the scope-resolution logic in Tasks 3/4/6
    // will silently mis-classify sessions — a Driver-looking Solo or a
    // Solo-looking Driver is exactly the kind of bug that only surfaces
    // at the MCP boundary where we don't want it.
    // -----------------------------------------------------------------

    #[test]
    fn session_role_defaults_to_solo() {
        let s = Session::dummy_exited(1, "a");
        assert_eq!(s.role, SessionRole::Solo);
    }

    #[test]
    fn session_spawned_by_defaults_to_none() {
        let s = Session::dummy_exited(1, "a");
        assert_eq!(s.spawned_by, None);
    }

    #[test]
    fn with_role_promotes_to_driver() {
        let s = Session::dummy_exited(1, "orchestrator").with_role(SessionRole::Driver {
            spawn_budget: 5,
            spawn_policy: SpawnPolicy::Budget,
        });
        assert_eq!(
            s.role,
            SessionRole::Driver {
                spawn_budget: 5,
                spawn_policy: SpawnPolicy::Budget,
            },
        );
        // Idempotence of the ADJACENT field: promoting the role must
        // NOT touch spawned_by, because drivers themselves have no
        // parent and Task 3's attachment flow builds the child-set
        // from spawned_by — corrupting it here would make children
        // invisible to their own driver.
        assert_eq!(s.spawned_by, None);
    }

    #[test]
    fn with_spawned_by_sets_parent_pointer() {
        let child = Session::dummy_exited(2, "child").with_spawned_by(1);
        assert_eq!(child.spawned_by, Some(1));
        // spawned_by alone does NOT promote to Driver — children are
        // always Solo under the v1 nesting cap.
        assert_eq!(child.role, SessionRole::Solo);
    }

    #[test]
    fn spawn_policy_defaults_to_ask() {
        // The `SpawnPolicy::default()` implementation is a load-bearing
        // safety property: if config loading falls back to the default
        // (no CLI flag, no TOML), we get the strictest policy, not
        // silent auto-spawning.
        assert_eq!(SpawnPolicy::default(), SpawnPolicy::Ask);
    }

    #[test]
    fn session_role_default_matches_solo() {
        assert_eq!(SessionRole::default(), SessionRole::Solo);
    }

    #[test]
    fn allocate_turn_id_is_independent_per_session() {
        // Two sessions on the same manager (or in this test, just two
        // independent `Session`s) must have independent counters —
        // a turn id allocated by session A says nothing about session
        // B's next turn id.
        let mut a = Session::dummy_exited(1, "a");
        let mut b = Session::dummy_exited(2, "b");

        let _ = a.allocate_turn_id(); // a is now at 1
        let _ = a.allocate_turn_id(); // a is now at 2

        assert_eq!(b.allocate_turn_id(), TurnId::new(0));
        assert_eq!(a.allocate_turn_id(), TurnId::new(2));
        assert_eq!(b.allocate_turn_id(), TurnId::new(1));
    }
}
