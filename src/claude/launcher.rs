pub fn claude_command() -> &'static str {
    "claude"
}

pub fn claude_args() -> Vec<&'static str> {
    vec![]
}

#[allow(dead_code)]
pub fn claude_fork_args(session_id: &str) -> Vec<String> {
    vec![
        "--resume".to_string(),
        session_id.to_string(),
        "--fork-session".to_string(),
    ]
}

#[allow(dead_code)]
pub fn claude_resume_args(session_id: &str) -> Vec<String> {
    vec!["--resume".to_string(), session_id.to_string()]
}

#[allow(dead_code)]
pub fn find_claude_binary() -> Option<String> {
    // Check common locations
    for path in &[
        "claude",
        "/usr/local/bin/claude",
        "/opt/homebrew/bin/claude",
    ] {
        if which_exists(path) {
            return Some(path.to_string());
        }
    }
    // Check ~/.claude/local/
    let home = dirs::home_dir()?;
    let local_bin = home.join(".claude").join("local").join("claude");
    if local_bin.exists() {
        return Some(local_bin.to_string_lossy().to_string());
    }
    None
}

fn which_exists(cmd: &str) -> bool {
    use std::path::Path;
    if Path::new(cmd).is_absolute() {
        return Path::new(cmd).is_file();
    }
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(cmd))
                .find(|p| p.is_file())
        })
        .is_some()
}
