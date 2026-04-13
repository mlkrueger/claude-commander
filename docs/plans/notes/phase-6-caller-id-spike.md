# Phase 6 Task 0 — Caller-ID Spike Findings

**Branch:** `session-mgmt/phase-6-caller-id-spike`
**Status:** PASS ✅
**Date:** 2026-04-13

## Question

Phase 6 needs each MCP tool-call handler to know *which ccom session
is making the call*, so it can apply role-based scope filtering
(`Driver` vs `Solo`) and decide whether `spawn_session` / `kill_session`
need confirmation. The Phase 6 plan (`docs/plans/phase-6-driver-role.md`
§Caller identification) proposes writing a custom HTTP header
(`X-Ccom-Caller: <session_id>`) into the spawned session's `.mcp.json`
and reading it back inside the `#[tool]` handler via rmcp's
`RequestContext::extensions`.

Phase 4's spike proved rmcp 1.4 exposes `Mcp-Session-Id` to handlers,
but did **not** exercise arbitrary custom headers. Risk #1 in the
Phase 6 plan flagged this as the single biggest unknown that could
force a redesign (clientInfo handshake fallback). This spike closes
the unknown on the rmcp side.

## Approach

1. Added a diagnostic `#[tool]` method `_caller_probe` on `Ccom` that
   reads `ctx.extensions.get::<http::request::Parts>()`, pulls
   `x-ccom-caller` off the headers, and returns its value as a
   text-content result. Falls back to the sentinel `"<missing>"`
   when the header is absent so the handler never panics on callers
   that predate the Phase 6 config change.
2. Added `http = "1"` as an explicit dep in `Cargo.toml` (was already
   a transitive dep via axum; making it direct keeps the import
   `http::request::Parts` honest).
3. Wrote `tests/spike_caller_header.rs` — a fully self-contained
   integration test that drives the real `McpServer` over loopback
   HTTP using the same hand-rolled `ureq` client pattern as
   `tests/mcp_readonly.rs`, extended with an optional
   `X-Ccom-Caller` header passed on every POST.

Three test cases:
- `custom_header_propagates_to_tool_handler_via_request_context_extensions`
  — header value `"42"` round-trips unchanged
- `missing_caller_header_returns_sentinel` — absent header yields
  `"<missing>"`
- `multibyte_header_value_preserved` — printable ASCII caller id
  (`"ccom-session-99"`) survives unchanged

## Result

All three tests pass on first run. The rmcp 1.4 documented pattern

```rust
ctx.extensions.get::<http::request::Parts>()
    .and_then(|p| p.headers.get("x-ccom-caller"))
```

works exactly as advertised in `StreamableHttpService`'s rustdoc
(`/Users/mkrueger/.cargo/registry/src/index.crates.io-*/rmcp-1.4.0/src/transport/streamable_http_server/tower.rs:270-329`).
Custom headers injected via `.mcp.json`'s `"headers"` block will
reach the tool handler the same way the existing `Mcp-Session-Id`
header does.

## What this means for Phase 6

- **Risk #1 downgraded.** The rmcp-side unknown is closed. Task 3's
  handler can identify the caller in ~6 lines.
- **Option 1 wins.** No need for the clientInfo-handshake fallback.
- **Plan text still accurate.** Phase 6 §Caller identification
  Option 1 is the chosen path — no design rewrite needed.

## What this does NOT prove

The spike exercises the rmcp/server side only. It does **not** verify
that Claude Code's own MCP HTTP client:
1. Honors the `"headers"` block in an HTTP MCP server entry in
   `.mcp.json`
2. Propagates those headers on every tool-call POST (not just
   `initialize`)

These are Claude-Code-side contracts documented in the Claude Code
2.1.x MCP config schema. Verifying them requires a manual smoke test
with a real Claude subprocess — cheapest to do as part of Phase 6
Task 10, but can be pulled forward if we want early confidence.

Fallback if Claude Code's header propagation turns out to be broken:
switch to encoding the caller id into the `clientInfo.name` field at
`initialize` time, which Phase 4's spike already proved Claude Code
sends through. That's a local change to Task 3's handler — it does
not ripple into Tasks 4-9.

## Artifacts

- `src/mcp/handlers.rs:213-242` — `_caller_probe` tool (kept as a
  permanent diagnostic; ~30 lines, leaks no state)
- `tests/spike_caller_header.rs` — 3 integration tests
- `Cargo.toml:18` — explicit `http = "1"` dep

## Follow-up

1. Merge this spike to `main`.
2. Branch `session-mgmt/phase-6-driver-role` from main.
3. Start Phase 6 Task 1 (session role data model) with caller
   identification de-risked.
