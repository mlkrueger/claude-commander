use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use crate::claude::context;
use crate::event::Event;
use crate::pty::detector::PromptDetector;

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
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = event_tx.send(Event::SessionExited {
                            session_id,
                            code: 0,
                        });
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        parser_clone.lock().unwrap().process(&data);
                        let _ = event_tx.send(Event::PtyOutput { session_id, data });
                    }
                    Err(_) => {
                        let _ = event_tx.send(Event::SessionExited {
                            session_id,
                            code: -1,
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
        })
    }

    pub fn write(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.pty_size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.master.resize(self.pty_size)?;
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        Ok(())
    }

    pub fn check_attention(&mut self, detector: &PromptDetector) {
        if matches!(self.status, SessionStatus::Exited(_)) {
            return;
        }

        let parser = self.parser.lock().unwrap();
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

    pub fn elapsed_since_activity(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }
}
