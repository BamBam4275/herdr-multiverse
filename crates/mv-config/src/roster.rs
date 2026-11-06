//! The two-tier `roster.toml` model: it replaces per-role chains, it
//! does not merge with them.
//!
//! `<project>/.multiverse/roster.toml` lives inside the project's own
//! repository and may name exactly one table, `[chains]`. Loading it
//! is stricter than loading the global config in two ways that matter:
//! an unknown top-level table is a **hard error**, not a warning
//! (spec-config.md §B, "unlike the global config's warn-only policy"),
//! and a role named here **replaces** that role's global chain
//! entirely rather than merging with it (spec-config.md §K, Q18).

use std::fs;
use std::path::Path;

use toml::Value;

use crate::error::ConfigError;
use crate::load::{self, Config};
use crate::roles::Role;

/// A project-local chain override. Every role named here replaces that
/// role's chain from the global config; a role not named here keeps
/// whatever the global config configured for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RosterOverride {
    pub chains: std::collections::BTreeMap<Role, Vec<String>>,
}

/// Loads `<project>/.multiverse/roster.toml`, or `Ok(None)` if the
/// project has no local override at all — distinct from a present but
/// empty `[chains]` table, which parses to `Some` with an empty map.
pub fn load_roster(path: &Path) -> Result<Option<RosterOverride>, ConfigError> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ConfigError::at(
                path,
                format!("could not read roster.toml: {e}"),
            ))
        }
    };
    load_roster_str(&content, path).map(Some)
}

/// `load_roster`'s parse core, given the file's contents directly.
pub fn load_roster_str(content: &str, path: &Path) -> Result<RosterOverride, ConfigError> {
    let root: toml::value::Table = content
        .parse()
        .map_err(|e| ConfigError::at(path, format!("malformed TOML: {e}")))?;

    for key in root.keys() {
        if key != "chains" {
            return Err(ConfigError::at(
                path,
                format!("{key:?} is not permitted in roster.toml — only [chains] is"),
            ));
        }
    }

    let mut chains = std::collections::BTreeMap::new();
    if let Some(chains_value) = root.get("chains") {
        let chains_table = chains_value
            .as_table()
            .ok_or_else(|| ConfigError::at(path, "chains must be a table"))?;
        for (key, list_value) in chains_table {
            if Role::is_legacy_key(key) && Role::from_config_key(key).is_err() {
                return Err(ConfigError::at(
                    path,
                    format!(
                        "chains: {key:?} is a retired role key; run the config migrator \
                         (`multiverse config migrate`) before adding new chains"
                    ),
                ));
            }
            let role = Role::from_config_key(key)
                .map_err(|e| ConfigError::at(path, format!("chains: {e}")))?;
            let accounts = match list_value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::String(s) => out.push(s.clone()),
                            other => {
                                return Err(ConfigError::at(
                                    path,
                                    format!(
                                    "chains.{key}: must be a list of strings, got {other:?} in it"
                                ),
                                ))
                            }
                        }
                    }
                    out
                }
                other => {
                    return Err(ConfigError::at(
                        path,
                        format!("chains.{key}: must be a list of strings, got {other:?}"),
                    ))
                }
            };
            if accounts.is_empty() {
                return Err(ConfigError::at(
                    path,
                    format!("chains.{key}: must not be an empty chain"),
                ));
            }
            chains.insert(role, accounts);
        }
    }

    Ok(RosterOverride { chains })
}

/// Re-applies every chain rule that the global config enforces (known
/// account, account serves the role's kind, interactive-seat
/// restriction) to a loaded override, against `config`'s accounts and
/// roles.
pub fn validate_roster(
    config: &Config,
    project_name: &str,
    roster: &RosterOverride,
) -> Result<(), ConfigError> {
    for (role, accounts) in &roster.chains {
        let scope = format!(
            "{project_name}/.multiverse/roster.toml: chains.{}",
            role.config_key()
        );
        load::validate_chain_entry(config, &scope, *role, accounts)?;
    }
    Ok(())
}

