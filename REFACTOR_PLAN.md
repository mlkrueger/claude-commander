# Refactor Plan — Phases 1–3

*Detailed execution plan for the top-priority findings in `TECH_ANALYSIS.md`. Each task has a checkbox. Parallelism is called out explicitly — anything under the same "parallel group" can be worked on simultaneously (ideally as separate branches/PRs), and "sequential" items must land in order.*

**Estimated total:** ~2 working days for a focused pass.

**Correction from `TECH_ANALYSIS.md`:** ratatui 0.30 is the latest release on crates.io — no bump is available today. Dropping that recommendation; revisit if 0.31 ships before we finish.

---

## Phase 1 — Stop the bleeding (~2 hours)

**Goal:** eliminate the two CRITICAL panic risks and the silent PTY failures without changing architecture. Every task is local and individually revertable.

**Parallelism:** Tasks 1.1, 1.2, 1.4, and 1.5 are fully independent and can be done in parallel (four separate branches). Task 1.3 depends on 1.1 landing first because it reuses the same helper. Task 1.6 is the verification step and must be last.

### Parallel group A — independent fixes

- [ ] **1.1** Add a `lock_parser` helper in `src/pty/session.rs` that recovers poisoned mutexes:
  ```rust
  fn lock_parser(p: &Mutex<vt100::Parser>) -> MutexGuard<'_, vt100::Parser> {
      p.lock().unwrap_or_else(|e| e.into_inner())
  }
  ```
  Replace both `parser.lock().unwrap()` sites inside `pty/session.rs` (`:85`, `:131`).

- [ ] **1.2** Wrap the PTY reader loop body (`pty/session.rs:72-97`) in `std::panic::catch_unwind`. On panic, send `Event::SessionExited { id, reason: "reader panicked: <msg>" }` via `event_tx` before the thread dies. Keep the normal EOF path unchanged.

- [ ] **1.4** Fix the two risky `path.strip_prefix(&home).unwrap()` calls at `app.rs:894` and `app.rs:906`. Use `.ok()` + a graceful fallback (probably: show the absolute path if not under `$HOME`).

- [ ] **1.5** Replace the 8 `let _ = session.{write,resize}(...)` sinks in `app.rs` (`:223, :421, :658, :692, :722, :968, :974, :986`) with logged errors via `log::warn!`. Decision already made below — log only for resize, log + mark exited for repeated write failures.
  - Add a small helper on `Session` (or on the future `SessionManager`): `fn try_write(&mut self, bytes: &[u8])` that tracks consecutive failures and transitions `status` to `SessionStatus::Exited("write failed: ...")` after N (start with N=3).

### Sequential — depends on 1.1

- [ ] **1.3** Now that `lock_parser` exists, replace the 3 `parser.lock().unwrap()` sites in `src/app.rs` (`:307, :331, :1217`) with calls to the helper. Export it as `pub(crate) fn` from `pty::session` or move to a shared `pty::parser` module.

### Sequential — verification gate

- [ ] **1.6** Manual smoke test (no automated tests for app-level code yet):
  - [ ] Start `ccom`, spawn 2 sessions, verify both render output.
  - [ ] Kill one session's underlying process externally (`kill -9 $(pgrep claude | head -1)`) and confirm the UI marks it exited rather than hanging.
  - [ ] Trigger a deliberate write failure (e.g., close the PTY pipe) and confirm a `log::warn!` shows up in `RUST_LOG=warn` output and the session eventually marks itself exited.
  - [ ] Resize the terminal aggressively during streaming output and confirm no panic, no stuck sessions.

**Phase 1 exit criteria:**
- Zero `.unwrap()` on `parser.lock()` anywhere in the codebase.
- Zero `let _ = session.{write,resize}(...)` anywhere in the codebase.
- All smoke-test steps pass.
- Single PR (or up to 4 small PRs if working in parallel).

---

## Phase 2 — Extract `SessionManager` (~1 day)

**Goal:** pull session lifecycle and lookup out of `App` into a dedicated, testable module. Shrinks `app.rs` by ~200 LOC and makes Phase 3 possible.

**Parallelism:** This is mostly **sequential** — each step builds on the previous one because they all touch the same types. The one exception is Task 2.6 (documentation/rustdoc) which can happen in parallel with 2.5 (call-site migration). Do **not** start Phase 2 until Phase 1 has landed on `main`.

### Sequential core

- [ ] **2.1** Create `src/session/mod.rs` and `src/session/manager.rs`. Move the existing `pty::session::Session` and `SessionStatus` types into `src/session/` as well, so the module owns the full session concept. Keep `pty/` responsible only for raw PTY I/O (spawn/read/write), not session state.
  - Decision point: name is `session::` vs `sessions::`. Going with singular `session::` for consistency with `ui::`, `pty::`, `fs::`.

