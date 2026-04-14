//! Read-only MCP tool handlers.
//!
//! All three read-only tools (`list_sessions`, `read_response`,
//! `subscribe`) live in this module as `#[tool]`-annotated methods
//! on the `Ccom` handler struct. Each MCP session gets its own
//! `Ccom` instance via the factory closure in
//! [`super::server::run_server`]; the instance holds an
//! `Arc<McpCtx>` for shared state.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};

use super::confirm::{ConfirmResponse, ConfirmTool};
use super::sanitize::{sanitize_label, sanitize_prompt_text};
use super::state::{McpCtx, Scope, SendPromptRejection};

/// Parse the `X-Ccom-Caller` request header into a ccom session id.
///
/// Returns `None` when the header is absent (pre-Phase-6 client,
/// unit tests that construct a `RequestContext` without it, or a
/// direct HTTP caller that never set it) or when the value fails
/// to parse as a `usize`. Callers default to `Scope::Full` on
/// `None` — the legacy path — so an unparseable header silently
/// degrades to "unknown caller" rather than hard-failing the tool.
///
/// **No authentication is performed.** Any local process that can
/// reach the loopback port can claim any caller id; there is no
/// cryptographic identity check. The server is loopback-only
/// (pinned via `StreamableHttpServerConfig::with_allowed_hosts` in
/// `src/mcp/server.rs`), which bounds the threat surface to local
/// processes on the same host. If the server ever grows a
/// non-loopback binding, this header mechanism becomes insufficient
/// and needs replacing with a real auth layer — see
/// `docs/plans/phase-6-driver-role.md` §Risks.
fn caller_id_from_ctx(ctx: &RequestContext<RoleServer>) -> Option<usize> {
    ctx.extensions
        .get::<http::request::Parts>()
        .and_then(|parts| parts.headers.get("x-ccom-caller"))
        .and_then(|hv| hv.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
}

/// Per-MCP-session tool handler. Constructed by the factory closure
/// in [`super::server::run_server`] on every new MCP session, so
/// instance-level state here is per-session (not shared across
/// sessions). Cross-session state lives in [`McpCtx`].
#[derive(Clone)]
pub struct Ccom {
    /// Shared ctx used by all tool handlers. `Arc<>` so the clone
    /// inside the factory closure is cheap.
    pub(super) ctx: Arc<McpCtx>,
    /// Loopback port the embedded MCP server is listening on. `Some`
    /// in production (set by the factory closure in
    /// `run_server`); `None` in unit tests that construct a `Ccom`
    /// directly. The `spawn_session` handler uses it to write the
    /// child session's `.mcp.json` so the child points at the same
    /// server. Children with `None` get no `.mcp.json` — a degraded
    /// but still functional mode that matches the pre-Phase-4 Terminal
    /// session spawn path.
    #[allow(dead_code)]
    pub(super) mcp_port: Option<u16>,
    /// Generated tool-router field required by the rmcp
    /// `#[tool_router]` macro. The field name is load-bearing — the
    /// macro reads it by name. Triggers a dead_code warning because
    /// rustc can't trace through the macro.
    #[allow(dead_code)]
    tool_router: ToolRouter<Ccom>,
}

/// Arguments for the `read_response` tool.
///
/// Lives in `handlers.rs` (not `state.rs`) so all MCP wire types
/// stay local to the module that owns the rmcp `#[tool]` macros.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadResponseArgs {
    /// The ccom session id to read from.
    pub session_id: usize,
    /// Optional turn id. If omitted, returns the most recently
    /// completed turn for the session.
    pub turn_id: Option<u64>,
    /// Long-poll timeout in seconds. Default 60, clamped to max 300.
    /// If the requested turn isn't stored yet, waits up to this
    /// duration for a `ResponseComplete` event on the bus.
    pub timeout_secs: Option<u64>,
}

/// Wire-format representation of a completed turn returned by
/// `read_response`.
///
/// The domain [`crate::session::StoredTurn`] contains `Instant`
/// fields which don't serialize cleanly across the MCP boundary —
/// this flat shape is the externally-visible projection.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct StoredTurnWire {
    pub turn_id: u64,
    pub body: String,
    pub completed: bool,
}

impl From<&crate::session::StoredTurn> for StoredTurnWire {
    fn from(t: &crate::session::StoredTurn) -> Self {
        Self {
            // `TurnId`'s inner `u64` is `pub(crate)` — `handlers.rs`
            // lives in the same crate, so direct `.0` access is
            // permitted (and matches the idiom used in
            // `events.rs` tests).
            turn_id: t.turn_id.0,
            body: t.body.clone(),
            completed: t.completed_at.is_some(),
        }
    }
}

/// Arguments for the `send_prompt` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SendPromptArgs {
    /// The ccom session id to send the prompt to.
    pub session_id: usize,
    /// Prompt text. Sanitized before delivery: control chars stripped
    /// (except `\n`/`\t`), ANSI escape sequences stripped, CR/CRLF
    /// normalized to LF, max 16 KB.
    pub text: String,
}

/// Wire-format reply for `send_prompt`. The domain [`crate::session::TurnId`]
/// wraps a `pub(crate) u64`; this struct is the externally-visible
/// projection.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SendPromptWire {
    pub turn_id: u64,
}

/// Arguments for the `kill_session` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct KillSessionArgs {
    /// The ccom session id to kill. Scope-restricted: must be a
    /// session currently owned by the TUI.
    pub session_id: usize,
}

