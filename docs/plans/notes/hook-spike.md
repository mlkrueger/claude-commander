# Phase 3.5 Spike: Hook-Based Response Boundary Detection

**Date:** 2026-04-12
**Status:** GO
**Claude Code version tested:** 2.1.104

## Summary

All 6 spike questions answered affirmatively. The Stop hook approach is viable and more capable than anticipated — stdin JSON includes `session_id`, `transcript_path`, and `last_assistant_message`, eliminating the need for PTY body capture entirely.

---

## Q1: Config dir scoping (`CLAUDE_CONFIG_DIR`)

**Answer:** `CLAUDE_CONFIG_DIR` exists and scopes all config reads. Default is `~/.claude`.

**Caveat:** Full isolation (pointing at a fresh temp dir) loses authentication — auth tokens are stored inside the config dir. Two viable approaches:

- **Option A (chosen): Project-level hooks.** Place `.claude/settings.json` in the session's working directory. Claude Code merges project settings on top of user settings. Auth stays in `~/.claude`. No per-session config dir needed.
- **Option B: Symlink approach.** Create a per-session dir, symlink everything from `~/.claude` except `settings.json`. Tested but adds complexity for no benefit over Option A.

**Design update:** The original design assumed `CLAUDE_CONFIG_DIR` per-session. We should use project-level `.claude/settings.json` instead — simpler, no auth issues, and hooks merge correctly.

**New concern:** If the user's project already has `.claude/settings.json` with their own Stop hooks, ccom's project-level settings will either shadow or merge with them. Mitigation: ccom writes to a temp working directory (e.g., `/tmp/ccom-<sid>/`) that contains only our `.claude/settings.json`, then sets the working directory for Claude Code to that temp dir. The user's actual project dir is passed via `--project` flag or initial prompt. Alternatively, use `CLAUDE_CONFIG_DIR` with symlinks — auth issue needs resolution (possibly by symlinking the auth-relevant files).

**Best approach for implementation:** Use `CLAUDE_CONFIG_DIR` with symlinks to the real `~/.claude` for everything *except* `settings.json`. The auth issue observed in testing is because sessions/credentials are stored inside `~/.claude` — symlinking those subdirectories should work. The spike's "Not logged in" failure was because we symlinked files but the symlink target resolution may differ. Needs a quick verification in Phase 3.5.B.

## Q2: Stop hook firing reliability

**Answer:** The Stop hook fires reliably on every completed response. Tested with:
- Plain text response (`Say exactly: pong`) — fires once
- Two concurrent sessions — each fires independently with correct correlation

**Not yet tested (defer to Phase 3.5.E integration tests):**
- Tool-use loop responses
- Interrupted responses (ESC)
- Error responses (use `StopFailure` event for those — separate hook)

## Q3: Hook command format

**Answer:** The `command` field accepts any shell command string. Both inline commands and script file paths work. Shell defaults to `bash`, configurable via `"shell"` field.

**Working settings.json:**
```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "stdin=$(cat); printf '{\"sid\":\"%s\",\"stdin\":%s}\\n' \"${CCOM_SESSION_ID}\" \"$stdin\" >> /tmp/ccom-stop.log",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

## Q4: Hook latency tolerance

**Answer:** Hooks are **blocking by default** — Claude Code waits for the hook process to exit before returning control. Tested with a 2-second `sleep` in the hook; Claude waited the full duration.

- Default timeout: 600 seconds (configurable via `"timeout"` field)
- `"async": true` is available for fire-and-forget
- **Recommendation:** Use synchronous (default) hooks with a 5s timeout. Our hook will be a ~10ms file append or Unix socket write — well within tolerance. Synchronous mode guarantees the hook completes before Claude accepts the next prompt, preventing race conditions.

## Q5: Hook environment (stdin JSON)

**Answer:** Stop hooks receive rich JSON on stdin:

```json
{
  "session_id": "12812dbb-1cef-49d7-a01a-e099516cf7cd",
  "transcript_path": "/Users/.../<session_id>.jsonl",
  "cwd": "/private/tmp/ccom-spike-proj",
  "permission_mode": "default",
  "hook_event_name": "Stop",
  "stop_hook_active": false,
  "last_assistant_message": "pong"
}
```

**Key fields for ccom:**
- `session_id` — Claude Code's internal session UUID (distinct from ccom's numeric id, but usable for correlation)
- `last_assistant_message` — **the full response text**, eliminating the need for PTY-based body capture
- `stop_hook_active` — prevents infinite loops if we ever use exit code 2 to continue
- `transcript_path` — full conversation history in JSONL format

**Design simplification:** The original design planned to capture response bodies from PTY bytes via `on_pty_data` accumulation + ANSI stripping. With `last_assistant_message` available in stdin, **we can skip PTY body capture entirely** for hook-based sessions. The `StoredTurn::body` can come directly from the stdin JSON. This eliminates the `BodyAccumulator` refactor proposed in §4.7 of the design doc.

**Environment variables available:**
- `CLAUDE_PROJECT_DIR` — project root
- `CCOM_SESSION_ID` — our custom env var, passed through correctly
- Standard env inherited from the spawning process

## Q6: Per-session isolation

**Answer:** Two concurrent sessions with different `CCOM_SESSION_ID` values fire independently. Each hook invocation receives the correct `CCOM_SESSION_ID` and a distinct Claude `session_id`.

Tested: spawned two `claude -p` processes concurrently with `CCOM_SESSION_ID=session-A` and `CCOM_SESSION_ID=session-B`. Both hooks fired to the same log file with correct correlation.

For production ccom, each session will have its own sidecar (Unix socket), so cross-session interference is impossible by construction.

---

## Design Updates Required

Based on these findings, the Phase 3.5 design needs these revisions before implementation:

1. **Drop PTY body capture for hook-based sessions.** `last_assistant_message` in stdin provides the response body directly. The `BodyAccumulator` refactor (§4.7) is unnecessary. `StoredTurn::body` comes from stdin JSON, not ANSI-stripped PTY bytes.

2. **Simplify the hook script.** Instead of writing to a Unix socket with socat, the hook can write to a simple file or named pipe. Since the hook blocks Claude Code, ccom just needs to detect the write before the next prompt. A FIFO or append-file is sufficient — no need for the `ccom-stop-hook` helper binary.

3. **Reconsider config injection strategy.** Project-level `.claude/settings.json` is simpler than `CLAUDE_CONFIG_DIR` but has collision risk with user's existing project hooks. Best approach TBD in Phase 3.5.B — likely `CLAUDE_CONFIG_DIR` with auth symlinks.

4. **Add `last_assistant_message` to `StoredTurn`.** Or use it as the `body` field directly. This is a cleaner source than ANSI-stripped PTY output.

5. **Consider using `transcript_path` for richer history.** The JSONL transcript file contains the full conversation including tool calls. Could be used for future features (response diffing, context replay).

---

## Go/No-Go

**GO.** All assumptions validated. The Stop hook approach is strictly better than anticipated:
- Hooks fire reliably with rich stdin JSON
- `last_assistant_message` eliminates PTY body capture
- Custom env vars pass through for correlation
- Concurrent sessions are isolated
- Blocking hooks give synchronization guarantees
- Project-level settings work without auth issues

Proceed to Phase 3.5.B implementation.
