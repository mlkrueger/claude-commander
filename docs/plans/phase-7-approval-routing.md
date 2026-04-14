# Phase 7 — Approval Routing (Driver-Mediated Tool Permissions)

**Branch:** `session-mgmt/phase-7-approval-routing`
**Depends on:**
- Phase 6 merged (driver role, attachments, scope resolution, `spawn_session`)
- PreToolUse hook viability spike (`docs/plans/notes/phase-7-hook-spike.md`) — **GO with one adjustment (see §Spike findings summary below)**
- UUID capture work (only required if we ever pivot to Option F)
**Blocks:** Nothing in the current roadmap — Phase 7 is a capability add-on, not a prerequisite for other phases.
**Design refs:**
- `docs/designs/session-management.md` §6 (driver policy — the authoritative spec for fleet orchestration)
- `docs/plans/notes/phase-7-hook-spike.md` (spike findings — **read before starting Task 1**)
- Claude Code hook docs: `PreToolUse` event, `hookSpecificOutput` decision protocol

## Spike findings summary

The spike (`docs/plans/notes/phase-7-hook-spike.md`, 2026-04-13, Claude Code 2.1.105) ran all six load-bearing questions. Five of six confirmed; one (Q4) failed with a documented workaround:

| # | Question | Answer |
|---|----------|--------|
| 1 | PreToolUse is synchronous? | YES — verified empirically |
| 2 | Decision protocol works? | YES — `hookSpecificOutput.permissionDecision` |
| 3 | Structured stdin including `session_id`? | YES — `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id` all present |
| 4 | settings.json live-reload (for "allow always")? | **NO — not reloaded mid-session** |
| 5 | Timeout? | 600s default, per-hook `timeout` field; post-timeout behavior unverified |
| 6 | Per-call (not per-batch)? | YES — confirmed two distinct `tool_use_id`s on a two-call prompt |

**Q4 failure and its fix:** "Allow always" cannot be implemented by writing to `settings.json` mid-session — Claude Code does not reload it during a running process. The fix is a **ccom-owned per-session approvals state file** (see §Data model delta). The hook reads this file on every invocation and short-circuits to allow when a matching rule exists, without a daemon round-trip.

**Secondary findings:**
- `permissionDecision: "ask"` in `-p` (non-interactive) mode **degrades to allow**, not a dialog. Do not use "ask" as a deferral mechanism in headless sessions.
- `PermissionRequest` hooks **do not fire in `-p` mode**. Claude auto-denies anything requiring a dialog without firing the hook. All ccom managed sessions are `-p`; this hook type is useless for Phase 7.
- Caller identification for the hook is free: `session_id` on stdin is the Claude Code session UUID. The Phase 6 `X-Ccom-Caller` HTTP header approach is not needed for hook routing — ccom maps `session_id` → ccom session using the UUID captured at session start.
- **Overall verdict: PARTIAL GO.** Option E (universal PreToolUse hook, decide at fire time) is sound. Proceed with the plan as written, substituting the state-file mechanism for settings.json writes throughout.

## Context

Phase 6 gave a driver session the ability to spawn, prompt, read, and kill its own children. Phase 7 closes the last rough edge in the driver experience: **tool approvals**. Today, when a driver-spawned child hits a tool call Claude Code's permission system would otherwise gate (an unpermitted `Bash`, a file write outside the allowed glob, etc.), a modal pops in the TUI — even though the human's attention is on the driver, not on the child. The human then has to context-switch to a sub-session to say "yes, that's fine" on behalf of an agent that was supposed to be autonomous.

**The goal:** route those approvals to the driver as a conversational message. The driver's LLM can decide to approve silently, deny, or ask the human in chat. If the human says "always allow this," ccom writes a rule into a per-session approvals state file; the hook reads that file on every subsequent invocation and short-circuits without a daemon round-trip. Claude Code's own settings layers are not modified.

### Design principles (non-negotiable)

These came out of a long design conversation and frame every decision in this plan. Any task that violates one of these is wrong and should be redesigned.

1. **Ccom owns routing, not policy.** Ccom is a **routing proxy** — it transports approval requests to a driver and transports decisions back. It does not evaluate rules against Claude Code's permission matchers. The "allow always" affordance writes into a **ccom-owned per-session state file** (not into Claude Code's `settings.json`) because the spike confirmed settings.json is not reloaded mid-session. Claude Code's own user-level and project-level `settings.json` layers are left untouched. The state file is ccom infrastructure, not a third permissions layer — it stores allow-always decisions made by the driver, consulted only by the ccom hook binary.

2. **Reuse Claude Code's `PreToolUse` hook.** The hook mechanism is the seam. It provides:
   - Structured JSON on stdin (tool name, tool input, `cwd`, `session_id`, `tool_use_id`)
   - Synchronous gating (Claude Code waits for the hook to exit before firing the tool)
   - A JSON decision protocol (`hookSpecificOutput.permissionDecision: allow|deny`)
   All three confirmed by the spike.

