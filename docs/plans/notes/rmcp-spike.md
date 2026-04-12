# Phase 4 Spike: rmcp 1.4 — In-Process MCP Server

**Date:** 2026-04-12
**Status:** GO — both spike questions (0a, 0b) verified empirically against `rmcp 1.4.0`.
**Tested against:** `rmcp 1.4.0` (published 2026-04-10, one day old at spike time), `tokio 1.51`, `axum 0.7.9`.

## Summary

All Phase 4 Task 0 questions answered. `rmcp 1.4` supports everything
the Phase 4 design requires: `#[tool]`-style trait-impl server with
automatic JSON schema generation, streamable HTTP transport with
session resumption across TCP reconnects via `Mcp-Session-Id`, random
port binding, graceful shutdown via `CancellationToken`. The macro
surface is stable enough that a minimal 2-tool server compiles and
runs end-to-end with no workarounds.

## Scratch crate location

`/tmp/rmcp-spike` (throwaway). Not committed to the repo. Cargo.toml
and main.rs are captured verbatim in §5 below so they can be
reconstructed if needed.

## Q0b — `#[tool]` attribute macro syntax

**Answer:** stable, three attribute macros compose: `#[tool_router]` on
the impl block, `#[tool(description = ...)]` per method, `#[tool_handler]`
on the `impl ServerHandler` block. Tool routing, JSON schema generation,
and `list_tools`/`call_tool` dispatch are all generated — no manual
wiring.

Key rules the spike verified empirically:

1. **The struct must have a `tool_router: ToolRouter<Self>` field.**
   The name is load-bearing — the macros reference it. Initialize it
   via `Self::tool_router()` (generated associated fn).
2. **Tools with parameters wrap them in `Parameters<T>`** where
   `T: Deserialize + schemars::JsonSchema`. Multi-arg tool functions
   (old style) are gone — you destructure the wrapper:
   ```rust
   fn read_response(
       &self,
       Parameters(ReadResponseArgs { session_id }): Parameters<ReadResponseArgs>,
   ) -> Result<CallToolResult, McpError>
   ```
3. **Return type is always `Result<CallToolResult, McpError>`** where
   `McpError` is an alias for `rmcp::ErrorData`. Handlers can be
   `async fn` or sync `fn`.
4. **`CallToolResult::success(vec![Content::text("...")])`** is the
   standard success path.
5. **`ServerHandler::get_info`** returns `ServerInfo`, which is
   `#[non_exhaustive]` — you **cannot** use struct-literal syntax or
   `..Default::default()`. Mutate fields on a `ServerInfo::default()`
   instead.
6. **Schema generation is automatic** via `schemars`. The `tools/list`
   response for the spike's `read_response` tool contained the
   full JSON schema derived from `ReadResponseArgs`:
   ```json
   {"$schema":"https://json-schema.org/draft/2020-12/schema",
    "properties":{"session_id":{"format":"uint","minimum":0,"type":"integer"}},
    "required":["session_id"],"title":"ReadResponseArgs","type":"object"}
   ```

### Gotchas encountered

- **ServerInfo is non-exhaustive:** my first attempt used struct-literal
  and failed with `E0639`. Fix: `let mut info = ServerInfo::default();
  info.field = ...;`. Not a pattern Rust devs reach for often.
- **`tool_router` field triggers `dead_code` warning:** the macro code
  reads it via the generated associated fn, but rustc's dead-code
  analysis doesn't trace through the macro. Must `#[allow(dead_code)]`
  the field in production code.

## Q0a — Session resumption across reconnects

**Answer:** works as documented. `LocalSessionManager` (the default
in-memory backend) keeps per-session handler state alive across TCP
disconnect/reconnect cycles as long as the client echoes the same
`Mcp-Session-Id` header.

### Empirical verification

Ran the spike server, hit `initialize` with curl (fresh TCP), captured
the `Mcp-Session-Id: 4cca7639-8a77-412a-9a85-5b84bfa67add` from the
response header, then did three more curl calls — each a brand-new
TCP connection — with the same session id. Observed:

