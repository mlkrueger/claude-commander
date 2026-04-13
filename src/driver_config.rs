// Phase 6 Task 2: driver role config surface.
//
// Three layers of config, resolved in this precedence order:
//   1. CLI flags on `ccom` — `--driver`, `--spawn-policy`, `--budget`
//   2. TOML at `~/.config/claude-commander/driver.toml`
//   3. Hardcoded fallback — `Ask`, budget 0 (strictest; if nothing
//      else says otherwise, the TUI modals every spawn).
//
// The CLI struct from `main.rs` feeds `load_driver_config`, which
// returns a fully resolved `DriverConfig` (or `None` if `--driver`
// was not passed). Missing TOML file is not an error — the file is
// optional. Malformed TOML IS an error — we log it and fall through
// to layer 3 so a typo can't silently flip us from Ask to Trust.
//
// `resolve_driver_config` is the pure core of the module: it takes
// already-extracted CLI values plus an in-memory TOML string and
// returns the resolved config. `load_driver_config` is the thin IO
// wrapper around it that reads the TOML file. Keeping them split
// means the unit tests at the bottom of this file never need to
// touch the filesystem or clap.

use crate::session::{SessionRole, SpawnPolicy};

/// CLI-facing enum that maps 1:1 to `SpawnPolicy`. Kept separate so
/// clap's `ValueEnum` derive doesn't pollute the domain enum in
/// `session::types`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SpawnPolicyArg {
    Ask,
    Budget,
    Trust,
}

impl From<SpawnPolicyArg> for SpawnPolicy {
    fn from(arg: SpawnPolicyArg) -> Self {
        match arg {
            SpawnPolicyArg::Ask => SpawnPolicy::Ask,
            SpawnPolicyArg::Budget => SpawnPolicy::Budget,
            SpawnPolicyArg::Trust => SpawnPolicy::Trust,
        }
    }
}

/// Resolved driver config — `None` (from `resolve_driver_config` /
/// `load_driver_config`) means the current ccom run is not in driver
/// mode, so every spawned session stays `SessionRole::Solo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverConfig {
    pub spawn_policy: SpawnPolicy,
    pub spawn_budget: u32,
}

impl DriverConfig {
    /// Construct the `SessionRole::Driver` variant this config maps to.
    /// Called once per ccom run, right after the first Claude session
    /// lands in `SessionManager::sessions`.
    pub fn to_role(&self) -> SessionRole {
        SessionRole::Driver {
            spawn_budget: self.spawn_budget,
            spawn_policy: self.spawn_policy,
        }
    }
}

