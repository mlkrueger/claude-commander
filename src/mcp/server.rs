//! `McpServer` lifecycle: spawn a dedicated thread, run tokio
//! inside it, serve rmcp over streamable HTTP on loopback.
//!
//! See `docs/plans/phase-4-mcp-readonly.md` Task 3 for the full
//! design rationale and `docs/plans/notes/rmcp-spike.md` for the
//! empirical rmcp 1.4.0 findings this module is built against.
//!
//! Architecture: the main TUI thread calls [`McpServer::start`],
//! which spawns a dedicated OS thread named `ccom-mcp`. That thread
//! owns a current-thread tokio runtime and `block_on`s the axum
//! server. The main thread receives the assigned loopback port back
//! via a `std::sync::mpsc` channel and stores a shutdown
//! `oneshot::Sender` + a `CancellationToken` for graceful teardown.
//! No tokio primitives ever escape the `ccom-mcp` thread.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use super::handlers::Ccom;
use super::state::McpCtx;

/// Handle to the running MCP server. `start` spawns the `ccom-mcp`
/// thread; `stop` consumes the handle and joins cleanly.
pub struct McpServer {
    port: u16,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    cancel: CancellationToken,
}

impl McpServer {
    /// Spawn the dedicated `ccom-mcp` thread, bind to
    /// `127.0.0.1:0`, and return a handle once the assigned port
    /// is available. Waits up to 2 seconds for the thread to report
    /// its assigned port.
    pub fn start(ctx: Arc<McpCtx>) -> anyhow::Result<Self> {
        // Review M1: the port channel carries `Result<u16, String>` so
        // that a bind/local_addr/loopback-assertion failure in
        // `run_server` surfaces as `Err` here instead of the previous
        // "return Ok(port=0)" hack. The main thread must see
        // startup failures, not silently get a zero port.
        let (port_tx, port_rx) = std::sync::mpsc::sync_channel::<Result<u16, String>>(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();

        let thread = std::thread::Builder::new()
            .name("ccom-mcp".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        log::error!("ccom-mcp tokio runtime build failed: {e}");
                        let _ = port_tx.send(Err(format!("tokio runtime build failed: {e}")));
                        return;
                    }
                };
                rt.block_on(run_server(ctx, port_tx, shutdown_rx, cancel_for_thread));
            })?;

        let port = match port_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(port)) => port,
            Ok(Err(msg)) => return Err(anyhow::anyhow!("ccom-mcp startup failed: {msg}")),
            Err(e) => return Err(anyhow::anyhow!("ccom-mcp thread did not report port: {e}")),
        };

        Ok(Self {
            port,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
            cancel,
        })
    }

    /// Test/integration helper: construct a [`McpCtx`] internally
    /// from the provided `sessions` and `bus` and start the server.
    /// Lets integration tests (which can't name the `pub(crate)`
    /// `McpCtx` type) drive the full server lifecycle without
    /// widening the visibility of the ctx type itself.
    ///
    /// Used only by `tests/mcp_readonly.rs`. Integration tests compile
    /// against this crate as an external consumer, so rustc's per-crate
    /// dead-code analysis doesn't see the callers — hence the
    /// `#[allow(dead_code)]`.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn start_with(
        sessions: Arc<Mutex<crate::session::SessionManager>>,
        bus: Arc<crate::session::EventBus>,
    ) -> anyhow::Result<Self> {
        // Integration tests that don't need the confirmation bridge —
        // `kill_session` auto-denies when `confirm` is `None`, which
        // is fine for tests that don't exercise the modal path
        // (`tests/mcp_readonly.rs`, `tests/mcp_write.rs::send_*`).
        // Tests that DO need it use `start_with_confirm` below.
        Self::start(Arc::new(McpCtx {
            sessions,
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        }))
    }

    /// Phase 5 test seam: start the MCP server with a real
    /// `ConfirmBridge` wired in. Returns the server handle plus the
    /// receiver side of the bridge so the test can simulate the
    /// main-thread modal-drain loop and answer each incoming
    /// `ConfirmRequest` with `Allow`/`Deny`.
    ///
    /// Same `#[allow(dead_code)]` rationale as `start_with` — only
    /// called from `tests/mcp_write.rs` which rustc's per-crate
    /// dead-code analysis doesn't observe.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn start_with_confirm(
        sessions: Arc<Mutex<crate::session::SessionManager>>,
        bus: Arc<crate::session::EventBus>,
    ) -> anyhow::Result<(
        Self,
        std::sync::mpsc::Receiver<super::confirm::ConfirmRequest>,
    )> {
        let (bridge, rx) = super::confirm::ConfirmBridge::new();
        let ctx = Arc::new(McpCtx {
            sessions,
            bus,
            confirm: Some(bridge),
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        });
        let server = Self::start(ctx)?;
        Ok((server, rx))
    }

    /// Phase 6 Task 3 test seam: like [`Self::start_with_confirm`]
    /// but also wires an `event_tx` onto the ctx so `spawn_session`
    /// can actually create child sessions. Tests pass a real
    /// `MonitoredSender` they can drain to observe PTY output.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn start_with_confirm_and_event_tx(
        sessions: Arc<Mutex<crate::session::SessionManager>>,
        bus: Arc<crate::session::EventBus>,
        event_tx: crate::event::MonitoredSender,
    ) -> anyhow::Result<(
        Self,
        std::sync::mpsc::Receiver<super::confirm::ConfirmRequest>,
    )> {
        let (bridge, rx) = super::confirm::ConfirmBridge::new();
        let ctx = Arc::new(McpCtx {
            sessions,
            bus,
            confirm: Some(bridge),
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: Some(event_tx),
        });
        let server = Self::start(ctx)?;
        Ok((server, rx))
    }

    /// Phase 6 Task 9 test seam: like
    /// [`Self::start_with_confirm_and_event_tx`] but also accepts a
    /// pre-built `attachments` map so integration tests can seed
    /// driver-side attachment visibility before the server starts.
    /// Used by `tests/driver_spawn.rs` test #9
    /// (`attached_session_visible_in_driver_scope`) to validate that
    /// a driver's `list_sessions` respects the shared attachment map.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn start_with_confirm_event_tx_and_attachments(
        sessions: Arc<Mutex<crate::session::SessionManager>>,
        bus: Arc<crate::session::EventBus>,
        event_tx: crate::event::MonitoredSender,
        attachments: Arc<Mutex<std::collections::HashMap<usize, std::collections::HashSet<usize>>>>,
    ) -> anyhow::Result<(
        Self,
        std::sync::mpsc::Receiver<super::confirm::ConfirmRequest>,
    )> {
        let (bridge, rx) = super::confirm::ConfirmBridge::new();
        let ctx = Arc::new(McpCtx {
            sessions,
            bus,
            confirm: Some(bridge),
            attachments,
            event_tx: Some(event_tx),
        });
        let server = Self::start(ctx)?;
        Ok((server, rx))
    }

    /// Assigned loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Graceful shutdown: signal axum to stop accepting new
    /// connections, cancel in-flight SSE streams, and join the
    /// `ccom-mcp` thread with a 2-second timeout. Mirrors the
    /// polling pattern in `SidecarHandle::join_with_timeout`.
    pub fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        self.cancel.cancel();
        if let Some(handle) = self.thread.take() {
            let start = Instant::now();
            let timeout = Duration::from_secs(2);
            while !handle.is_finished() {
                if start.elapsed() >= timeout {
                    log::error!("ccom-mcp thread did not exit within {timeout:?}; orphaning");
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Err(e) = handle.join() {
                log::error!("ccom-mcp thread panicked on join: {e:?}");
            }
        }
    }
}

