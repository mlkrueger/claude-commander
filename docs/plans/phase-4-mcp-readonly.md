# Phase 4 — In-Process MCP Server, Read-Only Tools

**Branch:** `session-mgmt/phase-4-mcp-readonly`
**Depends on:** Phase 3.5 (merged), rmcp spike (merged)
**Blocks:** Phases 5 and 6
**Design refs:**
- `docs/designs/session-management.md` §5 (MCP tool surface)
- `docs/plans/session-management-phase-4-6.md` §Phase 4 (master plan)
- `docs/plans/notes/rmcp-spike.md` (empirically verified API & gotchas)

## Context

Ship an embedded `rmcp 1.4` HTTP MCP server on loopback that Claude Code child sessions can connect to. **Read-only** tools only: `list_sessions`, `read_response`, `subscribe`. No writes, no spawning — those come in Phases 5 and 6. Phase 4 establishes the scaffolding (dedicated-thread tokio runtime, shared-state adapter, per-session handler factory, `.mcp.json` injection) that later phases plug into.

The spike established:
- `rmcp 1.4.0` works end-to-end with the `#[tool_router]` / `#[tool]` / `#[tool_handler]` macro trio
- `LocalSessionManager` keeps per-session handler state alive across TCP reconnects (the key property for `subscribe`)
- Server binds to `127.0.0.1:0`, reports assigned port via `TcpListener::local_addr()`
- Graceful shutdown needs both `axum::serve(...).with_graceful_shutdown(fut)` AND `CancellationToken::cancel()`

## Architecture

```
App (main thread)
 │
 ├── SessionManager  ◄── existing
 ├── Arc<EventBus>   ◄── existing (Phase 1)
 │
 └── McpServer       ◄── NEW
       │
       ├── ReadOnlyCtx (Arc<…>, passed to handler factory)
       │     ├── sessions: Arc<Mutex<SessionManager>>   (existing, not new)
       │     └── bus: Arc<EventBus>                     (existing)
       │
       └── std::thread "ccom-mcp"
             │
             └── tokio::runtime::Builder::new_current_thread()
                   │
                   └── rt.block_on(axum::serve(listener, rmcp_router)
                                    .with_graceful_shutdown(...))
                         │
                         └── per MCP session: Ccom { ctx: Arc<ReadOnlyCtx> }
                               ├── #[tool] list_sessions
                               ├── #[tool] read_response
                               └── #[tool] subscribe
```

**Key principle:** the main TUI thread never awaits on anything. Tokio lives inside the dedicated `ccom-mcp` thread. Cross-thread communication uses `std::sync::mpsc` + `Arc<Mutex<...>>` — no `tokio::sync::Mutex` reaching outside the mcp thread.

## Task dependency graph

```
Task 0 (spike) ─── done
     │
Task 1 (deps)
     │
Task 2 (scaffolding) ─── module dirs, empty structs
     │
Task 3 (runtime)     ─── thread + tokio + axum + shutdown
     │
Task 4 (state)       ─── ReadOnlyCtx + SessionSummary snapshot type
     │
     ├─── Task 5a (list_sessions)   ┐
     ├─── Task 5b (read_response)   ├─── PARALLEL — 3 subagents
     └─── Task 5c (subscribe)       ┘
     │
Task 6 (.mcp.json injection)
Task 7 (loopback binding assert)
Task 8 (integration test)
Task 9 (real-Claude verify)
```

Sequential phases 1–4 happen in the main worktree. Task 5 fans out into three parallel subagents working against the same branch — handlers are disjoint (separate `#[tool]` methods in one impl block, each in its own git commit, no code collisions expected beyond shared imports). Tasks 6–9 re-converge.

## Task breakdown

### Task 1 — Dep addition (~15 min)

**File:** `Cargo.toml`

```toml
rmcp = { version = "=1.4.0", features = [
    "server",
    "macros",
    "transport-streamable-http-server",
] }
axum = "0.7"
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros", "time"] }
tokio-util = "0.7"
schemars = "0.8"
```

