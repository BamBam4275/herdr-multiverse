//! `multiverse add <path> [--name NAME] [--dry-run]`: register an existing
//! project by appending a `[projects.<name>]` table to `config.toml`.
//!
//! Validation (duplicate name/path checks) goes through the real
//! `mv_config::load::Config` merged in from `main` partway through this
//! task. The actual *write* still appends plain text rather than using
//! `mv_config::edit`'s `toml_edit`-based appender: that appender needs a
//! `toml_edit::DocumentMut` in the caller's own code, and `toml_edit` is
//! not a declared `mv-cli` dependency (unlike `mv-config` itself, which
//! `mv-cli` already depends on for reading). Adding it was judged outside
//! this task's scope — see `result.md`'s undecided-calls list — so this
//! keeps the same "append, never rewrite" discipline as a plain-text
//! append, without `toml_edit`'s comment-preserving round-trip.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::load::{Account, Config};
use mv_config::roles::Role;

use super::doctor::{project_path, resolve_config_path};
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::{Failure, NEEDS_CONFIG};

/// Same fix as `start.rs`'s `print_config_load_error` (card
/// `20261102-b-5c41bb`): this file owns only this call site, so it keeps
/// its own copy rather than a cross-file shared helper.
fn print_config_load_error(message: &str) {
    println!("config error: {message}");
    if is_retired_role_key_error(message) {
        println!("{MIGRATE_FIX_LINE}");
    } else {
        println!("Run `multiverse doctor` for a full readiness check.");
    }
}

/// Python `RoleId` iteration order (`roles.py:29-37`), mapped onto the
/// current config keys. `add` writes chains in this order so a generated
/// block matches `_cmd_add_locked` rather than `Role::ALL`.
const ADD_ROLE_ORDER: [Role; 9] = [
    Role::Nexus,
    Role::Scout,
    Role::Seer,
    Role::Builder,
    Role::Critic,
    Role::Council,
    Role::Archivist,
    Role::Artisan,
    Role::Watcher,
];

/// Vendor family each role's chain leads on when an eligible seat of
/// that family exists (`cli.py` `_LEAD_VENDOR_FAMILY`). A preference,
/// not a requirement: a missing family falls through to config order.
fn lead_vendor_family(role: Role) -> &'static [&'static str] {
    match role {
        Role::Scout | Role::Seer | Role::Builder | Role::Watcher => &["claude"],
        Role::Critic => &["chatgpt"],
        Role::Archivist => &["grok"],
        Role::Artisan => &["grok", "chatgpt"],
        Role::Nexus | Role::Council => &[],
    }
}

const LOCAL_VENDOR: &str = "local";

fn no_local(role: Role) -> bool {
    matches!(role, Role::Seer | Role::Watcher | Role::Council)
}

fn usable_accounts(config: &Config) -> Vec<&Account> {
    config
        .accounts
        .values()
        .filter(|account| account.available && !account.interactive)
        .collect()
}

fn serves(config: &Config, role: Role, account: &Account) -> bool {
    config
        .roles
        .get(&role)
        .is_some_and(|settings| settings.models.contains_key(&account.kind))
}

fn split_local<'a>(seats: &[&'a Account]) -> (Vec<&'a Account>, Vec<&'a Account>) {
    let non_local = seats
        .iter()
        .copied()
        .filter(|account| account.vendor != LOCAL_VENDOR)
        .collect();
    let local = seats
        .iter()
        .copied()
        .filter(|account| account.vendor == LOCAL_VENDOR)
        .collect();
    (non_local, local)
}

fn prefer_vendor_family<'a>(seats: &[&'a Account], families: &[&str]) -> Vec<&'a Account> {
    if families.is_empty() {
        return seats.to_vec();
    }
    let preferred: Vec<_> = seats
        .iter()
        .copied()
        .filter(|account| families.contains(&account.vendor.as_str()))
        .collect();
    let rest: Vec<_> = seats
        .iter()
        .copied()
        .filter(|account| !families.contains(&account.vendor.as_str()))
        .collect();
    preferred.into_iter().chain(rest).collect()
}

