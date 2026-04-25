# ccom — Claude Commander

A terminal dashboard for managing multiple [Claude Code](https://docs.anthropic.com/en/docs/claude-code) sessions. Spawn, monitor, and interact with Claude instances from a single TUI — no tmux required.

```
┌─ ccom ──────────────────────────────────────────────────────────────────┐
│ # Name        Dir              Status         Cost    Last              │
│ 1 backend     ~/dev/api        working         $0.12   3s               │
│ 2 frontend    ~/dev/web        ⚡ WAIT APPROVE  $0.04                    │
│ 3 tests       ~/dev/api        idle            $0.08   45s              │
├─────────────────────────────────────────────────────────────────────────┤
│ [n]new [Enter]view [a]approve [d]deny [f]fork [R]resume [?]help [q]quit │
└─────────────────────────────────────────────────────────────────────────┘
```

## Features

- **Multi-session dashboard** — spawn, monitor, and manage many Claude Code sessions at once
- **Live terminal view** — drop into any session with full interactive terminal passthrough
- **Tool approval routing** — approve or deny individual tool calls without leaving the dashboard; grant allow-always per-tool
- **Session picker** — switch between sessions without returning to the dashboard (`Alt+s` in session view)
- **Fork & resume** — fork a session to branch work, or resume an externally-started Claude session
- **Per-session cost tracking** — see token cost accumulate per session in real time
- **File tree** — browse the working directory with git status colors; open files in `$EDITOR`
- **Inline prompt** — send a prompt to a session directly from the dashboard (`Alt+p`)
- **MCP server** — embedded MCP server lets Claude sessions call back into ccom for coordinated multi-agent work
- **Auto-update** — checks GitHub on launch and can self-update in place (`U` on the dashboard)
- **Themes** — cycle through color themes with `t`

## Install

Each release ships two binaries in the tarball: `ccom` (the TUI) and `ccom-hook-pretooluse` (the per-session PreToolUse hook). Both must be installed to the same directory.

### Download a release

```bash
# macOS (Apple Silicon)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-macos-arm64.tar.gz | tar xz
sudo mv ccom ccom-hook-pretooluse /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-macos-x86_64.tar.gz | tar xz
sudo mv ccom ccom-hook-pretooluse /usr/local/bin/

# Linux (x86_64)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-linux-x86_64.tar.gz | tar xz
sudo mv ccom ccom-hook-pretooluse /usr/local/bin/

# Linux (ARM64)
curl -L https://github.com/mlkrueger/claude-commander/releases/latest/download/ccom-linux-arm64.tar.gz | tar xz
sudo mv ccom ccom-hook-pretooluse /usr/local/bin/
```

### Build from source

Requires [Rust](https://rustup.rs/):

```bash
git clone https://github.com/mlkrueger/claude-commander.git
cd claude-commander
cargo build --release
sudo cp target/release/ccom target/release/ccom-hook-pretooluse /usr/local/bin/
```

## Prerequisites

- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) installed and available as `claude` in your PATH

## Usage

```bash
ccom                        # launch dashboard
ccom -d ~/myproject         # start rooted at a specific directory
ccom -s 2                   # launch with 2 Claude sessions pre-spawned
```

On first launch, ccom shows a setup screen. Press `Enter` to let ccom configure the statusline hook automatically, or `Esc` to skip. Press `S` on the dashboard at any time to re-run setup.

## Keyboard shortcuts

### Dashboard

| Key | Action |
|-----|--------|
| `n` | New session |
| `Enter` | Enter session (live terminal view) |
| `a` | Approve — send "yes" to selected session |
| `d` | Deny — send "no" to selected session |
| `r` | Rename session |
| `f` | Fork session (branch from current state) |
| `R` | Resume an externally-started Claude session |
| `K` | Kill session |
| `x` | Clear dead sessions |
| `c` | Send `/commit` to selected session |
| `Alt+p` | Send an inline prompt to the selected session |
| `t` | Cycle color theme |
| `U` | Check for updates / install update |
| `S` | Open setup screen |
| `?` | Toggle help overlay |
| `Tab` | Switch focus between session list and file tree |
| `q` | Quit |

### Session view (live terminal)

| Key | Action |
|-----|--------|
| `Alt+d` | Return to dashboard |
| `Alt+s` | Open session picker (switch to another session) |
| `PageUp` / `PageDown` | Scroll through history (freezes auto-follow; scroll to bottom to resume) |
| `Mouse wheel` | Scroll through history |
| Everything else | Forwarded to Claude |

### Session picker (`Alt+s` from session view)

| Key | Action |
|-----|--------|
| `↑/↓` or `j/k` | Navigate |
| `Enter` | Switch to highlighted session |
| `Esc` | Cancel |

### File tree (dashboard `Tab`)

| Key | Action |
|-----|--------|
| `↑/↓` | Navigate |
| `Enter` / `→` | Expand directory |
| `←` | Collapse directory / navigate up to parent |
| `e` | Open file in `$EDITOR` |
| `n` | New Claude session in selected directory |
| `R` | Refresh tree |
| `Tab` | Switch back to session list |

## How it works

ccom spawns Claude Code processes in pseudo-terminals (PTYs) and renders their output using a built-in VT100 emulator. No tmux or screen required — ccom is the multiplexer.

**Tool approval**: each Claude session is started with a per-session `ccom-hook-pretooluse` PreToolUse hook. When Claude requests a tool call, the hook routes the request to ccom's approval coordinator over a Unix socket. ccom shows the pending approval in the dashboard and waits for your `a`/`d` keypress. Allow-always rules skip the prompt for subsequent identical calls.

**MCP server**: ccom runs an embedded MCP server on loopback. Each spawned Claude session gets the server's port injected so Claude can call back into ccom — for example to coordinate across sessions.

**File tree**: git status colors throughout — yellow for modified, green for staged, gray for untracked, red for deleted. Refreshes every 5 seconds.

**Auto-update**: on startup, ccom checks GitHub for a newer release in the background. If one is found, a banner appears. Press `U` to download and install both binaries in place (Homebrew installs skip this — use `brew upgrade ccom` instead).

## License

Apache License 2.0 — see [LICENSE](LICENSE) for details.
