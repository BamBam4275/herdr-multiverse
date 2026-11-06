//! Agent-kind and permission-deny policy ported from `herdr.py` (card
//! `20261102-b-e6cef7`): `AGENT_KINDS`, `grok_never_list`,
//! `claude_never_list`, and the rest of that module's policy/validation
//! "friends" this task's brief names in the same breath — the reader-only
//! deny lists and their worktree-deny helpers, the Claude permission-mode
//! and disallowed-tools flags, the specialist deny list, the
//! config-dir-aware settings denies and orchestrator launch wrapper, and
//! the integration-name/state-integration/degraded-kind helpers.
//!
//! Still not ported, because it is a different concern from this file's
//! agent-kind/deny-list policy rather than a "friend" of it:
//! `codex_worktree_permissions`/`ensure_codex_trusted` (already own a
//! home in `crate::codex_trust`), `temporary_projects_refusal`, and
//! `dispatch_pane_label` (both still referenced only in comments —
//! `crate::fleet_start` and `commands::start`). The CLI's
//! `commands::start::nexus_launch_script` calls `claude_orchestrator_launch`
//! for Claude Nexus accounts, applying the settings denies and launch
//! wrapper before starting the interactive seat.

use std::path::Path;

/// Agent kinds `herdr agent start --kind` accepts (`VERIFICATION.md` A14, E1).
pub const AGENT_KINDS: &[&str] = &[
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
];

/// Fleet-wide Grok denies for `home` (the running user's home directory).
/// Verbatim from the Oracle (card `20260913-b-c15598`, amended by
/// `20260913-oracle-e21cb5`) — not re-derived here. A rule quietly dropped
/// from this list is a rule that stops being denied on every grok seat with
/// nothing to notice it.
///
/// `Bash(rm *)` is deliberately NOT here: it cuts on `write_scope`, not
/// role — a grok WRITER runs in its own isolated worktree and needs `rm` to
/// clean it up, while a grok READER runs in the shared project root and
/// still must not (`GROK_READER_NEVER_LIST` in the Python original, not yet
/// ported here).
///
/// **THE ACCEPTED COST:** grok under `--always-approve` auto-approves `rm`
/// unless denied, with no Claude-style critical-path stall — so a grok
/// writer can `rm` anything the OS user can, including the main checkout
/// and `$HOME`. The Oracle accepted this explicitly: the hole is already
/// open via `python3` and `/bin/rm`, so denying only the polite `Bash(rm *)`
/// spelling fleet-wide punished writers without closing it.
pub fn grok_never_list(home: &Path) -> Vec<String> {
    vec![
        "Bash(git push)".to_string(),
        "Bash(git stash)".to_string(),
        "Bash(sudo *)".to_string(),
        "Bash(herdr)".to_string(),
        "Bash(herdr *)".to_string(),
        format!("Edit({})", home.join(".grok").join("config.toml").display()),
        format!("Edit({})", home.join(".ssh").join("**").display()),
        format!("Read({})", home.join(".ssh").join("**").display()),
        format!("Edit({})", home.join(".gitconfig").display()),
        "MCPTool(gmail__*)".to_string(),
        "MCPTool(robinhood__*)".to_string(),
        "MCPTool(google_drive__*)".to_string(),
        "MCPTool(google_calendar__*)".to_string(),
        "MCPTool(finance__*)".to_string(),
        "MCPTool(github__*)".to_string(),
    ]
}

/// Fleet-wide Claude denies for `home` (the running user's home directory)
/// and `directives_root` (`crate::directives::directives_root`'s result for
/// the selected fleet state directory). Verbatim from the Oracle (card
/// `20260913-b-d6c719`, amended by `20260913-oracle-9828e5`) — not
/// re-derived here.
///
/// The doubled leading slash on every `Edit` rule (`Edit(/{home}/...)`,
/// where `home` is itself already absolute) is the Oracle's own anchor
/// syntax for an absolute path — not a typo, and not the same convention as
/// grok's plain single-slash absolute paths above; the two harnesses do not
/// share a pattern syntax.
///
/// `Bash(rm *)` is deliberately NOT here, for the same reason as
/// `grok_never_list`'s own note (`CLAUDE_READER_NEVER_LIST` in the Python
/// original, not yet ported here). The three `Edit` path rules are not a
/// complete fence either — `python3`, Node, or `sed`/`cp`/`mv`/`rm` invoked
/// via `bash -c` all still reach these files — closing that needs the Bash
/// sandbox, deliberately unprobed and untried here; do not add a
/// `Bash(python3 -c *)` or `Bash(sed *)` rule, it would not close the hole.
pub fn claude_never_list(home: &Path, directives_root: &Path) -> Vec<String> {
    vec![
        "Bash(git push *)".to_string(),
        "Bash(git stash *)".to_string(),
        "Bash(sudo *)".to_string(),
        "Bash(multiverse dispatch *)".to_string(),
        format!(
            "Edit(/{})",
            home.join(".claude").join("settings.json").display()
        ),
        format!("Edit(/{})", home.join(".ssh").join("**").display()),
        format!("Edit(/{})", home.join(".gitconfig").display()),
        format!("Edit(/{}/**)", directives_root.display()),
    ]
}