Pin `=1.4.0` exactly — the spike hit a one-day-old release, and the macro surface is new enough that a 1.4.1 patch could shift things. The pin will get relaxed in a future maintenance pass.

Note in the PR description: **first phase that introduces tokio to the dep graph.** The dedicated-thread runtime pattern (Task 3) keeps tokio out of the main TUI thread.

**Verify:** `cargo build` compiles, `cargo test` passes.

### Task 2 — Module scaffolding (~30 min)

**New files:**

```
src/mcp/
├── mod.rs        — pub use McpServer; pub(crate) use ReadOnlyCtx;
├── server.rs     — McpServer struct + start/stop lifecycle
├── state.rs      — ReadOnlyCtx, SessionSummary snapshot struct
└── handlers.rs   — Ccom struct + #[tool_router] impl block (tools added in Task 5)
```

Register `mod mcp;` in `src/main.rs` and `src/lib.rs`.

Empty-but-compiling scaffolding so Task 3 has somewhere to land. `McpServer::start()` returns `anyhow::Result<Self>`, `McpServer::stop(self)` is a no-op at this stage.

**Verify:** `cargo build`, `cargo clippy`.

### Task 3 — Dedicated-thread runtime (~1.5 hours)

**File:** `src/mcp/server.rs`

Implement the thread + runtime + axum + rmcp plumbing from the spike, but wired into a real `McpServer` struct:

```rust
pub struct McpServer {
    port: u16,
    shutdown: tokio::sync::oneshot::Sender<()>, // consumed by stop()
    thread: Option<JoinHandle<()>>,
    cancel: CancellationToken,                   // for in-flight session teardown
}

impl McpServer {
    pub fn start(ctx: Arc<ReadOnlyCtx>) -> anyhow::Result<Self> {
        let (port_tx, port_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let cancel_for_thread = cancel.clone();
        let thread = std::thread::Builder::new()
            .name("ccom-mcp".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build tokio rt");
                rt.block_on(run_server(ctx, port_tx, shutdown_rx, cancel_for_thread));
            })?;
        let port = port_rx
            .recv_timeout(Duration::from_secs(2))?;
        Ok(Self { port, shutdown: shutdown_tx, thread: Some(thread), cancel })
    }

    pub fn port(&self) -> u16 { self.port }

    pub fn stop(mut self) {
        let _ = self.shutdown.send(()); // axum graceful shutdown
        self.cancel.cancel();            // tear down in-flight SSE
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

async fn run_server(
    ctx: Arc<ReadOnlyCtx>,
    port_tx: std::sync::mpsc::SyncSender<u16>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    cancel: CancellationToken,
) {
    let service = StreamableHttpService::new(
        {
            let ctx = Arc::clone(&ctx);
            move || Ok(Ccom::new(Arc::clone(&ctx)))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => {
            log::error!("mcp listener bind failed: {e}");
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    if port_tx.send(port).is_err() {
        return;
    }
    let cancel_for_shutdown = cancel.clone();
    let _ = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
            cancel_for_shutdown.cancel();
        })
        .await;
}
```

**App integration:** `App::new` calls `McpServer::start(ReadOnlyCtx::from(&app))` and stores the server. `main.rs` shutdown path calls `app.mcp.take().map(McpServer::stop)` before the existing `session.kill` loop.

**Gotchas from the spike:**
- `tool_router` field requires `#[allow(dead_code)]`.
- `ServerInfo` is `#[non_exhaustive]` — mutate a `::default()` in place.
- `StreamableHttpServerConfig::default()` is what we want — don't flip `stateful_mode` or `json_response`.

**Verify:** `cargo test`. New test: `McpServer::start` succeeds, `server.port() > 0`, `server.stop()` joins cleanly within 2s.

### Task 4 — Shared state adapter (~1 hour)

**File:** `src/mcp/state.rs`