- [ ] **2.2** Define `SessionManager`:
  ```rust
  pub struct SessionManager {
      sessions: Vec<Session>,
      selected: usize,
      next_id: usize,
  }
  ```
  With methods (signatures to finalize during implementation):
  - [ ] `new() -> Self`
  - [ ] `len(&self) -> usize`, `is_empty(&self) -> bool`
  - [ ] `iter(&self) -> impl Iterator<Item = &Session>`
  - [ ] `iter_mut(&mut self) -> impl Iterator<Item = &mut Session>`
  - [ ] `get(&self, id: usize) -> Option<&Session>`
  - [ ] `get_mut(&mut self, id: usize) -> Option<&mut Session>` — replaces the 5+ inline `iter_mut().find(|s| s.id == id)` patterns at `app.rs:170, :418, :691, :719, :1217`
  - [ ] `selected_index(&self) -> Option<usize>` — returns `None` when empty
  - [ ] `selected(&self) -> Option<&Session>`
  - [ ] `selected_mut(&mut self) -> Option<&mut Session>` — replaces all `self.sessions[self.selected]` panics at `app.rs:691, :718, :736`
  - [ ] `select_prev(&mut self)`, `select_next(&mut self)` — saturating, cannot desynchronize
  - [ ] `spawn(&mut self, config: SpawnConfig) -> anyhow::Result<usize>` — returns new id
  - [ ] `kill(&mut self, id: usize) -> bool` — fixes up `selected` if the killed session was at or before the current index
  - [ ] `refresh_contexts(&mut self)` — moves the tick loop from `app.rs:190`
  - [ ] `check_attention(&mut self)` — moves the attention-check logic from its current home in `app.rs`
  - **Invariant (maintained internally):** `sessions.is_empty() || selected < sessions.len()`. Enforced at construction and on every mutation.

- [ ] **2.3** Move `spawn_session`, `spawn_from_modal`, and `kill_selected` out of `App` and into `SessionManager`. The modal's path validation stays in `App` (it's UI-layer concern); only the actual spawn call migrates.

- [ ] **2.4** Replace the `pub sessions: Vec<Session>`, `pub selected: usize`, `pub next_id: usize` fields on `App` with a single `pub(crate) sessions: SessionManager`.

### Parallel group B — once 2.1–2.4 are in place

- [ ] **2.5** Migrate every remaining call site in `app.rs` to go through `SessionManager`. Grep for:
  - [ ] `self.sessions.iter_mut().find(` (5 sites)
  - [ ] `self.sessions[self.selected]` (3 sites: `:691, :718, :736`)
  - [ ] `self.sessions.push(` (spawn paths)
  - [ ] `self.sessions.remove(` (kill paths)
  - [ ] Any `self.selected` / `self.next_id` reads/writes
  Each should become a `self.sessions.<method>()` call.

- [ ] **2.6** Add module-level rustdoc to `src/session/manager.rs` explaining the invariant and the ownership model. Short — ~15 lines. (Can be done in parallel with 2.5 by a second contributor, or squeezed in before the PR.)

### Sequential — verification gate

- [ ] **2.7** Verification:
  - [ ] `cargo build` clean.
  - [ ] `cargo clippy --all-targets` — no new warnings introduced.
  - [ ] Existing test suite (`cargo test`) passes unchanged.
  - [ ] Manual smoke test: spawn 3 sessions, navigate between them, kill the middle one, confirm selection jumps correctly and no panic.
  - [ ] Manual smoke test: kill the currently-selected session when it's the last in the list — confirm `selected` gets clamped correctly.

