# Phase 3.5: Hook-Based Response Boundary Detection

**Status:** Implementation plan
**Depends on:** Spike GO (docs/plans/notes/hook-spike.md)
**Branch:** `session-mgmt/phase-3.5-hook-boundary`

## Context

The Phase 3 response boundary detector uses a placeholder regex that never fires
for real Claude sessions. The spike confirmed Claude Code's Stop hook provides a
reliable, rich signal including `last_assistant_message` in stdin JSON. This plan
implements the hook-based detector to replace the placeholder for Claude sessions.

## Key Spike Findings That Shape This Plan

1. **`last_assistant_message` in stdin eliminates PTY body capture.** No need for
   the `BodyAccumulator` refactor. `StoredTurn::body` comes from hook stdin.
2. **Project-level `.claude/settings.json` works for hook injection** without
   auth issues. But for ccom we need per-session isolation, so we use a temp
   working directory per Claude session with our `.claude/settings.json` in it.
3. **Hooks are blocking by default.** Our hook script must be fast (<100ms).
4. **Custom env vars pass through.** `CCOM_SESSION_ID` works for correlation.

## Architecture

```
App::spawn_session_kind(Claude, ...)
  │
  ├── Create /tmp/ccom-<sid>/ with .claude/settings.json containing Stop hook
  ├── Create /tmp/ccom-<sid>/stop.fifo (named pipe)
  ├── Spawn Claude Code with:
  │     cwd = user's actual working_dir
  │     env CCOM_SESSION_ID = <sid>
  │     env CCOM_STOP_FIFO = /tmp/ccom-<sid>/stop.fifo
  │     .claude/settings.json in cwd OR via project flag
  │
  ├── Spawn sidecar reader thread:
  │     loop { read line from FIFO → parse JSON → send HookEvent to channel }
  │
  └── Session struct stores: fifo_path, sidecar_handle, hook_rx

App::handle_event(Event::Tick)
  └── check_hook_signals()
        for each session with a hook_rx:
          while let Ok(signal) = hook_rx.try_recv():
            if active turn exists:
              complete turn with signal.last_assistant_message as body
              publish ResponseComplete
            else:
              drop (user-typed prompt, no TurnId allocated)
```

## Implementation Steps

### Step 1: New Event + Hook Signal Types (~30 min)

**Files:** `src/event.rs`, new `src/session/hook.rs`

Add a `HookStopSignal` struct to carry parsed Stop hook stdin:
```rust
pub struct HookStopSignal {
    pub ccom_session_id: usize,
    pub claude_session_id: String,
    pub last_assistant_message: String,
    pub transcript_path: Option<String>,
}
```

Add `src/session/hook.rs` with:
- `HookStopSignal` struct
- `parse_hook_stdin(json: &str) -> Option<HookStopSignal>` parser
- Unit tests for parsing

### Step 2: Hook Settings + FIFO Infrastructure (~1 hour)

**Files:** new `src/session/hook.rs` (continued), `src/session/types.rs`

Add to `hook.rs`:
- `create_hook_dir(session_id: usize) -> Result<PathBuf>` — creates `/tmp/ccom-<pid>-<sid>/`
  with `.claude/settings.json` containing the Stop hook config
- `create_stop_fifo(hook_dir: &Path) -> Result<PathBuf>` — creates the named pipe
- `cleanup_hook_dir(hook_dir: &Path)` — removes the temp dir
- The hook command in settings.json: reads stdin, writes JSON line to the FIFO
- `spawn_fifo_reader(fifo_path: PathBuf, session_id: usize) -> (JoinHandle, Receiver<HookStopSignal>)`
  — spawns a thread that reads lines from the FIFO and sends parsed signals

Add to `Session` struct:
- `hook_dir: Option<PathBuf>` — temp dir for hook config (None for Terminal sessions)
- `hook_rx: Option<mpsc::Receiver<HookStopSignal>>` — channel from sidecar reader
- `hook_reader_handle: Option<JoinHandle<()>>` — sidecar thread handle