3. **Option E primary — install the hook on every ccom-spawned session; decide at fire time.** The hook's logic:
   ```
   on every tool call:
       read per-session approvals state file
       if a matching allow-always rule exists: exit 0 immediately (no daemon RPC)
       query ccom daemon: "session <uuid> — owned/attached to a driver?"
       if no driver: exit 0 — let Claude Code's own permission system handle it
       if driver found: serialize the request, block on daemon, relay back
   ```
   Consequences:
   - **Late attachment "just works."** A Solo session attached to a driver mid-run is already running the hook; the pass-through branch silently starts routing on the next tool call. No restart.
   - **Solo sessions pay a fork+exec per tool call.** Sub-millisecond on modern hardware. The state-file read adds ~5ms on first call but is dominated by fork/exec overhead.
   - **Solo-session UX is unchanged.** The pass-through path (no driver owner) means Claude Code's normal TUI modal fires exactly as it does today.

4. **Option F (restart-with-resume on adoption) is a documented alternative, not implementation.** Kept in this doc so future-us knows why we didn't pick it and what the tradeoffs would be. **Not** in the task breakdown.

5. **"Allow always" lives in ccom's state file, not in Claude Code's settings.** After the write, the hook short-circuits on the next matching call without a daemon round-trip. Claude Code's own `settings.json` is never modified by Phase 7. This keeps the settings files stable and avoids any concurrent-write races with Claude Code itself.

6. **No modal for driver-routed approvals.** The driver session receives the request as an in-chat message and handles it conversationally. A subtle status-line hint ("driver has a pending approval") is acceptable to draw the human's eye — a blocking modal is not.

7. **No "ask" in hook decisions.** The spike confirmed that `permissionDecision: "ask"` in `-p` mode degrades to allow. The hook only emits `"allow"` or `"deny"` (or exits with no output for pass-through). Never emit `"ask"` or `"defer"` — these have undefined behavior in non-interactive mode.

## Architecture

### Primary flow (Option E)

```
Child session wants to call a tool
        │
        ▼
Claude Code fires PreToolUse hook
        │  (stdin: {session_id, tool_name, tool_input, cwd, tool_use_id, ...})
        ▼
ccom-hook binary:
    1. read session_id from stdin JSON
    2. read per-session approvals state file at
       $XDG_STATE_HOME/ccom/sessions/<uuid>/approvals.json
    3. if matching allow-always rule: exit 0 with {"hookSpecificOutput":
       {"permissionDecision":"allow"}} — no daemon RPC
    4. connect to ccom daemon socket in session's hook_dir
    5. write ApprovalQuery{session_id, tool, args, cwd, tool_use_id, nonce}
    6. block on response with 600s timeout
        │
        ▼
ccom main process (on the hook reader thread):
    1. look up session by session_id (UUID → ccom session mapping)
    2. resolve its owning driver via spawned_by OR attachment map
    3. if no driver owner: reply Passthrough — hook exits 0 with no body,
       Claude Code handles it with its own permission layer
    4. if owner found: publish SessionEvent::ToolApprovalRequested
       { request_id, session_id, driver_id, tool, args, cwd }
       and stash an open request record keyed by request_id
        │
        ▼
Driver session (Phase 4 subscribe stream):
    receives the event as an MCP notifications/message entry
    (push path unverified end-to-end — see Risks §1 and Task 7 fallback)
        │
        ▼
Driver's LLM presents the request to the human in chat
        │
        ▼
Human answers in chat ("yes", "no", "always allow Bash in this repo")
        │
        ▼
Driver calls respond_to_tool_approval(request_id, decision, scope)
        │
        ▼
ccom:
    1. match request_id to the open request
    2. if scope = AllowAlways: write rule to the per-session approvals
       state file ($XDG_STATE_HOME/ccom/sessions/<uuid>/approvals.json)
    3. write ApprovalResponse{decision} to the response socket
    4. close the request
        │
        ▼
ccom-hook binary:
    reads the response, prints the JSON decision to stdout, exits 0
        │
        ▼
Claude Code:
    proceeds or aborts the tool call per the hook's verdict
```

### Option F (documented alternative — NOT implementing)

Instead of installing the hook on every session, install it only on driver-owned/attached sessions. When a Solo session is adopted by a driver mid-run:

1. Capture the Claude session UUID (work in flight as a separate task).
2. Kill the adopted session's Claude subprocess.
3. Respawn with `claude --resume <uuid> --settings <new_hook_dir>/settings.json`.
4. The resumed process picks up conversation history and now has the hook installed.

