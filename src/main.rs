mod app;
mod claude;
mod event;
mod fs;
mod pty;
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
    env_logger::init();

    let cli = Cli::parse();
    let working_dir = cli.dir.canonicalize().unwrap_or_else(|_| cli.dir.clone());

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Get initial terminal size
    let size = terminal.size()?;

    // Create event collector with 50ms tick rate
    let events = EventCollector::new(Duration::from_millis(50));
    let event_tx = events.sender();

    // Send periodic ticks
    let tick_tx = event_tx.clone();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(200));
            if tick_tx.send(event::Event::Tick).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(event_tx, working_dir.clone());
    app.terminal_cols = size.width;
    app.terminal_rows = size.height;

    // Spawn initial sessions if requested
    for _ in 0..cli.spawn {
        app.spawn_session(working_dir.clone());
    }

    // Main loop
    loop {
        terminal.draw(|frame| app.draw(frame))?;

        if let Ok(event) = events.next() {
            app.handle_event(event);
        }

        // Drain any queued events
        while let Some(event) = events.try_next() {
            app.handle_event(event);
        }

        // Handle mouse capture toggle
        if app.toggle_mouse_capture {
            app.toggle_mouse_capture = false;
            if app.mouse_captured {
                execute!(io::stdout(), DisableMouseCapture)?;
                app.mouse_captured = false;
                app.status_message = Some(
                    "Mouse capture OFF — select text freely, Ctrl+Shift+M to re-enable".to_string(),
                );
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

    // Cleanup: kill all sessions
    for session in &mut app.sessions {
        session.kill();
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;

    Ok(())
}
