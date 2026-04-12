# Design: Response Boundary Detection

**Status:** Design phase. Tier 1 (hook-based) chosen; spike pending.
**Author:** @mkrueger
**Date:** 2026-04-11
**Supersedes:** the placeholder pattern in `crate::pty::response_boundary::ResponseBoundaryDetector::for_claude_code` (Phase 3)
**Related:** `docs/designs/session-management.md` §4, `docs/pr-review-pr9.md` (the limitation this resolves)

## 1. Problem statement

The Phase 3 response boundary detector (`crate::pty::response_boundary`)
needs to know **when a Claude Code session has finished generating a
response and is back at its idle input prompt.** Without that signal:

- `SessionEvent::ResponseComplete` never fires for real Claude sessions.
- The Model Council's synthesizer (Council Phase 3) has nothing to wait
  on — it can't tell when its panelist sessions are done.
- The MCP `read_response` tool (Phase 4) returns nothing useful for
  in-flight turns; the response store stays empty for real sessions.
- Driver sessions (Phase 6) can't poll "is my child done yet?"

The Phase 3 implementation shipped with a placeholder regex
(`__CCOM_PLACEHOLDER_CLAUDE_IDLE__`) that will essentially never match
real Claude Code output. The detector is **installed but dormant** in
production. This doc resolves that.

## 2. The naive approach and why it fails

The first instinct is "watch the PTY for the input prompt and regex-
match it." This is what the current `ResponseBoundaryDetector` does.
The fundamental problem: **after ANSI-stripping, Claude Code's idle
prompt may degrade to a few characters of plain text** — possibly just
`> `, a newline, or some box-drawing characters that have been stripped
to whitespace.

False-positive risk:

- Code blocks in responses contain `>` characters (shell prompts in
  examples, blockquote markers, comparison operators).
- Quoted text has `> ` line prefixes.
- Punctuation and newlines appear constantly.
- Even more distinctive markers like `╰─` can appear inside formatted
  output if Claude renders a box for some other reason.

**Visual pattern matching is the wrong primitive** for this problem.
The signal we want is structurally meaningful ("Claude finished
generating") but visual content alone doesn't carry it reliably.

## 3. Decision

**Tier 1: Stop hooks + sidecar signal.** Use Claude Code's built-in
hook system to programmatically signal turn completion via a side
channel that ccom owns. No visual inference. No regex. Claude Code
itself tells us when it's done.

The current `ResponseBoundaryDetector` (regex-based) stays in the tree
as a fallback for non-Claude runners that don't support hooks (e.g.
`SessionKind::Terminal` shell sessions, future Aider/OpenCode
integrations that have different mechanisms).

Rationale captured below alongside the rejected alternatives.

## 4. Tier 1 — Hook-based detection (chosen)

### 4.1 Architecture

```
spawn(claude session)
    │
    ├── ccom creates per-session config dir at /tmp/ccom-<sid>/.claude/
    │   with a settings.json containing:
    │     hooks.Stop = [{ command: <hook script>, env: {...} }]
    │
    ├── ccom creates per-session sidecar (Unix socket OR FIFO OR file)
    │   at /tmp/ccom-<sid>.sock (or .fifo / .stops)
    │
    ├── ccom spawns Claude Code with:
    │     CLAUDE_CONFIG_DIR=/tmp/ccom-<sid>/.claude
    │     CCOM_SESSION_ID=<sid>
    │     CCOM_SIDECAR_PATH=/tmp/ccom-<sid>.sock
    │
    └── (running)
            │
            user → send_prompt(text) ──> turn_id allocated, prompt sent
            │
            (Claude generates response)
            │
            Claude Code finishes → fires Stop hook
            │
            Stop hook script reads its env and posts to the sidecar:
              `{"session": "<sid>", "ts": <unix>}`
            │
            ccom's sidecar reader thread receives the post,
            forwards to HookBasedBoundaryDetector
            │
            HookBasedBoundaryDetector pairs the post with the most
            recent active turn for this session, completes the turn,
            pushes a StoredTurn into the session's response_store,
            publishes SessionEvent::ResponseComplete on the bus
```

The hook is **the** signal. The PTY is still parsed for display (vt100),
but PTY content is **not** consulted for boundary detection in the
hook-based path.

### 4.2 Per-session config injection

We must install the hook **per ccom session, never globally.** Polluting
the user's `~/.config/claude/settings.json` is unacceptable.