**Tradeoffs vs Option E:**
- Solo sessions never pay the per-tool-call hook overhead. (But the overhead is sub-millisecond — a non-concern.)
- No need to answer the "would Claude Code have allowed this anyway" question — hook is only present when routing is definitely wanted.
- **But:** attachment has a visible, disruptive restart. Conversation replay may show UI flicker. Real failure modes: resume might fail, the in-flight turn is lost, the child's ephemeral state (untracked stdin buffers, streaming reads) is dropped. Adopted sessions become second-class.
- **And:** depends on UUID capture working reliably, which itself has failure modes if Claude Code doesn't emit the UUID early enough in the session lifecycle.

**Verdict:** documented here for future-us. **Not in Phase 7's task breakdown.**

### Data model delta

```rust
// src/bus.rs (or wherever SessionEvent lives)
pub enum SessionEvent {
    // existing variants…
    ToolApprovalRequested {
        request_id: u64,                // monotonic, minted by ccom
        session_id: usize,              // the ccom session id of the child asking
        driver_id: usize,               // the driver that should answer
        tool: String,                   // e.g. "Bash"
        args: serde_json::Value,        // raw tool input
        cwd: PathBuf,
        timestamp: SystemTime,
    },
    ToolApprovalResolved {
        request_id: u64,
        session_id: usize,
        driver_id: usize,
        decision: ApprovalDecision,
        scope: ApprovalScope,
    },
}

pub enum ApprovalDecision { Allow, Deny }

pub enum ApprovalScope {
    Once,
    AllowAlways,  // writes to per-session approvals state file
}

// src/approvals.rs (new)
pub struct PendingApproval {
    pub request_id: u64,
    pub session_id: usize,           // ccom session index
    pub claude_uuid: String,         // Claude Code session_id (for state file path)
    pub driver_id: usize,
    pub tool: String,
    pub args: serde_json::Value,
    pub cwd: PathBuf,
    pub created_at: SystemTime,
    pub response_tx: oneshot::Sender<ApprovalDecision>, // back to the hook
}

pub struct ApprovalRegistry {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingApproval>>,
}

// State file schema: $XDG_STATE_HOME/ccom/sessions/<claude_uuid>/approvals.json
// Written by ccom daemon on AllowAlways; read by hook binary on every call.
// Format:
// {
//   "allow_always": [
//     { "tool": "Bash", "input_fingerprint": "sha256:<hex>" },
//     ...
//   ]
// }
// The `input_fingerprint` is a SHA-256 of the canonicalized `tool_input` JSON.
// A zero-length `input_fingerprint` means "all inputs for this tool".
```

The registry lives on `McpCtx` behind an `Arc`, same ownership pattern as the attachment map from Phase 6. The hook reader thread calls `ApprovalRegistry::open_request(...)` to insert and get a receiver; the MCP handler for `respond_to_tool_approval` calls `resolve(request_id, decision)`.

Note: `SettingsTarget` (Project/User) from earlier drafts is removed. Phase 7 does not write to Claude Code's `settings.json`. The only write target is the ccom-owned state file per session UUID.

### Communication between hook and daemon

Phase 3.5 introduced a one-way FIFO for `Stop` events. Phase 7 needs **bidirectional** per-session communication: hook → ccom (request) and ccom → hook (response). Options:

- **Unix socket**: one endpoint, bidirectional, clean. More complex (accept loop, per-request peer).
- **FIFO pair**: `approval_request.fifo` and `approval_response.fifo` inside each session's `hook_dir`. Consistent with Phase 3.5 conventions.

**Pick:** Unix socket per session, under `hook_dir/approval.sock`. A FIFO pair works but the response FIFO must be filtered by `nonce` to correlate concurrent requests from the same session — adding complexity without benefit. A Unix socket with a per-request message exchange (write query, read response) is self-framing and simpler. The ccom main process spawns a per-session accept loop task when the session is created.

### New MCP tool: `respond_to_tool_approval`

```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RespondToToolApprovalArgs {
    pub request_id: u64,
    pub decision: ApprovalDecisionArg,   // "allow" | "deny"
    #[serde(default)]
    pub scope: ApprovalScopeArg,          // "once" (default) | "allow_always"
}
```

