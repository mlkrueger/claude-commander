# Phase 6 Task 10 — Manual End-to-End Smoke Test

**Status:** Pending manual execution.
**Branch to run from:** `session-mgmt/phase-6-tasks-8-to-10` (or whatever
branch currently holds merged Phase 6 Tasks 1–9).
**Time estimate:** 15–30 minutes (most of it watching a real Claude session
work through an orchestration task).

This doc is the entire smoke test script — self-contained, so you can run it
cold without re-reading the Phase 6 planning conversation. It captures what
to run, what to watch for, and what to write down afterward.

---

## Preflight

Before starting the TUI:

1. **Branch check.** You should be on the branch containing all Phase 6
   tasks:
   ```bash
   cd /Users/mkrueger/dev/claude-commander
   git checkout session-mgmt/phase-6-tasks-8-to-10
   git log --oneline -10
   ```
   Confirm the log shows commits for Tasks 1–9 including
   `phase 6 task 8` and `phase 6 task 9`.

2. **Clean build.**
   ```bash
   cargo build --release 2>&1 | tail -5
   cargo test 2>&1 | grep "test result" | head -3
   ```
   Expect: clean build, ~200 lib / ~217 bin tests passing.

3. **Clean state.** Make sure no other ccom is running:
   ```bash
   pgrep -fl ccom || echo "no ccom running"
   ```

4. **Have a second terminal ready** for watching the log file. Phase 6
   redirects all TUI logs to `/tmp/ccom-<pid>.log` (see the log redirection
   in `src/main.rs`). You'll want `tail -F /tmp/ccom-*.log` running in a
   second shell before you start the TUI.

---

## Launch

```bash
cargo run --release -- --driver --spawn-policy budget --budget 3
```

**Expected at launch:**
- The TUI starts clean
- The session list panel is empty (no `--spawn N` flag was passed)
- The MCP server logs a line like `ccom-mcp server listening on 127.0.0.1:NNNN`
  (visible in the log file from the second terminal, not in the TUI)
- No sessions exist yet — you need to spawn the driver

**Important nuance on `--driver`:** The flag only queues the driver role to
be applied to the **first Claude session spawned**. It doesn't itself spawn
anything. You have to create the driver session manually with the new-session
key.

From the session list, press `n` to open the new-session modal and spawn a
Claude session. This first session will automatically be promoted to
`SessionRole::Driver { spawn_budget: 3, spawn_policy: Budget }` because of
the queued role from `--driver`.

Verify in the session list panel:
- The driver row shows a `◆ ` prefix
- The row label has a suffix like `[driver · budget 3]`
- If you open the driver in the session view (Enter), the title bar also
  shows `[driver · budget 3]`

If those markers are missing, Phase 6 Task 7 didn't wire up correctly.
Note it and stop — don't continue the smoke test until the tree
rendering is visible.

---

## The orchestration prompt

Switch into the driver's session view (Enter on the driver row) and paste
this prompt verbatim:

> You have access to ccom MCP tools including `spawn_session`, `send_prompt`,
> `read_response`, `list_sessions`, `subscribe`, and `kill_session`. I want you
> to orchestrate three helper sessions. For each, call `spawn_session` with a
> label (`worker-1`, `worker-2`, `worker-3`) and an `initial_prompt` that asks
> the worker to run `ls src/` in the current repo and reply with ONLY a JSON
> array of the top-level entries it sees (no prose, just the array). Then
> read each worker's completed turn and aggregate the three arrays into a
> single merged deduplicated list. Return that merged list to me as your
> final answer.

---

## What to watch for

### 1. Silent spawns (Budget policy, budget = 3)

- **Expected:** All three `spawn_session` calls execute silently. No TUI
  modal pops up. Each call appears in the log as
  `spawn_session(<driver_id>)` or similar.
