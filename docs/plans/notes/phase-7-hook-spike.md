# Phase 7 Task 0 — PreToolUse Hook Viability Spike

**Branch:** `session-mgmt/phase-7-hook-spike`
**Status:** PARTIAL — PreToolUse works for synchronous gating; "allow always"
needs a ccom-owned state file, not Claude's permission system.
**Date:** 2026-04-13
**Claude Code version tested:** 2.1.105

## Question

Phase 7 wants ccom to intercept every tool call a managed Claude session
makes and route it through a central approval UI (allow once / deny once
/ allow always / deny always). The proposed mechanism is a PreToolUse
hook configured in every spawned session's `.claude/settings.json` (or
`--settings` JSON), pointing at a per-ccom-session hook binary that
talks to the ccom daemon over a Unix socket.

Six load-bearing assumptions need to hold for the Option E design to be
viable:

1. PreToolUse is **synchronous** — Claude Code blocks on the hook's exit
   before proceeding.
2. The decision protocol has an unambiguous allow/deny contract.
3. The hook receives structured input that identifies the session, the
   tool, and the arguments.
4. A session-mid update to `settings.json` (e.g. writing a new allow
   rule) takes effect on the NEXT tool call without restart, so that
   "allow always" skips the hook on subsequent matching calls.
5. The hook's timeout is either unbounded or large/configurable.
6. The hook fires **per tool call**, not per batch.

## Approach

1. Read the official Claude Code hooks reference at
   `https://code.claude.com/docs/en/hooks` and the permissions reference
   at `https://code.claude.com/docs/en/permissions`. Quote verbatim.
2. Write scratch hook scripts in `/tmp/ccom-phase7-spike/` and drive a
   real Claude subprocess via `claude -p --settings '<json>' ...` so the
   spike doesn't touch the repo tree or `.claude/` config. All testing
   used `claude` 2.1.105, non-interactive (`-p`) mode, on darwin.
3. Exercise: (a) the allow path with a 2s sleep to prove blocking, (b)
   the deny path via `hookSpecificOutput.permissionDecision: "deny"`,
   (c) a hook that rewrites the settings file between calls to test
   live-reload, (d) a PermissionRequest hook with `updatedPermissions`
   `addRules`/`destination: session` to see whether session-scoped
   allow rules are reachable from `-p` mode.

All scratch artifacts live in `/tmp/ccom-phase7-spike/` and are
disposable.

## Findings

### Q1. Is PreToolUse synchronous? — **YES, verified.**

Docs verbatim (`hooks` reference):

> PreToolUse runs **after Claude creates tool parameters and before
> processing the tool call**. It matches on tool name and allows you to
> allow, deny, ask, or defer the tool call.

Empirical: a hook that `sleep 2`s before `exit 0` pushes the end-to-end
`claude -p` run from ~14s to ~16.5s on a one-tool-call prompt, and the
tool output appears only after the hook returns. Confirmed blocking.

### Q2. Decision protocol — **verified, documented, well-shaped.**

Docs verbatim:

> - **Exit 0**: Success. Claude Code parses stdout for JSON output
>   fields. Proceeds with the tool call unless JSON specifies otherwise.
> - **Exit 2**: Blocking error. Ignores stdout/JSON. Stderr is fed to
>   Claude. **Blocks the tool call**.
> - **Any other exit code**: Non-blocking error. Shows first line of
>   stderr in transcript, continues execution.