/// The chain that actually applies to `role` in `project_name`: the
/// roster override if it names this role, otherwise the global
/// config's chain for the same role. `is_locally_overridden` tells a
/// caller (e.g. `multiverse check`) which one it got, so a human
/// debugging routing does not read only the global file and believe
/// it is the whole story.
pub fn effective_chain<'a>(
    config: &'a Config,
    project_name: &str,
    role: Role,
    roster: Option<&'a RosterOverride>,
) -> Option<&'a [String]> {
    if let Some(chain) = roster.and_then(|r| r.chains.get(&role)) {
        return Some(chain.as_slice());
    }
    config
        .projects
        .get(project_name)
        .and_then(|p| p.chains.get(&role))
        .map(Vec::as_slice)
}

/// Whether `role`'s chain for this project comes from the local
/// roster override rather than the global config.
pub fn is_locally_overridden(roster: Option<&RosterOverride>, role: Role) -> bool {
    roster.is_some_and(|r| r.chains.contains_key(&role))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::load_config_str;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn config_with_two_accounts(project_root: &Path) -> Config {
        let content = format!(
            r#"
[accounts.claude-work]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"

[accounts.grok-work]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "<<PLACEHOLDER: x>>"

[roles.critic]
[roles.critic.models]
claude = "<<PLACEHOLDER: x>>"
grok = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
critic = ["claude-work"]
"#,
            path = project_root.display()
        );
        load_config_str(&content, &PathBuf::from("/tmp/config.toml")).unwrap()
    }

    #[test]
    fn missing_roster_file_is_none_not_an_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        assert_eq!(load_roster(&path).unwrap(), None);
    }

    #[test]
    fn roster_rejects_any_table_but_chains() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        let err = load_roster_str("[accounts.sneaky]\nkind = \"claude\"\n", &path).unwrap_err();
        assert!(err.message().contains("accounts"));
    }

    #[test]
    fn roster_overrides_replace_the_global_chain_entirely() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let config = config_with_two_accounts(&project_root);

        let roster = load_roster_str(
            "[chains]\ncritic = [\"grok-work\"]\n",
            &dir.path().join("roster.toml"),
        )
        .unwrap();
        validate_roster(&config, "example", &roster).unwrap();

        let chain = effective_chain(&config, "example", Role::Critic, Some(&roster)).unwrap();
        assert_eq!(chain, ["grok-work"]);
        assert!(is_locally_overridden(Some(&roster), Role::Critic));
    }

    #[test]
    fn a_role_not_named_in_the_roster_keeps_the_global_chain() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let config = config_with_two_accounts(&project_root);

        let roster = load_roster_str("[chains]\n", &dir.path().join("roster.toml")).unwrap();
        let chain = effective_chain(&config, "example", Role::Critic, Some(&roster)).unwrap();
        assert_eq!(chain, ["claude-work"]);
        assert!(!is_locally_overridden(Some(&roster), Role::Critic));
    }

    #[test]
    fn roster_rejects_an_empty_chain() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        let err = load_roster_str("[chains]\ncritic = []\n", &path).unwrap_err();
        assert!(err.message().contains("empty"));
    }

    #[test]
    fn roster_rejects_a_retired_role_key() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.toml");
        let err = load_roster_str("[chains]\nreviewer = [\"a\"]\n", &path).unwrap_err();
        assert!(err.message().contains("reviewer"));
        assert!(err.message().contains("migrator"));
        assert!(err.message().contains("multiverse config migrate"));
    }

    #[test]
    fn validate_roster_still_enforces_the_interactive_seat_restriction() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[accounts.human]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
command = "claude-interactive"
interactive = true

[roles.critic]
[roles.critic.models]
claude = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
"#,
            path = project_root.display()
        );
        let config = load_config_str(&content, &PathBuf::from("/tmp/config.toml")).unwrap();

        let roster = load_roster_str(
            "[chains]\ncritic = [\"human\"]\n",
            &dir.path().join("roster.toml"),
        )
        .unwrap();
        let err = validate_roster(&config, "example", &roster).unwrap_err();
        assert!(err.message().contains("lend_interactive_to_workers"));
    }
}
