//! `multiverse onboard`: interactive first-run setup.
//!
//! Writes a fleet `config.toml` from the shipped `config.example.toml`
//! (comments preserved via `toml_edit`), walks vendors, asks which seat is
//! the interactive Nexus seat, offers the model tiers from
//! `docs/ONBOARDING.md` §4 (never a guessed model id: the exact id is
//! asked for, or a tier placeholder is left for `check` to list), asks for
//! the first project, then offers that project the recommended chain set
//! from §3 ([`recommended_chain_lines`], the one source for both the
//! banner and the write) and registers it through
//! [`super::add::register_with_chains`], and optionally starts it.
//! `--non-interactive` applies those defaults; `--dry-run` prints the
//! plan without writing.
//!
//! An existing config is merged into, never replaced: its accounts,
//! projects, comments, and hand-set models are kept, new seats are added
//! with targeted edits, and the file is copied to `<name>.bak-<UTC stamp>`
//! first. One that already has accounts is left untouched unless `--yes`
//! (in either mode) or an interactive confirmation says otherwise. Every
//! answer is validated — the first project included — before anything is
//! written, so a refusal leaves the config as it was.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::edit;
use mv_config::load::{self, Account, Config};
use mv_config::placeholder;
use mv_config::roles::Role;
use mv_engine::selector;
use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use super::add::{self, canonicalize_project_path};
use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use super::start;
use crate::exit::{Failure, ERROR, OK};

const EXAMPLE: &str = include_str!("../../../../config.example.toml");

/// Day-one cheat sheet printed at the end of every successful run
/// (including `--dry-run`). Keep in sync with `docs/cheat-sheet.md`.
const CHEAT_SHEET: &str = "\
Day-one cheat sheet (see docs/cheat-sheet.md):
  multiverse start [<project>...] [--all]   open home + each project's Nexus
  multiverse board [--all]                  BLOCKED / IN PROGRESS / BACKLOG
  multiverse workers                        live worker panes (auto-reconciles)
  multiverse tasks <project> --reconcile    record outcomes dispatch gave up on
  multiverse backlog list                   open and asked cards
  multiverse backlog add \"<title>\" --why \"<evidence>\"
  multiverse activity inbox                 unread inter-project mail
  multiverse check [--verbose]              readiness; exit 0 ready, 3 needs config
  multiverse down / up                      wrap the fleet, then bring it back
  multiverse stop [<project>...] --force    close busy sessions
  multiverse worktrees <path> --prune [--dry-run]  reclaim finished lanes
";

/// Model tiers from ONBOARDING.md §4. The doc is vendor-neutral on
/// purpose ("model names change") and says never to make up a model id,
/// so onboard writes the tier, not a guessed id: the exact id is asked
/// for, and a blank answer (or `--non-interactive`) leaves a placeholder
/// that `multiverse check` lists until it is filled in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    Capable,
    Fast,
}

impl Tier {
    fn of(role: Role) -> Self {
        match role {
            Role::Scout | Role::Archivist | Role::Watcher => Self::Fast,
            Role::Builder
            | Role::Seer
            | Role::Critic
            | Role::Artisan
            | Role::Council
            | Role::Nexus => Self::Capable,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Capable => "capable",
            Self::Fast => "fast/cheap",
        }
    }

    fn roles(self) -> &'static str {
        match self {
            Self::Capable => "Builder, Seer, Critic, Artisan, Council, Nexus",
            Self::Fast => "Scout, Archivist, Watcher",
        }
    }

    fn placeholder(self, kind: &str) -> String {
        format!(
            "<<PLACEHOLDER: {kind} model id for the {} tier>>",
            self.label()
        )
    }
}

/// The model id onboard writes for each seat kind and tier: what the user
/// typed, or the tier's placeholder.
struct ModelIds {
    capable: BTreeMap<String, String>,
    fast: BTreeMap<String, String>,
}

impl ModelIds {
    fn get(&self, tier: Tier, kind: &str) -> String {
        let ids = match tier {
            Tier::Capable => &self.capable,
            Tier::Fast => &self.fast,
        };
        ids.get(kind)
            .cloned()
            .unwrap_or_else(|| tier.placeholder(kind))
    }
}

/// The local seat's model is whatever its server serves; never guessed.
const LOCAL_MODEL_PLACEHOLDER: &str = "<<PLACEHOLDER: local model id>>";

/// The rules behind [`recommended_chain_lines`]. Written as the chain
/// table's first comment, so the banner (which prints that table) and the
/// config onboard writes always say the same thing.
const CHAIN_RULES: &str =
    "critic off builder's vendor, artisan off the house vendor (builder's lead), \
read-only roles on the cheap seat first, local only last in scout and archivist";

/// ONBOARDING.md §3 "A recommended default set": each worker role's
/// vendors in preference order, in the order the doc lists the roles.
/// Nexus is always the interactive seat and is not listed here.
const RECOMMENDED: &[(Role, &[&str])] = &[
    (Role::Builder, &["claude", "chatgpt"]),
    (Role::Seer, &["chatgpt", "claude"]),
    (Role::Critic, &["chatgpt", "grok"]),
    (Role::Artisan, &["grok", "chatgpt"]),
    (Role::Scout, &["grok", "claude", "local"]),
    (Role::Archivist, &["grok", "chatgpt", "local"]),
    (Role::Watcher, &["claude", "chatgpt"]),
    (Role::Council, &["claude", "chatgpt", "grok"]),
];

struct Options {
    non_interactive: bool,
    dry_run: bool,
    yes: bool,
    project: Option<String>,
    name: Option<String>,
    start: bool,
}

impl Options {
    fn from_args(args: &ArgMatches) -> Self {
        Self {
            non_interactive: args.get_flag("non-interactive"),
            dry_run: args.get_flag("dry-run"),
            yes: args.get_flag("yes"),
            project: args.get_one::<String>("project").cloned(),
            name: args.get_one::<String>("name").cloned(),
            start: args.get_flag("start"),
        }
    }
}

#[derive(Clone, Copy)]
struct Vendor {
    id: &'static str,
    account: &'static str,
    kind: &'static str,
    config_dir_var: &'static str,
    default_home_suffix: &'static str,
    pool: &'static str,
    short: Option<&'static str>,
    command: Option<&'static str>,
    default_include: bool,
}

const VENDORS: &[Vendor] = &[
    Vendor {
        id: "claude",
        account: "claude-personal",
        kind: "claude",
        config_dir_var: "CLAUDE_CONFIG_DIR",
        default_home_suffix: ".claude-personal",
        pool: "claude:personal",
        short: None,
        command: Some("claude-personal"),
        default_include: true,
    },
    Vendor {
        id: "codex",
        account: "chatgpt-personal",
        kind: "codex",
        config_dir_var: "CODEX_HOME",
        default_home_suffix: ".codex-personal",
        pool: "chatgpt:personal",
        short: Some("gp"),
        command: None,
        default_include: true,
    },
    Vendor {
        id: "grok",
        account: "grok",
        kind: "grok",
        config_dir_var: "GROK_HOME",
        default_home_suffix: ".grok",
        pool: "grok",
        short: None,
        command: None,
        default_include: true,
    },
    Vendor {
        id: "local",
        account: "local",
        kind: "codex",
        config_dir_var: "CODEX_HOME",
        default_home_suffix: ".codex-local",
        pool: "local",
        short: None,
        command: Some("codex --oss --local-provider ollama"),
        default_include: false,
    },
];

const DROP_ACCOUNTS: &[&str] = &["claude-work", "chatgpt-work"];

/// How many times a prompt re-asks after an answer it cannot use before
/// onboard gives up.
const ATTEMPTS: usize = 3;

struct SeatPlan {
    vendor: Vendor,
    config_dir: String,
    /// The local seat's model id (asked, or its placeholder); `None` for
    /// cloud seats, whose models come from `[roles.*.models]`.
    model: Option<String>,
}

trait Prompt {
    fn confirm(
        &mut self,
        out: &mut dyn Write,
        question: &str,
        default: bool,
    ) -> Result<bool, Failure>;
    fn ask(
        &mut self,
        out: &mut dyn Write,
        question: &str,
        default: &str,
    ) -> Result<String, Failure>;
}

struct Defaults;

impl Prompt for Defaults {
    fn confirm(
        &mut self,
        _out: &mut dyn Write,
        _question: &str,
        default: bool,
    ) -> Result<bool, Failure> {
        Ok(default)
    }
    fn ask(
        &mut self,
        _out: &mut dyn Write,
        _question: &str,
        default: &str,
    ) -> Result<String, Failure> {
        Ok(default.to_owned())
    }
}

struct StdinPrompt<'a> {
    input: &'a mut dyn BufRead,
}

impl StdinPrompt<'_> {
    /// One answer line. End of input is a refusal, never an empty answer:
    /// taking the default on EOF would let a closed stdin walk every
    /// remaining question (and re-ask loops) on defaults nobody chose.
    fn read_answer(&mut self, out: &mut dyn Write, question: &str) -> Result<String, Failure> {
        let mut line = String::new();
        let read = self.input.read_line(&mut line).map_err(io_fail)?;
        if read == 0 {
            writeln!(out).map_err(io_fail)?;
            return Err(Failure::Operation(format!(
                "stdin closed before an answer to {question:?}; onboard stopped there"
            )));
        }
        Ok(line.trim().to_owned())
    }
}

impl Prompt for StdinPrompt<'_> {
    fn confirm(
        &mut self,
        out: &mut dyn Write,
        question: &str,
        default: bool,
    ) -> Result<bool, Failure> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let mut last = String::new();
        for _ in 0..ATTEMPTS {
            write!(out, "{question} {hint} ").map_err(io_fail)?;
            out.flush().map_err(io_fail)?;
            let answer = self.read_answer(out, question)?;
            let trimmed = answer.as_str();
            if trimmed.is_empty() {
                return Ok(default);
            }
            match trimmed.to_ascii_lowercase().as_str() {
                "y" | "yes" => return Ok(true),
                "n" | "no" => return Ok(false),
                other => {
                    writeln!(out, "Please answer y or n (got {other:?}).").map_err(io_fail)?;
                    last = other.to_owned();
                }
            }
        }
        Err(Failure::Operation(format!(
            "expected y or n, got {last:?} ({ATTEMPTS} tries)"
        )))
    }

    fn ask(
        &mut self,
        out: &mut dyn Write,
        question: &str,
        default: &str,
    ) -> Result<String, Failure> {
        write!(out, "{question} [{default}]: ").map_err(io_fail)?;
        out.flush().map_err(io_fail)?;
        let answer = self.read_answer(out, question)?;
        if answer.is_empty() {
            Ok(default.to_owned())
        } else {
            Ok(answer)
        }
    }
}

