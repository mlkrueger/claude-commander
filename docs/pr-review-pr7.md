# PR #7 Review — `session-mgmt phase 1: SessionEvent bus + production wiring`

**Branch:** `session-mgmt/phase-1-event-bus` → `main`
**Stats:** +1023 / -11, 5 files
**Reviewer:** Claude (self-review via `/review`)
**Date:** 2026-04-11

This is the canonical record of the PR #7 review and the disposition
of every action item it surfaced. Items marked **applied in PR** were
fixed before merge; items marked **deferred** are tracked in their
respective plan/spec docs and linked from this file.

---

## Overview

Phase 1 of the session-management plan. Adds an internal pub/sub bus
carrying `SessionEvent`s as a sibling to the existing raw
`crate::event::Event` channel, wires it into `SessionManager` so spawn
/ kill / reap_exited / check_attention publish state transitions, and
stores the bus on `App` for future consumers. Marker-only contract:
`PromptSubmitted` and `ResponseComplete` carry `TurnId`s, not bodies.
No production consumer subscribes yet — Phase 2+ will activate them.

---

## Code correctness

**✅ Solid**

- Bus pruning logic in `EventBus::publish` is the correct, idiomatic
  pattern. Dead receivers are pruned in the same lock that publishes.
- Mutex poisoning recovery (`unwrap_or_else(|p| p.into_inner())`)
  matches the pre-existing `lock_parser` pattern in `types.rs`.
- `reap_exited` correctly uses collect-then-publish to sidestep the
  `&mut self.sessions` / `&self.bus` borrow conflict.
- `publish_status_diffs` correctly fires `PromptPending` only on the
  *transition into* `WaitingForApproval`, not within. The
  `WaitingForApproval(A) → WaitingForApproval(B)` test exercises this.
- `kill` captures `exit_code` from the session's `Exited(_)` status
  *before* removing from the vec — order is right.

**⚠️ Minor issues**

| # | Item | Disposition |
|---|---|---|
| C1 | `ExitedChild::try_wait` casts `i32 → u32`. Inline comment acknowledges, constrains tests to non-negative. | **Applied (A4):** `debug_assert!(code >= 0)` in the constructor. |
| C2 | Pre-existing `as i32` cast on `status.exit_code()` (u32 → i32) in `reap_exited`. Not introduced by this PR but flagged for future-me. | **Noted, not changed.** Still in `i32` range for typical signal-driven exit codes (e.g. 137). |
| C3 | `EventBus::publish` clones the event for every subscriber including the last one. Minor optimization opportunity. | **Skipped.** Not worth complicating the loop for a TUI's event volume. |

## Project conventions

**✅ Followed:** mod layout, inline `#[cfg(test)] mod tests`,
`#[allow(dead_code)]` annotations with explanatory comments, TDD
red→green throughout, pre-commit hook passes.

**⚠️ Worth reconsidering**

| # | Item | Disposition |
|---|---|---|
| K1 | `publish_status_diffs` is `pub(crate)` purely for test visibility — leaks an internal helper to the whole crate. | **Applied (A2):** dropped to private `fn`. The `mod tests` child can still call private items of its parent module, so tests work without any visibility bump. |
| K2 | `TurnId(pub u64)` exposes the field publicly. Future Phase 2's `Session::next_turn_id` allocator should be the only way to mint a TurnId, but in Phase 1 tests construct `TurnId(7)` directly. | **Deferred to after Phase 2.** Tracked as a forward note in `docs/plans/session-management-phase-1-3.md` Phase 2 section. Revisit when the allocator lands. |

## Performance implications

- `SessionEvent::Clone` per subscriber allocates strings for
  `Spawned`/`PromptPending`/`StatusChanged(WaitingForApproval)`.
  Negligible for a TUI; revisit if MCP scenarios in Phase 4+ scale up.
- `check_attention` snapshots all session statuses on every tick —
  `O(n)` clones. Pre-existing per-tick allocation pattern; not
  regressing.
- Mutex contention on `EventBus::senders`: every publish takes the
  full lock. Low-frequency TUI events make this irrelevant.

## Test coverage

**Strong (+31 tests):** see PR description for the full breakdown.

**Gaps identified at review time:**

| # | Gap | Disposition |
|---|---|---|
| T1 | No end-to-end test of `check_attention` publishing via a real `PromptDetector`. | **Applied (D2):** added `check_attention_publishes_via_real_detector` in `manager.rs` tests. Injects a known approval prompt into a dummy session's vt100 parser and asserts both `StatusChanged` and `PromptPending` fire. |
| T2 | No test for "subscribe during a concurrent publish". Mutex serializes them so it should work, but no explicit assertion. | **Deferred.** Defensive at best — Mutex is well-tested by stdlib. Revisit if we ever switch the bus backing store. |
| T3 | Real-PTY tests assume `/bin/sh` exists at `/bin/sh`. Universal on Unix; if Windows is ever a target, needs `#[cfg(unix)]`. | **Deferred.** Project is Unix-only per `portable-pty` usage and existing `#[cfg(unix)]` annotations. Add `#[cfg(unix)]` to the integration test module if Windows support ever lands. |

## Security considerations

- No new IPC, no network, no unsafe code, no untrusted input parsing.
  Still in-process pub/sub.
