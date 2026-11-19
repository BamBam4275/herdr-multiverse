//! `activity send|reply|close|broadcast|nudge|inbox|update`.
//!
//! `send`/`reply`/`broadcast` need the *recipient* project's filesystem
//! root, not just its configured name, so they load config (via
//! `super::load_config`) and resolve `[projects.<name>].path`.
//! `activity_pairs` (deny-by-default, confirmed by mv-config's own test
//! `activity_pairs_deny_by_default_and_check_referential_integrity`)
//! supplies `may_send`.
//!
//! `nudge` (card `20261102-b-3b6243`) types `NUDGE_PROMPT` into each
//! permitted recipient's live orchestrator pane and confirms it landed,
//! the same way `commands::start`'s `--reseed` re-points one: both go
//! through `mv_engine::fleet_start::FleetState::orchestrator_agent` to
//! find the pane by name. Before typing, it `agent read`s and runs
//! `dialog_or_menu_reason` (`cli.py:13734-13818`): a parked dialog is
//! never sent keystrokes; an already-working agent is prompted once and
//! reported unverifiable.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ArgMatches;
use mv_core::redact::{redact, redact_text};
use mv_engine::fleet_start::{find_orchestrator_agent, FleetState};
use mv_engine::mv_herdr::argv;
use mv_engine::runner::{
    deliver_prompt_to, dialog_or_menu_reason, Herdr, ProcessHerdr, RunnerError, Timing,
};
use mv_state::activity::{
    self, ActivityError, MessageRecord, ProjectLog, SendOutcome, UpdatePhase,
};

use crate::exit::Failure;

use super::doctor::{resolve_config_path, state_dir_from_config};
use super::{current_project, load_config, project_root_by_name};

const ACTIVITY_CONFIG_ERROR: &str = "config error: the multiverse config could not be loaded; run \
`multiverse check` to see what is wrong with it.";

fn fail(e: ActivityError) -> Failure {
    match e {
        ActivityError::NotPermitted { .. }
        | ActivityError::AskNotFound(_)
        | ActivityError::InvalidAskRef { .. }
        | ActivityError::RecipientDirectoryMissing { .. }
        | ActivityError::UnknownCard { .. }
        | ActivityError::TerminalCard { .. }
        | ActivityError::AskNotHeldHere { .. }
        | ActivityError::NotHub(_) => Failure::ItemLookup(e.to_string()),
        other => Failure::Operation(other.to_string()),
    }
}

/// Path-free wrap of `load_config` (`cli.py:13315-13323`): config.py puts
/// resolved project paths into some of its own error text, which a sender
/// must never see.
fn load_activity_config(args: &ArgMatches) -> Result<mv_config::load::Config, Failure> {
    match load_config(args) {
        Ok(config) => Ok(config),
        Err(Failure::Config(_)) | Err(Failure::Operation(_)) => {
            Err(Failure::ItemLookup(ACTIVITY_CONFIG_ERROR.into()))
        }
        Err(other) => Err(other),
    }
}

fn may_send(config: &mv_config::load::Config, from: &str, to: &str) -> bool {
    config
        .activity_pairs
        .get(from)
        .is_some_and(|allowed| allowed.iter().any(|p| p == to))
}

fn ask_ref_label(raw: &str) -> String {
    if raw.contains('#') {
        raw.to_string()
    } else {
        format!("#{raw}")
    }
}

fn probe_message(from: &str, to: &str, body: &str) -> MessageRecord {
    MessageRecord {
        seq: 0,
        when: String::new(),
        id: String::new(),
        from_project: from.to_string(),
        to_project: to.to_string(),
        body: body.to_string(),
        ask: false,
        reply_to: None,
        relayed_by: None,
    }
}

/// What `send` does about the sender's own open asks held against `to`
/// (card `20261110-b-657008`): the only case `references_ask`'s own
/// citation covers (`cli.py:13325-13433`, scout2-gaps #13, pinned by
/// conformance `activity-send-auto-routes-one-open-ask`) is exactly one
/// open candidate. More than one is this port's own addition, not a line
/// from Python — guessing which of several still-open asks a plain
/// message answers risks closing the wrong one, so it refuses instead.
/// Extracted from `send()` so the three-way decision is testable without
/// a project tree, the same reason `nudge` keeps [`plan_nudge`] separate.
#[derive(Debug, PartialEq, Eq)]
enum SendAskPlan {
    /// No open ask from `to`: send a plain message.
    Plain,
    /// Exactly one open ask, and `references_ask` says this message
    /// answers it: auto-route through `reply()`.
    AutoReply(MessageRecord),
    /// Exactly one open ask, but this message does not look like its
    /// answer: send plain, with a hint pointing at it.
    PlainWithHint(MessageRecord),
    /// More than one open ask from `to`: refuse rather than guess.
    Ambiguous(Vec<MessageRecord>),
}

fn plan_send_against_open_asks(
    candidates: Vec<MessageRecord>,
    from: &str,
    to: &str,
    body: &str,
) -> SendAskPlan {
    match candidates.len() {
        0 => SendAskPlan::Plain,
        1 => {
            let candidate = candidates.into_iter().next().expect("len == 1");
            let probe = probe_message(to, from, body);
            if activity::references_ask(&candidate, &probe) {
                SendAskPlan::AutoReply(candidate)
            } else {
                SendAskPlan::PlainWithHint(candidate)
            }
        }
        _ => {
            let mut ordered = candidates;
            ordered.sort_by_key(|m| m.seq);
            SendAskPlan::Ambiguous(ordered)
        }
    }
}

