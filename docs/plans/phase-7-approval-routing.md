# Phase 7 — Approval Routing (Driver-Mediated Tool Permissions)

**Branch:** `session-mgmt/phase-7-approval-routing`
**Depends on:**
- Phase 6 merged (driver role, attachments, scope resolution, `spawn_session`)
- PreToolUse hook viability spike (`docs/plans/notes/phase-7-hook-spike.md`) — **Phase 7 is GO/REDESIGN contingent on spike findings**
- UUID capture work (only required if we ever pivot to Option F)
**Blocks:** Nothing in the current roadmap — Phase 7 is a capability add-on, not a prerequisite for other phases.
**Design refs:**
- `docs/designs/session-management.md` §6 (driver policy — the authoritative spec for fleet orchestration)
- `docs/plans/notes/phase-7-hook-spike.md` (spike findings — **read before starting Task 1**)
- Claude Code hook docs: `PreToolUse` event, JSON decision protocol, per-project `settings.json` layering

## Context

Phase 6 gave a driver session the ability to spawn, prompt, read, and kill its own children. Phase 7 closes the last rough edge in the driver experience: **tool approvals**. Today, when a driver-spawned child hits a tool call Claude Code's permission system would otherwise gate (an unpermitted `Bash`, a file write outside the allowed glob, etc.), a modal pops in the TUI — even though the human's attention is on the driver, not on the child. The human then has to context-switch to a sub-session to say "yes, that's fine" on behalf of an agent that was supposed to be autonomous.

**The goal:** route those approvals to the driver as a conversational message. The driver's LLM can decide to approve silently, deny, or ask the human in chat. If the human says "always allow this," we write an allow rule into the child's project `settings.json` and Claude Code's own permission layer handles identical prompts forever after — ccom never sees them again.

### Design principles (non-negotiable)

These came out of a long design conversation and frame every decision in this plan. Any task that violates one of these is wrong and should be redesigned.

1. **No ccom-side permissions layer.** Ccom does **not** invent a third permissions model on top of Claude Code's user-level (`~/.claude/settings.json`) and project-level (`<repo>/.claude/settings.json`) layers. Those two layers remain the source of truth. Ccom is a **routing proxy** — it transports approval requests to a driver and transports decisions back. It does not remember policy, does not maintain an allowlist of its own, and does not evaluate rules. The "allow always" affordance writes into the **existing** project or user `settings.json` — the same file the Claude Code "Always allow" button writes to — so the next identical prompt is handled entirely by Claude Code without ever reaching ccom.

2. **Reuse Claude Code's `PreToolUse` hook.** The hook mechanism is the seam. It provides:
   - Structured JSON on stdin (tool name, tool input, `cwd`, session context)
   - Synchronous gating (Claude Code waits for the hook to exit before firing the tool)
   - A JSON decision protocol (exit code + stdout JSON → allow/deny)
   Phase 7 assumes all three. The spike at `docs/plans/notes/phase-7-hook-spike.md` must confirm them before Phase 7 lands. If any one is missing, Phase 7 is a **redesign**, not a build-out.

3. **Option E primary — install the hook on every ccom-spawned session; decide at fire time.** The hook's logic:
   ```
   on every tool call:
       query ccom: "session $CCOM_SESSION_ID — owned/attached to a driver?"
       if yes:
           serialize the request, block, wait for driver decision, relay back
       if no:
           exit 0 — let Claude Code's own permission system handle it
   ```
   Consequences:
   - **Late attachment "just works."** A Solo session attached to a driver mid-run is already running the hook; the pass-through branch silently starts routing on the next tool call. No restart.
   - **Solo sessions pay a fork+exec per tool call.** On modern systems this is sub-millisecond. Negligible.
   - **Solo-session UX is unchanged.** The pass-through path means Claude Code's normal TUI modal fires exactly as it does today.

4. **Option F (restart-with-resume on adoption) is a documented alternative, not implementation.** Kept in this doc so future-us knows why we didn't pick it and what the tradeoffs would be. **Not** in the task breakdown.

5. **"Allow always" is not a new ccom concept.** It writes to the same `settings.json` Claude Code's own affordance writes to. After the write, subsequent identical prompts never reach ccom. This is the mechanical implementation of principle 1.

6. **No modal for driver-routed approvals.** The driver session receives the request as an in-chat message and handles it conversationally. A subtle status-line hint ("driver has a pending approval") is acceptable to draw the human's eye — a blocking modal is not.