/// Herdr command groups present in 0.9.0 (`herdr.py:44-50`). `tool` is
/// deliberately absent: `herdr tool run` did NOT ship.
pub const COMMAND_GROUPS: &[&str] = &[
    "agent",
    "pane",
    "workspace",
    "tab",
    "worktree",
    "terminal",
    "notification",
    "integration",
    "session",
    "api",
    "config",
    "channel",
    "plugin",
    "server",
    "machine",
];

/// Integrations `herdr integration install` offers, by their *install*
/// names (`herdr.py:65-71`). Not all spelled the same as the agent kind —
/// see [`integration_name`].
pub const INTEGRATION_NAMES: &[&str] = &[
    "pi",
    "omp",
    "claude",
    "codex",
    "copilot",
    "devin",
    "droid",
    "kimi",
    "opencode",
    "kilo",
    "hermes",
    "qodercli",
    "qwen",
    "letta",
    "cursor",
    "mastracode",
    "antigravity-cli",
    "grok",
];

/// The `herdr integration install` name for an agent `kind`
/// (`herdr.py:73-77, 617-619`). Antigravity is the only kind whose
/// integration installs under a different name: the kind is `agy`, the
/// integration is `antigravity-cli`. Comparing `AGENT_KINDS` against
/// `INTEGRATION_NAMES` naively would wrongly report `agy` as having no
/// integration.
pub fn integration_name(kind: &str) -> &str {
    match kind {
        "agy" => "antigravity-cli",
        other => other,
    }
}

/// Whether `kind` can report authoritative working/blocked/done state
/// (`herdr.py:622-629`). Spec §12.1: *"Install the integration for every
/// harness you actually dispatch: without it you run degraded."* For the
/// kinds this returns `false` for, degraded is the only mode there is —
/// screen-pattern detection only.
pub fn has_state_integration(kind: &str) -> bool {
    INTEGRATION_NAMES.contains(&integration_name(kind))
}

/// Kinds startable by `agent start` that have no installable integration
/// under any name (`herdr.py:634-636`, `VERIFICATION.md` A14).
pub fn degraded_kinds() -> Vec<&'static str> {
    AGENT_KINDS
        .iter()
        .copied()
        .filter(|kind| !has_state_integration(kind))
        .collect()
}

/// Kinds that do have an integration — the complement of
/// [`degraded_kinds`] (`herdr.py:639`, kept there as a "backwards-compatible
/// alias").
pub fn integration_kinds() -> Vec<&'static str> {
    AGENT_KINDS
        .iter()
        .copied()
        .filter(|kind| has_state_integration(kind))
        .collect()
}

/// Additional rule for a grok READER (`write_scope == "none"`) only — the
/// other half of moving `Bash(rm *)` off [`grok_never_list`] (card
/// `20260913-oracle-e21cb5`, `herdr.py:376-383`). A reader runs in the
/// shared project root rather than an isolated worktree, so unlike a
/// writer it still needs `rm` denied.
///
/// Not narrowed to `Bash(rm -rf *)`: the broader form is the one that also
/// covers `rm file`.
pub const GROK_READER_NEVER_LIST: &[&str] = &["Bash(rm *)"];

