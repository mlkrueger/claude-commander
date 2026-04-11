//! Internal pub/sub bus carrying high-level `SessionEvent`s to multiple
//! subscribers.
//!
//! This is distinct from `crate::event::Event`, which is the raw event-loop
//! channel (keystrokes, ticks, PTY bytes). `SessionEvent` is a structured
//! state-transition signal designed for in-process consumers that want to
//! react to session lifecycle without replaying raw PTY output.
//!
//! Marker-only: `PromptSubmitted` and `ResponseComplete` carry a `TurnId`,
//! not prompt/response bodies. Subscribers that need the body fetch it via
//! `SessionManager::get_response` (added in Phase 3). See
//! `docs/designs/session-management.md` §2 for rationale.

use crate::session::SessionStatus;
use std::sync::{Arc, Mutex, mpsc};

/// Per-session monotonic counter identifying one prompt/response round-trip.
///
/// A new `TurnId` is allocated by `SessionManager::send_prompt` (Phase 2)
/// and paired with a matching `ResponseComplete` from the response
/// boundary detector (Phase 3). Phase 1 only defines the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(pub u64);

/// High-level state transition events published on the `EventBus`.
///
/// Marker-only: no prompt or response bodies ride on events. See the
/// module doc.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// A new session has been spawned and registered with `SessionManager`.
    Spawned { session_id: usize, label: String },
    /// A prompt was submitted to a session via `send_prompt`. The
    /// corresponding body is not carried — fetch via
    /// `SessionManager::get_prompt` (future Phase).
    ///
    /// Constructed by `SessionManager::send_prompt` in Phase 2.
    #[allow(dead_code)]
    PromptSubmitted { session_id: usize, turn_id: TurnId },
    /// The response boundary detector observed the target turn complete.
    /// Fetch the body via `SessionManager::get_response` (Phase 3).
    ///
    /// Constructed by the response boundary detector in Phase 3.
    #[allow(dead_code)]
    ResponseComplete { session_id: usize, turn_id: TurnId },
    /// Claude Code is waiting on an interactive prompt (allow-once,
    /// Y/n, etc.). `kind` is a short label from `PromptDetector`.
    PromptPending { session_id: usize, kind: String },
    /// The session process exited with the given code.
    Exited { session_id: usize, code: i32 },
    /// The session's status field changed (observable via
    /// `Session::status`).
    StatusChanged {
        session_id: usize,
        status: SessionStatus,
    },
}

/// Sync pub/sub bus for `SessionEvent`.
///
/// Fan-out is implemented as one `mpsc::channel` per subscriber. Each
/// subscriber holds its own `Receiver`; the bus holds the paired
/// `Sender`s. `publish` clones the event into every live sender and
/// prunes senders whose receivers have been dropped.
///
/// `EventBus` is `Send + Sync` — clone by wrapping in `Arc`.
pub struct EventBus {
    senders: Arc<Mutex<Vec<mpsc::Sender<SessionEvent>>>>,
}

