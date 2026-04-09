# ccom — Claude Commander

A terminal dashboard for managing multiple [Claude Code](https://docs.anthropic.com/en/docs/claude-code) sessions. Spawn, monitor, and interact with Claude instances from a single TUI — no tmux required.

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

## Features

- **Multi-session management** — spawn and switch between multiple Claude Code sessions
- **Live terminal view** — drop into any session with full interactive terminal
- **Permission prompt detection** — highlights sessions waiting for approval
- **Quick approve/deny** — approve or reject tool calls from the dashboard
- **File tree** — browse the working directory, see git status, open files
- **Built-in editor** — quick edits with line numbers, send file paths to Claude
- **Git status** — modified, staged, and untracked files highlighted in the tree
- **Commit shortcut** — send `/commit` to a session with one keystroke

## Install

### Download a release

Grab the latest binary from [GitHub Releases](https://github.com/mlkrueger/claude-commander/releases):

```bash
# macOS (Apple Silicon)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-macos-arm64.tar.gz | tar xz
sudo mv ccom /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-macos-x86_64.tar.gz | tar xz
sudo mv ccom /usr/local/bin/

# Linux (x86_64)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-linux-x86_64.tar.gz | tar xz
sudo mv ccom /usr/local/bin/

# Linux (ARM64)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-linux-arm64.tar.gz | tar xz
sudo mv ccom /usr/local/bin/
```

### Build from source

Requires [Rust](https://rustup.rs/):

```bash
git clone https://github.com/mlkrueger/claude-commander.git
cd claude-commander
cargo build --release
cp target/release/ccom /usr/local/bin/
```

## Prerequisites

- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) must be installed and available as `claude` in your PATH

## Usage

```bash
ccom                        # launch dashboard
ccom -d ~/myproject         # start rooted at a specific directory
ccom -s 2                   # launch with 2 Claude sessions immediately
```

### Keyboard shortcuts

**Dashboard (session list)**

| Key | Action |
|-----|--------|
| `n` | New session (type path, `Tab` to autocomplete) |
| `Enter` | View session (live terminal) |
| `a` | Approve — send "Yes" to selected session |
| `d` | Deny — send "No" to selected session |
| `c` | Send `/commit` to selected session |
| `K` | Kill session |
| `x` | Clear dead sessions |
| `r` | Rename session |
| `↑/↓` | Navigate sessions |
| `Tab` | Switch to file tree |
| `q` | Quit |

**File tree**

| Key | Action |
|-----|--------|
| `↑/↓` | Navigate |
| `Enter` / `→` | Expand directory |
| `←` | Collapse directory |
| `e` | Edit file |
| `n` | New Claude session in selected directory |
| `R` | Refresh tree |
| `Tab` | Switch to session list |

**Session view**

| Key | Action |
|-----|--------|
| `Ctrl+O` | Back to dashboard |
| Everything else | Forwarded to the Claude session |

**Editor**

| Key | Action |
|-----|--------|
| `Ctrl+S` | Save |
| `Ctrl+P` | Send file path to a Claude session |
| `Ctrl+O` | Close editor |
| `Tab` | Insert 4 spaces |

## How it works

ccom spawns Claude Code processes in pseudo-terminals (PTYs) and renders their output using a virtual terminal emulator. No tmux or screen required — ccom is the multiplexer.

The file tree shows git status colors: yellow for modified, green for staged, gray for untracked, red for deleted. It refreshes automatically every 5 seconds.

Permission prompts (like "Allow once", "Yes/No") are detected by scanning the terminal output and flagged in the session list so you can approve from the dashboard without switching into the session.

## License

MIT