### Step 3: Wire Hook Infrastructure into Session::spawn (~1 hour)

**Files:** `src/session/types.rs`, `src/session/manager.rs`, `src/app/mod.rs`

For Claude sessions (determined by a new `is_claude: bool` field on `SpawnConfig`):
1. `Session::spawn` creates hook dir + FIFO before spawning the child
2. Adds env vars to `CommandBuilder`: `CCOM_SESSION_ID`, `CCOM_STOP_FIFO`
3. Sets up project-level `.claude/settings.json` in the hook dir
4. Spawns the FIFO reader thread
5. Stores hook_dir, hook_rx, hook_reader_handle on the Session

For Terminal sessions: all hook fields are `None`.

Update `Session::kill()` and add cleanup:
- Join hook reader thread
- Remove hook dir (including FIFO)

Update `SpawnConfig` to carry `is_claude: bool`.
Update `App::spawn_session_kind` to set `is_claude` based on `SessionKind`.

### Step 4: Hook-Based Boundary Detection (~1 hour)

**Files:** `src/session/manager.rs`

Add `SessionManager::check_hook_signals()`:
- For each session with a `hook_rx`, drain signals via `try_recv()`
- For each signal: if there's an active turn in the boundary detector,
  complete it using `signal.last_assistant_message` as the body
- Push `StoredTurn` to the session's `ResponseStore`
- Publish `SessionEvent::ResponseComplete` on the bus
- If no active turn: silently drop (user-typed prompt)

Call `check_hook_signals()` from `App::check_all_attention()` alongside
the existing `check_response_boundaries()`.

The existing regex-based `check_response_boundaries()` stays for Terminal
sessions. For Claude sessions, the hook signal takes precedence.

### Step 5: Integration Tests (~1 hour)

**Files:** `tests/unit_tests.rs`

- `hook_stop_signal_parses_valid_json` — unit test for parser
- `hook_settings_json_is_valid` — validate generated settings.json
- `fifo_round_trip` — write to FIFO, read from reader thread
- `session_spawn_creates_hook_dir_for_claude` — verify hook dir exists after spawn
- `session_kill_cleans_up_hook_dir` — verify cleanup

Real-Claude integration test (may be `#[ignore]` for CI):
- Spawn real Claude session via ccom
- Send prompt via `send_prompt`
- Wait for `ResponseComplete` on bus
- Assert `get_latest_response` contains the response text

### Step 6: Documentation Updates (~30 min)

**Files:** design doc, spike doc

- Update `docs/designs/response-boundary-detection.md` §4 to reflect
  the actual implementation (FIFO instead of Unix socket, no BodyAccumulator)
- Mark the limitation in `docs/pr-review-pr9.md` as resolved
- Update spike doc with pointer to implementation PR

## What Stays vs. What Changes

| Component | Before | After |
|---|---|---|
| `ResponseBoundaryDetector` (regex) | Active for all sessions, never fires for Claude | Active only for Terminal sessions |
| `StoredTurn::body` source | ANSI-stripped PTY bytes | `last_assistant_message` from hook stdin (Claude) / PTY bytes (Terminal) |
| `check_response_boundaries()` | Runs for all sessions | Runs only for non-hook sessions |
| `feed_pty_data()` | Called for all sessions | Still called (PTY display), but not used for body capture on hook sessions |
| `on_prompt_submitted()` | Marks turn start in regex detector | Also marks turn start for hook correlation |

## Verification

1. `cargo test` — all existing 249 tests pass + new tests
2. `cargo clippy` — zero warnings
3. Manual: spawn Claude session in ccom, send prompt, verify `ResponseComplete`
   fires and response body is captured
4. Manual: spawn Terminal session, verify no hook artifacts created
5. Manual: kill Claude session, verify `/tmp/ccom-*` dir cleaned up