**Approach:** environment variable `CLAUDE_CONFIG_DIR` (or whatever
Claude Code uses to scope its config dir). ccom creates a temp dir
per session, writes a `settings.json` containing only our hook, and
sets the env var when spawning Claude Code.

```rust
// pseudocode in src/session/types.rs::Session::spawn
let config_dir = create_session_config_dir(session_id)?;
write_hook_settings(&config_dir, /* hook command */)?;
cmd.env("CLAUDE_CONFIG_DIR", &config_dir);
cmd.env("CCOM_SESSION_ID", session_id.to_string());
cmd.env("CCOM_SIDECAR_PATH", sidecar_path);
```

**Open question for the spike:** does Claude Code respect a
`CLAUDE_CONFIG_DIR` (or similarly named) env var that scopes ALL its
config to that dir? If yes, this is clean. If no, we either need a
CLI flag, or we have to merge into the user's existing settings.json
(invasive — would need careful merge + restore).

**Open question for the spike:** if the user has their own Stop hook
configured globally, does our per-session settings.json shadow it
entirely (good — clean isolation) or get merged (need to handle the
user's hook + ours together)?

### 4.3 Hook script

The hook needs to be:

- **Tiny** — runs on every Stop event so latency matters.
- **Self-contained** — no external dependencies, no Python/Node.
- **Robust** — must not crash Claude Code if the sidecar is gone.

POSIX shell, ~5 lines:

```bash
#!/bin/sh
# Installed by ccom at <CLAUDE_CONFIG_DIR>/hooks/ccom-stop.sh
# Posts a Stop event to ccom's per-session sidecar.
sidecar="${CCOM_SIDECAR_PATH:-}"
sid="${CCOM_SESSION_ID:-unknown}"
[ -z "$sidecar" ] && exit 0   # not running under ccom, no-op
ts="$(date +%s)"
# Best-effort write — ccom may have already torn down.
printf '{"session":"%s","ts":%s}\n' "$sid" "$ts" \
  | { socat - "UNIX-CONNECT:$sidecar" 2>/dev/null || true; }
exit 0
```

**Open questions for the spike:**
- Does Claude Code accept a hook command as an inline string in
  settings.json, or does it need to point to a script file? If file,
  ccom must write the script to the per-session config dir.
- Does Claude Code wait for the hook to exit before continuing? If
  yes, the hook must be fast (< 100ms). The shell + socat invocation
  is on the order of 10ms on a warm system.
- `socat` is a hard dependency for the script. Alternatives: use `nc`
  (more common but flaky over Unix sockets), or write a tiny Rust
  helper that ccom ships and the hook execs (`ccom-stop-hook`). The
  tiny Rust helper is the most portable.

**Recommended:** ship a tiny `ccom-stop-hook` binary (single fn,
opens the sidecar, writes JSON, exits) and have the hook script just
exec it. Removes the socat/nc/awk dependency entirely. ccom already
has `cargo build` infrastructure for binaries.

### 4.4 Sidecar transport

Three options, in order of preference:

| | Transport | Pros | Cons |
|---|---|---|---|
| **A** | **Unix socket** (`/tmp/ccom-<sid>.sock`) | Async, structured, OS-portable, ccom can accept connections in a dedicated reader thread. Multiple writes per second handled gracefully. | Cleanup needed on session exit. Can't be used across machine boundaries (irrelevant — we're local). |
| **B** | **Named pipe / FIFO** (`/tmp/ccom-<sid>.fifo`) | Simpler than socket — just `mkfifo` and read. Built-in OS support. | Non-blocking reads need care. Only one reader at a time. macOS FIFO behavior differs slightly from Linux. |
| **C** | **Append-only file** (`/tmp/ccom-<sid>.stops`) | Trivially simple. No special syscalls. | Requires polling (latency) OR inotify/FSEvents (platform-specific). |

**Recommended: Unix socket (A).** ccom owns one reader thread per
session-with-sidecar that accepts connections, reads one JSON line,
forwards to the boundary detector via channel, closes. Latency is
sub-millisecond. Cleanup is `unlink(path)` on session exit.

### 4.5 Turn correlation

The hook fires after every Claude Code Stop event but **doesn't know
ccom's TurnId.** Correlation must happen on the ccom side.

The simplest correlation rule that holds:

