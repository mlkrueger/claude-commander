# PR #8 Review — `session-mgmt phase 2: programmatic write path`

**Branch:** `session-mgmt/phase-2-write-path` → `main`
**Stats:** +632 / −15, 4 files (initial PR)
**Reviewer:** Claude (self-review via `/review`)
**Date:** 2026-04-11

This is the canonical record of the PR #8 review and the disposition
of every action item it surfaced. Items marked **applied in PR** were
fixed before merge; items marked **deferred** are tracked in their
respective plan/spec docs and linked from this file.

---

## Overview

Phase 2 of the session-management plan. Adds the programmatic write
path that Phase 3 (response detector) and the Model Council both need.
Three building blocks land together: `Session::allocate_turn_id` (the
canonical TurnId mint site), the `SUBMIT_SEQUENCE` constant, and the
two methods `SessionManager::send_prompt` (allocates TurnId, writes
payload + submit, publishes `PromptSubmitted`) and
`SessionManager::broadcast` (raw byte fan-out, no TurnId, no events).
PR #7's K2 carry-forward applied: `TurnId(pub u64)` →
`TurnId(pub(crate) u64)` + `::new` constructor.

Tasks 2 and 3 were developed in parallel by subagents in worktrees,
then cherry-picked back. One conflict in `manager.rs` (both adding
tests in the same place); resolved by hand keeping both groups.

---

## Code correctness

**✅ Solid**

- Borrow scoping in `send_prompt` correctly closes the
  `&mut Session` borrow before `self.bus.publish` runs (mirrors the
  pattern from `reap_exited` in Phase 1).
- `Session::allocate_turn_id` is a straightforward read-then-bump.
  Tests pin monotonicity, uniqueness across 1000 iterations, and
  per-session independence.
- Cherry-pick conflict resolution kept all 16 new tests across both
  Tasks 2 and 3, both methods, and the `BroadcastResult` struct.

**⚠️ Real correctness gaps**

| # | Item | Disposition |
|---|---|---|
| **C1** | `send_prompt` masks `try_write` failures. The bus reports `PromptSubmitted` even if the bytes never reached the PTY. Severity: medium. | **D1 status quo + explicit doc.** Added a multi-paragraph caveat to `send_prompt`'s doc-comment explaining the limitation and pointing callers at `Session::consecutive_write_failures`. A future refactor may switch `try_write` to return a `Result`; until then, treat `PromptSubmitted` as "we attempted to submit," not "the runner has the bytes." Same caveat added to `broadcast`. |
| **C2** | Same issue applies to `broadcast` — `result.sent` reports attempts, not delivery. | **Applied** alongside C1 — `BroadcastResult` doc now explicitly says `sent` is attempts. |
| **C3** | `read_pty_until_contains` silently drops events for non-target sessions. The `broadcast_through_real_pty_writes_to_each_session` test passed by ordering luck — under different scheduling, `id_b`'s events would be drained while waiting for `id_a`'s and then unrecoverable. | **Applied (A1).** Replaced the helper with `PtyOutputAccumulator` — a per-session `HashMap<usize, Vec<u8>>` that buffers cross-session events instead of dropping them. Both real-PTY tests rebuilt to use it; the broadcast test threads a single accumulator through both `wait_for_bytes` calls. |

## Project conventions

**✅ Followed:** mod layout, doc comments, TDD red→green, pre-commit
hook compliance, sparse comments, descriptive snake_case test names.

**⚠️ Worth fixing in this PR**

| # | Item | Disposition |
|---|---|---|
| **K1** | `#[allow(dead_code)]` left on `RecordingWriter` and on production items (`send_prompt`, `broadcast`, `BroadcastResult`, `SUBMIT_SEQUENCE`, `PromptSubmitted`). | **Partially applied (A2) — and a lesson learned.** Removed from `RecordingWriter` and `make_recording_session` because they live inside `#[cfg(test)] mod test_support` and have callers in tests within the same module. **Restored on the production-side items** because `cargo build` analyzes reachability from the binary `main`, and tests don't satisfy the binary lint pass. Empirically verified: removing the production-side allows triggers warnings on `cargo build`. The annotations are doing real work and stay until the first production caller exists in Council Phase 2/3. Each restoration includes a comment naming the future caller. |
| **K2** | `SUBMIT_SEQUENCE` test pins the value but doesn't verify equivalence with `crate::app::key_event_to_bytes(KeyCode::Enter)`. | **Applied (A3).** Added a `// MUST match SUBMIT_SEQUENCE` comment in `app.rs::key_event_to_bytes` near `KeyCode::Enter => vec![b'\r']`. Defends the invariant at the source of truth so a future editor sees the cross-reference inline. |

## Performance implications