async fn run_server(
    ctx: Arc<McpCtx>,
    port_tx: std::sync::mpsc::SyncSender<Result<u16, String>>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    cancel: CancellationToken,
) {
    // Review H1: explicitly pin `allowed_hosts` to loopback only.
    // rmcp 1.4's default already sets this, but making it explicit
    // here pins the security contract in ccom's source so a future
    // rmcp patch that loosens the default can't silently widen our
    // attack surface. The Host-header check defends against DNS
    // rebinding attacks from a malicious web page loaded in the
    // user's browser.
    let config = StreamableHttpServerConfig::default().with_allowed_hosts(vec![
        "localhost",
        "127.0.0.1",
        "::1",
    ]);

    // Task 7: bind loopback ONLY. A future refactor that accidentally
    // flips this to `0.0.0.0` would expose ccom's internal state to
    // the network; the runtime assertion below refuses to start the
    // server in that case.
    const BIND_ADDR: &str = "127.0.0.1:0";
    let listener = match tokio::net::TcpListener::bind(BIND_ADDR).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("ccom-mcp listener bind failed: {e}");
            let _ = port_tx.send(Err(format!("bind({BIND_ADDR}) failed: {e}")));
            return;
        }
    };
    let local_addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            log::error!("ccom-mcp local_addr() failed: {e}");
            let _ = port_tx.send(Err(format!("local_addr() failed: {e}")));
            return;
        }
    };
    if !local_addr.ip().is_loopback() {
        log::error!(
            "ccom-mcp refusing to serve: bound address {} is not loopback",
            local_addr.ip()
        );
        let _ = port_tx.send(Err(format!(
            "bound address {} is not loopback",
            local_addr.ip()
        )));
        return;
    }
    let port = local_addr.port();
    if port_tx.send(Ok(port)).is_err() {
        log::error!("ccom-mcp port channel closed before send; main thread gone");
        return;
    }

    // Phase 6 Task 3: the `spawn_session` handler needs to know the
    // port so it can write a `.mcp.json` for newly spawned children.
    // Build the factory closure AFTER bind so we can hand the port
    // into every `Ccom` instance.
    let service = StreamableHttpService::new(
        {
            let ctx = Arc::clone(&ctx);
            move || Ok(Ccom::new_with_port(Arc::clone(&ctx), port))
        },
        LocalSessionManager::default().into(),
        config,
    );
    let router = axum::Router::new().nest_service("/mcp", service);

    let cancel_for_shutdown = cancel.clone();
    if let Err(e) = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
            cancel_for_shutdown.cancel();
        })
        .await
    {
        log::error!("ccom-mcp axum::serve exited with error: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EventBus, SessionManager};
    use std::sync::Mutex;

    fn test_ctx() -> Arc<McpCtx> {
        let bus = Arc::new(EventBus::new());
        let sessions = Arc::new(Mutex::new(SessionManager::with_bus(Arc::clone(&bus))));
        Arc::new(McpCtx {
            sessions,
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        })
    }

    #[test]
    fn server_start_binds_nonzero_port_and_stops_cleanly() {
        let server = McpServer::start(test_ctx()).expect("start");
        assert!(server.port() > 0, "port must be nonzero");
        server.stop();
    }

    #[test]
    fn server_port_is_loopback_accessible() {
        let server = McpServer::start(test_ctx()).expect("start");
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", server.port()).parse().unwrap();
        let conn = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2));
        assert!(conn.is_ok(), "tcp connect failed: {:?}", conn.err());
        server.stop();
    }

    /// Review T4: verify the `Result<u16, String>` channel shape in
    /// the port handoff. This is a direct test of the
    /// `recv_timeout` → pattern-match logic in `start()` — we
    /// simulate an error case by posting `Err(..)` on the same
    /// channel type and confirming it maps to `anyhow::Err`.
    ///
    /// This doesn't exercise `run_server`'s error paths (hard to
    /// trigger `TcpListener::bind("127.0.0.1:0")` failure in a
    /// portable test), but it DOES pin the contract that an error
    /// payload from the thread surfaces as a startup failure on
    /// the main thread — the exact regression the M1 fix closes.
    #[test]
    fn port_handoff_maps_error_to_start_failure() {
        // Build the channel the real code uses.
        let (port_tx, port_rx) = std::sync::mpsc::sync_channel::<Result<u16, String>>(1);
        // Simulate `run_server` reporting a bind failure.
        port_tx
            .send(Err("bind(127.0.0.1:0) failed: synthetic".into()))
            .expect("send");

        // Reproduce the match arms from `McpServer::start`.
        let result: anyhow::Result<u16> =
            match port_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(Ok(port)) => Ok(port),
                Ok(Err(msg)) => Err(anyhow::anyhow!("ccom-mcp startup failed: {msg}")),
                Err(e) => Err(anyhow::anyhow!("ccom-mcp thread did not report port: {e}")),
            };

        let err = result.expect_err("error payload must surface as startup failure");
        let msg = format!("{err}");
        assert!(
            msg.contains("startup failed"),
            "error message should include 'startup failed': {msg}"
        );
        assert!(
            msg.contains("synthetic"),
            "error message should preserve inner reason: {msg}"
        );
    }

    /// Companion to the T4 test above: verify the Ok-of-Ok path
    /// surfaces the port cleanly.
    #[test]
    fn port_handoff_ok_returns_port() {
        let (port_tx, port_rx) = std::sync::mpsc::sync_channel::<Result<u16, String>>(1);
        port_tx.send(Ok(42)).expect("send");
        let result: anyhow::Result<u16> =
            match port_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(Ok(port)) => Ok(port),
                Ok(Err(msg)) => Err(anyhow::anyhow!("ccom-mcp startup failed: {msg}")),
                Err(e) => Err(anyhow::anyhow!("ccom-mcp thread did not report port: {e}")),
            };
        assert_eq!(result.unwrap(), 42);
    }
}