> The Nth Stop hook fire for session S corresponds to the Nth
> `send_prompt` call for session S.

This holds **as long as every prompt to a session goes through
`send_prompt`.** Currently it does NOT — when the user types directly
into the TUI's session view, keystrokes flow through
`App::handle_session_view_key` → `Session::try_write`, NOT
`send_prompt`. So the turn counter doesn't increment, but Claude Code
still fires a Stop hook on completion.

**Two ways to fix:**

1. **Route all prompts through `send_prompt`**, including user keystrokes
   from the TUI. Architectural change in `App::handle_session_view_key`:
   buffer user keystrokes until Enter, then call `send_prompt(id, text)`.
   Big rewrite of input handling.

2. **Track active turns and silently drop hook fires that don't
   correspond to one.** When a Stop hook fires:
   - If there's an active turn for the session, complete it (the
     "expected" path — a `send_prompt` allocated this turn).
   - If there's NO active turn, ignore the fire (the user typed
     directly and we don't have a turn id to attach).
   - This means user-typed responses don't appear in the response
     store. They flow through the PTY for display but don't get
     `ResponseComplete` events. **Acceptable for v1** — the response
     store is for programmatic consumers (Council, MCP), not for
     direct user interaction.

**Recommended: option 2.** Defer #1 until a phase that explicitly
needs it (Council might).

### 4.6 The `HookBasedBoundaryDetector`

A new type alongside the existing regex-based one:

```rust
// src/pty/hook_boundary.rs
pub struct HookBasedBoundaryDetector {
    /// Per-session active turn state. Mirrors the regex detector's
    /// HashMap, but the body bytes are still accumulated by a
    /// separate component (or by the existing detector running in
    /// parallel for the body capture only — see §4.7).
    active_turns: HashMap<usize, ActiveTurn>,
}

struct ActiveTurn {
    turn_id: TurnId,
    started_at: Instant,
    body_bytes: Vec<u8>,  // accumulated from on_pty_data
}

impl HookBasedBoundaryDetector {
    pub fn on_prompt_submitted(&mut self, session_id: usize, turn_id: TurnId);
    pub fn on_pty_data(&mut self, session_id: usize, data: &[u8]);
    pub fn on_hook_stop(&mut self, session_id: usize, sink: &mut impl TurnSink);
    pub fn forget_session(&mut self, session_id: usize);
}
```

The shape mirrors the existing detector except `check_for_boundary`
is replaced by `on_hook_stop`. The hook reader thread calls
`on_hook_stop` directly when a sidecar message arrives, instead of
the App ticker calling `check_for_boundary` periodically.

### 4.7 Body capture

The hook tells us *when* a response is complete but **not** *what* the
response was. We still need to capture the body bytes from the PTY
between prompt-submit and Stop-hook-fire.

The existing `ResponseBoundaryDetector::on_pty_data` already does
this. We can either:

- Reuse that detector's accumulator (it accumulates bytes regardless
  of whether the regex ever matches) and just override the boundary
  signal source.
- Or write a fresh, simpler accumulator inside
  `HookBasedBoundaryDetector` (essentially a copy).

**Recommended: reuse the existing accumulator.** Refactor: split
`ResponseBoundaryDetector` into a "body accumulator" trait and a
"boundary signal source" — regex source vs hook source. Both sources
share the accumulator. This keeps the code DRY and makes future
runners (Aider, OpenCode) easy to add — just write a new signal
source.

### 4.8 Cleanup

`SessionManager::kill` and `reap_exited` already call
`forget_session` on the regex detector. They'll need to also:

- Call `forget_session` on the hook-based detector.
- Close the sidecar reader thread for that session (channel-shutdown).
- `unlink` the sidecar socket file.
- Recursively remove the per-session `CLAUDE_CONFIG_DIR`.

This is a small extension of the existing cleanup path. Wrap in a
new `Session::cleanup_phase3_artifacts()` method called from kill /
reap.

### 4.9 Fallback to regex detector

For sessions that aren't Claude Code (e.g. `SessionKind::Terminal`),
the hook approach doesn't apply. `SessionManager` picks the detector
based on session kind:

```rust
enum BoundaryDetector {
    Hook(HookBasedBoundaryDetector),
    Regex(ResponseBoundaryDetector),
}
```

Or — cleaner — define a `BoundaryDetectorSource` trait that both
implement, and `SessionManager` holds a `Box<dyn BoundaryDetectorSource>`
or a per-session detector.

