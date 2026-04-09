# ccom (Claude Commander) — Rust TUI for managing Claude Code sessions

## Context

Replacing the bash script `cc-manager` (~/.local/bin/cc-manager) with a proper Rust TUI application. The bash version uses tmux as a multiplexer — the new tool drops tmux and owns PTY management directly. Goal: a portable, cross-compiled binary for macOS + Linux with a rich dashboard experience.

## Design Summary

**htop-like TUI with three panels:**

```
┌─ Files ──────┬─ Sessions ────────────────────────────────┐
│ ~/dev/api    │ # Name       Dir           Status    Last  │
│  ├── src/    │ 1 backend    ~/dev/api     working   3s   │
│  │  ├── ..  │ 2 frontend   ~/dev/web     ⚡ WAIT        │
│  │  └── ..  │ 3 tests      ~/dev/api     idle      45s  │
│  ├── tests/  │                                           │
│  └── ...     │                                           │
├──────────────┴───────────────────────────────────────────┤
│ [n]ew [f]ork [k]ill [a]pprove [Enter]view  [e]dit  [q]uit│
└──────────────────────────────────────────────────────────┘
```

- **Left**: File tree rooted at selected session's working dir. Navigate, preview, edit files, spawn new sessions from any dir.
- **Center**: Session list (default) or live terminal view of selected session.
- **Bottom**: Context-sensitive shortcuts + command input.

## Key Features

- **PTY-based session management** — no tmux dependency. Uses `portable-pty` crate.
- **Live terminal rendering** — `vt100` crate parses PTY output, custom ratatui widget renders it.
- **Permission prompt detection** — regex patterns on vt100 screen text, highlights sessions needing attention.
- **Fork** — uses `claude --resume <session-id> --fork-session`. Session ID discovered from `~/.claude/sessions/{pid}.json`.
- **Built-in editor** — minimal nano-level editor with `syntect` syntax highlighting. Can send file path to a Claude session.
- **File tree** — lazy-loaded, shows active session indicators, supports spawning sessions from dirs.

## Architecture

```
User Input (keyboard) → Event Loop → Dispatch to:
  → PTY Manager (spawn, kill, send keystrokes)
  → UI State Machine (panel focus, navigation)
  → File Tree (navigation, file ops)

PTY Output → vt100 Parser → Screen Buffer → ratatui Widget
File System → notify watcher → File Tree update
```

**No async runtime** — threads + `mpsc` channels. Each PTY reader is a thread. Main thread polls crossterm events + checks channel.

## Project Structure

```
claude-commander/
├── Cargo.toml
├── .github/workflows/release.yml
├── src/
│   ├── main.rs                  # Entry point, clap args, terminal setup/teardown
│   ├── app.rs                   # App state machine, event loop
│   ├── event.rs                 # Event enum (Key, PtyOutput, Tick, FsChange)
│   ├── config.rs                # ~/.config/ccom/ persistence
│   ├── pty/
│   │   ├── mod.rs
│   │   ├── manager.rs           # PTY lifecycle (spawn, kill, list)
│   │   ├── session.rs           # Session: PTY handle + vt100 parser + metadata
│   │   └── detector.rs          # Permission prompt pattern detection
│   ├── claude/
│   │   ├── mod.rs
│   │   ├── discovery.rs         # Read ~/.claude/sessions/{pid}.json for session IDs
│   │   └── launcher.rs          # Build claude commands (new, fork)
│   ├── ui/
│   │   ├── mod.rs
│   │   ├── layout.rs            # Three-panel layout calculations
│   │   ├── theme.rs             # Colors, styles
│   │   ├── panels/
│   │   │   ├── mod.rs
│   │   │   ├── file_tree.rs
│   │   │   ├── session_list.rs
│   │   │   ├── session_view.rs
│   │   │   ├── editor.rs
│   │   │   └── command_bar.rs
│   │   └── widgets/
│   │       ├── mod.rs
│   │       ├── terminal.rs      # vt100::Screen → ratatui::Buffer renderer
│   │       └── tree.rs          # Generic tree widget
│   └── fs/
│       ├── mod.rs
│       └── tree.rs              # Directory tree model + notify watcher
└── tests/
```

## Key Types