fn lead_differs_from<'a>(
    seats: Vec<&'a Account>,
    avoid_vendor: Option<&str>,
) -> (Vec<&'a Account>, bool) {
    let Some(avoid) = avoid_vendor else {
        return (seats, true);
    };
    if seats.is_empty() || seats[0].vendor != avoid {
        return (seats, true);
    }
    if let Some(index) = seats.iter().position(|account| account.vendor != avoid) {
        let mut reordered = Vec::with_capacity(seats.len());
        reordered.push(seats[index]);
        reordered.extend(seats.iter().take(index).copied());
        reordered.extend(seats.iter().skip(index + 1).copied());
        return (reordered, true);
    }
    (seats, false)
}

fn format_chain(accounts: &[&Account]) -> String {
    accounts
        .iter()
        .map(|account| format!("{:?}", account.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `[projects.<name>.chains]` body: one `<role> = [...]` line per
/// role with at least one eligible account, rendered under the role's
/// *current* key (`Role::config_key()`). Order follows `_cmd_add_locked`
/// (`cli.py:6480-6646`): vendor-family lead, Critic off Builder's
/// vendor, Seer prefers off Critic, local last or dropped, Council one
/// seat per vendor, Nexus is the interactive seat only. `None` means no
/// role can be served — the caller must refuse rather than append an
/// empty table.
fn chain_lines(config: &Config) -> Option<String> {
    let usable = usable_accounts(config);
    let mut ordered: BTreeMap<Role, Vec<&Account>> = BTreeMap::new();
    let mut local_tail: BTreeMap<Role, Vec<&Account>> = BTreeMap::new();

    for role in ADD_ROLE_ORDER {
        if matches!(role, Role::Nexus | Role::Council) {
            continue;
        }
        if !config.roles.contains_key(&role) {
            continue;
        }
        let seats: Vec<_> = usable
            .iter()
            .copied()
            .filter(|account| serves(config, role, account))
            .collect();
        let (non_local, local) = if no_local(role) {
            (
                seats
                    .iter()
                    .copied()
                    .filter(|account| account.vendor != LOCAL_VENDOR)
                    .collect(),
                Vec::new(),
            )
        } else {
            split_local(&seats)
        };
        ordered.insert(
            role,
            prefer_vendor_family(&non_local, lead_vendor_family(role)),
        );
        local_tail.insert(role, local);
    }

    let mut critic_comment: Option<String> = None;
    if ordered.contains_key(&Role::Critic) {
        let builder_vendor = ordered
            .get(&Role::Builder)
            .and_then(|seats| seats.first())
            .map(|account| account.vendor.as_str());
        let (reordered, satisfied) = lead_differs_from(
            ordered.remove(&Role::Critic).unwrap_or_default(),
            builder_vendor,
        );
        ordered.insert(Role::Critic, reordered);
        if let Some(vendor) = builder_vendor {
            critic_comment = Some(if satisfied {
                format!(
                    "# critic: leads on a vendor other than builder's \
                     ({vendor}) on purpose — reviewing on the vendor \
                     that wrote the code is theatre; same family, same blind \
                     spots (§12.1)."
                )
            } else {
                format!(
                    "# critic: every eligible seat here is {vendor}, \
                     same as builder's lead. Cross-vendor review is not \
                     achievable with the accounts configured for this \
                     project — add a seat from another vendor to close this \
                     gap (§12.1)."
                )
            });
        }
    }

    let mut seer_comment: Option<String> = None;
    if ordered.contains_key(&Role::Seer) && ordered.contains_key(&Role::Critic) {
        let critic_vendor = ordered
            .get(&Role::Critic)
            .and_then(|seats| seats.first())
            .map(|account| account.vendor.as_str());
        let before = ordered
            .get(&Role::Seer)
            .and_then(|seats| seats.first())
            .map(|account| account.vendor.clone());
        let (reordered, _) = lead_differs_from(
            ordered.remove(&Role::Seer).unwrap_or_default(),
            critic_vendor,
        );
        let after = reordered.first().map(|account| account.vendor.clone());
        ordered.insert(Role::Seer, reordered);
        if let (Some(before), Some(after)) = (before, after) {
            if before != after {
                seer_comment = Some(format!(
                    "# seer: moved off {before} to also differ from \
                     critic's lead — otherwise a seer re-checking a \
                     critic finding would be the same seat marking its own \
                     homework."
                ));
            }
        }
    }

    let mut council_seats: Vec<&Account> = Vec::new();
    if config.roles.contains_key(&Role::Council) {
        let mut seen_vendors = BTreeSet::new();
        for account in &usable {
            if !serves(config, Role::Council, account) || account.vendor == LOCAL_VENDOR {
                continue;
            }
            if !seen_vendors.insert(account.vendor.as_str()) {
                continue;
            }
            council_seats.push(*account);
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut has_chain = false;
    if config.roles.contains_key(&Role::Nexus) {
        if let Some(seat) = config.accounts.values().find(|account| account.interactive) {
            lines.push(format!("nexus = [{:?}]", seat.name));
            has_chain = true;
        } else {
            lines.push(
                "# nexus: no account is marked `interactive = \
                 true`, so there is no seat to name here yet. `start` \
                 selects it that way, never through this table — mark \
                 one and add the line yourself."
                    .into(),
            );
        }
    }

    for role in ADD_ROLE_ORDER {
        if role == Role::Nexus {
            continue;
        }
        if role == Role::Council {
            if council_seats.is_empty() {
                continue;
            }
            lines.push(
                "# council: one seat per vendor, never two of the same — a \
                 second account of a vendor already seated is silently \
                 skipped, so this writes only the one that would actually \
                 run (§3.6)."
                    .into(),
            );
            lines.push(
                "# council: no local — a candidate pool is not a fallback \
                 chain, and adding it here would permanently seat a 20B \
                 model beside Opus rather than give the council a floor."
                    .into(),
            );
            lines.push(format!("council = [{}]", format_chain(&council_seats)));
            has_chain = true;
            continue;
        }
        let Some(seats) = ordered.get(&role) else {
            continue;
        };
        let mut full = seats.clone();
        if let Some(local) = local_tail.get(&role) {
            full.extend(local.iter().copied());
        }
        if full.is_empty() {
            continue;
        }
        if role == Role::Critic {
            if let Some(comment) = &critic_comment {
                lines.push(comment.clone());
            }
        }
        if role == Role::Seer {
            lines.push(
                "# seer: no local — `on_exhausted = \"queue\"` means \
                 waiting beats a confident wrong answer from the smallest \
                 model (§12.1)."
                    .into(),
            );
            if let Some(comment) = &seer_comment {
                lines.push(comment.clone());
            }
        }
        if role == Role::Watcher {
            lines.push(
                "# watcher: no local — a 20B model can't see the \
                 screenshot it was dispatched to look at."
                    .into(),
            );
        }
        lines.push(format!("{} = [{}]", role.config_key(), format_chain(&full)));
        has_chain = true;
    }

    has_chain.then(|| {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    })
}

/// Escapes `s` for use inside a TOML basic string (`"…"`): backslash,
/// double quote, and any control byte — a basic string cannot contain a
/// literal newline, tab, or other control character any more than it can
/// contain an unescaped quote. Shared by `toml_quote_key`'s quoted branch
/// and by `register`'s `path = "…"` value, since a project directory's
/// name can legally contain any of these on a real filesystem.
fn toml_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Bare TOML keys allow `[A-Za-z0-9_-]`; quote anything else. Spec-cli.md
/// §7 (commit `4dc61bb`) flags exactly this as a port-breaking regression
/// in the Python original: "`add` not quoting/escaping a project name that
/// the config reload then couldn't parse back."
fn toml_quote_key(name: &str) -> String {
    let is_bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_bare {
        name.to_string()
    } else {
        format!("\"{}\"", toml_escape_string(name))
    }
}

/// Append a `[projects.<name>]` table naming `canonical_path` to
/// `config_path`. Shared by `add` and `new` (new calls this in-process
/// after `git init`, matching spec-cli.md §2's description of the Python
/// original: "`new`... calls `cmd_add`/`cmd_start` directly (in-process)").
///
/// Returns the rendered block (for `--dry-run` display) without writing
/// when `dry_run` is true.
pub(crate) fn register(
    config_path: &Path,
    config: &Config,
    canonical_path: &Path,
    name: &str,
    dry_run: bool,
) -> Result<String, Failure> {
    if config.projects.contains_key(name) {
        println!("config error: a project named {name:?} is already configured.");
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    if config
        .projects
        .values()
        .any(|project| project_path(project) == Some(canonical_path))
    {
        println!(
            "config error: {} is already registered as a project.",
            canonical_path.display()
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }

    let Some(chains) = chain_lines(config) else {
        return Err(Failure::AccountSelection(
            "No role can be served by any available account, so a chain table \
             would be empty.\nCheck `multiverse accounts` and your roles' \
             `models` tables."
                .into(),
        ));
    };
    let key = toml_quote_key(name);
    let mut block = format!(
        "\n[projects.{key}]\npath = \"{}\"\n",
        toml_escape_string(&canonical_path.display().to_string())
    );
    block.push_str(&format!("\n[projects.{key}.chains]\n{chains}"));

    if dry_run {
        println!("Would append to {}:{block}", config_path.display());
        return Ok(block);
    }

    let mut file = OpenOptions::new()
        .append(true)
        .open(config_path)
        .map_err(|error| {
            Failure::Operation(format!("could not open {}: {error}", config_path.display()))
        })?;
    file.write_all(block.as_bytes()).map_err(|error| {
        Failure::Operation(format!(
            "could not write {}: {error}",
            config_path.display()
        ))
    })?;

    println!("Added to {}:{block}", config_path.display());
    Ok(block)
}

/// Resolve `raw_path` to an existing directory, or report why not.
pub(crate) fn canonicalize_project_path(raw_path: &str) -> Result<PathBuf, Failure> {
    match std::fs::canonicalize(raw_path) {
        Ok(path) if path.is_dir() => Ok(path),
        _ => {
            println!("config error: {raw_path:?} is not an existing directory.");
            Err(Failure::Reported(NEEDS_CONFIG))
        }
    }
}

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        println!(
            "config error: no config file found at {}\nRun `multiverse doctor` for a full readiness check.",
            config_path.display()
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    let config_path =
        std::fs::canonicalize(&config_path).map_err(|error| Failure::Config(error.to_string()))?;
    // A stable sidecar protects the entire load/validate/append transaction,
    // including against an editor replacing the config inode.
    let _lock = if args.get_flag("dry-run") {
        None
    } else {
        let mut lock_path = config_path.as_os_str().to_owned();
        lock_path.push(".lock");
        Some(
            mv_core::lock::acquire_default(Path::new(&lock_path), "config add").map_err(
                |error| {
                    Failure::AccountSelection(format!("{error}; retry when the other add finishes"))
                },
            )?,
        )
    };
    let config = mv_config::load::load_config(&config_path).map_err(|error| {
        print_config_load_error(&error.to_string());
        Failure::Reported(NEEDS_CONFIG)
    })?;

    let raw_path = args.get_one::<String>("path").expect("path is required");
    let dry_run = args.get_flag("dry-run");
    let canonical = canonicalize_project_path(raw_path)?;
    let git = std::process::Command::new("git")
        .args(["-C"])
        .arg(&canonical)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    if !git.is_ok_and(|output| output.status.success() && output.stdout == b"true\n") {
        return Err(Failure::AccountSelection(format!(
            "{} is not a git repository. Writing roles need a git worktree to work in, and this project's first one would fail on that — minutes or days after `add` said this succeeded.\nRun `git init` in {} (or point `add` at the real repository root), then add it again.",
            canonical.display(), canonical.display()
        )));
    }

    let name = args
        .get_one::<String>("name")
        .cloned()
        .or_else(|| {
            canonical
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .ok_or_else(|| {
            Failure::Operation("could not derive a project name from the path".into())
        })?;

    register(&config_path, &config, &canonical, &name, dry_run)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_waits_for_config_sidecar_then_loads_the_latest_config() {
        use std::sync::mpsc;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .status()
            .unwrap()
            .success());
        std::fs::write(&config, "# initial config\n").unwrap();
        let lock =
            mv_core::lock::acquire_default(&dir.path().join("config.toml.lock"), "test").unwrap();
        let argv = vec![
            "multiverse".to_owned(),
            "--config".to_owned(),
            config.display().to_string(),
            "add".to_owned(),
            project.display().to_string(),
            "--name".to_owned(),
            "added".to_owned(),
        ];
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let args = crate::args::command().try_get_matches_from(argv).unwrap();
            tx.send(run(args.subcommand_matches("add").unwrap()).map_err(|e| e.code()))
                .unwrap();
        });
        let premature = rx.recv_timeout(std::time::Duration::from_millis(150));
        std::fs::write(
            &config,
            format!(
                "# concurrent edit\n[projects.added]\npath = {:?}\n",
                project.display().to_string()
            ),
        )
        .unwrap();
        drop(lock);
        let result = premature
            .as_ref()
            .ok()
            .cloned()
            .unwrap_or_else(|| rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        worker.join().unwrap();
        assert!(premature.is_err(), "add must wait for the config sidecar");
        assert_eq!(
            result,
            Err(3),
            "the latest config already registered this project"
        );
        assert_eq!(
            std::fs::read_to_string(config)
                .unwrap()
                .matches("[projects.added]")
                .count(),
            1
        );
    }

    /// Spec-cli.md §7's `toml_quote_key` regression, but for the
    /// *value* rather than the key: a project directory whose name
    /// contains a `"` must still produce a `config.toml` fragment the
    /// loader can read back, not one that breaks mid-string the way a
    /// raw `canonical_path.display()` interpolation does.
    #[test]
    fn register_escapes_a_quote_in_the_project_path_so_reload_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("proj\"x");
        std::fs::create_dir(&project_dir).unwrap();

        let toml = r#"
[accounts.seat]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/tmp"
pool = "codex:a"

[roles.scout]
[roles.scout.models]
codex = "m"
"#;
        let config = mv_config::load::load_config_str(toml, Path::new("config.toml")).unwrap();
        let block = register(
            Path::new("config.toml"),
            &config,
            &project_dir,
            "projx",
            true,
        )
        .unwrap();

        let reloaded =
            mv_config::load::load_config_str(&format!("{toml}{block}"), Path::new("config.toml"))
                .expect("the appended block must itself be loadable TOML");
        let project = &reloaded.projects["projx"];
        assert_eq!(project_path(project), Some(project_dir.as_path()));
    }

    fn dry_run_block(toml: &str) -> Result<String, Failure> {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("newrepo");
        std::fs::create_dir(&project_dir).unwrap();
        let config = mv_config::load::load_config_str(toml, Path::new("config.toml")).unwrap();
        register(
            Path::new("config.toml"),
            &config,
            &project_dir,
            "newrepo",
            true,
        )
    }

    fn role_models(roles: &[&str], kinds: &str) -> String {
        roles
            .iter()
            .map(|role| format!("[roles.{role}]\n[roles.{role}.models]\n{kinds}\n"))
            .collect()
    }

    /// Row #26: each role's documented vendor family leads when an
    /// eligible seat of it exists (`cli.py` `_LEAD_VENDOR_FAMILY`).
    #[test]
    fn add_leads_each_role_on_its_vendor_family() {
        let toml = format!(
            r#"
[accounts.claude-work]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/claude-work"
pool = "claude:work"

[accounts.chatgpt-personal]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/abs/chatgpt-personal"
pool = "chatgpt:personal"
short = "gp"

[accounts.chatgpt-work]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/abs/chatgpt-work"
pool = "chatgpt:work"
short = "gw"

[accounts.grok]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "/abs/grok"
pool = "grok"

[accounts.local]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/abs/local"
pool = "local"

{roles}
"#,
            roles = role_models(
                &[
                    "scout",
                    "seer",
                    "builder",
                    "critic",
                    "council",
                    "archivist",
                    "artisan",
                    "watcher",
                ],
                "claude = \"m\"\ncodex = \"m\"\ngrok = \"m\"",
            )
        );
        let block = dry_run_block(&toml).unwrap();
        let chains = block.split("[projects.newrepo.chains]").nth(1).unwrap();
        assert!(
            chains.contains(
                "scout = [\"claude-work\", \"chatgpt-personal\", \"chatgpt-work\", \"grok\", \"local\"]"
            ),
            "{chains}"
        );
        assert!(
            chains.contains(
                "builder = [\"claude-work\", \"chatgpt-personal\", \"chatgpt-work\", \"grok\", \"local\"]"
            ),
            "{chains}"
        );
        assert!(
            chains.contains(
                "critic = [\"chatgpt-personal\", \"chatgpt-work\", \"claude-work\", \"grok\", \"local\"]"
            ),
            "{chains}"
        );
        assert!(
            chains.contains("archivist = [\"grok\",") && chains.contains("\"local\"]"),
            "{chains}"
        );
        assert!(
            chains.contains("artisan = [\"chatgpt-personal\", \"chatgpt-work\", \"grok\","),
            "{chains}"
        );
        assert!(
            chains.contains(
                "seer = [\"claude-work\", \"chatgpt-personal\", \"chatgpt-work\", \"grok\"]"
            ),
            "{chains}"
        );
        assert!(
            !chains.contains("seer = [") || {
                let line = chains
                    .lines()
                    .find(|line| line.starts_with("seer = ["))
                    .unwrap();
                !line.contains("local")
            }
        );
        let watcher = chains
            .lines()
            .find(|line| line.starts_with("watcher = ["))
            .unwrap();
        assert!(!watcher.contains("local"), "{watcher}");
        assert!(
            chains.contains("council = [\"chatgpt-personal\", \"claude-work\", \"grok\"]")
                || chains.contains("council = [\"claude-work\", \"chatgpt-personal\", \"grok\"]"),
            "{chains}"
        );
        let council = chains
            .lines()
            .find(|line| line.starts_with("council = ["))
            .unwrap();
        assert!(!council.contains("chatgpt-work"), "{council}");
        assert!(!council.contains("local"), "{council}");
    }

    /// Row #26 rule 2/3: Critic must not lead on Builder's vendor; Seer
    /// then prefers off Critic's (forced) lead.
    #[test]
    fn add_forces_critic_off_builders_vendor_and_moves_seer() {
        let toml = r#"
[accounts.aa-grok]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "/abs/gr"
pool = "grok"

[accounts.zz-chatgpt]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/abs/cg"
pool = "chatgpt:work"

[roles.builder]
[roles.builder.models]
codex = "m"
[roles.critic]
[roles.critic.models]
codex = "m"
grok = "m"
[roles.seer]
[roles.seer.models]
codex = "m"
grok = "m"
"#;
        let block = dry_run_block(toml).unwrap();
        let chains = block.split("[projects.newrepo.chains]").nth(1).unwrap();
        assert!(chains.contains("builder = [\"zz-chatgpt\"]"), "{chains}");
        assert!(
            chains.contains("critic = [\"aa-grok\", \"zz-chatgpt\"]"),
            "{chains}"
        );
        assert!(
            chains.contains("seer = [\"zz-chatgpt\", \"aa-grok\"]"),
            "{chains}"
        );
        assert!(
            chains.contains("leads on a vendor other than builder's"),
            "{chains}"
        );
        assert!(chains.contains("moved off grok"), "{chains}");
    }

    /// Row #26 rule 2 honesty clause: one vendor cannot satisfy
    /// cross-vendor review, and the block says so.
    #[test]
    fn add_says_cross_vendor_review_is_not_achievable_with_one_vendor() {
        let toml = format!(
            r#"
[accounts.cl1]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/cl1"
pool = "claude:one"

[accounts.cl2]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/cl2"
pool = "claude:two"
short = "c2"

{roles}
"#,
            roles = role_models(&["builder", "critic"], "claude = \"m\"")
        );
        let block = dry_run_block(&toml).unwrap();
        let chains = block.split("[projects.newrepo.chains]").nth(1).unwrap();
        assert!(chains.contains("builder = [\"cl1\", \"cl2\"]"), "{chains}");
        assert!(chains.contains("critic = [\"cl1\", \"cl2\"]"), "{chains}");
        assert!(
            chains.contains("Cross-vendor review is not achievable"),
            "{chains}"
        );
    }

    /// Row #26: Nexus names the interactive seat only; other roles skip it.
    #[test]
    fn add_names_the_interactive_account_for_nexus_only() {
        let toml = format!(
            r#"
[accounts.seat-ix]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/seat-ix"
pool = "claude:interactive"
command = "seat-ix"
interactive = true

[accounts.claude]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/claude"
pool = "claude:work"

{roles}
"#,
            roles = role_models(&["nexus", "scout"], "claude = \"m\"")
        );
        let block = dry_run_block(&toml).unwrap();
        let chains = block.split("[projects.newrepo.chains]").nth(1).unwrap();
        assert!(chains.contains("nexus = [\"seat-ix\"]"), "{chains}");
        assert!(!chains.contains("nexus = [\"claude\"]"), "{chains}");
        assert!(chains.contains("scout = [\"claude\"]"), "{chains}");
        assert!(!chains.contains("scout = [\"seat-ix\"]"), "{chains}");
    }

    /// Row #26: an empty chain table is refused rather than written.
    #[test]
    fn add_refuses_when_no_role_can_be_served() {
        let toml = r#"
[accounts.down]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/abs/down"
pool = "claude:p"
available = false

[roles.scout]
[roles.scout.models]
claude = "m"
"#;
        let err = dry_run_block(toml).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err.to_string().contains("No role can be served"), "{err}");
    }
}
