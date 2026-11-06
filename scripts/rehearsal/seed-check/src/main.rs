//! Cutover rehearsal tool: render the Nexus seed `mv-engine::nexus_seed`
//! would produce for one or more real projects, from a config file given
//! as an argument (never a path baked in here). Takes no opinion on
//! where that config lives — point it at a scratch copy, never the
//! operator's real `~/.config/multiverse/config.toml`.
//!
//! Usage:
//!   seed-check --config PATH --directives DIR --out DIR \
//!       [--project NAME]... [--handoff NAME=PATH]...
//!
//! `--project` may repeat; omit it to render every configured project.
//! `--handoff` overrides the handoff-card path shown for one project;
//! a project with no override gets `<repository>/.multiverse/handoff.md`,
//! which is a naming convention this tool assumes, not one `mv-engine`
//! enforces — see `result.md` for why that default does not exist for
//! most real projects today.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use mv_config::load::{load_config, Config, Project};
use mv_config::roles::Role;
use mv_engine::nexus_seed::{render_seed, DispatchableRole, SeedContext, SeedRole};

struct Args {
    config: PathBuf,
    directives: PathBuf,
    out: PathBuf,
    projects: Vec<String>,
    handoff_overrides: BTreeMap<String, PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut config = None;
    let mut directives = None;
    let mut out = None;
    let mut projects = Vec::new();
    let mut handoff_overrides = BTreeMap::new();

    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--config" => config = Some(PathBuf::from(value()?)),
            "--directives" => directives = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--project" => projects.push(value()?),
            "--handoff" => {
                let raw = value()?;
                let (name, path) = raw
                    .split_once('=')
                    .ok_or_else(|| format!("--handoff expects NAME=PATH, got {raw:?}"))?;
                handoff_overrides.insert(name.to_string(), PathBuf::from(path));
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }

    Ok(Args {
        config: config.ok_or("--config is required")?,
        directives: directives.ok_or("--directives is required")?,
        out: out.ok_or("--out is required")?,
        projects,
        handoff_overrides,
    })
}

/// `nexus_seed::SeedRole` is a deliberately private vocabulary until
/// `mv-config`'s role API grows a shared one (its own doc comment);
/// this is the mapping back from `mv_config::roles::Role`, skipping
/// `Role::Nexus` itself, which does not dispatch to a chain.
fn seed_role(role: Role) -> Option<SeedRole> {
    match role {
        Role::Nexus => None,
        Role::Scout => Some(SeedRole::Scout),
        Role::Builder => Some(SeedRole::Builder),
        Role::Archivist => Some(SeedRole::Archivist),
        Role::Critic => Some(SeedRole::Critic),
        Role::Seer => Some(SeedRole::Seer),
        Role::Council => Some(SeedRole::Council),
        Role::Artisan => Some(SeedRole::Artisan),
        Role::Watcher => Some(SeedRole::Watcher),
    }
}

/// First-hop-only approximation of real seat selection: the chain's
/// first account if it is both configured and marked `available`,
/// else an `Err` naming why. Real dispatch (`mv-engine::selector`)
/// also weighs quotas and breaker state; this tool checks what the
/// seed renderer needs to know to label a role, nothing more.
fn first_hop_availability(config: &Config, chain: &[String]) -> Result<String, String> {
    let first = chain.first().expect("empty chains are rejected at load");
    match config.accounts.get(first) {
        None => Err(format!("{first}: not configured as an account")),
        Some(account) if account.available => Ok(account.name.clone()),
        Some(account) => Err(format!(
            "{first}: marked unavailable{}",
            account
                .unavailable_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        )),
    }
}

/// `SeedContext::review_policy` is caller-rendered text with no shared
/// generator yet (`crates/mv-cli/src/commands/review_policy.rs`'s
/// `format_policy` produces a longer operator report, not a seed-sized
/// phrase) — a one-line approximation good enough to confirm the field
/// reaches the renderer, not a proposal for the real string.
fn review_policy_summary(config: &Config) -> String {
    let p = &config.review_policy;
    format!(
        "scope={}, effort={}, rereview={}, run_tests={}",
        p.scope, p.effort, p.rereview, p.run_tests
    )
}

fn roles_for(config: &Config, project: &Project) -> Vec<DispatchableRole> {
    let mut roles: Vec<DispatchableRole> = Vec::new();
    for role in Role::ALL {
        let Some(seed_role) = seed_role(role) else {
            continue;
        };
        let Some(chain) = project.chains.get(&role).filter(|c| !c.is_empty()) else {
            continue;
        };
        roles.push(DispatchableRole {
            role: seed_role,
            account: first_hop_availability(config, chain),
        });
    }
    roles
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let config = load_config(&args.config).map_err(|e| e.to_string())?;
    if !config.warnings.is_empty() {
        eprintln!("config warnings ({}):", config.warnings.len());
        for warning in &config.warnings {
            eprintln!("  {warning}");
        }
    }

    let names: Vec<String> = if args.projects.is_empty() {
        config.projects.keys().cloned().collect()
    } else {
        args.projects.clone()
    };

    fs::create_dir_all(&args.out).map_err(|e| e.to_string())?;

    for name in &names {
        let project = config
            .projects
            .get(name)
            .ok_or_else(|| format!("no such project in config: {name}"))?;
        let repository = project
            .path
            .value()
            .map(Path::new)
            .ok_or_else(|| format!("{name}: path is an unfilled placeholder"))?;
        let handoff_card = args
            .handoff_overrides
            .get(name)
            .cloned()
            .unwrap_or_else(|| repository.join(".multiverse/handoff.md"));
        let roles = roles_for(&config, project);
        let review_policy = review_policy_summary(&config);

        let context = SeedContext {
            project: name,
            repository,
            handoff_card: &handoff_card,
            directives_root: &args.directives,
            roles: &roles,
            review_policy: &review_policy,
            max_concurrent: config.max_concurrent as usize,
            dispatch_min_quota_percent: config.dispatch_min_quota_percent as u8,
            lend_interactive_to_workers: config.lend_interactive_to_workers,
            role_prompt: None,
        };

        let rendered = render_seed(&context);
        let out_path = args.out.join(format!("{name}.md"));
        fs::write(&out_path, &rendered).map_err(|e| e.to_string())?;

        let handoff_exists = handoff_card.is_file();
        println!(
            "{name}: wrote {} ({} bytes); handoff_card={} (exists={handoff_exists}); roles={}",
            out_path.display(),
            rendered.len(),
            handoff_card.display(),
            roles
                .iter()
                .map(|r| format!(
                    "{}:{}",
                    r.role.display(),
                    if r.account.is_ok() { "ok" } else { "err" }
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("seed-check: {error}");
        std::process::exit(1);
    }
}
