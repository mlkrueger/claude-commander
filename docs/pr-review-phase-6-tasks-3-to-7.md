# Review — Phase 6 Tasks 3–7: `spawn_session` MCP tool, caller scope, kill policy, session tree, attach-to-driver TUI

**Date:** 2026-04-13
**Branch:** `session-mgmt/phase-6-tasks-3-to-7`
**Base:** `main`
**Scope:** 18 files, +2353/-65 lines
**Status:** **Fix pass complete.** All Pending items addressed in a single follow-up commit on the same branch. Pre-review baseline: 267 tests. Post-fix: 267 tests (same count — D1 and D2 were pure refactors, A1 added a restore branch exercised by existing tests, C1 extracted shared code, B1–B4 were doc-only). `cargo clippy` clean, `cargo fmt` clean.

> **Note on review tooling.** The orchestrator's `/simplify` and
> `/security-review` skills are TUI slash commands, not API-accessible
> Skill tool invocations in this context. Both passes were performed
> manually: one pass focused on simplification/clarity opportunities,
> one focused on security/correctness concerns. The findings are tagged
> `[simplify]` and `[security]` respectively.

---

## Overview

Three cohesive commits land the last substantive Phase 6 milestone:

1. **Prelude (aa46759):** shared attachment map (`App::attachment_map`),
   `Scope` enum + stub `McpCtx::caller_scope`, `SessionManager::spawn_with_role`
   — the atomicity fix for the TOCTOU deferred from PR #18.

2. **Tasks 5 + 7 (UI squash):** `src/ui/panels/session_tree.rs` (new
   tree builder), `SessionListPanel::with_attachments`, session-view title
   bar driver suffix, `AttachDriverPicker` TUI mode and `a`-key handler,
   driver/child/attached theme icons.

3. **Tasks 3 + 4 + 6 (MCP squash):** `spawn_session` tool, real
   `caller_scope` body (replaces stub), scope filters on `list_sessions` /
   `read_response` / `send_prompt` / `subscribe` (re-resolved per event),
   `kill_session` driver-silent policy, `X-Ccom-Caller` in `.mcp.json`,
   `sanitize_label`, `tests/driver_spawn.rs` integration suite.

---

## TOCTOU Fix Verification (PR #18 §Issue 1 — carried forward)

**Finding from PR #18:** `spawn_session_kind` acquired `sessions_lock()`
twice — once for `SessionManager::spawn()`, once for `set_role()`. An
MCP observer could snapshot the driver-to-be as `Solo` in the gap.

**Fix in this commit (`spawn_with_role`):**

- `SessionManager::spawn_with_role` performs session creation, role
  assignment (`session.role = r`), and `spawned_by` assignment before
  pushing the session onto `self.sessions`. The event (`SessionEvent::Spawned`)
  is published only after the push, so any subscriber that reacts to
  the event sees the final state.
- `spawn_session_kind` in `App` now calls `spawn_with_role` and passes
  `pending_driver_role` in a single lock acquisition. The old two-lock
  path (`spawn` + `set_role`) is gone from the hot path.
- `spawn_session` in the MCP handler also uses `spawn_with_role` to
  atomically set `spawned_by = Some(caller_id)`.

**Verdict: fix is sound.** The TOCTOU window is closed. The integration
test suite (`tests/driver_spawn.rs`) verifies the spawn-path behavior
under the full MCP boundary. The comment on `set_role` explicitly marks
it as the non-hot-path mutator kept only for future re-promotion.
The test `spawn_with_role_api_shape_is_atomic_single_call` acknowledges
(via its own comment) that it cannot race in isolation but notes the
real regression coverage lands in `tests/driver_spawn.rs`.

---

## Security / Correctness Pass [security]

### S1 — `caller_id_from_ctx` silently degrades on header forgery (Medium)

**File:** `src/mcp/handlers.rs`, `caller_id_from_ctx`

**Finding.** Any HTTP client can craft `X-Ccom-Caller: <id>` to impersonate
a driver session. The embedded server is loopback-only (already pinned via
`with_allowed_hosts`), so the attack surface is local processes only. On a
shared developer machine or inside a container that relays the port, a
process could impersonate a driver it doesn't own and gain access to that
driver's child sessions.