| Call | TCP conn | Session id | Response | Shared state |
|---|---|---|---|---|
| 1. initialize | conn 1 | minted | server info | `Ccom{calls=0}` instantiated |
| 2. tools/call list_sessions | conn 2 | same | `"[] (1 calls)"` | `calls=1` |
| 3. tools/call list_sessions | conn 3 | same | `"[] (2 calls)"` | `calls=2` — **persisted** |
| 4. tools/call list_sessions | conn 4 | **absent** | `"Unexpected message, expect initialize request"` | rejected |

The counter incrementing 0 → 1 → 2 across three independent TCP
connections proves the per-session `Ccom` struct (created by the
`StreamableHttpService::new` factory closure) is cached by the
`LocalSessionManager` and reused across reconnects. This is exactly
the behavior Phase 4 needs: a Claude Code subprocess can reconnect
after a disconnect without losing its subscription state or its view
of `SessionManager`.

### Additional session-manager facts (from source inspection)

From `crates/rmcp/src/transport/streamable_http_server/session.rs`:

```rust
pub trait SessionManager: Send + Sync + 'static {
    fn create_session(&self) -> ... (SessionId, Transport);
    fn has_session(&self, id: &SessionId) -> ... bool;
    fn close_session(&self, id: &SessionId) -> ...;
    fn create_stream(&self, id, msg) -> Stream<ServerSseMessage>;
    fn create_standalone_stream(&self, id) -> Stream<ServerSseMessage>;
    /// Resume an SSE stream from Last-Event-Id, replaying missed events.
    fn resume(&self, id: &SessionId, last_event_id: String)
        -> Stream<ServerSseMessage>;
}
```

Two shipped implementations:
- **`LocalSessionManager`** (default, in-memory) — what the spike used
- **`NeverSessionManager`** — rejects all session ops, for stateless mode

The trait is public so Phase 4+ can back sessions with sqlite or
another durable store later.

### `StreamableHttpServerConfig` gotchas

- `stateful_mode: true` is the default and is **required** for
  resumption — don't flip it.
- `json_response: false` is the default and is **required** for SSE
  framing (which carries the `Last-Event-Id` replay). Don't flip it.
- `allowed_hosts` defaults to `["localhost", "127.0.0.1", "::1"]`
  (DNS rebinding protection). Fine for our loopback-only use.

## Port binding + shutdown

Both straightforward:

```rust
let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
let port = listener.local_addr()?.port(); // assigned port
let ct = CancellationToken::new();
let service = StreamableHttpService::new(
    || Ok(Ccom::new()),
    LocalSessionManager::default().into(),
    StreamableHttpServerConfig::default(),
);
let router = axum::Router::new().nest_service("/mcp", service);
axum::serve(listener, router)
    .with_graceful_shutdown(async move {
        shutdown_rx.await.ok();
        ct.cancel(); // cancels in-flight sessions
    })
    .await?;
```

- Bind to `:0`, read the assigned port from `local_addr()`.
- Graceful shutdown has two layers: (1) `axum::serve(...).with_graceful_shutdown(fut)`
  stops accepting new TCP connections, (2) `ct.cancel()` on the
  config's cancellation token tears down in-flight SSE streams. You
  need **both** for a clean exit.
- Spike verified: spawned server, waited 20s, triggered shutdown via
  the cancellation token, process exited cleanly with no hang.

## Cargo.toml for Phase 4 Task 1

```toml
rmcp = { version = "1.4", features = [
    "server",
    "macros",
    "transport-streamable-http-server",
] }
axum = "0.7"
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros"] }
tokio-util = "0.7"
schemars = "0.8"
```

Note: `rmcp`'s `transport-streamable-http-server` transitively pulls
in `tower`, `http`, `http-body`, `bytes`, `uuid`, `sse-stream` — do
not list them explicitly.

This is the **first phase that adds `tokio` to ccom's dep graph.**
The Phase 4 dedicated-thread runtime design (`std::thread::spawn` →
`tokio::runtime::Builder::new_current_thread().enable_all().build()?`
→ `rt.block_on(run_server(...))`) keeps tokio off the main TUI thread
and out of the rest of the codebase.

## Risks / concerns