/// Arguments for the `spawn_session` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SpawnSessionArgs {
    /// Short human label for the child session (shown in the TUI
    /// session list). Sanitized: ANSI/control stripped, whitelist
    /// of ASCII alnum + ` -_./:`, truncated to 64 chars.
    pub label: String,
    /// Working directory for the child. If omitted, the child
    /// inherits the driver's `working_dir`.
    pub working_dir: Option<String>,
    /// Optional first prompt to send to the child after spawn.
    /// Sanitized by the same policy as `send_prompt`.
    pub initial_prompt: Option<String>,
}

/// Wire-format reply for `spawn_session`.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
pub struct SpawnSessionWire {
    pub session_id: usize,
    pub label: String,
}

/// Arguments for the `subscribe` tool.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SubscribeArgs {
    /// Optional filter: only forward events for sessions whose id
    /// is in this list. `None` or empty means no filter.
    pub session_ids: Option<Vec<usize>>,
    /// Optional filter: only forward events whose type name is in
    /// this list. Valid type names: `spawned`, `prompt_submitted`,
    /// `response_complete`, `prompt_pending`, `exited`,
    /// `status_changed`. `None` or empty means no filter.
    pub event_types: Option<Vec<String>>,
}

/// Wire-format representation of a filtered `SessionEvent` forwarded
/// over the MCP notification channel. Mirrors the domain enum but
/// uses a flat JSON shape with an explicit `type` discriminator so
/// clients don't need to understand rmcp's enum serialization.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEventWire {
    Spawned { session_id: usize, label: String },
    PromptSubmitted { session_id: usize, turn_id: u64 },
    ResponseComplete { session_id: usize, turn_id: u64 },
    PromptPending { session_id: usize, kind: String },
    Exited { session_id: usize, code: i32 },
    StatusChanged { session_id: usize, status: String },
}

impl SessionEventWire {
    /// Canonical lowercase type-name used for filter matching.
    fn type_name(&self) -> &'static str {
        match self {
            Self::Spawned { .. } => "spawned",
            Self::PromptSubmitted { .. } => "prompt_submitted",
            Self::ResponseComplete { .. } => "response_complete",
            Self::PromptPending { .. } => "prompt_pending",
            Self::Exited { .. } => "exited",
            Self::StatusChanged { .. } => "status_changed",
        }
    }

    /// The session id this event belongs to — used for filter
    /// matching. Every variant has one.
    fn session_id(&self) -> usize {
        match self {
            Self::Spawned { session_id, .. }
            | Self::PromptSubmitted { session_id, .. }
            | Self::ResponseComplete { session_id, .. }
            | Self::PromptPending { session_id, .. }
            | Self::Exited { session_id, .. }
            | Self::StatusChanged { session_id, .. } => *session_id,
        }
    }
}

impl From<&crate::session::SessionEvent> for SessionEventWire {
    fn from(ev: &crate::session::SessionEvent) -> Self {
        use crate::session::SessionEvent;
        match ev {
            SessionEvent::Spawned { session_id, label } => Self::Spawned {
                session_id: *session_id,
                label: label.clone(),
            },
            SessionEvent::PromptSubmitted {
                session_id,
                turn_id,
            } => Self::PromptSubmitted {
                session_id: *session_id,
                turn_id: turn_id.0,
            },
            SessionEvent::ResponseComplete {
                session_id,
                turn_id,
            } => Self::ResponseComplete {
                session_id: *session_id,
                turn_id: turn_id.0,
            },
            SessionEvent::PromptPending { session_id, kind } => Self::PromptPending {
                session_id: *session_id,
                kind: kind.clone(),
            },
            SessionEvent::Exited { session_id, code } => Self::Exited {
                session_id: *session_id,
                code: *code,
            },
            SessionEvent::StatusChanged { session_id, status } => Self::StatusChanged {
                session_id: *session_id,
                status: format!("{status:?}"),
            },
        }
    }
}

#[tool_router]
impl Ccom {
    #[allow(dead_code)] // used by unit tests; bin target can't see them
    pub(super) fn new(ctx: Arc<McpCtx>) -> Self {
        Self {
            ctx,
            mcp_port: None,
            tool_router: Self::tool_router(),
        }
    }

