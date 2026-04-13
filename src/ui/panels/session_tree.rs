//! Phase 6 Task 7: session tree builder.
//!
//! Groups a flat `SessionManager` slice into a tree for the session
//! list panel: drivers at top level, their spawned children + explicit
//! attachments indented beneath. Orphaned children (parent not in the
//! manager — e.g. driver exited and was reaped) render at top level.
//!
//! Pure-Rust logic — no ratatui, no locks. The session list panel
//! snapshots the shared attachment map before calling `build_session_tree`
//! so this module stays testable against `Session::dummy_exited` fixtures.

use std::collections::{HashMap, HashSet};

use crate::session::{Session, SessionRole};

/// A single row in the rendered session tree. Ordering in the returned
/// `Vec` matches visual order top-to-bottom.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRow {
    /// A driver session — rendered with `◆ ` prefix at top level.
    Driver { index: usize },
    /// A child of a driver. `parent_index` points at the driver's
    /// manager slice index so the renderer can look up the parent's
    /// label for the dim suffix. `attached` distinguishes a session
    /// that was explicitly attached (`↪ `) from one spawned by the
    /// driver (`└─ `).
    Child {
        parent_index: usize,
        index: usize,
        attached: bool,
    },
    /// Top-level non-driver row — either a regular solo session or
    /// an orphaned child whose driver is gone.
    Solo { index: usize },
}