## Architecture

### Primary flow (Option E)

```
Child session wants to call a tool
        │
        ▼
Claude Code fires PreToolUse hook
        │  (stdin: {tool_name, tool_input, cwd, session_id, ...})
        ▼
ccom-hook binary:
    1. reads $CCOM_SESSION_ID from env
    2. connects to approval FIFO pair in the session's hook_dir
    3. writes ApprovalQuery{session_id, tool, args, cwd, ts, nonce}
    4. blocks on response FIFO (with timeout)
        │
        ▼
ccom main process (on the MCP/hook reader thread):
    1. look up session by $CCOM_SESSION_ID
    2. resolve its owning driver via spawned_by OR attachment map
    3. if no driver owner: reply Passthrough — hook exits 0, Claude Code
       handles it with its own permission layer
    4. if owner found: publish SessionEvent::ToolApprovalRequested
       { request_id, session_id, driver_id, tool, args, cwd }
       and stash an open request record keyed by request_id
        │
        ▼
Driver session (Phase 4 subscribe stream):
    receives the event as an MCP notifications/message entry
    (CRITICAL UNVERIFIED ASSUMPTION — see Risks §1)
        │
        ▼
Claude Code surfaces the notification in the driver's context
    (either mid-turn or between turns)
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
    2. if scope = AllowAlways: atomically write settings.json
    3. write ApprovalResponse{decision} to the response FIFO
    4. close the request
        │
        ▼
ccom-hook binary:
    reads the response, prints the JSON decision to stdout (or sets exit code)
        │
        ▼
Claude Code:
    proceeds or aborts the tool call per the hook's verdict
```

If `scope = AllowAlways`, the **next** identical prompt fires the hook, the hook re-queries ccom, ccom sees the owning driver, publishes the request, the driver receives it — **wait**. That's wrong. The whole point of "allow always" is that the **next** request is handled by Claude Code's own layer and never reaches ccom.

The correct sequence: when `AllowAlways` fires, ccom writes the rule into `settings.json` **before** replying to the hook. Claude Code re-reads `settings.json` on subsequent tool calls (spike must confirm; see Task 0). The next matching tool call is gated by Claude Code's layer as an auto-allow and the `PreToolUse` hook is **still** invoked — but our hook's pass-through branch is irrelevant because Claude Code never would have blocked in the first place. Actually, the hook runs regardless. That's fine: the hook sees the driver-owned session, publishes to the driver, the driver is now responsible for answering — which defeats the point.

**Resolution:** the hook must ask ccom **before** blocking the driver, and ccom must check whether Claude Code would have auto-allowed this call. Ccom does not want to re-implement Claude Code's matcher. Two options:

- **(a)** Ccom's first line of defense is "read `settings.json`, check matchers, if this call matches an allow rule, reply `Passthrough` without ever paging the driver." This is a shallow matcher implementation — risky, duplicates Claude Code logic. **Reject.**
- **(b)** Trust Claude Code. If Claude Code fired the `PreToolUse` hook at all, it means the call was going to happen — either because it was already allowed or because the user was about to be prompted. The hook input JSON (per spike findings) should tell us which. If Claude Code has already decided "allow" by the time the hook fires, the hook input includes a hint; ccom replies `Passthrough` immediately. If the hook is firing because Claude Code was about to block/prompt, that's when ccom routes to the driver. **Preferred — contingent on the spike confirming the hook input contains this "would-block" signal.**

If the spike shows `PreToolUse` fires **before** Claude Code's own permission check (so the hook has no way of knowing whether Claude Code was going to allow or block), then option (b) doesn't work and we fall to option (a) or to **Option F** (restart-with-resume), neither of which is attractive. This is the single most important spike question. Task 0 must answer it.

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

**Verdict:** documented here for future-us. **Not in Phase 7's task breakdown.** If Option E's hook-input question (above) has no satisfactory answer, we re-open this section.

### Data model delta

```rust
// src/bus.rs (or wherever SessionEvent lives)
pub enum SessionEvent {
    // existing variants…
    ToolApprovalRequested {
        request_id: u64,                // monotonic, minted by ccom
        session_id: usize,              // the child asking
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
    AllowAlways { target: SettingsTarget },
}

pub enum SettingsTarget {
    Project,  // <child_cwd>/.claude/settings.json
    User,     // ~/.claude/settings.json
}

// src/approvals.rs (new)
pub struct PendingApproval {
    pub request_id: u64,
    pub session_id: usize,
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
```