Handler:
1. Caller-id check (Phase 6's machinery) — caller must be a driver.
2. Look up the `request_id` in the registry. Reject if not found (already resolved or expired) or if the registered `driver_id` doesn't match the caller — drivers can only answer their own requests.
3. If `scope = AllowAlways`: compute the input fingerprint, write the rule to the state file at `$XDG_STATE_HOME/ccom/sessions/<claude_uuid>/approvals.json` (create dirs if needed, atomic rename for durability). On state-file write failure, log loudly and **still** resolve the decision — a write failure shouldn't hang the child.
4. Resolve the oneshot with the decision.
5. Publish `SessionEvent::ToolApprovalResolved` on the bus (for TUI status-line + audit log).
6. Return `{resolved: true, state_file_written: bool}`.

### Fallback tool: `list_pending_approvals`

If the spike (Phase 6 Task 10 smoke test or Phase 7 Task 11 manual test) reveals that MCP `notifications/message` entries **don't** reach the driver's LLM context, we need a pull-based alternative. The tool:

```rust
pub struct ListPendingApprovalsArgs {}  // no args — scope comes from caller id

pub struct PendingApprovalWire {
    pub request_id: u64,
    pub session_id: usize,
    pub tool: String,
    pub args: serde_json::Value,
    pub cwd: String,
    pub age_secs: u64,
}
```

Returns all pending approvals in the caller's driver scope. The driver would poll (e.g., once per turn) to find requests it needs to answer. This is implemented in Task 7 **only if the smoke test or integration test reveals the push path doesn't work.**

### Hook binary behavior

New binary: `ccom-hook-pretooluse` (or reuse the existing `ccom-hook` binary with a `--pretooluse` subcommand; pick based on Phase 3.5's binary shape).

```
read stdin JSON -> struct HookInput { session_id, cwd, tool_name, tool_input, tool_use_id, ... }
read $CCOM_HOOK_DIR from env (or derive from cwd convention)
read approvals state file at $XDG_STATE_HOME/ccom/sessions/<session_id>/approvals.json
if matching allow-always rule:
    print {"hookSpecificOutput":{"permissionDecision":"allow"}}
    exit 0
connect to $CCOM_HOOK_DIR/approval.sock
write {tool_name, tool_input, session_id, tool_use_id, nonce: <local u64>} as JSON line
select on socket with timeout:
    - ApprovalResponse{decision: Allow}       → exit 0 with {"hookSpecificOutput":{"permissionDecision":"allow"}}
    - ApprovalResponse{decision: Deny}        → exit 0 with {"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"..."}}
    - ApprovalResponse{decision: Passthrough} → exit 0 with no body (let CC handle it)
    - timeout (600s)                          → exit 0 with {"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"ccom: driver timeout"}}
                                               + write marker into hook_dir/.timeouts
```

**IMPORTANT:** Never emit `permissionDecision: "ask"` or `"defer"`. In `-p` mode "ask" degrades to allow (spike Q4 secondary finding), so using it as a deferral would silently permit calls the driver hasn't approved. Only `"allow"` and `"deny"` are valid outputs.

The exact stdout JSON shape is confirmed by the spike (Q2). Timeout is 600s to match Claude Code's default hook timeout; this can be configured via `timeout` in the `settings.json` hook entry if needed.

### Installation in `Session::spawn`

Phase 6's `Session::spawn` already writes a per-session `hook_dir` and a `.mcp.json`. Phase 7 augments it:

1. Additionally write a `settings.json` into the hook config dir with a `PreToolUse` hook entry pointing at `ccom-hook-pretooluse`, with `timeout: 600`.
2. Create the `approval.sock` listener task (spawned as a Tokio task, one per session).
3. Pass the `hook_dir` via env (`CCOM_HOOK_DIR`) — already set in Phase 3.5.
4. The per-session `settings.json` must **merge** with the project's existing `.claude/settings.json` rather than replace it. Claude Code's layering rules apply; ccom writes to its own per-session layer which sits alongside the user's project layer. Verify Claude Code honors multiple `--settings` sources at the start of Task 1.

**Note:** ccom does NOT write `permissions.allow` entries to any `settings.json` at any point. All allow-always decisions land exclusively in the per-session approvals state file.

**Critical:** installing the hook on every ccom-spawned Claude session means **all** Phase 1–6 behavior is affected, not just Phase 7's driver flow. For Solo sessions, the hook's pass-through branch must be rock solid — any bug here breaks normal ccom usage. Task 5 tests this exhaustively.

## Task breakdown

### Task 0 — Spike review gate (~15 min, already done)

The spike at `docs/plans/notes/phase-7-hook-spike.md` has answered all six questions. Read it. The decision:

- **GO on Option E.** All load-bearing assumptions are confirmed.
- **AllowAlways uses the ccom state file, not settings.json** (Q4 failed, workaround in place).
- **No "ask" or "defer" in hook output** (degrades to allow in -p mode).
- **PermissionRequest hooks are not used** (don't fire in -p mode).
- **Remaining unknowns:** post-timeout behavior when a hook exceeds its `timeout` (block or pass-through?) — test explicitly in Task 1. Whether MCP tool calls fire PreToolUse the same as built-in tools — quick smoke test in Task 1.

Deliverable: this section. Task 0 is closed.

### Task 1 — Hook infrastructure: binary + socket + spawn integration (~2.5 hours)

**Files:** `src/bin/ccom-hook-pretooluse.rs` (new), `src/session/spawn.rs`, `src/session/hook_dir.rs` (wherever Phase 3.5 put the hook dir helper).

- New binary: reads stdin, checks state file, opens Unix socket, writes request, reads response, prints JSON decision. Under 200 lines. No ccom internals imported.
- Extend `Session::spawn` to:
  - Create `hook_dir/approval.sock` listener task.
  - Write a `settings.json` into `hook_dir/` with a `PreToolUse` entry pointing at the binary. Merge with project-level settings as needed.
- Per-session state file path: `$XDG_STATE_HOME/ccom/sessions/<claude_uuid>/approvals.json`. Ensure the directory is created when the session is first promoted (or lazily on first AllowAlways write).
- Smoke-test at the start of Task 1: verify Claude Code honors multiple `--settings` paths; verify MCP tool calls (`mcp__<server>__<tool>`) fire PreToolUse; verify post-timeout behavior.

**Tests:**
- `hook_binary_allow_always_rule_short_circuits_without_socket_call`
- `hook_binary_sends_request_and_reads_allow_response`
- `hook_binary_sends_request_and_reads_deny_response`
- `hook_binary_times_out_after_configured_deadline` (short override via env)
- `session_spawn_creates_approval_socket_listener`
- `session_spawn_writes_pretooluse_hook_entry`

### Task 2 — `ToolApprovalRequested` event + approval registry (~1 hour)

**Files:** `src/bus.rs`, `src/approvals.rs` (new), `src/mcp/state.rs`.

- Add `SessionEvent::ToolApprovalRequested` and `SessionEvent::ToolApprovalResolved` variants.
- New module `approvals.rs` with `ApprovalRegistry`, `PendingApproval`, `ApprovalDecision`, `ApprovalScope`.
- Wire `Arc<ApprovalRegistry>` onto `McpCtx` at construction.
- The approval coordinator (a new small function, can live in `src/approvals.rs`) ties it together: on incoming hook request, resolve owning driver via UUID → ccom session mapping → `spawned_by`/attachment lookup, either reply `Passthrough` immediately (no driver) or open a registry entry + publish `ToolApprovalRequested`.

**Tests:**
- `registry_open_and_resolve_round_trip`
- `registry_resolve_unknown_request_id_errors`
- `registry_resolve_wrong_driver_rejected` — driver A tries to answer driver B's request
- `approval_coordinator_replies_passthrough_for_orphan_session`
- `approval_coordinator_publishes_event_for_driver_owned_session`

### Task 3 — MCP tool `respond_to_tool_approval` (~1.5 hours)

**Files:** `src/mcp/handlers.rs`, `src/mcp/state.rs`.

- Add `RespondToToolApprovalArgs` and `RespondToToolApprovalWire` structs.
- Handler per §Architecture above.
- `scope: ApprovalScope` is `Once` (default) or `AllowAlways` — no `SettingsTarget` variant. The state file path is derived from the `claude_uuid` stored in the `PendingApproval`.

**Tests** (in `tests/approval_routing.rs`):
- `driver_allows_once_child_proceeds`
- `driver_denies_child_aborts`
- `driver_allows_always_writes_state_file`
- `driver_cannot_answer_other_drivers_request`
- `solo_caller_rejected_from_respond_tool`
- `unknown_request_id_returns_error`

### Task 4 — Per-session approvals state file helper (~1 hour)

**Files:** `src/approvals_state.rs` (new).

- `fn read_approvals(claude_uuid: &str) -> io::Result<ApprovalsState>` — reads `$XDG_STATE_HOME/ccom/sessions/<uuid>/approvals.json`. Returns empty state if missing.
- `fn add_allow_always(claude_uuid: &str, tool: &str, input_fingerprint: Option<&str>) -> io::Result<()>`:
  1. Lock the state file with `fs2::FileExt::try_lock_exclusive`.
  2. Read current state (empty if missing).
  3. De-dup and insert.
  4. Write to `approvals.json.tmp.<pid>.<nonce>` in the same dir.
  5. `fsync` the temp file.
  6. `rename` to `approvals.json` (atomic on POSIX).
  7. `fsync` the directory.
- `fn input_fingerprint(tool_input: &serde_json::Value) -> String` — SHA-256 of the canonicalized JSON (sorted keys, no whitespace). A caller passing `None` for the fingerprint means "all inputs for this tool".
- `fn matches_allow_always(state: &ApprovalsState, tool: &str, tool_input: &serde_json::Value) -> bool` — used by the hook binary to short-circuit.

**Tests:**
- `state_file_creates_dirs_if_missing`
- `state_file_add_and_match_exact_fingerprint`
- `state_file_wildcard_matches_any_input`
- `state_file_is_idempotent_for_duplicate_rules`
- `state_file_atomic_rename_preserves_existing_on_parse_error`
- `state_file_handles_concurrent_writers` (two threads, assert both rules present)

### Task 5 — Install hook on every `Session::spawn` (~1 hour)

**Files:** `src/session/spawn.rs`.

- Extend `Session::spawn` (the single spawn entry point from Phase 1–6) to always install the PreToolUse hook. No feature gate — every Claude session gets the hook.
- The hook's pass-through path (no driver owner → `Passthrough`) is the critical code path for Solo sessions. Exhaustively test it.
- Add a `ccom_hook_timing` debug log line around the pass-through so we can measure the per-tool-call overhead in the wild.

**Tests:**
- `solo_session_gets_hook_installed`
- `solo_session_hook_replies_passthrough` — end-to-end via a fake tool call writing to the socket
- `driver_owned_child_routes_to_driver`
- `attached_session_routes_to_driver_dynamically` — attach post-spawn, next request routes to driver

### Task 6 — Driver-side subscription filter for approval events (~30 min)

**Files:** `src/mcp/handlers.rs` (specifically the `subscribe` tool from Phase 4).

Phase 6's `subscribe` filters events by caller scope. `ToolApprovalRequested` events carry an explicit `driver_id`; the subscribe filter must use it directly.

- Update the scope filter in `subscribe`'s forwarder task: if the event is `ToolApprovalRequested { driver_id, .. }`, forward only to the matching driver. Same for `ToolApprovalResolved`.

**Tests:**
- `approval_event_only_reaches_target_driver`
- `approval_event_not_visible_to_solo_caller`
- `resolved_event_reaches_target_driver`

### Task 7 — Fallback `list_pending_approvals` poll tool (~1 hour — CONDITIONAL)

**Files:** `src/mcp/handlers.rs`.

**Conditional on Task 11 (manual verification) revealing that MCP `notifications/message` entries do not reach the driver's LLM context.** If the push path works, skip this task.

- Add `list_pending_approvals` tool. Scope-filters the registry by caller's driver id. Returns age + tool + args + session_id for each open request.
- Document the polling cadence expectation in the tool's description so the driver's LLM knows to call it at turn start.

**Tests:**
- `list_pending_approvals_returns_scoped_requests`
- `list_pending_approvals_empty_for_solo_caller`
- `list_pending_approvals_excludes_resolved_requests`

### Task 8 — TUI status-line hint for pending approvals (~1 hour)

**Files:** `src/ui/status_line.rs` (or wherever the current status line lives), `src/app/mod.rs`.

- Subscribe to `ToolApprovalRequested` / `ToolApprovalResolved` in the main App tick loop. Maintain a `pending_approvals_per_driver: HashMap<usize, u32>` counter.
- When the current active view is a driver with a non-zero pending count, render `" ▲ <n> pending approval(s)"` in the status line, dim accent color. No modal. No blocking key capture.
- Also render a subtle `▲` marker next to the driver's row in the session list panel when that driver has open approvals.
- On `ToolApprovalResolved`, decrement. On driver exit, drop the entry and any still-pending approvals fall back to deny + a log line.

**Tests:**
- `status_line_shows_pending_count_for_active_driver`
- `status_line_clears_on_resolution`
- `session_list_marker_tracks_pending_count`

### Task 9 — Timeout + orphan handling (~45 min)

**Files:** `src/approvals.rs`, `src/bin/ccom-hook-pretooluse.rs`.

- Hook binary enforces a 600s timeout matching Claude Code's default hook timeout (configurable via `CCOM_APPROVAL_TIMEOUT_SECS` env for testing). On timeout, hook emits `deny` with reason `"ccom: driver timeout"`.
- Registry side: on timeout the hook moves on, but the registry entry is still there. Add a reaper that sweeps entries older than 120s beyond the hook timeout and drops them with a warning log. Also publish `ToolApprovalResolved { decision: Deny }` so the status-line counter clears.
- On driver exit, all pending approvals for that driver are resolved `Deny` immediately.
- Surface timeouts to the user: write a line into the TUI's activity log panel (Phase 3.5's existing log surface) with the session id and tool name.

**Tests:**
- `hook_times_out_after_configured_deadline`
- `registry_reaper_clears_stale_entries`
- `driver_exit_fails_all_pending_approvals_for_that_driver`
- `timeout_publishes_resolved_event_for_status_line`

### Task 10 — Integration tests (~1.5 hours)

**File:** `tests/approval_routing.rs` (new).

All tests construct a driver session via `SessionManager::spawn` with `role: Driver { .. }`, spawn a child via the Phase 6 `spawn_session` tool, then simulate `PreToolUse` hook requests by writing to the child's approval socket directly. Cover:

1. `child_of_driver_routes_approval_to_driver_and_allows_once`
2. `child_of_driver_routes_approval_to_driver_and_denies`
3. `child_of_driver_allow_always_writes_state_file_and_short_circuits_next_call`
4. `solo_session_hook_replies_passthrough_without_publishing`
5. `late_attached_session_starts_routing_on_next_request`
6. `two_concurrent_approvals_from_same_child_serialize_correctly`
7. `driver_exit_denies_all_pending_approvals_for_its_children`
8. `hook_timeout_denies_and_surfaces_to_tui_log`
9. `driver_cannot_answer_another_drivers_request`
10. `state_file_round_trip_under_concurrent_write`

### Task 11 — End-to-end manual verification (~1 hour)

Set up a real test scenario:

1. `cargo run -- --driver --spawn-policy budget --budget 3`
2. Tell the driver: "Spawn a helper in `<current repo>`. Have it run `ls -la src/` and report the output."
3. Observe:
   - Driver spawns child silently (Phase 6 happy path).
   - Child calls `Bash` for `ls`.
   - **Approval routes to driver** — observe whether it arrives as a push notification or requires the driver to poll via `list_pending_approvals`.
   - Driver's LLM sees it, asks the human in chat, or decides autonomously.
   - Human says "always allow this".
   - `respond_to_tool_approval` fires with `scope: "allow_always"`.
   - Child proceeds.
   - Child is then asked to run `ls -la docs/` — hook short-circuits via state file, no daemon round-trip.
4. Verify `$XDG_STATE_HOME/ccom/sessions/<uuid>/approvals.json` contains the new allow rule.
5. Spot check: kill the driver mid-approval → child's pending request resolves to Deny within a few seconds.
6. Spot check: take a Solo session, tool call with a denied Bash — verify TUI modal still fires (pass-through intact).
7. Spot check: `ccom_hook_timing` log line shows sub-millisecond pass-through for Solo sessions.
8. **Key observation:** did the driver receive the approval request via push (subscribe notification) or did it have to call `list_pending_approvals`? Record explicitly — this determines whether Task 7 is needed.

## Parallelism plan

- **Task 0** (spike review) is done — unblocked.
- **Tasks 1 + 2** sequential (registry needs the socket infra to have somewhere to call into). Go into one worktree.
- **Tasks 3 + 4** can run in parallel once Task 2 is on the branch — Task 3 touches MCP handlers, Task 4 is a self-contained new module. Two worktrees.
- **Tasks 5 + 6** sequential after 3+4 merge. Task 5 touches spawn, Task 6 touches subscribe — they don't collide but Task 5 enables the end-to-end test for Task 6 so it's cleaner sequenced.
- **Task 7** is conditional and happens only after Task 11 manual verification.
- **Tasks 8 + 9 + 10** can be parallel after Task 5 lands: Task 8 is UI, Task 9 is approvals module, Task 10 is tests. Three worktrees, minimal collision.
- **Task 11** is the final manual gate.

Subagent decomposition once Tasks 1+2 are on the phase branch:
- **Subagent A (MCP handler + tool side)**: Tasks 3, 6, 7 (if needed). Touches `handlers.rs`, `state.rs`, subscribe filter.
- **Subagent B (state file + approvals plumbing side)**: Tasks 4, 9. Touches `approvals_state.rs`, `approvals.rs`, hook binary.
- **Subagent C (TUI side)**: Task 8. Touches `ui/status_line.rs`, `app/mod.rs`.

Collision surface: `app/mod.rs` (C adds pending counter field; A/B don't touch it). `approvals.rs` (B touches for timeout, A reads for filtering — interface is stable by end of Task 2, so parallel work is fine).

## Risks

1. **MCP `notifications/message` delivery into the driver's LLM context is unverified.** This is the #1 risk and the whole premise of the push path. Phase 4's `subscribe` tool forwards events as MCP notifications, but we have **not** confirmed end-to-end that Claude Code surfaces those into the model's context window mid-turn or between turns. If it doesn't, Phase 7 falls back to the polling tool (Task 7), which requires the driver's prompt template to call `list_pending_approvals` each turn — a UX regression. **Mitigation:** Task 11 tests this explicitly and falls back fast. **Escalation:** if polling is also unreliable (e.g., driver goes silent for many minutes), we need a third path — possibly a side channel that writes the request into the driver's TTY directly, which is ugly but always works.

2. **Post-timeout behavior for hooks is unverified.** The spike confirmed the timeout field exists (600s default) but did not test what Claude Code does when a hook exceeds it — block the tool call or pass through? **Mitigation:** Test explicitly at the start of Task 1 with a 2s timeout override. If it passes through on timeout rather than blocking, the ccom-side reaper must also handle the case where the child gets a pass-through while the daemon still has an open registry entry.

3. **"Would Claude Code have blocked this anyway?" ambiguity.** If the hook input doesn't distinguish "CC was about to allow" from "CC was about to block," every tool call on a driver-owned session routes to the driver — including trivially-allowed ones. **Mitigation:** ccom adds a shallow "is this tool call's fingerprint already in the state file" precheck as the first step in the hook binary. For calls not in the state file, the daemon resolves ownership and routes. For truly noisy sessions, the driver can populate the state file proactively with broad allow rules.

4. **Multi-driver attachment collision.** Phase 6 allows a session to be attached to multiple drivers. Phase 7 can only route an approval to one. **Mitigation:** Task 1 tightens attachment to single-driver — refuse at attach time if already attached. The attach-to-driver TUI action from Phase 6 Task 5 needs a small guard update. Document this as a narrowing of the Phase 6 affordance.

5. **Concurrent writes to the approvals state file.** The daemon writes on AllowAlways; the hook binary reads on every call. Two different processes. The write path uses `fs2::FileExt::try_lock_exclusive` + atomic rename; the read path reads the file open without a lock (read-only). A read coinciding with a rename will either see the old file (still consistent) or the new file (also consistent). No torn read possible. The lock + atomic rename sequence is correct.

6. **Timeout UX.** A 600s timeout on a driver approval means a child may hang for a long time before denying. **Mitigation:** surface pending approvals prominently in the status line (Task 8) so the human is aware. Configure a shorter per-session timeout in the hook entry (e.g., 60s) if the 600s default is too generous. The `timeout` field in the `settings.json` hook entry controls this per-session.

7. **Pass-through overhead on high-frequency tool callers.** A Solo session running hundreds of tool calls a minute pays fork+exec per call. Still sub-millisecond, but on constrained hardware could become visible. **Mitigation:** measure in Task 11 via the `ccom_hook_timing` log. If the numbers are bad, fall back to Option F for Solo sessions only (no hook installed unless adoption happens) — but that re-opens the late-attachment problem. Do not optimize prematurely.

8. **Hook binary is a separate deployable.** Unlike the rest of ccom, the hook binary must be discoverable at an absolute path by Claude Code at tool-call time. **Mitigation:** bundle it in the same `cargo build` output (`src/bin/ccom-hook-pretooluse.rs`) and have `Session::spawn` record its absolute path via `std::env::current_exe()`-relative resolution at startup, then embed that absolute path in the per-session `settings.json`. Don't rely on `PATH`.

9. **Security: approval socket is accessible by anyone who can write to the hook dir.** Currently mode 0600 and under the user's home. Fine for single-user workstations. Document the assumption explicitly.

10. **Hook binary errors leave child hanging.** If the hook binary crashes, the child session blocks on an empty socket forever. **Mitigation:** Claude Code's own hook timeout is the last line of defense. The 120s registry reaper catches stuck entries from the ccom side. Accept the possibility that a crashed hook fails the tool call after Claude Code's deadline expires.

## Verification

- `cargo test` — existing Phase 6 tests still pass, new Phase 7 tests pass (expect ~30+ new test functions across tasks 1–10).
- `cargo clippy --all-targets` — zero warnings.
- `cargo fmt --check` — clean.
- `cargo build --release` — compiles; `ccom-hook-pretooluse` binary is emitted.
- `tests/approval_routing.rs` integration tests pass.
- Manual Task 11 end-to-end with a real repo + real driver + real child:
  - Approve once → child proceeds.
  - Deny → child aborts.
  - Allow always → state file updated; next matching call short-circuits in hook, no daemon RPC.
  - Late attach → next request routes.
  - Solo session pass-through → TUI modal still fires.
  - Driver exit → pending approvals deny within 2s.
  - Timeout → child aborts after timeout with a surfaced error.

## Acceptance criteria

- A driver session receives child tool approvals as conversational events (push or poll), answers them, and the child proceeds or aborts accordingly — **no modal on the driver side**.
- "Allow always" writes a rule into the ccom-owned per-session state file. The **next** matching hook invocation short-circuits without a daemon round-trip. Claude Code's `settings.json` is not modified.
- A Solo session's behavior is **unchanged** from Phase 6 — the hook's pass-through path fires and Claude Code's own permission flow is undisturbed.
- A session attached to a driver post-spawn starts routing approvals on the very next tool call, with no restart.
- Driver exit cleanly fails any in-flight approvals for its children within seconds.
- A 600s hook timeout is enforced (or a configured shorter value); timeouts are surfaced in the TUI activity log.
- No `permissionDecision: "ask"` or `"defer"` is ever emitted by the hook binary.
- `PermissionRequest` hooks are not used anywhere in Phase 7.
- Phase 6's driver scope and attachment semantics remain intact; Phase 7's additions are strictly routing.

## Effort estimate

- Task 0 (spike review): done
- Task 1 (hook binary + socket): 2.5 hours
- Task 2 (event + registry): 1 hour
- Task 3 (respond_to_tool_approval): 1.5 hours
- Task 4 (approvals state file): 1 hour
- Task 5 (spawn integration): 1 hour
- Task 6 (subscribe filter): 30 min
- Task 7 (list_pending_approvals fallback): 1 hour **(conditional)**
- Task 8 (status-line hint): 1 hour
- Task 9 (timeout + orphan handling): 45 min
- Task 10 (integration tests): 1.5 hours
- Task 11 (manual end-to-end): 1 hour

**Total: ~11.75 hours** of focused work (10.75 without the conditional Task 7), with ~3 hours parallelizable across two to three subagents once Task 2 lands on the phase branch.