/// The extra `--deny` a grok READER additionally carries, naming
/// `project_root` absolutely (`herdr.py:386-400`). Readers run in the
/// shared project root rather than their own worktree, so without this a
/// read-only seat could write into another lane's
/// `.multiverse/worktrees/<task_id>/`. `project_root` must already be
/// absolute — grok matches `~` in a pattern literally, never expanding
/// it, so a `~`-relative project path would silently match nothing.
pub fn grok_reader_worktrees_deny(project_root: &Path) -> String {
    format!(
        "Edit({}/**)",
        project_root.join(".multiverse").join("worktrees").display()
    )
}

/// Claude's own name for what grok calls `--always-approve`
/// (`herdr.py:403-410`). Equivalent to `--dangerously-skip-permissions`,
/// but the named mode is documented rather than inferred from the flag's
/// own name — and, like grok's `--always-approve`, deny rules are a hard
/// block even under it.
pub const CLAUDE_PERMISSION_MODE_FLAG: &str = "--permission-mode";

/// The value passed with [`CLAUDE_PERMISSION_MODE_FLAG`] (`herdr.py:413`).
pub const CLAUDE_BYPASS_PERMISSIONS_MODE: &str = "bypassPermissions";

/// Claude's bounding lever (`herdr.py:415-420`). Unlike grok's repeated
/// `--deny`, this flag is variadic — passed once, then every rule as its
/// own argv element. A *bare* tool name (`Edit`, `*`) would remove the
/// tool from context entirely; every rule [`claude_never_list`] returns is
/// scoped, which leaves the tool available and denies only the matching
/// call.
pub const CLAUDE_DISALLOWED_TOOLS_FLAG: &str = "--disallowedTools";

/// Additional rules for a *dispatched specialist* — every claude `Card`
/// (`herdr.py:491-506`), since the Orchestrator never runs as one (see
/// [`claude_never_list`]). Without these, a specialist could drive Herdr
/// directly, or dispatch siblings the orchestrator never scheduled.
/// `Bash(multiverse activity inbox)` is an exact match only, so `inbox
/// --peek` stays allowed.
pub const CLAUDE_SPECIALIST_NEVER_LIST: &[&str] = &[
    "Bash(herdr)",
    "Bash(herdr *)",
    "Bash(multiverse activity inbox)",
];

/// Additional rule for a claude READER (`write_scope == "none"`) only —
/// the other half of moving `Bash(rm *)` off [`claude_never_list`] (card
/// `20260913-oracle-9828e5`, `herdr.py:508-512`).
pub const CLAUDE_READER_NEVER_LIST: &[&str] = &["Bash(rm *)"];

/// The extra rule a claude READER additionally carries, naming
/// `project_root` absolutely (`herdr.py:515-530`). Uses the Oracle's
/// doubled-slash anchor syntax, matching [`claude_never_list`]'s `Edit`
/// rules — a different shape from grok's [`grok_reader_worktrees_deny`],
/// because the two harnesses do not share a pattern syntax.
pub fn claude_reader_worktrees_deny(project_root: &Path) -> String {
    format!(
        "Edit(/{}/**)",
        project_root.join(".multiverse").join("worktrees").display()
    )
}

/// Whether `config_dir` is the default Claude config directory, `home`
/// joined with `.claude` (`herdr.py:533-545`'s `DEFAULT_CLAUDE_CONFIG_DIR`
/// comparison). `home` is threaded through explicitly rather than read
/// from the process environment at const-init time, since this crate's
/// functions take every host fact as an argument.
pub fn claude_config_dir_is_default(config_dir: &Path, home: &Path) -> bool {
    config_dir == home.join(".claude").as_path()
}

/// `Edit` denies on `config_dir`'s `settings.json` and
/// `settings.local.json` (`herdr.py:548-572`). [`claude_never_list`]'s own
/// `Edit` rule names only the default `~/.claude/settings.json` — a seat
/// running under a different `CLAUDE_CONFIG_DIR` has its real settings
/// files left open, and no existing rule reaches `settings.local.json` at
/// all. `Edit`-only, matching the existing rule's own shape: a live probe
/// (card `20261011-b-ff1f61`) showed Claude Code ignoring
/// `Write`/`MultiEdit` path rules on `--disallowedTools` outright, so
/// `Edit` alone is both what the CLI actually honours and already covers
/// every file-editing tool.
pub fn claude_settings_denies(config_dir: &Path) -> Vec<String> {
    ["settings.json", "settings.local.json"]
        .iter()
        .map(|name| format!("Edit(/{})", config_dir.join(name).display()))
        .collect()
}