```rust
// App state machine
pub struct App {
    sessions: Vec<Session>,
    selected_session: usize,
    focus: PanelFocus,        // FileTree | SessionList | SessionView | Editor | CommandBar
    mode: AppMode,            // Dashboard | SessionView | Editor | Command
    file_tree: FileTree,
    command_input: String,
}

// Session
pub struct Session {
    pub id: usize,
    pub label: String,
    pub claude_session_id: Option<String>,
    pub working_dir: PathBuf,
    pub status: SessionStatus,
    pub pty: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
    pub parser: vt100::Parser,
    pub last_activity: Instant,
    pub needs_attention: bool,
}

pub enum SessionStatus { Running, WaitingForApproval(PromptKind), Idle, Exited(i32) }

// Events
pub enum Event {
    Key(KeyEvent),
    PtyOutput { session_id: usize, data: Vec<u8> },
    Tick,
    FileSystemChange(PathBuf),
    SessionExited { session_id: usize, code: i32 },
}
```

## Dependencies

```toml
ratatui = "0.29"
crossterm = "0.28"
portable-pty = "0.8"
vt100 = "0.15"
clap = { version = "4", features = ["derive"] }
regex = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
syntect = { version = "5", default-features = false, features = ["default-fancy"] }
notify = "7"
dirs = "6"
unicode-width = "0.2"
chrono = "0.4"
```

## Phased Implementation

### Phase 1: MVP — Sessions + PTY + Dashboard

Deliver: launch ccom, see session list, spawn Claude, view live output, approve/deny prompts.

1. `Cargo.toml`, `main.rs` — project scaffold, clap args, terminal setup/teardown
2. `event.rs` — event types + collector (crossterm + mpsc receiver)
3. `pty/session.rs` — Session struct, PTY spawn, reader thread feeding vt100 parser
4. `pty/manager.rs` — spawn/kill/list sessions
5. `pty/detector.rs` — regex prompt detection on screen text
6. `ui/widgets/terminal.rs` — **critical**: vt100::Screen → ratatui buffer mapping
7. `ui/layout.rs` — two-panel layout (session list + command bar)
8. `ui/panels/session_list.rs` — table with status, attention indicators
9. `ui/panels/session_view.rs` — full terminal view, key forwarding to PTY
10. `ui/panels/command_bar.rs` — shortcut display
11. `app.rs` — state machine, event loop, mode switching

**Keybindings (Phase 1):**
- `n` — new session (uses cwd)
- `Enter` — enter session view (live terminal)
- `Esc` — back to dashboard
- `k` — kill session
- `a` — approve (send Enter to PTY)
- `d` — deny (send Down Down Enter)
- `j/k` or arrows — navigate session list
- `q` — quit
- In session view: all keys forwarded to PTY, `Esc` returns to dashboard

### Phase 2: File Tree + Session Discovery

1. `fs/tree.rs` — directory tree model, lazy loading, expand/collapse
2. `ui/panels/file_tree.rs` — tree rendering with session indicators
3. `ui/widgets/tree.rs` — reusable tree widget
4. `ui/layout.rs` — update to three-panel layout
5. `claude/discovery.rs` — poll `~/.claude/sessions/{pid}.json` for session IDs
6. `config.rs` — persist labels, layout prefs to `~/.config/ccom/`
7. `notify` integration for live file tree updates
8. Spawn new sessions from file tree directories

### Phase 3: Fork + Editor + Release

1. `claude/launcher.rs` — `claude --resume <id> --fork-session` command builder
2. Fork workflow: `f` key, read session ID, spawn forked PTY
3. `ui/panels/editor.rs` — minimal editor with syntect highlighting
4. Editor: line numbers, insert/delete/save, `Ctrl+P` to send path to session
5. `.github/workflows/release.yml` — cross-compile for 4 targets, GitHub Releases
6. Targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`

## Risk Areas

1. **vt100 → ratatui color mapping** — need lookup table for 256-color palette. Test with real Claude output.
2. **Key forwarding** — crossterm KeyEvent must be reverse-mapped to ANSI escape sequences for the PTY. Known-solved but careful impl needed.
3. **PTY resize** — window resize must update PTY dimensions + vt100 parser in sync.
4. **Claude output volume** — bounded channel or ring buffer for scrollback to avoid memory issues.
5. **Editor scope** — keep it minimal (nano-level). Don't creep toward vim.

## Verification

- Phase 1: `cargo build && ./target/debug/ccom` → dashboard appears, `n` spawns Claude, `Enter` shows live output, `a` approves prompts
- Phase 2: file tree shows dirs, navigating sessions updates tree root, spawning from tree works
- Phase 3: `f` forks a session with full context, editor opens/saves files, cross-compiled binaries run on both platforms
