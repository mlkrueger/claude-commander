# Phase 6 — Driver Role + `spawn_session`

**Branch:** `session-mgmt/phase-6-driver-role`
**Depends on:** Phases 1–5 merged (Phase 5 is `main`'s current head)
**Blocks:** Nothing in the current roadmap — Phase 6 is the last session-management phase
**Design refs:**
- `docs/designs/session-management.md` §6 (driver sessions & safety policy — the authoritative spec)
- `docs/plans/session-management-phase-4-6.md` §Phase 6 (master plan)
- `docs/pr-review-pr7.md` (label sanitization security item)

## Context

Phase 6 turns Commander from "managed multi-session TUI" into a **fleet orchestrator**. It adds a `driver` session role whose purpose is to spawn, prompt, read, and kill a fleet of non-driver children with reasonable guardrails and no nagging confirmation fatigue. The policy is captured in design doc §6: gates exist but a driver that can spawn can already type, so gates are placed only where they prevent real damage (`spawn_session` and cross-scope `kill_session`), not where they'd just annoy the user (`send_prompt` — still unconfirmed).

Three policy levers at the user's disposal:
- **`ask`** — TUI modal on every spawn. Safe default for untrusted drivers.
- **`budget`** — pre-authorize N silent spawns; after budget is exhausted, fall back to `ask`. **Recommended default.**
- **`trust`** — silent; opt-in per driver run.

## Architecture

### Data model delta

```rust
// src/session/types.rs
pub enum SessionRole {
    Solo,                                // existing behavior (all Phase 1-5 sessions)
    Driver {
        spawn_budget: u32,               // remaining silent spawns
        spawn_policy: SpawnPolicy,
    },
}

pub enum SpawnPolicy {
    Ask,     // modal every time
    Budget,  // silent until spawn_budget hits 0, then modal
    Trust,   // always silent
}

pub struct Session {
    // existing fields…
    pub role: SessionRole,                    // NEW, default Solo
    pub(super) spawned_by: Option<usize>,     // NEW, driver id if applicable
}
```

### Attachment store

Explicit driver-owned sessions separate from parent-child spawning:

```rust
// on App
pub(crate) driver_attachments: HashMap<usize, HashSet<usize>>,
// key = driver session id, value = set of session ids attached to this driver
```

Attachments are TUI-initiated (new "attach to driver" action in the session picker). They persist until the driver exits; then the entry is dropped.

### Scope resolution

A single helper used by every scope check:

```rust
// src/mcp/state.rs (or a new src/mcp/scope.rs)
impl McpCtx {
    /// Returns the set of session ids a driver is allowed to see /
    /// touch. Always includes the driver's own children
    /// (`spawned_by == Some(driver_id)`) plus explicit attachments
    /// from the App's attachment map.
    pub fn driver_scope(&self, driver_id: usize) -> HashSet<usize>;

    /// For a given caller (identified by MCP session → ccom session id),
    /// returns `Scope::Full` for Solo callers or `Scope::Restricted(set)`
    /// for drivers.
    pub fn caller_scope(&self, caller_id: usize) -> Scope;
}

pub enum Scope {
    Full,                  // solo caller — can see/touch everything
    Restricted(HashSet<usize>),
}
```

Every MCP handler in Phases 4 and 5 gains a `caller_scope` check before touching `SessionManager`. Solo callers continue to work as before; drivers see a filtered view.

### Caller identification

**Core challenge:** the MCP handler needs to know "which ccom session is making this tool call?" so it can apply the correct scope. The rmcp session id (minted on `initialize` and echoed back via `Mcp-Session-Id` header) identifies the MCP session, but not which ccom session it corresponds to.

**Approach:** when `Session::spawn` writes the `.mcp.json`, it also writes a session-specific identifier into the config that Claude Code will propagate back via an HTTP header OR a tool-call initialization parameter. Two concrete options:

1. **Custom Authorization header in `.mcp.json`** — the schema supports `"headers"` on HTTP server configs. Write a header like `"X-Ccom-Caller": "<session_id>"`. rmcp's `RequestContext<RoleServer>` surfaces the HTTP request headers (the Phase 4 spike confirmed this via the `get_session_id` example). Handler reads the header at tool-call time.
2. **`CCOM_CALLER_ID` env var** — already set as a similar env var (`CCOM_SESSION_ID`) by Phase 3.5. Claude Code doesn't propagate env vars through MCP tool calls, so this doesn't work by itself — unless we encode it into the `initialize` client-info block, which Claude Code DOES send through.

**Decision point for the spike:** try option 1 first. If rmcp's header-reading API from inside a `#[tool]` handler is awkward, fall back to option 2 via an initialize-time handshake where the caller passes the ccom id in `clientInfo.name` or similar.

### MCP tool additions

**New tool: `spawn_session`**
```json
{
  "role": "claude",         // optional, default "claude"; "terminal" also allowed
  "label": "worker-3",      // user-visible label, sanitized
  "working_dir": "...",     // optional, defaults to driver's cwd
  "initial_prompt": "..."   // optional — first prompt to the new session
}
```

Validates:
- Caller is a driver (`SessionRole::Driver { .. }`)
- Nesting cap: caller's own role is `Driver`, child's role will be `Solo` (drivers can't spawn drivers in v1)
- Label sanitization passes (see Task 3)
- Spawn policy (`Ask` → modal, `Budget` → decrement or fall back to modal, `Trust` → silent)

Returns `{session_id: usize, label: String}` on success.

### Kill policy update

Phase 5's `kill_session` always prompts. Phase 6 changes this:

```
kill_session(target_id) called by caller_id:
    caller_scope = ctx.caller_scope(caller_id)
    match caller_scope:
        Full (solo caller):
            → prompt as in Phase 5
        Restricted(own_scope):
            if target_id in own_scope:
                → silent kill (driver killing its own child or attached)
            else:
                → return NotFound (target outside scope — don't even prompt)
```

### UI markers

- **Session list panel**: driver sessions render with a `◆` prefix + accent color. Non-driver children of a driver render indented 2 cols under the driver, with `└─ ` prefix and the driver's label shown in dim text.
- **Session-view title**: when viewing a driver, the title bar includes `" driver budget: <n>"` if remaining budget is finite.
- **Attach-to-driver flow**: new key in the session picker (`a` — for "attach") opens a second picker listing currently-alive drivers; choosing one adds the selected session to that driver's attachment set. Visible via indentation in the session list.

## Task breakdown

### Task 1 — Session role data model (~1 hour)

**File:** `src/session/types.rs` (mostly) + `src/session/manager.rs` (tests)

- Add `SessionRole` and `SpawnPolicy` enums
- Add `role: SessionRole` field to `Session` (default `Solo`)
- Add `spawned_by: Option<usize>` field to `Session` (default `None`)
- Update `Session::spawn` to accept an optional `role` + `spawned_by` — default is `(Solo, None)` so existing call sites don't break
- Update `Session::dummy_exited` test helper with a `with_role` builder method
- Update `SessionManager::push_for_test` paths that need driver-variant fixtures
- **Migration note:** all existing Phase 1–5 sessions are `Solo`. Phase 6 doesn't need a data migration — we're adding new field defaults, not changing existing fields.

**Tests:**
- `session_role_defaults_to_solo`
- `session_spawn_accepts_driver_role`
- `session_spawned_by_defaults_to_none`

### Task 2 — Config surface + CLI flags (~1 hour)

**File:** `src/main.rs`, new `src/driver_config.rs`

CLI additions (`clap` struct):
```rust
#[arg(long)]
driver: bool,
#[arg(long, value_enum, requires = "driver")]
spawn_policy: Option<SpawnPolicyArg>,
#[arg(long, requires = "driver")]
budget: Option<u32>,
```

TOML file at `~/.config/claude-commander/driver.toml`:
```toml
[driver]
spawn_policy = "budget"  # ask | budget | trust
budget = 5
```

Resolution order (highest precedence first): CLI → TOML → fallback (`ask`, budget = 0).

**New file:** `src/driver_config.rs` with:
- `DriverConfig { spawn_policy, budget }` struct
- `load_driver_config(cli: &Cli) -> DriverConfig` — reads TOML if present, merges with CLI overrides
- Unit tests for each precedence layer

**Integration:** when `--driver` is passed, the first Claude session `ccom` spawns is a driver. Solo sessions spawned later via `n` in the dashboard stay solo unless explicitly upgraded (deferred — v1 has only one driver per ccom run for simplicity).

### Task 3 — `spawn_session` MCP tool (~2 hours)

**Files:** `src/mcp/handlers.rs`, `src/mcp/state.rs`, `src/mcp/sanitize.rs`

Add:
```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SpawnSessionArgs {
    pub label: String,
    pub working_dir: Option<String>,
    pub initial_prompt: Option<String>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SpawnSessionWire {
    pub session_id: usize,
    pub label: String,
}
```

Handler flow:
1. Identify caller via `RequestContext<RoleServer>` headers or clientInfo
2. Look up caller in `SessionManager`; verify role is `Driver { .. }`
3. Nesting cap: reject if caller isn't a driver, or if for some reason the call wants to create a driver (`role` arg is intentionally not in `SpawnSessionArgs` — v1 always spawns Solo children)
4. Sanitize `label` via new `sanitize_label()` in `sanitize.rs` (see below)
5. Apply `spawn_policy`:
   - `Trust` → silent allow
   - `Budget { remaining: 0 }` → fall back to `Ask`
   - `Budget { remaining: n }` → silent allow, decrement counter (locked under the sessions mutex)
   - `Ask` → request confirmation via `ConfirmBridge::request(ConfirmTool::SpawnSession, caller_id)`
6. On allow: construct `SpawnConfig` with `spawned_by: Some(caller_id)`, call `SessionManager::spawn`, optionally send initial_prompt
7. Return `SpawnSessionWire`

**New sanitizer:** `sanitize_label(input: &str) -> Result<String, String>`:
- Cap at 64 chars (labels show in narrow session list columns)
- Strip ASCII controls (`< 0x20` AND `0x7f`) — no newlines or tabs allowed in labels
- Strip ANSI CSI/OSC (reuse `ansi_strip`)
- Strip anything that's not a printable ASCII letter, digit, space, dash, underscore, slash, dot, or colon — whitelist approach prevents a driver from encoding emoji confusables or combining marks that corrupt the terminal
- Reject empty post-sanitization

**Note on `ConfirmTool`:** Phase 5's enum already has `SendPrompt` and `KillSession`. Add `SpawnSession` variant.

**Integration tests** in `tests/driver_spawn.rs`:
- `driver_with_budget_2_spawns_silently_then_asks_on_third`
- `driver_with_ask_policy_triggers_modal_on_every_spawn`
- `driver_with_trust_policy_never_asks`
- `solo_caller_cannot_use_spawn_session` (returns tool error)
- `driver_cannot_spawn_driver` (nesting cap)
- `spawn_session_sanitizes_label`
- `spawn_session_empty_label_rejected`

### Task 4 — Scope-restricted tool views (~1 hour)

**File:** `src/mcp/state.rs`, `src/mcp/handlers.rs`

Add scope resolution:
```rust
pub fn caller_scope(&self, caller_id: usize) -> Scope {
    let mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
    let Some(session) = mgr.get(caller_id) else {
        // caller isn't a registered session (e.g. Claude Code child
        // of a driver calling back) — default to Solo/Full
        return Scope::Full;
    };
    match session.role {
        SessionRole::Solo => Scope::Full,
        SessionRole::Driver { .. } => {
            let mut scope: HashSet<usize> = mgr.iter()
                .filter(|s| s.spawned_by == Some(caller_id))
                .map(|s| s.id)
                .collect();
            // Attachments come from the App via a shared snapshot
            // updated each tick (see Task 5). For the scope check we
            // read the current snapshot under an Arc<Mutex<>>.
            if let Some(attached) = self.attachments.lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&caller_id)
            {
                scope.extend(attached.iter().copied());
            }
            Scope::Restricted(scope)
        }
    }
}
```

Thread a new `attachments: Arc<Mutex<HashMap<usize, HashSet<usize>>>>` field through `McpCtx` from `App::new`.

Update existing handlers to filter:
- `list_sessions` — filter the returned vec by scope
- `read_response` — return `NotFound` if target outside scope
- `send_prompt` — return `NotFound` if target outside scope
- `kill_session` — scope check affects the modal-vs-silent branch (see Task 6)
- `subscribe` — filter the bus events in the forwarder task by scope

**Tests:**
- `list_sessions_filtered_for_driver_caller`
- `read_response_returns_not_found_for_out_of_scope_session`
- `send_prompt_rejects_out_of_scope_target`
- `subscribe_filters_events_by_caller_scope`

### Task 5 — "Attach to driver" TUI action (~1 hour)

**File:** `src/app/keys.rs`, `src/app/render.rs` (session picker overlay)

- Add `a` key binding in the session picker modal: opens a second overlay listing all currently-live drivers (with `◆` prefix). If there are zero drivers, show a status message and return.
- Selecting a driver adds the currently-selected session to that driver's attachment set in `App::driver_attachments`.
- Update the session list panel to show attached sessions indented under their driver with a secondary `↪ ` prefix (distinct from child `└─ `).
- On driver exit (`SessionEvent::Exited` for a driver id), clear the driver's entry from `driver_attachments`.

**Cross-task contract:** `App` owns `driver_attachments` but `McpCtx` needs to read it for scope checks. Wire via a shared `Arc<Mutex<HashMap<..>>>` that both sides reference. Updates happen on the main thread; reads happen on the `ccom-mcp` thread. Same pattern as `sessions` / `bus`.

**Tests:** the key-handler logic is hard to unit-test without a full ratatui test harness, so cover the attachment-store semantics directly:
- `attach_session_to_driver_adds_to_set`
- `attach_session_to_unknown_driver_errors`
- `driver_exit_clears_attachments`
- `attached_session_appears_in_driver_scope`

### Task 6 — Kill policy update (~30 min)

**File:** `src/mcp/handlers.rs::kill_session`

Branch on `caller_scope`:
```rust
let scope = self.ctx.caller_scope(caller_id);
match scope {
    Scope::Full => {
        // Solo caller — Phase 5 behavior: always modal
        request_confirm_and_kill(...)
    }
    Scope::Restricted(set) if set.contains(&target_id) => {
        // Driver killing its own child / attached — silent
        kill_silently(...)
    }
    Scope::Restricted(_) => {
        // Driver trying to kill something outside its scope — NotFound
        tool_error("session not found")
    }
}
```

**Tests** (in `tests/driver_spawn.rs`):
- `driver_kill_own_child_is_silent`
- `driver_kill_attached_session_is_silent`
- `driver_kill_out_of_scope_returns_not_found`
- `solo_kill_still_prompts` (regression)

### Task 7 — UI markers (~1.5 hours)

**File:** `src/ui/panels/session_list.rs`, `src/ui/theme.rs`

- Add `driver_icon: "◆ "` and `child_icon: "└─ "`, `attached_icon: "↪ "` to the theme
- When rendering the session list:
  - Iterate sessions once, building a tree-ish structure: drivers at top level, their children (via `spawned_by`) and attachments (via `driver_attachments`) indented beneath
  - Render each line with the appropriate icon + indentation
  - Driver rows use the accent color; child/attached rows use dim text for the parent label suffix
- For the session view title bar: when the displayed session is a driver, append `  [driver · budget <n>]` (or `· no budget` for `Ask`/`Trust`)

**Tests:** visual output isn't unit-testable, but we can pin the tree-building logic:
- `session_tree_builder_groups_children_under_driver`
- `session_tree_builder_handles_orphaned_children` (driver exited, children still alive)
- `session_tree_builder_respects_attachments`

### Task 8 — Budget reset + orphan handling (~30 min)

**File:** `src/session/manager.rs::reap_exited`, `src/app/mod.rs`

When a driver session transitions to `Exited`:
- Clear its entry from `App::driver_attachments` (done in the Tick handler when the `Exited` event is observed)
- Drop its `spawn_budget` counter (implicit — the `Session` is removed from the manager)
- Leave its children alive — they become orphans with a stale `spawned_by` pointing at a now-dead id. The UI renders them at top level (no parent to indent under). Document this explicitly.

**Tests:**
- `driver_exit_clears_attachments`
- `orphaned_children_render_at_top_level`
- `reap_exited_drops_driver_budget` (implicit via session removal, but worth asserting no leak)

### Task 9 — Integration tests (~2 hours)

**File:** `tests/driver_spawn.rs` (new)

All tests use `McpServer::start_with_confirm` to wire the bridge + spawn a real driver session via `SessionManager::spawn` with `role: Driver { .. }`. Cover:

1. `driver_with_budget_2_silent_spawns_then_asks_on_third` — budget flow
2. `driver_with_ask_policy_modals_every_spawn` — always-ask flow
3. `driver_with_trust_policy_silent_spawns` — trust flow
4. `solo_caller_rejected_from_spawn_session` — role check
5. `driver_cannot_spawn_driver_nesting_cap` — nesting cap
6. `list_sessions_filtered_for_driver_caller` — scope filter on reads
7. `driver_kill_own_child_silent` — kill policy silent path
8. `driver_kill_out_of_scope_returns_not_found` — kill policy reject path
9. `attached_session_visible_in_driver_scope` — attachment semantics (test seam pokes the attachment map directly)
10. `spawn_session_label_sanitization` — sanitizer end-to-end

### Task 10 — End-to-end verification (manual)

`cargo run -- --driver --spawn-policy budget --budget 3` in the working dir. Ask the driver Claude:

> "Spawn three helper sessions. Each one should `ls src/` and report the top-level module names. Then aggregate the results into a summary for me."

Expected:
- Three silent spawns (budget = 3 → 0)
- Each child gets its own initial prompt from the driver
- Driver calls `read_response` on each child's turn id as they complete
- Driver writes a summary to its own stdout
- TUI shows the 4-session tree: driver at top with 3 children indented underneath

Additional spot checks:
- After the spawns, try sending a prompt from outside the driver's scope (e.g. via another `ccom` MCP session) and verify it can't see the children
- Kill one of the children via the driver's `kill_session` → silent
- Kill the driver manually from the TUI → children become orphans, render at top level

## Parallelism plan

Per the master plan:
- **Tasks 1 + 2** sequential (config depends on the types)
- **Tasks 3 + 4 + 6 (MCP side)** go in one worktree — all touch `src/mcp/handlers.rs` and share the `Scope` helper
- **Tasks 5 + 7 (UI side)** go in a parallel worktree — session list rendering + attach action + tree builder
- **Tasks 8 + 9** sequential after both worktrees merge

Subagent decomposition once Tasks 1–2 are on the phase branch:
- **Subagent A (MCP side)**: Tasks 3 + 4 + 6 — `spawn_session`, scope filter, kill policy. Touches `handlers.rs`, `state.rs`, `sanitize.rs` (new `sanitize_label`), `confirm.rs` (new `ConfirmTool::SpawnSession` variant).
- **Subagent B (UI side)**: Tasks 5 + 7 — attach action, session tree builder, icons/theme. Touches `app/keys.rs`, `app/render.rs`, `app/mod.rs` (attachment map field), `ui/panels/session_list.rs`, `ui/theme.rs`.

Collision surface: `app/mod.rs` (B adds the attachment map field, A reads it via `McpCtx`). Coordinate by having me land the attachment map field on the branch before launching the subagents (~5 min change, 1 field + Arc wiring).

## Risks

1. **Caller identification is unproven.** The Phase 4 spike verified that `RequestContext<RoleServer>` can read the `Mcp-Session-Id` header, but did NOT exercise custom headers from `.mcp.json`. Spike this first — 30 minutes in a scratch crate. If custom headers via `.mcp.json` don't propagate, fall back to the clientInfo handshake approach. **This is Task 0 for Phase 6.**

2. **Scope leaks into `subscribe`.** The `subscribe` tool forwards events as they arrive via `notify_logging_message`. Adding a scope filter to the spawned task is straightforward but needs to handle the case where the caller's scope **changes** while the subscription is live (e.g., a new child spawns into the driver's scope). Simplest approach: re-resolve scope on every forwarded event. Higher cost than caching, but correct.

3. **Nesting cap enforcement has an edge case.** "Drivers can't spawn drivers" is enforced in `spawn_session`, but a driver could still call `SessionManager::spawn` directly if it found some other way. Since drivers are sandboxed Claude sessions that can only reach ccom via MCP tools, this isn't reachable in practice — document it as an implicit guarantee of the MCP boundary.

4. **Budget counter race.** The budget check + decrement happens under `sessions.lock()`. Two concurrent `spawn_session` calls from the same driver (e.g., parallel tool calls) could double-decrement if we're not careful. Hold the lock across the whole check-and-decrement sequence — no `.await` between them. The lock is brief so this is fine.

5. **Attachment map divergence.** `App::driver_attachments` and the shared `McpCtx::attachments` are the same `Arc<Mutex<HashMap<..>>>` — same memory, no divergence possible. Document the shared-pointer contract in the struct field doc.

6. **Orphan rendering.** When a driver exits, its children should be visible but not indented under a missing parent. The tree-builder must detect "parent not in manager" and render the child at top level.

7. **TUI test coverage is shallow.** Task 5/7 change rendering; ratatui's test APIs are limited. Cover via pure-Rust tree-builder tests (Task 7 tests) plus manual smoke test (Task 10). This is consistent with how Phases 4 and 5 tested their UI additions.

8. **Label sanitization whitelist may be too strict.** The proposed whitelist (ASCII alnum + ` -_./:`) is safe but excludes common label characters like `#` or `@`. Relax based on what the smoke test reveals. The allowlist is the safer default for v1.

## Verification

- `cargo test` — existing 376 + new tests pass (expect ~400+ total)
- `cargo clippy` — zero warnings
- `cargo fmt --check` — clean
- `cargo build --release` — compiles
- `tests/driver_spawn.rs` integration tests pass
- Manual Task 10 smoke test with `--driver --spawn-policy budget --budget 3`:
  - Three silent spawns
  - Children visible only to the driver
  - Driver aggregates results from all three
  - Orphan handling on driver exit

## Acceptance criteria (from master plan)

- A driver session can spawn, prompt, read, and kill its own children with no user-visible friction beyond the initial setup.
- A driver cannot see or touch unrelated sessions by default.
- Attaching a user-owned session to a driver brings it into scope.
- Nesting depth capped at 1.
- Spawn policy precedence (CLI > TOML > fallback) verified manually.
- End-to-end test with a real driver-orchestrated fleet passes.

## Effort estimate

- Task 0 (caller-id spike): 30 min
- Task 1 (data model): 1 hour
- Task 2 (config): 1 hour
- Task 3 (spawn_session): 2 hours
- Task 4 (scope filter): 1 hour
- Task 5 (attach UI): 1 hour
- Task 6 (kill policy): 30 min
- Task 7 (UI markers): 1.5 hours
- Task 8 (orphan handling): 30 min
- Task 9 (integration tests): 2 hours
- Task 10 (manual verification): 30 min

**Total: ~11.5 hours** of focused work, with ~3 hours parallelizable via two subagents.