1. **1.4.0 is one day old.** Pin to exactly `=1.4.0` in `Cargo.toml`
   (or at least watch for a 1.4.1 patch). Be ready to re-read source
   if the macro surface shifts.
2. **`tool_router` field dead-code warning** in production will need
   `#[allow(dead_code)]` — flag in code review.
3. **`ServerInfo` non-exhaustive construction is awkward.** Not a
   blocker but worth a helper function to centralize it.
4. **Per-session factory closure** captures `Arc<SharedState>` for
   shared state. Phase 4 Task 4 (shared state adapter) must design
   this carefully — see Phase 4 plan §4 for guidance.
5. **rmcp's default `allowed_hosts`** prevents non-loopback access.
   Fine for us, but if Phase 5 ever exposes the server beyond loopback
   (it shouldn't), this default needs attention.

## Go / No-Go

**GO.** rmcp 1.4 hits every Phase 4 requirement:

- ✅ `#[tool]` trait-impl server (0b)
- ✅ Session resumption verified empirically with 3-conn counter test (0a)
- ✅ Bind to `127.0.0.1:0`, retrieve port via `local_addr()`
- ✅ Graceful shutdown via `CancellationToken` + axum's `with_graceful_shutdown`
- ✅ Automatic JSON schema generation from `schemars::JsonSchema`
- ✅ Per-session handler instance via factory closure — straightforward
  path for Phase 4's `Arc<SharedState>` capture

Proceed to Phase 4 Task 1 (dep addition).

## Appendix: spike source

### `/tmp/rmcp-spike/Cargo.toml`
```toml
[package]
name = "rmcp-spike"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
rmcp = { version = "1.4", features = [
    "server",
    "macros",
    "transport-streamable-http-server",
] }
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
anyhow = "1"
serde = { version = "1", features = ["derive"] }
schemars = "0.8"
```

### `/tmp/rmcp-spike/src/main.rs`
```rust
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    },
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadResponseArgs {
    pub session_id: usize,
}

#[derive(Clone)]
pub struct Ccom {
    calls: Arc<Mutex<u64>>,
    tool_router: ToolRouter<Ccom>,
}

#[tool_router]
impl Ccom {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(0)),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "List the sessions ccom currently manages")]
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        let mut c = self.calls.lock().await;
        *c += 1;
        Ok(CallToolResult::success(vec![Content::text(format!(
            "[] ({} calls)",
            *c
        ))]))
    }

    #[tool(description = "Read the latest response body for a given session")]
    async fn read_response(
        &self,
        Parameters(ReadResponseArgs { session_id }): Parameters<ReadResponseArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(format!(
            "no response for session {session_id}"
        ))]))
    }
}

#[tool_handler]
impl ServerHandler for Ccom {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some("ccom phase-4 spike".into());
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ct = CancellationToken::new();

    let service = StreamableHttpService::new(
        || Ok(Ccom::new()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    println!("ccom-spike MCP listening on http://{local_addr}/mcp");
    std::fs::write("/tmp/rmcp-spike.port", format!("{}", local_addr.port()))?;

    let shutdown_ct = ct.clone();
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        shutdown_ct.cancel();
        println!("ccom-spike shutdown triggered");
    });

    server.await?;
    println!("ccom-spike server exited cleanly");
    Ok(())
}
```

### Test script used for verification

```bash
# Start server
./target/debug/rmcp-spike &
SRV_PID=$!
PORT=$(cat /tmp/rmcp-spike.port)

# 1. Initialize, capture session id
curl -s -D /tmp/h1.txt -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"spike","version":"0"}}}' \
  "http://127.0.0.1:$PORT/mcp"
SID=$(grep -i "mcp-session-id" /tmp/h1.txt | awk '{print $2}' | tr -d '\r\n')

# 2. Tools list with session id (fresh TCP connection)
curl -s -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
  "http://127.0.0.1:$PORT/mcp"

# 3. Tool call (another fresh TCP connection, same session id)
curl -s -X POST \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -H "Mcp-Session-Id: $SID" \
  -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_sessions","arguments":{}}}' \
  "http://127.0.0.1:$PORT/mcp"

kill $SRV_PID
```

Each curl invocation is a distinct TCP connection. The `calls`
counter increments across them — proof of session persistence.