fn io_fail(error: io::Error) -> Failure {
    Failure::Operation(format!("onboard i/o failed: {error}"))
}

fn home_dir() -> Result<PathBuf, Failure> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Failure::Config("HOME is unset; cannot derive default account paths".into()))
}

/// Public entry: real stdin/stdout.
pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let stdin = io::stdin();
    run_with(
        args,
        &mut io::stdout(),
        &mut stdin.lock(),
        io::stdin().is_terminal(),
    )
}

fn run_with(
    args: &ArgMatches,
    out: &mut dyn Write,
    input: &mut dyn BufRead,
    stdin_is_terminal: bool,
) -> Result<(), Failure> {
    let options = Options::from_args(args);
    if !options.non_interactive && !stdin_is_terminal {
        return Err(Failure::Operation(
            "onboard is interactive; pass --non-interactive to apply defaults".into(),
        ));
    }

    let (config_path, source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    writeln!(out, "Config location ({source}): {}", config_path.display()).map_err(io_fail)?;

    // An existing config is merged into, never replaced: its accounts,
    // projects, comments, and hand-set models survive, and it is backed
    // up before the write. Only a TOML parse failure stops here — onboard
    // will not overwrite a file it cannot read back.
    let existing = read_existing(&config_path)?;
    let kept = existing.as_ref().map(kept_accounts).unwrap_or_default();
    if let Some(doc) = &existing {
        let names = account_names(doc);
        if !names.is_empty() {
            writeln!(
                out,
                "{} already has {} account(s): {}.",
                config_path.display(),
                names.len(),
                names.join(", ")
            )
            .map_err(io_fail)?;
            let allowed = if options.yes {
                true
            } else if options.non_interactive {
                false
            } else {
                let mut prompt = StdinPrompt { input };
                prompt.confirm(
                    out,
                    "Merge new seats into this config (existing accounts and projects are kept; a backup is written first)",
                    false,
                )?
            };
            if !allowed {
                return Err(Failure::Config(if options.non_interactive {
                    format!(
                        "{} already has accounts; re-run with --yes to merge new seats into it (existing accounts and projects are kept)",
                        config_path.display()
                    )
                } else {
                    format!(
                        "{} already has accounts; merge declined, nothing written. Re-run and answer y (or pass --yes) to merge new seats into it",
                        config_path.display()
                    )
                }));
            }
        }
    }

    let home = home_dir()?;
    let kept_names: BTreeSet<&str> = kept.iter().map(|account| account.name.as_str()).collect();
    let mut seats = Vec::new();
    if options.non_interactive {
        let mut prompt = Defaults;
        collect_seats(&mut prompt, &home, &kept_names, out, &mut seats)?;
    } else {
        let mut prompt = StdinPrompt { input };
        collect_seats(&mut prompt, &home, &kept_names, out, &mut seats)?;
    }
    if seats.is_empty() && kept.is_empty() {
        return Err(Failure::Config(
            "no vendors selected; onboard needs at least one seat".into(),
        ));
    }

    let interactive_seat = match kept.iter().find(|account| account.interactive) {
        Some(account) => {
            writeln!(
                out,
                "Keeping {} as the interactive (Nexus) seat.",
                account.name
            )
            .map_err(io_fail)?;
            None
        }
        None if options.non_interactive => choose_interactive(&mut Defaults, out, &seats)?,
        None => choose_interactive(&mut StdinPrompt { input }, out, &seats)?,
    };

    let kinds: BTreeSet<&str> = seats
        .iter()
        .map(|seat| seat.vendor.kind)
        .chain(kept.iter().map(|account| account.kind.as_str()))
        .collect();
    show_model_plan(out, &kinds)?;
    let model_ids = if options.non_interactive {
        writeln!(
            out,
            "Writing tier placeholders for every model id (non-interactive default); fill each in before dispatch."
        )
        .map_err(io_fail)?;
        Some(collect_model_ids(&mut Defaults, out, &kinds)?)
    } else {
        let mut prompt = StdinPrompt { input };
        if prompt.confirm(out, "Write these [roles.*.models] tables", true)? {
            Some(collect_model_ids(&mut prompt, out, &kinds)?)
        } else {
            None
        }
    };
    let write_models = model_ids.is_some();

    let merge = existing.is_some();
    let mut doc = match existing {
        Some(doc) => doc,
        None => EXAMPLE.parse().map_err(|error| {
            Failure::Operation(format!(
                "internal error: shipped config.example.toml is not TOML: {error}"
            ))
        })?,
    };
    apply_seats(&mut doc, &seats, interactive_seat)?;
    let dropped = drop_unused_accounts(&mut doc, &seats, merge)?;
    if merge && !dropped.is_empty() {
        writeln!(
            out,
            "Dropping unfilled placeholder account(s): {}.",
            dropped.join(", ")
        )
        .map_err(io_fail)?;
    }
    match &model_ids {
        Some(ids) => apply_models(&mut doc, &kinds, ids)?,
        None => writeln!(out, "Skipping model-tier writes.").map_err(io_fail)?,
    }
    if !merge {
        drop_example_project(&mut doc)?;
    }

    let rendered = doc.to_string();
    writeln!(
        out,
        "{} {}:",
        if options.dry_run {
            "Would write"
        } else {
            "Writing"
        },
        config_path.display()
    )
    .map_err(io_fail)?;
    writeln!(out, "  accounts: {}", seat_names(&seats).join(", ")).map_err(io_fail)?;
    if !kept.is_empty() {
        let names: Vec<&str> = kept.iter().map(|account| account.name.as_str()).collect();
        writeln!(out, "  kept accounts: {}", names.join(", ")).map_err(io_fail)?;
    }
    writeln!(
        out,
        "  roles with models: {}",
        match (write_models, merge) {
            (true, false) => "all nine (the model ids given, or tier placeholders)",
            (true, true) =>
                "all nine (the model ids given, or tier placeholders; models you already set are kept)",
            (false, false) => "(unchanged from example)",
            (false, true) => "(unchanged from the existing config)",
        }
    )
    .map_err(io_fail)?;
    if merge {
        writeln!(
            out,
            "  existing projects, accounts, and comments are preserved; a backup is written first."
        )
        .map_err(io_fail)?;
    } else {
        writeln!(out, "  comments from config.example.toml are preserved.").map_err(io_fail)?;
    }

    // Everything below up to `write_config` only reads: every refusal
    // from here on leaves the config exactly as it was.
    let loaded = load::load_config_str(&rendered, &config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;
    let servable = recommended_chain_lines(&loaded).is_some();
    if !write_models && !servable {
        return Err(Failure::Config(format!(
            "declining the recommended model tiers leaves no role any worker seat can serve \
             ([roles.*.models] has no entry for a non-interactive seat's kind); nothing written to {}. \
             Re-run onboard and accept the tiers, or set [roles.*.models] by hand",
            config_path.display()
        )));
    }
    probe_seats(out, &loaded, &config_path)?;

    // The project comes first: its chain table is only offered once there
    // is a project to write it for, under that project's own name.
    let project_spec = resolve_first_project(&options, &loaded, out, input)?;
    let write_chains = match &project_spec {
        Some(spec) if spec.add => {
            // "Would write" only before a confirmation or on --dry-run;
            // a non-interactive write is actually writing.
            let would = !options.non_interactive || options.dry_run;
            show_chain_plan(out, &loaded, &spec.name, would)?;
            if options.non_interactive {
                writeln!(
                    out,
                    "{} these chains with {} (non-interactive default).",
                    if would { "Would write" } else { "Writing" },
                    spec.name
                )
                .map_err(io_fail)?;
                true
            } else {
                let mut prompt = StdinPrompt { input };
                prompt.confirm(
                    out,
                    &format!("Write this [projects.{}.chains] table", spec.name),
                    true,
                )?
            }
        }
        _ => false,
    };
    if let Some(spec) = &project_spec {
        if spec.add && write_chains && !servable {
            return Err(Failure::AccountSelection(format!(
                "No role can be served by any available account, so {}'s chain table would be empty; nothing written to {}.\n\
                 Add a second (non-interactive) seat, or decline the chain table to register the project without one.",
                spec.name,
                config_path.display()
            )));
        }
    }

    if !options.dry_run {
        if merge {
            let backup = backup_config(&config_path)?;
            writeln!(
                out,
                "Backed up the existing config to {}.",
                backup.display()
            )
            .map_err(io_fail)?;
        }
        write_config(&config_path, rendered.as_bytes())?;
        writeln!(out, "Wrote {}.", config_path.display()).map_err(io_fail)?;
    } else {
        writeln!(out, "Dry-run: config not written.").map_err(io_fail)?;
    }

    if let Some(spec) = project_spec {
        let name = spec.name.as_str();
        if spec.add {
            add_first_project(
                &options,
                &config_path,
                &loaded,
                &spec.path,
                name,
                write_chains,
                out,
            )?;
        } else {
            writeln!(
                out,
                "Project {name:?} is already registered at {}; leaving it as is.",
                spec.path.display()
            )
            .map_err(io_fail)?;
        }
        let should_start = if options.non_interactive {
            options.start
        } else {
            let mut prompt = StdinPrompt { input };
            prompt.confirm(out, &format!("Run `multiverse start {name}` now"), true)?
        };
        if should_start {
            if options.dry_run {
                writeln!(out, "Dry-run: would run `multiverse start {name}`.").map_err(io_fail)?;
            } else {
                start_project(args, name, out)?;
            }
        }
    } else {
        writeln!(
            out,
            "No first project added. Register one later with `multiverse add <path>`."
        )
        .map_err(io_fail)?;
    }

    writeln!(out).map_err(io_fail)?;
    write!(out, "{CHEAT_SHEET}").map_err(io_fail)?;
    Ok(())
}

/// The existing config's document, or `None` when there is no file yet.
/// A file that is not TOML is refused rather than replaced.
fn read_existing(config_path: &Path) -> Result<Option<DocumentMut>, Failure> {
    if !config_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(config_path).map_err(|error| {
        Failure::Operation(format!("could not read {}: {error}", config_path.display()))
    })?;
    text.parse().map(Some).map_err(|error| {
        Failure::Config(format!(
            "{} exists but is not valid TOML, so onboard will not touch it: {error}\nFix or move it, then re-run onboard.",
            config_path.display()
        ))
    })
}

fn account_names(doc: &DocumentMut) -> Vec<String> {
    let mut names: Vec<String> = doc
        .get("accounts")
        .and_then(Item::as_table_like)
        .map(|accounts| accounts.iter().map(|(name, _)| name.to_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// An account already in the config with a real (non-placeholder)
/// `config_dir`: onboard keeps it as written and does not re-ask for it.
struct KeptAccount {
    name: String,
    kind: String,
    interactive: bool,
}

fn kept_accounts(doc: &DocumentMut) -> Vec<KeptAccount> {
    let Some(accounts) = doc.get("accounts").and_then(Item::as_table_like) else {
        return Vec::new();
    };
    let mut kept: Vec<KeptAccount> = accounts
        .iter()
        .filter_map(|(name, item)| {
            let table = item.as_table_like()?;
            let config_dir = table.get("config_dir").and_then(Item::as_str)?;
            if placeholder::is_placeholder(config_dir) {
                return None;
            }
            Some(KeptAccount {
                name: name.to_owned(),
                kind: table
                    .get("kind")
                    .and_then(Item::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                interactive: table
                    .get("interactive")
                    .and_then(Item::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect();
    kept.sort_by(|a, b| a.name.cmp(&b.name));
    kept
}

/// Copy `config_path` to `<name>.bak-<UTC stamp>` beside it before a
/// merge rewrites it, so even a merge the user did not expect is
/// recoverable. Never overwrites an earlier backup.
fn backup_config(config_path: &Path) -> Result<PathBuf, Failure> {
    let now = mv_core::time::now_utc();
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    );
    let file_name = config_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let mut backup = config_path.with_file_name(format!("{file_name}.bak-{stamp}"));
    let mut n = 1;
    while backup.exists() {
        backup = config_path.with_file_name(format!("{file_name}.bak-{stamp}-{n}"));
        n += 1;
    }
    std::fs::copy(config_path, &backup).map_err(|error| {
        Failure::Operation(format!(
            "could not back up {} to {}: {error}; nothing written",
            config_path.display(),
            backup.display()
        ))
    })?;
    Ok(backup)
}

fn collect_seats(
    prompt: &mut impl Prompt,
    home: &Path,
    kept: &BTreeSet<&str>,
    out: &mut dyn Write,
    seats: &mut Vec<SeatPlan>,
) -> Result<(), Failure> {
    writeln!(
        out,
        "Vendors: claude, codex (ChatGPT), grok, local. Each step is skippable."
    )
    .map_err(io_fail)?;
    for vendor in VENDORS {
        if kept.contains(vendor.account) {
            writeln!(out, "Keeping existing [accounts.{}].", vendor.account).map_err(io_fail)?;
            continue;
        }
        let default_dir = home.join(vendor.default_home_suffix).display().to_string();
        let include = prompt.confirm(
            out,
            &format!("Include a {} seat (`{}`)", vendor.id, vendor.account),
            vendor.default_include,
        )?;
        if !include {
            writeln!(out, "Skipping {}.", vendor.id).map_err(io_fail)?;
            continue;
        }
        let config_dir = prompt.ask(
            out,
            &format!(
                "{} for {} (absolute; ~ is not expanded)",
                vendor.config_dir_var, vendor.account
            ),
            &default_dir,
        )?;
        if !config_dir.starts_with('/') {
            return Err(Failure::Config(format!(
                "accounts.{}.config_dir must be an absolute path, got {config_dir:?}",
                vendor.account
            )));
        }
        writeln!(out, "Would write:").map_err(io_fail)?;
        writeln!(out, "  [accounts.{}]", vendor.account).map_err(io_fail)?;
        writeln!(out, "  kind = {:?}", vendor.kind).map_err(io_fail)?;
        writeln!(out, "  config_dir_var = {:?}", vendor.config_dir_var).map_err(io_fail)?;
        writeln!(out, "  config_dir = {config_dir:?}").map_err(io_fail)?;
        writeln!(out, "  pool = {:?}", vendor.pool).map_err(io_fail)?;
        if let Some(command) = vendor.command {
            writeln!(out, "  command = {command:?}").map_err(io_fail)?;
        }
        let model = if vendor.id == "local" {
            Some(prompt.ask(
                out,
                "Exact model id your local server serves (blank keeps a placeholder)",
                LOCAL_MODEL_PLACEHOLDER,
            )?)
        } else {
            None
        };
        if let Some(model) = &model {
            writeln!(out, "  model = {model:?}").map_err(io_fail)?;
            writeln!(out, "  health_url = \"http://127.0.0.1:11434/\"").map_err(io_fail)?;
            writeln!(out, "  roles_allowed = [\"scout\", \"archivist\"]").map_err(io_fail)?;
            writeln!(out, "  max_brief_chars = 5000").map_err(io_fail)?;
        }
        let write = prompt.confirm(out, "Write this seat", true)?;
        if !write {
            writeln!(out, "Skipping {}.", vendor.id).map_err(io_fail)?;
            continue;
        }
        seats.push(SeatPlan {
            vendor: *vendor,
            config_dir,
            model,
        });
    }
    Ok(())
}

fn seat_names(seats: &[SeatPlan]) -> Vec<&'static str> {
    seats.iter().map(|seat| seat.vendor.account).collect()
}

/// Which included seat `multiverse start` runs each project's Nexus on.
/// Asked, never assumed; the default is the first included cloud seat (a
/// local seat cannot be the Nexus). `--non-interactive` takes the default
/// and says which it took.
fn choose_interactive(
    prompt: &mut impl Prompt,
    out: &mut dyn Write,
    seats: &[SeatPlan],
) -> Result<Option<&'static str>, Failure> {
    let candidates: Vec<&'static str> = seats
        .iter()
        .filter(|seat| seat.vendor.id != "local")
        .map(|seat| seat.vendor.account)
        .collect();
    let Some(&default) = candidates.first() else {
        writeln!(
            out,
            "No cloud seat included, so no account is marked `interactive = true`; `multiverse start` needs one."
        )
        .map_err(io_fail)?;
        return Ok(None);
    };
    let choices = candidates.join(", ");
    for _ in 0..ATTEMPTS {
        let answer = prompt.ask(
            out,
            &format!(
                "Interactive (Nexus) seat, the one `multiverse start` opens for you to talk to ({choices})"
            ),
            default,
        )?;
        if let Some(&name) = candidates.iter().find(|name| **name == answer) {
            writeln!(out, "Interactive (Nexus) seat: {name}.").map_err(io_fail)?;
            return Ok(Some(name));
        }
        writeln!(out, "{answer:?} is not an included seat ({choices}).").map_err(io_fail)?;
    }
    Err(Failure::Config(format!(
        "no interactive seat chosen after {ATTEMPTS} tries; nothing was written"
    )))
}

/// `interactive` is the seat chosen by [`choose_interactive`]; `None` when
/// a kept account already holds the interactive (Nexus) seat, so a merge
/// never adds a second one.
fn apply_seats(
    doc: &mut DocumentMut,
    seats: &[SeatPlan],
    interactive: Option<&str>,
) -> Result<(), Failure> {
    for seat in seats {
        ensure_account(doc, seat)?;
        let table = account_table(doc, seat.vendor.account)?;
        table["kind"] = value(seat.vendor.kind);
        table["config_dir_var"] = value(seat.vendor.config_dir_var);
        table["config_dir"] = value(seat.config_dir.as_str());
        table["pool"] = value(seat.vendor.pool);
        table["available"] = value(true);
        if let Some(short) = seat.vendor.short {
            table["short"] = value(short);
        }
        if let Some(command) = seat.vendor.command {
            table["command"] = value(command);
        }
        table["interactive"] = value(interactive == Some(seat.vendor.account));
        if let Some(model) = &seat.model {
            table["model"] = value(model.as_str());
            table["health_url"] = value("http://127.0.0.1:11434/");
            table["max_brief_chars"] = value(5000);
            let mut allowed = Array::new();
            allowed.push("scout");
            allowed.push("archivist");
            table["roles_allowed"] = Item::Value(Value::Array(allowed));
        }
    }
    Ok(())
}

fn ensure_account(doc: &mut DocumentMut, seat: &SeatPlan) -> Result<(), Failure> {
    let accounts = doc
        .get("accounts")
        .and_then(Item::as_table)
        .map(|table| table.contains_key(seat.vendor.account))
        .unwrap_or(false);
    if accounts {
        return Ok(());
    }
    let mut fields: Vec<(&str, &str)> = vec![
        ("kind", seat.vendor.kind),
        ("config_dir_var", seat.vendor.config_dir_var),
        ("config_dir", seat.config_dir.as_str()),
        ("pool", seat.vendor.pool),
    ];
    if let Some(short) = seat.vendor.short {
        fields.push(("short", short));
    }
    if let Some(command) = seat.vendor.command {
        fields.push(("command", command));
    }
    edit::append_account(doc, seat.vendor.account, &fields)
        .map_err(|error| Failure::Config(error.to_string()))
}

fn account_table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, Failure> {
    doc.get_mut("accounts")
        .and_then(Item::as_table_mut)
        .and_then(|accounts| accounts.get_mut(name))
        .and_then(Item::as_table_mut)
        .ok_or_else(|| {
            Failure::Config(format!(
                "internal error: accounts.{name} missing after write"
            ))
        })
}

/// Fresh configs drop every example account not selected. A merge drops
/// only unselected accounts whose `config_dir` is still a placeholder —
/// unfilled example scaffolding, never an account the user configured.
/// Returns the dropped names.
fn drop_unused_accounts(
    doc: &mut DocumentMut,
    seats: &[SeatPlan],
    merge: bool,
) -> Result<Vec<String>, Failure> {
    let keep: BTreeSet<&str> = seats.iter().map(|seat| seat.vendor.account).collect();
    let Some(accounts) = doc.get_mut("accounts").and_then(Item::as_table_like_mut) else {
        return Ok(Vec::new());
    };
    let names: Vec<String> = accounts.iter().map(|(name, _)| name.to_string()).collect();
    let mut dropped = Vec::new();
    for name in names {
        let selected = keep.contains(name.as_str());
        let drop = if merge {
            let config_dir = accounts
                .get(&name)
                .and_then(Item::as_table_like)
                .and_then(|table| table.get("config_dir"))
                .and_then(Item::as_str);
            !selected && config_dir.is_none_or(placeholder::is_placeholder)
        } else {
            !selected || DROP_ACCOUNTS.contains(&name.as_str())
        };
        if drop {
            accounts.remove(&name);
            dropped.push(name);
        }
    }
    Ok(dropped)
}

fn drop_example_project(doc: &mut DocumentMut) -> Result<(), Failure> {
    if let Some(projects) = doc.get_mut("projects").and_then(Item::as_table_mut) {
        projects.remove("example");
    }
    Ok(())
}

fn show_model_plan(out: &mut dyn Write, kinds: &BTreeSet<&str>) -> Result<(), Failure> {
    writeln!(
        out,
        "Recommended model tiers for seat kinds: {}",
        kinds.iter().copied().collect::<Vec<_>>().join(", ")
    )
    .map_err(io_fail)?;
    for tier in [Tier::Capable, Tier::Fast] {
        writeln!(
            out,
            "  {} → each vendor's {} tier",
            tier.roles(),
            tier.label()
        )
        .map_err(io_fail)?;
    }
    writeln!(
        out,
        "  Model ids are not guessed: give the exact id each vendor's CLI accepts, or leave the \
         placeholder (`multiverse check` lists every one still unfilled; Watcher's must accept images)."
    )
    .map_err(io_fail)?;
    Ok(())
}

/// The exact model id for each kind and tier. A blank answer keeps the
/// tier's placeholder, which is also what [`Defaults`] answers.
fn collect_model_ids(
    prompt: &mut impl Prompt,
    out: &mut dyn Write,
    kinds: &BTreeSet<&str>,
) -> Result<ModelIds, Failure> {
    let mut ids = ModelIds {
        capable: BTreeMap::new(),
        fast: BTreeMap::new(),
    };
    for kind in kinds {
        for tier in [Tier::Capable, Tier::Fast] {
            let answer = prompt.ask(
                out,
                &format!(
                    "Exact {kind} model id for the {} tier ({}; blank keeps a placeholder)",
                    tier.label(),
                    tier.roles()
                ),
                &tier.placeholder(kind),
            )?;
            let slot = match tier {
                Tier::Capable => &mut ids.capable,
                Tier::Fast => &mut ids.fast,
            };
            slot.insert((*kind).to_owned(), answer);
        }
    }
    Ok(ids)
}

fn apply_models(
    doc: &mut DocumentMut,
    kinds: &BTreeSet<&str>,
    ids: &ModelIds,
) -> Result<(), Failure> {
    for role in Role::ALL {
        let tier = Tier::of(role);
        let models: Vec<(&str, String)> = kinds
            .iter()
            .map(|kind| (*kind, ids.get(tier, kind)))
            .collect();
        upsert_role(doc, role, &models, kinds)?;
    }
    Ok(())
}

fn upsert_role(
    doc: &mut DocumentMut,
    role: Role,
    models: &[(&str, String)],
    kinds: &BTreeSet<&str>,
) -> Result<(), Failure> {
    let roles = doc
        .as_table_mut()
        .entry("roles")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| Failure::Config("roles is not a table".into()))?;
    let key = role.config_key();
    if !roles.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(false);
        table["write_scope"] = value(role_write_scope(role));
        if let Some(effort) = role_effort(role) {
            table["effort"] = value(effort);
        }
        roles.insert(key, Item::Table(table));
    }
    let table = roles
        .get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| Failure::Config(format!("roles.{key} is not a table")))?;
    if table.get("write_scope").is_none() {
        table["write_scope"] = value(role_write_scope(role));
    }
    // Targeted edits: fill a kind that is missing or still a placeholder
    // (with the id given, or this tier's placeholder), keep any model the
    // user already set, and drop leftover placeholders for kinds no seat
    // has.
    let model_table = table
        .entry("models")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(false);
            Item::Table(table)
        })
        .as_table_like_mut()
        .ok_or_else(|| Failure::Config(format!("roles.{key}.models is not a table")))?;
    for (kind, model) in models {
        let current = model_table.get(kind).and_then(Item::as_str);
        if current.is_none_or(placeholder::is_placeholder) {
            model_table.insert(kind, value(model.as_str()));
        }
    }
    let stale: Vec<String> = model_table
        .iter()
        .filter(|(kind, item)| {
            !kinds.contains(kind) && item.as_str().is_some_and(placeholder::is_placeholder)
        })
        .map(|(kind, _)| kind.to_owned())
        .collect();
    for kind in stale {
        model_table.remove(&kind);
    }
    if model_table.is_empty() {
        return Err(Failure::Config(format!(
            "roles.{key}.models would be empty with the selected vendors"
        )));
    }
    Ok(())
}

fn role_write_scope(role: Role) -> &'static str {
    match role {
        Role::Builder | Role::Artisan => "declared",
        _ => "none",
    }
}

fn role_effort(role: Role) -> Option<&'static str> {
    match role {
        Role::Scout | Role::Archivist | Role::Watcher => Some("low"),
        Role::Builder | Role::Seer | Role::Critic => Some("high"),
        Role::Artisan | Role::Council | Role::Nexus => Some("medium"),
    }
}

fn write_config(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Failure::Operation(format!("could not create {}: {error}", parent.display()))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let original_mode = path.is_file().then(|| file_mode(path)).transpose()?;
    // Resolve a symlink target the same way `add::register` does, so a
    // config.toml that is a link keeps being a link.
    let resolved = if path.exists() {
        std::fs::canonicalize(path).map_err(|error| {
            Failure::Operation(format!("could not resolve {}: {error}", path.display()))
        })?
    } else {
        path.to_path_buf()
    };
    mv_core::atomic::write_atomic(&resolved, bytes).map_err(|error| {
        Failure::Operation(format!("could not write {}: {error}", resolved.display()))
    })?;
    if let Some(mode) = original_mode {
        restore_file_mode(&resolved, mode)?;
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(path: &Path) -> Result<u32, Failure> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode())
        .map_err(|error| Failure::Operation(format!("could not stat {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn file_mode(_path: &Path) -> Result<(), Failure> {
    Ok(())
}

#[cfg(unix)]
fn restore_file_mode(path: &Path, mode: u32) -> Result<(), Failure> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|error| {
        Failure::Operation(format!(
            "could not restore permissions on {}: {error}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn restore_file_mode(_path: &Path, _mode: ()) -> Result<(), Failure> {
    Ok(())
}

const NOT_LOGGED_IN: &str = "not logged in";
const LOGIN_UNKNOWN: &str = "login unknown";

/// What a seat's config dir says about its login. `multiverse accounts`
/// cannot tell (it never reads the dir), so a seat nobody ever signed in
/// to would otherwise read `available`. `None` means a login was found, or
/// none is needed (the local seat, or a placeholder dir `check` already
/// reports). A missing or empty dir has never been logged in; Codex keeps
/// its login in `auth.json`; Claude in `.credentials.json`, or (macOS
/// keychain) an `oauthAccount` in `.claude.json`; anything else is
/// unknown rather than assumed.
fn login_gap(account: &Account) -> Option<&'static str> {
    if account.vendor == "local" {
        return None;
    }
    let dir = Path::new(account.config_dir.value()?);
    let has_entries = std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some());
    if !has_entries {
        return Some(NOT_LOGGED_IN);
    }
    match account.kind.as_str() {
        "codex" => (!dir.join("auth.json").is_file()).then_some(NOT_LOGGED_IN),
        "claude" => {
            let signed_in = dir.join(".credentials.json").is_file()
                || std::fs::read_to_string(dir.join(".claude.json"))
                    .is_ok_and(|text| text.contains("\"oauthAccount\""));
            (!signed_in).then_some(LOGIN_UNKNOWN)
        }
        _ => Some(LOGIN_UNKNOWN),
    }
}

fn sign_in_hint(account: &Account) -> String {
    let dir = account.config_dir.as_raw();
    match account.kind.as_str() {
        "claude" => format!("CLAUDE_CONFIG_DIR={dir} claude, then /login"),
        "codex" => format!("CODEX_HOME={dir} codex login"),
        _ => format!(
            "start the {} CLI with {}={dir} and sign in",
            account.kind, account.config_dir_var
        ),
    }
}

fn probe_seats(out: &mut dyn Write, config: &Config, config_path: &Path) -> Result<(), Failure> {
    writeln!(
        out,
        "Verifying seats (same probe as `multiverse accounts`, plus a login check of each config dir):"
    )
    .map_err(io_fail)?;
    writeln!(
        out,
        "{:<18} {:<16} {:<14} CONFIG DIR",
        "ACCOUNT", "POOL", "STATE"
    )
    .map_err(io_fail)?;
    let state_dir = state_dir_from_config(config_path);
    let gauges = mv_state::gauge::load(&state_dir);
    let mut accounts: Vec<_> = config.accounts.values().collect();
    accounts.sort_by(|a, b| a.name.cmp(&b.name));
    let mut unconfirmed = Vec::new();
    for account in accounts {
        let mut state = super::accounts::account_state(
            config,
            &state_dir,
            &gauges,
            account,
            None,
            &selector::HttpLocalLivenessProbe,
        );
        if state == "available" {
            if let Some(gap) = login_gap(account) {
                state = gap.to_owned();
                unconfirmed.push(account);
            }
        }
        let marker = if account.config_dir.is_placeholder() {
            "  ← placeholder"
        } else {
            ""
        };
        writeln!(
            out,
            "{:<18} {:<16} {:<14} {}={}{marker}",
            account.name,
            account.pool,
            state,
            account.config_dir_var,
            account.config_dir.as_raw()
        )
        .map_err(io_fail)?;
    }
    for account in unconfirmed {
        writeln!(
            out,
            "  {}: no login found in its config dir; sign in once: {}",
            account.name,
            sign_in_hint(account)
        )
        .map_err(io_fail)?;
    }
    Ok(())
}

fn serves(config: &Config, role: Role, account: &Account) -> bool {
    config
        .roles
        .get(&role)
        .is_some_and(|settings| settings.models.contains_key(&account.kind))
}

/// The recommended chain for each worker role ([`RECOMMENDED`], in its
/// order), restricted to the seats this config has, plus the house vendor
/// (Builder's lead). A seat is eligible when it is available, not the
/// interactive seat (unless `lend_interactive_to_workers`), its kind has a
/// model for the role, and its `roles_allowed` admits the role; a local
/// seat only where the role's list names `local`. A role none of whose
/// recommended vendors is present falls back to every eligible seat,
/// cloud first. Critic and Artisan then lead off the house vendor when a
/// seat of another vendor exists; Council keeps one seat per vendor.
fn recommended_chains(config: &Config) -> (Vec<(Role, Vec<&Account>)>, Option<String>) {
    let mut chains = Vec::new();
    let mut house: Option<String> = None;
    for &(role, vendors) in RECOMMENDED {
        if !config.roles.contains_key(&role) {
            continue;
        }
        let eligible: Vec<&Account> = config
            .accounts
            .values()
            .filter(|account| {
                account.available
                    && (!account.interactive || config.lend_interactive_to_workers)
                    && serves(config, role, account)
                    && account
                        .roles_allowed
                        .as_ref()
                        .is_none_or(|allowed| allowed.contains(&role))
                    && (account.vendor != "local" || vendors.contains(&"local"))
            })
            .collect();
        let mut seats: Vec<&Account> = vendors
            .iter()
            .flat_map(|vendor| {
                eligible
                    .iter()
                    .copied()
                    .filter(move |account| account.vendor == *vendor)
            })
            .collect();
        if seats.is_empty() {
            seats = eligible
                .iter()
                .copied()
                .filter(|account| account.vendor != "local")
                .chain(
                    eligible
                        .iter()
                        .copied()
                        .filter(|account| account.vendor == "local"),
                )
                .collect();
        }
        if role == Role::Council {
            let mut seen = BTreeSet::new();
            seats.retain(|account| seen.insert(account.vendor.clone()));
        }
        if matches!(role, Role::Critic | Role::Artisan) {
            if let Some(house) = &house {
                if let Some(index) = seats.iter().position(|account| &account.vendor != house) {
                    let lead = seats.remove(index);
                    seats.insert(0, lead);
                }
            }
        }
        if role == Role::Builder {
            house = seats.first().map(|account| account.vendor.clone());
        }
        if !seats.is_empty() {
            chains.push((role, seats));
        }
    }
    (chains, house)
}

/// The `[projects.<name>.chains]` body onboard prints in its banner and
/// writes: the ONBOARDING.md §3 set for these seats. `None` when no role
/// has a seat, which the caller refuses rather than write an empty table.
fn recommended_chain_lines(config: &Config) -> Option<String> {
    let mut lines = vec![format!("# Recommended set: {CHAIN_RULES}.")];
    let mut has_chain = false;
    if config.roles.contains_key(&Role::Nexus) {
        match config.accounts.values().find(|account| account.interactive) {
            Some(seat) => {
                lines.push(format!("nexus = [{:?}]", seat.name));
                has_chain = true;
            }
            None => lines.push(
                "# nexus: no account is marked `interactive = true`; `start` runs the \
                 Nexus on that seat, so mark one and add the line yourself."
                    .into(),
            ),
        }
    }
    let (chains, house) = recommended_chains(config);
    for (role, seats) in chains {
        if matches!(role, Role::Critic | Role::Artisan)
            && house
                .as_deref()
                .is_some_and(|house| seats[0].vendor == house)
        {
            lines.push(format!(
                "# {}: every eligible seat is {}, builder's vendor; add a seat from \
                 another vendor to take it off the house vendor.",
                role.config_key(),
                seats[0].vendor
            ));
        }
        let names: Vec<String> = seats
            .iter()
            .map(|account| format!("{:?}", account.name))
            .collect();
        lines.push(format!("{} = [{}]", role.config_key(), names.join(", ")));
        has_chain = true;
    }
    has_chain.then(|| {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    })
}

fn show_chain_plan(
    out: &mut dyn Write,
    config: &Config,
    name: &str,
    would: bool,
) -> Result<(), Failure> {
    match recommended_chain_lines(config) {
        Some(body) => {
            writeln!(
                out,
                "Recommended chains for {name}. {} [projects.{name}.chains]:",
                if would { "Would write" } else { "Writing" }
            )
            .map_err(io_fail)?;
            for line in body.lines() {
                writeln!(out, "  {line}").map_err(io_fail)?;
            }
        }
        None => writeln!(
            out,
            "No role can be served yet — add seats whose kinds appear in [roles.*.models]."
        )
        .map_err(io_fail)?,
    }
    Ok(())
}

/// The first project, validated before anything is written: an existing
/// git repository whose name and path do not collide with a different
/// configured project. `add` is false when exactly this name and path are
/// already registered (a re-run), which is left as is.
struct ProjectSpec {
    path: PathBuf,
    name: String,
    add: bool,
}

fn resolve_first_project(
    options: &Options,
    config: &Config,
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Result<Option<ProjectSpec>, Failure> {
    if let Some(raw) = options.project.as_deref() {
        let path = canonicalize_project_path(raw)?;
        let name = options
            .name
            .clone()
            .or_else(|| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .ok_or_else(|| {
                Failure::Operation("could not derive a project name from --project".into())
            })?;
        if !is_git_repo(&path) {
            return Err(Failure::AccountSelection(format!(
                "{}\nNothing was written.",
                not_git_message(&path)
            )));
        }
        let add = project_conflict(config, &path, &name)
            .map_err(|message| Failure::Config(format!("{message}; nothing was written")))?;
        return Ok(Some(ProjectSpec { path, name, add }));
    }
    if options.non_interactive {
        return Ok(None);
    }
    let mut prompt = StdinPrompt { input };
    if !prompt.confirm(out, "Add a first project now", true)? {
        return Ok(None);
    }
    let cwd = std::env::current_dir()
        .map_err(|error| Failure::Operation(format!("cannot read current directory: {error}")))?;
    let default = cwd.display().to_string();
    for _ in 0..ATTEMPTS {
        let raw = prompt.ask(out, "Project path", &default)?;
        let path = match std::fs::canonicalize(&raw) {
            Ok(path) if path.is_dir() => path,
            _ => {
                writeln!(
                    out,
                    "{raw:?} is not an existing directory. Try another path."
                )
                .map_err(io_fail)?;
                continue;
            }
        };
        if !is_git_repo(&path) {
            writeln!(out, "{}\nTry another path.", not_git_message(&path)).map_err(io_fail)?;
            continue;
        }
        let default_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into());
        let name = prompt.ask(out, "Project name", &default_name)?;
        match project_conflict(config, &path, &name) {
            Ok(add) => return Ok(Some(ProjectSpec { path, name, add })),
            Err(message) => {
                writeln!(out, "{message}. Try again.").map_err(io_fail)?;
            }
        }
    }
    Err(Failure::Config(format!(
        "no usable first project after {ATTEMPTS} tries; nothing was written. \
         Re-run onboard, or decline the first project and use `multiverse add <path>` later"
    )))
}

/// `Ok(true)` to add, `Ok(false)` when this exact name and path are
/// already registered, `Err` when either collides with another project.
fn project_conflict(config: &Config, path: &Path, name: &str) -> Result<bool, String> {
    let same_path = |project: &mv_config::load::Project| {
        project_path(project)
            .and_then(|configured| std::fs::canonicalize(configured).ok())
            .is_some_and(|configured| configured == path)
    };
    if let Some(project) = config.projects.get(name) {
        if same_path(project) {
            return Ok(false);
        }
        return Err(format!("a project named {name:?} is already configured"));
    }
    if let Some(project) = config.projects.values().find(|project| same_path(project)) {
        return Err(format!(
            "{} is already registered as project {:?}",
            path.display(),
            project.name
        ));
    }
    Ok(true)
}

fn is_git_repo(path: &Path) -> bool {
    let git = std::process::Command::new("git")
        .args(["-C"])
        .arg(path)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    git.is_ok_and(|output| output.status.success() && output.stdout == b"true\n")
}

fn not_git_message(path: &Path) -> String {
    format!(
        "{} is not a git repository. Writing roles need a git worktree to work in, and this project's first one would fail on that — minutes or days after onboard said this succeeded.\nRun `git init` in {} (or point onboard at the real repository root), then add it again.",
        path.display(),
        path.display()
    )
}

fn add_first_project(
    options: &Options,
    config_path: &Path,
    config: &Config,
    path: &Path,
    name: &str,
    write_chains: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    if write_chains {
        add::register_with_chains(
            config_path,
            config,
            path,
            name,
            recommended_chain_lines(config),
            options.dry_run,
        )?;
        return Ok(());
    }
    let block = format!("\n[projects.{name}]\npath = \"{}\"\n", path.display());
    writeln!(
        out,
        "{} {}:{block}",
        if options.dry_run {
            "Would append to"
        } else {
            "Writing project (no chains) to"
        },
        config_path.display()
    )
    .map_err(io_fail)?;
    if options.dry_run {
        return Ok(());
    }
    let text = std::fs::read_to_string(config_path).map_err(|error| {
        Failure::Operation(format!("could not read {}: {error}", config_path.display()))
    })?;
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        Failure::Config(format!(
            "could not re-parse {}: {error}",
            config_path.display()
        ))
    })?;
    edit::append_project(&mut doc, name, &[("path", &path.display().to_string())])
        .map_err(|error| Failure::Config(error.to_string()))?;
    write_config(config_path, doc.to_string().as_bytes())?;
    writeln!(out, "Added project {name:?} without a chain table.").map_err(io_fail)?;
    Ok(())
}

fn start_project(args: &ArgMatches, name: &str, out: &mut dyn Write) -> Result<(), Failure> {
    writeln!(out, "Starting {name}…").map_err(io_fail)?;
    let mut argv = vec!["multiverse".to_owned(), "start".to_owned(), name.to_owned()];
    if let Some(config) = args.get_one::<String>("config") {
        argv.splice(1..1, ["--config".to_owned(), config.clone()]);
    }
    let matches = crate::args::command()
        .try_get_matches_from(&argv)
        .map_err(|error| {
            Failure::Operation(format!("could not parse start invocation: {error}"))
        })?;
    match start::run(matches.subcommand_matches("start").expect("start")) {
        Ok(()) => Ok(()),
        Err(error) => {
            // `start` reports most refusals itself and returns a bare
            // `Reported` code whose Display is empty; never print an empty
            // reason, and carry its exit code out instead of exit 0.
            let code = match error.code() {
                OK => ERROR,
                code => code,
            };
            let reason = error.to_string();
            if reason.is_empty() {
                writeln!(
                    out,
                    "start failed: `multiverse start {name}` exited {code}; its reason is printed above."
                )
                .map_err(io_fail)?;
            } else {
                writeln!(out, "start failed: {reason}").map_err(io_fail)?;
            }
            writeln!(
                out,
                "The config and project {name:?} are written; fix that, then run `multiverse start {name}`."
            )
            .map_err(io_fail)?;
            Err(Failure::Reported(code))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;
    use crate::exit::NEEDS_CONFIG;
    use mv_config::placeholder;
    use std::ffi::OsString;
    use std::fs;

    struct Env {
        _herdr: HerdrEnvGuard,
        dir: tempfile::TempDir,
        previous_home: Option<OsString>,
        previous_state: Option<OsString>,
        previous_config: Option<OsString>,
    }

    impl Env {
        fn new() -> Self {
            // `HerdrEnvGuard::unreachable` already holds `herdr_env::lock()`.
            let herdr = HerdrEnvGuard::unreachable();
            let dir = tempfile::tempdir().unwrap();
            let previous_home = std::env::var_os("HOME");
            let previous_state = std::env::var_os("MULTIVERSE_STATE_DIR");
            let previous_config = std::env::var_os("MULTIVERSE_CONFIG");
            let home = dir.path().join("home");
            let state = dir.path().join("state");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&state).unwrap();
            std::env::set_var("HOME", &home);
            std::env::set_var("MULTIVERSE_STATE_DIR", &state);
            std::env::remove_var("MULTIVERSE_CONFIG");
            Self {
                _herdr: herdr,
                dir,
                previous_home,
                previous_state,
                previous_config,
            }
        }

        fn config_path(&self) -> PathBuf {
            self.dir.path().join("config.toml")
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.previous_state.take() {
                Some(value) => std::env::set_var("MULTIVERSE_STATE_DIR", value),
                None => std::env::remove_var("MULTIVERSE_STATE_DIR"),
            }
            match self.previous_config.take() {
                Some(value) => std::env::set_var("MULTIVERSE_CONFIG", value),
                None => std::env::remove_var("MULTIVERSE_CONFIG"),
            }
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap()
            .subcommand_matches("onboard")
            .unwrap()
            .clone()
    }

    fn run_onboard(argv: &[&str]) -> Result<String, Failure> {
        let args = matches(argv);
        let mut out = Vec::new();
        run_with(&args, &mut out, &mut io::empty(), false).map(|()| String::from_utf8(out).unwrap())
    }

    /// Accounts and projects are filled in; every role model is a tier
    /// placeholder, because onboard never guesses a model id.
    fn only_model_placeholders(config: &Config) {
        for account in config.accounts.values() {
            assert!(
                !account.config_dir.is_placeholder(),
                "{} still a placeholder",
                account.name
            );
        }
        for (role, settings) in &config.roles {
            for (kind, model) in &settings.models {
                assert!(
                    placeholder::is_placeholder(model) && model.contains(" tier>>"),
                    "roles.{}.{kind} = {model:?} is not a tier placeholder",
                    role.config_key()
                );
            }
        }
        for project in config.projects.values() {
            assert!(
                !project.path.is_placeholder(),
                "project {} path still a placeholder",
                project.name
            );
        }
    }

    #[test]
    fn dry_run_writes_nothing() {
        let env = Env::new();
        let config = env.config_path();
        let before = snapshot(env.dir.path());
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap();
        assert!(!config.exists(), "dry-run must not create the config");
        assert_eq!(snapshot(env.dir.path()), before);
        assert!(out.contains("Dry-run: config not written."));
        assert!(out.contains("Day-one cheat sheet"));
        assert!(
            out.contains("# Herdr Multiverse") || out.contains("comments from config.example.toml")
        );
    }

    fn git_init(path: &Path) {
        fs::create_dir_all(path).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    #[test]
    fn non_interactive_writes_a_config_check_accepts() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("sandbox");
        git_init(&project);
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
            "--name",
            "sandbox",
        ])
        .unwrap();
        assert!(config.is_file(), "{out}");
        let text = fs::read_to_string(&config).unwrap();
        assert!(
            text.contains("# Herdr Multiverse — example configuration"),
            "comments from the example must survive"
        );
        let loaded = load::load_config(&config).unwrap();
        assert!(!loaded.accounts.is_empty());
        assert!(loaded.projects.contains_key("sandbox"));
        assert!(!loaded.projects.contains_key("example"));
        assert!(loaded.accounts.contains_key("claude-personal"));
        assert!(loaded.accounts.contains_key("chatgpt-personal"));
        assert!(loaded.accounts.contains_key("grok"));
        assert!(!loaded.accounts.contains_key("claude-work"));
        assert!(loaded.accounts["claude-personal"].interactive);
        only_model_placeholders(&loaded);
        assert!(out.contains("Verifying seats"));
        assert!(out.contains("available") || out.contains("STATE"));

        // Once the user fills in the model ids, nothing else is missing.
        let filled: String = fs::read_to_string(&config)
            .unwrap()
            .lines()
            .map(|line| {
                if !line.starts_with('#') && line.contains(" tier>>\"") {
                    let key = line.split('=').next().unwrap();
                    format!("{key}= \"test-model\"\n")
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        fs::write(&config, filled).unwrap();
        let check_args = crate::args::command()
            .try_get_matches_from(["multiverse", "check", "--config", config.to_str().unwrap()])
            .unwrap();
        super::super::check::run(check_args.subcommand_matches("check").unwrap())
            .expect("check must accept the onboarded config (host warnings only)");
    }

    #[test]
    fn existing_accounts_are_refused_without_yes() {
        let env = Env::new();
        let config = env.config_path();
        fs::write(
            &config,
            "[accounts.already]\n\
             kind = \"codex\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:a\"\n",
        )
        .unwrap();
        let original = fs::read_to_string(&config).unwrap();
        let err = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap_err();
        assert_eq!(err.code(), NEEDS_CONFIG);
        assert!(err.to_string().contains("already has accounts"));
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
    }

    fn run_full(argv: &[&str], input: &str, terminal: bool) -> (Result<(), Failure>, String) {
        let args = matches(argv);
        let mut out = Vec::new();
        let mut input = io::Cursor::new(input.as_bytes().to_vec());
        let result = run_with(&args, &mut out, &mut input, terminal);
        (result, String::from_utf8(out).unwrap())
    }

    /// Answers for a fresh interactive run: default claude/codex/grok
    /// seats, skip local, the default Nexus seat, then `models` for the
    /// model-tier prompt and, unless that is declined, a blank
    /// (placeholder-keeping) id for each of the three kinds' two tiers.
    fn seat_answers(models: &str) -> String {
        let ids = if models == "n" { "" } else { "\n\n\n\n\n\n" };
        format!("\n\n\n\n\n\n\n\n\n\n\n{models}\n{ids}")
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("config.toml.bak-")
            })
            .collect()
    }

    // Item 1: a merge keeps projects, accounts, comments, and hand-set
    // models, and backs the old file up first.
    #[test]
    fn yes_merges_into_an_existing_config_and_backs_it_up() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("keepme");
        git_init(&project);
        let original = format!(
            "# my own notes\n\
             [accounts.already]\n\
             kind = \"codex\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:a\"\n\
             \n\
             [roles.scout]\n\
             write_scope = \"none\"\n\
             \n\
             [roles.scout.models]\n\
             codex = \"my-model\"\n\
             \n\
             [projects.keepme]\n\
             path = \"{}\"\n",
            fs::canonicalize(&project).unwrap().display()
        );
        fs::write(&config, &original).unwrap();
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--yes",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap();
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("# my own notes"), "{text}");
        let loaded = load::load_config(&config).unwrap();
        assert!(loaded.projects.contains_key("keepme"), "{text}");
        assert!(loaded.accounts.contains_key("already"));
        assert!(loaded.accounts.contains_key("claude-personal"));
        assert!(loaded.accounts.contains_key("grok"));
        assert_eq!(loaded.roles[&Role::Scout].models["codex"], "my-model");
        assert_eq!(
            loaded.roles[&Role::Scout].models["claude"],
            "<<PLACEHOLDER: claude model id for the fast/cheap tier>>"
        );
        let backups = backups(env.dir.path());
        assert_eq!(backups.len(), 1, "{out}");
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), original);
        assert!(out.contains(&format!(
            "Backed up the existing config to {}",
            backups[0].display()
        )));
    }

    #[test]
    fn a_config_without_accounts_keeps_its_projects() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("keepme");
        git_init(&project);
        fs::write(
            &config,
            format!(
                "[projects.keepme]\npath = \"{}\"\n",
                fs::canonicalize(&project).unwrap().display()
            ),
        )
        .unwrap();
        run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap();
        let loaded = load::load_config(&config).unwrap();
        assert!(loaded.projects.contains_key("keepme"));
        assert!(loaded.accounts["claude-personal"].interactive);
        assert_eq!(backups(env.dir.path()).len(), 1);
    }

    #[test]
    fn a_config_that_is_not_toml_is_never_replaced() {
        let env = Env::new();
        let config = env.config_path();
        fs::write(&config, "this is = = not toml\n").unwrap();
        let err = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--yes",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap_err();
        assert_eq!(err.code(), NEEDS_CONFIG);
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "this is = = not toml\n"
        );
    }

    fn existing_account_config(env: &Env) -> (PathBuf, String) {
        let config = env.config_path();
        let original = "[accounts.already]\n\
                        kind = \"codex\"\n\
                        config_dir_var = \"CODEX_HOME\"\n\
                        config_dir = \"/tmp\"\n\
                        pool = \"codex:a\"\n"
            .to_owned();
        fs::write(&config, &original).unwrap();
        (config, original)
    }

    // Item 2: interactive mode honours --yes, and its refusal names the
    // action that actually works there.
    #[test]
    fn interactive_yes_skips_the_merge_question() {
        let env = Env::new();
        let (config, _) = existing_account_config(&env);
        let (result, out) = run_full(
            &["onboard", "--yes", "--config", config.to_str().unwrap()],
            &format!("{}n\n", seat_answers("")),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        assert!(!out.contains("this config [y/N]"), "{out}");
        let loaded = load::load_config(&config).unwrap();
        assert!(loaded.accounts.contains_key("already"));
        assert!(loaded.accounts.contains_key("claude-personal"));
    }

    #[test]
    fn interactive_decline_leaves_the_config_and_names_the_real_action() {
        let env = Env::new();
        let (config, original) = existing_account_config(&env);
        let (result, _) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            "n\n",
            true,
        );
        let err = result.unwrap_err();
        assert_eq!(err.code(), NEEDS_CONFIG);
        let message = err.to_string();
        assert!(message.contains("answer y"), "{message}");
        assert!(!message.contains("re-run with --yes"), "{message}");
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert!(backups(env.dir.path()).is_empty());
    }

    // Item 3: a non-git project is refused (or re-asked) before any write.
    #[test]
    fn non_git_project_flag_is_refused_before_the_config_is_written() {
        let env = Env::new();
        let config = env.config_path();
        let plain = env.dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let err = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
            "--project",
            plain.to_str().unwrap(),
        ])
        .unwrap_err();
        assert_eq!(err.code(), NEEDS_CONFIG);
        assert!(err.to_string().contains("not a git repository"));
        assert!(
            !config.exists(),
            "nothing may be written before the refusal"
        );
    }

    #[test]
    fn interactive_non_git_path_is_asked_again() {
        let env = Env::new();
        let config = env.config_path();
        let plain = env.dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let repo = env.dir.path().join("repo");
        git_init(&repo);
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!(
                "{}\n{}\n{}\n\n\nn\n",
                seat_answers(""),
                plain.display(),
                repo.display()
            ),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        assert!(out.contains("not a git repository"), "{out}");
        let loaded = load::load_config(&config).unwrap();
        assert!(loaded.projects.contains_key("repo"));
    }

    #[test]
    fn interactive_non_git_path_gives_up_with_nothing_written() {
        let env = Env::new();
        let config = env.config_path();
        let plain = env.dir.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let path = plain.display();
        let (result, _) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!("{}\n{path}\n{path}\n{path}\n", seat_answers("")),
            true,
        );
        assert_eq!(result.unwrap_err().code(), NEEDS_CONFIG);
        assert!(!config.exists());
    }

    // Item 4: an unusable y/n answer is asked again, a bounded number of
    // times.
    #[test]
    fn confirm_reasks_a_bad_answer_then_gives_up() {
        let mut out = Vec::new();
        let mut input = io::Cursor::new(b"maybe\nyes\n".to_vec());
        let mut prompt = StdinPrompt { input: &mut input };
        assert!(prompt.confirm(&mut out, "Go", false).unwrap());
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("Please answer y or n"));

        let mut out = Vec::new();
        let mut input = io::Cursor::new(b"a\nb\nc\ny\n".to_vec());
        let mut prompt = StdinPrompt { input: &mut input };
        assert!(prompt.confirm(&mut out, "Go", true).is_err());
    }

    // End of input is a refusal, not a default answer, for both kinds of
    // question.
    #[test]
    fn eof_on_stdin_is_a_refusal_not_a_default() {
        let mut out = Vec::new();
        let mut input = io::empty();
        let mut prompt = StdinPrompt { input: &mut input };
        let err = prompt.confirm(&mut out, "Go", true).unwrap_err();
        assert!(err.to_string().contains("stdin closed"), "{err}");
        let err = prompt.ask(&mut out, "Name", "x").unwrap_err();
        assert!(err.to_string().contains("stdin closed"), "{err}");
    }

    // Item 5: declining tiers into an unservable config, or a failed
    // start, ends non-zero with a real reason.
    #[test]
    fn declining_model_tiers_with_no_servable_role_is_refused() {
        let env = Env::new();
        let config = env.config_path();
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!("{}\nn\n", seat_answers("n")),
            true,
        );
        let err = result.unwrap_err();
        assert_eq!(err.code(), NEEDS_CONFIG, "{out}");
        assert!(
            err.to_string()
                .contains("declining the recommended model tiers"),
            "{err}"
        );
        assert!(!config.exists());
    }

    #[test]
    fn a_failed_start_exits_non_zero_with_a_reason() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("sandbox");
        git_init(&project);
        let (result, out) = run_full(
            &[
                "onboard",
                "--non-interactive",
                "--config",
                config.to_str().unwrap(),
                "--project",
                project.to_str().unwrap(),
                "--start",
            ],
            "",
            false,
        );
        let err = result.unwrap_err();
        assert_ne!(err.code(), crate::exit::OK);
        let line = out
            .lines()
            .find(|line| line.starts_with("start failed:"))
            .unwrap_or_else(|| panic!("{out}"));
        assert!(line.len() > "start failed: ".len(), "{line}");
        assert!(config.is_file());
    }

    fn chain(loaded: &Config, project: &str, role: Role) -> Vec<String> {
        loaded.projects[project]
            .chains
            .get(&role)
            .cloned()
            .unwrap_or_default()
    }

    // Card aae367 item 5: the first project is asked for before its chain
    // table is offered, and declining the project offers no chain table.
    #[test]
    fn the_first_project_is_asked_for_before_its_chains() {
        let env = Env::new();
        let config = env.config_path();
        let repo = env.dir.path().join("repo");
        git_init(&repo);
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!("{}\n{}\n\n\nn\n", seat_answers(""), repo.display()),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        let project_at = out.find("Add a first project now").expect(&out);
        let chains_at = out.find(".chains] table").expect(&out);
        assert!(project_at < chains_at, "{out}");
        assert!(out.contains("[projects.repo.chains]"), "{out}");
        let loaded = load::load_config(&config).unwrap();
        assert!(!loaded.projects["repo"].chains.is_empty());

        drop(env); // releases herdr_env::lock() before the next Env takes it
        let env = Env::new();
        let config = env.config_path();
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!("{}n\n", seat_answers("")),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        assert!(!out.contains(".chains]"), "{out}");
    }

    fn fleet_config(lend: bool) -> Config {
        let mut toml = format!(
            "lend_interactive_to_workers = {lend}\n\
             [accounts.claude-personal]\nkind = \"claude\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"/tmp/c\"\npool = \"claude:personal\"\ncommand = \"claude-personal\"\ninteractive = true\n\
             [accounts.chatgpt-personal]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp/x\"\npool = \"chatgpt:personal\"\nshort = \"gp\"\n\
             [accounts.grok]\nkind = \"grok\"\nconfig_dir_var = \"GROK_HOME\"\nconfig_dir = \"/tmp/g\"\npool = \"grok\"\n\
             [accounts.local]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"/tmp/l\"\n\
             pool = \"local\"\ncommand = \"codex --oss --local-provider ollama\"\nmodel = \"m\"\n\
             health_url = \"http://127.0.0.1:11434/\"\nroles_allowed = [\"scout\", \"archivist\"]\nmax_brief_chars = 5000\n"
        );
        for role in Role::ALL {
            toml.push_str(&format!(
                "[roles.{key}]\nwrite_scope = \"{scope}\"\n[roles.{key}.models]\n\
                 claude = \"a\"\ncodex = \"b\"\ngrok = \"c\"\n",
                key = role.config_key(),
                scope = role_write_scope(role)
            ));
        }
        load::load_config_str(&toml, Path::new("/tmp/config.toml")).unwrap()
    }

    fn chain_entries(body: &str) -> Vec<&str> {
        body.lines().filter(|line| !line.starts_with('#')).collect()
    }

    // Card aae367 item 6: for the fleet ONBOARDING.md §3 describes (one
    // Claude seat lent to workers, one ChatGPT, one Grok, one local), the
    // written chains are exactly the doc's recommended default set.
    #[test]
    fn recommended_chains_are_the_onboarding_default_set() {
        let body = recommended_chain_lines(&fleet_config(true)).unwrap();
        assert_eq!(
            chain_entries(&body),
            [
                r#"nexus = ["claude-personal"]"#,
                r#"builder = ["claude-personal", "chatgpt-personal"]"#,
                r#"seer = ["chatgpt-personal", "claude-personal"]"#,
                r#"critic = ["chatgpt-personal", "grok"]"#,
                r#"artisan = ["grok", "chatgpt-personal"]"#,
                r#"scout = ["grok", "claude-personal", "local"]"#,
                r#"archivist = ["grok", "chatgpt-personal", "local"]"#,
                r#"watcher = ["claude-personal", "chatgpt-personal"]"#,
                r#"council = ["claude-personal", "chatgpt-personal", "grok"]"#,
            ],
            "{body}"
        );
        assert!(body.starts_with(&format!("# Recommended set: {CHAIN_RULES}.")));

        // Not lent: the interactive Claude seat leaves every worker chain,
        // and the house vendor becomes chatgpt; critic and artisan still
        // lead off it.
        let body = recommended_chain_lines(&fleet_config(false)).unwrap();
        assert_eq!(
            chain_entries(&body),
            [
                r#"nexus = ["claude-personal"]"#,
                r#"builder = ["chatgpt-personal"]"#,
                r#"seer = ["chatgpt-personal"]"#,
                r#"critic = ["grok", "chatgpt-personal"]"#,
                r#"artisan = ["grok", "chatgpt-personal"]"#,
                r#"scout = ["grok", "local"]"#,
                r#"archivist = ["grok", "chatgpt-personal", "local"]"#,
                r#"watcher = ["chatgpt-personal"]"#,
                r#"council = ["chatgpt-personal", "grok"]"#,
            ],
            "{body}"
        );
    }

    // Card aae367 item 6: with onboard's own default seats the written
    // chains keep critic and artisan off builder's vendor (artisan used to
    // land on it), and the banner prints exactly the table that is written.
    #[test]
    fn default_seats_write_the_chains_the_banner_shows() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("sandbox");
        git_init(&project);
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
            "--name",
            "sandbox",
        ])
        .unwrap();
        let loaded = load::load_config(&config).unwrap();
        let vendor = |name: &String| loaded.accounts[name].vendor.clone();
        let house = vendor(&chain(&loaded, "sandbox", Role::Builder)[0]);
        for role in [Role::Critic, Role::Artisan] {
            let lead = vendor(&chain(&loaded, "sandbox", role)[0]);
            assert_ne!(
                lead,
                house,
                "{} leads on builder's vendor\n{out}",
                role.config_key()
            );
        }
        assert_eq!(
            chain(&loaded, "sandbox", Role::Artisan),
            ["grok", "chatgpt-personal"]
        );
        assert_eq!(chain(&loaded, "sandbox", Role::Scout), ["grok"]);

        let banner: Vec<&str> = out
            .lines()
            .skip_while(|line| !line.contains("Writing [projects.sandbox.chains]:"))
            .skip(1)
            .take_while(|line| line.starts_with("  "))
            .map(|line| &line[2..])
            .collect();
        let text = fs::read_to_string(&config).unwrap();
        let written: Vec<&str> = text
            .lines()
            .skip_while(|line| *line != "[projects.sandbox.chains]")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .collect();
        assert!(!banner.is_empty(), "{out}");
        assert_eq!(banner, written, "{out}\n{text}");
    }

    // Card aae367 item 7: onboard asks which included seat is the
    // interactive Nexus seat (re-asking an answer that is not one), rather
    // than silently marking the first.
    #[test]
    fn the_interactive_nexus_seat_is_asked_for() {
        let env = Env::new();
        let config = env.config_path();
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!("{}nope\ngrok\n\n{}n\n", "\n".repeat(10), "\n".repeat(6)),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        assert!(out.contains("Interactive (Nexus) seat, the one"), "{out}");
        assert!(out.contains("\"nope\" is not an included seat"), "{out}");
        let loaded = load::load_config(&config).unwrap();
        assert!(loaded.accounts["grok"].interactive);
        assert!(!loaded.accounts["claude-personal"].interactive);
        assert!(!loaded.accounts["chatgpt-personal"].interactive);

        drop(env); // releases herdr_env::lock() before the next Env takes it
        let env = Env::new();
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--dry-run",
            "--config",
            env.config_path().to_str().unwrap(),
        ])
        .unwrap();
        assert!(
            out.contains("Interactive (Nexus) seat: claude-personal."),
            "{out}"
        );
    }

    // Card aae367 item 8: no model id is guessed. Non-interactive writes
    // tier placeholders; interactive writes exactly the ids typed,
    // including the local seat's.
    #[test]
    fn model_ids_are_asked_for_never_guessed() {
        let env = Env::new();
        let config = env.config_path();
        run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
        ])
        .unwrap();
        let text = fs::read_to_string(&config).unwrap();
        for guessed in [
            "\"opus\"",
            "\"haiku\"",
            "\"gpt-5\"",
            "\"grok-4\"",
            "llama3.2",
        ] {
            assert!(!text.contains(guessed), "{guessed} written\n{text}");
        }
        let loaded = load::load_config(&config).unwrap();
        assert_eq!(
            loaded.roles[&Role::Builder].models["codex"],
            "<<PLACEHOLDER: codex model id for the capable tier>>"
        );

        drop(env); // releases herdr_env::lock() before the next Env takes it
        let env = Env::new();
        let config = env.config_path();
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            &format!(
                "{}y\n\nmy-local\n\n\n\ncap-claude\nfast-claude\ncap-codex\nfast-codex\ncap-grok\nfast-grok\nn\n",
                "\n".repeat(9)
            ),
            true,
        );
        result.unwrap_or_else(|error| panic!("{error}\n{out}"));
        let loaded = load::load_config(&config).unwrap();
        assert_eq!(loaded.roles[&Role::Builder].models["claude"], "cap-claude");
        assert_eq!(loaded.roles[&Role::Nexus].models["grok"], "cap-grok");
        assert_eq!(loaded.roles[&Role::Scout].models["codex"], "fast-codex");
        assert_eq!(loaded.roles[&Role::Watcher].models["grok"], "fast-grok");
        assert_eq!(loaded.accounts["local"].model.as_deref(), Some("my-local"));
    }

    fn seat_row<'a>(out: &'a str, account: &str) -> &'a str {
        out.lines()
            .find(|line| line.starts_with(&format!("{account} ")))
            .unwrap_or_else(|| panic!("no row for {account}\n{out}"))
    }

    // Card aae367 item 9: a seat whose config dir was never logged in reads
    // `not logged in` (or `login unknown`), never `available`.
    #[test]
    fn never_logged_in_seats_are_not_shown_available() {
        let env = Env::new();
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--dry-run",
            "--config",
            env.config_path().to_str().unwrap(),
        ])
        .unwrap();
        for account in ["claude-personal", "chatgpt-personal", "grok"] {
            let row = seat_row(&out, account);
            assert!(row.contains("not logged in"), "{row}");
            assert!(!row.contains("available"), "{row}");
        }
        assert!(out.contains("codex login"), "{out}");

        drop(env); // releases herdr_env::lock() before the next Env takes it
        let env = Env::new();
        let home = env.dir.path().join("home");
        fs::create_dir_all(home.join(".codex-personal")).unwrap();
        fs::write(home.join(".codex-personal/auth.json"), "{}").unwrap();
        fs::create_dir_all(home.join(".claude-personal")).unwrap();
        fs::write(
            home.join(".claude-personal/.claude.json"),
            r#"{"oauthAccount": {}}"#,
        )
        .unwrap();
        fs::create_dir_all(home.join(".grok")).unwrap();
        fs::write(home.join(".grok/settings.json"), "{}").unwrap();
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--dry-run",
            "--config",
            env.config_path().to_str().unwrap(),
        ])
        .unwrap();
        assert!(
            seat_row(&out, "chatgpt-personal").contains("available"),
            "{out}"
        );
        assert!(
            seat_row(&out, "claude-personal").contains("available"),
            "{out}"
        );
        assert!(seat_row(&out, "grok").contains("login unknown"), "{out}");
    }

    #[test]
    fn non_interactive_write_says_writing_not_would_write() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("sandbox");
        git_init(&project);
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--config",
            config.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
            "--name",
            "sandbox",
        ])
        .unwrap();
        let writing = format!("Writing {}:", config.display());
        let would = format!("Would write {}:", config.display());
        assert!(out.contains(&writing), "{out}");
        assert!(!out.contains(&would), "{out}");
        assert!(out.contains("Writing [projects.sandbox.chains]:"), "{out}");
        assert!(
            !out.contains("Would write [projects.sandbox.chains]:"),
            "{out}"
        );
        assert!(!out.contains("ONBOARDING.md"), "{out}");
        assert!(!out.contains("spec §"), "{out}");
        let text = fs::read_to_string(&config).unwrap();
        assert!(!text.contains("ONBOARDING.md"), "{text}");
    }

    #[test]
    fn dry_run_keeps_would_write_because_nothing_is_confirmed() {
        let env = Env::new();
        let config = env.config_path();
        let project = env.dir.path().join("sandbox");
        git_init(&project);
        let out = run_onboard(&[
            "onboard",
            "--non-interactive",
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
            "--project",
            project.to_str().unwrap(),
            "--name",
            "sandbox",
        ])
        .unwrap();
        let would = format!("Would write {}:", config.display());
        let writing = format!("Writing {}:", config.display());
        assert!(out.contains(&would), "{out}");
        assert!(!out.contains(&writing), "{out}");
        assert!(
            out.contains("Would write [projects.sandbox.chains]:"),
            "{out}"
        );
        assert!(!out.contains("Writing [projects.sandbox.chains]:"), "{out}");
        assert!(!out.contains("Writing these chains"), "{out}");
        assert!(!config.exists(), "dry-run must not create the config");
    }

    #[test]
    fn interactive_seat_preview_says_would_write_before_the_confirm() {
        let env = Env::new();
        let config = env.config_path();
        let (result, out) = run_full(
            &["onboard", "--config", config.to_str().unwrap()],
            "\n\nn\nn\nn\nn\n",
            true,
        );
        let _ = result;
        assert!(out.contains("Would write:"), "{out}");
        assert!(out.contains("Write this seat"), "{out}");
        let would_idx = out.find("Would write:").expect(&out);
        let confirm_idx = out.find("Write this seat").expect(&out);
        assert!(
            would_idx < confirm_idx,
            "Would write must precede the confirmation: {out}"
        );
    }

    fn snapshot(root: &Path) -> BTreeSet<(String, Vec<u8>)> {
        let mut files = BTreeSet::new();
        for entry in walkdir(root) {
            if entry.is_file() {
                files.insert((
                    entry.strip_prefix(root).unwrap().display().to_string(),
                    fs::read(&entry).unwrap(),
                ));
            }
        }
        files
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = vec![root.to_path_buf()];
        let mut i = 0;
        while i < out.len() {
            if out[i].is_dir() {
                if let Ok(entries) = fs::read_dir(&out[i]) {
                    for entry in entries.flatten() {
                        out.push(entry.path());
                    }
                }
            }
            i += 1;
        }
        out
    }
}
