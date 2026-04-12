use std::fs;
use std::path::PathBuf;

/// A configuration item that ccom needs to function fully.
#[derive(Debug, Clone)]
pub struct SetupItem {
    pub name: String,
    pub description: String,
    pub status: SetupStatus,
    /// The prompt to send to a Claude Code session to fix this item.
    pub fix_prompt: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupStatus {
    Ok,
    Missing,
}

/// Check all required configurations and return items that need attention.
pub fn check_setup() -> Vec<SetupItem> {
    vec![check_statusline_hook()]
}

/// Returns only items that are missing.
pub fn missing_items() -> Vec<SetupItem> {
    check_setup()
        .into_iter()
        .filter(|i| i.status == SetupStatus::Missing)
        .collect()
}

fn claude_settings_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".claude").join("settings.json"))
}

fn check_statusline_hook() -> SetupItem {
    let status = match claude_settings_path().and_then(|p| fs::read_to_string(p).ok()) {
        Some(contents) => {
            if contents.contains("\"statusLine\"") && contents.contains("ccom-statusline") {
                SetupStatus::Ok
            } else {
                SetupStatus::Missing
            }
        }
        None => SetupStatus::Missing,
    };

    let script_path = find_statusline_script();

    SetupItem {
        name: "Statusline hook".to_string(),
        description: "Writes rate limit data for quota display".to_string(),
        status,
        fix_prompt: format!(
            concat!(
                "I need you to set up the ccom statusline hook in my Claude Code settings. ",
                "Please do the following:\n\n",
                "1. Read ~/.claude/settings.json\n",
                "2. Add a \"statusLine\" field with this value:\n",
                "   ```json\n",
                "   \"statusLine\": {{\n",
                "     \"command\": \"{script_path}\"\n",
                "   }}\n",
                "   ```\n",
                "3. Write the updated settings.json back\n\n",
                "The script extracts rate limit data from the statusline JSON and writes it to ",
                "~/.claude/ccom-rate-limits.json for the ccom TUI to read.\n\n",
                "Important: preserve all existing settings — only add the statusLine field.",
            ),
            script_path = script_path,
        ),
    }
}

/// Path to the marker file that indicates setup has been completed at least once.
fn initialized_marker() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("ccom")
        .join(".initialized")
}

/// Returns true if this is the first launch (no initialized marker exists).
pub fn is_first_launch() -> bool {
    !initialized_marker().exists()
}

/// Mark setup as complete so future launches skip the setup screen.
pub fn mark_initialized() {
    let path = initialized_marker();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, "");
}

/// Find the statusline script, checking common locations.
fn find_statusline_script() -> String {
    // Check relative to the ccom binary
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join("scripts").join("ccom-statusline.sh");
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
        let candidate = parent
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("scripts").join("ccom-statusline.sh"));
        if let Some(c) = candidate
            && c.exists()
        {
            return c.to_string_lossy().to_string();
        }
    }

    // Check in the source repo
    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("scripts").join("ccom-statusline.sh");
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }

    // Fallback: assume it's in the repo
    "~/.local/share/ccom/scripts/ccom-statusline.sh".to_string()
}