fn unknown_ask(seq: u64, project: &str, suggestions: &[String]) -> Failure {
    let base = format!("no ask #{seq} in {project}'s log");
    let message = match suggestions {
        [] => format!("{base}; nothing to reply to or close"),
        [one] => format!("{base}; did you mean {one}#{seq}?"),
        many => {
            let listed = many
                .iter()
                .map(|name| format!("{name}#{seq}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{base}; did you mean {listed}?")
        }
    };
    Failure::ItemLookup(message)
}

/// No default-id generator is exposed by `mv_state::activity` (`mv_state::
/// mail::message_id` belongs to the separate mail subsystem, which
/// `architecture.md` says explicitly not to repurpose: "mail.py: port the
/// on-disk types if files exist; do not invent a CLI"). This is a
/// deliberately simple stand-in for when `--id` is omitted, not a claim
/// that it matches Python's own default scheme.
fn default_message_id(from: &str, to: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{from}-{to}-{nanos:x}")
}

/// Loads config once (so every subcommand below fails the same way on a
/// broken config, via [`load_activity_config`]'s path-free error text)
/// and routes to the named subcommand.
pub fn run(matches: &ArgMatches) -> Result<(), Failure> {
    let (name, args) = matches
        .subcommand()
        .expect("activity requires a subcommand");
    let _ = load_activity_config(args)?;
    match name {
        "send" => send(args),
        "reply" => reply(args),
        "close" => close(args),
        "show" => show(args),
        "broadcast" => broadcast(args),
        "inbox" => inbox(args),
        "update" => update(args),
        "nudge" => nudge(args),
        _ => unreachable!("clap restricts activity to its documented subcommands"),
    }
}

/// What `activity nudge` prompts an orchestrator pane with (card
/// `20260911-b-13d412`, `cli.py:249-257`'s `NUDGE_PROMPT`, verbatim).
/// Points at the same two commands the orchestrator seed's own "Before you
/// dispatch anything" section already names, so a nudge asks for nothing a
/// cold-started orchestrator would not already have been told to do on its
/// own.
const NUDGE_PROMPT: &str = "Nudge from the hub: check your mail (`multiverse activity inbox`) and \
reconcile before you dispatch anything else (`multiverse tasks --reconcile`).";

/// Python's already-working report (`cli.py:13764-13767`). `state_change_seq`
/// cannot confirm a prompt sent to a busy agent, so we send once and do
/// not retry.
const WORKING_UNVERIFIABLE: &str = "agent was already working — state_change_seq cannot \
confirm a prompt sent to a busy agent; sent once, not retried";

/// What to do with one permitted recipient after we know its live
/// orchestrator (or that we could not read its pane). Extracted so the
/// parked-dialog / already-working decisions can be unit-tested without
/// a Herdr.
#[derive(Debug, PartialEq, Eq)]
enum NudgePlan {
    UnreachedReadFailed,
    UnreachedParked(String),
    UnverifiableWorking,
    Deliver,
}

fn plan_nudge(working: bool, pane_text: Option<&str>, kind: &str) -> NudgePlan {
    if working {
        return NudgePlan::UnverifiableWorking;
    }
    match pane_text {
        None => NudgePlan::UnreachedReadFailed,
        Some(text) => match dialog_or_menu_reason(text, kind) {
            Some(reason) => NudgePlan::UnreachedParked(reason),
            None => NudgePlan::Deliver,
        },
    }
}

/// Split `candidates` (already sorted, already validated as configured
/// project names) into projects `[activity.pairs]` lets `from` reach and
/// projects it does not — `activity.py`'s `nudge_targets` loop
/// (`activity.py:2416-2427`), minus the `Config`/filesystem-path lookup
/// that function also does (this command resolves paths separately, after
/// this split, exactly where `broadcast` above resolves them for its own
/// recipients).
fn partition_nudge_targets(
    from: &str,
    candidates: Vec<String>,
    activity_pairs: &std::collections::BTreeMap<String, Vec<String>>,
) -> (Vec<String>, Vec<(String, &'static str)>) {
    let mut permitted = Vec::new();
    let mut skipped = Vec::new();
    for name in candidates {
        if name == from {
            continue;
        }
        let may_send = activity_pairs
            .get(from)
            .is_some_and(|allowed| allowed.iter().any(|p| p == &name));
        if may_send {
            permitted.push(name);
        } else {
            skipped.push((name, "not permitted by [activity.pairs]"));
        }
    }
    (permitted, skipped)
}

/// Resolves `nudge`'s requested project tokens the same way `backlog
/// list`'s own tokens resolve (`backlog::resolve_named_project`): a literal
/// `[projects.<name>]` key first, else `.`, a relative path, or an
/// absolute path resolved against `cwd` (card `20261117-b-486a55` —
/// `nudge` used to require an exact config key, with no path handling at
/// all, so `activity nudge .` failed with "no such project: ." even from
/// inside the hub's own directory). Returns the canonical configured name
/// for every token that resolves, and every token that resolves to
/// nothing, in request order.
fn resolve_requested_projects<'a>(
    config: &mv_config::load::Config,
    cwd: Option<&Path>,
    requested: &'a [String],
) -> (Vec<String>, Vec<&'a String>) {
    let mut resolved = Vec::with_capacity(requested.len());
    let mut unknown = Vec::new();
    for name in requested {
        match super::backlog::resolve_named_project(config, cwd, name) {
            Some(project) => resolved.push(project.name),
            None => unknown.push(name),
        }
    }
    (resolved, unknown)
}

/// `activity nudge [PROJECTS...]`: hub-only, like `broadcast`/`fan_out`
/// (card `20260911-b-13d412`). Resolves which configured projects the
/// `[activity.pairs]` table permits reaching (Python's
/// `activity.nudge_targets`, re-derived here rather than factored into
/// `mv_state::activity` — the recipient list this builds, with real
/// filesystem paths attached, is exactly what `broadcast` above already
/// builds inline in this same file), then finds each permitted
/// recipient's live orchestrator pane and delivers [`NUDGE_PROMPT`] with
/// landing confirmation.
fn nudge(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let state_dir = state_dir_from_config(&config_path);
    let halt = mv_state::halt::read_halt_state(&state_dir);
    if halt.halted {
        return Err(Failure::Operation(format!(
            "activity nudge refused: {}",
            mv_state::halt::format_halt_line(&halt)
        )));
    }

    let (from, _root) = current_project(args)?;
    if from != activity::HUB_PROJECT_NAME {
        return Err(Failure::Config(format!(
            "only {} may nudge; caller project was {from:?}",
            activity::HUB_PROJECT_NAME
        )));
    }
    let config = load_activity_config(args)?;

    let requested: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let cwd = std::env::current_dir().ok();
    let (resolved, unknown) = resolve_requested_projects(&config, cwd.as_deref(), &requested);
    if !unknown.is_empty() {
        let mut known: Vec<&str> = config.projects.keys().map(String::as_str).collect();
        known.sort_unstable();
        let known = if known.is_empty() {
            "(none)".to_string()
        } else {
            known.join(", ")
        };
        let unknown = unknown
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Failure::Config(format!(
            "no such project: {unknown}; configured: {known} (see `multiverse projects`)"
        )));
    }

    let mut candidates: Vec<String> = if requested.is_empty() {
        config.projects.keys().cloned().collect()
    } else {
        resolved
    };
    candidates.sort_unstable();
    let (permitted, skipped) = partition_nudge_targets(&from, candidates, &config.activity_pairs);

    let mut herdr = ProcessHerdr;
    let fleet = FleetState::fetch(&mut herdr, true).map_err(|message| {
        Failure::Operation(format!(
            "could not reach Herdr to find Nexus panes: {message}"
        ))
    })?;
    let timing = Timing::default();
    let interactive = config.accounts.values().find(|account| account.interactive);
    let seat = interactive
        .map(|account| account.short.as_str())
        .unwrap_or("");
    let kind = interactive
        .map(|account| account.kind.as_str())
        .unwrap_or("");

    let mut reached = Vec::new();
    let mut unconfirmed: Vec<(String, String)> = Vec::new();
    let mut unverifiable: Vec<(String, String)> = Vec::new();
    let mut unreached: Vec<(String, String)> = Vec::new();
    for name in permitted {
        let Some(path) = config.projects.get(&name).and_then(|p| p.path.value()) else {
            unreached.push((name, "project has an unfilled path placeholder".into()));
            continue;
        };
        let root = PathBuf::from(path);
        let project = config.projects.get(&name);
        let short = project.and_then(|p| p.short.as_deref()).unwrap_or("");
        match find_orchestrator_agent(&fleet, &name, &root, seat, short) {
            None => {
                let expected = mv_engine::fleet_start::nexus_agent_name(&name, seat, short)
                    .unwrap_or_else(|_| format!("{name}-nexu"));
                unreached.push((name, format!("no live {expected} pane found")));
            }
            Some((agent_name, live)) => {
                nudge_one(
                    &mut herdr,
                    &name,
                    &agent_name,
                    &live,
                    kind,
                    &timing,
                    &mut reached,
                    &mut unconfirmed,
                    &mut unverifiable,
                    &mut unreached,
                );
            }
        }
    }

    for name in &reached {
        println!("{}", nudge_line("nudged", name, None));
    }
    for (name, reason) in &unconfirmed {
        println!("{}", nudge_line("unconfirmed", name, Some(reason)));
    }
    for (name, reason) in &unverifiable {
        println!("{}", nudge_line("unverifiable", name, Some(reason)));
    }
    for (name, reason) in &unreached {
        println!("{}", nudge_line("unreached", name, Some(reason)));
    }
    for (name, reason) in &skipped {
        println!("{}", nudge_line("skipped", name, Some(reason)));
    }
    println!(
        "{} nudged, {} unconfirmed, {} unverifiable, {} unreached, {} skipped.",
        reached.len(),
        unconfirmed.len(),
        unverifiable.len(),
        unreached.len(),
        skipped.len()
    );
    Ok(())
}

/// One permitted recipient: working → send once / unverifiable; otherwise
/// `agent read` + `dialog_or_menu_reason` before any prompt. A parked
/// dialog is never typed into (`cli.py:13754-13818`).
#[allow(clippy::too_many_arguments)]
fn nudge_one(
    herdr: &mut ProcessHerdr,
    name: &str,
    agent_name: &str,
    live: &mv_engine::fleet_start::LiveAgent,
    kind: &str,
    timing: &Timing,
    reached: &mut Vec<String>,
    unconfirmed: &mut Vec<(String, String)>,
    unverifiable: &mut Vec<(String, String)>,
    unreached: &mut Vec<(String, String)>,
) {
    if live.is_working() {
        match herdr.call(
            &argv::agent_prompt(agent_name, NUDGE_PROMPT, false, None),
            Duration::from_secs(60),
        ) {
            Ok(result) if result.ok() => {
                unverifiable.push((name.to_string(), WORKING_UNVERIFIABLE.into()));
            }
            Ok(result) => unreached.push((name.to_string(), result.message())),
            Err(error) => unreached.push((name.to_string(), error.to_string())),
        }
        return;
    }

    let pane_text = match herdr.call(
        &argv::agent_read(agent_name, Some("visible"), None),
        Duration::from_secs(30),
    ) {
        Ok(result) if result.ok() => Some(result.stdout),
        _ => None,
    };
    match plan_nudge(false, pane_text.as_deref(), kind) {
        NudgePlan::UnreachedReadFailed => unreached.push((
            name.to_string(),
            "its pane could not be read; nothing was sent".into(),
        )),
        NudgePlan::UnreachedParked(reason) => unreached.push((
            name.to_string(),
            format!("parked on a {reason}; nothing was sent"),
        )),
        NudgePlan::UnverifiableWorking => unreachable!("working is handled above"),
        NudgePlan::Deliver => match deliver_prompt_to(
            herdr,
            agent_name,
            &live.pane_id,
            NUDGE_PROMPT,
            kind,
            live.state_change_seq,
            timing,
        ) {
            Ok(()) => reached.push(name.to_string()),
            Err(RunnerError::NeedsAttention { reason, .. }) => {
                unreached.push((name.to_string(), reason));
            }
            Err(error) => {
                let first = error
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("prompt did not land")
                    .to_string();
                unconfirmed.push((name.to_string(), first));
            }
        },
    }
}

/// One `nudge` summary line. `name`/`reason` come from configured project
/// names and Herdr's own error/dialog text, so both are sanitized before
/// they reach the operator's terminal.
fn nudge_line(label: &str, name: &str, reason: Option<&str>) -> String {
    let name = mv_board::layout::sanitize_title(name);
    match reason {
        Some(reason) => format!(
            "{label:<10}{name} — {}",
            mv_board::layout::sanitize_title(reason)
        ),
        None => format!("{label:<10}{name}"),
    }
}

/// After a successful `send`/`reply`/`close`, name the closer when the
/// caller's own inbox is still sitting on unacked mail (`cli.py`'s
/// `_print_unacked_mail_hint`, `cli.py:13270-13288`). None of the three
/// ever touch this project's read cursor, so a caller who triaged by
/// answering an ask can walk away believing it is handled while the board's
/// MAIL card still flags it (card `20261021-b-7b7d6f`).
///
/// Peeks with `read_inbox` rather than checking the cursor directly: that
/// peek is also what writes this project's own `activity.lastseen` marker
/// on every call, unread or not (`activity.py:2573-2581`) — the same
/// byproduct Python's `has_unread_mail` relies on, which is why a plain
/// `send` with no prior `activity inbox` still leaves a fresh marker behind
/// (card `20261103-b-00824c`). A read failure is swallowed, matching
/// `has_unread_mail`'s own best-effort posture: this is a hint after the
/// caller's real command already succeeded, not a reason to fail it.
fn print_unacked_mail_hint(name: &str, root: &std::path::Path) {
    let log = ProjectLog::open(name.to_owned(), root);
    let state_dir = root.join(".multiverse");
    if let Ok(page) = activity::read_inbox(&state_dir, &log, usize::MAX, true) {
        if !page.messages.is_empty() {
            println!("  inbox still has unacked mail; run `multiverse activity inbox`.");
        }
    }
}

