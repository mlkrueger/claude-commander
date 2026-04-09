# ccom (Claude Commander) — Rust TUI for managing Claude Code sessions

## Context

Replacing the bash script `cc-manager` (~/.local/bin/cc-manager) with a Rust TUI application. The bash version used tmux as a multiplexer — ccom drops tmux and owns PTY management directly via `portable-pty`. Goal: a portable, cross-compiled binary for macOS + Linux with a rich dashboard experience.

## Status (2026-04-08)

### Done

- **Phase 1: MVP** — fully working
  - PTY-based session management (spawn, kill, resize)
  - Live terminal rendering (vt100 -> ratatui widget)
  - Permission prompt detection (regex on screen text)
  - Session list dashboard with colored status (green=running, yellow=waiting, gray=idle, red=exited)
  - Session view with full key forwarding (`Ctrl+O` to exit back to dashboard)
  - Approve (`a`), deny (`d`), kill (`K`), rename (`r`), clear dead (`x`)
  - New session with tab-complete on directory paths + validation

- **Phase 2: File Tree + Session Discovery**
  - Three-panel layout: file tree (left) | session list (center) | command bar (bottom)
  - Lazy-loaded directory tree, expand/collapse, session indicators (●)
  - `Tab` switches focus between file tree and session list
  - `n` in file tree spawns new session in selected directory
  - Git status integration: files colored by status (M=yellow, S=green, ?=gray, A=green, D=red)
  - Git status auto-refreshes every 5 seconds
  - Directories show "worst" child status color
  - Session discovery module ready (`~/.claude/sessions/{pid}.json` parsing)

- **Phase 3 (partial): Editor + CI**
  - Built-in editor: line numbers, cursor, insert/delete/save, page navigation
  - `Ctrl+S` save, `Ctrl+O` close, `Ctrl+P` send file path to a Claude session
  - `e` from file tree opens files in editor
  - `/commit` shortcut (`c` from dashboard sends `/commit` to selected session)
  - GitHub Actions workflow: cross-compile for 4 targets, release on tag push
  - Targets: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu
  - 18 unit tests passing (detector, editor, file tree, git status)
  - Zero compiler warnings, code formatted with `cargo fmt`

### Not Yet Done

- **Fork**: `f` key to fork a session using `claude --resume <id> --fork-session`
  - Discovery module exists, launcher helpers exist, just needs wiring
  - Need to capture PID from spawned Claude process, poll for session file
- **Config persistence**: save session labels, layout prefs to `~/.config/ccom/`
- **Syntax highlighting**: tried syntect, had rendering issues — stripped for now. Could revisit with tree-sitter or simpler regex coloring
- **File system watcher**: `notify` crate for live file tree updates (currently manual `R` refresh)

## Layout

```
┌─ Files ──────┬─ Sessions ────────────────────────────────┐
│ ~/dev/api    │ # Name       Dir           Status    Last  │
│  ├── src/ M  │ 1 backend    ~/dev/api     working   3s   │
│  │  ├── ..  │ 2 frontend   ~/dev/web     ⚡ WAIT        │
│  ├── tests/  │ 3 tests      ~/dev/api     idle      45s  │
│  └── Cargo ? │                                           │
├──────────────┴───────────────────────────────────────────┤
│ [n]new [Enter]view [a]pprove [c]commit [Tab]files [q]quit│
└──────────────────────────────────────────────────────────┘
```

## Keybindings

### Dashboard (session list focused)
- `↑/↓` — navigate sessions
- `Enter` — view session (live terminal)
- `n` — new session (type path, Tab to complete)
- `a` — approve (send Enter to PTY)
- `d` — deny (send Down+Down+Enter)
- `c` — send `/commit` to selected session
- `K` — kill session
- `x` — clear dead sessions
- `r` — rename session
- `Tab` — switch to file tree
- `q` / `Ctrl+C` — quit

### Dashboard (file tree focused)
- `↑/↓` — navigate tree
- `Enter` / `→` — expand directory
- `←` — collapse directory
- `e` — edit file
- `n` — new session in selected directory
- `R` — refresh tree
- `Tab` — switch to session list

### Session view
- `Ctrl+O` — back to dashboard
- All other keys forwarded to the PTY (including Esc)

### Editor
- `Ctrl+S` — save
- `Ctrl+P` — send file path to a Claude session
- `Ctrl+O` — close editor
- Arrows, Home/End, PgUp/PgDn — navigation
- Tab — insert 4 spaces

## Architecture

```
User Input (keyboard) → Event Loop → Dispatch to:
  → PTY Manager (spawn, kill, send keystrokes)
  → UI State Machine (panel focus, navigation)
  → File Tree (navigation, file ops)

PTY Output → vt100 Parser → Screen Buffer → ratatui Widget
Git status → periodic poll (5s) → file tree coloring
```

Threads + `mpsc` channels, no async runtime. Each PTY reader is a thread.

## Project Structure

```
src/
├── main.rs           # Entry point, clap, terminal setup
├── lib.rs            # Public module exports for tests
├── app.rs            # App state machine, event loop, all key handling
├── event.rs          # Event enum + collector
├── pty/
│   ├── session.rs    # Session: PTY + vt100 parser + writer + metadata
│   └── detector.rs   # Permission prompt pattern detection
├── claude/
│   ├── discovery.rs  # Session ID extraction from ~/.claude/sessions/
│   └── launcher.rs   # CLI command builders (new, fork, resume)
├── ui/
│   ├── layout.rs     # Three-panel + session-view layouts
│   ├── theme.rs      # Color constants
│   ├── panels/
│   │   ├── file_tree.rs     # Left panel with git status
│   │   ├── session_list.rs  # Center panel htop-like table
│   │   ├── session_view.rs  # Full terminal view
│   │   ├── editor.rs        # Built-in file editor
│   │   └── command_bar.rs   # Bottom shortcuts bar
│   └── widgets/
│       └── terminal.rs      # vt100::Screen → ratatui buffer
├── fs/
│   ├── tree.rs       # Directory tree model
│   └── git.rs        # Git status parsing
tests/
└── unit_tests.rs     # 18 tests
.github/workflows/
└── release.yml       # Cross-compile CI + GitHub Releases
```

## Building & Running

```bash
cargo build                      # debug build
cargo build --release            # release build
cargo test                       # run tests
./target/debug/ccom              # launch
./target/debug/ccom -s 2         # launch with 2 Claude sessions
./target/debug/ccom -d ~/myproj  # launch rooted at a directory
```

## Releasing

Tag a version to trigger CI:
```bash
git tag v0.1.0
git push origin v0.1.0
```

Or manually trigger via GitHub Actions workflow_dispatch with a specific target.
