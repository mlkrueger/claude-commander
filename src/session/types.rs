use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use crate::claude::context;
use crate::event::Event;
use crate::pty::detector::PromptDetector;

pub fn lock_parser(p: &Mutex<vt100::Parser>) -> MutexGuard<'_, vt100::Parser> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Running,
    WaitingForApproval(String),
    Idle,
    Exited(i32),
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
}

impl Session {
    pub fn spawn(
        id: usize,
        label: String,
        working_dir: PathBuf,
        command: &str,
        args: &[&str],
        event_tx: mpsc::Sender<Event>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(pty_size)?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        cmd.cwd(&working_dir);

        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let parser_clone = Arc::clone(&parser);
        let session_id = id;

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                let _ = event_tx.send(Event::SessionExited {
                                    session_id,
                                    code: 0,
                                });
                                true
                            }
                            Ok(n) => {
                                let data = buf[..n].to_vec();
                                lock_parser(&parser_clone).process(&data);
                                let _ = event_tx.send(Event::PtyOutput { session_id, data });
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
        })
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
    }

    pub fn refresh_context(&mut self) {
        if matches!(self.status, SessionStatus::Exited(_)) {
            return;
        }
        if let Some(pid) = self.child.process_id() {
            self.context_percent = context::get_context_percent(pid);
        }
    }

    #[allow(dead_code)]
    pub fn elapsed_since_activity(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }

    /// Build a fake `Session` in the `Exited(0)` state for unit tests.
    ///
    /// No real PTY is opened and no process is spawned. The `master`,
    /// `writer`, and `child` fields are stub objects that panic if anything
    /// tries to drive them — tests that exercise lifecycle bookkeeping only
    /// (id/label/status/selection) should never touch them.
    #[cfg(test)]
    pub(crate) fn dummy_exited(id: usize, label: &str) -> Self {
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
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
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
