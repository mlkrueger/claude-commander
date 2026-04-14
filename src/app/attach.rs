//! Phase 6 Task 5: pure-logic helpers for driver attachment-store
//! mutation. Extracted from `App` so unit tests can exercise the
//! attach / idempotency / cleanup semantics without constructing a
//! full `App` (which spins up the embedded MCP server and can't be
//! driven headless).
//!
//! `App::attach_session_to_driver` and
//! `App::scrub_attachments_for_exited` are thin wrappers over these
//! functions that add the lock-acquisition + session-role lookup.

use std::collections::{HashMap, HashSet};

/// Add `target` to `driver`'s attachment set, creating the set if
/// needed. Idempotent — attaching the same pair twice is a no-op.
/// The caller is responsible for verifying that `driver` points at
/// a live driver session; this helper is unconditional so tests can
/// poke it directly.
pub(crate) fn attach(map: &mut HashMap<usize, HashSet<usize>>, driver: usize, target: usize) {
    map.entry(driver).or_default().insert(target);
}

/// Scrub `id` from the attachment store in response to a session
/// exit. If `was_driver` is true, drop the driver's entry entirely
/// — otherwise just remove `id` from every driver's attached set.
pub(crate) fn scrub_on_exit(map: &mut HashMap<usize, HashSet<usize>>, id: usize, was_driver: bool) {
    if was_driver {
        map.remove(&id);
    }
    for attached in map.values_mut() {
        attached.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_session_to_driver_adds_to_set() {
        let mut map: HashMap<usize, HashSet<usize>> = HashMap::new();
        attach(&mut map, 1, 2);
        assert!(map.get(&1).unwrap().contains(&2));
    }

    #[test]
    fn attaching_twice_is_idempotent() {
        let mut map: HashMap<usize, HashSet<usize>> = HashMap::new();
        attach(&mut map, 1, 2);
        attach(&mut map, 1, 2);
        assert_eq!(map.get(&1).unwrap().len(), 1);
    }

    #[test]
    fn driver_exit_clears_attachment_set() {
        let mut map: HashMap<usize, HashSet<usize>> = HashMap::new();
        attach(&mut map, 1, 2);
        attach(&mut map, 1, 3);
        scrub_on_exit(&mut map, 1, true);
        assert!(!map.contains_key(&1));
    }

    #[test]
    fn non_driver_exit_removes_id_from_other_drivers_sets() {
        let mut map: HashMap<usize, HashSet<usize>> = HashMap::new();
        attach(&mut map, 1, 2);
        attach(&mut map, 1, 5);
        attach(&mut map, 4, 5);
        scrub_on_exit(&mut map, 5, false);
        assert!(!map.get(&1).unwrap().contains(&5));
        assert!(map.get(&1).unwrap().contains(&2));
        assert!(map.get(&4).unwrap().is_empty());
    }

    #[test]
    fn attaching_to_unknown_driver_id_via_helper_just_creates_entry() {
        // The `attach` helper is unconditional — the "unknown driver"
        // guard lives on `App::attach_session_to_driver`. This test
        // pins the helper's contract: callers are expected to do the
        // liveness check themselves.
        let mut map: HashMap<usize, HashSet<usize>> = HashMap::new();
        attach(&mut map, 999, 1);
        assert!(map.get(&999).unwrap().contains(&1));
    }
}