/// The shell script `cmd_start` runs in an orchestrator's pane, for a
/// claude-kind interactive seat (`herdr.py:575-614`). Gives the
/// Orchestrator the same `--disallowedTools` shape every dispatched
/// claude `Card` gets, plus [`claude_settings_denies`] for its own
/// `config_dir` — unconditionally, since unlike a dispatched Card there is
/// no existing `~/.claude` rule to avoid repeating for the common case.
///
/// [`CLAUDE_SPECIALIST_NEVER_LIST`] is deliberately NOT included: it
/// denies `Bash(herdr)`/`Bash(herdr *)`, which would stop the Orchestrator
/// driving Herdr at all — the thing this seat exists to do. Only the
/// file-write denies are carried over from [`claude_never_list`], never
/// the Bash ones — `Bash(multiverse dispatch *)`, `Bash(sudo *)`, etc.,
/// every one of which an Orchestrator runs each cycle — and no
/// `--permission-mode` is added, so the one change at start is that the
/// seat can no longer rewrite its own permissions or the human's
/// directives.
pub fn claude_orchestrator_launch(
    command: &str,
    config_dir: &Path,
    home: &Path,
    directives_root: &Path,
) -> String {
    let mut rules: Vec<String> = claude_never_list(home, directives_root)
        .into_iter()
        .filter(|rule| rule.starts_with("Edit("))
        .collect();
    rules.extend(claude_settings_denies(config_dir));
    let mut argv = vec![
        command.to_string(),
        CLAUDE_DISALLOWED_TOOLS_FLAG.to_string(),
    ];
    argv.extend(rules);
    mv_herdr::argv::shell_join(&argv)
}