The registry lives on `McpCtx` behind an `Arc`, same ownership pattern as the attachment map from Phase 6. The hook reader thread calls `ApprovalRegistry::open_request(...)` to insert and get a receiver; the MCP handler for `respond_to_tool_approval` calls `resolve(request_id, decision, scope)` to send on the oneshot and remove the entry.

### FIFO pair

Phase 3.5 introduced a one-way FIFO for `Stop` events. Phase 7 needs **bidirectional** per-session communication: hook → ccom (request) and ccom → hook (response). Two options:

- **Unix socket**: one endpoint, bidirectional, clean. More complex (accept loop, per-request peer).
- **Second FIFO pair**: `approval_request.fifo` and `approval_response.fifo` inside each session's `hook_dir`. Consistent with Phase 3.5 conventions.

**Pick:** FIFO pair. Ccom's hook reader thread gains a second file descriptor. Each approval query is a single JSON line on the request FIFO; the response is a single JSON line on the response FIFO, correlated by `request_id`. The hook binary writes to the request FIFO and blocks reading the response FIFO, filtering for its own `request_id` (so a single FIFO pair can serialize multiple concurrent approvals from the same session — Claude Code rarely issues them, but the protocol should support it).

### New MCP tool: `respond_to_tool_approval`

```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RespondToToolApprovalArgs {
    pub request_id: u64,
    pub decision: ApprovalDecisionArg,       // "allow" | "deny"
    #[serde(default)]
    pub scope: ApprovalScopeArg,              // "once" | "allow_always_project" | "allow_always_user"
}
```

