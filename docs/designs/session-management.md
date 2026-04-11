# Session Management — Design Spec

Status: draft
Author: @mkrueger
Date: 2026-04-11

## 1. Motivation

Today Claude Commander is a TUI that manages N independent Claude Code
PTY sessions. Each session is a silo: output lives in a vt100 buffer,
input flows only from the focused session's keystrokes, and anything
that wants to observe or drive a session has to go through the main
`App` loop directly.

Two upcoming features — the Model Council (`docs/model-council.md`) and
a "driver session" that can manage other sessions on its own — both need
the same set of primitives:

1. A **structured event bus** that broadcasts per-session events
   (`ResponseComplete`, `PromptPending`, `Spawned`, `Exited`) to multiple
   subscribers, not just the single `App` consumer.
2. A **programmatic write path** — `SessionManager::broadcast()` and
   per-session prompt injection that isn't tied to keystroke handling.
3. A **response boundary detector** that can tell when a session has
   finished producing output and expose the delta as a response body.
4. An **external interface** so one Claude session can observe and drive
   other Commander sessions directly, without the user being the courier.

This spec defines those primitives as a standalone layer. The Model
Council and driver-session features build on top of it.

**Relation to other specs:**

- `docs/model-council.md` consumes §§2–4 of this doc (event bus,
  broadcast write, response extraction) and adds a fan-out/synthesize
  shape on top.
- `docs/designs/stats-panel.md` will benefit from §2 (event bus) as a
  cleaner source of per-session metadata updates, though it doesn't
  strictly require it.

## 2. Internal event bus

