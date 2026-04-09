use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GitFileStatus {
    Modified,
    Staged,
    StagedModified, // staged + working tree changes
    Untracked,
    Added,
    Deleted,
    Renamed,
    Conflict,
}

impl GitFileStatus {
    pub fn indicator(&self) -> &'static str {
        match self {
            GitFileStatus::Modified => "M",
            GitFileStatus::Staged => "S",
            GitFileStatus::StagedModified => "SM",
            GitFileStatus::Untracked => "?",
            GitFileStatus::Added => "A",
            GitFileStatus::Deleted => "D",
            GitFileStatus::Renamed => "R",
            GitFileStatus::Conflict => "!",
        }
    }

    pub fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            GitFileStatus::Modified => Color::Yellow,
            GitFileStatus::Staged | GitFileStatus::Added => Color::Green,
            GitFileStatus::StagedModified => Color::Cyan,
            GitFileStatus::Untracked => Color::DarkGray,
            GitFileStatus::Deleted => Color::Red,
            GitFileStatus::Renamed => Color::Magenta,
            GitFileStatus::Conflict => Color::Red,
        }
    }
}

/// Map of absolute file path -> git status
pub type GitStatusMap = HashMap<PathBuf, GitFileStatus>;

/// Run `git status --porcelain=v1` in the given directory and parse results.
/// Returns None if not a git repo.
pub fn get_git_status(dir: &Path) -> Option<GitStatusMap> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-uall"])
        .current_dir(dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // Not a git repo or error
    }

    // Get the repo root so we can build absolute paths
    let root_output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;

    let repo_root = PathBuf::from(String::from_utf8_lossy(&root_output.stdout).trim());

    let mut map = HashMap::new();
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let index = line.as_bytes()[0];
        let worktree = line.as_bytes()[1];
        let file_path = &line[3..];

        // Handle renames: "R  old -> new"
        let file_path = if let Some(pos) = file_path.find(" -> ") {
            &file_path[pos + 4..]
        } else {
            file_path
        };

        let abs_path = repo_root.join(file_path);

        let status = match (index, worktree) {
            (b'?', b'?') => GitFileStatus::Untracked,
            (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => GitFileStatus::Conflict,
            (b'A', b' ') => GitFileStatus::Added,
            (b'A', b'M') | (b'M', b'M') => GitFileStatus::StagedModified,
            (b'D', b' ') | (b' ', b'D') => GitFileStatus::Deleted,
            (b'R', _) => GitFileStatus::Renamed,
            (b'M', b' ') => GitFileStatus::Staged,
            (b' ', b'M') => GitFileStatus::Modified,
            _ => GitFileStatus::Modified, // fallback
        };

        map.insert(abs_path, status);
    }

    Some(map)
}

/// Check if a directory contains any files with the given statuses
pub fn dir_has_changes(dir: &Path, status_map: &GitStatusMap) -> Option<GitFileStatus> {
    let mut worst: Option<GitFileStatus> = None;
    for (path, status) in status_map {
        if path.starts_with(dir) {
            let priority = |s: &GitFileStatus| -> u8 {
                match s {
                    GitFileStatus::Conflict => 7,
                    GitFileStatus::StagedModified => 6,
                    GitFileStatus::Modified => 5,
                    GitFileStatus::Staged => 4,
                    GitFileStatus::Added => 3,
                    GitFileStatus::Deleted => 3,
                    GitFileStatus::Renamed => 2,
                    GitFileStatus::Untracked => 1,
                }
            };
            match &worst {
                None => worst = Some(*status),
                Some(current) if priority(status) > priority(current) => worst = Some(*status),
                _ => {}
            }
        }
    }
    worst
}