```rust
pub struct ReadOnlyCtx {
    pub sessions: Arc<Mutex<SessionManager>>,
    pub bus: Arc<EventBus>,
}

#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    pub id: usize,
    pub label: String,
    pub working_dir: String,
    pub status: String,        // Exited(code) stringified
    pub last_activity_secs: u64,
    pub context_percent: Option<f64>,
}

impl ReadOnlyCtx {
    pub fn list_sessions(&self) -> Vec<SessionSummary> { /* snapshot */ }
    pub fn get_response(&self, id: usize, turn_id: Option<TurnId>) -> Option<StoredTurn> { /* … */ }
}
```

**Critical constraint:** handlers must never hold `sessions.lock()` across `.await` points. Snapshot into owned data (`Vec<SessionSummary>`, cloned `StoredTurn`), release the lock, then return from the tool method.

**App integration:** `App` currently owns `SessionManager` by value. Phase 4 wraps it in `Arc<Mutex<SessionManager>>`. Every existing caller (dozens of sites across `app/mod.rs`, `app/keys.rs`, `app/render.rs`) needs a `.lock()` wrapper. This is the **riskiest refactor** in Phase 4 — touch count is high, but each change is mechanical.

**Alternative to consider:** add an intermediate `SessionsHandle` type that wraps the `Arc<Mutex<>>` and exposes the same method names as `SessionManager`, so call sites don't see the locking. Decide during implementation — if the mechanical change is under ~40 sites, do it inline; if it's 100+, introduce the handle.

**Verify:** `cargo test` — all existing tests still pass after the `Arc<Mutex<>>` wrap. One new test: `ReadOnlyCtx::list_sessions` returns a snapshot matching a fixture-populated `SessionManager`.

### Task 5 — Tool handlers (parallel, ~1 hour each)

**File:** `src/mcp/handlers.rs`

All three tools live in the same `#[tool_router] impl Ccom` block. Each subagent adds one `#[tool]` method plus its unit test. The three sub-PRs merge into the phase branch sequentially (they touch the same file but different methods — trivially rebaseable).

Coordination: stub all three tool signatures before launching the subagents so each can compile against the others' presence.

#### 5a — `list_sessions`

```rust
#[tool(description = "List all sessions Commander is currently managing")]
async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
    let summaries = self.ctx.list_sessions();
    let json = serde_json::to_string(&summaries)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
```

Returns JSON-serialized `Vec<SessionSummary>` as a text content block. rmcp's auto-schema handles the output type documentation.

**Tests:**
- `list_sessions_empty` — no sessions → `"[]"`
- `list_sessions_populated` — 2 dummy sessions → both summaries present

#### 5b — `read_response`

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ReadResponseArgs {
    session_id: usize,
    turn_id: Option<u64>,
    /// Long-poll timeout in seconds. Default 60, max 300.
    timeout_secs: Option<u64>,
}

#[tool(description = "Read the body of a completed turn for a session. \
                      If the turn is still in flight, long-polls until \
                      ResponseComplete fires or timeout expires.")]
