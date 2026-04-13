# PR #18 Review — `Phase 6 Tasks 1+2: driver role data model + config surface`

**Date:** 2026-04-13
**Branch:** `session-mgmt/phase-6-driver-role`
**Scope:** 8 files, +634/-3
**Status:** Merged as-is (PR #18). Follow-up fixes applied on
`session-mgmt/phase-6-review-fixes` — Issue 2 + Issue 5 addressed,
Issues 1/3/4 triaged below. 186 bin-tests passing, zero new clippy
warnings, fmt clean.

## Overview

This PR introduces Phase 6's type-level foundation for driver-role
orchestration in two commits:

- **Task 1** (`d07669f`): `SessionRole { Solo, Driver { .. } }` and
  `SpawnPolicy { Ask, Budget, Trust }` enums in `src/session/types.rs`,
  plus `Session.role` / `Session.spawned_by` fields defaulting to
  `(Solo, None)`. Zero `SpawnConfig` churn.
- **Task 2** (`ad85a8c`): new `src/driver_config.rs` with three-layer
  config resolution (CLI → TOML → `Ask`/0 fallback), `--driver` /
  `--spawn-policy` / `--budget` clap flags, and a new
  `SessionManager::set_role` seam that promotes the first Claude
  session to `Driver` via `App::pending_driver_role`.

The review ran the standard five-agent sweep (CLAUDE.md compliance,
shallow bug scan, git history, prior-PR cross-ref, code-comment
compliance) followed by Haiku scoring on a 0-100 rubric (≥80 = must
comment on the PR). The final scoreboard was:

| # | Finding | Score | Action |
|---|---|---|---|
| 1 | TOCTOU window: two separate `sessions_lock()` calls in `spawn_session_kind` | 25 | ⏸️ Deferred to Task 4 |
| 2 | `--budget` stored even when `spawn_policy != Budget` | 75 | ✅ Fixed on review-fixes branch |
| 3 | Stale `#[allow(unused_imports)]` in `session/mod.rs` | 0 | ❌ False positive |
| 4 | `pub` on `with_role`/`with_spawned_by` test builders | 0 | ❌ False positive |
| 5 | Stale `set_driver_role` comment reference | 50 | ✅ Fixed on review-fixes branch |

No issue scored ≥80, so no in-line PR comment was posted. The two
moderate-score items (2, 5) are still worth fixing before Task 3
starts reading the affected code, so both are patched on
`session-mgmt/phase-6-review-fixes`.

---

## Issue 1 — TOCTOU window in `spawn_session_kind` (score 25, deferred)

**Finding.** `src/app/mod.rs::spawn_session_kind` acquires
`self.sessions_lock()` twice in sequence — once to call
`SessionManager::spawn(config)`, then a second time to call
`set_role(id, role)`. The guard drops between the two calls. Because
the MCP server thread holds the same `Arc<Mutex<SessionManager>>`,
an MCP handler could snapshot the session list in the gap and
observe the driver-to-be as `Solo`.

**Why the score was 25 (not 75).** PR #18 adds no MCP handler that
reads `role`. The scope filter + `spawn_session` tool (Tasks 3–4)
are where the first real consumers land. Until then the window
cannot be observed by any code path. The Haiku scorer flagged this
correctly as "real bug pattern, latent, fix-before-it-matters."

**Historical context.** PR #15's H1/H2 review pass fixed the same
class of bug (`read_response` check-then-act TOCTOU) by reordering
subscribe-then-recheck. The same codebase convention applies here —
multi-step state mutations held under one lock.

**Action — deferred to Task 4.** The fix (~4 lines) is to fold both
calls under a single `let mut mgr = self.sessions_lock();` guard.
Done here in isolation it would be a cosmetic change with no
observable behavior. Done as part of Task 4 — when the scope filter
lands and *does* observe role — it'll be covered by an integration
test that races a driver spawn against a list_sessions call. Noted
explicitly so Task 4's subagent brief includes the fold-under-one-lock
requirement plus a regression test against the TOCTOU shape.

---

## Issue 2 — `spawn_budget` stored when policy ≠ Budget (score 75, fixed)

**Finding.** `resolve_driver_config` in `src/driver_config.rs`
accepts `--budget N` even when `--spawn-policy` is `Ask` or `Trust`,
or when no `--spawn-policy` flag is passed at all (policy falls back
to `Ask`). The resulting `DriverConfig` carries a non-zero
`spawn_budget` on a non-Budget policy — which violates the
documented convention on `SessionRole::Driver::spawn_budget`:

> Meaningless for `Ask` / `Trust` — left at 0 in those cases by
> convention.

**Why it matters.** Not a bug in PR #18 (nothing reads
`spawn_budget` yet), but Task 3's `spawn_session` MCP handler will
decrement `spawn_budget` under `SpawnPolicy::Budget`. If a future
commit ever forgets to gate the decrement on
`spawn_policy == Budget`, a driver configured with
`--spawn-policy ask --budget 5` would silently get Budget-mode
behavior. The convention is a safety net; closing it here means the
safety net holds even if Task 3's handler has a gate bug.

