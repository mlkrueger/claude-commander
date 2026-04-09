# Changelog

All notable changes to ccom (Claude Commander) will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.3] - 2026-04-09

### Fixed
- **Mouse capture toggle not working** — `Ctrl+Shift+M` wasn't arriving reliably. Switched to `Alt+M` for consistency with other Alt keybindings.

## [0.2.2] - 2026-04-09

### Fixed
- **Keybindings not working** — `Ctrl+\` and `Ctrl+]` never reached the app (OS SIGQUIT / terminal swallowing). Switched to reliable Alt keybindings: `Alt+D` for dashboard, `Alt+S` for session picker.
- **Filter key repeat/release events** — Only handle `KeyEventKind::Press`, ignoring crossterm 0.29 Release/Repeat events.

### Changed
- **Mouse capture toggle** — Changed from `Alt+M` to `Ctrl+Shift+M`.

## [0.2.1] - 2026-04-09

### Changed
- **Back-to-dashboard keybind** — Changed from `Ctrl+O` to `Alt+O` so that `Ctrl+O` passes through to Claude Code for expanding/collapsing tool call output. All ccom-level controls now use the `Alt+` modifier consistently (`Alt+O`, `Alt+S`, `Alt+M`).

## [0.2.0] - 2026-04-09

### Added
- **Color themes** — 7 built-in themes: Default, Green Terminal, Tron, Amber, Ocean, Hot Pink, and Rainbow. Press `t` in dashboard mode to cycle.
- **Animated rainbow borders** — Rainbow theme paints HSL-cycling colors that rotate around every panel border.
- **Mouse support** — Scroll through sessions and file tree with the mouse. `Alt+M` toggles capture for native text selection.
- **Session quick-picker** — `Alt+S` in session view opens a fast switcher overlay without returning to dashboard.
- **Usage monitoring** — Rate limit gauges (5-hour session and 7-day weekly), context percentage tracking per session, and session cost display.
- **Usage graph panel** — New bottom panel on dashboard showing rate limit progress bars and reset times.
- **Statusline hook script** — `scripts/ccom-statusline.sh` reads Claude Code statusline JSON and writes rate limit data for the TUI to consume.
- **Setup/onboarding screen** — Checks for required tools and presents an interactive screen (`S` key) to fix missing configurations.
- **Forward Ctrl+C to sessions** — Ctrl+C in session view is sent to the PTY instead of quitting ccom.

## [0.1.0] - 2026-04-08

### Added
- Initial release of Claude Commander.
- Multi-session management — spawn, rename, kill Claude Code instances from a single TUI.
- Dashboard with session list showing status, working directory, and last activity.
- Full-screen session view with VT100 terminal emulation.
- File tree panel with git status indicators.
- Built-in file editor with syntax-aware gutter.
- Approve/deny tool requests from the dashboard (`a`/`d` keys).
- Send commit prompts to sessions (`c` key).
- PTY-based session management with automatic prompt detection.
- Cross-platform build support (macOS, Linux).

[0.2.3]: https://github.com/mlkrueger/claude-commander/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/mlkrueger/claude-commander/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mlkrueger/claude-commander/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mlkrueger/claude-commander/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mlkrueger/claude-commander/releases/tag/v0.1.0