/// Build the tree. Iterates sessions in manager order. For each
/// driver we emit its spawned children first (stable manager order),
/// then any attached session ids not already pulled in as children.
/// Everything else falls through to a top-level `Solo` row.
pub fn build_session_tree(
    sessions: &[Session],
    attachments: &HashMap<usize, HashSet<usize>>,
) -> Vec<TreeRow> {
    // id → slice index, so `spawned_by` and attachment ids can be
    // resolved back to the manager index used by callers.
    let id_to_index: HashMap<usize, usize> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id, i))
        .collect();

    let mut rows: Vec<TreeRow> = Vec::with_capacity(sessions.len());
    // Track which indices have been consumed as rows of some driver's
    // subtree so the final solo pass can skip them.
    let mut consumed: HashSet<usize> = HashSet::new();

    for (driver_idx, session) in sessions.iter().enumerate() {
        if !matches!(session.role, SessionRole::Driver { .. }) {
            continue;
        }
        rows.push(TreeRow::Driver { index: driver_idx });
        consumed.insert(driver_idx);

        // Spawned children in manager order. Collect their ids so we
        // can avoid double-rendering a session that's both a child
        // and an explicit attachment.
        let mut child_ids: HashSet<usize> = HashSet::new();
        for (child_idx, child) in sessions.iter().enumerate() {
            if child.spawned_by == Some(session.id) {
                rows.push(TreeRow::Child {
                    parent_index: driver_idx,
                    index: child_idx,
                    attached: false,
                });
                consumed.insert(child_idx);
                child_ids.insert(child.id);
            }
        }

        // Explicit attachments — stable order by attached-session id so
        // the tree is deterministic under test. (`HashSet` iteration
        // is not, hence the sort.)
        if let Some(attached) = attachments.get(&session.id) {
            let mut ids: Vec<usize> = attached
                .iter()
                .copied()
                .filter(|id| !child_ids.contains(id))
                .collect();
            ids.sort_unstable();
            for id in ids {
                if let Some(&idx) = id_to_index.get(&id) {
                    rows.push(TreeRow::Child {
                        parent_index: driver_idx,
                        index: idx,
                        attached: true,
                    });
                    consumed.insert(idx);
                }
                // Unknown ids silently drop — can happen if the
                // attached session exited and was reaped between the
                // attachment and this render.
            }
        }
    }

    // Everything not yet placed: orphaned children (parent id absent
    // from the manager) and plain solos.
    for (idx, _session) in sessions.iter().enumerate() {
        if !consumed.contains(&idx) {
            rows.push(TreeRow::Solo { index: idx });
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionRole, SpawnPolicy};

    fn driver(id: usize, label: &str) -> Session {
        Session::dummy_exited(id, label).with_role(SessionRole::Driver {
            spawn_budget: 3,
            spawn_policy: SpawnPolicy::Budget,
        })
    }

    fn solo(id: usize, label: &str) -> Session {
        Session::dummy_exited(id, label)
    }

    fn child_of(id: usize, label: &str, parent: usize) -> Session {
        Session::dummy_exited(id, label).with_spawned_by(parent)
    }

    #[test]
    fn empty_manager_returns_empty_tree() {
        let rows = build_session_tree(&[], &HashMap::new());
        assert!(rows.is_empty());
    }

    #[test]
    fn solo_only_returns_flat_list() {
        let sessions = vec![solo(1, "a"), solo(2, "b"), solo(3, "c")];
        let rows = build_session_tree(&sessions, &HashMap::new());
        assert_eq!(
            rows,
            vec![
                TreeRow::Solo { index: 0 },
                TreeRow::Solo { index: 1 },
                TreeRow::Solo { index: 2 },
            ]
        );
    }

    #[test]
    fn single_driver_with_children_groups_correctly() {
        let sessions = vec![
            driver(1, "orch"),
            child_of(2, "kid-a", 1),
            child_of(3, "kid-b", 1),
        ];
        let rows = build_session_tree(&sessions, &HashMap::new());
        assert_eq!(
            rows,
            vec![
                TreeRow::Driver { index: 0 },
                TreeRow::Child {
                    parent_index: 0,
                    index: 1,
                    attached: false
                },
                TreeRow::Child {
                    parent_index: 0,
                    index: 2,
                    attached: false
                },
            ]
        );
    }

    #[test]
    fn attached_session_renders_under_driver() {
        let sessions = vec![driver(1, "orch"), solo(2, "guest")];
        let mut atts = HashMap::new();
        atts.insert(1, HashSet::from([2]));
        let rows = build_session_tree(&sessions, &atts);
        assert_eq!(
            rows,
            vec![
                TreeRow::Driver { index: 0 },
                TreeRow::Child {
                    parent_index: 0,
                    index: 1,
                    attached: true
                },
            ]
        );
    }

    #[test]
    fn attached_and_spawned_child_do_not_double_count() {
        // Session 2 is both spawned by driver 1 and listed in the
        // driver's attachment set. Only the spawned-child row should
        // appear — the attached row must be suppressed.
        let sessions = vec![driver(1, "orch"), child_of(2, "kid", 1)];
        let mut atts = HashMap::new();
        atts.insert(1, HashSet::from([2]));
        let rows = build_session_tree(&sessions, &atts);
        assert_eq!(
            rows,
            vec![
                TreeRow::Driver { index: 0 },
                TreeRow::Child {
                    parent_index: 0,
                    index: 1,
                    attached: false
                },
            ]
        );
    }

    #[test]
    fn orphaned_child_renders_at_top_level() {
        // spawned_by points at id 99 which isn't in the manager —
        // driver exited and was reaped. Child becomes a top-level
        // Solo row.
        let sessions = vec![child_of(2, "orphan", 99), solo(3, "sibling")];
        let rows = build_session_tree(&sessions, &HashMap::new());
        assert_eq!(
            rows,
            vec![TreeRow::Solo { index: 0 }, TreeRow::Solo { index: 1 }]
        );
    }

    #[test]
    fn multiple_drivers_each_get_own_subtree() {
        let sessions = vec![
            driver(1, "orch-a"),
            child_of(2, "kid-a1", 1),
            driver(3, "orch-b"),
            child_of(4, "kid-b1", 3),
            solo(5, "loner"),
        ];
        let rows = build_session_tree(&sessions, &HashMap::new());
        assert_eq!(
            rows,
            vec![
                TreeRow::Driver { index: 0 },
                TreeRow::Child {
                    parent_index: 0,
                    index: 1,
                    attached: false
                },
                TreeRow::Driver { index: 2 },
                TreeRow::Child {
                    parent_index: 2,
                    index: 3,
                    attached: false
                },
                TreeRow::Solo { index: 4 },
            ]
        );
    }
}