/// What to print, if anything, after a best-effort notify-hook attempt for
/// a freshly sent ask (card `20261115-b-51cb75`). A missing or failing hook
/// is advisory only — the ask already landed by the time this is called —
/// so this never turns into a `Failure`; it only decides the wording.
fn ask_notify_warning(outcome: super::notify::PushOutcome) -> Option<&'static str> {
    use super::notify::PushOutcome;
    match outcome {
        PushOutcome::HookMissing => Some(
            "could not notify about this ask: the notify hook (curl) was not found on PATH; \
             the ask was still sent.",
        ),
        PushOutcome::Failed => Some("could not notify about this ask; the ask was still sent."),
        PushOutcome::NoTopic | PushOutcome::Sent => None,
    }
}

/// Best-effort alert for a freshly sent `--ask` (card `20261115-b-51cb75`):
/// an open ask blocks someone, the same reason `NUDGE_PROMPT` above exists,
/// so it is worth trying to notify through the same channel the project's
/// own prompts already point operators at (`multiverse notify`, never curl
/// directly). Called only after `send_message` has already appended the
/// message, so nothing here can cost the post itself — a missing hook
/// binary, an unconfigured topic, or a failed push all just print a warning
/// and return.
fn notify_ask_posted(args: &ArgMatches, to: &str, from: &str, body: &str) {
    let Ok((config_path, _source)) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))
    else {
        return;
    };
    let topic = super::notify::resolve_topic(&config_path);
    let token = super::notify::resolve_token(&config_path);
    let record = mv_state::notify::QueuedNotification {
        project: to.to_string(),
        title: format!("ask from {from}"),
        message: body.to_string(),
        priority: "default".to_string(),
        dedup_key: None,
        queued_at: mv_core::time::now_stamp(),
        kind: None,
        subject: None,
        tags: Vec::new(),
        episode_key: None,
        episode_state: None,
    };
    let outcome = super::notify::push_with_token_detailed(
        topic.as_deref(),
        token.as_deref(),
        &record,
        "default",
        &[],
    );
    if let Some(warning) = ask_notify_warning(outcome) {
        println!("  {warning}");
    }
}

fn send(args: &ArgMatches) -> Result<(), Failure> {
    let to = args.get_one::<String>("project").expect("required");
    let body = args.get_one::<String>("message").expect("required");
    let (from, from_root) = current_project(args)?;
    let config = load_activity_config(args)?;
    let to_root = project_root_by_name(&config, to).map_err(|e| match e {
        Failure::Config(message) => Failure::ItemLookup(message),
        other => other,
    })?;
    let permitted = may_send(&config, &from, to);
    let id = args
        .get_one::<String>("id")
        .cloned()
        .unwrap_or_else(|| default_message_id(&from, to));
    let ask = args.get_flag("ask");
    let not_a_reply = args.get_flag("not-a-reply");
    let re_raw = args.get_one::<String>("re");
    if re_raw.is_some() && (ask || not_a_reply) {
        return Err(Failure::Operation(
            "--re cannot be combined with --ask or --not-a-reply.".into(),
        ));
    }

    let own_log = ProjectLog::open(from.clone(), &from_root);
    let mut linked_ask: Option<MessageRecord> = None;
    if !ask && !not_a_reply {
        let candidates: Vec<MessageRecord> = activity::open_asks(&own_log)
            .map_err(fail)?
            .into_iter()
            .filter(|m| m.from_project == *to)
            .collect();
        if let Some(raw) = re_raw {
            let (qualifier, requested_seq) = activity::parse_ask_ref(raw).map_err(fail)?;
            if let Some(name) = qualifier {
                if name != *to {
                    return Err(Failure::Operation(format!(
                        "--re {raw} does not name an ask from {to}."
                    )));
                }
            }
            linked_ask = candidates.into_iter().find(|m| m.seq == requested_seq);
            if linked_ask.is_none() {
                return Err(Failure::Operation(format!(
                    "no open ask #{requested_seq} from {to} is available to answer."
                )));
            }
        } else {
            match plan_send_against_open_asks(candidates, &from, to, body) {
                SendAskPlan::Plain => {}
                SendAskPlan::AutoReply(candidate) => {
                    linked_ask = Some(candidate);
                }
                SendAskPlan::PlainWithHint(candidate) => {
                    eprintln!(
                        "{to} has an open ask (#{seq}) but this message doesn't look like a \
                         reply to it; sending as a plain message. Use \
                         `multiverse activity reply {seq}` or `--re {seq}` to answer it \
                         directly.",
                        seq = candidate.seq
                    );
                }
                SendAskPlan::Ambiguous(ordered) => {
                    let listed = ordered
                        .iter()
                        .map(|m| format!("#{}: {}", m.seq, m.body))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Err(Failure::ItemLookup(format!(
                        "{to} has multiple open asks from you ({listed}); nothing was sent — \
                         answer one with `multiverse activity reply <seq>` (or resend with \
                         `--re <seq>`)."
                    )));
                }
            }
        }
    }

    // Pairing first, then the directory: a sender without permission
    // must not learn whether the recipient path exists
    // (`activity.py:1780-1804`).
    if !permitted {
        return Err(fail(ActivityError::NotPermitted {
            from: from.clone(),
            to: to.clone(),
        }));
    }
    if !to_root.exists() {
        return Err(fail(ActivityError::RecipientDirectoryMissing {
            name: to.clone(),
        }));
    }

    if let Some(ask_message) = linked_ask {
        let asker_log = ProjectLog::open(to.clone(), &to_root);
        activity::reply(
            &own_log,
            &asker_log,
            ask_message.seq,
            &id,
            body,
            None,
            permitted,
            &redact,
        )
        .map_err(fail)?;
        println!("sent to {to}.");
        println!("  linked as reply to #{}.", ask_message.seq);
    } else {
        let recipient_log = ProjectLog::open(to.clone(), &to_root);
        let outcome = activity::send_message(
            &recipient_log,
            &id,
            &from,
            to,
            body,
            ask,
            None,
            permitted,
            &redact,
        )
        .map_err(fail)?;
        match outcome {
            SendOutcome::Sent { .. } => {
                println!("sent to {to}.");
                if ask {
                    notify_ask_posted(args, to, &from, body);
                }
            }
            SendOutcome::AlreadySent => println!("message {id} was already sent to {to}"),
        }
        if !ask && body.contains('?') {
            println!(
                "  that reads like a question; resend with --ask if you need an \
                 answer, so it shows on the board until someone replies."
            );
        }
    }
    print_unacked_mail_hint(&from, &from_root);
    Ok(())
}

/// `activity reply <SEQ>` / `reply <Project>#<SEQ>` (cards `20261115-b-91722d`,
/// `20261116-builder-9a50a2`): a bare `SEQ` resolves against the caller's own
/// log — the ordinary case, where the caller is the ask's own holder (its
/// recipient). Since the caller's own log is always checked first regardless
/// of whether a qualifier was given, a holder may *also* qualify with the
/// ask's own sender's name (`reply <Sender>#<SEQ>`) — natural when a
/// recipient juggling several open asks wants to name who they're answering,
/// and harmless because `seq` alone already disambiguates within one log; the
/// qualifier is just validated against `own_name`/`ask.from_project` rather
/// than used to pick which log to open. The *other* qualified form names the
/// project *holding* the ask, the same syntax [`close_ref`] already accepts,
/// for the case a bare `SEQ` cannot reach: the caller is the ask's original
/// *sender* (own log never holds an ask it sent, only one it received), who
/// needs to post an answer on the holder's behalf without first working
/// inside that project's own pane. [`resolve_open_ask`]'s authorization for
/// that second form mirrors [`resolve_ask`]'s own qualifier rule exactly.
fn reply(args: &ArgMatches) -> Result<(), Failure> {
    let raw = args.get_one::<String>("seq").expect("required");
    let body = args.get_one::<String>("message").expect("required");
    let (own_name, own_root) = current_project(args)?;
    let config = load_activity_config(args)?;
    let (qualifier, seq) = activity::parse_ask_ref(raw).map_err(fail)?;
    let (holder_name, holder_root, ask) =
        resolve_open_ask(&own_name, &own_root, &config, qualifier.as_deref(), seq)?;
    let holder_log = ProjectLog::open(holder_name.clone(), &holder_root);
    let asker_root = project_root_by_name(&config, &ask.from_project).map_err(|e| match e {
        Failure::Config(message) => Failure::ItemLookup(message),
        other => other,
    })?;
    let asker_log = ProjectLog::open(ask.from_project.clone(), &asker_root);
    let may_send_reverse = may_send(&config, &holder_name, &ask.from_project);
    let reply_id = args
        .get_one::<String>("id")
        .cloned()
        .unwrap_or_else(|| default_message_id(&holder_name, &ask.from_project));
    // The caller relays this reply on `holder_name`'s behalf exactly when
    // they are not `holder_name` itself — `resolve_open_ask` has already
    // confirmed that caller can only then be `ask.from_project`, the
    // asker, so `own_name` is the relaying asker's own name to record.
    let relayed_by = (own_name != holder_name).then_some(own_name.as_str());
    let outcome = activity::reply(
        &holder_log,
        &asker_log,
        seq,
        &reply_id,
        body,
        relayed_by,
        may_send_reverse,
        &redact,
    )
    .map_err(fail)?;
    match outcome {
        SendOutcome::Sent { seq } => {
            if relayed_by.is_some() {
                println!(
                    "replied to {} as seq {seq}, relayed on behalf of {holder_name}",
                    ask.from_project
                );
            } else {
                println!("replied to {} as seq {seq}", ask.from_project);
            }
        }
        SendOutcome::AlreadySent => println!("reply {reply_id} was already sent"),
    }
    print_unacked_mail_hint(&own_name, &own_root);
    Ok(())
}

