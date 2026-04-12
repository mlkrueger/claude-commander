# PR #9 Review — `session-mgmt phase 3: response boundary detector + bounded store`

**Branch:** `session-mgmt/phase-3-response-detector` → `main`
**Stats:** +1308 / −14, 15 files (initial PR)
**Reviewer:** Claude (self-review via `/review`)
**Date:** 2026-04-11

This is the canonical record of the PR #9 review and the disposition
of every action item it surfaced. Items marked **applied in PR** were
fixed before merge; items marked **deferred** are tracked in their
respective plan/spec docs and linked from this file.

---

## Overview

Phase 3 of the session-management plan — the last prereq before the
Model Council. Adds a bounded per-session response store, a
configurable response boundary detector, and the wiring that hooks
both into the production `App` pipeline. Tasks 1+2 (store) and 3+6
(detector + fixtures) developed in parallel via subagents in
worktrees, both cherry-picked back conflict-free thanks to the
pre-staged Task 0 interface (`StoredTurn`, `TurnSink`,
`ResponseStore` skeleton).

End-to-end pipeline: `send_prompt` → `boundary_detector.on_prompt_submitted`
→ PTY reader → `Event::PtyOutput` → `App::handle_event` →
`feed_pty_data` → `boundary_detector.on_pty_data` (accumulates) →
`App::check_all_attention` → `check_response_boundaries` →
`boundary_detector.check_for_boundary` (matches idle marker) →
`StoreAndBus::push_turn` → store + `SessionEvent::ResponseComplete`
→ `get_response`/`get_latest_response` retrieves the body.

---

## Code correctness

**✅ Solid**

- Borrow gymnastics in `check_response_boundaries` work correctly via
  disjoint field access.
- `ResponseStore` eviction policy correct (`while total_bytes > budget
  && len > min_retain`).
- `StoreAndBus::push_turn` captures `turn_id` before moving `turn`.
- `ansi_strip` UTF-8 safety verified — strips only ASCII bytes or
  contiguous OSC payloads bounded by ASCII terminators, so multi-byte
  UTF-8 sequences stay intact.
- `Session::dummy_exited` and `make_dummy_session` both initialize
  `response_store`.

**⚠️ Real correctness gaps**

| # | Item | Disposition |
|---|---|---|
| **C1** | `ResponseBoundaryDetector` per-session HashMap leak — killed/reaped sessions never get removed from `states`, so `body_bytes` buffers leak across the lifetime of a long-running TUI. | **Applied (A1).** Added `ResponseBoundaryDetector::forget_session(session_id)` and called it from `SessionManager::kill` (after the `Exited` publish) and from the `reap_exited` transition loop. Added `#[cfg(test)] pub(crate) fn knows_session(&self, id) -> bool` test seam. Two regression tests pin the cleanup contract. |
| **C2** | The idle marker regex matches the **ANSI-stripped** body, not raw bytes. Future contributor pinning the real Claude Code marker could waste hours debugging this if they include escape sequences in their pattern. | **Applied (A2).** Added multi-paragraph doc warning to both `ResponseBoundaryDetector::new` and `for_claude_code()` explicitly stating that markers are matched against post-strip text and giving an example of the visible-form marker shape. |
| **C3** | `make_dummy_session` references `super::super::response_store::ResponseStore::new()` — fragile path. | **Applied (A3).** Replaced with `crate::session::ResponseStore::new()` using the re-export. |

## Project conventions

**✅ Followed:** module layout, doc comments, TDD red→green flow,
pre-commit hook compliance, sparse comments, the test seam pattern
from PR #8 (`set_boundary_detector_for_test`).

**⚠️ Worth fixing in this PR**

| # | Item | Disposition |
|---|---|---|
| **K1** | Stale `#[allow(dead_code)]` annotations on `ResponseBoundaryDetector` (struct + impl block). Task 4's wiring made the type reachable through `for_claude_code` from production code. | **Applied (A4).** Removed both annotations. Verified empirically with `cargo build`: warnings stayed clean. The annotations were genuinely stale — Task 4's `SessionManager::boundary_detector` field is the production caller that closes the reachability gap. |
| **K2** | Two `impl SessionManager` blocks with `StoreAndBus` sandwiched between. Doc-comment claimed the split helped the borrow checker; it doesn't (the borrow checker only cares about field disjointness, not impl-block boundaries). | **Applied (A5).** Unified into one `impl SessionManager` block. Moved `StoreAndBus` and its `TurnSink` impl to immediately after the `impl` block ends, with a clearer doc-comment explaining its module-private role. Verified compiles + tests still pass. |

## Performance implications

