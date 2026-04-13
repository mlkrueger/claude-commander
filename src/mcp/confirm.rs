//! Cross-thread confirmation bridge for MCP write tools.
//!
//! The MCP server runs on a dedicated `ccom-mcp` OS thread inside a
//! current-thread tokio runtime. Write tools like `kill_session`
//! need to block (asynchronously) until the user on the main TUI
//! thread answers a confirmation modal.
//!
//! This module provides [`ConfirmBridge`] — a one-way queue of
//! [`ConfirmRequest`]s from MCP handlers to the main thread, each
//! carrying a `tokio::sync::oneshot::Sender` the main thread uses
//! to send back the user's [`ConfirmResponse`].
//!
//! Concurrency model:
//! - MCP → main: `std::sync::mpsc::Sender` wrapped in `Mutex` so it
//!   can be shared across tokio tasks via `Arc<ConfirmBridge>`.
//!   `mpsc::Sender::send` takes `&self`, but we hold an immutable
//!   reference to the bridge via `Arc` and must mediate concurrent
//!   access to the sender. The mutex is held only for the duration
//!   of a single `send()` call — **never across an `.await`** — so
//!   there is no deadlock risk.
//! - Main → MCP: `tokio::sync::oneshot` — the handler `.await`s on
//!   the receiver after putting the sender into the request.
//!
//! Error semantics:
//! - If the `mpsc::Sender::send` fails (the main thread dropped the
//!   receiver), the handler sees `ConfirmResponse::Deny`. This is
//!   the "TUI already exited" case.
//! - If the oneshot resolves with `Err` (the main thread dropped
//!   the `ConfirmRequest` without responding), the handler also
//!   sees `ConfirmResponse::Deny` and logs a warning.
//!
//! The bridge is not integrated into `McpCtx` yet — that wiring
//! happens downstream when Task 3 lands the `kill_session` handler.
//! Until that landing, nothing in the crate constructs a
//! `ConfirmBridge` outside of tests, so the public items below look
//! dead to the compiler. The `#![allow(dead_code)]` below suppresses
//! those warnings for this module only; the integration pass will
//! remove the allow once the types are wired up.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, mpsc};

use tokio::sync::oneshot;

/// Which write tool is requesting confirmation.
///
/// Phase 5 only uses [`ConfirmTool::KillSession`]; [`ConfirmTool::SendPrompt`]
/// is reserved for a future phase where prompt gating may become a
/// user-configurable policy. Design doc §6 currently treats prompt
/// gating as theater, but we leave room in the type space in case
/// that call reverses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmTool {
    /// Reserved for a future phase — see enum docs. Phase 5 does not
    /// gate `send_prompt`, but the type space leaves room in case
    /// that policy reverses.
    SendPrompt,
    KillSession,
    /// Phase 6 Task 3: a driver called `spawn_session` and its
    /// `SpawnPolicy` requires user confirmation (either `Ask`, or
    /// `Budget` after the budget was exhausted). The modal should
    /// show the proposed child label and working dir.
    SpawnSession,
}

/// Response from the user after seeing the modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmResponse {
    Allow,
    Deny,
}

/// A pending confirmation request crossing from the ccom-mcp thread
/// to the main TUI thread. The main thread resolves it by calling
/// `resp_tx.send(...)`; if the request is dropped without a response
/// the MCP-side awaiter observes `RecvError` and treats it as a
/// denial.
pub struct ConfirmRequest {
    pub tool: ConfirmTool,
    pub session_id: usize,
    pub resp_tx: oneshot::Sender<ConfirmResponse>,
}

/// Cross-thread confirmation bridge. See module docs.
pub struct ConfirmBridge {
    tx: Mutex<mpsc::Sender<ConfirmRequest>>,
}

impl ConfirmBridge {
    /// Construct a new bridge. Returns an `Arc<Self>` for the MCP
    /// side and the raw `Receiver` for the main thread to drain.
    pub fn new() -> (Arc<Self>, mpsc::Receiver<ConfirmRequest>) {
        let (tx, rx) = mpsc::channel();
        let bridge = Arc::new(Self { tx: Mutex::new(tx) });
        (bridge, rx)
    }