/// Resolve `reply`'s target ask. The caller's own log is tried first,
/// qualifier or not (card `20261116-builder-9a50a2`): an ask the caller
/// itself holds is fully identified by `seq` alone, so a qualifier given
/// alongside one is validated — it must name either the caller or the ask's
/// own sender — rather than used to choose which log to open. This is what
/// lets the ask's actual recipient use `reply <Sender>#<SEQ>` (naming who
/// they're answering) and not just the bare form; before this, that
/// qualified call always opened the *named* project's own log, which can
/// never hold an ask addressed to someone else, so naming your own asker
/// always missed with a bare "no ask #N found" ([`ActivityError::
/// AskNotFound`]'s own text, surfaced unformatted since the qualified
/// branch historically had no reason to expect this particular miss).
///
/// Only once the own-log check misses does qualifier handling fall back to
/// [`resolve_ask`]'s own model: unqualified, an own-log miss can only ever
/// mean "not open" — a closed or unknown seq both report through the same
/// [`ActivityError::AskNotFound`] — so the scan below exists purely to name
/// which other project to requalify against, the same suggestion
/// [`unknown_ask`] already renders for `close`. Qualified, the *named*
/// project's log must hold it, and the caller must either *be* that project
/// or be the ask's own `from_project` (the sender, answering on the
/// holder's behalf — the case a bare `SEQ` cannot reach at all, since a
/// project's own log never holds an ask it sent).
fn resolve_open_ask(
    own_name: &str,
    own_root: &std::path::Path,
    config: &mv_config::load::Config,
    qualifier: Option<&str>,
    seq: u64,
) -> Result<(String, PathBuf, activity::OpenAsk), Failure> {
    let own_log = ProjectLog::open(own_name.to_owned(), own_root);
    match activity::find_open_ask(&own_log, seq) {
        Ok(ask) => {
            if let Some(name) = qualifier {
                if name != own_name && name != ask.from_project {
                    return Err(Failure::Operation(format!(
                        "{name:?} names neither {own_name:?} nor #{seq}'s own sender \
                         ({from:?}); it is held here regardless of the qualifier \
                         given — retry with `reply {seq}` or `reply {from}#{seq}`.",
                        from = ask.from_project
                    )));
                }
            }
            return Ok((own_name.to_string(), own_root.to_path_buf(), ask));
        }
        Err(ActivityError::AskNotFound(_)) => {}
        Err(e) => return Err(fail(e)),
    }

    match qualifier {
        None => {
            let mut suggestions = Vec::new();
            for (name, project) in &config.projects {
                if name == own_name {
                    continue;
                }
                let Some(path) = project.path.value() else {
                    continue;
                };
                let log = ProjectLog::open(name.clone(), std::path::Path::new(path));
                if let Ok(ask) = activity::find_open_ask(&log, seq) {
                    if ask.from_project == own_name {
                        suggestions.push(name.clone());
                    }
                }
            }
            suggestions.sort();
            Err(unknown_ask(seq, own_name, &suggestions))
        }
        Some(holder_name) => {
            let holder_root = project_root_by_name(config, holder_name).map_err(|e| match e {
                Failure::Config(message) => Failure::ItemLookup(message),
                other => other,
            })?;
            let holder_log = ProjectLog::open(holder_name.to_string(), &holder_root);
            let ask = activity::find_open_ask(&holder_log, seq).map_err(fail)?;
            if holder_name != own_name && ask.from_project != own_name {
                return Err(Failure::Operation(format!(
                    "{own_name:?} cannot act on {holder_name}#{seq}: \
                     that ask is {} -> {holder_name}",
                    ask.from_project
                )));
            }
            // Card `20261116-b-40aecf`: relaying a reply into another
            // project's log is itself an activity-messaging send and must
            // clear its own `[activity.pairs]` pair, the same re-check
            // `close_ref` applies before it writes into a holder's log —
            // being this ask's own sender is authorization to *act* on it,
            // not a bypass of deny-by-default messaging.
            if holder_name != own_name && !may_send(config, own_name, holder_name) {
                return Err(Failure::Operation(format!(
                    "{own_name:?} may not relay a reply into {holder_name:?}'s log: \
                     activity messaging is deny-by-default and no [activity.pairs] \
                     entry permits {own_name} -> {holder_name}."
                )));
            }
            Ok((holder_name.to_string(), holder_root, ask))
        }
    }
}

fn close(args: &ArgMatches) -> Result<(), Failure> {
    let raw = args.get_one::<String>("seq").expect("required");
    let reason = args
        .get_one::<String>("reason")
        .cloned()
        .unwrap_or_else(|| "closed".to_string());
    let (own_name, own_root) = current_project(args)?;
    let config = load_activity_config(args)?;
    close_ref(&own_name, &own_root, &config, raw, &reason)
}

// Closes the resolved ask in whichever log actually holds it (which may
// be a remote project's, via the `Project#SEQ` qualifier `resolve_ask`
// accepts), then best-effort notifies the other side of the ask: the
// holder when `own_name` retracted a remote ask, otherwise the original
// asker. A failed notification is reported but never turns `close`
// itself into an error — the close already succeeded on disk.
fn close_ref(
    own_name: &str,
    own_root: &std::path::Path,
    config: &mv_config::load::Config,
    raw: &str,
    reason: &str,
) -> Result<(), Failure> {
    let (qualifier, seq) = activity::parse_ask_ref(raw).map_err(fail)?;
    let (holder_name, holder_root, ask) =
        resolve_ask(own_name, own_root, config, qualifier.as_deref(), seq)?;
    let remote = holder_name != own_name;
    if remote && !may_send(config, own_name, &holder_name) {
        return Err(Failure::ItemLookup(format!(
            "{own_name:?} may not close an ask in {holder_name:?}'s \
             log: activity messaging is deny-by-default and no \
             [activity.pairs] entry permits {own_name} -> {holder_name}."
        )));
    }
    let holder_log = ProjectLog::open(holder_name.clone(), &holder_root);
    let label = ask_ref_label(raw);
    if activity::has_close_record(&holder_log, ask.seq).map_err(fail)? {
        println!("already closed {label}.");
        return Ok(());
    }
    activity::close_ask(&holder_log, ask.seq, reason, &redact).map_err(fail)?;
    let (notice_to, notice_root, notice_body) = if remote {
        (
            holder_name.clone(),
            holder_root.clone(),
            format!(
                "{own_name} retracted its ask {holder_name}#{}: {reason}",
                ask.seq
            ),
        )
    } else {
        println!("closed {label}.");
        let asker_root = project_root_by_name(config, &ask.from_project).map_err(|e| match e {
            Failure::Config(message) => Failure::ItemLookup(message),
            other => other,
        })?;
        (
            ask.from_project.clone(),
            asker_root,
            format!("closed {label} without a reply: {reason}"),
        )
    };
    let notice_log = ProjectLog::open(notice_to.clone(), &notice_root);
    let notice_id = default_message_id(own_name, &notice_to);
    match activity::send_message(
        &notice_log,
        &notice_id,
        own_name,
        &notice_to,
        &notice_body,
        false,
        Some(ask.seq),
        may_send(config, own_name, &notice_to),
        &redact,
    ) {
        Ok(_) => {
            if remote {
                println!("retracted {holder_name}#{}; told {holder_name}.", ask.seq);
            } else {
                println!("told the asker why.");
            }
        }
        Err(exc) => {
            if remote {
                println!("retracted {holder_name}#{}.", ask.seq);
                println!("  could not tell {holder_name}: {exc}");
            } else {
                println!("  could not tell the asker: {exc}");
            }
        }
    }
    print_unacked_mail_hint(own_name, own_root);
    Ok(())
}

// Unqualified `SEQ` only ever means "in my own log" — if it's not there,
// scan every other configured project's log for an ask with this seq
// that was sent *to* `own_name`, so a wrong-project mistake gets a
// `did you mean Project#SEQ?` hint instead of a bare "not found".
// Qualified `Project#SEQ` resolves against that project's log directly,
// but is only honored when `own_name` is either the named holder or the
// ask's original sender (never an unrelated third project's business).
fn resolve_ask(
    own_name: &str,
    own_root: &std::path::Path,
    config: &mv_config::load::Config,
    qualifier: Option<&str>,
    seq: u64,
) -> Result<(String, PathBuf, MessageRecord), Failure> {
    match qualifier {
        None => {
            let own_log = ProjectLog::open(own_name.to_owned(), own_root);
            if let Some(ask) = activity::find_ask(&own_log, seq).map_err(fail)? {
                return Ok((own_name.to_string(), own_root.to_path_buf(), ask));
            }
            let mut suggestions = Vec::new();
            for (name, project) in &config.projects {
                if name == own_name {
                    continue;
                }
                let Some(path) = project.path.value() else {
                    continue;
                };
                let log = ProjectLog::open(name.clone(), std::path::Path::new(path));
                if let Ok(Some(ask)) = activity::find_ask(&log, seq) {
                    if ask.from_project == own_name {
                        suggestions.push(name.clone());
                    }
                }
            }
            suggestions.sort();
            Err(unknown_ask(seq, own_name, &suggestions))
        }
        Some(in_project) => {
            let holder_root = project_root_by_name(config, in_project).map_err(|e| match e {
                Failure::Config(message) => Failure::ItemLookup(message),
                other => other,
            })?;
            let holder_log = ProjectLog::open(in_project.to_string(), &holder_root);
            let ask = activity::find_ask(&holder_log, seq)
                .map_err(fail)?
                .ok_or_else(|| unknown_ask(seq, in_project, &[]))?;
            if in_project != own_name && ask.from_project != own_name {
                return Err(Failure::Operation(format!(
                    "{own_name:?} cannot act on {in_project}#{}: \
                     that ask is {} -> {}",
                    ask.seq, ask.from_project, ask.to_project
                )));
            }
            Ok((in_project.to_string(), holder_root, ask))
        }
    }
}