| # | Item | Disposition |
|---|---|---|
| **P1** | `ansi_strip` runs on the entire body buffer on every `check_for_boundary` call. For long responses this becomes O(n × ticks) work where it could be O(n) total. | **Deferred (D2).** Current volumes don't warrant the complexity. Worth flagging as a known cost. Future optimization: incremental strip as bytes arrive, or only strip the suffix new since last check. |
| **P2** | `String::from_utf8_lossy` allocates on every check. | **Deferred** — same rationale as P1. |
| **P3** | `feed_pty_data` is called per `Event::PtyOutput` chunk. Just delegates to `extend_from_slice`. Fast. ✓ | none |

## Test coverage

**+38 tests in the initial PR; +2 in the review pass** (regression
guards for the C1 leak fix).

| Surface | Tests |
|---|---|
| `ResponseStore` (Task 1+2 unit) | 14 |
| `ResponseBoundaryDetector` + `ansi_strip` (Task 3+6) | 15 |
| `SessionManager` Phase 3 wiring (Task 4+5 unit) | 8 + 2 (C1 regression) = 10 |
| Real-PTY end-to-end (Task 8) | 1 |

**Gaps acknowledged**

| # | Gap | Disposition |
|---|---|---|
| **T1** | No test for the C1 HashMap leak. | **Applied alongside C1 fix.** Added `kill_drops_boundary_detector_state_for_session` and `reap_exited_drops_boundary_detector_state_for_transitioned_sessions`. |
| **T2** | No `App`-level unit test for `feed_pty_data` being called from `Event::PtyOutput` arm. | **Deferred (D1).** The cat-based e2e covers the wire path; adding `App` test infrastructure is a separate concern. |
| **T3** | No test for `ResponseStore::with_budget(0, 0)`. | **Skipped** — covered by `with_budget(50, 0)` in spirit. |

## Security considerations

- No new IPC, no network, no unsafe code, no untrusted input parsing.
- **`ansi_strip` defense:** stripping ANSI before storing prevents
  stored bodies from re-emitting escape sequences when displayed by
  future bus subscribers. Defense-in-depth. ✓
- **`regex` crate is guaranteed linear-time** — no ReDoS vector if a
  future MCP code path lets external callers supply a marker regex.
  Worth noting for the Phase 5 plan.
- **`text` parameter to `send_prompt` is still not sanitized** at
  this layer — already tracked from PR #8 review for Phase 5 Task 1.

## Risks called out at review time

| # | Risk | Disposition |
|---|---|---|
| **R1** | C1 — HashMap leak. | **A1 fix applied** — `forget_session` called from kill + reap_exited paths. |
| **R2** | C2 — Hidden marker matching contract. | **A2 doc fix applied** — loud warnings on both `new` and `for_claude_code`. |
| **R3** | K1 — Stale `#[allow(dead_code)]` annotations. | **A4 fix applied** — removed, verified clean. |
| **R4** | K2 — Two impl blocks with the wrong rationale. | **A5 fix applied** — unified, `StoreAndBus` moved to module scope after the impl. |
| **R5** | C3 — Fragile `super::super::` path in `make_dummy_session`. | **A3 fix applied** — replaced with `crate::session::ResponseStore::new()`. |
| **R6** | `for_claude_code()` placeholder marker won't fire on real Claude. | **Already documented** as known limitation in the PR description. The follow-up empirical pinning step is gated on running a real Claude session. |

---

## Action items applied in this PR

| ID | Change | File |
|---|---|---|
| **A1** | `forget_session` + cleanup hooks in `kill` and `reap_exited` + 2 regression tests + `knows_session` test seam | `src/pty/response_boundary.rs`, `src/session/manager.rs` |
| **A2** | Doc warnings on `new` and `for_claude_code` re: ANSI-stripped marker matching | `src/pty/response_boundary.rs` |
| **A3** | `super::super::response_store::ResponseStore::new()` → `crate::session::ResponseStore::new()` | `src/session/manager.rs` |
| **A4** | Removed `#[allow(dead_code)]` from `ResponseBoundaryDetector` (struct + impl) | `src/pty/response_boundary.rs` |
| **A5** | Unified two `impl SessionManager` blocks; moved `StoreAndBus` to module scope after the impl | `src/session/manager.rs` |

## Forward-looking items deferred

| ID | Item | Where tracked |
|---|---|---|
| **D1** | App-level `feed_pty_data` regression test | This file |
| **D2** | Incremental ANSI strip optimization | This file |
| **for_claude_code marker pinning** | Empirical Claude Code idle prompt regex | PR description + this file. Will be a follow-up commit after PR #9 merges. |

## Lessons learned

- **Pre-staging a shared interface** (Task 0) made the parallel
  subagent work essentially conflict-free. Worth carrying forward
  to Phase 4+ if those phases use parallelism.
- **The `expect("ansi_strip preserves utf8")` reasoning needed
  manual verification** — the safety isn't immediately obvious. Worth
  considering whether to write up the byte-level argument as a code
  comment so the next reader doesn't have to re-derive it.