Today, `Event::PtyOutput` and friends flow into `App` through a single
channel consumed by the main loop. For multiple subscribers (a driver
session's MCP subscription, the Council controller, the stats panel) we
need a fan-out.

**Shape.** A small pub/sub bus the main loop pushes every
`SessionEvent` onto, with per-subscriber receivers. Keep it sync:
`crossbeam::channel` per subscriber, or a `Vec<Sender<SessionEvent>>`
hanging off `App`. No tokio here — per §5, async stays confined to the
MCP thread.

**Event taxonomy.** Structured, not raw PTY bytes:

- `Spawned { session_id, label, model }`
- `PromptSubmitted { session_id, turn_id }`
- `ResponseComplete { session_id, turn_id }`
- `PromptPending { session_id, kind }` (Allow once / Y-n / etc.)
- `Exited { session_id, status }`
- `StatusChanged { session_id, status }`

Raw `PtyOutput` is *not* on the bus — it stays on the existing internal
path. The bus is for high-level state transitions.

**Marker-only events, pull-on-demand bodies.** `PromptSubmitted` and
`ResponseComplete` carry a `turn_id`, not a prompt/response body.
`turn_id` is a per-session monotonic counter incremented when a prompt
is submitted; the detector emits `ResponseComplete` with the same
`turn_id` when it finds the corresponding response boundary. Prompts
and responses therefore pair naturally through a shared counter.

Bodies live in a bounded per-session store (§4) and are fetched via
`SessionManager::get_response(session_id, turn_id)` /
`get_latest_response(session_id)`. Rationale:

1. **No Phase 4 rebuild when the MCP server lands.** An MCP subscriber
   (a driver session) sees transitions without seeing content. If a
   driver legitimately needs the body, it makes a separate tool call
   (`read_response`) which can be policy-gated independently from
   `subscribe` — two distinct permissions instead of one blanket.
2. **Size.** Large pastes (code, long instructions) aren't copied into
   every subscriber channel.
3. **Additive, not destructive.** If a future consumer (transcript
   viewer, audit log) wants bodies, adding `get_prompt` or wider
   accessors is additive. Moving *from* body-on-event *to*
   marker-only is the rebuild — this direction isn't.
4. **Late subscribers.** A subscriber that joined after an event fired
   can still recover state via `get_latest_response(session_id)`
   without replaying the bus.

**Migration.** Existing consumers (usage panel, UI refreshes) can
migrate to the bus opportunistically — not required for v1. Phase 1
just adds the bus alongside the current event path.

## 3. Programmatic write path

Today the only entry for writing bytes to a session is
`App::handle_session_view_key()` (`src/app.rs:669`) calling
`Session::try_write()` (`src/session/types.rs:151`) — i.e. the focused
session from a keystroke.

Add:

- `SessionManager::broadcast(ids: &[SessionId], bytes: &[u8])` — loops
  `try_write` over a set of sessions. Used by the Council broadcast
  dispatch (model-council §4.3) and by any caller that wants to fan a
  prompt out.
- `SessionManager::send_prompt(id: SessionId, text: &str) -> TurnId` —
  writes a prompt-shaped payload (text + submit newline sequence)
  without the caller needing to know Claude Code's exact input
  encoding. Increments the session's `turn_id` counter, emits
  `PromptSubmitted { session_id, turn_id }` on the bus (§2), and
  returns the new `turn_id` to the caller so they can correlate it
  with the matching `ResponseComplete` later. One place to centralize
  "how do you submit a prompt to Claude Code."

Both are callable from the main loop *or* from a tool handler running
on the MCP thread (via shared state in §5).

## 4. Response boundary detection

The hardest part of everything downstream: Claude Code's PTY output is
a terminal stream, not structured data. To emit `ResponseComplete` on
the bus we need to recognize when a session has transitioned from
"working" to "idle" with new content since the last prompt.

**Approach.** Extend `PromptDetector` (`src/pty/detector.rs`) — or add
a sibling `ResponseBoundaryDetector` — that tracks:

- A "last prompt submitted at line N" marker per session, keyed by the
  `turn_id` emitted from `send_prompt()` (§3).
- The idle-prompt pattern already matched by `PromptDetector`.
- The delta between the two idle markers (normalized for ANSI) as the
  response body.

On boundary detection, the detector writes the response body into a
**bounded per-session response store** and emits
`ResponseComplete { session_id, turn_id }` on the bus.

**Response store.** Owned by `Session` (or hung next to the vt100
parser), a small structure holding recent turns:

```rust
struct StoredTurn {
    turn_id: TurnId,
    started_at: Instant,
    completed_at: Option<Instant>,
    body: String, // ANSI-normalized response body
}
```

**Bounding rule.** Cap by total bytes per session (suggest 256 KB)
with a **minimum retention floor** of the last 3 turns regardless of
size. This gracefully handles "one enormous response consumes the
whole budget" — you always have at least the last few turns, even if
one of them is 500 KB on its own. Prompts, if stored later, use the
same pattern with their own budget.

**Accessors** (on `SessionManager`, usable from the main loop and
from MCP tool handlers via shared state):

- `get_response(session_id, turn_id) -> Option<StoredTurn>` — exact
  turn lookup.
- `get_latest_response(session_id) -> Option<StoredTurn>` — convenience
  for late-subscribing consumers ("I subscribed after the event fired,
  give me the most recent completed turn").
- A future `get_prompt(session_id, turn_id)` is an additive change
  once prompt bodies also land in the store. Not in v1.

**Why interactive PTY scraping and not `claude -p`.** Same reasoning as
model-council §4.4, repeated here because this is the spec that owns
the detector:

1. **Generality across runners.** The long-term goal is for a managed
   session to be any interactive CLI agent — Claude Code today, but
   potentially Gemini CLI, OpenCode, Aider, etc. `claude -p` is a
   Claude-Code-specific escape hatch; interactive PTY scraping is the
   lowest common denominator that works for any agent with a REPL.
2. **Follow-ups.** A live interactive session lets the user (or a
   driver session) submit a follow-up without re-spawning and losing
   context. `-p` is one-shot and throws that context away.

Accept that extraction will sometimes be fuzzy around tool-use chatter
and streaming markdown. Invest in `ResponseBoundaryDetector`
accordingly.

## 5. External interface: bundled MCP server

Claude Commander ships its own MCP server so driver sessions (and
future tooling) can observe and drive other sessions programmatically.

**Implementation:** [`rmcp`](https://crates.io/crates/rmcp) (the
official `modelcontextprotocol/rust-sdk` crate, v1.4 as of 2026-04-10)
with the `transport-streamable-http-server` feature. The server binds
loopback `127.0.0.1:<port>` and each spawned Claude child is configured
— via `.mcp.json` or `claude mcp add --transport http commander
http://127.0.0.1:<port>/mcp` — to connect to it. Loopback HTTP is
chosen over stdio (stdio is 1:1, can't serve multiple children from
one shared in-process server) and over a unix socket (possible via
`transport-async-rw`, but requires a hand-rolled accept loop for no
meaningful gain). Claude Code natively supports HTTP MCP, so this is
fully compatible.

**Runtime isolation.** `rmcp` is tokio-locked (hard dep on `tokio ^1`,
axum/hyper under the HTTP transport). To keep the rest of the codebase
sync, the MCP server runs on a **dedicated OS thread** that owns a
`tokio::runtime::Runtime` and `block_on`s the rmcp server. Commander's
sync `App` / `SessionManager` communicates with tool handlers via
`crossbeam` channels and `Arc<Mutex<…>>` over shared state. The tokio
contagion is confined to that one thread plus whatever workers tokio
spawns for itself; no other module imports `tokio`.

Per `TECH_ANALYSIS.md:186`, sync-thread-per-PTY is fine up to ~10
sessions, and adding a driver adds one more session, not a new scaling
axis. The dedicated MCP thread does not change that calculus.

**Tools offered** (opt-in via spawn flag / session role):

- `list_sessions()` — id, label, model, role, status, last activity.
- `spawn_session(label, model?, cwd?, args?)` — returns new session id.
  Subject to safety policy (§6).
- `send_prompt(session_id, text)` — writes to the target session's PTY
  via `SessionManager::send_prompt()` from §3.
- `read_response(session_id, turn_id?)` — returns a stored turn from
  the response store (§4). With `turn_id`, fetches that specific turn
  via `get_response`. Without, fetches the latest completed turn via
  `get_latest_response`. If the target is still producing the
  requested turn, blocks (or long-polls) on the bus until the
  matching `ResponseComplete` fires, with a timeout.
- `kill_session(session_id)` — tear down a child.
- `subscribe(session_id, events)` — stream events (response_complete,
  prompt_pending, exited) over the MCP channel, backed by a bus
  subscriber from §2.

The server runs in-process inside Commander (no extra daemon), so tool
handlers have direct access to `SessionManager` and the event bus.

## 6. Driver sessions & safety policy

A **driver session** is a regular Claude Code session spawned with the
`driver` role — that is, with the Commander MCP server wired into its
`.mcp.json` so it can call the §5 tools. The user hands off a chunk of
work ("coordinate these three refactors") to a driver, and it manages
its fleet directly.

Goal: reasonable guardrails, not nagging. The point of a driver session
is that it can manage its fleet itself — every confirmation defeats
that.

v1 policy:

- **Spawn.** Gated by a `driver.spawn_policy` setting:
  - `ask` — prompt the user in the TUI for each `spawn_session` call.
    Safe default for a new driver.
  - `budget` — user pre-authorizes up to N children for this driver
    run; spawns inside the budget are silent, spawns over it prompt.
    Budget resets when the driver exits. **Recommended default.**
  - `trust` — silent; only available behind an explicit opt-in per
    driver run.
- **Send prompt.** Always silent. A driver that can spawn a session can
  already type into it, so gating prompts is theater.
- **Kill.** Always prompts unless the killed session was spawned by
  this same driver in this run. Prevents a driver from nuking
  user-owned work.
- **Scope.** Driver tools only see sessions spawned by the same driver
  *or* explicitly attached by the user via an "attach to driver"
  action. A driver cannot enumerate or touch unrelated sessions by
  default.
- **Nesting.** Drivers can only spawn non-driver children. Nesting
  depth capped at 1 for v1. Revisit if a legitimate use case shows up.
- **Visible status.** Driver sessions render with a distinct marker in
  the session list, and any child spawned by a driver shows its parent
  driver id. Makes it obvious at a glance what the fleet looks like.

## 7. Open questions

**Resolved.** *Should `PromptSubmitted` / `ResponseComplete` carry
bodies?* No — marker-only with `turn_id`, bodies pulled on demand
from the §4 store. Reasoning: no Phase 4 rebuild when the MCP server
lands (subscribers see transitions, not content; body access becomes
a separately-policyable tool call), symmetric with the response
store, size-friendly for large pastes, and additive-only if future
consumers need bodies. See §2 for the full rationale.

**Resolved.** *Phase ordering: detector or write path first?* Write
path first (Phase 2 → Phase 3). The detector has a real signal to
develop against once `broadcast` / `send_prompt` exist; otherwise
detector development depends on hand-typed prompts in the TUI.

1. **rmcp smoke tests.** Before coding: verify (a) rmcp 1.4's
   streamable HTTP session resumption behaves the way Claude Code's
   client expects, and (b) the exact `#[tool]` attribute syntax at 1.4
   — mirror an example from `rust-sdk/examples/servers`.
2. **Idempotency / retries.** If a driver's `send_prompt` +
   `read_response` round-trip times out, what does it see? Probably a
   `timeout` error plus a "response so far" snapshot; don't retry
   automatically.
3. **Budget default.** What's a sensible default child-budget? 3 feels
   right for "manage a council-sized fleet"; revisit with real usage.
4. **Bus backpressure.** If a slow MCP subscriber falls behind, do we
   drop events, block the main loop, or grow the queue? Probably drop
   with a counter the subscriber can read.

## 8. Phased plan

- **Phase 1: event bus + `SessionEvent` taxonomy.** No driver behavior
  yet; new bus runs alongside the current event path. Marker-only
  events (no prompt/response bodies). Existing consumers can migrate
  opportunistically.
- **Phase 2: programmatic write path.** `SessionManager::broadcast()`
  and `send_prompt()`. `send_prompt` increments `turn_id` and emits
  `PromptSubmitted` on the bus. Usable immediately by the Council
  broadcast dispatch (model-council §4.3). Lands before the detector
  so Phase 3 has a real signal to develop against (script broadcasts,
  watch what happens) instead of hand-typing prompts into the TUI.
- **Phase 3: response boundary detector + response store.** Detector
  watches idle-prompt transitions keyed by `turn_id`, writes response
  bodies into the bounded per-session store (§4), and emits
  `ResponseComplete` on the bus. Adds `get_response` /
  `get_latest_response` on `SessionManager`. Usable immediately by the
  Council synthesizer (model-council §4.4).
- **Phase 4: in-process MCP server, read-only tools.** `list_sessions`,
  `read_response`, `subscribe`. Lets a driver observe the fleet. This
  is the first phase that adds tokio to the dep graph (dedicated-
  thread pattern from §5).
- **Phase 5: MCP write tools.** `send_prompt`, `kill_session` behind
  the §6 safety policy, minus spawn.
- **Phase 6: driver role + `spawn_session`.** `driver.spawn_policy`
  with `budget` as the default, plus UI markers for drivers and their
  children.

Phases 1–3 unblock the Model Council spec (see its §7 phased plan).
Phases 4–6 deliver driver sessions and are independent of the Council
feature landing.

## References

- `docs/model-council.md` — consumer spec.
- `docs/designs/stats-panel.md` — another downstream consumer of the
  event bus.
- `TECH_ANALYSIS.md` — ambient architecture notes, especially line 186
  on the sync-thread-per-PTY decision.
- [`rmcp` on crates.io](https://crates.io/crates/rmcp)
- [`modelcontextprotocol/rust-sdk`](https://github.com/modelcontextprotocol/rust-sdk)
- [Claude Code MCP docs](https://code.claude.com/docs/en/mcp)
