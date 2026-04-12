//! Read-only MCP tool handlers.
//!
//! All three read-only tools (`list_sessions`, `read_response`,
//! `subscribe`) live in this module as `#[tool]`-annotated methods
//! on the `Ccom` handler struct. Each MCP session gets its own
//! `Ccom` instance via the factory closure in
//! [`super::server::run_server`]; the instance holds an
//! `Arc<ReadOnlyCtx>` for shared state.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};

use super::state::ReadOnlyCtx;

/// Per-MCP-session tool handler. Constructed by the factory closure
/// in [`super::server::run_server`] on every new MCP session, so
/// instance-level state here is per-session (not shared across
/// sessions). Cross-session state lives in [`ReadOnlyCtx`].
#[derive(Clone)]
pub struct Ccom {
    /// Shared ctx used by all tool handlers. `Arc<>` so the clone
    /// inside the factory closure is cheap.
    pub(super) ctx: Arc<ReadOnlyCtx>,
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
    pub(super) fn new(ctx: Arc<ReadOnlyCtx>) -> Self {
        Self {
            ctx,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "List all sessions ccom is currently managing. \
                       Returns a JSON array of SessionSummary objects \
                       with id, label, working_dir, status, last_activity_secs, \
                       and context_percent.")]
    async fn list_sessions(&self) -> Result<CallToolResult, McpError> {
        let summaries = self.ctx.list_sessions();
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
    ) -> Result<CallToolResult, McpError> {
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

        Err(McpError::internal_error(
            format!(
                "read_response timeout after {}s waiting for session {} turn {:?}",
                timeout.as_secs(),
                args.session_id,
                args.turn_id,
            ),
            None,
        ))
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

    fn test_ctx() -> Arc<ReadOnlyCtx> {
        let bus = Arc::new(EventBus::new());
        let mgr = SessionManager::with_bus(Arc::clone(&bus));
        Arc::new(ReadOnlyCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
        })
    }

    fn test_ctx_with_sessions() -> Arc<ReadOnlyCtx> {
        let bus = Arc::new(EventBus::new());
        let mut mgr = SessionManager::with_bus(Arc::clone(&bus));
        let id_a = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_a, "alpha"));
        let id_b = mgr.peek_next_id();
        mgr.push_for_test(Session::dummy_exited(id_b, "beta"));
        Arc::new(ReadOnlyCtx {
            sessions: Arc::new(Mutex::new(mgr)),
            bus,
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