**Fix** (`src/driver_config.rs::resolve_driver_config`):

```rust
let spawn_budget = if matches!(spawn_policy, SpawnPolicy::Budget) {
    cli_budget.or(toml_cfg.budget).unwrap_or(0)
} else {
    if cli_budget.is_some() || toml_cfg.budget.is_some() {
        log::warn!(
            "driver budget specified but spawn_policy is {spawn_policy:?} — \
             ignoring budget (meaningless unless policy is Budget)"
        );
    }
    0
};
```

Also warns loudly when a user passes `--budget` without
`--spawn-policy budget`, so the silent discard doesn't hide a config
mistake. Updated the existing `unknown_policy_string_falls_back_to_ask`
test to assert the new zero-out behavior, and added two fresh
regression tests:

- `budget_is_zeroed_when_policy_is_ask_even_with_cli_budget`
- `budget_is_zeroed_when_policy_is_trust`

---

## Issue 3 — Stale `#[allow(unused_imports)]` in `session/mod.rs` (score 0, false positive)

**Finding.** Reviewer Agent #4 claimed the `#[allow(unused_imports)]`
on the `pub use types::{Session, SessionRole, SessionStatus, SpawnPolicy, lock_parser};`
re-export in `src/session/mod.rs` is decorative because
`SessionRole` / `SpawnPolicy` are already consumed by `src/app/mod.rs`
and `src/driver_config.rs` in the same PR.

**Why the score was 0.** The file's pre-existing re-exports
(`events::{SessionEvent, TurnId}`, `response_store::{...}`) already
carry the same `#[allow(unused_imports)]` attribute and have done so
since Phase 1. Following an established module-level convention is
not a stale annotation — it's consistency. The Haiku scorer confirmed
that linter/compiler-catchable style issues are explicitly listed as
false positives in the review protocol.

**Action.** None.

---

## Issue 4 — `pub` visibility on `with_role`/`with_spawned_by` test builders (score 0, false positive)

**Finding.** Reviewer Agent #4 claimed the new `Session::with_role`
and `Session::with_spawned_by` test builders should be `pub(super)`
rather than `pub` (+ `#[doc(hidden)]` + `#[allow(dead_code)]`), citing
`docs/pr-review-pr7.md` K1 which reduced `publish_status_diffs`
visibility from `pub(crate)` to private.

**Why the score was 0.** The reviewer misapplied K1. PR #7's lesson
was about `pub(crate)` on a genuinely-internal helper — the fix was
possible because `mod tests` child modules can reach parent-private
items directly. That's not the shape here: `Session::with_role` is an
**inherent builder method** marked `#[doc(hidden)]` +
`#[allow(dead_code)]` + `pub`, which is the **existing established
pattern in the same file** — `Session::dummy_exited` (which has been
on `main` since Phase 1) uses exactly this shape. Changing the new
builders to `pub(super)` would contradict the codebase's own
precedent.

**Action.** None.

---

## Issue 5 — Stale `set_driver_role` comment reference (score 50, fixed)

**Finding.** A comment inside `Session::spawn` in
`src/session/types.rs` says:

```rust
// ... Drivers are promoted after construction by
// `SessionManager::set_driver_role` (Task 2 / 3 plumbing), ...
```

but the method added in Task 2 is named `SessionManager::set_role`,
not `set_driver_role`. The comment was written as an aspirational
forward reference during Task 1 and the Task 2 subagent picked a
different name.

**Why it matters.** Misleads future readers who grep for
`set_driver_role` and find nothing. Minor in isolation but the
comment is load-bearing documentation about the Phase 6 driver
promotion path — a future reviewer chasing the data flow should hit
a live cross-reference, not a dead one.

**Fix** (`src/session/types.rs::Session::spawn` comment, ~line 383):
rewrote to say `SessionManager::set_role` and clarified the three-step
data flow (`App::pending_driver_role` → `spawn_session_kind` →
`set_role`).

---

## Test count delta

Pre-fix: 184 bin-tests (PR #18 HEAD)
Post-fix: **186 bin-tests**

| Source | Delta |
|---|---|
| `budget_is_zeroed_when_policy_is_ask_even_with_cli_budget` (Issue 2) | +1 |
| `budget_is_zeroed_when_policy_is_trust` (Issue 2) | +1 |
| `unknown_policy_string_falls_back_to_ask` (Issue 2, asserts updated — no count delta) | 0 |

## Overall assessment

**Approved after fix pass.** No issues reached the in-line-comment
threshold (≥80). The two moderate-score findings (Issue 2 budget
zero-out and Issue 5 stale comment) are patched on the review-fixes
branch as a safety-net commit before Task 3 consumes them. The TOCTOU
finding (Issue 1) is explicitly carried forward into Task 4's brief
so it lands with its own regression test at the point where the race
first becomes observable.