- Both methods are O(n). No new allocations beyond what's required.
- `send_prompt` issues two `try_write` calls (each calls `flush`
  internally). Could be combined for one less flush, but the cost is
  negligible and combining would obscure the "text + submit chord"
  intent. **Skipped.**
- `BroadcastResult` allocates two empty `Vec<usize>` even for empty
  inputs. Cost: negligible.

## Test coverage

**+23 tests in the initial PR; +0 in the review pass** (existing tests
adapted to the new accumulator helper, no new test cases needed for
the action items).

| Surface | Tests |
|---|---|
| `TurnId::new` (K2 carry-forward) | 3 |
| `Session::allocate_turn_id` | 4 |
| `SUBMIT_SEQUENCE` | 1 |
| `send_prompt` unit | 8 |
| `broadcast` unit | 8 |
| Real-PTY integration (`send_prompt` + `broadcast`) | 2 |

**Gaps acknowledged**

| # | Gap | Disposition |
|---|---|---|
| **T1** | No test exercises the `try_write` failure path in either method. | **Deferred** — paired with C1/C2's status-quo + doc decision. If/when `try_write` is refactored to return a `Result`, add tests then. |
| **T2** | No test for `broadcast` writing to a session in `Status::Exited(_)`. | **Deferred** — `try_write` doesn't check status, so it would still attempt the write. Considered intentional ("broadcast is dumb fan-out") but worth a comment in a future hardening pass. |
| **T3** | Real-PTY tests assume `/bin/cat` is at `/bin/cat`. | **Already tracked** in `docs/pr-review-pr7.md` (same Unix-only assumption as Phase 1's `/bin/sh` tests). |

## Security considerations

- No new IPC, no network, no unsafe code, no untrusted input parsing
  in production code paths.
- **`text` parameter to `send_prompt` is not sanitized.** ANSI escapes
  flow through to the PTY. Currently fine because callers are trusted
  (TUI code), but Phase 6's MCP `send_prompt` tool will accept
  arbitrary `text` from driver sessions and needs sanitization at the
  tool boundary. **Tracked in `docs/plans/session-management-phase-4-6.md`
  Phase 6 task 1** alongside the existing label sanitization note.
  Required policy: strip control chars (allowlist `\n`, `\t`),
  normalize newlines, strip ANSI CSI/OSC, cap length, reject empty
  post-sanitization text.

## Risks called out at review time

| # | Risk | Disposition |
|---|---|---|
| **R1** | C1/C2 — `try_write` failure masking. | **D1: status quo + explicit doc** (above). |
| **R2** | C3 — `read_pty_until_contains` drops cross-session events; test fragility. | **A1 fix applied** — `PtyOutputAccumulator` with per-session buffering. |
| **R3** | K1 — leftover `#[allow(dead_code)]` annotations. | **A2 partially applied** — removed where they were unnecessary, restored where the binary lint pass requires them. K1 review item was wrong; documented the lesson here so future PRs don't re-raise it. |
| **R4** | K2 — `SUBMIT_SEQUENCE` divergence risk vs. `app.rs`. | **A3 fix applied** — cross-reference comment at the `app.rs` source of truth. |

---

## Action items applied in this PR

| ID | Change | File |
|---|---|---|
| **A1** | `read_pty_until_contains` → `PtyOutputAccumulator` with per-session buffers; both real-PTY tests updated to thread one accumulator through their checks | `tests/unit_tests.rs` |
| **A2** | `#[allow(dead_code)]` removed from `RecordingWriter`, `make_recording_session` (cfg(test) items with test callers); restored on production items (`SUBMIT_SEQUENCE`, `BroadcastResult`, `send_prompt`, `broadcast`, `PromptSubmitted`) with clearer comments naming the future caller | `src/session/manager.rs`, `src/session/events.rs` |
| **A3** | `// MUST match SUBMIT_SEQUENCE` cross-reference comment near `KeyCode::Enter` | `src/app.rs` |
| **A4** | `send_prompt` text sanitization note added to Phase 6 Task 1 alongside label sanitization | `docs/plans/session-management-phase-4-6.md` |
| **D1** | Status-quo + explicit `try_write`-failure caveats documented on both `send_prompt` and `broadcast` | `src/session/manager.rs` |
| **D2** | `TurnId::new` made `const fn` so it can be used in const contexts | `src/session/events.rs` |

## Lessons learned (for future reviews)

- **`#[allow(dead_code)]` on `pub` items is sometimes load-bearing.**
  The K1 review item recommended removing the annotations because
  "tests use them now." That recommendation was wrong: `cargo build`'s
  reachability analysis starts from the binary `main`, and tests don't
  count toward that pass. Empirically verifying with `cargo build`
  *before* recommending the removal would have caught this. Lesson:
  always run the relevant cargo command before flagging dead-code
  annotations as "removable."
