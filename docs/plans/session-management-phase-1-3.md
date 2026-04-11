# Session Management — Phases 1–3 Implementation Plan

Status: ready to start
Date: 2026-04-11
Design spec: [`docs/designs/session-management.md`](../designs/session-management.md)

This plan implements the prerequisites for the Model Council. When all
three phases have landed on `main`, `docs/model-council.md` Phase 2 is
unblocked.

## Ground rules (apply to all phases)

1. **No work on `main`.** Every phase lives on its own branch cut from
   fresh `main`. Pull `main`, branch, work, PR back.
2. **One phase per PR.** Each phase is one PR. Do not combine phases.
   Do not start the next phase until the current phase's PR has merged
   to `main`.
3. **Phase branch naming.** `session-mgmt/phase-N-<slug>` — e.g.
   `session-mgmt/phase-1-event-bus`.
4. **Parallel tasks within a phase use git worktrees.** When tasks
   inside a phase are marked **PARALLEL**, run them in separate
   worktrees off the phase branch:
   ```
   git worktree add ../ccom-phase-N-task-X session-mgmt/phase-N-<slug>
   cd ../ccom-phase-N-task-X
   git checkout -b session-mgmt/phase-N-<slug>-task-X
   # work, commit
   ```
   Each parallel task opens its own PR *into the phase branch* (not
   into `main`). The phase branch rolls up and then PRs to `main`.
   Worktrees let you context-switch between parallel tasks without
   stashing.
5. **Definition of done for each phase.**
   - Code builds with no new warnings.
   - New and existing tests pass.
   - Manual verification: run `cargo run`, exercise the relevant code
     path in the TUI, confirm behavior matches the phase's acceptance
     criteria (per `~/.claude/CLAUDE.md` workflow rule — do not skip).
   - Draft PR reviewed, approved, merged via squash-merge to `main`.
6. **Never commit without explicit approval** (per global `CLAUDE.md`).
   The commit step in every task is "prepare the commit and ask."

---

## Sequencing constraints — what CAN'T be parallelized

### Cross-phase (hard serial between phases)

- **Phase 1 → Phase 2.** Phase 2's `send_prompt` depends on Phase 1's
  `EventBus` (to publish `PromptSubmitted`) and on the `TurnId`
  newtype defined in Phase 1's `events` module. **Do not start Phase
  2 until Phase 1 is merged to `main`.** No worktree shortcut — the
  dependency is through types that don't exist until Phase 1 lands.
- **Phase 2 → Phase 3.** Phase 3's detector keys its "prompt at line
  N" tracking off the `turn_id` returned by `send_prompt`, and its
  integration tests drive input through `send_prompt`. The detector
  literally has no signal to develop against without Phase 2.
  **Do not start Phase 3 until Phase 2 is merged to `main`.**
- **Model Council** cannot be started until Phase 3 is merged. Model
  pinning (Council Phase 1) is the one exception — it's independent
  of all session-management work and can run on its own branch in
  parallel with any phase here.

### Within-phase hard orderings

**Phase 1:**
- Task 1 (event taxonomy) → Task 2 (`EventBus`) — the bus owns
  `Sender<SessionEvent>`, so `SessionEvent` must exist first.
- Task 2 → Task 3 (wire into `App`) — `App` constructs the bus.
- Task 3 → Task 4 (publishers) — publishers call `bus.publish()`
  through the `App`-owned `Arc<EventBus>`.
- **Only Task 5 (unit tests) can parallelize**, and only once Task
  2 has merged onto the phase branch — the bus type must exist
  before tests can reference it. The worktree is worth it only if
  Tasks 3–4 are going to take more than a session; otherwise just
  do them all in-line.

**Phase 2:**
- Task 1 (TurnId counter on `Session` + submit-byte constant) is a
  prerequisite for Task 2 (`send_prompt`). Task 3 (`broadcast`)
  technically doesn't read from the counter, but the PARALLEL
  convention still branches both 2 and 3 off the phase branch
  *after* Task 1 has been committed on it. Do not try to start
  Task 2 before Task 1 — `send_prompt` has nowhere to store the
  counter.
- Tasks 4–6 (unit tests, smoke test, manual verify) are
  **sequential** on the phase branch after Tasks 2 and 3 merge in.
  Do not open the phase PR against `main` until all three are
  complete.

**Phase 3:**
- Task 1–2 (store + store tests) are independent of Task 3
  (detector) *only because* the detector stubs
  `trait TurnSink { fn push(&mut self, turn: StoredTurn); }` until
  the real store lands. Without that stub, the worktrees become
  serial. Include the stub in the shared prelude that both
  worktrees start from — otherwise the detector worktree can't
  build.