async fn read_response(
    &self,
    Parameters(args): Parameters<ReadResponseArgs>,
) -> Result<CallToolResult, McpError> {
    // Fast path: turn already stored
    if let Some(stored) = self.ctx.get_response(args.session_id, args.turn_id.map(TurnId::new)) {
        return Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string(&StoredTurnWire::from(&stored)).unwrap()
        )]));
    }
    // Long-poll path: subscribe to bus, wait for matching ResponseComplete
    let rx = self.ctx.bus.subscribe();
    let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(60).min(300));
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        // Use tokio::task::spawn_blocking for the std::sync::mpsc::Receiver,
        // OR poll in a loop with tokio::time::sleep. Decide during impl.
        ...
    }
    Err(McpError::internal_error("timeout waiting for response", None))
}
```

Long-poll implementation is the interesting part — `EventBus::subscribe` returns a `std::sync::mpsc::Receiver`, which doesn't play nicely with async. Options:
1. `tokio::task::spawn_blocking` wrapping `recv_timeout`
2. Poll via `rx.try_recv()` + `tokio::time::sleep(10ms)`
3. Adapt `EventBus::subscribe` to return a `tokio::sync::broadcast` channel (bigger change, Phase 4+ only)

**Recommendation:** option 2 for Phase 4 (simple, good enough for the expected low-volume read_response traffic). Revisit in Phase 5 if profiling shows the poll loop is hot.

**Tests:**
- `read_response_fast_path` — turn already stored, returns immediately
- `read_response_long_poll` — turn arrives mid-wait via a background `SessionManager::check_hook_signals` simulation
- `read_response_timeout` — turn never arrives, returns error after timeout (use a short 500ms timeout in the test)

#### 5c — `subscribe`

```rust
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SubscribeArgs {
    session_ids: Option<Vec<usize>>,
    events: Option<Vec<String>>, // event type names
}

#[tool(description = "Stream SessionEvents filtered by session id and event type. \
                      The stream stays open for the lifetime of the MCP session.")]
