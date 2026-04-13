//! Session state and lifecycle management.
//!
//! This module owns the `Session` concept (a running Claude PTY process plus
//! its display parser and status) and the `SessionManager` that tracks the
//! collection of live sessions for the app.
//!
//! `pty::` handles raw PTY spawn/read/write plumbing; `session::` owns the
//! higher-level state machine (status, attention detection, context usage).

mod events;
pub(crate) mod hook;
mod manager;
mod response_store;
mod types;

pub use events::EventBus;
// Re-exported for Phase 2+ consumers (Council, MCP server, stats
// panel). Phase 1 only constructs these inside the `session` module.
#[allow(unused_imports)]
pub use events::{SessionEvent, TurnId};
pub use manager::{SessionManager, SpawnConfig};
// Phase 3 Task 0 skeleton. Real implementation lands in Task 1.
#[allow(unused_imports)]
pub use response_store::{
    DEFAULT_BUDGET_BYTES, DEFAULT_MIN_RETAIN, ResponseStore, StoredTurn, TurnSink,
};
// Phase 6 Task 1: `SessionRole` + `SpawnPolicy` are re-exported so
// Task 2's driver_config module (and the MCP handlers in Tasks 3-6)
// can reference them through `crate::session::` without reaching into
// the private `types` submodule.
#[allow(unused_imports)]
pub use types::{Session, SessionRole, SessionStatus, SpawnPolicy, lock_parser};
