# Phase 5 — MCP Write Tools

**Branch:** `session-mgmt/phase-5-mcp-write`
**Depends on:** Phase 4 (merged)
**Blocks:** Phase 6
**Design refs:**
- `docs/designs/session-management.md` §5 (tools), §6 (safety stubs)
- `docs/plans/session-management-phase-4-6.md` §Phase 5 (master plan)
- `docs/pr-review-pr8.md` (input sanitization security item)

## Context

Phase 5 adds two **write** MCP tools to the Phase 4 server:

- **`send_prompt`** — thin wrapper over `SessionManager::send_prompt` with strict input sanitization. **No confirmation modal** (per design doc §6 — gating prompts is theater; if an MCP caller can reach the loopback port it's already inside the trust boundary).
- **`kill_session`** — destructive; **always** triggers a TUI confirmation modal in Phase 5 (silent-kill-for-own-children arrives in Phase 6 with driver role).

Both tools are **scope-restricted**: they only operate on sessions currently owned by the TUI's `SessionManager`. If the caller passes an unknown `session_id`, the handler returns `NotFound` without side effects.

## Architecture

### Cross-thread confirmation flow (for `kill_session`)

The MCP handler runs on the dedicated `ccom-mcp` thread inside a tokio task. The TUI's `handle_event` loop runs on the main thread. The handler needs to block (asynchronously) until the main thread's user presses `y`/`n`.

```
ccom-mcp thread                      main TUI thread
──────────────                       ───────────────
kill_session handler fires
  │
  ├── builds ConfirmRequest
  │    { tool, session_id, resp_tx: oneshot::Sender<ConfirmResponse> }
  │
  ├── sends request via std::sync::mpsc::Sender<ConfirmRequest>
  │   (channel owned by ReadOnlyCtx → Arc<ConfirmBridge>)
  │
  ├── .await on oneshot::Receiver ──────────────►  next handle_event tick
  │                                                 │
  │                                                 ├── drain confirm_rx
  │                                                 ├── push `ConfirmPending` into
  │                                                 │   `App::pending_confirm`
  │                                                 ├── App::mode → McpConfirm
  │                                                 │
  │                                                 ├── user presses 'y'
  │                                                 │   → app.confirm_current(Allow)
  │                                                 │   → oneshot_tx.send(Allow)
  │                                                 │
  ◄──────────────────────────────────────────────── oneshot resolves with Allow
  │
  ├── Allow → self.ctx.send_kill(session_id) → returns turn_id
  ├── Deny  → McpError with "user denied"
  │
  └── returns CallToolResult to rmcp client
```

**Why `std::sync::mpsc` + `tokio::sync::oneshot` bridge:** the MCP → main direction uses `std::sync::mpsc::Sender` because the main thread has a `try_recv` loop (matches the existing `EventBus` subscribe pattern). The response direction uses `tokio::sync::oneshot` because the waiting side is an async tool handler inside tokio. Both are `Send + 'static` and can cross the thread boundary via `Arc`.

### `ConfirmBridge` type

Lives in `src/mcp/confirm.rs` (new file):

```rust
pub struct ConfirmBridge {
    /// Sender used by MCP tool handlers to request confirmation.
    /// Receiver is held by `App`, drained each tick.
    pub tx: Mutex<std::sync::mpsc::Sender<ConfirmRequest>>,
}

pub struct ConfirmRequest {
    pub tool: ConfirmTool,     // `SendPrompt` | `KillSession`
    pub session_id: usize,
    pub resp_tx: tokio::sync::oneshot::Sender<ConfirmResponse>,
}

pub enum ConfirmResponse {
    Allow,
    Deny,
}

pub enum ConfirmTool {
    SendPrompt,   // reserved — not actually used in Phase 5 (see Task 1 notes)
    KillSession,
}
```

The bridge is constructed in `main.rs`, shared via `Arc`, and handed to both:
1. `App` (receives via owned `Receiver<ConfirmRequest>`)
2. `ReadOnlyCtx` (gains a new `confirm: Option<Arc<ConfirmBridge>>` field so handlers can request)

### State on `App`

```rust
pub struct App {
    // existing fields…
    pub(crate) confirm_rx: std::sync::mpsc::Receiver<ConfirmRequest>,
    pub(crate) pending_confirm: Option<ConfirmRequest>,
}

pub enum AppMode {
    // existing variants…
    McpConfirm,
}
```

`handle_event(Event::Tick)` drains `confirm_rx` with `try_recv`. If a request arrives, store it in `pending_confirm` and switch `mode = AppMode::McpConfirm`. The existing `handle_key` dispatcher routes keys to a new `handle_mcp_confirm_key` that resolves `y`/`n`/`Esc`.

## Task breakdown

### Task 1 — `send_prompt` tool (~1 hour)

**File:** `src/mcp/handlers.rs`

Add a new `#[tool]` method:

```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendPromptArgs {
    pub session_id: usize,
    pub text: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SendPromptResult {
    pub turn_id: u64,
}

#[tool(description = "Send a prompt to a session. Returns the allocated turn_id. \
                      Scope-restricted: session_id must exist in the TUI. \
                      Text is sanitized: control characters stripped (except \n/\t), \
                      ANSI escape sequences stripped, max 16 KB.")]
async fn send_prompt(
    &self,
    Parameters(args): Parameters<SendPromptArgs>,
) -> Result<CallToolResult, McpError> {
    // 1. Sanitize text.
    let sanitized = match sanitize_prompt_text(&args.text) {
        Ok(t) => t,
        Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
    };
    // 2. Scope check + send.
    let result = self.ctx.send_prompt(args.session_id, &sanitized);
    match result {
        Ok(turn_id) => {
            let wire = SendPromptResult { turn_id: turn_id.0 };
            let json = serde_json::to_string(&wire)
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
            Ok(CallToolResult::success(vec![Content::text(json)]))
        }
        Err(SendPromptRejection::NotFound) => {
            Ok(CallToolResult::error(vec![Content::text(format!(
                "session {} not found", args.session_id
            ))]))
        }
    }
}
```

**New helper in `src/mcp/handlers.rs`** (or a new `src/mcp/sanitize.rs` if it grows):

```rust
const MAX_PROMPT_BYTES: usize = 16 * 1024;

fn sanitize_prompt_text(input: &str) -> Result<String, String> {
    if input.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "text too large: {} bytes (max {})",
            input.len(), MAX_PROMPT_BYTES
        ));
    }

    // Strip ANSI CSI and OSC sequences first. Reuse the ansi_strip
    // helper from pty/response_boundary.rs if it's exposed; otherwise
    // reimplement here. The function already exists and is tested.
    let stripped = ansi_strip(input);

    // Byte-by-byte control-char filter.
    let mut out = String::with_capacity(stripped.len());
    for ch in stripped.chars() {
        match ch {
            '\n' | '\t' => out.push(ch),
            '\r' => out.push('\n'),  // normalize CR/CRLF to LF
            c if c.is_control() => {} // drop
            c => out.push(c),
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        return Err("text is empty after sanitization".to_string());
    }

    Ok(trimmed.to_string())
}
```

**`ReadOnlyCtx::send_prompt`** — new method on `ReadOnlyCtx`:

```rust
pub fn send_prompt(&self, session_id: usize, text: &str) -> Result<TurnId, SendPromptRejection> {
    let mut mgr = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
    if mgr.get(session_id).is_none() {
        return Err(SendPromptRejection::NotFound);
    }
    mgr.send_prompt(session_id, text)
        .map_err(|_| SendPromptRejection::NotFound) // send_prompt already returns Err on unknown id
}

pub enum SendPromptRejection { NotFound }
```

Note: **rename `ReadOnlyCtx`** — it's no longer read-only. Call it `McpCtx` or `SessionCtx`. This rename ripples through `server.rs`, `handlers.rs`, `state.rs`, `app/mod.rs`. Do it as the first step in Task 1 so Task 2 doesn't collide.

**Tests:**
- `sanitize_prompt_text_strips_ansi`
- `sanitize_prompt_text_normalizes_cr_to_lf`
- `sanitize_prompt_text_rejects_empty_post_sanitize`
- `sanitize_prompt_text_rejects_oversized`
- `send_prompt_returns_not_found_for_unknown_session`

### Task 2 — `ConfirmBridge` infrastructure (~1 hour)

**New file:** `src/mcp/confirm.rs`

Define `ConfirmBridge`, `ConfirmRequest`, `ConfirmResponse`, `ConfirmTool`. Provide:

- `ConfirmBridge::new() -> (Arc<Self>, Receiver<ConfirmRequest>)`
- `ConfirmBridge::request(&self, tool, session_id) -> impl Future<Output = ConfirmResponse>` — async, sends a `ConfirmRequest` on the std mpsc and awaits the oneshot

**Wire into `McpCtx`:**
```rust
pub struct McpCtx {
    pub sessions: Arc<Mutex<SessionManager>>,
    pub bus: Arc<EventBus>,
    pub confirm: Arc<ConfirmBridge>,  // NEW
}
```

**Wire into `App`:**
```rust
pub struct App {
    // existing…
    confirm_rx: mpsc::Receiver<ConfirmRequest>,
    pending_confirm: Option<ConfirmRequest>,
}
```

**Wire into `App::new`:**
```rust
let (confirm_bridge, confirm_rx) = ConfirmBridge::new();
let ctx = Arc::new(McpCtx { sessions, bus, confirm: confirm_bridge });
```

**`App::handle_event(Event::Tick)`** gains a confirm drain:
```rust
while let Ok(req) = self.confirm_rx.try_recv() {
    if self.pending_confirm.is_none() {
        self.pending_confirm = Some(req);
        self.mode = AppMode::McpConfirm;
    } else {
        // A second request while one is pending — deny immediately.
        let _ = req.resp_tx.send(ConfirmResponse::Deny);
    }
}
```

### Task 3 — `kill_session` tool handler (~1 hour)

**File:** `src/mcp/handlers.rs`

```rust
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KillSessionArgs {
    pub session_id: usize,
}

#[tool(description = "Kill a session. Triggers a TUI confirmation modal — \
                      the caller blocks until the user presses y/n. \
                      Scope-restricted: session_id must exist in the TUI. \
                      Returns the exit code after kill or an error on denial.")]
async fn kill_session(
    &self,
    Parameters(args): Parameters<KillSessionArgs>,
) -> Result<CallToolResult, McpError> {
    // Scope check first.
    {
        let mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if mgr.get(args.session_id).is_none() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "session {} not found", args.session_id
            ))]));
        }
    }
    // Request confirmation.
    let resp = self.ctx.confirm
        .request(ConfirmTool::KillSession, args.session_id)
        .await;
    match resp {
        ConfirmResponse::Allow => {
            let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            mgr.kill(args.session_id);
            Ok(CallToolResult::success(vec![Content::text(format!(
                "session {} killed", args.session_id
            ))]))
        }
        ConfirmResponse::Deny => Ok(CallToolResult::error(vec![Content::text(format!(
            "kill_session({}) denied by user", args.session_id
        ))])),
    }
}
```

### Task 4 — TUI confirmation modal (~1 hour)

**New file:** `src/ui/panels/mcp_confirm.rs`

Simple centered modal (pattern follows `quit_confirm` in `src/app/render.rs`):

```
┌─ MCP Confirmation ─┐
│                    │
│ Allow MCP tool     │
│ kill_session on    │
│ session 3?         │
│                    │
│ [y] Yes  [n] No    │
└────────────────────┘
```

**Keyboard handling in `src/app/keys.rs`** (new `handle_mcp_confirm_key`):

```rust
fn handle_mcp_confirm_key(&mut self, key: KeyEvent) {
    let resp = match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(ConfirmResponse::Allow),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ConfirmResponse::Deny),
        _ => None,
    };
    if let Some(resp) = resp {
        if let Some(req) = self.pending_confirm.take() {
            let _ = req.resp_tx.send(resp);
        }
        self.mode = AppMode::Dashboard;
    }
}
```

**Dispatcher update:** `handle_key` routes `AppMode::McpConfirm` keys through `handle_mcp_confirm_key`.

**Render:** `src/app/render.rs` → new branch for `AppMode::McpConfirm` that draws the dashboard underneath (via existing `draw_dashboard_mode`) plus a centered modal on top (similar to `draw_quit_confirm`).

### Task 5 — Integration tests (~1.5 hours)

**New file:** `tests/mcp_write.rs`

Uses the same `ureq`-based `McpClient` helper from `tests/mcp_readonly.rs`. Tests:

1. **`send_prompt_delivers_bytes`** — spawn a `/bin/cat` session via `McpServer::start_with`, call `send_prompt("echo test")`, verify `cat` echoes it back via PTY output events.
2. **`send_prompt_rejects_control_chars`** — send `"hello\x01world"`, verify the `\x01` is stripped.
3. **`send_prompt_rejects_ansi_escape`** — send `"hello\x1b[31mred\x1b[0m"`, verify the escape sequence is stripped.
4. **`send_prompt_not_found_for_unknown_session`** — call with `session_id: 999`, verify tool error.
5. **`kill_session_waits_for_confirmation`** — call `kill_session`, verify the tool blocks until the main thread resolves the confirm. Test infrastructure: main-thread simulation drains `confirm_rx` and immediately responds `Allow`.
6. **`kill_session_denied`** — same flow but respond `Deny`, verify the session survives and the tool returns an error.

### Task 6 — End-to-end verification (manual, Task 9 equivalent)

- Spawn two sessions A and B via `cargo run`
- From A, ask Claude to call `send_prompt` on B's id
- Verify the prompt arrives in B
- From A, ask Claude to call `kill_session` on B
- Verify the TUI modal pops
- Press `y` → B exits
- Repeat with `n` → B survives
- Press `Esc` → treated as Deny

## Parallelism plan

Per the master plan: **Task 1 (`send_prompt`) and Tasks 2+3 (`ConfirmBridge` + `kill_session`) are independent** once Phase 4's scaffolding is in place. Two parallel subagents:

- **Subagent A**: Task 1 (send_prompt + sanitize). Also does the `ReadOnlyCtx` → `McpCtx` rename since this is the path where the field access is densest.
- **Subagent B**: Tasks 2 + 3 (ConfirmBridge + kill_session handler). Waits for the rename to land.

Coordination: do the rename first on the main branch (trivially mechanical, ~15 min), then launch both subagents.

After both merge: Task 4 (TUI modal), Task 5 (integration tests) sequential. Task 6 is manual.

## Risks

1. **Cross-thread confirmation bridge** is the hardest piece. The `std::sync::mpsc` + `tokio::sync::oneshot` bridge is non-trivial; get it wrong and you either deadlock the MCP handler or drop confirmations. Mitigate with a unit test in `src/mcp/confirm.rs` that exercises the round-trip using a synthetic sender thread.
2. **Input sanitization false negatives** — if the ANSI strip or control-char filter misses a sequence, driver-supplied text can still smuggle escape codes into the target PTY. Mitigate: reuse the existing `ansi_strip` from `response_boundary.rs` (already battle-tested) rather than reimplementing.
3. **`ReadOnlyCtx` rename ripples** through several files. Do it first so the subagents don't collide.
4. **`send_prompt` returning a `TurnId`** — the `TurnId` inner `u64` was accessed via `.0` in Phase 4's wire type. Same idiom applies.
5. **Rmcp session 30s idle timeout** (known from Phase 4) — `kill_session` could block longer than 30s waiting for user confirmation, at which point rmcp tears down the MCP session. Mitigation: either bump `sse_keep_alive` or keep the confirmation timeout short (e.g., 20s client-side; after that, auto-deny). For Phase 5 we'll document the 30s ceiling as a known limitation and cap the confirmation wait at 25s.

## Verification

- `cargo test` — existing 328 + new tests pass
- `cargo clippy` — zero warnings
- `cargo fmt --check` — clean
- Integration tests in `tests/mcp_write.rs` pass
- Manual Task 6 smoke test: send_prompt delivers, kill_session modal works for allow/deny/esc paths

## Task 6 smoke test results (2026-04-12)

Ran all six checkpoints from the protocol. All green:

1. **`send_prompt` clean delivery** — session A called
   `list_sessions` to find B's id, then `send_prompt` with plain
   text. B received the prompt and responded. ✅

2. **Confirmation modal UI** — `kill_session` from A triggered the
   expected modal overlay on the dashboard with the correct tool
   name and target label (`kill_session` → `session N (claude-2)`).
   ✅

3. **Allow / Deny paths** — `y` killed the target session and the
   session disappeared from the list; `n` / `Esc` preserved the
   target and returned a tool error containing `denied`. ✅

4. **Input sanitization (caveat, not a bug)** — asking Claude A to
   send text containing `\u001b` sequences resulted in the LLM
   transmitting the literal backslash-u-0-0-1-b characters via its
   tool-call JSON rather than a real 0x1b byte. Our sanitizer saw
   plain ASCII and passed it through, which is correct. Claude A
   then explained to the user that "had a real escape been sent,
   it would have been stripped." This is an LLM instruction-
   following quirk, not a ccom issue. The integration test
   `send_prompt_strips_ansi_escapes_before_write` in
   `tests/mcp_write.rs` constructs a real 0x1b byte at the Rust
   level (`"\u{1b}[31m..."`), which serde_json emits as `\u001b`
   on the wire and rmcp correctly deserializes back to 0x1b before
   the tool body runs — that path is covered and passing.

5. **No visual glitches** — no scroll artifacts, no redraw issues,
   no log leakage into the TUI (the Phase 4 log-to-file redirect
   continues to work). ✅

6. **Logs clean** — no panics, no ERROR entries beyond the known
   rmcp session-keep-alive lines (which are filtered to `warn`
   level via the default `RUST_LOG` filter set up in `main.rs`).

## Known limitations carried forward to Phase 6

- rmcp 30-second session keep-alive — mitigated at the handler
  level with a 25s `tokio::time::timeout` on the confirm wait,
  which converts the stall into a clean tool error rather than
  letting rmcp tear down the transport mid-modal.
- `subscribe` (Phase 4) still cuts out after ~30s idle — Phase 6
  or a later polish pass will add a periodic heartbeat
  notification from the spawned task.
- LLM-driven tool-call escape semantics — documented above as an
  LLM-side quirk. Nothing actionable in ccom.