/// `activity show <SEQ>` / `show <Project>#<SEQ>` (card `20261115-b-9ebad2`):
/// print one message in full, unwrapped, read-only — no cursor/marker
/// write, no close, the read-only sibling of `reply`/`close`'s own
/// `Project#SEQ` qualifier. A bare `SEQ` reads the caller's own log, like
/// `reply`'s unqualified form; the qualifier names any configured project
/// directly, with no `[activity.pairs]` gate — this only ever reads a log
/// already on disk, the same unrestricted scan [`resolve_ask`]'s own
/// suggestion fallback already performs across every configured project.
fn show(args: &ArgMatches) -> Result<(), Failure> {
    let raw = args.get_one::<String>("seq").expect("required");
    let (own_name, own_root) = current_project(args)?;
    let config = load_activity_config(args)?;
    let (qualifier, seq) = activity::parse_ask_ref(raw).map_err(fail)?;
    let (project_name, project_root) = match qualifier {
        None => (own_name, own_root),
        Some(name) => {
            let root = project_root_by_name(&config, &name).map_err(|e| match e {
                Failure::Config(message) => Failure::ItemLookup(message),
                other => other,
            })?;
            (name, root)
        }
    };
    let log = ProjectLog::open(project_name.clone(), &project_root);
    let message = activity::find_message(&log, seq)
        .map_err(fail)?
        .ok_or_else(|| unknown_message(seq, &project_name))?;
    println!("{}", format_full_message(&message));
    Ok(())
}

fn unknown_message(seq: u64, project: &str) -> Failure {
    Failure::ItemLookup(format!("no message #{seq} found in {project}'s log"))
}

/// One message, unwrapped: every field on its own line, body last and
/// never truncated — unlike [`format_inbox_line`]'s single compact line,
/// `show` exists specifically so a long ask is readable in full rather
/// than grepped out of the raw log by hand.
fn format_full_message(message: &mv_state::activity::MessageRecord) -> String {
    let mut lines = vec![format!(
        "#{} {} -> {} at {}",
        message.seq,
        mv_board::layout::sanitize_title(&message.from_project),
        mv_board::layout::sanitize_title(&message.to_project),
        mv_board::layout::sanitize_title(&message.when),
    )];
    if message.ask {
        lines.push("ask: yes".to_string());
    }
    if let Some(re) = message.reply_to {
        lines.push(format!("reply to: #{re}"));
    }
    if let Some(by) = &message.relayed_by {
        lines.push(format!(
            "relayed by: {}",
            mv_board::layout::sanitize_title(by)
        ));
    }
    lines.push(mv_board::layout::sanitize_title(&redact_text(
        &message.body,
    )));
    lines.join("\n")
}

fn broadcast(args: &ArgMatches) -> Result<(), Failure> {
    let body = args.get_one::<String>("message").expect("required");
    let (from, _root) = current_project(args)?;
    let config = load_activity_config(args)?;
    let msg_id = args
        .get_one::<String>("id")
        .cloned()
        .unwrap_or_else(|| default_message_id(&from, "broadcast"));
    let ask = args.get_flag("ask");
    let mut recipients = Vec::new();
    for (project_name, project) in &config.projects {
        if project_name == &from {
            continue;
        }
        let Some(path) = project.path.value() else {
            continue;
        };
        recipients.push((
            ProjectLog::open(project_name.clone(), std::path::Path::new(path)),
            may_send(&config, &from, project_name),
        ));
    }
    let report = activity::broadcast(activity::BroadcastRequest {
        from_project: &from,
        body,
        ask,
        recipients: &recipients,
        msg_id: &msg_id,
        redact: &redact,
    })
    .map_err(fail)?;
    let sent_count = report.sent().count();
    let skipped_count = report.skipped().count();
    for name in report.sent() {
        println!("{:<10}{name}", "sent");
    }
    for (name, reason) in report.skipped() {
        println!("{:<10}{name} — {reason}", "skipped");
    }
    println!("{sent_count} sent, {skipped_count} skipped.");
    Ok(())
}

fn inbox(args: &ArgMatches) -> Result<(), Failure> {
    let (name, root) = current_project(args)?;
    if is_hub_inbox(&name) {
        // Said once, up front, before either branch below: the hub's own
        // cwd resolves to its own project like any other, but this command
        // still only ever reads that one project's log, not the fleet —
        // unlike `board`, which widens hub scope to every project.
        // `cli.py:13877-13892` prints this ahead of its own replay check
        // too, so a replay gets it exactly like an ordinary read does.
        println!(
            "hub's own inbox only, not the fleet — see `multiverse board` for the fleet view."
        );
    }
    let peek = args.get_flag("peek");
    let as_nexus = args.get_flag("as-nexus");
    let limit = args.get_one::<u32>("limit").copied().unwrap_or(20) as usize;
    let replay_since = args.get_one::<u64>("replay-since").copied();
    let replay_last = args.get_one::<u32>("replay-last").copied();
    // `--peek` is declared `conflicts_with` every replay flag in `args.rs`,
    // so reaching here with a replay flag set means it is absent — matching
    // Python's own refusal (`cli.py:13896-13918`) that a replay never
    // acknowledges, so `--peek` has nothing to authorize or prevent, just
    // enforced a step earlier, at the parser. `--as-nexus` is read below
    // without that same conflict (card `20261115-b-2a0f8f`): it still has
    // nothing to do here either, since neither replay branch ever reads it,
    // but unlike `--peek` it is not a parse-time error to pass alongside
    // a replay flag — just as inert.
    if let Some(since) = replay_since {
        return replay_at(&name, &root, since);
    }
    if let Some(last) = replay_last {
        return replay_last_at(&name, &root, last as usize);
    }
    inbox_at(
        &name,
        &root,
        limit,
        peek,
        as_nexus,
        carries_dispatched_task_id().as_deref(),
    )
}

/// Whether `inbox`'s hub-only preamble (Python's `cmd_activity`,
/// `cli.py:13877-13892`) applies — true only for the hub's own inbox,
/// never a fleet member's, since this command only ever reads one
/// project's log either way.
fn is_hub_inbox(name: &str) -> bool {
    name == activity::HUB_PROJECT_NAME
}