impl EventBus {
    /// Create an empty bus with no subscribers.
    pub fn new() -> Self {
        Self {
            senders: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Register a new subscriber and return its `Receiver`. The bus
    /// retains the paired `Sender`; future `publish` calls will push
    /// events into the receiver's queue.
    ///
    /// In Phase 1 the only callers are tests and the manual-verification
    /// debug subscriber. Phase 2+ adds production consumers (Council,
    /// MCP server, stats panel).
    #[allow(dead_code)]
    pub fn subscribe(&self) -> mpsc::Receiver<SessionEvent> {
        let (tx, rx) = mpsc::channel();
        let mut senders = self
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        senders.push(tx);
        rx
    }

    /// Clone `event` into every live subscriber. Subscribers whose
    /// `Receiver` has been dropped are pruned. Safe to call
    /// concurrently from multiple threads; safe to call on an empty
    /// bus (no-op).
    pub fn publish(&self, event: SessionEvent) {
        let mut senders = self
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        senders.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::TryRecvError;
    use std::thread;
    use std::time::Duration;

    // -------- TurnId --------

    #[test]
    fn turn_id_constructs_from_u64() {
        let t = TurnId(42);
        assert_eq!(t.0, 42);
    }

    #[test]
    fn turn_id_is_copy_and_eq() {
        let a = TurnId(7);
        let b = a; // Copy
        assert_eq!(a, b);
        assert_eq!(a, TurnId(7));
        assert_ne!(a, TurnId(8));
    }

    #[test]
    fn turn_id_is_ordered() {
        assert!(TurnId(1) < TurnId(2));
        assert!(TurnId(5) > TurnId(4));
        assert!(TurnId(3) <= TurnId(3));
    }

    // -------- SessionEvent --------

    #[test]
    fn session_event_variants_are_constructible() {
        // Smoke test: every variant can be built with its expected fields.
        let _ = SessionEvent::Spawned {
            session_id: 1,
            label: "claude-1".into(),
        };
        let _ = SessionEvent::PromptSubmitted {
            session_id: 1,
            turn_id: TurnId(0),
        };
        let _ = SessionEvent::ResponseComplete {
            session_id: 1,
            turn_id: TurnId(0),
        };
        let _ = SessionEvent::PromptPending {
            session_id: 1,
            kind: "AllowOnce".into(),
        };
        let _ = SessionEvent::Exited {
            session_id: 1,
            code: 0,
        };
        let _ = SessionEvent::StatusChanged {
            session_id: 1,
            status: SessionStatus::Idle,
        };
    }

    #[test]
    fn session_event_clone_is_equal() {
        let a = SessionEvent::PromptSubmitted {
            session_id: 3,
            turn_id: TurnId(7),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    // -------- EventBus: construction & empty-bus behavior --------

    #[test]
    fn new_bus_publish_is_noop() {
        // Publishing on a bus with no subscribers must not panic or
        // block. This is the "bus exists before any subscriber" case —
        // common during `App` startup.
        let bus = EventBus::new();
        bus.publish(SessionEvent::Spawned {
            session_id: 0,
            label: "s0".into(),
        });
    }

    #[test]
    fn default_matches_new() {
        let _: EventBus = EventBus::default();
    }

    // -------- EventBus: single-subscriber delivery --------

    #[test]
    fn subscribe_receives_published_event() {
        let bus = EventBus::new();
        let rx = bus.subscribe();
        let event = SessionEvent::Exited {
            session_id: 4,
            code: 0,
        };
        bus.publish(event.clone());
        assert_eq!(rx.try_recv().unwrap(), event);
    }

    #[test]
    fn subscriber_only_receives_events_published_after_subscribe() {
        // Late subscribers do not see events that fired before they
        // joined — the bus has no replay buffer.
        let bus = EventBus::new();
        bus.publish(SessionEvent::Spawned {
            session_id: 1,
            label: "s1".into(),
        });
        let rx = bus.subscribe();
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    // -------- EventBus: fan-out --------

    #[test]
    fn publish_fans_out_to_all_subscribers() {
        let bus = EventBus::new();
        let rx_a = bus.subscribe();
        let rx_b = bus.subscribe();
        let rx_c = bus.subscribe();

        let event = SessionEvent::StatusChanged {
            session_id: 2,
            status: SessionStatus::Running,
        };
        bus.publish(event.clone());

        assert_eq!(rx_a.try_recv().unwrap(), event);
        assert_eq!(rx_b.try_recv().unwrap(), event);
        assert_eq!(rx_c.try_recv().unwrap(), event);
    }

    #[test]
    fn each_subscriber_has_independent_queue() {
        // One subscriber draining does not affect the others.
        let bus = EventBus::new();
        let rx_a = bus.subscribe();
        let rx_b = bus.subscribe();

        bus.publish(SessionEvent::Exited {
            session_id: 1,
            code: 0,
        });
        bus.publish(SessionEvent::Exited {
            session_id: 2,
            code: 1,
        });

        // Drain A fully.
        assert!(matches!(
            rx_a.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 1, .. }
        ));
        assert!(matches!(
            rx_a.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 2, .. }
        ));
        assert_eq!(rx_a.try_recv(), Err(TryRecvError::Empty));

        // B still has its full backlog.
        assert!(matches!(
            rx_b.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 1, .. }
        ));
        assert!(matches!(
            rx_b.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 2, .. }
        ));
    }

    // -------- EventBus: ordering --------

    #[test]
    fn events_arrive_in_publish_order() {
        let bus = EventBus::new();
        let rx = bus.subscribe();

        for i in 0..5 {
            bus.publish(SessionEvent::Spawned {
                session_id: i,
                label: format!("s{i}"),
            });
        }

        for i in 0..5 {
            match rx.try_recv().unwrap() {
                SessionEvent::Spawned { session_id, .. } => assert_eq!(session_id, i),
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }

    // -------- EventBus: pruning dropped subscribers --------

    #[test]
    fn dropped_receiver_is_pruned_without_panic() {
        // Dropping a receiver must not panic or block subsequent
        // publishes, and remaining subscribers must keep working.
        let bus = EventBus::new();
        let rx_live = bus.subscribe();
        let rx_dead = bus.subscribe();
        drop(rx_dead);

        // First publish encounters the dead sender and must prune it.
        bus.publish(SessionEvent::Exited {
            session_id: 9,
            code: 0,
        });
        assert!(matches!(
            rx_live.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 9, .. }
        ));

        // Second publish must still work for the live subscriber.
        bus.publish(SessionEvent::Exited {
            session_id: 10,
            code: 0,
        });
        assert!(matches!(
            rx_live.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 10, .. }
        ));
    }

    #[test]
    fn all_subscribers_dropped_leaves_empty_bus_functional() {
        let bus = EventBus::new();
        let rx1 = bus.subscribe();
        let rx2 = bus.subscribe();
        drop(rx1);
        drop(rx2);

        // Publish after all subscribers dropped: no panic, no hang.
        bus.publish(SessionEvent::Exited {
            session_id: 0,
            code: 0,
        });

        // Bus is still usable — new subscriber works as expected.
        let rx3 = bus.subscribe();
        bus.publish(SessionEvent::Exited {
            session_id: 1,
            code: 0,
        });
        assert!(matches!(
            rx3.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 1, .. }
        ));
    }

    // -------- EventBus: thread safety --------

    #[test]
    fn bus_is_shareable_across_threads() {
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe();

        let bus_tx = Arc::clone(&bus);
        let handle = thread::spawn(move || {
            bus_tx.publish(SessionEvent::Exited {
                session_id: 99,
                code: 0,
            });
        });
        handle.join().unwrap();

        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionEvent::Exited { session_id: 99, .. }
        ));
    }

    #[test]
    fn concurrent_publishers_deliver_all_events() {
        // Four publisher threads each emit one event; the single
        // subscriber must receive all four (order between threads is
        // unspecified, but the count must be exact).
        let bus = Arc::new(EventBus::new());
        let rx = bus.subscribe();

        let mut handles = Vec::new();
        for i in 0..4 {
            let bus = Arc::clone(&bus);
            handles.push(thread::spawn(move || {
                bus.publish(SessionEvent::Exited {
                    session_id: i,
                    code: 0,
                });
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // Give any pending wakes a moment to settle (still sync
        // channels, so this is belt-and-suspenders).
        thread::sleep(Duration::from_millis(10));

        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let SessionEvent::Exited { session_id, .. } = ev {
                seen.push(session_id);
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2, 3]);
    }
}