async fn subscribe(
    &self,
    Parameters(args): Parameters<SubscribeArgs>,
) -> Result<CallToolResult, McpError> {
    // This is NOT actually a streaming tool in the rmcp sense — MCP tool
    // results are one-shot. Real streaming happens via MCP's notifications
    // mechanism. For Phase 4, this tool's implementation needs to spawn a
    // task that forwards bus events into rmcp's notification channel.
    //
    // OPEN QUESTION: how does rmcp 1.4 expose server-initiated notifications
    // to the client from inside a tool handler? Investigation required
    // BEFORE implementing. May need RequestContext<RoleServer> access to
    // send notifications out-of-band.
    todo!("Task 5c blocker — investigate rmcp notification API")
}
```

**⚠️ Subscribe is the hardest tool.** MCP tool calls are synchronous request/response. True streaming (one event per bus publish) requires MCP's server-to-client notification channel, which rmcp exposes via `RequestContext<RoleServer>`. The spike did NOT verify this path.

**Decision point before Task 5c starts:** investigate rmcp's notification surface for ~30 min. If it's straightforward, implement streaming. If not, degrade to a **one-shot** `subscribe` that returns the N events accumulated since the last call (caller polls repeatedly) — document the degradation in the tool description.

**Tests:**
- Depends on the implementation chosen. At minimum: basic filter matching on `session_ids` + `events`.

### Task 6 — Auto-generated `.mcp.json` (~45 min)

**Files:** `src/session/types.rs` or new `src/session/mcp_config.rs`

When `Session::spawn` runs (Claude sessions only — same `install_hook` gate from Phase 3.5), write a `.mcp.json` file to the hook dir (reuse the Phase 3.5 symlinked `.claude/` temp dir structure) that points at the live `McpServer` port.

Exact schema: Claude Code's `.mcp.json` format is:
```json
{
  "mcpServers": {
    "ccom": {
      "type": "http",
      "url": "http://127.0.0.1:54321/mcp"
    }
  }
}
```

**Verification required before implementation:** confirm the exact schema with the current Claude Code docs. The rmcp spike didn't exercise this — Phase 4 needs a quick 10-minute side verification.

**Config threading:** `SpawnConfig` grows an `mcp_port: Option<u16>` field. `App::spawn_session_kind` populates it from `self.mcp.port()`.

**Verify:** manual — spawn a session, inspect the generated `.mcp.json`, confirm it parses correctly and Claude Code picks up the server.

### Task 7 — Loopback binding assertion (~15 min)

**File:** `src/mcp/server.rs`

Hard-code `127.0.0.1` as the bind address (already done in the spike). Add a `debug_assert!` that the configured address is `Ipv4Addr::LOCALHOST` or `Ipv6Addr::LOCALHOST`. Also add a module-level doc comment flagging the lint.

In the test, assert the bound address's `is_loopback()`.

**Verify:** `cargo test`.

### Task 8 — Integration test (~1 hour)

**File:** `tests/mcp_readonly.rs`

Spin up a real `McpServer` with a dummy `ReadOnlyCtx` (populated with 2 dummy sessions via the Phase 2 test seam), use `rmcp`'s client features to:

1. `initialize` against the server
2. `tools/list` — assert 3 tools present with correct schemas
3. `tools/call list_sessions` — assert 2 summaries returned
4. `tools/call read_response` with an unknown turn — assert timeout error
5. `tools/call read_response` with a stored turn — assert fast path returns body
6. server.stop() — assert thread joins within 2s

Use `rmcp`'s client-side features (`transport-streamable-http-client`) to drive the server end-to-end. This is the first time we exercise both sides of the rmcp API in ccom — verify the client feature set compiles in Cargo.toml.

**Verify:** `cargo test --test mcp_readonly`.

### Task 9 — Real-Claude verification (~30 min, manual)

Launch `cargo run` with a Claude session. Ask the session:

> "Call the `list_sessions` MCP tool and tell me what you see."

Expected: the session sees itself in the returned list. This is the end-to-end proof that (a) `.mcp.json` injection works, (b) Claude Code connects to the embedded server, (c) the tool handler returns correctly.

**Not automated.** Findings recorded in the PR description.

## Risks

1. **`Arc<Mutex<SessionManager>>` refactor (Task 4) is high-touch.** Every existing call site in `app/` needs wrapping. Risk: a missed `.lock()` causes a compile error, not a runtime bug — mechanical fix. Worst case: the refactor takes longer than estimated.
2. **`subscribe` tool streaming (Task 5c) is the one unverified path.** The spike didn't exercise MCP notifications. May need to fall back to a poll-based one-shot — acceptable degradation for Phase 4, revisit in follow-up.
3. **rmcp 1.4.0 is one day old.** Pinning `=1.4.0` keeps us stable but blocks security patches until we manually update. Track via dependabot or a calendar check.
4. **Tokio in the dep graph.** Introduces a runtime + async machinery the rest of the codebase doesn't use. Keeping it contained in `src/mcp/` + the dedicated thread is a load-bearing boundary — any future code that pulls `tokio::sync::Mutex` or `async fn` out of `mcp/` breaks the contract.
5. **`.mcp.json` schema drift** (Task 6) — Claude Code's config format could change. Verify before implementing.

## Parallelism plan

### Sequential phase (one main checkout, this branch):
- Tasks 1 → 2 → 3 → 4 — scaffolding and state adapter. Each commits on top of the previous. ~4 hours total.

### Parallel phase (3 subagents, same branch):
- Task 5a (`list_sessions`) — subagent A
- Task 5b (`read_response`) — subagent B
- Task 5c (`subscribe`) — subagent C (starts with 30-min investigation, then implements or degrades)

Each subagent touches only its own `#[tool]` method in `handlers.rs` + tests. They commit against the same branch, rebased if needed. Coordination contract: Task 4 stubs all three method signatures in `handlers.rs` before launching.

### Sequential finish (one main checkout):
- Tasks 6 → 7 → 8 → 9 — polish, binding assert, integration test, manual verification.

**Estimated total:** 8–10 hours of focused work.

## Verification

- `cargo test` — all existing 296 tests plus new Phase 4 tests pass
- `cargo clippy` — zero warnings (as established)
- `cargo fmt --check` — clean
- Integration test (`tests/mcp_readonly.rs`) passes
- Manual real-Claude verification: spawn Claude session, have it call `list_sessions`, confirm it sees itself in the response

## Acceptance criteria

From the existing master plan:
- rmcp spike findings documented — ✅ done
- New deps in `Cargo.toml`: `rmcp` 1.4 + `tokio` 1
- Real Claude Code session connects to the embedded server and successfully calls all three read-only tools
- Server binds loopback only; verified in integration test and at runtime
- All existing tests still pass