/// `activity.dispatched_task_id()`'s one Rust equivalent already in use
/// (`commands::scratch`/`commands::backlog`): the env var a dispatched
/// task's own launch sets. `current_project` now uses Python's
/// `config.project_for_path` longest-prefix match, so a writer inside its
/// lane worktree still resolves to the project; carrying a dispatched task
/// id is the remaining condition that can make this caller a specialist
/// rather than the project's own interactive Nexus.
/// What this does not reproduce: Python's live-pane fallback (`cli.py:
/// 13958-13978`), which trusts a non-root caller anyway if Herdr's own
/// `agent list` finds it seated as the project's Nexus — this command reads
/// no live Herdr state. See this task's `result.md`.
fn carries_dispatched_task_id() -> Option<String> {
    std::env::var("MULTIVERSE_TASK_ID")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn inbox_at(
    name: &str,
    root: &std::path::Path,
    limit: usize,
    peek: bool,
    as_nexus: bool,
    task_id: Option<&str>,
) -> Result<(), Failure> {
    let log = ProjectLog::open(name.to_owned(), root);
    let state_dir = root.join(".multiverse");
    let acking_requested = !peek;
    let task_id = task_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let is_orchestrator = task_id.is_none();
    // Only the project's own Nexus (no dispatched task id) or an explicit
    // `--as-nexus` override may advance the read marker; a dispatched
    // specialist calling without it always gets a non-destructive peek,
    // regardless of `--peek` (see `carries_dispatched_task_id`'s doc).
    let will_acknowledge = acking_requested && (is_orchestrator || as_nexus);
    let page = activity::read_inbox(&state_dir, &log, limit, !will_acknowledge).map_err(fail)?;
    if page.messages.is_empty() {
        println!("nothing new for {name}.");
    }
    for message in &page.messages {
        println!("{}", format_inbox_line(message));
    }
    println!(
        "current through event {} as of {}",
        page.current_seq, page.current_at
    );
    let verb = if will_acknowledge { "read" } else { "peeked" };
    println!(
        "last {verb} at {}, through event {}",
        page.marker_at, page.marker_seq
    );
    if !will_acknowledge {
        println!("sequence numbers are per project and cannot be compared across projects.");
    }
    if page.more_unread > 0 {
        println!("{} more unread — run inbox again", page.more_unread);
    }
    if acking_requested && !will_acknowledge {
        let id = task_id.expect("not is_orchestrator implies a task id was found");
        eprintln!("{}", task_id_refusal_message(name, &id));
    }
    if will_acknowledge {
        if let Some(seq) = page.through_seq {
            activity::acknowledge(&state_dir, &log, seq, page.messages.len() as u64)
                .map_err(fail)?;
        }
    }
    print_open_ask_aging_footer(name, &log)?;
    Ok(())
}

fn task_id_refusal_message(name: &str, id: &str) -> String {
    format!(
        "not acknowledged: this process carries a dispatched task id ({id}), not \
         {name}'s Nexus — a specialist must never advance the cursor the \
         Nexus's own mail read depends on. Pass --as-nexus if that is wrong, \
         or --peek to look without it."
    )
}

/// `--replay-since SEQ`: Python's own selection is inclusive of `since_seq`
/// (`activity.py:3222-3225`, "seq >= since_seq"); `mv_state::activity::
/// replay_inbox`'s own filter is exclusive (`m.seq > since_seq`), so this
/// passes `since.saturating_sub(1)` to land on the same set. `--limit` is
/// deliberately not threaded through here, matching Python: `cli.py`'s own
/// replay branch (`cli.py:13919-13924`) never reads `args.limit`, only
/// `read_inbox`'s ordinary branch does.
fn replay_at(name: &str, root: &std::path::Path, since: u64) -> Result<(), Failure> {
    let log = ProjectLog::open(name.to_owned(), root);
    let messages =
        activity::replay_inbox(&log, since.saturating_sub(1), usize::MAX).map_err(fail)?;
    print_replay(name, &messages);
    print_open_ask_aging_footer(name, &log)?;
    Ok(())
}

/// `--replay-last N`: the last `N` messages by physical log position,
/// regardless of read state (`activity.py:3226-3229`) — a different
/// selection than "since", so this reads everything through the same
/// library call and keeps only the tail, rather than guessing a `since_seq`
/// that would land on the same N.
fn replay_last_at(name: &str, root: &std::path::Path, last: usize) -> Result<(), Failure> {
    let log = ProjectLog::open(name.to_owned(), root);
    let mut messages = activity::replay_inbox(&log, 0, usize::MAX).map_err(fail)?;
    let keep = messages.len().saturating_sub(last);
    messages.drain(..keep);
    print_replay(name, &messages);
    print_open_ask_aging_footer(name, &log)?;
    Ok(())
}

fn print_replay(name: &str, messages: &[mv_state::activity::MessageRecord]) {
    if messages.is_empty() {
        println!("nothing to replay for {name}.");
    }
    for message in messages {
        println!("{}", format_inbox_line(message));
    }
}

fn format_inbox_line(message: &mv_state::activity::MessageRecord) -> String {
    let tag = if message.ask {
        format!("ASK #{} ", message.seq)
    } else if let Some(re) = message.reply_to {
        format!("re #{re} ")
    } else {
        String::new()
    };
    let body = mv_board::layout::sanitize_title(&redact_text(&message.body));
    let from = mv_board::layout::sanitize_title(&message.from_project);
    let who = match &message.relayed_by {
        Some(by) => format!(
            "{from} (relayed by {})",
            mv_board::layout::sanitize_title(by)
        ),
        None => from,
    };
    format!(
        "[{}] {who}: {tag}{body}",
        mv_board::layout::sanitize_title(&message.when),
    )
}

/// Card `20261110-b-5a407b`: an aging signal for open asks, printed as a
/// footer after `inbox`'s listing (and after a replay's) so a still-open
/// ask does not sit invisible in the log between inbox reads. One line per
/// still-open ask this project holds, oldest first, worded in the same
/// `<holder> owes <sender>` direction as the board's OWED REPLIES card
/// (`mv-board/src/status_cards.rs`'s `owed_reply_card_lines`), condensed to
/// `<recipient> owes <sender> #<seq> <age>` to fit inline rather than in a
/// boxed card. Nothing is printed when `open` is empty.
fn open_ask_aging_lines(
    open: &[MessageRecord],
    this_project: &str,
    now: time::OffsetDateTime,
) -> Vec<String> {
    let mut open: Vec<&MessageRecord> = open.iter().collect();
    open.sort_by_key(|ask| ask.seq);
    open.into_iter()
        .map(|ask| {
            format!(
                "{} owes {} #{} {}",
                mv_board::layout::sanitize_title(this_project),
                mv_board::layout::sanitize_title(&ask.from_project),
                ask.seq,
                ask_age(&ask.when, now),
            )
        })
        .collect()
}

/// Same `Ns`/`Nm`/`Nh` unit selection as `board_data::compact_elapsed`
/// (`crates/mv-cli/src/commands/board_data.rs`), duplicated here rather
/// than shared because that helper is private to its own module and this
/// file's write scope does not reach across to it. `"?"` for a timestamp
/// that fails to parse or is somehow in the future, matching
/// `board_data::elapsed_compact`'s own `None` case.
fn ask_age(when: &str, now: time::OffsetDateTime) -> String {
    let Ok(at) = mv_core::time::parse_flexible(when) else {
        return "?".to_string();
    };
    if at > now {
        return "?".to_string();
    }
    let seconds = (now - at).whole_seconds().max(0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3_600)
    }
}

/// Looks up `log`'s still-open asks and prints [`open_ask_aging_lines`] for
/// them — the shared tail `inbox_at`/`replay_at`/`replay_last_at` all call,
/// so `--peek` and the replay flags show the same footer as an ordinary
/// read.
fn print_open_ask_aging_footer(name: &str, log: &ProjectLog) -> Result<(), Failure> {
    let open = activity::open_asks(log).map_err(fail)?;
    for line in open_ask_aging_lines(&open, name, mv_core::time::now_utc()) {
        println!("{line}");
    }
    Ok(())
}