    /// Request confirmation for a write tool. Sends a request to
    /// the main thread and awaits the user's decision.
    ///
    /// Returns [`ConfirmResponse::Deny`] if the request cannot be
    /// delivered (receiver dropped — the TUI has exited) or if the
    /// main thread drops the request without sending a response.
    /// Never panics.
    pub async fn request(&self, tool: ConfirmTool, session_id: usize) -> ConfirmResponse {
        let (resp_tx, resp_rx) = oneshot::channel();
        let req = ConfirmRequest {
            tool,
            session_id,
            resp_tx,
        };

        // Scope the mutex guard so it is dropped before the await.
        // `mpsc::Sender::send` is synchronous and never blocks, so
        // holding the guard here is brief.
        {
            let guard = self.tx.lock().unwrap_or_else(|p| p.into_inner());
            if guard.send(req).is_err() {
                // Receiver dropped — no one can service the request.
                return ConfirmResponse::Deny;
            }
        }

        match resp_rx.await {
            Ok(resp) => resp,
            Err(_) => {
                log::warn!(
                    "mcp confirm: responder dropped without answering (tool={:?}, session_id={})",
                    tool,
                    session_id,
                );
                ConfirmResponse::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[tokio::test]
    async fn request_and_resolve_allow() {
        let (bridge, rx) = ConfirmBridge::new();

        // Drain one request on a worker thread and resolve Allow.
        let drain = thread::spawn(move || {
            let req = rx.recv().expect("request delivered");
            assert_eq!(req.tool, ConfirmTool::KillSession);
            assert_eq!(req.session_id, 7);
            let _ = req.resp_tx.send(ConfirmResponse::Allow);
        });

        let resp = bridge.request(ConfirmTool::KillSession, 7).await;
        assert_eq!(resp, ConfirmResponse::Allow);
        drain.join().unwrap();
    }

    #[tokio::test]
    async fn request_and_resolve_deny() {
        let (bridge, rx) = ConfirmBridge::new();

        let drain = thread::spawn(move || {
            let req = rx.recv().expect("request delivered");
            let _ = req.resp_tx.send(ConfirmResponse::Deny);
        });

        let resp = bridge.request(ConfirmTool::KillSession, 1).await;
        assert_eq!(resp, ConfirmResponse::Deny);
        drain.join().unwrap();
    }

    #[tokio::test]
    async fn request_returns_deny_when_receiver_dropped_before_request() {
        let (bridge, rx) = ConfirmBridge::new();
        drop(rx);

        let resp = bridge.request(ConfirmTool::KillSession, 42).await;
        assert_eq!(resp, ConfirmResponse::Deny);
    }

    #[tokio::test]
    async fn request_returns_deny_when_responder_drops_oneshot_without_answering() {
        let (bridge, rx) = ConfirmBridge::new();

        let drain = thread::spawn(move || {
            let req = rx.recv().expect("request delivered");
            // Drop the request (and its resp_tx) without answering.
            drop(req);
        });

        let resp = bridge.request(ConfirmTool::KillSession, 3).await;
        assert_eq!(resp, ConfirmResponse::Deny);
        drain.join().unwrap();
    }

    #[test]
    fn bridge_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ConfirmBridge>();
        assert_send_sync::<Arc<ConfirmBridge>>();
        assert_send_sync::<ConfirmRequest>();
    }

    #[tokio::test]
    async fn concurrent_requests_are_serialized_through_mutex_but_do_not_deadlock() {
        let (bridge, rx) = ConfirmBridge::new();

        // Drain 4 requests, resolving each with Allow.
        let drain = thread::spawn(move || {
            for _ in 0..4 {
                let req = rx.recv().expect("request delivered");
                let _ = req.resp_tx.send(ConfirmResponse::Allow);
            }
        });

        let mut handles = Vec::new();
        for id in 0..4 {
            let b = Arc::clone(&bridge);
            handles.push(tokio::spawn(async move {
                b.request(ConfirmTool::KillSession, id).await
            }));
        }

        for h in handles {
            assert_eq!(h.await.unwrap(), ConfirmResponse::Allow);
        }
        drain.join().unwrap();
    }
}