Handler:
1. Caller-id check (Phase 6's machinery) — caller must be a driver.
2. Look up the `request_id` in the registry. Reject if not found (already resolved or expired) or if the registered `driver_id` doesn't match the caller — drivers can only answer their own requests.
3. If `scope = AllowAlways { target }`: build the settings.json patch for this tool+args shape, atomically rename-into-place (see Task 4).
4. Resolve the oneshot with the decision.
5. Publish `SessionEvent::ToolApprovalResolved` on the bus (for TUI status-line + audit log).
6. Return `{resolved: true}`.

### Fallback tool: `list_pending_approvals`

If the spike reveals that MCP `notifications/message` entries **don't** reach the driver's LLM context, we need a pull-based alternative. The tool:

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

Returns all pending approvals in the caller's driver scope. The driver would poll (e.g., once per turn) to find requests it needs to answer. This is implemented in Task 7 **only if the spike or integration test reveals the push path doesn't work.**

### Hook binary behavior

New binary: `ccom-hook-pretooluse` (or reuse the existing `ccom-hook` binary with a `--pretooluse` subcommand; pick based on Phase 3.5's binary shape).

```
read stdin JSON -> struct HookInput
read $CCOM_SESSION_ID, $CCOM_HOOK_DIR from env
open request FIFO (write end) and response FIFO (read end)
write {request_id: <local nonce>, ...HookInput} to request FIFO
select on response FIFO with 60s timeout:
    - ApprovalResponse{decision: Allow} → exit 0 with JSON {"decision": "approve"}
    - ApprovalResponse{decision: Deny}  → exit 0 with JSON {"decision": "block", "reason": "..."}
    - ApprovalResponse{decision: Passthrough} → exit 0 with no body (let CC handle it)
    - timeout → exit 0 with JSON {"decision": "block", "reason": "driver timeout"} +
      write a marker into hook_dir/.timeouts so the TUI surfaces it
```

The exact stdout JSON schema and exit-code semantics come from the spike. This pseudocode is illustrative.

### Installation in `Session::spawn`

Phase 6's `Session::spawn` already writes a per-session `hook_dir` and a `.mcp.json`. Phase 7 augments it:

1. Additionally write a `settings.json` into the hook config dir with a `PreToolUse` hook entry pointing at `ccom-hook-pretooluse`.
2. Create the `approval_request.fifo` and `approval_response.fifo` under `hook_dir/`.
3. Pass the `hook_dir` via env (`CCOM_HOOK_DIR`) — already set in Phase 3.5.
4. The per-session `settings.json` must **merge** with the project's existing `.claude/settings.json` rather than replace it. Claude Code's layering rules apply; ccom writes to its own per-session layer which sits alongside the user's project layer. (Spike must confirm Claude Code honors multiple `--settings` sources or a layered config dir.)

**Critical:** installing the hook on every ccom-spawned Claude session means **all** Phase 1–6 behavior is affected, not just Phase 7's driver flow. For Solo sessions, the hook's pass-through branch must be rock solid — any bug here breaks normal ccom usage. Task 5 tests this exhaustively.

## Task breakdown

### Task 0 — Hook viability spike review (~30 min)

**Not a coding task — a read-and-decide gate.**

The spike at `docs/plans/notes/phase-7-hook-spike.md` will (by the time Phase 7 starts) have answered:

1. Does `PreToolUse` fire **synchronously** before Claude Code evaluates its own permission layer?
2. Does it fire **before** or **after** Claude Code's own matcher has already decided allow/deny?
3. Does the hook input JSON contain a field indicating "this call would be blocked without hook intervention" vs "this call would pass without hook intervention"?
4. Is the JSON decision protocol reliable — does `{"decision": "approve"}` on stdout + exit 0 actually cause the tool to fire?
5. Does Claude Code re-read `settings.json` between tool calls, so an in-turn `AllowAlways` write is honored on the next call?
6. Do multiple `settings.json` sources layer cleanly, or does ccom's per-session settings file clobber the user's project settings file?

**If any of (1), (4), or (6) is "no":** Phase 7 is a redesign. Stop and write a new plan.
**If (3) is "no":** Phase 7 proceeds with Option E but we accept that `AllowAlways` has a one-call window where the next tool call is still routed to the driver before settings take effect. Document as a known quirk.
**If (5) is "no":** `AllowAlways` is effectively per-session; document and move on.

Deliverable: a 5-line decision summary appended to the spike notes doc, referenced from this plan.

### Task 1 — Extend hook infrastructure with PreToolUse binary + bidirectional FIFO (~2 hours)

**Files:** `src/bin/ccom-hook-pretooluse.rs` (new), `src/session/spawn.rs`, `src/session/hook_dir.rs` (if exists, else wherever Phase 3.5 put FIFO creation).

- New binary: reads stdin, reads env, opens FIFO pair, writes request, reads response, prints JSON decision. Keep the binary small — under 200 lines. No ccom internals imported; it's a standalone tool-approval relay.
- Extend `Session::spawn` to `mkfifo` both `approval_request.fifo` and `approval_response.fifo` under `hook_dir/`. Set mode 0600.
- Extend the `settings.json` written into the session's hook config to include a `PreToolUse` entry that invokes the new binary. Merge with any project-level settings per Task 0's layering findings.
- The ccom main process's hook reader thread gains a new task: `tokio::spawn` a blocking reader on the request FIFO (one per session). Requests parsed and forwarded to the approval coordinator (Task 2).

**Tests:**
- `hook_binary_writes_request_and_reads_response` — spawn a fake FIFO pair, exec the binary, assert protocol.
- `hook_binary_times_out_after_60s` — with a short override via env for the test.
- `session_spawn_creates_approval_fifos` — unit test on the spawn helper.
- `session_spawn_writes_pretooluse_hook_entry` — read back the settings.json.

### Task 2 — `ToolApprovalRequested` event + approval registry (~1 hour)

**Files:** `src/bus.rs`, `src/approvals.rs` (new), `src/mcp/state.rs`.

- Add `SessionEvent::ToolApprovalRequested` and `SessionEvent::ToolApprovalResolved` variants.
- New module `approvals.rs` with `ApprovalRegistry`, `PendingApproval`, `ApprovalDecision`, `ApprovalScope`, `SettingsTarget`.
- Wire `Arc<ApprovalRegistry>` onto `McpCtx` at construction. The hook reader (Task 1) calls `registry.open_request(session_id, tool, args, cwd)` → returns `(request_id, oneshot::Receiver<ApprovalDecision>)`. The handler (Task 3) calls `registry.resolve(request_id, driver_id, decision)`.
- The approval coordinator (a new small function, can live in `src/approvals.rs`) ties it together: on incoming hook request, resolve owning driver via `caller_scope`-style lookup, either reply `Passthrough` immediately (no driver) or open a registry entry + publish `ToolApprovalRequested`.

**Tests:**
- `registry_open_and_resolve_round_trip`
- `registry_resolve_unknown_request_id_errors`
- `registry_resolve_wrong_driver_rejected` — driver A tries to answer driver B's request
- `approval_coordinator_replies_passthrough_for_orphan_session`
- `approval_coordinator_publishes_event_for_driver_owned_session`

### Task 3 — MCP tool `respond_to_tool_approval` (~1.5 hours)

**Files:** `src/mcp/handlers.rs`, `src/mcp/state.rs`.

- Add `RespondToToolApprovalArgs` and `RespondToToolApprovalWire` structs.
- Handler:
  1. Identify caller via the Phase 6 caller-id machinery (`caller_scope`).
  2. Reject if caller isn't a `Driver`.
  3. Look up request. Reject if not found or driver mismatch.
  4. If `scope = AllowAlways`: invoke the settings writer (Task 4). On settings-writer failure, log loudly and **still** resolve the decision — denying the write shouldn't hang the child.
  5. Call `registry.resolve(...)`.
  6. Publish `ToolApprovalResolved` on the bus.
  7. Return `{resolved: true, settings_written: bool}`.

**Tests** (in `tests/approval_routing.rs`):
- `driver_allows_once_child_proceeds`
- `driver_denies_child_aborts`
- `driver_allows_always_project_writes_settings_json`
- `driver_cannot_answer_other_drivers_request`
- `solo_caller_rejected_from_respond_tool`
- `unknown_request_id_returns_error`

### Task 4 — Settings.json writer helper with atomic rename (~1 hour)

**Files:** `src/settings_writer.rs` (new).

- `fn add_allow_rule(target: &Path, tool: &str, pattern: &MatchPattern) -> io::Result<()>`:
  1. Read current `settings.json` (create empty if missing).
  2. Parse as `serde_json::Value`.
  3. Insert into the `permissions.allow` array (or whatever Claude Code's current schema uses — verify against its docs). De-dup.
  4. Write to `settings.json.tmp.<pid>.<nonce>` in the same dir.
  5. `fsync` the temp file.
  6. `rename` to `settings.json` (atomic on POSIX).
  7. `fsync` the directory (for durability).
- File locking: use `fs2::FileExt::try_lock_exclusive` on `settings.json` during the read-modify-write. Release on drop. Document that concurrent modifications by Claude Code itself may still race — atomic rename at least guarantees no torn writes.
- `MatchPattern` construction from a tool call: for `Bash`, the pattern might be a command prefix or exact match; for `Edit`, a file path. Map from the raw `tool_input` to Claude Code's matcher syntax. **Spike must confirm the matcher syntax.** Worst case, fall back to an exact-match rule for the specific invocation.

**Tests:**
- `writer_creates_settings_if_missing`
- `writer_appends_to_existing_allow_list`
- `writer_is_idempotent_for_duplicate_rules`
- `writer_atomic_rename_preserves_existing_on_parse_error`
- `writer_handles_concurrent_writers` (two threads, assert both rules present)

### Task 5 — Install hook on every `Session::spawn` (~1 hour)

**Files:** `src/session/spawn.rs`.

- Extend `Session::spawn` (the single spawn entry point from Phase 1–6) to always install the PreToolUse hook via the mechanism from Task 1. No feature gate — every Claude session gets the hook.
- The hook's pass-through path (no driver owner → `Passthrough`) is the critical code path for Solo sessions. Exhaustively test it: a Solo session must behave **exactly** as it does in Phase 6.
- Add a `ccom_hook_timing` debug log line around the pass-through so we can measure the per-tool-call overhead in the wild. Remove or demote to trace once we've got numbers.

**Tests:**
- `solo_session_gets_hook_installed`
- `solo_session_hook_replies_passthrough` — end-to-end via a fake tool call writing to the request FIFO
- `driver_owned_child_routes_to_driver` — the happy path
- `attached_session_routes_to_driver_dynamically` — attach post-spawn, next request routes to driver

### Task 6 — Driver-side subscription filter for approval events (~30 min)

**Files:** `src/mcp/handlers.rs` (specifically the `subscribe` tool from Phase 4), `src/mcp/scope.rs` (or wherever Phase 6's scope helper lives).

Phase 6's `subscribe` filters events by caller scope (drivers only see events for sessions in their scope). `ToolApprovalRequested` events carry an explicit `driver_id`; the subscribe filter must use it directly rather than falling back to `spawned_by`/attachment lookup (faster + correct when the child is multi-owned, even though Phase 7 tightens attachments to single-driver).

- Update the scope filter in `subscribe`'s forwarder task: if the event is `ToolApprovalRequested { driver_id, .. }`, forward only to the matching driver. Same for `ToolApprovalResolved`.
- Non-approval events continue to use Phase 6's scope logic unchanged.

**Tests:**
- `approval_event_only_reaches_target_driver`
- `approval_event_not_visible_to_solo_caller`
- `resolved_event_reaches_target_driver`

### Task 7 — Fallback `list_pending_approvals` poll tool (~1 hour — CONDITIONAL)

**Files:** `src/mcp/handlers.rs`.

**Conditional on Task 10 (manual verification) revealing that MCP `notifications/message` entries do not reach the driver's LLM context.** If the push path works, skip this task.

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
- On `ToolApprovalResolved`, decrement. On driver exit, drop the entry and any still-pending approvals fall back to deny + a log line (see Task 9 timeout handling).

**Tests:**
- `status_line_shows_pending_count_for_active_driver`
- `status_line_clears_on_resolution`
- `session_list_marker_tracks_pending_count`

### Task 9 — Timeout + orphan handling (~45 min)

**Files:** `src/approvals.rs`, `src/bin/ccom-hook-pretooluse.rs`.

- Hook binary enforces a 60s timeout (configurable via `CCOM_APPROVAL_TIMEOUT_SECS` env for testing). On timeout, hook emits `block` with reason `"ccom: driver timeout"`.
- Registry side: on timeout the hook moves on, but the registry entry is still there. Add a reaper that sweeps entries older than 120s (2x hook timeout) and drops them with a warning log. Also publish `ToolApprovalResolved { decision: Deny, scope: Once }` so the status-line counter clears.
- On driver exit (the owning driver), all pending approvals for that driver are resolved `Deny` immediately so their children don't hang for the full timeout.
- Surface timeouts to the user: write a line into the TUI's activity log panel (Phase 3.5's existing log surface) with the session id and tool name so the user knows which child failed.

**Tests:**
- `hook_times_out_after_configured_deadline`
- `registry_reaper_clears_stale_entries`
- `driver_exit_fails_all_pending_approvals_for_that_driver`
- `timeout_publishes_resolved_event_for_status_line`

### Task 10 — Integration tests (~1.5 hours)

**File:** `tests/approval_routing.rs` (new).

All tests construct a driver session via `SessionManager::spawn` with `role: Driver { .. }`, spawn a child via the Phase 6 `spawn_session` tool, then simulate `PreToolUse` hook requests by writing to the child's approval request FIFO directly. Cover:

1. `child_of_driver_routes_approval_to_driver_and_allows_once`
2. `child_of_driver_routes_approval_to_driver_and_denies`
3. `child_of_driver_allow_always_project_writes_settings_json`
4. `child_of_driver_allow_always_user_writes_home_settings_json`
5. `solo_session_hook_replies_passthrough_without_publishing`
6. `late_attached_session_starts_routing_on_next_request`
7. `two_concurrent_approvals_from_same_child_serialize_correctly`
8. `driver_exit_denies_all_pending_approvals_for_its_children`
9. `hook_timeout_denies_and_surfaces_to_tui_log`
10. `driver_cannot_answer_another_drivers_request`
11. `settings_json_round_trip_under_concurrent_write`

### Task 11 — End-to-end manual verification (~1 hour)

Set up a real test scenario:

1. Create a throwaway repo with a restrictive `.claude/settings.json` that denies `Bash` calls outside a narrow whitelist.
2. `cargo run -- --driver --spawn-policy budget --budget 3`
3. Tell the driver: "Spawn a helper in `<throwaway-repo>`. Have it run `ls -la src/` and report the output."
4. Observe:
   - Driver spawns child silently (Phase 6 happy path).
   - Child calls `Bash` for `ls`.
   - Approval routes to driver as an in-chat notification.
   - Driver's LLM sees it, asks the human in chat, or decides autonomously.
   - Human says "always allow `ls` in this repo".
   - `respond_to_tool_approval` fires with `AllowAlways { target: Project }`.
   - Child proceeds.
   - Child is then asked to run `ls -la docs/` — approval does **not** fire; Claude Code's own layer handles it.
5. Verify `<throwaway-repo>/.claude/settings.json` contains the new allow rule.
6. Spot check: kill the driver mid-approval → child's pending request resolves to Deny within a few seconds.
7. Spot check: take a Solo session, tool call with a denied Bash — verify TUI modal still fires (pass-through intact).
8. Spot check: `ccom_hook_timing` log line shows sub-millisecond pass-through for Solo sessions.

**If the push path fails** (driver's LLM never surfaces the approval notification), immediately implement Task 7 (`list_pending_approvals`) and retry with the driver's prompt updated to poll each turn.

## Parallelism plan

- **Task 0** (spike review) is a hard gate — everything else waits.
- **Tasks 1 + 2** sequential (registry needs the FIFO infra to have somewhere to call into). Go into one worktree.
- **Tasks 3 + 4** can run in parallel once Task 2 is on the branch — Task 3 touches MCP handlers, Task 4 is a self-contained new module. Two worktrees.
- **Tasks 5 + 6** sequential after 3+4 merge. Task 5 touches spawn, Task 6 touches subscribe — they don't collide but Task 5 enables the end-to-end test for Task 6 so it's cleaner sequenced.
- **Task 7** is conditional and happens only after Task 11 manual verification.
- **Tasks 8 + 9 + 10** can be parallel after Task 5 lands: Task 8 is UI, Task 9 is approvals module, Task 10 is tests. Three worktrees, minimal collision.
- **Task 11** is the final manual gate.

Subagent decomposition once Tasks 1+2 are on the phase branch:
- **Subagent A (MCP handler + tool side)**: Tasks 3, 6, 7 (if needed). Touches `handlers.rs`, `state.rs`, subscribe filter.
- **Subagent B (settings + approvals plumbing side)**: Tasks 4, 9. Touches `settings_writer.rs`, `approvals.rs`, hook binary.
- **Subagent C (TUI side)**: Task 8. Touches `ui/status_line.rs`, `app/mod.rs`.

Collision surface: `app/mod.rs` (C adds pending counter field; A/B don't touch it). `approvals.rs` (B touches for timeout, A reads for filtering — interface is stable by end of Task 2, so parallel work is fine).

## Risks

1. **MCP `notifications/message` delivery into the driver's LLM context is unverified.** This is the #1 risk and the whole premise of the push path. Phase 4's `subscribe` tool forwards events as MCP notifications, but we have **not** confirmed end-to-end that Claude Code surfaces those into the model's context window mid-turn or between turns. If it doesn't, Phase 7 falls back to the polling tool (Task 7), which requires the driver's prompt template to call `list_pending_approvals` each turn — a UX regression. **Mitigation:** Task 11 tests this explicitly and falls back fast. **Escalation:** if polling is also unreliable (e.g., driver goes silent for many minutes), we need a third path — possibly a side channel that writes the request into the driver's TTY directly, which is ugly but always works.

2. **Hook spike may force a redesign.** Task 0 lists six must-answer questions. If any of (1), (4), or (6) fails, Phase 7 as written doesn't build. Mitigation: spike first, read the notes before starting Task 1.

3. **"Would Claude Code have allowed this anyway?" ambiguity.** If the hook input doesn't distinguish "CC was about to allow" from "CC was about to block," every tool call on a driver-owned session routes to the driver — including trivially-allowed ones. The driver gets spammed. **Mitigation:** ccom adds a shallow "is this tool call already listed in the project settings allow list" precheck (just string-match against the permissions.allow array, no matcher parsing). Imperfect but reduces noise. If that's not enough, we may need to reluctantly implement a more complete matcher — which is exactly the "third permissions layer" we said we wouldn't. Tradeoff to revisit after Task 11.

4. **Multi-driver attachment collision.** Phase 6 allows a session to be attached to multiple drivers. Phase 7 can only route an approval to one. **Mitigation:** Task 1 tightens attachment to single-driver — refuse at attach time if already attached. The attach-to-driver TUI action from Phase 6 Task 5 needs a small guard update. Document this as a narrowing of the Phase 6 affordance.

5. **Concurrent `settings.json` mutation.** Claude Code itself writes `settings.json` when the user clicks "Always allow" in its own UI. Ccom + Claude Code writing concurrently to the same file without a shared lock protocol is a recipe for lost updates. **Mitigation:** use OS-level flock in the writer + atomic rename. Claude Code (presumably) doesn't flock — so in the worst case our write happens, then Claude Code overwrites it a moment later, and we've lost the rule. The next matching call will route to the driver again, which is annoying but not catastrophic. Document.

6. **Timeout UX.** A 60-second timeout on a driver approval means a child may hang for a full minute before denying. For interactive tool calls this is bad. **Mitigation:** surface pending approvals prominently in the status line (Task 8) so the human is aware. Consider a second, shorter "soft timeout" (10s) that pings the driver session via a second notification to nudge it.

7. **Pass-through overhead on high-frequency tool callers.** A Solo session running hundreds of tool calls a minute pays fork+exec per call. Still sub-millisecond, but on constrained hardware could become visible. **Mitigation:** measure in Task 11 via the `ccom_hook_timing` log. If the numbers are bad, fall back to Option F for Solo sessions only (no hook installed unless adoption happens) — but that re-opens the late-attachment problem. Do not optimize prematurely.

8. **Hook binary is a separate deployable.** Unlike the rest of ccom, the hook binary must be discoverable at an absolute path by Claude Code at tool-call time. **Mitigation:** bundle it in the same `cargo build` output (`src/bin/ccom-hook-pretooluse.rs`) and have `Session::spawn` record its absolute path via `std::env::current_exe()`-relative resolution at startup, then embed that absolute path in the per-session `settings.json`. Don't rely on `PATH`.

9. **Security: request FIFO is writable by anyone who can write to the hook dir.** Currently mode 0600 and under the user's home. Fine for single-user workstations. Document the assumption explicitly so we notice if ccom ever grows a multi-user mode.

10. **Hook binary errors leave child hanging.** If the hook binary crashes (segfault, panic, disk full writing the request) the child session blocks on an empty pipe forever. **Mitigation:** Claude Code's own hook timeout (if any — spike to confirm) is our last line of defense. Beyond that, the 120s registry reaper catches stuck entries from the ccom side. Accept the possibility that a crashed hook fails the tool call after Claude Code's deadline expires.

## Verification

- `cargo test` — existing Phase 6 tests still pass, new Phase 7 tests pass (expect ~30+ new test functions across tasks 1–10).
- `cargo clippy --all-targets` — zero warnings.
- `cargo fmt --check` — clean.
- `cargo build --release` — compiles; `ccom-hook-pretooluse` binary is emitted.
- `tests/approval_routing.rs` integration tests pass.
- Manual Task 11 end-to-end with a real repo + real driver + real child:
  - Approve once → child proceeds.
  - Deny → child aborts.
  - Allow always project → `settings.json` updated; next identical call bypasses ccom.
  - Late attach → next request routes.
  - Solo session pass-through → TUI modal still fires.
  - Driver exit → pending approvals deny within 2s.
  - Timeout → child aborts after 60s with a surfaced error.

## Acceptance criteria

- A driver session receives child tool approvals as conversational events, answers them, and the child proceeds or aborts accordingly — **no modal on the driver side**.
- "Allow always" writes a rule into the correct `settings.json` (project or user, as the driver specifies). The next identical tool call is handled by Claude Code's own permission layer and never reaches ccom.
- A Solo session's behavior is **unchanged** from Phase 6 — the hook's pass-through path fires and Claude Code's own permission flow is undisturbed.
- A session attached to a driver post-spawn starts routing approvals on the very next tool call, with no restart.
- Driver exit cleanly fails any in-flight approvals for its children within seconds.
- A 60-second hook timeout is enforced; timeouts are surfaced in the TUI activity log.
- No "third permissions layer" exists in ccom — the codebase contains no matcher logic, no rule engine, no persistent policy state beyond the in-memory `ApprovalRegistry` (which is request tracking, not policy).
- Phase 6's driver scope and attachment semantics remain intact; Phase 7's additions are strictly routing.

## Effort estimate

- Task 0 (spike review): 30 min
- Task 1 (hook binary + FIFO pair): 2 hours
- Task 2 (event + registry): 1 hour
- Task 3 (respond_to_tool_approval): 1.5 hours
- Task 4 (settings writer): 1 hour
- Task 5 (spawn integration): 1 hour
- Task 6 (subscribe filter): 30 min
- Task 7 (list_pending_approvals fallback): 1 hour **(conditional)**
- Task 8 (status-line hint): 1 hour
- Task 9 (timeout + orphan handling): 45 min
- Task 10 (integration tests): 1.5 hours
- Task 11 (manual end-to-end): 1 hour

**Total: ~11.75 hours** of focused work (10.75 without the conditional Task 7), with ~3 hours parallelizable across two to three subagents once Task 2 lands on the phase branch.

**Contingent on the hook spike.** If Task 0's review concludes the spike has not answered the must-answer questions, stop and block Phase 7 on a follow-up spike rather than guess.
