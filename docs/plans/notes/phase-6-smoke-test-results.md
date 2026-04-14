# Phase 6 Task 10 — Smoke Test Results

**Date:** 2026-04-14
**Branch:** `session-mgmt/phase-6-tasks-8-to-10`
**Build:** `cargo build --release`, 267 tests passing
**Claude Code version:** 2.1.105

---

## Checklist

| # | Item | Result |
|---|------|--------|
| 1 | Silent spawns — budget=3, no TUI modal for any of the 3 spawns | **PASS** |
| 2 | Session tree rendering — `◆` driver prefix, `[driver · budget 0]` suffix, `└─` worker rows indented | **PASS** |
| 3 | Subscribe push vs poll | **POLL — see §Critical finding** |
| 4 | Aggregation output — correct deduplicated JSON array | **PASS** |
| 5 | Attach flow | not tested (optional) |
| 6 | Kill flow — worker-2 silent kill, session 9999 returns "not found" | **PASS** |
| 7 | Orphan rendering — workers go flat after driver kill | **PASS** |
| 8 | Log file — useful per-tool-call entries | **SPARSE — see §Logging gap** |

---

## Driver's final answer

```
["app", "claude", "driver_config.rs", "event.rs", "fs", "lib.rs",
 "main.rs", "mcp", "pty", "session", "setup.rs", "ui"]
```

12 entries. All three workers returned identical arrays — no differences
to reconcile. Correct against the repo's actual `src/` layout.

---

## Critical finding — item 3 (subscribe push vs poll)

The driver did **not** call `subscribe`. After spawning the three
workers it called `read_response` three times in parallel:

> "Workers spawned (IDs 2, 3, 4). Reading their responses in parallel.
> Called ccom 3 times."

This is a **pull-based** approach. The driver blocked on `read_response`
for each worker concurrently rather than reacting to push notifications.

**Phase 7 implication:** Task 7 (`list_pending_approvals` poll tool) is
**not conditional** — it is required. The push path (MCP
`notifications/message` surfacing into the driver's LLM context) was not
used by the driver in this run. Phase 7 must provide a pull-based
approval query mechanism from the start rather than treating it as a
fallback.

Phase 7 plan has been noted accordingly (Task 7 remains in the breakdown
and should be implemented alongside Task 6, not gated on manual
verification).

---

## Logging gap — item 8

The log file contained only startup entries and rmcp `serve_inner`
spans:

```
[INFO  ccom] ccom starting; log file: ...
[INFO  ccom::app] ccom-mcp server listening on 127.0.0.1:55138
[INFO  ccom::app] promoting next Claude spawn to driver role: Driver { spawn_budget: 3, spawn_policy: Budget }
[INFO  tracing::span] serve_inner;   (×6)
```

No per-tool-call log lines (`spawn_session`, `kill_session`,
`send_prompt`, `read_response`) appeared at `info` level. The MCP
handlers do not emit structured log entries on invocation.

**Action item for Phase 7:** add `log::info!` entries at the start of
each MCP tool handler (tool name + caller id + key args). Approval
routing will be difficult to debug without per-call visibility in the
log.

---

## Bug found — `initial_prompt` race condition

Workers 1 and 2 had their prompt text visible in the input box but
**not submitted**. Only worker-3 auto-submitted. Root cause: the MCP
`spawn_session` handler calls `mgr.send_prompt` immediately after
`spawn` — before the new Claude process has initialized and rendered
its input prompt. The text is written to the PTY but the submit
sequence (`\r`) fires before Claude is ready to accept it.

Worker-3 submitted correctly, likely because by the time the third
`spawn_session` call was processed the first worker was already running
and the system was under less contention.

**Fix:** `spawn_session` should wait for the first idle signal from the
new session before sending the `initial_prompt`. The response boundary
detector already tracks this; we just need to await it before writing.

This bug is being fixed before the Phase 6 PR is merged.

---

## Observations

- The driver orchestrated three workers entirely autonomously without
  human intervention.
- Budget policy enforced correctly — zero modals for the three allowed
  spawns.
- Session tree rendered exactly as designed throughout the test.
- Kill and orphan flows both worked without prompting or errors.
- The attach flow was not exercised in this run; it remains untested
  end-to-end with a real Claude session.
