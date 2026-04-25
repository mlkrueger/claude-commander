const REPO: &str = "mlkrueger/claude-commander";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const ARTIFACT_LABEL: &str = "macos-arm64";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const ARTIFACT_LABEL: &str = "macos-x86_64";
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const ARTIFACT_LABEL: &str = "linux-x86_64";
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const ARTIFACT_LABEL: &str = "linux-arm64";
#[cfg(not(any(
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
)))]
const ARTIFACT_LABEL: &str = "";

/// Returns true when the running binary lives inside a Homebrew Cellar.
/// In that case, the user should `brew upgrade ccom` rather than self-updating.
pub fn is_homebrew_install() -> bool {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().contains("/Cellar/"))
        .unwrap_or(false)
}

/// Spawn a background thread that checks GitHub for a newer release.
/// The receiver yields `Some(tag)` if an update exists, `None` if already current,
/// and never sends if the check fails silently (network error, etc.).
pub fn spawn_update_check() -> std::sync::mpsc::Receiver<Option<String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check_for_update());
    });
    rx
}

/// Spawn a background thread that downloads and installs `version`.
/// The receiver yields `Ok(())` on success or `Err(msg)` on failure.
pub fn spawn_install(version: String) -> std::sync::mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(download_and_install(&version));
    });
    rx
}

fn check_for_update() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let text = ureq::get(&url)
        .set("User-Agent", &format!("ccom/{CURRENT_VERSION}"))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let body: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tag = body.get("tag_name")?.as_str()?;
    let latest = tag.trim_start_matches('v');
    if is_newer(latest, CURRENT_VERSION) {
        Some(tag.to_string())
    } else {
        None
    }
}

fn download_and_install(version: &str) -> Result<(), String> {
    if ARTIFACT_LABEL.is_empty() {
        return Err("unsupported platform for auto-update".to_string());
    }

    let url = format!(
        "https://github.com/{REPO}/releases/download/{version}/ccom-{ARTIFACT_LABEL}.tar.gz"
    );
    let reader = ureq::get(&url)
        .set("User-Agent", &format!("ccom/{CURRENT_VERSION}"))
        .call()
        .map_err(|e| format!("download failed: {e}"))?
        .into_reader();

    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;
    let bin_dir = current_exe
        .parent()
        .ok_or("cannot determine binary directory")?;

    let gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);

    // Extract both binaries. We must iterate all entries in one pass
    // because the archive is a streaming reader.
    let targets = ["ccom", "ccom-hook-pretooluse"];
    let mut extracted: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in archive
        .entries()
        .map_err(|e| format!("archive error: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("entry error: {e}"))?;
        let name = entry
            .path()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();

        if targets.contains(&name.as_str()) {
            let dest = bin_dir.join(&name);
            let temp = dest.with_extension("update-tmp");
            entry
                .unpack(&temp)
                .map_err(|e| format!("unpack of {name} failed: {e}"))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&temp) {
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    let _ = std::fs::set_permissions(&temp, perms);
                }
            }

            std::fs::rename(&temp, &dest)
                .map_err(|e| format!("install of {name} failed (try running as root?): {e}"))?;

            extracted.insert(name);
        }
    }

    if !extracted.contains("ccom") {
        return Err("binary 'ccom' not found in archive".to_string());
    }

    Ok(())
}

/// Returns true when `latest` is a higher semver than `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> (u32, u32, u32) {
        let mut it = v.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }
    parse(latest) > parse(current)
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn newer_minor() {
        assert!(is_newer("0.4.0", "0.3.1"));
    }

    #[test]
    fn newer_patch() {
        assert!(is_newer("0.3.2", "0.3.1"));
    }

    #[test]
    fn same_version_not_newer() {
        assert!(!is_newer("0.3.1", "0.3.1"));
    }

    #[test]
    fn older_not_newer() {
        assert!(!is_newer("0.3.0", "0.3.1"));
    }

    #[test]
    fn major_bump() {
        assert!(is_newer("1.0.0", "0.99.99"));
    }
}