JSON output shape for PreToolUse uses `hookSpecificOutput`:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow|deny|ask|defer",
    "permissionDecisionReason": "...",
    "updatedInput": { "...": "..." },
    "additionalContext": "..."
  }
}
```

Precedence when multiple hooks match: `deny > defer > ask > allow`.
Top-level `decision: approve|block` is deprecated but still works.

Empirical: a hook returning `{"hookSpecificOutput":{"permissionDecision":
"deny", ...}}` aborts the tool call — Claude prints `Done.` and never
retries. Verified.

**Very important caveat (from the permissions reference, verbatim):**

> Hook decisions do not bypass permission rules. Deny and ask rules are
> evaluated regardless of what a PreToolUse hook returns, so a matching
> deny rule blocks the call and a matching ask rule still prompts even
> when the hook returned "allow" or "ask". ... A blocking hook also
> takes precedence over allow rules. A hook that exits with code 2 stops
> the tool call before permission rules are evaluated, so the block
> applies even when an allow rule would otherwise let the call proceed.

Practical meaning for Phase 7: the hook can deny calls that settings
would allow, but it can't force a call through a `permissions.deny`
rule. That's fine — ccom owns the settings block for managed sessions.

### Q3. Structured input — **fully verified.**

Docs verbatim (PreToolUse stdin schema):

```json
{
  "session_id": "abc123",
  "transcript_path": "/path/to/transcript.jsonl",
  "cwd": "/current/working/directory",
  "permission_mode": "default",
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "npm test" },
  "tool_use_id": "toolu_01ABC123..."
}
```

Empirical stdin captured from a real run:

```json
{"session_id":"8eb29cb3-6f45-4a0a-b720-074b6475c6b5",
 "transcript_path":"/Users/mkrueger/.claude/projects/-private-tmp-ccom-phase7-spike/8eb29cb3-6f45-4a0a-b720-074b6475c6b5.jsonl",
 "cwd":"/private/tmp/ccom-phase7-spike",
 "permission_mode":"default",
 "hook_event_name":"PreToolUse",
 "tool_name":"Bash",
 "tool_input":{"command":"echo hello-phase7","description":"Print hello-phase7"},
 "tool_use_id":"toolu_016ryXZxHgYZYvVcXshLofDe"}