For the v1 of this work, we only wire the hook detector for
`SessionKind::Claude` sessions. Terminal sessions have no detector
(no `ResponseComplete` events ever fire for them — they're shell
sessions, not request/response).

### 4.10 Spike plan (research-first)

Before writing any production code, run a spike to verify the core
assumptions. The spike answers:

1. **Config dir scoping.** Does Claude Code respect a
   `CLAUDE_CONFIG_DIR` (or similar) env var that points its config
   reads at a temp dir? Test: create a temp dir with a fake
   `settings.json` containing a Stop hook that writes to a temp
   file. Spawn Claude Code with the env var set. Send a prompt.
   Verify the hook fires AND the user's real config wasn't touched.

2. **Stop hook firing reliability.** Does the Stop hook fire on
   every response, or only some? Test scenarios:
   - Plain text response (no tool use)
   - Response that uses one tool then finishes
   - Response that uses tools in a loop and finishes
   - Response interrupted by user (ESC)
   - Response that triggers an error
   For each, verify exactly one Stop hook fire.

3. **Hook command format.** Inline command string in settings.json
   vs script file vs other? Capture an example settings.json that
   Claude Code accepts.

4. **Hook latency.** How long does Claude Code wait for the hook to
   exit before continuing? Test with a hook that sleeps 1s, 5s,
   30s. Determine the safe upper bound.

5. **Hook environment.** What env vars / stdin does Claude Code pass
   to the hook? Document them. (Especially: does it pass any
   correlation id we could use instead of order-based correlation?)

6. **Per-session isolation.** Run two ccom sessions concurrently
   with different `CLAUDE_CONFIG_DIR` values. Verify each gets its
   own hooks and they don't cross-contaminate.

The spike output is a short doc — `docs/plans/notes/hook-spike.md`
— with the empirical findings and a go/no-go recommendation.

**Spike effort:** ~1–2 hours including write-up.

### 4.11 Phased implementation plan (after spike)

**Phase 3.5.A — Spike (1–2 hours)**
- Read Claude Code hook docs
- Build a minimal scratch project that exercises Stop hooks
- Document findings in `docs/plans/notes/hook-spike.md`
- Go/no-go decision

**Phase 3.5.B — Sidecar infrastructure (~3 hours)**
- Add a small `ccom-stop-hook` helper binary
- Add `Sidecar` type that owns a Unix socket reader thread
- Add `CCOM_SIDECAR_PATH` env var injection in `Session::spawn`
- Unit tests for sidecar message round-trip (mock hook → sidecar → channel)

**Phase 3.5.C — `HookBasedBoundaryDetector` (~2 hours)**
- New type in `src/pty/hook_boundary.rs`
- Reuse the existing accumulator via refactor (extract `BodyAccumulator`)
- Wire `on_hook_stop` to push completed turns
- Unit tests using fake hook signals

**Phase 3.5.D — Per-session config injection (~2 hours)**
- `create_session_config_dir(session_id)` helper
- Write `settings.json` with our Stop hook
- `Session::spawn` integration: create dir, write settings, set env vars
- Cleanup hooks in `kill` / `reap_exited`

**Phase 3.5.E — Wiring + replacement (~1 hour)**
- `SessionManager` holds the hook-based detector for Claude sessions
- `for_claude_code()` gets removed (or marked deprecated)
- The placeholder regex is gone
- Real-Claude smoke test verifies `ResponseComplete` fires end-to-end

**Phase 3.5.F — Documentation (~1 hour)**
- Update `docs/designs/session-management.md` §4 with the new path
- Update `docs/pr-review-pr9.md` to mark the limitation resolved
- Update `docs/plans/session-management-phase-1-3.md` Phase 3 to point
  here

**Total estimate: ~10 hours of focused work**, gated on the spike
returning green.

---

## 5. Tier 2 — vt100 cursor position + idle timer (rejected alternative)

**Documented for posterity.** Considered but not chosen.

### Approach

Instead of regex-matching screen content, peek at the vt100 parser's
**cursor state**. The parser already tracks `(row, col)` for the
visible cursor. When Claude Code is at its input prompt, the cursor
lands at a specific (row, col) position — typically inside its input
text area.

The detector becomes:

