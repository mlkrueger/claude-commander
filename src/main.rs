mod app;
mod approvals;
mod approvals_state;
mod claude;
mod driver_config;
mod event;
mod fs;
mod mcp;
mod pty;
mod session;
mod setup;
mod ui;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::Write as _;

use app::App;
use event::EventCollector;

const TICK_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Parser)]
#[command(
    name = "ccom",
    about = "Claude Commander — manage multiple Claude Code sessions"
)]
struct Cli {
    /// Working directory for new sessions
    #[arg(short, long, default_value = ".")]
    dir: PathBuf,

    /// Immediately spawn N claude sessions on start
    #[arg(short, long, default_value_t = 0)]
    spawn: usize,

    /// Promote the first Claude session spawned by ccom to a driver
    /// (Phase 6 fleet orchestrator). No-op if more spawns have
    /// already happened.
    #[arg(long)]
    driver: bool,

    /// Spawn policy for the driver: `ask` (default — modal every
    /// spawn), `budget` (silent until budget is exhausted), or
    /// `trust` (always silent). Requires `--driver`.
    #[arg(long, value_enum, requires = "driver")]
    spawn_policy: Option<driver_config::SpawnPolicyArg>,

    /// Pre-authorized silent spawn budget for the driver. Requires
    /// `--driver`. Ignored unless `--spawn-policy budget`.
    #[arg(long, requires = "driver")]
    budget: Option<u32>,
}

fn main() -> anyhow::Result<()> {
    // TUI mode: env_logger writes to stderr by default, which
    // interleaves with ratatui's alternate-screen draw output and
    // visibly corrupts the TUI. Redirect all log output to a file
    // in the system temp dir instead.
    //
    // Also bumps rmcp logs to `warn` so the routine
    // "session keep alive timeout after 30000ms" / "Session service
    // terminated" messages (which rmcp 1.4 emits at ERROR level on
    // normal idle-teardown of an MCP session) don't spam the log
    // file either. Upgrade to `info` or drop the filter entirely
    // via `RUST_LOG` when actively debugging MCP.
    let log_path = std::env::temp_dir().join(format!("ccom-{}.log", std::process::id()));
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            let _ = env_logger::Builder::from_env(
                env_logger::Env::default().default_filter_or("info,rmcp=warn"),
            )
            .target(env_logger::Target::Pipe(Box::new(file)))
            .try_init();
        }
        Err(_) => {
            // Fall back to a no-op stderr logger if we can't open
            // the file — better to lose logs than corrupt the TUI.
            let _ =
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off"))
                    .try_init();
        }
    }
    log::info!("ccom starting; log file: {}", log_path.display());

    let cli = Cli::parse();
    let working_dir = cli.dir.canonicalize().unwrap_or_else(|_| cli.dir.clone());

    // Phase 6 Task 2: resolve the driver config from CLI + TOML
    // before constructing the App so `App::new`'s caller gets to
    // decide whether the first Claude spawn should be promoted.
    let driver_cfg = driver_config::load_driver_config(cli.driver, cli.spawn_policy, cli.budget);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Mouse tracking setup. crossterm's EnableMouseCapture sends five modes
    // including ?1003h (any-event motion), which some terminals treat as
    // unsupported and silently disable ALL mouse tracking in response.
    //
    // We send three modes only:
    //   ?1000h — normal tracking: button press/release (includes scroll wheel)
    //   ?1002h — button-event tracking: required by many terminals (iTerm2,
    //             Kitty, WezTerm) to actually deliver scroll wheel events
    //   ?1006h — SGR extended coordinates: preferred encoding, wider range
    //
    // We deliberately omit ?1003h (any-event / all-motion) and ?1015h
    // (URXVT coords, superseded by SGR).
    write!(stdout, "\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    log::info!("mouse capture enabled (?1000h ?1002h ?1006h)");
    stdout.flush()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let size = terminal.size()?;

    let events = EventCollector::new(Duration::from_millis(50));
    let event_tx = events.sender();

    let mut app = App::new(event_tx, working_dir.clone());
    app.terminal_cols = size.width;
    app.terminal_rows = size.height;
    // Phase 6 Task 2: stash the pending driver role on the App so
    // the first Claude session spawned (via `spawn_session_kind`)
    // gets promoted post-construction. `take()`-once semantics
    // inside `spawn_session_kind` guarantee only the first Claude
    // session is touched even if multiple spawn up-front via
    // `--spawn N`.
    app.pending_driver_role = driver_cfg.as_ref().map(|c| c.to_role());

    for _ in 0..cli.spawn {
        app.spawn_session(working_dir.clone());
    }

    loop {
        terminal.draw(|frame| app.draw(frame))?;

        if let Some(event) = events.next_timeout(TICK_INTERVAL) {
            app.handle_event(event);
        } else {
            app.handle_event(event::Event::Tick);
        }

        while let Some(event) = events.try_next() {
            app.handle_event(event);
        }

        if app.toggle_mouse_capture {
            app.toggle_mouse_capture = false;
            let mut out = io::stdout();
            if app.mouse_captured {
                // Disable: inverses of ?1000h ?1002h ?1006h in reverse order.
                write!(out, "\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
                out.flush()?;
                app.mouse_captured = false;
                app.status_message = Some(
                    "Mouse capture OFF — text selection enabled. Alt+M to re-enable".to_string(),
                );
                app.status_message_tick = app.tick_count;
                log::info!("mouse capture disabled");
            } else {
                write!(out, "\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
                out.flush()?;
                app.mouse_captured = true;
                app.status_message = Some("Mouse capture ON — scroll captured by ccom".to_string());
                app.status_message_tick = app.tick_count;
                log::info!("mouse capture re-enabled");
            }
        }

        // Some terminals silently reset mouse tracking mode after a resize.
        // Re-assert capture after every resize event so scroll stays captured.
        if app.reapply_mouse_capture {
            app.reapply_mouse_capture = false;
            if app.mouse_captured {
                let mut out = io::stdout();
                write!(out, "\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
                out.flush()?;
                log::info!("mouse capture re-asserted after resize");
            }
        }

        if app.should_quit {
            break;
        }
    }

    {
        let mut mgr = app.sessions.lock().unwrap_or_else(|p| p.into_inner());
        for session in mgr.iter_mut() {
            session.kill();
        }
    }
    drop(events);
    {
        let mut mgr = app.sessions.lock().unwrap_or_else(|p| p.into_inner());
        for session in mgr.iter_mut() {
            session.join_reader(Duration::from_millis(500));
        }
    }

    // Stop the embedded MCP server before clearing the terminal so
    // any log::error! on orphan thread teardown still reaches stderr.
    if let Some(mcp) = app.mcp.take() {
        mcp.stop();
    }

    disable_raw_mode()?;
    let mut out = io::stdout();
    write!(out, "\x1b[?1006l\x1b[?1002l\x1b[?1000l")?; // disable mouse tracking
    execute!(out, LeaveAlternateScreen)?;

    Ok(())
}