    /// Production constructor used by the `run_server` factory
    /// closure once the port is known. Tests use [`Self::new`] which
    /// leaves `mcp_port = None`.
    pub(super) fn new_with_port(ctx: Arc<McpCtx>, mcp_port: u16) -> Self {
        Self {
            ctx,
            mcp_port: Some(mcp_port),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "List all sessions ccom is currently managing. \
                       Returns a JSON array of SessionSummary objects \
                       with id, label, working_dir, status, last_activity_secs, \
                       and context_percent. A driver caller (identified by \
                       the X-Ccom-Caller header) sees only sessions it spawned \
                       or has attached to itself; solo callers see every session.")]
    async fn list_sessions(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let mut summaries = self.ctx.list_sessions();
        // Phase 6 Task 4: scope-filter the output for driver callers.
        // `caller_id_from_ctx` returns `None` for legacy clients
        // without the header; `caller_scope` then returns
        // `Scope::Full` and the retain is a no-op.
        if let Some(caller_id) = caller_id_from_ctx(&ctx) {
            let scope = self.ctx.caller_scope(caller_id);
            summaries.retain(|s| scope.permits(s.id));
        }
        let json = serde_json::to_string(&summaries)
            .map_err(|e| McpError::internal_error(format!("list_sessions serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Read the body of a completed turn for a session. \
                       Fast path: returns immediately if the turn is already \
                       stored. Long-poll path: if the turn is in flight, \
                       subscribes to the event bus and waits up to timeout_secs \
                       (default 60, max 300) for ResponseComplete.")]
    async fn read_response(
        &self,
        Parameters(args): Parameters<ReadResponseArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Phase 6 Task 4: scope check — a driver caller may only
        // read responses from sessions in its scope. Unknown caller
        // (no header) → `Scope::Full` → legacy behavior.
        if let Some(caller_id) = caller_id_from_ctx(&ctx) {
            let scope = self.ctx.caller_scope(caller_id);
            if !scope.permits(args.session_id) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "session {} not found",
                    args.session_id
                ))]));
            }
        }

        let requested_turn = args.turn_id.map(crate::session::TurnId::new);

        // Fast path: is the turn already stored?
        if let Some(stored) = self.ctx.get_response(args.session_id, requested_turn) {
            let wire = StoredTurnWire::from(&stored);
            let json = serde_json::to_string(&wire).map_err(|e| {
                McpError::internal_error(format!("read_response serialize: {e}"), None)
            })?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        // Long-poll path: subscribe to the bus first, *then* re-check
        // the store. Subscribing first closes the TOCTOU race where
        // the turn lands between the initial check and the
        // subscription.
        let rx = self.ctx.bus.subscribe();

        // Re-check after subscribing so we don't miss a turn that
        // landed in that tiny window.
        if let Some(stored) = self.ctx.get_response(args.session_id, requested_turn) {
            let wire = StoredTurnWire::from(&stored);
            let json = serde_json::to_string(&wire).map_err(|e| {
                McpError::internal_error(format!("read_response serialize: {e}"), None)
            })?;
            return Ok(CallToolResult::success(vec![Content::text(json)]));
        }

        let timeout = std::time::Duration::from_secs(args.timeout_secs.unwrap_or(60).min(300));
        let deadline = std::time::Instant::now() + timeout;

        // The receiver is `std::sync::mpsc::Receiver`, which doesn't
        // play nicely with async. Poll via `try_recv` + `tokio::sleep`
        // to yield back to the runtime between drains.
        while std::time::Instant::now() < deadline {
            while let Ok(ev) = rx.try_recv() {
                if let crate::session::SessionEvent::ResponseComplete {
                    session_id,
                    turn_id,
                } = ev
                    && session_id == args.session_id
                    && requested_turn.is_none_or(|t| t == turn_id)
                    && let Some(stored) = self.ctx.get_response(args.session_id, Some(turn_id))
                {
                    // Refetch — `check_response_boundaries` pushes to
                    // the store *before* publishing, so the turn is
                    // guaranteed present here.
                    let wire = StoredTurnWire::from(&stored);
                    let json = serde_json::to_string(&wire).map_err(|e| {
                        McpError::internal_error(format!("read_response serialize: {e}"), None)
                    })?;
                    return Ok(CallToolResult::success(vec![Content::text(json)]));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Review H2: timeout is an expected outcome, not an internal
        // bug. Surface it as a tool-level error result
        // (`CallToolResult { is_error: true }`) so clients can
        // distinguish "no response yet" from "server misbehaved".
        // The payload is plain text explaining the timeout — no
        // internal state leaked.
        Ok(CallToolResult::error(vec![Content::text(format!(
            "timeout after {}s waiting for session {} turn {:?}",
            timeout.as_secs(),
            args.session_id,
            args.turn_id,
        ))]))
    }

    #[tool(description = "Send a prompt to a session. Returns the \
                       allocated turn_id as JSON. Scope-restricted: \
                       session_id must exist in the TUI. Text is \
                       sanitized before delivery — ANSI escape \
                       sequences stripped, control chars stripped \
                       (except \\n/\\t), CR/CRLF normalized to LF, \
                       max 16 KB.")]
    async fn send_prompt(
        &self,
        Parameters(args): Parameters<SendPromptArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Phase 6 Task 4: scope gate. A driver caller that targets a
        // session outside its scope gets the same `NotFound` shape as
        // an unknown session id — a driver must not be able to probe
        // the existence of sibling drivers' children.
        if let Some(caller_id) = caller_id_from_ctx(&ctx) {
            let scope = self.ctx.caller_scope(caller_id);
            if !scope.permits(args.session_id) {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "session {} not found",
                    args.session_id
                ))]));
            }
        }

        // 1. Sanitize the caller-supplied text. Policy violations
        //    (empty, oversized, all-control) come back as plain
        //    strings suitable for a tool-level error.
        let sanitized = match sanitize_prompt_text(&args.text) {
            Ok(t) => t,
            Err(reason) => {
                return Ok(CallToolResult::error(vec![Content::text(reason)]));
            }
        };

        // 2. Scope-check against the TUI's SessionManager and dispatch.
        match self.ctx.send_prompt(args.session_id, &sanitized) {
            Ok(turn_id) => {
                // `TurnId`'s inner `u64` is `pub(crate)` — same idiom
                // as `StoredTurnWire::from`.
                let wire = SendPromptWire { turn_id: turn_id.0 };
                let json = serde_json::to_string(&wire).map_err(|e| {
                    McpError::internal_error(format!("send_prompt serialize: {e}"), None)
                })?;
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(SendPromptRejection::NotFound) => Ok(CallToolResult::error(vec![Content::text(
                format!("session {} not found", args.session_id),
            )])),
        }
    }

    #[tool(description = "Kill a session. Triggers a TUI confirmation \
                       modal — the caller blocks until the user presses \
                       y/n or Esc. Scope-restricted: session_id must \
                       exist in the TUI's SessionManager. Returns a \
                       success message after kill or a tool error on \
                       denial / unknown session. The TUI modal has a \
                       25-second window before auto-denying (prevents \
                       rmcp's 30-second idle session timeout from \
                       tearing the transport down mid-wait).")]
    async fn kill_session(
        &self,
        Parameters(args): Parameters<KillSessionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // 1. Scope check: unknown session → NotFound without any
        //    side effects and without raising a confirmation modal.
        {
            let mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if mgr.get(args.session_id).is_none() {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "session {} not found",
                    args.session_id
                ))]));
            }
        }

        // Phase 6 Task 6: driver-kill policy.
        //
        // - Solo caller (or legacy no-header call) → Scope::Full →
        //   confirmation modal (Phase 5 behavior, unchanged).
        // - Driver killing a child/attached session in its scope
        //   (and NOT itself) → silent: drivers own their children,
        //   prompting the user for every orchestrated shutdown is
        //   theater. A driver killing itself still goes through the
        //   modal — self-termination is destructive and the user
        //   should confirm.
        // - Driver targeting something outside its scope →
        //   NotFound (same opacity as an unknown id).
        let caller_id = caller_id_from_ctx(&ctx);
        let silent_driver_kill = if let Some(cid) = caller_id {
            match self.ctx.caller_scope(cid) {
                Scope::Full => false,
                Scope::Restricted(set) => {
                    if !set.contains(&args.session_id) {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "session {} not found",
                            args.session_id
                        ))]));
                    }
                    // In scope, and not the driver itself.
                    args.session_id != cid
                }
            }
        } else {
            false
        };

        if silent_driver_kill {
            let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if mgr.get(args.session_id).is_some() {
                mgr.kill(args.session_id);
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "session {} killed",
                    args.session_id
                ))]));
            } else {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "session {} was already gone",
                    args.session_id
                ))]));
            }
        }

        // 2. Request confirmation. If no bridge is wired (test-only
        //    McpCtx construction) we auto-deny — safer than
        //    auto-allowing destructive operations.
        let Some(bridge) = self.ctx.confirm.as_ref() else {
            log::warn!(
                "kill_session({}): no confirm bridge on McpCtx, auto-denying",
                args.session_id
            );
            return Ok(CallToolResult::error(vec![Content::text(
                "kill_session denied: no confirm bridge wired",
            )]));
        };

        // 3. Bound the wait so rmcp's 30s session keep-alive can't
        //    tear the transport down mid-modal. 25s leaves headroom.
        let request_fut = bridge.request(ConfirmTool::KillSession, args.session_id);
        let resp = match tokio::time::timeout(std::time::Duration::from_secs(25), request_fut).await
        {
            Ok(resp) => resp,
            Err(_elapsed) => {
                log::warn!(
                    "kill_session({}): confirm wait exceeded 25s, auto-denying",
                    args.session_id
                );
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "kill_session({}) timed out waiting for user confirmation",
                    args.session_id
                ))]));
            }
        };

        // 4. Act on the user's answer.
        match resp {
            ConfirmResponse::Allow => {
                let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
                // Re-check the session — the user may have killed it
                // manually in the TUI between the initial scope check
                // and the confirmation arriving.
                if mgr.get(args.session_id).is_some() {
                    mgr.kill(args.session_id);
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "session {} killed",
                        args.session_id
                    ))]))
                } else {
                    Ok(CallToolResult::error(vec![Content::text(format!(
                        "session {} was already gone by the time confirmation arrived",
                        args.session_id
                    ))]))
                }
            }
            ConfirmResponse::Deny => Ok(CallToolResult::error(vec![Content::text(format!(
                "kill_session({}) denied by user",
                args.session_id
            ))])),
        }
    }

    #[tool(description = "Subscribe to session events. The tool call returns \
                       immediately with a subscription acknowledgement; \
                       matching events are then delivered asynchronously as \
                       MCP `notifications/message` entries with level=info \
                       and logger=`ccom.session`. Events are JSON objects \
                       with a `type` discriminator and session-specific \
                       fields. Optional filters: session_ids (only these \
                       sessions), event_types (only these event type names).")]
    async fn subscribe(
        &self,
        Parameters(args): Parameters<SubscribeArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Subscribe to the bus BEFORE spawning the task so we don't
        // race with the first published event.
        let rx = self.ctx.bus.subscribe();
        let peer = ctx.peer.clone();
        let cancel = ctx.ct.clone();
        // Phase 6 Task 4: capture caller id for re-resolving scope
        // on every forwarded event. We deliberately do NOT cache the
        // scope itself — the set of permitted ids can change over
        // the lifetime of a subscription (e.g. the driver spawns a
        // new child, or the TUI attaches a peer). Re-reading once
        // per event is cheap (a pair of mutex locks) and always
        // correct; see Phase 6 Risk #2.
        let caller_id = caller_id_from_ctx(&ctx);
        let ctx_for_task = Arc::clone(&self.ctx);

        // Normalize filters for the spawned task's closure.
        let session_id_filter: Option<Vec<usize>> = args.session_ids.filter(|v| !v.is_empty());
        let event_type_filter: Option<Vec<String>> = args
            .event_types
            .map(|v| {
                v.into_iter()
                    .map(|s| s.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        tokio::spawn(async move {
            // Poll `rx.try_recv()` in a loop with a short sleep
            // between drains. The bus is `std::sync::mpsc`-backed,
            // so we can't `await` on it directly. 50ms cadence
            // matches `read_response` long-poll and keeps per-event
            // latency bounded.
            loop {
                if cancel.is_cancelled() {
                    log::debug!("mcp subscribe task: cancelled by client");
                    return;
                }

                // Drain everything currently queued before yielding.
                let mut any_received = false;
                loop {
                    match rx.try_recv() {
                        Ok(ev) => {
                            any_received = true;
                            let wire = SessionEventWire::from(&ev);

                            // Filter: caller scope. Re-resolve on
                            // every event so a driver subscription
                            // sees newly-spawned children as soon
                            // as they land in the manager. Legacy
                            // clients (no header) fall through to
                            // `Scope::Full` and are not filtered.
                            if let Some(cid) = caller_id {
                                let scope = ctx_for_task.caller_scope(cid);
                                if !scope.permits(wire.session_id()) {
                                    continue;
                                }
                            }

                            // Filter: session id.
                            if let Some(ids) = session_id_filter.as_ref()
                                && !ids.contains(&wire.session_id())
                            {
                                continue;
                            }

                            // Filter: event type.
                            if let Some(types) = event_type_filter.as_ref()
                                && !types.iter().any(|t| t == wire.type_name())
                            {
                                continue;
                            }

                            // Serialize + send as a logging notification.
                            let data = match serde_json::to_value(&wire) {
                                Ok(v) => v,
                                Err(e) => {
                                    log::warn!("mcp subscribe: serialize failed: {e}");
                                    continue;
                                }
                            };
                            // `LoggingMessageNotificationParam` is
                            // NOT `#[non_exhaustive]` in rmcp 1.4,
                            // so direct struct-literal construction
                            // is safe here (and avoids the builder
                            // dance `ServerInfo` needs).
                            let param = LoggingMessageNotificationParam {
                                level: LoggingLevel::Info,
                                logger: Some("ccom.session".to_string()),
                                data,
                            };
                            if let Err(e) = peer.notify_logging_message(param).await {
                                log::debug!(
                                    "mcp subscribe: notify_logging_message failed (peer gone?): {e}"
                                );
                                return;
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            log::debug!("mcp subscribe task: bus disconnected");
                            return;
                        }
                    }
                }

                // Yield back to the runtime. Use a shorter sleep when
                // we just drained something to keep latency low.
                let sleep_ms = if any_received { 10 } else { 50 };
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)) => {}
                    _ = cancel.cancelled() => {
                        log::debug!("mcp subscribe task: cancelled mid-sleep");
                        return;
                    }
                }
            }
        });

        Ok(CallToolResult::success(vec![Content::text(
            "subscribed: events will arrive as notifications/message entries with logger=ccom.session",
        )]))
    }

    #[tool(
        description = "Spawn a new Claude session under the calling driver's control. \
                       Driver-only: the caller must be a session with role=Driver (set \
                       via the `--driver` CLI flag / TOML config at ccom startup). \
                       The label is sanitized (ANSI/control stripped, 64 char max). \
                       SpawnPolicy gates user confirmation: Trust spawns silently, Budget \
                       spawns silently until the budget is exhausted then prompts, Ask \
                       prompts every time. The new child is Solo (v1 nesting cap — \
                       drivers cannot create drivers) with spawned_by = <driver id>. \
                       Returns the new session id and sanitized label as JSON."
    )]
    async fn spawn_session(
        &self,
        Parameters(args): Parameters<SpawnSessionArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // 1. Identify the caller. `spawn_session` is only reachable
        //    by a driver — any call without the header is rejected.
        let Some(caller_id) = caller_id_from_ctx(&ctx) else {
            return Ok(CallToolResult::error(vec![Content::text(
                "spawn_session requires a driver caller (missing X-Ccom-Caller header)",
            )]));
        };

        // 2. Sanitize the label up front so a policy rejection
        //    surfaces before any lock is taken or modal raised.
        let clean_label = match sanitize_label(&args.label) {
            Ok(l) => l,
            Err(reason) => {
                return Ok(CallToolResult::error(vec![Content::text(reason)]));
            }
        };

        // 3. Role check + budget policy decision + budget decrement,
        //    all under a single lock acquisition (Phase 6 Risk #4).
        //    We capture the decided policy and the caller's
        //    working_dir / cols / rows for the subsequent spawn.
        //
        //    Lock is released before any `.await` — critical because
        //    `ConfirmBridge::request` awaits and PTY spawn blocks.
        //
        // `budget_decremented` carries (policy, pre-decrement-value) so
        // step 6 can restore the budget if `spawn_with_role` fails.
        // Without this a failing PTY spawn permanently consumes a silent
        // spawn credit.
        let (needs_confirm, default_cwd, cols, rows, budget_decremented) = {
            let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            let Some(caller) = mgr.get(caller_id) else {
                return Ok(CallToolResult::error(vec![Content::text(
                    "spawn_session: caller session not found",
                )]));
            };
            let (policy, budget) = match &caller.role {
                crate::session::SessionRole::Driver {
                    spawn_budget,
                    spawn_policy,
                } => (*spawn_policy, *spawn_budget),
                crate::session::SessionRole::Solo => {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "spawn_session denied: caller is not a driver",
                    )]));
                }
            };
            let default_cwd = caller.working_dir.clone();
            let cols = caller.pty_size.cols;
            let rows = caller.pty_size.rows;

            let mut budget_decremented: Option<(crate::session::SpawnPolicy, u32)> = None;
            let needs_confirm = match policy {
                crate::session::SpawnPolicy::Trust => false,
                crate::session::SpawnPolicy::Budget => {
                    if budget > 0 {
                        // Atomic decrement under the same lock — no
                        // TOCTOU window where two concurrent calls
                        // could each see `budget > 0`.
                        mgr.set_role(
                            caller_id,
                            crate::session::SessionRole::Driver {
                                spawn_budget: budget - 1,
                                spawn_policy: policy,
                            },
                        );
                        budget_decremented = Some((policy, budget));
                        false
                    } else {
                        true
                    }
                }
                crate::session::SpawnPolicy::Ask => true,
            };
            (needs_confirm, default_cwd, cols, rows, budget_decremented)
        };

        // 4. If confirmation is needed, go through the same bridge
        //    pattern `kill_session` uses.
        if needs_confirm {
            let Some(bridge) = self.ctx.confirm.as_ref() else {
                log::warn!("spawn_session({caller_id}): no confirm bridge on McpCtx, auto-denying");
                return Ok(CallToolResult::error(vec![Content::text(
                    "spawn_session denied: no confirm bridge wired",
                )]));
            };
            let request_fut = bridge.request(ConfirmTool::SpawnSession, caller_id);
            let resp =
                match tokio::time::timeout(std::time::Duration::from_secs(25), request_fut).await {
                    Ok(resp) => resp,
                    Err(_) => {
                        log::warn!(
                            "spawn_session({caller_id}): confirm wait exceeded 25s, auto-denying"
                        );
                        return Ok(CallToolResult::error(vec![Content::text(
                            "spawn_session timed out waiting for user confirmation",
                        )]));
                    }
                };
            if resp != ConfirmResponse::Allow {
                return Ok(CallToolResult::error(vec![Content::text(
                    "spawn_session denied by user",
                )]));
            }
        }

        // 5. Resolve cwd and build the SpawnConfig. Children are
        //    always Claude sessions with hooks installed — v1
        //    doesn't let drivers spawn terminal children.
        let cwd = args
            .working_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or(default_cwd);

        let Some(event_tx) = self.ctx.event_tx.clone() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "spawn_session: no event_tx on McpCtx (test context?)",
            )]));
        };

        // Command resolution. Tests set `CCOM_TEST_SPAWN_CMD` to a
        // stand-in like `/bin/cat` so the full tool path can be
        // exercised without a real Claude binary on PATH. Production
        // reads the normal launcher.
        //
        // `use_test_command` also gates hook + mcp_config injection —
        // `/bin/cat` doesn't accept `--settings` / `--mcp-config` flags.
        let test_cmd = std::env::var("CCOM_TEST_SPAWN_CMD").ok();
        let use_test_command = test_cmd.is_some();
        let (claude_cmd, claude_args): (String, Vec<String>) = if let Some(override_cmd) = test_cmd
        {
            (override_cmd, Vec::new())
        } else {
            (
                crate::claude::launcher::claude_command().to_string(),
                crate::claude::launcher::claude_args()
                    .into_iter()
                    .map(String::from)
                    .collect(),
            )
        };
        let spawn_cfg = crate::session::SpawnConfig {
            label: clean_label.clone(),
            working_dir: cwd,
            command: &claude_cmd,
            args: claude_args,
            event_tx,
            cols,
            rows,
            install_hook: !use_test_command,
            mcp_port: if use_test_command {
                None
            } else {
                self.mcp_port
            },
        };

        // 6. Sanitize the initial_prompt before subscribing/spawning so
        //    we can bail early without side-effects. Subscribe to the
        //    bus *before* spawning to close the TOCTOU window between
        //    spawn and subscribe — the session could become Idle in that
        //    gap and we'd miss the event.
        let clean_prompt: Option<String> = match args.initial_prompt.as_deref() {
            Some(prompt) => match sanitize_prompt_text(prompt) {
                Ok(clean) => Some(clean),
                Err(reason) => {
                    log::warn!("spawn_session: initial_prompt rejected by sanitizer: {reason}");
                    None
                }
            },
            None => None,
        };
        // Only subscribe for the Idle wait when running against a real
        // Claude binary. The test command override (`/bin/cat`) never
        // produces PTY output that would trigger the Idle transition, so
        // waiting would always time out and slow every test by 30s.
        let prompt_rx =
            (clean_prompt.is_some() && !use_test_command).then(|| self.ctx.bus.subscribe());

        // 7. Spawn under a single lock acquisition.
        //    `send_prompt` is NOT called here — see step 8 below.
        //
        //    Two-phase note (pr-review-phase-6-tasks-3-to-7.md §B2):
        //    step 3 above holds the sessions mutex across the budget
        //    check-and-decrement, then releases. This lock is a FRESH
        //    acquisition — deliberate, not a TOCTOU slip. Holding the
        //    mutex across the intervening `bridge.request().await` and
        //    the `Session::spawn` PTY call would serialize every
        //    MCP reader behind a potentially multi-second blocking
        //    operation. Budget consistency is preserved by the restore
        //    path below (A1 fix).
        let new_id = {
            let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            match mgr.spawn_with_role(spawn_cfg, None, Some(caller_id)) {
                Ok(id) => id,
                Err(e) => {
                    // Restore the budget if we decremented it in step 3.
                    // Without this, a PTY-spawn failure permanently
                    // consumes a silent spawn credit — the driver ends
                    // up with fewer silent spawns than it should.
                    // pr-review-phase-6-tasks-3-to-7.md §A1 (S4).
                    if let Some((policy, original_budget)) = budget_decremented {
                        mgr.set_role(
                            caller_id,
                            crate::session::SessionRole::Driver {
                                spawn_budget: original_budget,
                                spawn_policy: policy,
                            },
                        );
                    }
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "spawn_session: spawn failed: {e}"
                    ))]));
                }
            }
        };

        // 8. Wait for the new session to reach Idle before sending the
        //    initial_prompt. Without this wait the submit sequence (\r)
        //    races Claude's startup — the text lands in the PTY buffer
        //    before Claude has rendered its input prompt and is ignored.
        //    `Idle` is emitted by `update_statuses` once the session has
        //    had no PTY activity for >5s, which is exactly when Claude
        //    is sitting at its input prompt ready for input.
        //    (Smoke test finding: workers 1+2 showed prompt in input box
        //    but unsubmitted; only the last worker — already past the
        //    race window — submitted correctly.)
        if let Some((rx, clean)) = prompt_rx.zip(clean_prompt) {
            const IDLE_WAIT_SECS: u64 = 30;
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(IDLE_WAIT_SECS);
            let mut became_idle = false;
            'wait: while std::time::Instant::now() < deadline {
                while let Ok(ev) = rx.try_recv() {
                    if matches!(
                        &ev,
                        crate::session::SessionEvent::StatusChanged {
                            session_id,
                            status: crate::session::SessionStatus::Idle,
                        } if *session_id == new_id
                    ) {
                        became_idle = true;
                        break 'wait;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            if !became_idle {
                log::warn!(
                    "spawn_session({new_id}): did not become Idle within {IDLE_WAIT_SECS}s, \
                     sending initial_prompt anyway"
                );
            }
            let mut mgr = self.ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = mgr.send_prompt(new_id, &clean) {
                log::warn!("spawn_session({new_id}): initial_prompt send failed: {e}");
            }
        }

        let wire = SpawnSessionWire {
            session_id: new_id,
            label: clean_label,
        };
        let json = serde_json::to_string(&wire)
            .map_err(|e| McpError::internal_error(format!("spawn_session serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }
}

#[tool_handler]
impl ServerHandler for Ccom {
    fn get_info(&self) -> ServerInfo {
        // `ServerInfo` is `#[non_exhaustive]` in rmcp 1.4 — must
        // mutate a `::default()` instance in place, not use struct
        // literal syntax or `..Default::default()`.
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        // The `subscribe` tool pushes `notifications/message`
        // entries via `peer.notify_logging_message`, so we must
        // advertise the `logging` capability — conformant clients
        // are allowed to ignore logging notifications otherwise.
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_logging()
            .build();
        info.server_info = Implementation::from_build_env();
        info.instructions =
            Some("ccom (Claude Commander) — read-only session inspection tools".into());
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EventBus, Session, SessionManager};
    use std::sync::Mutex;

    fn test_ctx() -> Arc<McpCtx> {
        let bus = Arc::new(EventBus::new());
        let mgr = SessionManager::with_bus(Arc::clone(&bus));
        Arc::new(McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        })
    }

    fn test_ctx_with_sessions() -> Arc<McpCtx> {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
        let id_a = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_a, "alpha"));
        let id_b = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_b, "beta"));
        Arc::new(McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
            confirm: None,
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        })
    }

    // The macro-generated tool dispatch routes through the rmcp
    // router, so the `#[tool]`-annotated methods aren't directly
    // invocable outside a full MCP round-trip. These tests exercise
    // the ctx-level methods the tool bodies call; the end-to-end
    // round-trip is covered by the Task 8 integration test.

    #[tokio::test]
    async fn list_sessions_empty_returns_empty_vec() {
        let ccom = Ccom::new(test_ctx());
        let summaries = ccom.ctx.list_sessions();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn list_sessions_populated_has_labels() {
        let ccom = Ccom::new(test_ctx_with_sessions());
        let summaries = ccom.ctx.list_sessions();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].label, "alpha");
        assert_eq!(summaries[1].label, "beta");
    }

    #[tokio::test]
    async fn read_response_missing_turn_returns_none() {
        // Sanity check that the ctx returns None when the requested
        // turn doesn't exist — the long-poll path of `read_response`
        // relies on this to fall through into the bus wait.
        let ctx = test_ctx_with_sessions();
        let result = ctx.get_response(1, Some(crate::session::TurnId::new(0)));
        assert!(result.is_none());
    }

    /// Review T2: exercise the `read_response` long-poll **success**
    /// path. The fast path is covered by the populated-manager test
    /// above, and the timeout path is covered by the integration test
    /// in `tests/mcp_readonly.rs`. This test plugs the hole in the
    /// middle — when a turn arrives on the bus mid-wait, the ctx
    /// recheck returns the stored body.
    ///
    /// We can't directly invoke the `#[tool]`-wrapped `read_response`
    /// method (the rmcp macros route through the router), so this
    /// test reproduces the handler's subscribe → recheck sequence
    /// against a real `SessionManager` driven by the synthetic
    /// boundary detector.
    #[tokio::test]
    async fn read_response_long_poll_success_via_bus_wakeup() {
        use crate::pty::response_boundary::ResponseBoundaryDetector;
        use regex::Regex;

        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
        mgr.set_boundary_detector_for_test(ResponseBoundaryDetector::new(
            Regex::new(r"## DONE").unwrap(),
        ));

        // Spawn a real PTY-less stand-in and send a prompt to
        // allocate a turn. `/bin/cat` echoes stdin so the detector
        // will see the marker when we feed it.
        let (raw_tx, _rx) = std::sync::mpsc::channel();
        let event_tx = crate::event::MonitoredSender::wrap(raw_tx);
        let id = mgr
            .spawn(crate::session::SpawnConfig {
                label: "t2".to_string(),
                working_dir: std::path::PathBuf::from("/tmp"),
                command: "/bin/cat",
                args: vec![],
                event_tx,
                cols: 80,
                rows: 24,
                install_hook: false,
                mcp_port: None,
            })
            .expect("spawn");
        let turn_id = mgr.send_prompt(id, "ping").expect("send_prompt");

        let ctx = Arc::new(McpCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus: Arc::clone(&bus),
            confirm: None,
            attachments: Arc::new(Mutex::new(std::collections::HashMap::new())),
            event_tx: None,
        });

        // Before the turn is pushed, ctx.get_response returns None —
        // the handler would fall into the long-poll branch.
        assert!(ctx.get_response(id, Some(turn_id)).is_none());

        // Subscribe first (mirrors the handler's TOCTOU-safe
        // ordering).
        let rx = ctx.bus.subscribe();

        // Feed bytes containing the synthetic marker. The detector
        // pushes the `StoredTurn` into the session's response store
        // BEFORE publishing `ResponseComplete` on the bus.
        {
            let mut mgr = ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
            mgr.feed_pty_data(id, b"hi there\n## DONE\n");
            mgr.check_response_boundaries();
        }

        // Drain the bus and find the ResponseComplete for our turn.
        let mut saw_complete = false;
        while let Ok(ev) = rx.try_recv() {
            if let crate::session::SessionEvent::ResponseComplete {
                session_id,
                turn_id: tid,
            } = ev
                && session_id == id
                && tid == turn_id
            {
                saw_complete = true;
                break;
            }
        }
        assert!(saw_complete, "expected ResponseComplete on bus");

        // The stored turn must be visible — this is the exact recheck
        // the handler does after observing the event.
        let stored = ctx
            .get_response(id, Some(turn_id))
            .expect("turn must be stored after ResponseComplete fires");
        assert!(stored.completed_at.is_some());
        let wire = StoredTurnWire::from(&stored);
        assert_eq!(wire.turn_id, turn_id.0);
        assert!(wire.completed);
        assert!(
            wire.body.contains("hi there"),
            "body should contain the prompt echo, got: {:?}",
            wire.body
        );

        // Clean up the real session.
        let mut mgr = ctx.sessions.lock().unwrap_or_else(|p| p.into_inner());
        mgr.kill(id);
    }

    #[test]
    fn session_event_wire_type_names_match_filter_convention() {
        // The filter in `subscribe` compares against lowercased
        // snake_case type names. This test pins that mapping so a
        // rename in `SessionEvent` can't silently break filters.
        use crate::session::{SessionEvent, SessionStatus, TurnId};
        let cases: Vec<(SessionEvent, &str, usize)> = vec![
            (
                SessionEvent::Spawned {
                    session_id: 1,
                    label: "a".into(),
                },
                "spawned",
                1,
            ),
            (
                SessionEvent::PromptSubmitted {
                    session_id: 2,
                    turn_id: TurnId::new(0),
                },
                "prompt_submitted",
                2,
            ),
            (
                SessionEvent::ResponseComplete {
                    session_id: 3,
                    turn_id: TurnId::new(1),
                },
                "response_complete",
                3,
            ),
            (
                SessionEvent::PromptPending {
                    session_id: 4,
                    kind: "YesNo".into(),
                },
                "prompt_pending",
                4,
            ),
            (
                SessionEvent::Exited {
                    session_id: 5,
                    code: 0,
                },
                "exited",
                5,
            ),
            (
                SessionEvent::StatusChanged {
                    session_id: 6,
                    status: SessionStatus::Idle,
                },
                "status_changed",
                6,
            ),
        ];
        for (ev, expected_name, expected_session) in cases {
            let wire = SessionEventWire::from(&ev);
            assert_eq!(wire.type_name(), expected_name, "event: {ev:?}");
            assert_eq!(wire.session_id(), expected_session);
        }
    }

    #[test]
    fn session_event_wire_serializes_with_type_discriminator() {
        use crate::session::{SessionEvent, TurnId};
        let wire = SessionEventWire::from(&SessionEvent::ResponseComplete {
            session_id: 42,
            turn_id: TurnId::new(7),
        });
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains("\"type\":\"response_complete\""));
        assert!(json.contains("\"session_id\":42"));
        assert!(json.contains("\"turn_id\":7"));
    }

    #[test]
    fn send_prompt_returns_not_found_for_unknown_session() {
        // Ctx-level scope check — the handler body short-circuits on
        // this and never touches the PTY.
        let ctx = test_ctx();
        match ctx.send_prompt(999, "hi") {
            Err(SendPromptRejection::NotFound) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn send_prompt_sanitizer_rejects_empty_before_dispatch() {
        // The handler calls sanitize_prompt_text first; if it
        // rejects, ctx.send_prompt is never invoked. Verify the
        // sanitizer layer directly since the #[tool]-wrapped method
        // isn't directly callable.
        assert!(sanitize_prompt_text("").is_err());
        assert!(sanitize_prompt_text("\x01\x02").is_err());
    }

    #[test]
    fn stored_turn_wire_projection_strips_instants() {
        // Build a `StoredTurn` via its public constructor path and
        // verify `StoredTurnWire::from` produces the expected flat
        // shape. This test is isolated from the session manager so
        // it doesn't depend on bus wiring.
        use crate::session::TurnId;
        let turn = crate::session::StoredTurn {
            turn_id: TurnId::new(7),
            body: "hello".to_string(),
            started_at: std::time::Instant::now(),
            completed_at: Some(std::time::Instant::now()),
        };
        let wire = StoredTurnWire::from(&turn);
        assert_eq!(wire.turn_id, 7);
        assert_eq!(wire.body, "hello");
        assert!(wire.completed);

        // JSON round-trip sanity.
        let json = serde_json::to_string(&wire).unwrap();
        assert!(json.contains("\"turn_id\":7"));
        assert!(json.contains("\"body\":\"hello\""));
        assert!(json.contains("\"completed\":true"));
    }
}
