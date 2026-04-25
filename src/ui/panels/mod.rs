pub mod command_bar;
pub mod file_tree;
pub mod session_detail;
pub mod session_list;
pub mod session_picker;
pub mod session_tree;
pub mod session_view;
pub mod usage_graph;

use crate::session::{SessionRole, SpawnPolicy};

/// Render the driver-specific suffix for a session title/row, e.g.
/// ` [driver · budget 3]`. Returns an empty string for non-driver
/// sessions. Phase 6 Task 5/7 — single source of truth for the
/// driver badge format so session list rows and session view title
/// bars stay in sync (pr-review-phase-6-tasks-3-to-7.md §C1/C5).
pub fn driver_role_suffix(role: &SessionRole) -> String {
    let SessionRole::Driver {
        spawn_budget,
        spawn_policy,
    } = role
    else {
        return String::new();
    };
    match spawn_policy {
        SpawnPolicy::Budget => format!(" [driver · budget {spawn_budget}]"),
        SpawnPolicy::Ask => " [driver · ask]".to_string(),
        SpawnPolicy::Trust => " [driver · trust]".to_string(),
    }
}