// Internal TOML shape. Every field is `Option` so that a partial
// file (e.g. only `budget = 5`, no policy) is valid and still layers
// cleanly on top of CLI overrides.
#[derive(Debug, Default, serde::Deserialize)]
struct TomlDriverSection {
    spawn_policy: Option<String>, // "ask" | "budget" | "trust" (case-insensitive)
    budget: Option<u32>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct TomlRoot {
    driver: Option<TomlDriverSection>,
}

/// Load the driver config with full precedence handling. Arguments
/// are passed in rather than reading them off a `Cli` struct so this
/// function is trivially unit-testable — the tests at the bottom of
/// this file never touch a real filesystem or clap.
///
/// Precedence for each field (policy, budget):
///   CLI flag > TOML value > hardcoded fallback (`Ask`, `0`)
pub fn resolve_driver_config(
    cli_driver: bool,
    cli_policy: Option<SpawnPolicyArg>,
    cli_budget: Option<u32>,
    toml_text: Option<&str>,
) -> Option<DriverConfig> {
    if !cli_driver {
        return None;
    }

    // Parse TOML if provided. Malformed TOML is logged and treated
    // as "no TOML" — we do NOT propagate the error. The strictest
    // fallback (`Ask`, 0) is the safe default on parse failure: it
    // prevents a typo in the config file from silently widening
    // permissions (e.g. accidentally enabling `Trust`).
    let toml_cfg: TomlDriverSection = toml_text
        .and_then(|t| match toml::from_str::<TomlRoot>(t) {
            Ok(r) => r.driver,
            Err(e) => {
                log::warn!("driver.toml parse failed: {e} — falling back to defaults");
                None
            }
        })
        .unwrap_or_default();

    // Policy: CLI > TOML > Ask
    let spawn_policy = if let Some(p) = cli_policy {
        SpawnPolicy::from(p)
    } else if let Some(s) = toml_cfg.spawn_policy.as_deref() {
        match s.to_ascii_lowercase().as_str() {
            "ask" => SpawnPolicy::Ask,
            "budget" => SpawnPolicy::Budget,
            "trust" => SpawnPolicy::Trust,
            other => {
                log::warn!("driver.toml has unknown spawn_policy={other:?} — falling back to Ask");
                SpawnPolicy::Ask
            }
        }
    } else {
        SpawnPolicy::Ask
    };

    // Budget: CLI > TOML > 0
    let spawn_budget = cli_budget.or(toml_cfg.budget).unwrap_or(0);

    Some(DriverConfig {
        spawn_policy,
        spawn_budget,
    })
}

/// Convenience wrapper around `resolve_driver_config` that reads the
/// TOML file from `~/.config/claude-commander/driver.toml` if it
/// exists. Missing file = `None` TOML, NOT an error. IO errors
/// other than NotFound are logged and treated as missing.
pub fn load_driver_config(
    cli_driver: bool,
    cli_policy: Option<SpawnPolicyArg>,
    cli_budget: Option<u32>,
) -> Option<DriverConfig> {
    let toml_text = read_toml_file();
    resolve_driver_config(cli_driver, cli_policy, cli_budget, toml_text.as_deref())
}

fn read_toml_file() -> Option<String> {
    let path = dirs::config_dir()?
        .join("claude-commander")
        .join("driver.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            log::warn!("failed to read {}: {e}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_when_driver_flag_absent() {
        assert!(resolve_driver_config(false, None, None, None).is_none());
        // Even with other flags set, absence of `--driver` means no
        // driver mode — the other flags are ignored (clap enforces
        // the `requires = "driver"` relationship at parse time, so
        // this path is mostly for defense-in-depth).
        assert!(
            resolve_driver_config(false, Some(SpawnPolicyArg::Trust), Some(99), None).is_none()
        );
    }

    #[test]
    fn cli_policy_overrides_toml() {
        let toml = r#"
            [driver]
            spawn_policy = "trust"
            budget = 7
        "#;
        let cfg = resolve_driver_config(true, Some(SpawnPolicyArg::Budget), None, Some(toml))
            .expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Budget);
        // Budget came from TOML since no CLI budget was given.
        assert_eq!(cfg.spawn_budget, 7);
    }

    #[test]
    fn cli_budget_overrides_toml() {
        let toml = r#"
            [driver]
            spawn_policy = "budget"
            budget = 1
        "#;
        let cfg = resolve_driver_config(true, None, Some(42), Some(toml)).expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Budget);
        assert_eq!(cfg.spawn_budget, 42);
    }

    #[test]
    fn toml_fills_in_when_cli_absent() {
        let toml = r#"
            [driver]
            spawn_policy = "budget"
            budget = 5
        "#;
        let cfg = resolve_driver_config(true, None, None, Some(toml)).expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Budget);
        assert_eq!(cfg.spawn_budget, 5);
    }

    #[test]
    fn fallback_is_ask_and_zero() {
        let cfg = resolve_driver_config(true, None, None, None).expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Ask);
        assert_eq!(cfg.spawn_budget, 0);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        // Not valid TOML at all.
        let garbage = "this is not valid toml at all ][ =";
        let cfg = resolve_driver_config(true, None, None, Some(garbage)).expect("driver mode");
        // Strictest fallback — a typo must not silently loosen policy.
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Ask);
        assert_eq!(cfg.spawn_budget, 0);
    }

    #[test]
    fn unknown_policy_string_falls_back_to_ask() {
        let toml = r#"
            [driver]
            spawn_policy = "nonsense"
            budget = 4
        "#;
        let cfg = resolve_driver_config(true, None, None, Some(toml)).expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Ask);
        // Budget is still honored — it's a separate field and parsed fine.
        assert_eq!(cfg.spawn_budget, 4);
    }

    #[test]
    fn policy_parsing_is_case_insensitive() {
        let toml = r#"
            [driver]
            spawn_policy = "BUDGET"
        "#;
        let cfg = resolve_driver_config(true, None, None, Some(toml)).expect("driver mode");
        assert_eq!(cfg.spawn_policy, SpawnPolicy::Budget);
        assert_eq!(cfg.spawn_budget, 0);
    }

    #[test]
    fn spawn_policy_arg_maps_to_domain_enum() {
        assert_eq!(SpawnPolicy::from(SpawnPolicyArg::Ask), SpawnPolicy::Ask);
        assert_eq!(
            SpawnPolicy::from(SpawnPolicyArg::Budget),
            SpawnPolicy::Budget
        );
        assert_eq!(SpawnPolicy::from(SpawnPolicyArg::Trust), SpawnPolicy::Trust);
    }

    #[test]
    fn to_role_constructs_driver_variant() {
        let cfg = DriverConfig {
            spawn_policy: SpawnPolicy::Budget,
            spawn_budget: 3,
        };
        assert_eq!(
            cfg.to_role(),
            SessionRole::Driver {
                spawn_budget: 3,
                spawn_policy: SpawnPolicy::Budget,
            }
        );
    }
}