```

Env vars set for the hook process:

```
CLAUDE_CODE_ENTRYPOINT=cli
CLAUDE_PROJECT_DIR=/private/tmp/ccom-phase7-spike
CLAUDE_CODE_EXECPATH=/Users/mkrueger/.local/share/claude/versions/2.1.104
CLAUDECODE=1
```

Everything Phase 7 needs is available:

- `session_id` — the Claude Code session UUID. **This closes the
  alternate-path question from the Stop-hook UUID-capture spike: the
  PreToolUse hook already has the session UUID on stdin on the very
  first tool call, so ccom does not need the Stop-hook scrape for
  routing.** The Stop-hook parser is still useful for other reasons
  (transcript replay), but caller-id for approval routing can come from
  this stdin field directly.
- `cwd` — lets the hook locate the per-ccom-session socket by convention.
- `tool_name` + `tool_input` — full tool-call context for rendering the
  approval prompt to the user.
- `tool_use_id` — stable per-call identifier for correlating a hook
  invocation with a daemon-side approval request.
- `transcript_path` — cross-check for session identity and replay.

### Q4. Live settings.json reload — **NO, verified empirically.**

Docs do not promise this. Empirical test:

- Start `claude -p` with a `--settings` JSON that registers a
  PreToolUse hook for `Bash` and has NO `permissions.allow` entry.
- The hook, on its first invocation, rewrites
  `settings-reload.json` to add `"permissions":{"allow":["Bash(echo *)"]}`
  in addition to keeping the hook block.
- Prompt asks Claude to run two `echo` commands in one turn.
- **Result: the hook fires twice.** The written-mid-session allow rule
  is NOT picked up by the already-running Claude Code process.

Hook invocation log confirms two entries with the same `session_id`
(`ede09f42-7856-44f4-8b83-063d37ed8705`), one per tool call.

This kills the naive "write an allow rule, skip the hook next time"
approach for "allow always". Phase 7 has three recoveries:

1. **Ccom-owned state file, hook re-reads it every call.** Cheapest.
   Per-session JSON (or even SQLite) at a predictable path. On "allow
   always", ccom daemon writes a rule to the state file. The hook
   reads the state file on every call and short-circuits to `exit 0`
   (or a trivial allow JSON) without round-tripping to the daemon when
   a matching rule exists. Skips the RPC overhead, not the hook
   exec overhead — but hook exec overhead is ~5ms, far below the LLM
   turn latency. This is the path of least resistance.

2. **PermissionRequest hook with `updatedPermissions` (session
   destination).** Docs verbatim:

   > `updatedPermissions` — For "allow" only: array of permission update
   > entries to apply, such as adding an allow rule or changing the
   > session permission mode

   And for `destination: "session"`:

   > in-memory only, discarded when the session ends

   This is the "proper" mechanism — Claude Code keeps the rule in the
   running session's memory, and subsequent matching calls skip the
   dialog AND the PermissionRequest hook.

   **BUT: this does not fire in `-p` mode.** Empirically verified: a
   settings block with `{"permissions":{"ask":["Bash"]}}` plus a
   PermissionRequest hook, prompted to run `date`, produces
   `Permission to run 'date' was not granted.` and the
   PermissionRequest hook log is NEVER created. `-p` mode auto-denies
   anything that would show a dialog instead of firing the
   PermissionRequest event. That matches the behavior observed for the
   permissions reference's `-p` auto-deny path.

   So PermissionRequest + `updatedPermissions: session` is viable only
   if ccom spawns managed sessions in **interactive** mode (not `-p`),
   e.g. via the Agent SDK's streaming-input form where the SDK's own
   `canUseTool` callback substitutes for the dialog. Switching ccom's
   spawn path to SDK streaming is not trivial — it's a separable
   decision from Phase 7 itself.

3. **Swap PreToolUse "ask" for an interactive SDK callback.** Not
   investigated in this spike; would require adopting the TS/Python
   Agent SDK rather than the raw `claude -p` binary. Out of scope.

   Note: empirically, in `-p` mode, a PreToolUse hook returning
   `permissionDecision: "ask"` is treated as **allow** (Claude ran
   `date` and returned the output) rather than firing a dialog. So
   "ask" is not useful for deferring decisions in non-interactive mode
   — it degrades to allow.

**Recommended path: Option 1 (state file).** It works today, in `-p`
mode, with no SDK rewrite, and matches what Phase 7 is asking for.

### Q5. Timeout behavior — **verified, generous default, configurable.**

Docs verbatim:

> Default timeout: **600 seconds** (10 minutes) for command hooks.
>
> Configure via the `timeout` field (in seconds):
> ```json
> { "type": "command", "command": "...", "timeout": 30 }
> ```

600s default is plenty for a socket RPC to the ccom daemon and a human
click. The `timeout` field is per-hook.

What happens on timeout is not quoted in the section I fetched — it
should be tested explicitly before Phase 7 ships, but `exit 2 =
blocking error` is the most plausible fallback. Marking this as
**unverified** for the post-timeout behavior specifically.

### Q6. Per-call vs batch — **verified per-call.**

Docs verbatim:

> PreToolUse fires **once per tool call**, before that specific call
> executes. If Claude makes multiple tool calls in a single turn,
> PreToolUse fires separately for each one.

Empirical confirmation: the two-`echo` test in Q4 produced exactly two
hook invocations with distinct `tool_use_id`s under a single
`session_id`.

## Verdict

| # | Question                              | Answer                  |
|---|---------------------------------------|-------------------------|
| 1 | Synchronous gating?                   | YES                     |
| 2 | Decision protocol?                    | YES, `hookSpecificOutput` |
| 3 | Structured input including session?   | YES, includes `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id`, `transcript_path` |
| 4 | settings.json live-reload skips hook? | NO — not reloaded mid-session |
| 5 | Timeout?                              | 600s default, per-hook `timeout` field, post-timeout behavior unverified |
| 6 | Per-call vs batch?                    | Per-call                |

**Overall: PARTIAL — Option E viable with one adjustment.**

Five of six assumptions hold cleanly. The one that doesn't (Q4) has a
cheap workaround that does not change Phase 7's user-visible contract:

**"Allow always" must be implemented by a ccom-owned per-session
state file that the PreToolUse hook consults on every invocation,
not by mutating Claude Code's own `settings.json` mid-session.**

The hook is re-executed per call regardless, so a ~5ms state-file read
is free. The hook can short-circuit to `exit 0` with no daemon RPC when
a matching "allow always" rule exists, preserving the low-latency
experience the plan wants. Implementation surface: ~20 lines of Rust in
the hook binary plus a JSON or SQLite schema for the state file.

## What this means for Phase 7

- **Primary design (PreToolUse as the universal gate) is sound.**
  Proceed with the plan's Option E.
- **Caller identification is free.** Use `session_id` from the hook's
  stdin JSON. No need for the `.mcp.json` header trick Phase 6 used.
  Drop the `X-Ccom-Caller` mechanism from the Phase 7 plan if it's
  still in there.
- **"Allow always" state lives in ccom, not in Claude Code.** Design
  a per-session state file (e.g.
  `${XDG_STATE_HOME}/ccom/sessions/<uuid>/approvals.json`) keyed by
  `(tool_name, tool_input fingerprint)`. Hook reads on every call;
  daemon writes on "allow always" clicks.
- **"Allow once" requires a daemon round-trip.** Hook opens the Unix
  socket, sends the tool-call context, blocks on the daemon's reply,
  translates to `allow` / `deny` JSON, exits 0. Straightforward.
- **Timeout post-behavior needs a follow-up test.** Write a hook that
  intentionally hangs past `timeout: 5` and observe whether Claude
  treats the timeout as a block or a pass-through. One short test,
  can be done at the start of Phase 7 Task 1.
- **Stop-hook UUID capture (sibling spike) has an alternate path.**
  The PreToolUse stdin already carries `session_id`, so Phase 7's
  approval router can map tool calls to ccom sessions without waiting
  for the Stop-hook parser. That doesn't obsolete the Stop-hook work
  (it still matters for transcript replay and out-of-band correlation)
  but it removes a blocking dependency: Phase 7 Task 1 can start
  before the Stop-hook spike lands.

## Fallback if this turns out to be broken in a later Claude Code release

If PreToolUse's synchronous gating regresses or the stdin schema drops
`session_id`, the fallback is the **Agent SDK streaming-input mode**
with `canUseTool` callbacks (TS or Python), which is the officially
blessed programmatic permission hook. That would require ccom to spawn
managed sessions via an SDK subprocess instead of `claude -p`, which
is a meaningful refactor but not a design-level blocker. Document this
as the Phase 7 "break glass" path in the plan; do not preemptively
invest in it.

## Artifacts

All scratch test scripts live in `/tmp/ccom-phase7-spike/` — not in the
project tree. Preserved here for reproduction:

- `hook.sh` — allow + 2s sleep, logs stdin/env
- `hook-deny.sh` — returns `permissionDecision: deny`
- `hook-reload.sh` — rewrites settings file on first call, always allows
- `hook-permreq.sh` — PermissionRequest hook with session-scope addRules
- `hook-ask.sh` — returns `permissionDecision: ask`
- `settings-reload.json` — seed settings for the reload test
- `*.log` and `*.state` — captured hook invocation records

## Unverified / needs follow-up

- Post-timeout behavior when a hook exceeds its `timeout` (block or pass
  through?). One quick test in Phase 7 Task 1.
- Whether MCP tool calls (`mcp__ccom__spawn_session` etc.) fire
  PreToolUse the same way as built-in tools. The docs say the matcher
  supports `mcp__<server>__<tool>` — almost certainly yes, but worth a
  5-minute smoke test since Phase 7's approval router needs to gate MCP
  tool calls too.
- Whether `additionalContext` from a PreToolUse hook actually reaches
  Claude's next turn in `-p` mode (the plan's "approval reasoning
  surfaces in transcript" story depends on this).