- **Phase 6 follow-up:** `String` labels in `SessionEvent::Spawned`
  come from `SpawnConfig.label`, which is currently set by `App`
  (trusted). When MCP adds `spawn_session` in Phase 6, **a remote
  driver session can supply arbitrary label content**, including
  control characters or injection attempts against subscribers that
  log/render labels. **Must add label sanitization** at the MCP tool
  boundary in Phase 6.
  - **Tracked in:** `docs/plans/session-management-phase-4-6.md` Phase
    6 task list.

## Risks called out at review time

| # | Risk | Disposition |
|---|---|---|
| R1 | `EventBus::publish` holds the senders mutex while iterating and calling `tx.send`. Works for unbounded `mpsc::channel`. If we ever switch to bounded channels, becomes a head-of-line blocker. | **Applied (A3):** added a code comment on `EventBus::publish` warning that the held-lock pattern depends on unbounded channels. |
| R2 | `pub(crate) publish_status_diffs` reachable from any module in the crate. | **Applied (A2):** see K1 above. |
| R3 | `SessionManager::new()` creates an internal bus only reachable via `bus()`. Production should use `with_bus`. Quiet failure mode if production code calls `new()` instead. | **Applied (A1):** `pub fn new()` → `pub(crate) fn new()`. External crates can no longer construct an isolated-bus manager; production must use `with_bus`. Tests inside the crate are unaffected. |

---

## Action items applied in this PR

| ID | Change | File |
|---|---|---|
| A1 | `SessionManager::new()` visibility → `pub(crate)` | `src/session/manager.rs` |
| A2 | `publish_status_diffs` → private `fn` (no `pub(crate)`) | `src/session/manager.rs` |
| A3 | Code comment on `EventBus::publish` re: held-lock + unbounded channel assumption | `src/session/events.rs` |
| A4 | `debug_assert!(code >= 0)` in `ExitedChild::new` (with constructor) | `src/session/manager.rs` |
| D2 | `check_attention_publishes_via_real_detector` test | `src/session/manager.rs` |

## Forward-looking items deferred to other docs

| ID | Item | Where tracked |
|---|---|---|
| K2 | `TurnId.0` field visibility revisit after the Phase 2 allocator lands | `docs/plans/session-management-phase-1-3.md` Phase 2 |
| T2 | Concurrent subscribe + publish test | This file (no plan-level note) |
| T3 | `#[cfg(unix)]` on real-PTY tests if Windows support ever lands | This file (no plan-level note) |
| Phase 6 label sanitization | MCP `spawn_session` must sanitize labels supplied by drivers | `docs/plans/session-management-phase-4-6.md` Phase 6 |

---

## Second review pass — delta commit `4ec4e6b`

After applying A1–A4 + D2 and pushing the follow-up commit, I ran the
review flow again on just the delta (commit `4ec4e6b`, +282 / −12, 5
files). This appendix is the second pass's record.

### Action item verification

| ID | Verified |
|---|---|
| **A1** | `new()` is `#[cfg(test)] pub(crate)`; `impl Default` is gone. `cargo build` succeeds, proving no non-test code path depended on either. |
| **A2** | `publish_status_diffs` is private `fn`. `check_attention` (same `impl` block) and `mod tests` (child of `mod manager`) both still reach it via parent-private access. |
| **A3** | Held-lock invariant comment on `EventBus::publish` is in place, names the alternatives (release-then-resend, RwLock + try_send), links design open question #4. |
| **A4** | `ExitedChild::new` constructor with `debug_assert!(code >= 0)` is in place. Field is now private. Both call sites updated (`clone_killer`, `make_exiting_session`). |
| **D2** | `check_attention_publishes_via_real_detector` exists. Cursor-position math verified: `ESC[20;1H` → row 19 (0-indexed), within the detector's 9..24 scan window for the dummy session's 24x80 screen. Subscribe-after-mutate avoids `Spawned` clutter. Drain loop is race-free because `EventBus::publish` is synchronous. Asserts on presence not order so future `publish_status_diffs` refactors aren't bound to a specific emission sequence. |

### New issues found by the second pass

**None substantive.** All five changes are restrictions or additions,
not relaxations. No new risks introduced.

### Stylistic nits — considered, not actionable

These were noticed during the second pass but explicitly judged
**not worth a follow-up commit.** Recording here so a future
reader doesn't re-raise them as fresh issues.

| # | Nit | Why not fixing |
|---|---|---|
| N1 | The 5-line comment block standing in where `impl Default for SessionManager` used to live is unusual. Most codebases just delete the impl and let `git blame` carry the rationale. | Defensible because "why not Default?" is a question reviewers will reasonably ask. The comment answers it inline. Cost: ~5 lines. Benefit: any future reviewer immediately understands the constraint without spelunking through git history or this review file. Net positive. |
| N2 | `ExitedChild::clone_killer` calls `ExitedChild::new(self.code)`, which re-runs the `debug_assert` on every clone. If `self.code` was valid at original construction, it's still valid at clone time. The redundant assert is harmless but slightly wasteful. | The cost is **zero** in release builds (`debug_assert!` compiles out) and trivial in debug. The alternative (`Self { code: self.code }`) bypasses the constructor and would diverge if a future invariant is added to `new`. Keep the assert; consistency wins. |
| N3 | The new test imports `PromptDetector` and `lock_parser` inline (`use` inside the test function) rather than at the test module top. | Stylistic. Inline `use` keeps the test self-documenting about its dependencies. Acceptable Rust idiom. |

### Final disposition

**Approved on second pass.** Delta commit is tight, every action item
is verifiably applied, the new test is well-constructed, and no new
issues were introduced. Ready to merge.