fn update(args: &ArgMatches) -> Result<(), Failure> {
    let task = args.get_one::<String>("card").expect("required");
    let phase = args
        .get_one::<String>("phase")
        .map(|s| {
            UpdatePhase::parse(s).ok_or_else(|| {
                Failure::Operation(format!(
                    "--phase must be one of working, finding, wait; got {s:?}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(UpdatePhase::Working);
    let note = args
        .get_one::<String>("note")
        .map(String::as_str)
        .unwrap_or("");
    let (name, root) = current_project(args)?;
    update_at(&name, &root, task, phase, note)
}

fn update_at(
    name: &str,
    root: &std::path::Path,
    task: &str,
    phase: UpdatePhase,
    note: &str,
) -> Result<(), Failure> {
    let log = ProjectLog::open(name.to_owned(), root);
    match activity::post_update(&log, task, phase, note, &redact).map_err(fail)? {
        Some(seq) => println!("{}", update_posted_line(task, seq)),
        None => println!("{}", update_suppressed_line(task)),
    }
    Ok(())
}

/// `task` is whatever a caller (or a dispatched task's own `--task`) passed
/// in, so sanitize it before it reaches the operator's terminal — same
/// untrusted-field treatment as [`nudge_line`].
fn update_posted_line(task: &str, seq: u64) -> String {
    format!(
        "posted update for {} as seq {seq}",
        mv_board::layout::sanitize_title(task)
    )
}

fn update_suppressed_line(task: &str) -> String {
    format!(
        "update for {} suppressed by cooldown",
        mv_board::layout::sanitize_title(task)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removed on drop, including on panic (card `20261115-b-c8065f`: the
    /// plain `PathBuf` this replaced left every one of these behind in
    /// `TMPDIR`). `Deref`s to `Path` so every existing call site keeps
    /// reading `&root`/`root.join(...)` unchanged.
    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("mv-cli-activity-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        TempDir::new(tag)
    }

    fn open_ask(seq: u64, from: &str, body: &str) -> MessageRecord {
        MessageRecord {
            seq,
            when: String::new(),
            id: String::new(),
            from_project: from.to_string(),
            to_project: "me".to_string(),
            body: body.to_string(),
            ask: true,
            reply_to: None,
            relayed_by: None,
        }
    }

    #[test]
    fn plan_send_against_open_asks_sends_plain_with_no_candidates() {
        assert_eq!(
            plan_send_against_open_asks(vec![], "me", "them", "just an update"),
            SendAskPlan::Plain
        );
    }

    #[test]
    fn plan_send_against_open_asks_auto_replies_to_the_one_matching_candidate() {
        let ask = open_ask(1, "them", "please rotate the staging creds before 9am");
        assert_eq!(
            plan_send_against_open_asks(
                vec![ask.clone()],
                "me",
                "them",
                "rotated the staging creds before 9am"
            ),
            SendAskPlan::AutoReply(ask)
        );
    }

    #[test]
    fn plan_send_against_open_asks_sends_plain_with_a_hint_when_the_one_candidate_is_unrelated() {
        let ask = open_ask(1, "them", "please rotate the staging creds before 9am");
        assert_eq!(
            plan_send_against_open_asks(
                vec![ask.clone()],
                "me",
                "them",
                "unrelated status update about something else entirely"
            ),
            SendAskPlan::PlainWithHint(ask)
        );
    }

    /// The reproduction for card `20261110-b-657008`: a stale open ask
    /// (seq 1) sitting alongside a newer one (seq 2) must never make
    /// `send` guess — it must refuse, name both, and never fall through
    /// to [`SendAskPlan::Plain`] or [`SendAskPlan::AutoReply`], either of
    /// which a caller could mistake for "sent" having recorded nothing.
    #[test]
    fn plan_send_against_open_asks_refuses_to_guess_among_several_candidates() {
        let stale = open_ask(1, "them", "an ask from days ago");
        let fresh = open_ask(2, "them", "a different, newer ask");
        let plan = plan_send_against_open_asks(
            vec![fresh.clone(), stale.clone()],
            "me",
            "them",
            "a plain status update",
        );
        assert_eq!(plan, SendAskPlan::Ambiguous(vec![stale, fresh]));
    }

    #[test]
    fn nudge_line_strips_escape_sequences_from_name_and_reason() {
        let name = "proj\u{1b}]0;pwned\u{7}ect";
        let reason = "\u{1b}[2Kherdr: dialog";
        assert_eq!(nudge_line("nudged", name, None), "nudged    project");
        assert_eq!(
            nudge_line("unreached", name, Some(reason)),
            "unreached project — herdr: dialog"
        );
    }

    /// Two configured projects, `a` and `b`, each a real directory under
    /// `dir` so a path token has something on disk to resolve against.
    fn two_project_config(dir: &std::path::Path) -> (mv_config::load::Config, std::path::PathBuf) {
        let project_a = dir.join("project-a");
        std::fs::create_dir_all(&project_a).unwrap();
        let project_b = dir.join("project-b");
        std::fs::create_dir_all(&project_b).unwrap();
        let toml = format!(
            "[projects.a]\npath = \"{}\"\n[projects.b]\npath = \"{}\"\n",
            project_a.display(),
            project_b.display(),
        );
        let config = mv_config::load::load_config_str(&toml, &dir.join("config.toml")).unwrap();
        (config, project_a)
    }

    #[test]
    fn resolve_requested_projects_matches_a_literal_configured_name() {
        let dir = temp_dir("resolve-requested-literal");
        let (config, _a) = two_project_config(&dir);
        let requested = vec!["a".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, None, &requested);
        assert_eq!(resolved, vec!["a".to_string()]);
        assert!(unknown.is_empty());
    }

    /// Card `20261117-b-486a55`: `activity nudge .` used to fail with "no
    /// such project: ." because this lookup was a bare `contains_key`
    /// match with no path handling at all — the revert check for this
    /// task ran with [`resolve_requested_projects`] stubbed back down to
    /// that bare check, and this test failed exactly as expected before
    /// the real fix was restored.
    #[test]
    fn resolve_requested_projects_resolves_dot_to_the_cwd_project() {
        let dir = temp_dir("resolve-requested-dot");
        let (config, project_a) = two_project_config(&dir);
        let requested = vec![".".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&project_a), &requested);
        assert_eq!(resolved, vec!["a".to_string()]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_resolves_a_relative_path() {
        let dir = temp_dir("resolve-requested-relative");
        let (config, _a) = two_project_config(&dir);
        let requested = vec!["project-b".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert_eq!(resolved, vec!["b".to_string()]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_resolves_an_absolute_path() {
        let dir = temp_dir("resolve-requested-absolute");
        let (config, project_a) = two_project_config(&dir);
        let requested = vec![project_a.display().to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert_eq!(resolved, vec!["a".to_string()]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_reports_an_unknown_token() {
        let dir = temp_dir("resolve-requested-unknown");
        let (config, _a) = two_project_config(&dir);
        let requested = vec!["no-such-project".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert!(resolved.is_empty());
        assert_eq!(unknown, vec![&requested[0]]);
    }

    #[test]
    fn update_lines_strip_escape_sequences_from_the_task_id() {
        let task = "\u{1b}[2Ktask-1";
        assert_eq!(
            update_posted_line(task, 3),
            "posted update for task-1 as seq 3"
        );
        assert_eq!(
            update_suppressed_line(task),
            "update for task-1 suppressed by cooldown"
        );
    }

    /// Card `20261115-b-51cb75`: a missing or failing notify hook must
    /// never read as "nothing happened" — the warning names which one it
    /// was — and a sent/unconfigured push must print nothing at all.
    #[test]
    fn ask_notify_warning_names_the_hook_only_when_something_went_wrong() {
        use crate::commands::notify::PushOutcome;
        let missing = ask_notify_warning(PushOutcome::HookMissing).unwrap();
        assert!(missing.contains("notify hook"), "{missing}");
        assert!(missing.contains("curl"), "{missing}");
        assert!(ask_notify_warning(PushOutcome::Failed).is_some());
        assert_eq!(ask_notify_warning(PushOutcome::NoTopic), None);
        assert_eq!(ask_notify_warning(PushOutcome::Sent), None);
    }

    #[test]
    fn ask_ref_label_uses_a_hash_for_a_bare_seq() {
        assert_eq!(ask_ref_label("7"), "#7");
        assert_eq!(ask_ref_label("project-a#7"), "project-a#7");
    }

    #[test]
    fn default_message_id_is_namespaced_by_sender_and_recipient() {
        let id = default_message_id("multiverse", "example");
        assert!(id.starts_with("multiverse-example-"));
    }

    #[test]
    fn hub_inbox_preamble_applies_only_to_the_hub_project() {
        assert!(is_hub_inbox(activity::HUB_PROJECT_NAME));
        assert!(!is_hub_inbox("example"));
    }

    #[test]
    fn update_then_inbox_round_trip_on_one_project_log() {
        let root = temp_dir("update-inbox");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            log.log_path(),
            "2026-01-01T00:00:00+00:00\tv=1\tseq=1\tevent=dispatch\ttask=20261101-fixer-abcdef\n",
        )
        .unwrap();
        update_at(
            "multiverse",
            &root,
            "20261101-fixer-abcdef",
            UpdatePhase::Working,
            "started",
        )
        .unwrap();
        // The project's own `update` records never show up in its own
        // inbox (inbox only surfaces `event=message` addressed `to` it).
        inbox_at("multiverse", &root, 20, true, false, None).unwrap();
    }

    #[test]
    fn dispatched_task_id_blocks_acknowledgment_without_as_nexus() {
        let root = temp_dir("inbox-task-id-no-as-nexus");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        activity::send_message(
            &log,
            "msg-1",
            "other",
            "multiverse",
            "hi",
            false,
            None,
            true,
            &redact,
        )
        .unwrap();
        let state_dir = root.join(".multiverse");
        inbox_at(
            "multiverse",
            &root,
            20,
            false,
            false,
            Some("20261103-fixer-abcdef"),
        )
        .unwrap();
        assert_eq!(activity::read_cursor(&state_dir, &log).unwrap(), 0);
    }

    #[test]
    fn task_id_refusal_message_uses_nexus_wording_not_orchestrator() {
        let message = task_id_refusal_message("multiverse", "20261103-fixer-abcdef");
        assert!(
            message.to_lowercase().contains("nexus"),
            "refusal message must name the Nexus: {message}"
        );
        assert!(
            !message.to_lowercase().contains("orchestrator"),
            "refusal message must not use legacy orchestrator wording: {message}"
        );
    }

    #[test]
    fn as_nexus_authorizes_acknowledgment_despite_a_dispatched_task_id() {
        let root = temp_dir("inbox-task-id-as-nexus");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        activity::send_message(
            &log,
            "msg-1",
            "other",
            "multiverse",
            "hi",
            false,
            None,
            true,
            &redact,
        )
        .unwrap();
        let state_dir = root.join(".multiverse");
        inbox_at(
            "multiverse",
            &root,
            20,
            false,
            true,
            Some("20261103-fixer-abcdef"),
        )
        .unwrap();
        assert_eq!(activity::read_cursor(&state_dir, &log).unwrap(), 1);
    }

    #[test]
    fn no_dispatched_task_id_acknowledges_by_default() {
        let root = temp_dir("inbox-no-task-id");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        activity::send_message(
            &log,
            "msg-1",
            "other",
            "multiverse",
            "hi",
            false,
            None,
            true,
            &redact,
        )
        .unwrap();
        let state_dir = root.join(".multiverse");
        inbox_at("multiverse", &root, 20, false, false, None).unwrap();
        assert_eq!(activity::read_cursor(&state_dir, &log).unwrap(), 1);
    }

    #[test]
    fn replay_since_never_advances_the_cursor() {
        let root = temp_dir("replay-since");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        activity::send_message(
            &log,
            "msg-1",
            "other",
            "multiverse",
            "hi",
            false,
            None,
            true,
            &redact,
        )
        .unwrap();
        let state_dir = root.join(".multiverse");
        replay_at("multiverse", &root, 1).unwrap();
        assert_eq!(activity::read_cursor(&state_dir, &log).unwrap(), 0);
    }

    #[test]
    fn replay_last_returns_only_the_tail() {
        let root = temp_dir("replay-last");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        for (id, body) in [("msg-1", "one"), ("msg-2", "two"), ("msg-3", "three")] {
            activity::send_message(
                &log,
                id,
                "other",
                "multiverse",
                body,
                false,
                None,
                true,
                &redact,
            )
            .unwrap();
        }
        let state_dir = root.join(".multiverse");
        replay_last_at("multiverse", &root, 2).unwrap();
        assert_eq!(activity::read_cursor(&state_dir, &log).unwrap(), 0);
    }

    #[test]
    fn close_resolves_an_ask_this_project_is_holding() {
        let root = temp_dir("close");
        let log = ProjectLog::open("multiverse".to_owned(), &root);
        activity::send_message(
            &log,
            "ask-1",
            "other",
            "multiverse",
            "can you help?",
            true,
            None,
            true,
            &redact,
        )
        .unwrap();
        assert!(!activity::is_closed(&log, 1).unwrap());
        activity::close_ask(&log, 1, "declined", &redact).unwrap();
        assert!(activity::is_closed(&log, 1).unwrap());
    }

    #[test]
    fn nudge_prompt_matches_pythons_exact_wording() {
        assert_eq!(
            NUDGE_PROMPT,
            "Nudge from the hub: check your mail (`multiverse activity inbox`) and reconcile \
             before you dispatch anything else (`multiverse tasks --reconcile`)."
        );
    }

    #[test]
    fn plan_nudge_refuses_a_parked_trust_dialog_without_typing() {
        let text = "Trust this folder?\n› 1. Trust and continue\n2. Cancel\n";
        match plan_nudge(false, Some(text), "codex") {
            NudgePlan::UnreachedParked(reason) => {
                assert!(
                    reason.contains("dialog"),
                    "expected a dialog reason, got {reason:?}"
                );
            }
            other => panic!("parked dialog must be UnreachedParked, got {other:?}"),
        }
        assert_eq!(
            plan_nudge(false, None, "codex"),
            NudgePlan::UnreachedReadFailed
        );
        assert_eq!(
            plan_nudge(false, Some("Ready >"), "codex"),
            NudgePlan::Deliver
        );
    }

    #[test]
    fn plan_nudge_reports_already_working_as_unverifiable_before_any_pane_read() {
        assert_eq!(
            plan_nudge(true, Some("Trust this folder?"), "codex"),
            NudgePlan::UnverifiableWorking
        );
        assert_eq!(
            plan_nudge(true, None, "codex"),
            NudgePlan::UnverifiableWorking
        );
    }

    #[test]
    fn nexus_agent_name_is_project_scoped() {
        assert_eq!(
            mv_engine::fleet_start::nexus_agent_name("sandbox", "ne", "").unwrap(),
            "sandbox-nexu-ne"
        );
        assert_eq!(
            mv_engine::fleet_start::nexus_agent_name("multiverse", "cl", "").unwrap(),
            "multiverse-nexu-cl"
        );
    }

    #[test]
    fn partition_nudge_targets_splits_on_activity_pairs_and_excludes_the_sender() {
        let mut pairs = std::collections::BTreeMap::new();
        pairs.insert(
            "multiverse".to_string(),
            vec!["alpha".to_string(), "beta".to_string()],
        );
        let candidates = vec![
            "alpha".to_string(),
            "beta".to_string(),
            "gamma".to_string(),
            "multiverse".to_string(),
        ];
        let (permitted, skipped) = partition_nudge_targets("multiverse", candidates, &pairs);
        assert_eq!(permitted, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(
            skipped,
            vec![("gamma".to_string(), "not permitted by [activity.pairs]")]
        );
    }

    #[test]
    fn partition_nudge_targets_skips_everyone_with_no_configured_pairing() {
        let pairs = std::collections::BTreeMap::new();
        let candidates = vec!["alpha".to_string()];
        let (permitted, skipped) = partition_nudge_targets("multiverse", candidates, &pairs);
        assert!(permitted.is_empty());
        assert_eq!(
            skipped,
            vec![("alpha".to_string(), "not permitted by [activity.pairs]")]
        );
    }

    #[test]
    fn format_inbox_line_strips_osc_52_from_message_body() {
        let message = mv_state::activity::MessageRecord {
            seq: 1,
            when: "12:00".into(),
            id: "m1".into(),
            from_project: "alpha".into(),
            to_project: "beta".into(),
            body: "hello\u{1b}]52;c;EVIL\u{07}world".into(),
            ask: false,
            reply_to: None,
            relayed_by: None,
        };
        let line = format_inbox_line(&message);
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains('\u{07}'));
        assert!(!line.contains("]52;"));
        assert!(line.contains("helloworld"));
    }

    #[test]
    fn format_full_message_lists_every_field_unwrapped() {
        let ask = mv_state::activity::MessageRecord {
            seq: 7,
            when: "12:00".into(),
            id: "m1".into(),
            from_project: "alpha".into(),
            to_project: "beta".into(),
            body: "which seat?".into(),
            ask: true,
            reply_to: None,
            relayed_by: None,
        };
        let shown = format_full_message(&ask);
        assert!(shown.contains("#7 alpha -> beta at 12:00"), "{shown}");
        assert!(shown.contains("ask: yes"), "{shown}");
        assert!(shown.contains("which seat?"), "{shown}");

        let reply = mv_state::activity::MessageRecord {
            seq: 8,
            when: "12:01".into(),
            id: "m2".into(),
            from_project: "beta".into(),
            to_project: "alpha".into(),
            body: "seat-a".into(),
            ask: false,
            reply_to: Some(7),
            relayed_by: None,
        };
        let shown = format_full_message(&reply);
        assert!(shown.contains("reply to: #7"), "{shown}");
        assert!(!shown.contains("ask: yes"), "{shown}");
    }

    #[test]
    fn format_full_message_marks_a_relayed_reply_without_hiding_from_project() {
        let reply = mv_state::activity::MessageRecord {
            seq: 9,
            when: "12:02".into(),
            id: "m3".into(),
            from_project: "holder".into(),
            to_project: "asker".into(),
            body: "yes".into(),
            ask: false,
            reply_to: Some(7),
            relayed_by: Some("asker".into()),
        };
        let shown = format_full_message(&reply);
        assert!(shown.contains("#9 holder -> asker"), "{shown}");
        assert!(shown.contains("relayed by: asker"), "{shown}");
    }

    #[test]
    fn format_inbox_line_prefixes_asks_and_replies_with_their_seq() {
        let ask = mv_state::activity::MessageRecord {
            seq: 7,
            when: "12:00".into(),
            id: "m1".into(),
            from_project: "alpha".into(),
            to_project: "beta".into(),
            body: "which seat?".into(),
            ask: true,
            reply_to: None,
            relayed_by: None,
        };
        assert!(format_inbox_line(&ask).contains("ASK #7 which seat?"));
        let reply = mv_state::activity::MessageRecord {
            seq: 8,
            when: "12:01".into(),
            id: "m2".into(),
            from_project: "beta".into(),
            to_project: "alpha".into(),
            body: "seat-a".into(),
            ask: false,
            reply_to: Some(7),
            relayed_by: None,
        };
        assert!(format_inbox_line(&reply).contains("re #7 seat-a"));
    }

    #[test]
    fn format_inbox_line_marks_a_relayed_reply() {
        let reply = mv_state::activity::MessageRecord {
            seq: 9,
            when: "12:02".into(),
            id: "m3".into(),
            from_project: "holder".into(),
            to_project: "asker".into(),
            body: "seat-a".into(),
            ask: false,
            reply_to: Some(7),
            relayed_by: Some("asker".into()),
        };
        let line = format_inbox_line(&reply);
        assert!(
            line.contains("holder (relayed by asker): re #7 seat-a"),
            "{line}"
        );
    }

    /// Card `20261110-b-5a407b`'s own reproduction: an open ask, created
    /// (per the real stamped `when` `send_message` writes) 3h before the
    /// clock the footer is asked to render against, must show up as
    /// `<holder> owes <sender> #<seq> 3h` — the same direction wording and
    /// age format the board's OWED REPLIES card uses, condensed to one
    /// line. Once the ask is closed, the same lookup produces no line at
    /// all, not a stale one.
    #[test]
    fn open_ask_aging_lines_ages_an_open_ask_and_drops_it_once_closed() {
        let root = temp_dir("aging-basic");
        let log = ProjectLog::open("me".to_owned(), &root);
        activity::send_message(
            &log,
            "ask-1",
            "A",
            "me",
            "please help",
            true,
            None,
            true,
            &redact,
        )
        .unwrap();
        let open = activity::open_asks(&log).unwrap();
        assert_eq!(open.len(), 1);
        let created_at = mv_core::time::parse_flexible(&open[0].when).unwrap();
        let now = created_at + time::Duration::hours(3);

        assert_eq!(
            open_ask_aging_lines(&open, "me", now),
            vec!["me owes A #1 3h".to_string()]
        );

        activity::close_ask(&log, 1, "answered", &redact).unwrap();
        let open_after_close = activity::open_asks(&log).unwrap();
        assert!(open_after_close.is_empty());
        assert!(open_ask_aging_lines(&open_after_close, "me", now).is_empty());
    }

    /// Byte-identical-to-today's-output requirement: with no open asks, the
    /// footer contributes nothing — not even a header or an empty line.
    #[test]
    fn open_ask_aging_lines_prints_nothing_when_there_are_no_open_asks() {
        let now = mv_core::time::now_utc();
        assert!(open_ask_aging_lines(&[], "me", now).is_empty());
    }

    /// Several open asks print oldest first, in file/seq order, regardless
    /// of which sender holds the newer one.
    #[test]
    fn open_ask_aging_lines_orders_oldest_first() {
        let root = temp_dir("aging-order");
        let log = ProjectLog::open("me".to_owned(), &root);
        activity::send_message(
            &log,
            "ask-1",
            "A",
            "me",
            "first ask",
            true,
            None,
            true,
            &redact,
        )
        .unwrap();
        activity::send_message(
            &log,
            "ask-2",
            "B",
            "me",
            "second ask",
            true,
            None,
            true,
            &redact,
        )
        .unwrap();
        let open = activity::open_asks(&log).unwrap();
        assert_eq!(open.len(), 2);
        let now = mv_core::time::parse_flexible(&open[1].when).unwrap() + time::Duration::hours(1);

        let lines = open_ask_aging_lines(&open, "me", now);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("me owes A #1 "));
        assert!(lines[1].starts_with("me owes B #2 "));
    }

    /// `ask_age` matches `board_data::compact_elapsed`'s own unit
    /// selection (`s`/`m`/`h`) and falls back to `"?"` for a timestamp that
    /// does not parse or sits in the future — the same `None` case
    /// `board_data::elapsed_compact` has.
    #[test]
    fn ask_age_picks_the_right_unit_and_falls_back_on_bad_input() {
        let now = mv_core::time::parse_flexible("2026-01-01T00:10:00+00:00").unwrap();
        assert_eq!(ask_age("2026-01-01T00:09:55+00:00", now), "5s");
        assert_eq!(ask_age("2026-01-01T00:08:00+00:00", now), "2m");
        assert_eq!(ask_age("2025-12-31T22:10:00+00:00", now), "2h");
        assert_eq!(ask_age("not a timestamp", now), "?");
        assert_eq!(ask_age("2026-01-01T00:20:00+00:00", now), "?");
    }

    /// Wiring smoke test: `inbox_at` must not error, and must still print
    /// the aging footer line, when the inbox read itself has nothing new
    /// — the brief's "also when it reports 'nothing new'" case.
    #[test]
    fn inbox_at_runs_clean_with_an_open_ask_and_nothing_new_to_read() {
        let root = temp_dir("aging-inbox-wiring");
        let log = ProjectLog::open("me".to_owned(), &root);
        activity::send_message(
            &log,
            "ask-1",
            "A",
            "me",
            "please help",
            true,
            None,
            true,
            &redact,
        )
        .unwrap();
        let state_dir = root.join(".multiverse");
        let high_water = log.high_water_seq().unwrap();
        activity::acknowledge(&state_dir, &log, high_water, 1).unwrap();

        inbox_at("me", &root, 20, true, false, None).unwrap();
        replay_at("me", &root, 1).unwrap();
        replay_last_at("me", &root, 10).unwrap();
    }
}
