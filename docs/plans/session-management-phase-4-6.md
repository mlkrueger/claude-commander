# Session Management — Phases 4–6 Implementation Plan

Status: queued (do not start until Phases 1–3 have merged)
Date: 2026-04-11
Design spec: [`docs/designs/session-management.md`](../designs/session-management.md)

This plan delivers the bundled MCP server and the driver-session
feature. These phases are **independent of the Model Council** — they
can land before, after, or interleaved with the Council, as long as
Phases 1–3 from
[`session-management-phase-1-3.md`](session-management-phase-1-3.md)
have merged to `main` first.

## Ground rules

Same as Phases 1–3. Repeated here for agents reading this doc cold:

1. **No work on `main`.** Branch from fresh `main` for every phase.
2. **One phase per PR.** Do not combine phases. Do not start the next
   phase until the current one has merged.
3. **Branch naming.** `session-mgmt/phase-N-<slug>`.
4. **Parallel tasks use git worktrees** off the phase branch. Each
   parallel task opens its own PR *into* the phase branch (not into
   `main`); the phase branch rolls up and then PRs to `main`.
5. **Definition of done.** Build clean, tests pass, manual
   verification completed per `~/.claude/CLAUDE.md`, PR merged via
   squash.
6. **Never commit without explicit approval.**

---

## Sequencing constraints — what CAN'T be parallelized

### Cross-phase (hard serial between phases)

- **Phases 1–3 → Phase 4.** Phase 4's handlers call Phase 1's bus
  (`subscribe`), Phase 2's `send_prompt` turn-id plumbing
  (indirectly through `read_response` long-polling on
  `ResponseComplete`), and Phase 3's `get_response` /
  `get_latest_response` accessors. **Do not start Phase 4 until
  Phase 3 has merged to `main`.** Every read-only tool in Phase 4
  has a direct code dependency on something added in 1–3.
- **Phase 4 → Phase 5.** Phase 5's write tools plug into the Phase
  4 MCP server — the `src/mcp/` module, the dedicated-thread
  runtime, the shared-state adapter, and the `#[tool]` impl block
  all live there. Phase 5 adds methods to that impl block and
  introduces the confirmation modal. **Do not start Phase 5 until
  Phase 4 is merged.** No worktree can fake the scaffolding.
- **Phase 5 → Phase 6.** Phase 6 reuses Phase 5's confirmation
  modal for the `Ask` spawn policy, and its kill-policy update
  rewrites logic that Phase 5 introduces. **Do not start Phase 6
  until Phase 5 is merged.**

### Within-phase hard orderings

**Phase 4:**
- **Task 0 (rmcp spike) is a hard gate for everything else.** It
  can run in a worktree of its own (cut from `main`, since no
  phase-branch code exists yet), but no other Phase 4 task starts
  until the spike findings are documented in
  `docs/plans/notes/rmcp-spike.md` and reviewed. If the spike
  reveals that rmcp 1.4's session resumption is broken for Claude
  Code's client, the phase is **blocked** pending a conversation
  about the fallback (older rmcp, `rust-mcp-sdk`, unix socket).
- Task 0 → Task 1 (dep add). Don't bring `rmcp` / `tokio` into
  `Cargo.toml` until the spike has proven the chosen version is
  viable, otherwise you commit a rollback.
- Task 1 → Task 2 (`src/mcp/` scaffolding) → Task 3 (runtime) →
  Task 4 (shared state). These four are **strictly sequential**;
  each references types introduced by the previous. Do them in
  one worktree, on the phase branch, back to back.
- Task 5 (three tool handlers) can parallelize **only after Tasks
  2–4 have landed on the phase branch.** A handler worktree cut
  before then has no `McpServer` type to hang off, no shared
  state, and no `#[tool]` impl block to extend.
- Tasks 6 (`.mcp.json` generator), 7 (loopback safety), 8
  (integration test), 9 (end-to-end verify) are **sequential**
  finish on the phase branch after the three handler PRs merge
  in. Do not open the phase PR against `main` until all of 6–9
  are complete.