- Task 4 (hook detector into PTY reader) requires **both** Tasks
  1–2 and Task 3 merged onto the phase branch. It's the
  convergence point.
- Task 5 (`SessionManager` accessors) requires Tasks 1–2. Can ride
  on the store worktree.
- Task 6 (PTY fixtures) is pure data capture and has no code
  dependencies — it can start as early as the phase branch exists,
  in either worktree.
- Task 7 (detector tests) requires Task 3 **and** Task 6.
- Task 8 (end-to-end integration) requires Tasks 4 **and** 5 — it
  needs the full hookup plus the accessors.
- Task 9 (manual verify) is last and depends on all of the above.

---

## Phase 1 — Internal event bus

**Branch:** `session-mgmt/phase-1-event-bus`
**Design ref:** session-management §2
**Blocks:** Phases 2 and 3, Council Phase 2+.

### Goal

Add a structured pub/sub bus carrying `SessionEvent`s to multiple
subscribers, running alongside the existing `App` event path. No
behavior change for existing users. Marker-only events (no bodies).

### Tasks

1. **Event taxonomy.** Create `src/session/events.rs` with:
   - `TurnId(u64)` newtype.
   - `SessionEvent` enum: `Spawned`, `PromptSubmitted { session_id,
     turn_id }`, `ResponseComplete { session_id, turn_id }`,
     `PromptPending { session_id, kind }`, `Exited { session_id,
     status }`, `StatusChanged { session_id, status }`.
   - Derives: `Debug`, `Clone`.

2. **`EventBus` struct.** In the same file:
   - Holds `Arc<Mutex<Vec<std::sync::mpsc::Sender<SessionEvent>>>>`.
   - `subscribe(&self) -> mpsc::Receiver<SessionEvent>` — creates a
     paired channel, stores the sender, returns the receiver.
   - `publish(&self, event: SessionEvent)` — clones to each sender,
     drops any sender that has been disconnected (prunes dead
     subscribers).
   - No tokio. No new deps — `std::sync::mpsc` is sufficient.

3. **Wire into `App` / `SessionManager`.** Construct an `Arc<EventBus>`
   in `App::new()`, pass into `SessionManager`. Hold alongside existing
   event machinery; do *not* migrate existing consumers yet.

4. **Publish existing transitions:**
   - `SessionManager::spawn()` → `Spawned`.
   - `SessionManager::reap_exited()` → `Exited`.
   - Status mutation sites → `StatusChanged`.
   - Existing `PromptDetector` hits → `PromptPending`.
   `PromptSubmitted` and `ResponseComplete` are **not** emitted in
   Phase 1 (those come with Phases 2 and 3).

5. **Unit tests.** `tests/event_bus.rs` or `src/session/events.rs`
   `#[cfg(test)]`:
   - Single subscriber receives a published event.
   - Two subscribers each receive their own copy.
   - Dropped receiver is pruned on next publish, no panic.
   - `Spawned` + `Exited` flow through a fake `SessionManager` run.

6. **Manual verification.** Add a temporary dev-only subscriber behind
   a `RUST_LOG=ccom::events=debug` line that logs each event. Run
   `cargo run`, spawn a session, kill it, confirm `Spawned` and
   `Exited` appear in the log. **Remove** the dev subscriber before
   opening the PR.

### Parallelism

Phase 1 is mostly sequential: Task 1 → Task 2 → Task 3 → Task 4. Task
5 (tests) can start as soon as Task 2 lands and run in a worktree
while Tasks 3–4 are being wired. That's the only parallel slice worth
spinning up a worktree for.

### Acceptance

- `cargo build` and `cargo test` are clean.
- A test subscriber receives `Spawned`, `Exited`, `StatusChanged`,
  `PromptPending` during a manual session run.
- No existing consumer is broken — TUI still works as before.
- PR description references this plan and design §2.

---

## Phase 2 — Programmatic write path

**Branch:** `session-mgmt/phase-2-write-path`
**Design ref:** session-management §3
**Depends on:** Phase 1 merged to `main`.
**Blocks:** Phase 3 (detector needs a real signal to develop against),
Council Phase 2.

### Goal

Two new `SessionManager` methods that let callers (main loop now, MCP
handlers later) submit prompts and broadcast bytes to sessions without
going through the keystroke handler. `send_prompt` is the first

### Carry-forward from Phase 1 PR review