**Phase 2 exit criteria:**
- `app.rs` LOC reduced from ~1,664 to ≤ ~1,450.
- Zero direct indexing into `sessions` anywhere outside `session/manager.rs`.
- Zero `iter_mut().find(|s| s.id == ...)` patterns remaining.
- `SessionManager` has no `pub` fields.
- Single PR (the internal tasks are sequential — splitting this across multiple PRs is more pain than it's worth).

---

## Phase 3 — First real tests (~½ day)

**Goal:** Lock in the invariants of `SessionManager` and the existing small modules that currently have thin coverage. This is the first meaningful test of the core state machine.

**Parallelism:** **Highly parallel.** Every task in this phase is independent — each test file targets a different module. Four contributors (or four focused sessions) could work on 3.1–3.4 simultaneously. Phase 3 must start after Phase 2 lands because 3.1 depends on `SessionManager` existing.

### Parallel group C — independent test files

- [ ] **3.1** Create `tests/session_manager.rs`. Unit tests:
  - [ ] `new() is empty`, `len() == 0`, `selected() is None`
  - [ ] `spawn` assigns monotonically increasing ids and keeps `selected` valid
  - [ ] `get_mut(unknown_id)` returns `None` without panicking
  - [ ] `selected_mut()` after removing the currently-selected session — should either clamp to the last session or return `None` if the list became empty
  - [ ] `select_next` / `select_prev` are saturating at both ends
  - [ ] `kill` of a session before `selected` decrements `selected` correctly
  - [ ] `kill` of a session after `selected` leaves `selected` unchanged
  - [ ] `kill` of the last remaining session leaves manager empty and `selected_mut()` returns `None`
  - **Do not** test the actual PTY spawn — inject a `SpawnConfig` variant or a test helper that constructs a `Session` in a dummy `Exited` state to avoid forking real processes.

- [ ] **3.2** Add a property test for `SessionManager` using `proptest` (new dev-dependency in `[dev-dependencies]`). Strategy: generate random sequences of `Spawn`, `Kill(id)`, `SelectNext`, `SelectPrev` operations; after each step assert the invariant:
  ```
  manager.is_empty() || manager.selected_index().unwrap() < manager.len()
  ```
  Also assert ids are never reused after kill.
  - Decision point: if you'd rather not add `proptest`, replace this task with a hand-rolled table-driven test covering ~20 scenarios. Ping me and I'll rewrite it.

- [ ] **3.3** Add tests for `claude/rate_limit.rs` — it has fiddly JSON-shape parsing and currently zero coverage. Cover:
  - [ ] Happy-path parse of a real `rate_limit.json` blob (capture one from a live session and vendor it as a fixture under `tests/fixtures/`).
  - [ ] Missing `resets_at` field.
  - [ ] Malformed JSON returns an error rather than panicking.
  - [ ] Zero / negative percentages are clamped or rejected.

- [ ] **3.4** Add tests for `claude/usage.rs` — also currently zero coverage. Cover:
  - [ ] Parse a `conversation.jsonl` fixture with a mix of user/assistant/tool messages and verify the token totals.
  - [ ] Empty file returns zero, not an error.
  - [ ] Lines that fail to parse are skipped without aborting the whole file.

### Sequential — verification gate

- [ ] **3.5** Verification:
  - [ ] `cargo test` — all tests pass.
  - [ ] `cargo test -- --ignored` if any tests need `#[ignore]` for PTY-related reasons.
  - [ ] Count coverage delta informally: before Phase 3, `tests/` is 236 LOC with ~17 tests; after, aim for ≥350 LOC with ≥30 tests. This isn't a hard gate, just a sanity check that Phase 3 moved the needle.

**Phase 3 exit criteria:**
- `SessionManager` has ≥8 unit tests plus the property test (or ≥20 table-driven cases if you skip `proptest`).
- `rate_limit.rs` and `usage.rs` each have ≥3 tests against vendored fixtures.
- No test uses real PTY spawning or real Claude CLI — tests must run offline and in CI.
- One PR per test file is fine; four small PRs are equally acceptable.

---

## Overall parallelism map

```
Phase 1  ──►  [1.1] ─┐
              [1.2]  ├─► [1.3] ─► [1.6 verify] ─► merge
              [1.4]  │
              [1.5] ─┘

Phase 2  ──►  [2.1] ─► [2.2] ─► [2.3] ─► [2.4] ─► [2.5] ─┐
                                                  [2.6] ─┴─► [2.7 verify] ─► merge

Phase 3  ──►  [3.1] ─┐
              [3.2]  ├─► [3.5 verify] ─► merge
              [3.3]  │
              [3.4] ─┘
```

**Hard ordering between phases:** Phase 2 must not start until Phase 1 is on `main` (Phase 1 touches `pty/session.rs` which Phase 2 moves). Phase 3 must not start until Phase 2 is on `main` (Phase 3 tests `SessionManager`).

**Within a phase:** use the parallel groups above to split work across branches. Phase 3 is the best candidate for parallelization (four fully independent test files).

---

## Decisions needed before starting

- [ ] **D1** (Phase 1.5) Confirm the retry count before a write-failing session is marked exited. Default: **3 consecutive failures**.
- [ ] **D2** (Phase 2.1) Confirm module name: **`session::`** (singular) vs `sessions::` (plural). Default: **singular**, to match `ui::`, `pty::`, `fs::`.
- [ ] **D3** (Phase 3.2) Add `proptest` as a dev-dependency? Default: **yes**, it's a ~5-minute integration and the payoff for invariant testing is large.
- [ ] **D4** Where do logs go? `env_logger` is already wired but nothing reads it. Default: **document `RUST_LOG=warn ccom` in the README** as the way to see PTY errors after Phase 1. Longer-term, Phase 6 can add an in-TUI log panel.

Tell me which defaults to flip and I'll start on Phase 1.