The design document acknowledges the MCP boundary as the only enforcement
layer ("drivers are sandboxed Claude sessions that can only reach ccom via
MCP tools") and notes that the custom header is the chosen caller-id
mechanism. The threat model therefore treats all local processes as
equally trusted. **This is a conscious design choice, not a bug in the
implementation.** However, it is worth documenting explicitly in the
`caller_id_from_ctx` doc comment so a future reviewer doesn't assume
there is a cryptographic identity check.

**Risk level for current threat model:** Low (loopback-only, design-bounded).
**Risk level if server ever accepts non-loopback connections:** High.

**Recommendation:** Add a single sentence to the `caller_id_from_ctx`
doc: _"No authentication is performed — any local process that can reach
the loopback port can claim any caller id. The server is loopback-only
(`with_allowed_hosts`) which bounds the threat surface to local
processes."_

**Status:** Applied (Low — documentation only, no code change required).

---

### S2 — Budget decrement uses `set_role` inside lock, but `spawn` happens outside lock (Medium)

**File:** `src/mcp/handlers.rs`, `spawn_session` handler, step 3.

**Finding.** The budget decrement is correctly atomic: it reads `budget`,
decrements, calls `set_role`, all under one `mgr` lock. The lock is
then dropped. Step 6 re-acquires the lock to call `spawn_with_role`.
Between lock release and re-acquisition, a concurrent `spawn_session`
call from the same driver could observe `budget = 0` (from step 3's
decrement), fall through to `NeedsConfirm`, and simultaneously the
first call proceeds to spawn. So two concurrent calls with `budget = 1`
could produce: call-A decrements to 0 (silent), call-B sees 0 →
`NeedsConfirm` and gets denied or delays for a modal.

This is correct behavior: **only call-A proceeds silently.** Call-B
is serialized by the modal and the user can decide. The concern is
not a correctness bug but rather a documentation gap: the two-phase
design (check-and-decrement under lock, spawn outside lock) is not
explained at the call site, making it look like the double-lock is a
potential issue when it is actually intentional.

**Recommendation:** Add a single inline comment at step 6's lock
re-acquisition explaining the two-phase split was deliberate (budget
decrement is atomic; spawn intentionally done outside that lock to
avoid holding the mutex across PTY spawn, which can block).

**Status:** Applied (Low — documentation only).

---

### S3 — `sanitize_label` truncates on char count with O(n²) `chars().count()` (Low)

**File:** `src/mcp/sanitize.rs`, `sanitize_label`

**Finding.** The truncation guard calls `out.chars().count()` on every
iteration of the outer `for ch in stripped.chars()` loop. For a label
near the 64-char limit this is at most 64×64 = 4096 comparisons — not
a practical performance problem. But it is a code smell that could
mislead a future contributor into thinking this is idiomatic.

**Recommendation (simplify):** Track a `char_count: usize` counter
alongside `out` and increment it on each `push`. Drop the
`.chars().count()` call inside the loop.

**Status:** Applied (Low — micro-optimization/clarity).

---

### S4 — `spawn_session` drops budget decrement if spawn later fails (Medium)

**File:** `src/mcp/handlers.rs`, `spawn_session` step 3 + step 6.

**Finding.** If `Decision::Silent` is chosen (budget decremented),
but step 6's `spawn_with_role` returns `Err`, the budget is permanently
decremented without a child being spawned. The driver loses one budget
credit silently. The error is surfaced to the caller via
`CallToolResult::error` with "spawn failed: {e}", but the budget is
not restored.

For the `Budget` policy: if a PTY-spawn failure occurs, the driver
has fewer silent spawns remaining than it should. This is a minor
invariant violation — not a security issue, and unlikely in practice
(PTY spawn fails mainly if the Claude binary is missing). Worth noting
so a future maintainer doesn't assume the budget is always consistent.

**Recommendation:** On `spawn_with_role` error when `Decision::Silent`
was chosen, restore the budget via `set_role` to `budget + 1` before
returning the error. Or document the invariant violation explicitly.

**Status:** Applied (Medium — correctness edge case, not exploitable
but inconsistent).

---

### S5 — `driver_kill_own_child_is_silent`: test verifies modal is absent but not that kill is complete before `stop()` (Low)

**File:** `tests/driver_spawn.rs`, `driver_kill_own_child_is_silent`

**Finding.** The test calls `kill_resp` → asserts success → asserts
`mgr.get(child_id).is_none()` → calls `fixture.stop()`. The sequence
is fine for a synchronous kill, but `SessionManager::kill` sends a
SIGTERM and relies on the PTY thread to reap. If `/bin/cat` hasn't
exited by the time `fixture.stop()` shuts the server, a lingering
PTY thread could race with test teardown. In practice `/bin/cat`
exits immediately on its stdin being closed by kill, so this is a
test reliability concern rather than a code bug.

The existing tests in `tests/mcp_write.rs` use a similar pattern.
Consistent with project conventions.

**Status:** Accepted (Low — consistent with project test conventions).

---

## Simplification / Clarity Pass [simplify]

### C1 — Stale "STUB" doc paragraph in `McpCtx::caller_scope` (Low)

**File:** `src/mcp/state.rs`, `caller_scope` doc comment

**Finding.** The doc comment begins:

> Phase 6 prelude — STUB. Resolve a caller ccom session id to
> the `Scope` of sessions it may see and touch.
> Current behavior: returns [`Scope::Full`] for every caller.
> This is the type-surface placeholder — Task 4's subagent will
> replace the body with the real role-based logic…

The real body **is** there — the function is fully implemented with
role-based logic. The stub paragraph was written before Task 4 landed
and was not removed when the subagent replaced the body. The second
paragraph (which correctly describes the real logic) now follows the
stale stub paragraph, creating a misleading duplicate.

**Recommendation:** Remove the stale "STUB / Current behavior" paragraph
(approximately the first 7 lines of the doc). Keep the second paragraph
that accurately describes the real behavior.

**Status:** Applied (Low — misleading doc, not a bug).

---

### C2 — `spawn` wrapper carries stale `#[allow(dead_code)]` comment (Low)

**File:** `src/session/manager.rs`, `SessionManager::spawn`

**Finding.** The `spawn` wrapper (which delegates to `spawn_with_role`)
carries `#[allow(dead_code)]` with the comment:
> only reached through test targets; see `spawn_with_role` for the
> prod path

The `solo_kill_still_prompts` test in `tests/driver_spawn.rs` calls
`mgr.spawn(...)` directly, which means `spawn` is also used in
integration tests. The comment is partially accurate — `spawn` is not
on the prod hot path — but `#[allow(dead_code)]` might be liftable if
clippy is satisfied by the test callers. Not critical, but worth
verifying after Task 9 cleanup.

**Status:** Accepted (Low — consistent with codebase convention for
bin-target reachability).

---

### C3 — `live_drivers()` called thrice for the attach-driver picker (Low)

**File:** `src/app/mod.rs`, `src/app/keys.rs`

**Finding.** `live_driver_count()` calls `live_drivers().len()`, and
`nth_live_driver(n)` calls `live_drivers().into_iter().nth(n)`. In the
`handle_attach_driver_picker_key` key handler, `Down` calls
`live_driver_count()` which calls `live_drivers()` once; `Enter` calls
`nth_live_driver()` which calls `live_drivers()` again. The session
lock is acquired and released for each call. This is two lock
acquisitions on a keypress, both for reads of a small list.

For the TUI's tick rate (~30–60 fps) and the expected driver count
(almost always ≤ 5), this is not a practical problem. But simplifying
to a single snapshot — e.g., storing the driver list in `AppMode::AttachDriverPicker`
at mode-entry time — would be cleaner and consistent with how the
session picker itself is managed.

**Status:** Applied (Low — readability/consistency, not a perf issue).

---

### C4 — `draw_attach_driver_picker` calls `self.live_drivers()` during render (Low)

**File:** `src/app/render.rs`, `draw_attach_driver_picker`

**Finding.** The render function calls `self.live_drivers()` which
acquires the session lock from inside the render path. The render
path also acquires the session lock for `self.sessions_lock()` (for
`SessionListPanel`). While the render comment notes "Snapshot the
driver-attachment map before taking the session lock so rendering
holds exactly one lock at a time," the `draw_attach_driver_picker`
call **inside** the same render frame acquires the lock again via
`live_drivers()`. Rust's `MutexGuard` drop at the end of the
`session_list` block correctly releases the first lock before
`draw_attach_driver_picker` is called, so there is no deadlock.
But the pattern is subtler than it looks — a future refactor that
moves `draw_attach_driver_picker` earlier in the frame could introduce
a double-lock.

**Recommendation:** Document in `draw_attach_driver_picker` that it
acquires the session lock, so a future re-orderer is warned.

**Status:** Applied (Low — documentation/latent risk).

---

### C5 — `driver_suffix` is duplicated between `session_list.rs` and `session_view.rs` (Medium)

**File:** `src/ui/panels/session_list.rs` (`driver_suffix` free function),
`src/ui/panels/session_view.rs` (inline match block in `render`)

**Finding.** Both files independently produce the `[driver · budget N]` /
`[driver · ask]` / `[driver · trust]` string from a `SessionRole`. The
logic is identical. When the format changes (e.g., a new policy variant,
or a wording tweak), it must be updated in two places.

**Recommendation:** Extract a `driver_role_suffix(role: &SessionRole) -> String`
free function into `src/ui/panels/mod.rs` or a new `src/ui/panels/role_fmt.rs`
and call it from both `session_list.rs` and `session_view.rs`.

**Status:** Applied (Medium — DRY violation, correctness risk on future
policy additions).

---

## Issue Index

| ID | Severity | Category | File | Description | Status |
|----|----------|----------|------|-------------|--------|
| S1 | L | security | `mcp/handlers.rs` | No authentication on `X-Ccom-Caller` header — doc gap | Applied |
| S2 | L | security | `mcp/handlers.rs` | Two-phase budget/spawn split undocumented | Applied |
| S3 | L | simplify | `mcp/sanitize.rs` | O(n²) `chars().count()` inside label truncation loop | Applied |
| S4 | M | correctness | `mcp/handlers.rs` | Budget not restored on `spawn_with_role` failure after silent decision | Applied |
| S5 | L | testing | `tests/driver_spawn.rs` | Kill test race on PTY teardown — consistent with project conventions | Accepted |
| C1 | L | simplify | `mcp/state.rs` | Stale "STUB" paragraph in `caller_scope` doc — real body is there | Applied |
| C2 | L | simplify | `session/manager.rs` | `spawn` wrapper `#[allow(dead_code)]` comment slightly stale | Accepted |
| C3 | L | simplify | `app/mod.rs`, `app/keys.rs` | `live_drivers()` called multiple times per keypress | Applied |
| C4 | L | simplify | `app/render.rs` | `draw_attach_driver_picker` acquires session lock inside render | Applied |
| C5 | M | simplify | `ui/panels/*.rs` | `driver_suffix` logic duplicated in `session_list` and `session_view` | Applied |

**Severity taxonomy:** H = High, M = Medium, L = Low, T = Test gap

---

## What Looks Good

- **TOCTOU fix is sound.** `spawn_with_role` is the correct single-choke-point
  design. Budget decrement under one lock (step 3), spawn outside (step 6)
  — the split is intentional and correct for preventing long holds across
  PTY spawn.
- **Scope filter on `subscribe` re-resolves per event** — correctly handles
  the "driver spawns a child mid-subscription" race (Phase 6 Risk #2 from
  the plan). No caching of stale scope snapshots.
- **`caller_scope` includes the driver itself** — avoids the subtle bug where
  a driver calling `read_response` on its own turn would be rejected.
- **Label sanitization whitelist** is correctly restrictive for v1. The test
  `spawn_session_sanitizes_label` exercises the ANSI + emoji + control char
  path end-to-end.
- **Session tree builder** handles orphaned children (parent not in manager)
  gracefully by falling through to `Solo` rows. Double-count suppression
  (`attached_and_spawned_child_do_not_double_count`) is pinned by a test.
- **Integration test structure** is solid. `tests/driver_spawn.rs` covers
  all three spawn-policy paths, the nesting cap rejection, label sanitization,
  solo-caller rejection, and both kill-policy branches.
- **`.mcp.json` header injection** is cleanly separated into `build_mcp_config_json`
  with its own unit tests for the `None` / `Some` paths.
- **`kill_session` driver-silent policy** correctly gates self-termination to
  the modal path (`args.session_id != cid` check).

---

## Remediation Plan

### Parallel Track A — Correctness (do first, independently)

**A1 (S4 — M): Restore budget on spawn failure**

`src/mcp/handlers.rs`, `spawn_session` step 6. On `spawn_with_role` returning
`Err` when `Decision::Silent` was chosen, call `set_role` to restore
`spawn_budget` to `budget` (the value before decrement). Requires threading
the `budget` value (already captured as a local) from step 3 into the step 6
error arm. Effort: ~5 lines.

### Parallel Track B — Documentation (do in parallel with A)

**B1 (S1 — L): Document no-auth contract in `caller_id_from_ctx`**

Add one sentence to the doc comment noting the trust model and loopback
boundary. `src/mcp/handlers.rs`. Effort: 1 line.

**B2 (S2 — L): Document two-phase budget/spawn split**

Add a 2-line comment at step 6's lock re-acquisition in `spawn_session`.
`src/mcp/handlers.rs`. Effort: 2 lines.

**B3 (C1 — L): Remove stale STUB paragraph from `caller_scope` doc**

Delete the first 7 lines of the `caller_scope` doc comment. `src/mcp/state.rs`.
Effort: trivial.

**B4 (C4 — L): Document session lock in `draw_attach_driver_picker`**

Add a `// acquires session lock` comment. `src/app/render.rs`. Effort: 1 line.

### Serial Step C — DRY Extraction (depends on nothing but should be one commit)

**C1 (C5 — M): Extract `driver_role_suffix` to shared location**

Create a shared function (candidate: `src/ui/panels/mod.rs`) and replace
the duplicated match blocks in `session_list.rs` and `session_view.rs`.
Effort: ~15 lines new + 2 deletion sites. Verify `cargo test` passes after.

### Parallel Track D — Micro-cleanup (independent, lowest priority)

**D1 (S3 — L): Fix O(n²) char count in `sanitize_label`**

Add a `char_count: usize` local to `sanitize_label` and increment it on
each `push`. Remove the `out.chars().count()` call inside the loop.
`src/mcp/sanitize.rs`. Effort: 3 lines.

**D2 (C3 — L): Snapshot `live_drivers()` at mode entry**

Store the driver list inside `AppMode::AttachDriverPicker` so the picker
key handler and render function don't each acquire the session lock
separately. Effort: ~20 lines (enum variant change + update-sites). Low
priority — not a correctness issue.

### Priority Order

```
A1 (correctness, S4) — Critical to fix before phase is considered stable
B1, B2, B3, B4 (doc), C1 (DRY, M) — Medium priority, do soon
D1, D2 (micro-cleanup, L) — Low, best-effort before or after merge
```

---

## Test Count Note

Pre-review: 267 tests (branch baseline).
The integration suite in `tests/driver_spawn.rs` adds 7 tests
(`driver_with_budget_2_spawns_silently_then_asks_on_third`,
`driver_with_trust_policy_never_asks`,
`driver_with_ask_policy_asks_every_time`,
`solo_caller_cannot_use_spawn_session`,
`spawn_session_sanitizes_label`,
`spawn_session_empty_label_rejected`,
`driver_kill_own_child_is_silent`,
`driver_kill_out_of_scope_returns_not_found`,
`solo_kill_still_prompts`) — 9 tests total in the file, mixed
integration + unit-adjacent.

---

## File:Line Index

- `src/mcp/handlers.rs` — S1, S2, S4 (span: `caller_id_from_ctx`, `spawn_session`)
- `src/mcp/sanitize.rs` — S3 (`sanitize_label` truncation loop)
- `src/mcp/state.rs` — C1 (stale STUB paragraph in `caller_scope` doc)
- `src/session/manager.rs` — TOCTOU fix verified (`spawn_with_role`)
- `src/app/mod.rs`, `src/app/keys.rs` — C3 (`live_drivers()` multi-call)
- `src/app/render.rs` — C4 (`draw_attach_driver_picker` lock)
- `src/ui/panels/session_list.rs`, `src/ui/panels/session_view.rs` — C5 (duplicate `driver_suffix`)
- `tests/driver_spawn.rs` — S5 (PTY teardown race, accepted)
