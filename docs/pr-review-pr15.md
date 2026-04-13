# PR #15 Review — `session-mgmt phase 4: in-process MCP server, read-only tools`

**Date:** 2026-04-12
**Branch:** `session-mgmt/phase-4-mcp-readonly`
**Scope:** 17 files, +3186/-139
**Status:** All high-priority and medium items resolved. 326 tests passing, zero clippy warnings, fmt clean. Ready for merge pending the Task 9 manual real-Claude smoke test.

## Overview

This PR introduces Phase 4 of session management: an embedded `rmcp 1.4` HTTP MCP server on loopback with three read-only tools (`list_sessions`, `read_response`, `subscribe`). It's a substantial PR that touches five subsystems and adds tokio to the dep graph for the first time.

The review verdict was **"ready to merge after two small fixes"** (H2 + M3/M4). All applicable items applied.

---

## Critical path verification (confirmed correct)

- `McpServer::start` port handshake, loopback assertion, thread join — all correct
- `Arc<Mutex<SessionManager>>` contention traced: no deadlock path (bus publishes are non-blocking mpsc sends)
- `subscribe` task termination: three clean exit paths (cancel flag, channel disconnect, peer-gone `Err`), no long-lived sender leak
- `read_response` TOCTOU guard verified — `check_response_boundaries` pushes to store **before** publishing the event, so the post-subscribe recheck closes the window
- `main.rs` shutdown order verified: sessions → events → readers → MCP
- `.mcp.json` written to `$CLAUDE_CONFIG_DIR/.mcp.json` — plausibly correct, pending Task 9 real-Claude verification

## High-priority resolutions

### H1. DNS-rebinding / `allowed_hosts` defense ✅ Applied (+ investigation)

**Finding:** `StreamableHttpServerConfig::default()` may not enforce a Host header check.

**Investigation:** read `rmcp-1.4.0/src/transport/streamable_http_server/tower.rs` directly. `StreamableHttpServerConfig::default()` **already** sets `allowed_hosts: vec!["localhost", "127.0.0.1", "::1"]` — the DNS-rebinding defense is **on by default** in rmcp 1.4. The finding was a partial false positive: the defense existed, but ccom's source didn't make the security contract visible.

**Fix:** `src/mcp/server.rs` now explicitly calls `.with_allowed_hosts(vec!["localhost", "127.0.0.1", "::1"])` in `run_server`, with a comment explaining that this pins the security contract in ccom's own source so a future rmcp patch that loosens the default can't silently widen our attack surface.

### H2. `read_response` timeout returns `McpError::internal_error` ✅ Applied

**Finding:** `handlers.rs:262-270` — timeout is an expected outcome, not an internal bug. A client can't distinguish "your turn id was wrong" from "server misbehaved".

**Fix:** Mapped timeout to `CallToolResult::error(vec![Content::text(...)])` (a tool-level error result with `is_error: Some(true)`) instead of a JSON-RPC internal error envelope. The integration test already accepts either shape, so no test changes were needed. Clients can now inspect `CallToolResult::is_error` to distinguish expected timeouts from transport failures.

## Medium resolutions

| ID | Item | Status | Notes |
|---|---|---|---|
| **M1** | `port_tx.send(0)` on bind failure is a type-system hack | ✅ Applied | Port channel now carries `Result<u16, String>` — bind/local_addr/loopback-assertion failures surface as `Err` on the main thread's `start()` call instead of silently returning a port-0 server. Also covers the `tokio::runtime::Builder::build()` error path which previously hung on timeout. |
| **M2** | `tokio` features include `rt-multi-thread` we don't use | ✅ Applied | `Cargo.toml` now lists only `rt, macros, time, sync, net, io-util`. Dropped `rt-multi-thread` (we use `new_current_thread()`). Added `net` and `io-util` explicitly since they're required by `tokio::net::TcpListener` and `axum::serve`. Release build verified. |
| **M3** | `#[allow(dead_code)]` on `ReadOnlyCtx` fields is stale | ✅ Applied | Removed. Fields are read by `list_sessions`/`get_response` in the same file. |
| **M4** | `#[allow(dead_code)]` on `SessionSummary` is stale | ✅ Applied | Removed. Struct is constructed by `list_sessions` immediately below its declaration. |
| **M5** | `McpServer::start_with` needs a comment explaining integration test usage | ✅ Applied | Added doc paragraph explaining why `#[allow(dead_code)]` is justified — integration tests compile against the crate as an external consumer, so rustc's per-crate dead-code analysis doesn't see the callers. |
| **M6** | `read_response` drops the subscribed `rx` on return | ⏸️ Deferred | Not urgent per the review. In high-frequency poll scenarios this can cause bus-sender churn until the next `publish` prunes dead senders. Not an issue for Phase 4's expected traffic. |