/// The trust/permission arguments a dispatched claude specialist's launch
/// carries after its native model/effort args (`mv_herdr::argv::
/// native_args`) — the Rust port of `dispatch.py`'s `Card.agent_args`
/// claude branch (cards `20260913-b-d6c719`, `20260913-oracle-9828e5`,
/// `20261006-b-19684e`). Every claude seat this builds args for is a
/// dispatched specialist: the Orchestrator's own launch goes through
/// [`claude_orchestrator_launch`] instead, so the specialist rules apply
/// unconditionally rather than being cut on role.
///
/// `config_dir` is the seat's own `CLAUDE_CONFIG_DIR` (not necessarily
/// `home`'s default — an account may use a custom config directory); `write_scope`
/// is the dispatching role's write scope, `"none"` for a reader;
/// `project_root` is needed only for a reader's worktrees deny. Shared by
/// every caller that builds a claude seat's launch — an ordinary dispatch
/// and a council seat alike — so there is exactly one place either could
/// drift from the other (card `20261105-b-50b425`).
pub fn claude_specialist_launch_args(
    config_dir: &Path,
    home: &Path,
    directives_root: &Path,
    write_scope: &str,
    project_root: &Path,
) -> Vec<String> {
    let mut rules: Vec<String> = claude_never_list(home, directives_root);
    rules.extend(CLAUDE_SPECIALIST_NEVER_LIST.iter().map(|s| s.to_string()));
    if !claude_config_dir_is_default(config_dir, home) {
        rules.extend(claude_settings_denies(config_dir));
    }
    if write_scope == "none" {
        rules.extend(CLAUDE_READER_NEVER_LIST.iter().map(|s| s.to_string()));
        rules.push(claude_reader_worktrees_deny(project_root));
    }
    let mut args = vec![
        CLAUDE_PERMISSION_MODE_FLAG.to_string(),
        CLAUDE_BYPASS_PERMISSIONS_MODE.to_string(),
        CLAUDE_DISALLOWED_TOOLS_FLAG.to_string(),
    ];
    args.extend(rules);
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_kinds_matches_the_verified_set() {
        assert_eq!(AGENT_KINDS.len(), 24);
        for kind in [
            "pi",
            "claude",
            "codex",
            "gemini",
            "cursor",
            "devin",
            "agy",
            "cline",
            "omp",
            "mastracode",
            "opencode",
            "copilot",
            "kimi",
            "kiro",
            "droid",
            "amp",
            "grok",
            "hermes",
            "kilo",
            "qodercli",
            "qwen",
            "letta",
            "maki",
            "muse",
        ] {
            assert!(AGENT_KINDS.contains(&kind), "missing kind {kind}");
        }
    }

    #[test]
    fn grok_never_list_matches_the_oracle_verbatim() {
        let home = Path::new("/home/demo");
        assert_eq!(
            grok_never_list(home),
            vec![
                "Bash(git push)",
                "Bash(git stash)",
                "Bash(sudo *)",
                "Bash(herdr)",
                "Bash(herdr *)",
                "Edit(/home/demo/.grok/config.toml)",
                "Edit(/home/demo/.ssh/**)",
                "Read(/home/demo/.ssh/**)",
                "Edit(/home/demo/.gitconfig)",
                "MCPTool(gmail__*)",
                "MCPTool(robinhood__*)",
                "MCPTool(google_drive__*)",
                "MCPTool(google_calendar__*)",
                "MCPTool(finance__*)",
                "MCPTool(github__*)",
            ]
        );
    }

    #[test]
    fn grok_never_list_follows_the_given_home_and_never_hardcodes_one() {
        let other = Path::new("/home/other-user");
        for rule in grok_never_list(other) {
            if rule.starts_with("Edit(") || rule.starts_with("Read(") {
                assert!(rule.contains("/home/other-user"), "{rule}");
                assert!(!rule.contains("/home/demo"), "{rule}");
            }
        }
    }

    #[test]
    fn no_grok_rule_contains_a_literal_tilde() {
        // grok matches `~` in a pattern literally, never expanding it.
        for rule in grok_never_list(Path::new("/home/demo")) {
            assert!(!rule.contains('~'), "{rule}");
        }
    }

    #[test]
    fn bash_rm_is_not_fleet_wide_for_grok() {
        // Card `20260913-oracle-e21cb5`: moved to the reader-only list,
        // not ported here (see this module's doc comment).
        assert!(!grok_never_list(Path::new("/home/demo")).contains(&"Bash(rm *)".to_string()));
    }

    #[test]
    fn claude_never_list_matches_the_oracle_verbatim() {
        let home = Path::new("/home/demo");
        let directives_root = Path::new("/home/demo/.config/multiverse/directives");
        assert_eq!(
            claude_never_list(home, directives_root),
            vec![
                "Bash(git push *)",
                "Bash(git stash *)",
                "Bash(sudo *)",
                "Bash(multiverse dispatch *)",
                "Edit(//home/demo/.claude/settings.json)",
                "Edit(//home/demo/.ssh/**)",
                "Edit(//home/demo/.gitconfig)",
                "Edit(//home/demo/.config/multiverse/directives/**)",
            ]
        );
    }

    #[test]
    fn bash_rm_is_not_fleet_wide_for_claude() {
        // Card `20260913-oracle-9828e5`: moved to the reader-only list,
        // not ported here (see this module's doc comment).
        let rules = claude_never_list(Path::new("/home/demo"), Path::new("/home/demo/directives"));
        assert!(!rules.contains(&"Bash(rm *)".to_string()));
    }

    #[test]
    fn no_sed_or_python3_rule_exists_for_claude() {
        // The Oracle's correction: the existing `Edit` path rules already
        // stop `sed -i`/redirects against those exact paths, and a
        // `python3` bypass is uncloseable by any `Bash` rule.
        for rule in claude_never_list(Path::new("/home/demo"), Path::new("/home/demo/directives")) {
            assert!(!rule.contains("sed"), "{rule}");
            assert!(!rule.contains("python3"), "{rule}");
        }
    }

    #[test]
    fn no_claude_rule_is_a_bare_tool_name() {
        // A bare name (`Edit`, `*`) would remove the tool from context
        // entirely; every rule must be scoped.
        for rule in claude_never_list(Path::new("/home/demo"), Path::new("/home/demo/directives")) {
            assert!(rule.contains('(') && rule.ends_with(')'), "{rule}");
        }
    }

    #[test]
    fn no_claude_rule_contains_a_literal_tilde() {
        for rule in claude_never_list(Path::new("/home/demo"), Path::new("/home/demo/directives")) {
            assert!(!rule.contains('~'), "{rule}");
        }
    }

    #[test]
    fn git_add_commit_and_rm_are_not_denied_for_claude() {
        // A Fixer has to commit in its lane — that is the handback path.
        let denied = claude_never_list(Path::new("/home/demo"), Path::new("/home/demo/directives"))
            .join(" ");
        assert!(!denied.contains("git add"));
        assert!(!denied.contains("git commit"));
        assert!(!denied.contains("git rm"));
    }

    #[test]
    fn claude_directives_deny_follows_the_given_directives_root() {
        let home = Path::new("/home/demo");
        let selected = Path::new("/home/demo/fleet/directives");
        assert!(claude_never_list(home, selected)
            .contains(&"Edit(//home/demo/fleet/directives/**)".to_string()));

        let explicit = Path::new("/home/demo/operator-directives");
        assert!(claude_never_list(home, explicit)
            .contains(&"Edit(//home/demo/operator-directives/**)".to_string()));
    }

    #[test]
    fn command_groups_match_the_verified_set_and_omit_tool() {
        assert_eq!(COMMAND_GROUPS.len(), 15);
        assert!(!COMMAND_GROUPS.contains(&"tool"));
        assert!(COMMAND_GROUPS.contains(&"agent"));
        assert!(COMMAND_GROUPS.contains(&"machine"));
    }

    #[test]
    fn integration_name_maps_agy_to_antigravity_cli_and_leaves_others_alone() {
        assert_eq!(integration_name("agy"), "antigravity-cli");
        assert_eq!(integration_name("claude"), "claude");
        assert_eq!(integration_name("grok"), "grok");
    }

    #[test]
    fn has_state_integration_follows_the_mapped_name() {
        assert!(has_state_integration("claude"));
        assert!(has_state_integration("agy"));
        // No `gemini` entry in INTEGRATION_NAMES under any mapped name.
        assert!(!has_state_integration("gemini"));
    }

    #[test]
    fn degraded_and_integration_kinds_partition_agent_kinds_without_overlap() {
        let degraded = degraded_kinds();
        let integrated = integration_kinds();
        assert_eq!(degraded.len() + integrated.len(), AGENT_KINDS.len());
        for kind in &degraded {
            assert!(!has_state_integration(kind));
            assert!(!integrated.contains(kind));
        }
        for kind in &integrated {
            assert!(has_state_integration(kind));
        }
        // `agy` has an integration installed under a different name
        // (`antigravity-cli`), not degraded despite the spelling mismatch.
        assert!(integrated.contains(&"agy"));
    }

    #[test]
    fn grok_reader_never_list_denies_only_rm() {
        assert_eq!(GROK_READER_NEVER_LIST, &["Bash(rm *)"]);
    }

    #[test]
    fn grok_reader_worktrees_deny_names_the_given_project_root_absolutely() {
        assert_eq!(
            grok_reader_worktrees_deny(Path::new("/projects/demo")),
            "Edit(/projects/demo/.multiverse/worktrees/**)"
        );
    }

    #[test]
    fn claude_reader_never_list_denies_only_rm() {
        assert_eq!(CLAUDE_READER_NEVER_LIST, &["Bash(rm *)"]);
    }

    #[test]
    fn claude_reader_worktrees_deny_uses_the_doubled_slash_anchor() {
        assert_eq!(
            claude_reader_worktrees_deny(Path::new("/projects/demo")),
            "Edit(//projects/demo/.multiverse/worktrees/**)"
        );
    }

    #[test]
    fn claude_specialist_never_list_blocks_raw_herdr_and_exact_inbox_only() {
        assert!(CLAUDE_SPECIALIST_NEVER_LIST.contains(&"Bash(herdr)"));
        assert!(CLAUDE_SPECIALIST_NEVER_LIST.contains(&"Bash(herdr *)"));
        assert!(CLAUDE_SPECIALIST_NEVER_LIST.contains(&"Bash(multiverse activity inbox)"));
        assert!(!CLAUDE_SPECIALIST_NEVER_LIST.contains(&"Bash(multiverse activity inbox --peek)"));
    }

    #[test]
    fn claude_config_dir_is_default_only_for_home_dot_claude() {
        let home = Path::new("/home/demo");
        assert!(claude_config_dir_is_default(&home.join(".claude"), home));
        assert!(!claude_config_dir_is_default(
            Path::new("/home/demo/PCODE"),
            home
        ));
    }

    #[test]
    fn claude_settings_denies_covers_both_settings_files_under_config_dir() {
        // The doubled leading slash matches `claude_never_list`'s own
        // `Edit(/{absolute_home}/...)` anchor syntax: `config_dir` is
        // already absolute, so `Edit(/{config_dir}/...)` doubles it.
        let denies = claude_settings_denies(Path::new("/home/demo/PCODE"));
        assert_eq!(
            denies,
            vec![
                "Edit(//home/demo/PCODE/settings.json)",
                "Edit(//home/demo/PCODE/settings.local.json)",
            ]
        );
    }

    #[test]
    fn claude_orchestrator_launch_carries_only_edit_rules_plus_settings_denies() {
        let home = Path::new("/home/demo");
        let config_dir = home.join(".claude");
        let directives_root = Path::new("/home/demo/.config/multiverse/directives");
        let launch = claude_orchestrator_launch("claude", &config_dir, home, directives_root);

        assert!(launch.starts_with("claude --disallowedTools "));
        // Every fleet-wide Bash rule is dropped: an Orchestrator runs
        // `multiverse dispatch`, `herdr`, `git push`/`stash`, and `sudo`-free
        // commands every cycle.
        assert!(!launch.contains("Bash("));
        assert!(launch.contains("Edit(//home/demo/.ssh/**)"));
        assert!(launch.contains("Edit(//home/demo/.gitconfig)"));
        assert!(launch.contains(&format!("Edit(/{}/**)", directives_root.display())));
        // Settings denies are config_dir-derived, not re-derived from
        // claude_never_list's own hardcoded `~/.claude/settings.json` rule.
        assert!(launch.contains("Edit(//home/demo/.claude/settings.json)"));
        assert!(launch.contains("Edit(//home/demo/.claude/settings.local.json)"));
    }

    #[test]
    fn claude_specialist_launch_args_writer_on_default_config_dir() {
        let home = Path::new("/home/demo");
        let config_dir = home.join(".claude");
        let directives_root = Path::new("/home/demo/.config/multiverse/directives");
        let project_root = Path::new("/projects/demo");
        let args =
            claude_specialist_launch_args(&config_dir, home, directives_root, "task", project_root);
        assert_eq!(
            &args[..3],
            &[
                CLAUDE_PERMISSION_MODE_FLAG,
                CLAUDE_BYPASS_PERMISSIONS_MODE,
                CLAUDE_DISALLOWED_TOOLS_FLAG
            ]
        );
        // Fleet-wide + specialist rules, but no settings-denies duplicate
        // (default config_dir already covered) and no reader-only rules.
        for rule in claude_never_list(home, directives_root) {
            assert!(args.contains(&rule), "{rule}");
        }
        for rule in CLAUDE_SPECIALIST_NEVER_LIST {
            assert!(args.contains(&rule.to_string()), "{rule}");
        }
        assert!(!args.contains(&"Bash(rm *)".to_string()));
        assert!(!args.iter().any(|a| a.contains(".multiverse/worktrees")));
        assert_eq!(
            args.len(),
            3 + claude_never_list(home, directives_root).len() + CLAUDE_SPECIALIST_NEVER_LIST.len()
        );
    }

    #[test]
    fn claude_specialist_launch_args_reader_adds_rm_and_worktrees_deny() {
        let home = Path::new("/home/demo");
        let config_dir = home.join(".claude");
        let directives_root = Path::new("/home/demo/directives");
        let project_root = Path::new("/projects/demo");
        let args =
            claude_specialist_launch_args(&config_dir, home, directives_root, "none", project_root);
        assert!(args.contains(&"Bash(rm *)".to_string()));
        assert!(args.contains(&claude_reader_worktrees_deny(project_root)));
    }

    #[test]
    fn claude_specialist_launch_args_non_default_config_dir_adds_settings_denies() {
        let home = Path::new("/home/demo");
        let config_dir = home.join("PCODE");
        let directives_root = Path::new("/home/demo/directives");
        let project_root = Path::new("/projects/demo");
        let args =
            claude_specialist_launch_args(&config_dir, home, directives_root, "task", project_root);
        for rule in claude_settings_denies(&config_dir) {
            assert!(args.contains(&rule), "{rule}");
        }
    }

    #[test]
    fn claude_orchestrator_launch_shell_quotes_a_command_with_spaces() {
        let home = Path::new("/home/demo");
        let launch = claude_orchestrator_launch(
            "claude --model opus",
            &home.join(".claude"),
            home,
            Path::new("/home/demo/directives"),
        );
        assert!(launch.starts_with("'claude --model opus' --disallowedTools"));
    }
}