```text
fn check_for_boundary(session, sink):
    if no active turn: return
    cursor = session.parser.cursor_position()
    last_byte_arrival = self.last_data_at[session_id]
    if cursor matches idle_template AND
       elapsed_since(last_byte_arrival) > IDLE_THRESHOLD_MS:
        complete the turn
```

The `idle_template` would be something like `(row >= total_rows - N,
col within input area bounds)` — a structural cursor location, not a
visual content match.

### Pros

- **Structurally meaningful.** Cursor position carries the same
  information a human watching the screen would use.
- **Robust to response content.** No false-positive risk from `>`
  characters in code blocks.
- **No external infrastructure.** Pure ccom-side change. No hooks,
  no sidecars, no per-session config.
- **Generalizes within ccom's existing stack.** Only requires the
  vt100 parser ccom already uses.

### Cons

- **Still empirical.** We have to capture the cursor template for
  Claude Code at least once, and re-capture if Claude Code's UI
  changes.
- **Sensitive to terminal width.** The input box might wrap
  differently at different widths, changing the cursor's resting
  position.
- **Sensitive to mid-response cursor movement.** Claude Code might
  reposition the cursor for spinner updates or status lines mid-
  generation, causing false positives.
- **Doesn't generalize across runners** — every CLI agent has its own
  UI layout.
- **Idle threshold tuning.** Too short → false positives during
  natural pauses (Claude thinking). Too long → user-perceived lag
  before Council/MCP sees a `ResponseComplete`.

### Why rejected

The hook-based approach is strictly more robust (no empirical
calibration) and strictly more general (works for any runner with a
hook system). The cursor approach is the **right fallback** if the
hook spike fails — it's better than regex matching but worse than
hooks. We hold it in reserve.

---

## 6. Tier 3 — Match raw bytes including ANSI (rejected alternative)

**Documented for posterity.** Considered but not chosen.

### Approach

Don't strip ANSI before matching. Match the regex against the raw
bytes (or against a UTF-8 string preserving ANSI escapes). Cursor-
positioning escapes like `ESC[24;3H` (move to row 24, col 3) are
distinctive sequences unlikely to appear inside response content. The
ANSI strip pass still runs, but only for the **stored** body — the
detector matches against the unstripped form.

This is the **smallest change** to the current Phase 3 code — just
swap which buffer the regex matches against.

### Pros

- **Smallest diff.** Changes only how the regex source is built.
- **Distinctive escape sequences** are much less likely to false-
  positive than plain text.
- **Empirical capture is still needed**, but the captured bytes are
  much more unique than ANSI-stripped text.

### Cons

- **Still visual inference.** Claude Code can re-emit cursor-
  positioning sequences mid-response (spinner updates, status line
  redraws), causing false positives.
- **Brittle to UI changes.** Any redesign that changes the escape
  sequences breaks the detector silently.
- **Doesn't generalize.** Same fragility as cursor position, with
  more pattern-matching ceremony.
- **No structured signal.** It's the same fundamental approach as
  Tier 0 (the current placeholder), just with a more distinctive
  pattern. The architectural problem isn't solved.

### Why rejected

This is the cheapest change but also the least robust answer. It
trades implementation cost for ongoing maintenance burden. The hook
approach (Tier 1) costs more upfront but produces a foundation that
doesn't require empirical re-tuning when Claude Code's UI changes.

---

## 7. Open questions (post-spike)

These are flagged in the Tier 1 spec above but listed here for
visibility:

1. **`CLAUDE_CONFIG_DIR` env var:** does Claude Code respect one,
   and is it scoped to all config reads?
2. **Stop hook event coverage:** does it fire on every response or
   only some?
3. **Hook command format:** inline string or script file path?
4. **Hook latency tolerance:** how long before Claude Code times
   out the hook and proceeds anyway?
5. **Hook environment:** what env vars / stdin does Claude Code
   pass that we could use for correlation?
6. **User-typed prompts:** how do we handle the asymmetry between
   `send_prompt`-driven turns (have a TurnId) and TUI-keystroke-driven
   turns (don't)?
7. **Per-session config isolation:** verified clean across concurrent
   sessions?

## 8. References

- `docs/designs/session-management.md` §4 — the original Phase 3
  design that introduced the placeholder.
- `docs/pr-review-pr9.md` — the limitation this doc resolves.
- `docs/plans/session-management-phase-1-3.md` Phase 3 — the
  implementation plan that landed Tier 0.
- Claude Code hooks documentation: TBD via spike.