## Test coverage gap resolutions

| ID | Item | Status |
|---|---|---|
| **T1** | `subscribe` streaming is untested end-to-end | ⏸️ Deferred to Phase 5 DoD per the review. The unit tests pin the wire shape + filter mapping; the rmcp spike confirmed the notification path; end-to-end test would require driving a real SSE stream. Noted in the review as an acceptable gap for Phase 4. |
| **T2** | `read_response` long-poll **success** path untested | ✅ Added | New test `read_response_long_poll_success_via_bus_wakeup` in `src/mcp/handlers.rs`. Spawns a real `/bin/cat` session via `SessionManager::spawn`, installs a synthetic boundary detector, sends a prompt, subscribes to the bus, feeds the marker bytes, drains the bus to observe `ResponseComplete`, then verifies the `ctx.get_response` recheck returns the stored turn with the expected body. This exercises the exact subscribe → recheck → refetch path the handler uses. |
| **T3** | Lock-contention stress test for MCP thread vs session manager | ⏸️ Deferred to Phase 5 per the review. Not needed until write tools land. |
| **T4** | Bind-failure / port-0 handoff regression test | ✅ Added | Two new tests in `src/mcp/server.rs`: `port_handoff_maps_error_to_start_failure` pins the `Result<u16, String>` channel contract by posting an `Err(..)` payload and confirming it surfaces as an `anyhow::Err` with the inner reason preserved; `port_handoff_ok_returns_port` is the companion happy-path test. These don't trigger a real `TcpListener::bind` failure (hard to do portably), but they DO pin the M1 fix's contract so a future refactor can't regress it. |

## Security findings follow-up

- **S1** = H1, resolved above.
- **S2–S4**: no action needed, confirmed safe.

## Test count delta

Pre-review-fix: 320 tests
Post-review-fix: **326 tests**

| Source | Delta |
|---|---|
| `read_response_long_poll_success_via_bus_wakeup` (T2) | +1 |
| `port_handoff_maps_error_to_start_failure` (T4) | +1 |
| `port_handoff_ok_returns_port` (T4 companion) | +1 |
| Lib + bin double-counting (same tests run under both targets) | +3 |

## Overall assessment

**Approved after fix pass.** All high-priority findings and all applicable mediums resolved. The deferred items (M6, T1, T3) are explicitly flagged for Phase 5 and don't block the current landing. Task 9 (real-Claude smoke test) remains as the only pre-merge verification, and that's expected to be a manual step.

## File:line index

- `src/mcp/server.rs:42-88` — M1 `Result<u16, String>` channel
- `src/mcp/server.rs:141-152` — H1 explicit `with_allowed_hosts`
- `src/mcp/server.rs:214-266` — T4 handoff contract tests
- `src/mcp/handlers.rs:262-274` — H2 `CallToolResult::error` mapping
- `src/mcp/handlers.rs:475-567` — T2 long-poll success path test
- `src/mcp/state.rs:24-47` — M3/M4 stale `#[allow(dead_code)]` removal
- `src/mcp/mod.rs:16-27` — M5 doc comment on `start_with`
- `Cargo.toml:31` — M2 tokio feature trim