**Phase 5:**
- There is no strict prerequisite task inside Phase 5 —
  `send_prompt` and `kill_session` + modal are genuinely
  independent from line one, as long as they both build against
  the Phase 4 scaffolding (which is already on `main` by the time
  Phase 5 starts). Split into two worktrees immediately off the
  phase branch; no serial warm-up required.
- Task 4 (scope check — `NotFound` for sessions not in
  `SessionManager`) must be present in **both** worktrees' merged
  result. Easiest: land it on one worktree's PR and ensure the
  other rebases onto it before merging. Don't forget this — it's
  the tool's only safety net in Phase 5.
- Tasks 5 (integration test) and 6 (end-to-end verify) are
  sequential finish after both worktrees merge.

**Phase 6:**
- Task 1 (`SessionRole` + `SpawnPolicy` + `spawned_by` field) is
  a prerequisite for **every other task in the phase.** The
  config surface, the `spawn_session` handler, the scope filters,
  the UI markers, and the kill policy all reference types
  introduced here. Do Task 1 in-line on the phase branch with no
  parallelism. Do not branch worktrees until Task 1 is committed.
- Task 2 (config surface) depends on Task 1 (`SpawnPolicy` enum).
  Sequential.
- Tasks 3 + 6 (MCP side: `spawn_session` tool, kill-policy
  rewrite) and Tasks 5 + 7 (UI side: attach-to-driver action,
  session list markers) can parallelize in two worktrees **after
  Tasks 1–2 have merged onto the phase branch.** They share no
  files and touch different modules.
