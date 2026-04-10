//! Session state and lifecycle management.
//!
//! This module owns the `Session` concept (a running Claude PTY process plus
//! its display parser and status) and the `SessionManager` that tracks the
//! collection of live sessions for the app.
//!
//! `pty::` handles raw PTY spawn/read/write plumbing; `session::` owns the
//! higher-level state machine (status, attention detection, context usage).

mod manager;
mod types;

pub use manager::{SessionManager, SpawnConfig};
pub use types::{Session, SessionStatus, lock_parser};