- **After the third spawn, the budget should be 0.** If the driver tries a
  fourth `spawn_session` during the same run, THAT call should trigger a
  TUI modal (the fallback-to-Ask policy when budget is exhausted).
- **What to report:** any spawn that triggered a modal unexpectedly. Paste
  the exact text of the modal prompt.

### 2. Session tree rendering

After the three workers spawn, the session list should look roughly like:

```
  ◆ claude-1      [driver · budget 0]  ...
    └─ worker-1                        (parent: claude-1)
    └─ worker-2                        (parent: claude-1)
    └─ worker-3                        (parent: claude-1)
```

- Driver row: `◆ ` prefix, accent color, `[driver · budget 0]` suffix in dim
- Child rows: 2-space indent, `└─ ` prefix, ` (parent: <driver_label>)` in dim
- **Report:** exact layout if it differs. Screenshot or paste the visible
  session list region.

### 3. `subscribe` push-vs-poll behavior (CRITICAL — informs Phase 7)

This is the single biggest thing the smoke test can teach us. Phase 6's
`subscribe` tool emits `SessionEvent::ResponseComplete` events as MCP
`notifications/message` entries. Phase 7's approval-routing design assumes
Claude Code surfaces those notifications into the driver's context so the
driver reacts to events without polling. That assumption has never been
verified end-to-end.

In this smoke test, watch the driver's behavior closely:

- **Did the driver call `subscribe` at the start?** If yes, note the
  arguments.
- **After `spawn_session` calls, did the driver wait on `read_response`
  with long timeouts (pull-based)**, or did it do something that looks
  like it reacted to incoming events (push-based)?
- **How did the driver know when each worker finished?** The most telling
  signal: if the driver calls `read_response(worker_id, turn_id, timeout=60)`
  and blocks, that's the pull path. If it calls `read_response` immediately
  after the worker finishes without waiting, that means it got a push
  notification — Phase 7's design is viable as-specified.

**Report this explicitly** — even one sentence: "driver polled each worker
sequentially with long-poll `read_response`" OR "driver subscribed and
reacted to events as they arrived." This is Phase 7's go/no-go signal on
whether subscribe pushes actually reach the driver's context.

### 4. Aggregation output

- **Expected:** the driver's final message contains a JSON array that
  merges the three workers' `ls src/` results with duplicates removed.
  Entries should include things like `["app", "claude", "driver_config.rs",
  "event.rs", "fs", "main.rs", "mcp", "pty", "session", "setup", "ui"]` —
  i.e., the current top-level contents of `src/`.
- **Report:** the actual output, plus any confusion / hallucinated entries
  / truncation.

### 5. Attach flow (optional but recommended)

Once the driver has finished its task, press Esc to return to the session
list. Make sure the driver session is NOT selected (select a child or
spawn a fresh solo session with `n`). With that session selected, press
`a`.

- **Expected:** a compact overlay appears listing live drivers. There
  should be one entry (the driver you just used) with a `◆ ` prefix.
  Use arrows to select it and press Enter.
- **Expected:** a status message appears: "Attached session N to driver
  claude-1" (or whatever the driver's label is).
- **Expected:** the session list re-renders with the just-attached session
  as a child of the driver, prefixed with `↪ ` (the attached icon, distinct
  from `└─ ` for spawned children).
- **Report:** anything that doesn't match. Try pressing `a` with the driver
  itself selected — what happens? (It should attach the driver to itself,
  which is probably meaningless; note the UX.)

### 6. Kill flow

Ask the driver in chat:

> Now kill worker-2 using the `kill_session` tool.

- **Expected:** the child session `worker-2` disappears from the list with
  NO TUI modal (silent kill because the driver owns the child).
- **Report:** any modal that pops. That would indicate the driver-silent
  kill-policy branch from Task 6 didn't fire.

Now ask the driver:

> Try to kill session <some-id-NOT-owned-by-you> using kill_session.