- **`TurnId.0` field visibility (PR #7 review item K2).** Phase 1 left
  `TurnId(pub u64)` so tests can construct `TurnId(7)` directly. Once
  Phase 2's `Session::next_turn_id` allocator exists and is the
  canonical mint site, drop the field to `pub(crate)` and add a
  `pub fn new(value: u64) -> Self` constructor. Tests in
  `src/session/events.rs` will need to switch from `TurnId(7)` to
  `TurnId::new(7)`. Tracked in `docs/pr-review-pr7.md`.
emitter of `PromptSubmitted` on the bus.

### Tasks

1. **`TurnId` counter on `Session`.** Add `next_turn_id: u64` field
   (init 0). Private; only `send_prompt` increments it. Define the
   submit-key byte sequence (terminal newline / Claude Code submit
   chord) as a module constant so both `send_prompt` and any future
   caller reference one source of truth.

2. **`SessionManager::send_prompt(id, text) -> Result<TurnId>`:**
   - Look up the session.
   - Increment `next_turn_id`, capture the new value.
   - Write `text` followed by the submit byte sequence via
     `Session::try_write()`.
   - Publish `SessionEvent::PromptSubmitted { session_id, turn_id }`
     on the bus.
   - Return the new `turn_id` so the caller can correlate with the
     matching `ResponseComplete` in Phase 3.

3. **`SessionManager::broadcast(ids, bytes) -> BroadcastResult`.**
   Loop `try_write` over each id. Return a small struct with
   per-session success/error entries so callers can report partial
   failures. Does *not* increment `turn_id` (it's raw bytes, not
   necessarily a prompt — the Council will use `send_prompt` per
   session for prompts, `broadcast` only for cases where raw bytes
   really are the right abstraction).

4. **Unit tests.** Use the existing PTY test harness (or add a mock
   `Session::try_write` seam if needed):
   - `send_prompt` increments `turn_id` on successive calls.
   - `send_prompt` publishes `PromptSubmitted` with the returned
     `turn_id`.
   - `send_prompt` writes `text + submit_seq` to the PTY.
   - `broadcast` writes to all targeted sessions and reports
     per-session outcomes.

5. **Integration/smoke test.** `tests/send_prompt_smoke.rs`: spawn a
   real `cat`-backed PTY (lightweight stand-in), call `send_prompt`,
   read back from the PTY, assert the bytes arrived.

6. **Manual verification.** Add a hidden dev keybind (e.g. `Ctrl+Alt+T`)
   that calls `send_prompt(focused_id, "hello from commander")`. Run
   `cargo run`, spawn a Claude session, press the binding, confirm
   the prompt is typed and submitted. Remove the dev keybind before
   opening the PR.

### Parallelism

Tasks 2 and 3 are independent — both build on Task 1 but not on each
other. **PARALLEL:** run Task 2 and Task 3 in separate worktrees once
Task 1 has merged into the phase branch.

```
# after Task 1 is committed on session-mgmt/phase-2-write-path
git worktree add ../ccom-phase-2-send-prompt session-mgmt/phase-2-write-path
git worktree add ../ccom-phase-2-broadcast   session-mgmt/phase-2-write-path

# in each worktree, create a sub-branch
cd ../ccom-phase-2-send-prompt
git checkout -b session-mgmt/phase-2-send-prompt

cd ../ccom-phase-2-broadcast
git checkout -b session-mgmt/phase-2-broadcast
```

Each sub-branch opens a PR *into* `session-mgmt/phase-2-write-path`.
When both land on the phase branch, continue with Tasks 4–6 (tests,
smoke, manual verify), then open the phase PR against `main`.

### Acceptance

- Calling `send_prompt` on a live Claude session actually submits the
  prompt (confirmed manually).
- `PromptSubmitted` events appear on the bus with monotonic
  `turn_id`s (confirmed via the Phase 1 debug subscriber pattern).
- `broadcast` correctly fans raw bytes to multiple sessions.
- All new and existing tests pass.

---

## Phase 3 — Response boundary detector + response store

**Branch:** `session-mgmt/phase-3-response-detector`
**Design ref:** session-management §4
**Depends on:** Phases 1 and 2 merged to `main`.
**Blocks:** Council Phase 3 (synthesizer).

### Goal

Detect when a session has finished producing a response to a given
turn, store the response body in a bounded per-session store, and
emit `ResponseComplete` on the bus. Ship `get_response` /
`get_latest_response` accessors.

### Tasks

1. **`StoredTurn` + response store.** New file
   `src/session/response_store.rs`:
   - `StoredTurn { turn_id, started_at, completed_at: Option<Instant>,
     body: String }`.
   - `ResponseStore { turns: VecDeque<StoredTurn>, total_bytes: usize,
     budget_bytes: usize, min_retain: usize }`.
   - Constructor defaults: `budget_bytes = 256 * 1024`,
     `min_retain = 3`.
   - `push(&mut self, turn: StoredTurn)` — append, then evict from
     the front while `turns.len() > min_retain` *and*
     `total_bytes > budget_bytes`. Update `total_bytes` on every
     mutation.
   - `get(&self, turn_id) -> Option<&StoredTurn>`.
   - `latest(&self) -> Option<&StoredTurn>`.

2. **Response store unit tests.** Same file `#[cfg(test)]`:
   - Push below budget: nothing evicted.
   - Push above budget, more than `min_retain`: oldest evicted.
   - Push a single oversized turn: retained because of `min_retain`.
   - `min_retain = 3` boundary: store of 4 turns, push a huge one —
     assert exactly last 3 survive.
   - `get` / `latest` correctness.

3. **`ResponseBoundaryDetector`.** New file `src/pty/response_boundary.rs`
   (sibling of `detector.rs`):
   - Per-session state: `active_turn: Option<TurnId>`,
     `turn_start_line: Option<usize>`, `idle_seen: bool`.
   - API:
     - `on_prompt_submitted(session_id, turn_id, current_line)` —
       stash `active_turn` and `turn_start_line`.
     - `on_pty_tick(session_id, parser: &vt100::Parser)` — inspect
       the parser's current screen, detect idle-prompt pattern
       (reuse `PromptDetector`'s idle matcher), and if we've
       transitioned from non-idle to idle while an `active_turn` is
       set, extract the delta, ANSI-normalize, return a
       `BoundaryHit { turn_id, body }`.
   - Unit-test against recorded PTY byte fixtures (see Task 6).

4. **Hook detector into the PTY reader loop.** Add the detector
   alongside the existing `PromptDetector` in the reader thread. On
   `BoundaryHit`:
   - Call `ResponseStore::push` on the session's store.
   - Publish `ResponseComplete { session_id, turn_id }` on the bus.

5. **`SessionManager` accessors.**
   - `get_response(session_id, turn_id) -> Option<StoredTurn>`
     (clones the entry out so callers don't hold a lock).
   - `get_latest_response(session_id) -> Option<StoredTurn>`.

6. **PTY fixtures.** Capture 3–5 real Claude Code PTY byte streams
   (one short response, one with tool-use chatter, one with
   streaming markdown, one mid-response cancel, one multi-turn
   conversation) into `tests/fixtures/pty/*.bin`. These are the
   detector's regression corpus. Document how they were captured in
   `tests/fixtures/pty/README.md`.

7. **Detector tests.** `tests/response_boundary.rs`: replay each
   fixture through the detector, assert the emitted
   `(turn_id, body)` pairs. This is where the design spec's warning
   about "fuzzy around tool-use chatter" gets empirically pinned
   down.

8. **Integration test.** `tests/end_to_end_turn.rs`: spawn a real
   Claude session, subscribe to the bus, call
   `send_prompt(id, "say hi")`, block on `ResponseComplete` with a
   30s timeout, call `get_response(id, turn_id)`, assert body is
   non-empty and contains some part of the response. Mark
   `#[ignore]` if it requires a real API key — keep the fixture tests
   (Task 7) as the always-running regression path.

9. **Manual verification.** Run `cargo run`, spawn a Claude session,
   use the Phase 2 dev keybind to send a prompt, wait for the
   response, add a dev-only command (or reuse the Phase 1 debug
   subscriber) that prints the stored body. Confirm it matches what
   you see on screen. Remove dev hooks before opening the PR.

### Parallelism

Tasks 1–2 (store) and Tasks 3 + 6 (detector + fixtures) are
independent. **PARALLEL:** two worktrees off the phase branch.

```
git worktree add ../ccom-phase-3-store    session-mgmt/phase-3-response-detector
git worktree add ../ccom-phase-3-detector session-mgmt/phase-3-response-detector
```

The detector sub-branch can stub the store with a trait
(`trait TurnSink { fn push(&mut self, turn: StoredTurn); }`) while the
real store is being built in the other worktree. Merge both
sub-branches back to the phase branch, then do Tasks 4, 5, 7, 8, 9
sequentially on the phase branch.

### Acceptance

- Fixture-driven detector tests pass (no real API).
- End-to-end turn test passes when run with an API key.
- `get_response` / `get_latest_response` return correct bodies.
- Response store bounded behavior works per unit tests.
- Manual verification confirms response bodies match screen content
  for a real Claude session.

---

## Rollup to Model Council

Once Phase 3 has merged:

- `model-council.md` Phase 1 (model pinning) could have landed in
  parallel on its own branch — it's independent of session-management
  per `model-council.md` §7. If it hasn't, land it now.
- `model-council.md` Phase 2 (council data model + broadcast writer)
  uses `SessionManager::broadcast()` and `send_prompt()`.
- `model-council.md` Phase 3 (synthesizer) uses `ResponseComplete`
  events and `get_response()`.

No further session-management work (Phases 4–6) is required to ship
the Model Council. See `session-management-phase-4-6.md` for the MCP
server and driver-session plan.