- Task 4 (scope filtering on Phase 4's read-only handlers) rides
  on the MCP-side worktree — it's a small edit to files that
  worktree already owns.
- Task 8 (budget reset on driver exit), Task 9 (integration
  tests), and Task 10 (end-to-end verify) are **sequential**
  finish on the phase branch after both parallel worktrees merge
  in.

---

## Phase 4 — In-process MCP server, read-only tools

**Branch:** `session-mgmt/phase-4-mcp-readonly`
**Design ref:** session-management §5
**Depends on:** Phases 1–3 merged to `main`.
**Blocks:** Phases 5–6.

### Goal

Ship an embedded `rmcp` HTTP MCP server on loopback that Claude Code
sessions can connect to. Read-only tools only: `list_sessions`,
`read_response`, `subscribe`. No writes, no spawning.

### Task 0 — Spike & smoke tests (BLOCKING)

Before any production code lands, resolve the two open questions from
session-management §7 that were flagged for pre-implementation
verification:

- **0a.** Can `rmcp` 1.4's `transport-streamable-http-server` handle
  session resumption the way Claude Code's MCP client expects?
  Stand up a minimal toy rmcp server in a scratch crate, configure
  Claude Code to connect, verify it can disconnect and reconnect
  without the server losing state.
- **0b.** Pin the exact `#[tool]` attribute-macro syntax for 1.4.
  Clone `modelcontextprotocol/rust-sdk`, find the server example
  closest to our shape (trait-impl with multiple tools), and copy
  its skeleton.

Document findings in `docs/plans/notes/rmcp-spike.md` (create the
`notes/` subfolder). If 0a fails, **stop and report** — we may need
to pin an older rmcp version, switch to `rust-mcp-sdk`'s `HyperServer`,
or fall back to unix sockets via `transport-async-rw`. Do not try to
work around the issue without a conversation.

This spike is a **prerequisite** for the rest of Phase 4. It can run
in a separate worktree, but no other Phase 4 task starts until it's
resolved.

### Tasks

1. **Dep addition.** Add `rmcp = { version = "1.4", features =
   ["server", "macros", "transport-streamable-http-server"] }` to
   `Cargo.toml`. Add `tokio = { version = "1", features = ["rt",
   "rt-multi-thread", "macros"] }`. This is the first phase that
   introduces tokio to the dep graph — note this in the PR
   description.

2. **Module scaffolding.** Create `src/mcp/` with:
   - `mod.rs` — re-exports.
   - `server.rs` — `McpServer` struct, lifecycle (`start`, `stop`),
     dedicated-thread runtime.
   - `state.rs` — shared state adapter (`Arc<Mutex<…>>` wrapper over
     the bits of `SessionManager` and `EventBus` the handlers need).
   - `handlers.rs` — `#[tool]`-annotated impl block.

3. **Dedicated-thread runtime.** In `McpServer::start`:
   - `std::thread::Builder::new().name("ccom-mcp").spawn(...)`.
   - Inside the thread: `tokio::runtime::Builder::new_current_thread()
     .enable_all().build()?`, then `rt.block_on(run_server(...))`.
   - Bind `127.0.0.1:0`, capture the assigned port, send it back to
     the main thread via a `std::sync::mpsc::sync_channel(1)`.
   - Store the thread handle and a shutdown signal
     (`Arc<AtomicBool>` or `tokio::sync::oneshot`) on `McpServer`
     so `stop` can join cleanly.

4. **Shared state.** Design the minimum surface the read-only
   handlers need:
   - `list_sessions` → a snapshot struct, not a reference to
     `SessionManager`.
   - `read_response` → calls `get_response` / `get_latest_response`.
   - `subscribe` → calls `EventBus::subscribe` (from Phase 1) and
     wraps the `mpsc::Receiver` in a stream rmcp can serve.
   Prefer passing `Arc<Mutex<ReadOnlyCtx>>` where `ReadOnlyCtx` is a
   struct of `Arc`s, not a lock over the whole `App`. Snapshotting
   is cheap and avoids holding a lock across `.await` points.

5. **Tool handlers** (read-only):
   - `list_sessions() -> Vec<SessionSummary>` — id, label, model,
     role, status, last activity timestamp.
   - `read_response(session_id, turn_id?) -> Option<StoredTurn>` —
     see session-management §5. If the requested turn isn't
     completed yet, subscribe to the bus and long-poll until
     `ResponseComplete { turn_id }` fires, with a caller-specified
     timeout (default 60s, max 5min).
   - `subscribe(session_ids?, events?) -> stream<SessionEvent>` —
     streams filtered events to the caller for as long as the MCP
     connection stays open.

6. **Auto-generated `.mcp.json`.** When Commander spawns a session,
   write (or append to) a session-local `.mcp.json` pointing at
   `http://127.0.0.1:<port>/mcp` so the child Claude Code picks up
   the server automatically. Verify the exact schema Claude Code
   expects against its docs during Task 0b.

7. **Port and binding safety.** Bind `127.0.0.1` only. Do not bind
   `0.0.0.0`. Add a lint / runtime assert so a stray dev change
   can't accidentally expose the server.

8. **Integration test.** `tests/mcp_readonly.rs`: start `McpServer`,
   open a raw MCP client over HTTP (use `rmcp`'s client features on
   the test side), call `list_sessions` and `read_response`, assert
   expected shapes.

9. **End-to-end verification.** Launch `cargo run`, spawn a Claude
   Code session (no driver flags yet — just a normal session with
   the auto-generated `.mcp.json`), ask the session to "call the
   `list_sessions` tool and tell me what you see." Confirm the tool
   call succeeds and returns the session list including itself.

### Parallelism

- **Task 0 (spike) + Task 1 (dep add):** sequential. Dep add waits
  for spike results.
- **Task 2 (scaffolding) + Task 3 (runtime) + Task 4 (shared
  state):** sequential inside one worktree — each depends on the
  previous.
- **Task 5 (tool handlers):** three **PARALLEL** sub-tasks once
  Tasks 2–4 are on the phase branch. One worktree per handler:
  ```
  git worktree add ../ccom-phase-4-list   session-mgmt/phase-4-mcp-readonly
  git worktree add ../ccom-phase-4-read   session-mgmt/phase-4-mcp-readonly
  git worktree add ../ccom-phase-4-sub    session-mgmt/phase-4-mcp-readonly
  ```
  Each handler is a self-contained `#[tool]` method plus its tests.
  They merge back into the phase branch as three sub-PRs.
- **Tasks 6–9:** sequential finish.

### Acceptance

- rmcp spike findings documented in `docs/plans/notes/rmcp-spike.md`.
- New deps appear in `Cargo.toml`: `rmcp` 1.4 + `tokio` 1.
- A real Claude Code session connects to the embedded server and
  successfully calls all three read-only tools.
- Server binds loopback only; verified in the integration test and
  at runtime.
- All existing tests still pass.

---

## Phase 5 — MCP write tools

**Branch:** `session-mgmt/phase-5-mcp-write`
**Design ref:** session-management §5 (tools), §6 (safety stubs)
**Depends on:** Phase 4 merged to `main`.
**Blocks:** Phase 6.

### Goal

Add `send_prompt` and `kill_session` MCP tools. No driver role yet —
every call is user-confirmed via a TUI modal, and both tools only
operate on sessions the main TUI already knows about.

### Tasks

1. **`send_prompt` tool handler.** Thin wrapper over
   `SessionManager::send_prompt` from Phase 2. Returns the new
   `turn_id` so the caller can follow up with `read_response(id,
   turn_id)`. No confirmation modal (per session-management §6 —
   "gating prompts is theater").

   **Sanitize the `text` argument** before passing to
   `SessionManager::send_prompt`. Driver-supplied text flows
   verbatim into the target session's PTY, where ANSI escape
   sequences and control characters are interpreted by the
   underlying runner (Claude Code, Gemini CLI, etc.) and by any
   subscriber that displays input echoes. Untrusted MCP callers can
   inject terminal escapes, hijack the runner's input parsing, or
   smuggle prompt-injection payloads. Required policy at the MCP
   tool boundary:
   - Strip raw control characters `< 0x20` except a small allowlist
     (newline `\n`, tab `\t`) — and even allowed ones get normalized
     (`\r`, `\r\n`, `\n` all collapse so the submit chord stays
     under `SUBMIT_SEQUENCE`'s control).
   - Strip ANSI CSI sequences (`ESC[…`) and OSC sequences
     (`ESC]…BEL` / `ESC]…ESC\`).
   - Cap total length at a reasonable bound (e.g. 16 KB) so a
     misbehaving driver can't send a multi-megabyte payload.
   - Reject empty post-sanitization text with a clear error.

   Same threat model as the label-sanitization task in Phase 6.
   **Tracked from PR #8 review security item, see
   `docs/pr-review-pr8.md`.**

2. **`kill_session` tool handler.** Always triggers a TUI
   confirmation modal in Phase 5 (no driver ownership yet — the
   silent-kill-for-your-own-children policy arrives in Phase 6).

3. **TUI confirmation modal.** New modal in
   `src/ui/panels/mcp_confirm.rs`:
   - Blocks the MCP handler on a `std::sync::mpsc::sync_channel(0)`
     (or `tokio::sync::oneshot` bridged across the thread boundary
     — whichever is cleaner given where the handler lives).
   - Renders "MCP: `<tool>` on session `<id>` — allow? (y/n)".
   - On `y` resolves the handler's oneshot with `Allow`; on `n`
     resolves with `Deny`; on ESC, defaults to `Deny` and surfaces a
     cancellation error to the caller.
   - Shows a small spinner in the session list row for the target
     session while the confirmation is pending.

4. **Safety scope for v1.** The write tools only operate on sessions
   currently owned by the TUI. Add an explicit check: if `session_id`
   doesn't exist in `SessionManager`, return `NotFound` without
   touching anything else.

5. **Integration test.** `tests/mcp_write.rs`:
   - Start `McpServer`, spawn a target session.
   - From a test MCP client, call `send_prompt`, assert the target
     received the bytes.
   - Call `kill_session`, auto-answer the confirmation (inject a
     test-only auto-allow flag), assert the session exits.

6. **End-to-end verification.** Launch `cargo run`, spawn two
   sessions A and B. From A, ask it to call `send_prompt` on B's id
   with some text. Confirm the text appears in B. Then from A, ask
   it to `kill_session` B — confirm the modal pops in the TUI,
   pressing `y` kills B, pressing `n` leaves B alive.

### Parallelism

- **Task 1** (`send_prompt`) and **Tasks 2 + 3** (`kill_session` +
  modal) are independent once Phase 4's scaffolding is in place.
  **PARALLEL:** two worktrees.

  ```
  git worktree add ../ccom-phase-5-send session-mgmt/phase-5-mcp-write
  git worktree add ../ccom-phase-5-kill session-mgmt/phase-5-mcp-write
  ```

- Task 4 (scope check) lands on either worktree's PR — it's a
  one-place addition.
- Tasks 5–6 sequential on the phase branch after both parallel
  sub-PRs merge.

### Acceptance

- External Claude session can send a prompt into any TUI-owned
  session.
- External Claude session's `kill_session` request produces a TUI
  modal; user can allow or deny.
- TUI-owned sessions not registered in `SessionManager` can't be
  touched by these tools (`NotFound`).
- Manual verification of both happy path and denial path.

---

## Phase 6 — Driver role + `spawn_session`

**Branch:** `session-mgmt/phase-6-driver-role`
**Design ref:** session-management §6 (safety policy in full)
**Depends on:** Phase 5 merged to `main`.

### Goal

Add the `driver` session role, the `spawn_session` MCP tool, the
`driver.spawn_policy` config, scope-restricted tool views, and the
"attach to driver" UX. This is the phase that makes Commander
genuinely a fleet-management tool.

### Tasks

1. **Session role data model.**
   - New enum `SessionRole`:
     ```rust
     enum SessionRole {
         Solo,
         Driver { spawn_budget: u32, spawn_policy: SpawnPolicy },
     }
     enum SpawnPolicy { Ask, Budget, Trust }
     ```
   - New field on `Session`: `spawned_by: Option<SessionId>` — who
     spawned this session. `None` = spawned by the user.
   - Migration: all existing sessions get `Solo`.

2. **Config surface.** `driver.spawn_policy` can be set three ways,
   precedence highest to lowest:
   - CLI flag on `ccom new --driver --spawn-policy budget --budget 3`.
   - TOML config: `~/.config/claude-commander/driver.toml` with a
     default policy and budget.
   - Fallback: `ask`.

3. **`spawn_session` MCP tool.**
   - Validates the caller is a driver session (look up the caller's
     session id via the MCP transport's session-aware context, which
     the Task 0 spike confirmed works).
   - Enforces nesting cap: a driver cannot spawn another driver
     (depth ≤ 1 in v1).
   - Applies `spawn_policy`:
     - `Ask` → TUI confirmation modal (reuse Phase 5's).
     - `Budget` → decrement; if `spawn_budget == 0`, fall back to
       `Ask` for this one spawn.
     - `Trust` → silent.
   - On allow: spawns a new session with `spawned_by = Some(driver_id)`.
   - **Sanitize the `label` argument** before passing to
     `SessionManager::spawn`. A driver-supplied label flows through
     `SessionEvent::Spawned` and into the session list UI, log
     consumers, and any future MCP `subscribe` stream — untrusted
     content can include ANSI escape sequences, control characters,
     or terminal injection attacks against any subscriber that
     renders or logs labels. Required policy: strip ASCII control
     characters (`< 0x20` except none, plus `0x7f`), strip or escape
     ANSI CSI sequences, cap length to a reasonable bound (e.g. 64
     characters), reject empty labels. **Tracked from PR #7 review
     security item, see `docs/pr-review-pr7.md`.**

4. **Scope-restricted tool views.** For a driver caller, the
   existing read-only tools (`list_sessions`, `read_response`,
   `subscribe`) filter to sessions where
   `spawned_by == Some(driver_id)` OR the session has been
   explicitly attached to the driver via Task 5.

5. **"Attach to driver" action.** New TUI action in the session
   picker modal: select a driver → pick an existing session → mark
   it attached to the driver. Store the attachment in
   `App.driver_attachments: HashMap<DriverId, HashSet<SessionId>>`.
   Surfaces in `list_sessions` / `read_response` / `subscribe` scope
   filters.

6. **Kill policy update.** Phase 5's always-prompt-on-kill becomes:
   - If the caller is a driver and the target is in the driver's
     scope (own child or attached), allow silently.
   - Otherwise prompt via the Phase 5 modal.

7. **UI markers.**
   - Session list: driver sessions render with a distinct icon/color
     (e.g. a `◆` prefix). Children of a driver render indented under
     the driver with their parent id shown.
   - Status bar: when viewing a driver, show its remaining budget if
     any.

8. **Budget reset on driver exit.** When a driver session exits, any
   `budget` counter is dropped and the attachment set for that
   driver is cleared. Existing Phase 5 `kill_session` flow applies
   to children — they aren't auto-killed when the parent exits,
   they just become orphans. Document this.

9. **Integration tests.** `tests/driver_spawn.rs`:
   - Spawn a driver with `budget = 2`. Have it spawn two children
     silently. Third spawn should fall back to `Ask`.
   - Spawn a driver with `ask`. First spawn shows modal; test
     approves it. Child is created.
   - Driver `list_sessions` only returns its own children +
     attached, not unrelated sessions.
   - Nesting cap: a driver trying to spawn another driver is
     rejected.

10. **End-to-end verification.** Launch `cargo run`, spawn a driver
    session via `ccom new --driver --spawn-policy budget --budget 3`,
    ask it: "Spawn three helper sessions and have each of them list
    the files in `src/`. Report back what you find." Confirm it
    works end to end: three spawns succeed silently, each child
    reports independently, the driver aggregates.

### Parallelism

- **Task 1** (data model) and **Task 2** (config) sequential —
  config depends on the `SpawnPolicy` type.
- Once Tasks 1–2 are on the phase branch, Tasks 3 + 6 (MCP side:
  tool + policy enforcement in kill) and Tasks 5 + 7 (UI side:
  attach action + session list markers) are **PARALLEL** in two
  worktrees.

  ```
  git worktree add ../ccom-phase-6-mcp session-mgmt/phase-6-driver-role
  git worktree add ../ccom-phase-6-ui  session-mgmt/phase-6-driver-role
  ```

- Task 4 (scope filtering) is a thin change to the Phase 4 handlers
  — can ride on the MCP worktree.
- Tasks 8–10 sequential finish on the phase branch.

### Acceptance

- A driver session can spawn, prompt, read, and kill its own
  children with no user-visible friction beyond the initial setup.
- A driver cannot see or touch unrelated sessions by default.
- Attaching a user-owned session to a driver brings it into scope.
- Nesting depth capped at 1.
- Spawn policy precedence (CLI > TOML > fallback) verified manually.
- End-to-end test with a real driver-orchestrated fleet passes.

---

## Overall sequencing recap

```
main
 │
 ├─ Phase 1 (event bus)      ── PR ── merge
 │
 ├─ Phase 2 (write path)     ── PR ── merge
 │    (parallel sub-tasks inside: send_prompt, broadcast)
 │
 ├─ Phase 3 (detector+store) ── PR ── merge
 │    (parallel sub-tasks inside: store, detector)
 │
 │   ── Model Council can start here if desired ──
 │
 ├─ Phase 4 (MCP read-only)  ── PR ── merge
 │    (spike first; then 3-way parallel handlers)
 │
 ├─ Phase 5 (MCP write)      ── PR ── merge
 │    (parallel sub-tasks: send_prompt, kill+modal)
 │
 └─ Phase 6 (driver role)    ── PR ── merge
      (parallel sub-tasks: MCP side, UI side)
```

Every arrow between phases is a merge to `main`. No phase starts
until the previous phase's PR is merged. No work happens on `main`
directly.