(Pick a session id that exists but isn't one of the driver's children —
e.g., a fresh solo session you spawn, or just pick id `9999` which almost
certainly doesn't exist.)

- **Expected:** the tool returns an error result containing "not found"
  (the `Scope::Restricted` miss branch).
- **Report:** the exact error text.

### 7. Orphan rendering

Manually kill the driver itself from the session list. In the TUI, select
the driver row and press `k` (or whatever kill-key the keybindings use;
check with `?`).

- **Expected:** the driver row disappears. The two remaining workers
  (worker-1 and worker-3 after the earlier kill) should now render at the
  top level as flat rows, no longer indented under anything. They stay
  alive — the orphan rendering path from Task 8 handles this.
- **Report:** whether the orphans render correctly, and whether they
  continue to respond (they should — they're independent Claude processes).

### 8. Log file

After the test, grab the log file:
```bash
ls -lt /tmp/ccom-*.log | head -3
cat /tmp/ccom-<newest-pid>.log | grep -E "spawn_session|kill_session|caller_scope|pending_confirm|error|warn" | tail -100
```

**Report:** anything with `error` or `warn` level, plus the first 20
`spawn_session` / `kill_session` entries.

---

## Writing up the results

Come back to the conversation with:

1. **Pass/fail per numbered item (1–8)** above. A simple checklist is fine.
2. **The driver's actual final answer** — paste it.
3. **The `subscribe`-vs-polling observation** (item 3) — this is the most
   important data point we don't have yet.
4. **Any crashes, panics, or error-level log entries** — paste backtraces
   in full.
5. **Any UX surprises** — even if they don't break anything, note what
   felt wrong.

I'll then write the findings into a proper notes file
(`docs/plans/notes/phase-6-smoke-test-results.md`), mark Phase 6 Task 10
done, and open the Phase 6 PR into main.

---

## Troubleshooting

- **Driver doesn't see `spawn_session` in its tool list** — the `.mcp.json`
  wasn't loaded. Check the log for `.mcp.json` errors. Verify the hook dir
  was created (`ls /tmp/ccom-hook-*`). The per-session hook dir should
  contain a `.mcp.json` with the `X-Ccom-Caller` header.
- **Every `spawn_session` triggers a modal even though budget is 3** — the
  `--driver --spawn-policy budget --budget 3` flags didn't resolve into
  `pending_driver_role`, or the first-Claude-spawn promotion in
  `spawn_session_kind` didn't fire. Check the log for
  `promoting session N to driver role`. If missing, Task 2's wiring is
  broken.
- **MCP tool calls all fail with "no mcp_port"** — `Ccom::new_with_port`
  isn't receiving the port. Check `src/mcp/server.rs::run_server`.
- **The TUI corrupts / flickers during the test** — that's a rendering
  issue, not a Phase 6 bug; known tradeoff with TUI + PTY output. Try
  `--release` if you're on `--debug`.
- **Subscribe events never arrive (driver has to poll)** — NOT a test
  failure, but it's the key signal for Phase 7. Note explicitly.

---

## Why this test matters for the roadmap

- **Phase 6 merge blocker:** this is the last item before merging Phase 6
  to main. All 267 tests passing is necessary but not sufficient — the
  end-to-end UX with a real Claude session is the final gate.
- **Phase 7 go/no-go:** item 3 (subscribe push vs poll) directly decides
  whether Phase 7's notification-driven approval routing is viable. If
  the driver had to poll for `ResponseComplete`, Phase 7 needs to route
  approvals through a pull-based tool instead of via subscribe events —
  and the plan doc from the Phase 7 subagent pass has to be revised.
- **Phase 8 dependency check:** item 8 (log file) should show the Claude
  session UUID in the Stop hook JSON. If it does, UUID capture has a
  clean path. If it doesn't, Phase 8 (session group restoration) needs
  a fallback capture mechanism — and the UUID capture subagent work
  that's running in parallel will surface the gap.
