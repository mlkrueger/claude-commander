mod app;
mod claude;
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
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let size = terminal.size()?;

    let events = EventCollector::new(Duration::from_millis(50));
    let event_tx = events.sender();

    let mut app = App::new(event_tx, working_dir.clone());
    app.terminal_cols = size.width;
    app.terminal_rows = size.height;

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
            if app.mouse_captured {
                execute!(io::stdout(), DisableMouseCapture)?;
                app.mouse_captured = false;
                app.status_message =
                    Some("Mouse capture OFF — select text freely, Alt+M to re-enable".to_string());
            } else {
                execute!(io::stdout(), EnableMouseCapture)?;
                app.mouse_captured = true;
                app.status_message = Some("Mouse capture ON — scroll with mouse".to_string());
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
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;

    Ok(())
}
