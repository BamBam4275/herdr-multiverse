//! `backlog add|list|done|reopen|annotate|reprioritize|show|ask|mark-asked|
//! answer`: durable project backlog cards, via `mv_state::backlog`.

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::load::{Config, Project};
use mv_state::backlog::{self, BacklogError, BacklogItem, Priority};
use time::OffsetDateTime;

use crate::exit::Failure;

/// Resolve every write/read verb against the configured registry, including
/// subdirectories and lanes containing their own `.multiverse` tree.
fn current_project(args: &ArgMatches) -> Result<(String, PathBuf), Failure> {
    let config = super::load_config(args)?;
    let project = cwd_project(&config).ok_or_else(|| {
        Failure::Config(format!(
            "{} is not inside any configured project; configured: {}",
            std::env::current_dir().unwrap_or_default().display(),
            config
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    Ok((
        project.name,
        PathBuf::from(project.path.value().expect("resolved project path")),
    ))
}

fn fail(e: BacklogError) -> Failure {
    Failure::Operation(e.to_string())
}

/// `cli.py`'s `done`/`reopen`/`reprioritize` branches each catch
/// `UnknownItemError`/`AmbiguousItemError` plus their own command-specific
/// wrong-state exception (`TerminalItemError`, `ItemNotDoneError`,
/// `ItemNotOpenError`) as `EXIT_NEEDS_CONFIG` — a question about which item
/// was meant or what state it is in, not a generic operation failure — and
/// let every other `BacklogError` fall through to `EXIT_ERROR`. This mirrors
/// that split for the library errors these commands can see after a
/// successful `resolve()`.
fn fail_terminal(e: BacklogError) -> Failure {
    match e {
        BacklogError::TerminalItem { .. }
        | BacklogError::ItemNotOpen { .. }
        | BacklogError::ItemNotDone { .. } => Failure::ItemLookup(e.to_string()),
        other => fail(other),
    }
}

/// Token resolution only ever fails with `UnknownItem`/`AmbiguousItem`
/// (`backlog::resolve_item_id`'s only two error returns), and every one of
/// `cli.py`'s `show`/`done`/`reopen`/`annotate`/`reprioritize`/`mark-asked`/
/// `answer` branches catches both as `EXIT_NEEDS_CONFIG` (3), never
/// `EXIT_ERROR` (1) — `_resolve_listed_backlog_item`'s two `except` clauses
/// for `show`, and the matching `except (UnknownItemError, AmbiguousItemError)`
/// pair repeated in each of the others. Found via `conformance/diff_fuzz.py`
/// seed 0: `backlog reopen` on an already-open item returned exit 1 here,
/// exit 3 in the frozen oracle.
fn resolve(root: &Path, token: &str) -> Result<String, Failure> {
    let items = backlog::read_items(root, false);
    let ids: Vec<&str> = items.iter().map(|i| i.item.as_str()).collect();
    backlog::resolve_item_id(ids, token).map_err(|e| Failure::ItemLookup(e.to_string()))
}

/// Resolve the caller through its live pane identity, never task environment
/// hints. Unreachable or unresolvable Herdr identity fails open.
fn dispatched_caller_name(args: &ArgMatches) -> Result<Option<String>, Failure> {
    let config = super::load_config(args)?;
    let Some(pane) = std::env::var("HERDR_PANE_ID")
        .ok()
        .filter(|pane| !pane.is_empty())
    else {
        return Ok(None);
    };
    let result = mv_engine::mv_herdr::run::run(&mv_engine::mv_herdr::argv::agent_list(), None).ok();
    let Some(result) = result.filter(|result| result.ok()) else {
        return Ok(None);
    };
    let mine = result
        .payload
        .as_ref()
        .and_then(|payload| payload["result"]["agents"].as_array())
        .and_then(|agents| {
            agents
                .iter()
                .find(|agent| agent["pane_id"].as_str() == Some(pane.as_str()))
        })
        .and_then(|agent| agent["name"].as_str())
        .map(str::trim)
        .filter(|name| super::workers::is_dispatched_name(&config, name));
    Ok(mine.map(str::to_owned))
}

fn default_priority(args: &ArgMatches) -> String {
    args.get_one::<String>("priority")
        .cloned()
        .unwrap_or_else(|| Priority::default().as_str().to_string())
}

/// `--recommend` is a 1-based index into the repeated `--option` list, not
/// one of the options' own text — `mv_state::backlog::{ask,mark_asked}`'s
/// `validate_options` checks the index is in range; this layer only converts
/// the CLI's signed integer into the state layer's index type.
fn recommendation_index(recommend: i64) -> Result<u32, Failure> {
    u32::try_from(recommend).map_err(|_| {
        Failure::Operation(format!(
            "--recommend {recommend:?} must be a 1-based index into --option"
        ))
    })
}

/// One `show`-output field, 2-space indented under the id/priority
/// header, with any continuation line of a multi-line value aligned
/// under the field's own content rather than column 0 — matches
/// Python's `show` branch (`cli.py:14588-14650`), whose own
/// `_print_indented` helper this mirrors (card `20261102-b-77c317`).
/// `list`'s own rendering is a separate, unrelated code path with its
/// own flush-left format and does not go through this.
fn render_indented(label: &str, value: &str) -> String {
    let value = mv_board::layout::sanitize_multiline(value);
    let prefix = format!("  {label}: ");
    let indent = " ".repeat(prefix.chars().count());
    let mut lines = value.lines();
    let mut out = format!("{prefix}{}\n", lines.next().unwrap_or(""));
    for line in lines {
        out.push_str(&format!("{indent}{line}\n"));
    }
    out
}

/// `backlog show`'s full rendering of one item — pulled out of
/// [`print_item`] as a string builder (mirroring `directives.rs`'s
/// `show_directive`) so its exact text is directly assertable in tests
/// without capturing stdout.
fn render_item(item: &BacklogItem) -> String {
    let marker = if item.state == "open" {
        String::new()
    } else {
        format!(" [{}]", item.state)
    };
    let mut text = format!("{} {}{marker}\n", item.item, item.priority);
    text.push_str(&render_indented("title", &item.title));
    if !item.options.is_empty() {
        text.push_str("  options:\n");
        for (i, option) in item.options.iter().enumerate() {
            let rec = if item.recommendation == Some((i + 1) as u32) {
                " (recommended)"
            } else {
                ""
            };
            let option = mv_board::layout::sanitize_title(option);
            text.push_str(&format!("    {}. {option}{rec}\n", i + 1));
        }
    }
    if !item.why.is_empty() {
        text.push_str(&render_indented("why", &item.why));
    }
    if !item.due.is_empty() {
        text.push_str(&render_indented("due", &item.due));
    }
    if !item.evidence.is_empty() {
        text.push_str(&render_indented("evidence", &item.evidence));
    }
    if !item.task_id.is_empty() {
        text.push_str(&render_indented("task", &item.task_id));
    }
    text.push_str(&render_indented("created", &item.created_at));
    text.push_str(&render_indented("updated", &item.updated_at));
    if item.annotations.is_empty() {
        text.push_str("  annotations: none\n");
    } else {
        text.push_str(&format!("  annotations ({}):\n", item.annotations.len()));
        for annotation in &item.annotations {
            text.push_str(&format!(
                "    [{}] {}: {}\n",
                mv_board::layout::sanitize_title(&annotation.when),
                mv_board::layout::sanitize_title(&annotation.by),
                mv_board::layout::sanitize_title(&annotation.note)
            ));
        }
    }
    text
}

fn print_item(item: &BacklogItem) {
    print!("{}", render_item(item));
}

pub fn run(matches: &ArgMatches) -> Result<(), Failure> {
    let (name, args) = matches.subcommand().expect("backlog requires a subcommand");
    // Like Python, load config before explaining surplus positionals, but
    // explain them before cwd resolution, pane checks, or any write.
    if name != "list" {
        super::load_config(args)?;
        validate_cwd_value(name, args)?;
    }
    match name {
        "add" => add(args),
        "list" => list(args),
        "done" => done(args),
        "reopen" => reopen(args),
        "annotate" => annotate(args),
        "reprioritize" => reprioritize(args),
        "show" => show(args),
        "ask" => ask(args),
        "mark-asked" => mark_asked(args),
        "answer" => answer(args),
        _ => unreachable!("clap restricts backlog to its documented subcommands"),
    }
}

/// `list` takes project names, but every other verb acts on cwd's project.
/// Accepting overflow lets us name the intended value instead of blaming it
/// as an unexpected argument at the parser boundary.
fn validate_cwd_value(command: &str, args: &ArgMatches) -> Result<(), Failure> {
    let field = match command {
        "add" => "title",
        "ask" => "question",
        _ => "item",
    };
    let values: Vec<&str> = args
        .get_many::<String>(field)
        .expect("required")
        .map(String::as_str)
        .collect();
    if values.len() == 1 {
        return Ok(());
    }
    let last = values.last().expect("required");
    let (noun, detail) = if field == "item" {
        let found = values
            .iter()
            .rfind(|value| {
                // Python's anchored item-id regex permits a final newline.
                let bytes = value.strip_suffix('\n').unwrap_or(value).as_bytes();
                bytes.len() == 17
                    && bytes[..8].iter().all(u8::is_ascii_digit)
                    && &bytes[8..11] == b"-b-"
                    && bytes[11..]
                        .iter()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            })
            .unwrap_or(last);
        (
            "an item id".to_string(),
            format!(
                "Found item id {} among {} arguments — drop the rest.",
                super::tasks::python_repr(found),
                values.len()
            ),
        )
    } else {
        (
            format!("a {field}"),
            format!(
                "Found {} arguments — did you mean just {}? Quote the {field} if it has spaces.",
                values.len(),
                super::tasks::python_repr(last)
            ),
        )
    };
    Err(Failure::Operation(format!(
        "backlog {command} takes only {noun}; it acts on the project your cwd is inside, \
         not one you name. {detail}"
    )))
}

fn add(args: &ArgMatches) -> Result<(), Failure> {
    let title = args.get_one::<String>("title").expect("required");
    let why = args
        .get_one::<String>("why")
        .map(String::as_str)
        .unwrap_or("");
    let due = args.get_one::<String>("due").map(String::as_str);
    let source = args.get_one::<String>("source").map(String::as_str);
    let role = args.get_one::<String>("role").map(String::as_str);
    let priority = default_priority(args);
    let (by, root) = current_project(args)?;
    let item =
        backlog::add(&root, title, &by, why, due, source, role, &priority, None).map_err(fail)?;
    println!("added {} to {}.", item.item, by);
    Ok(())
}

/// `cli.py::cmd_backlog`'s own fallback for `shutil.get_terminal_size
/// (fallback=(100, 24)).columns` — this crate has no real terminal-geometry
/// probe yet (`commands::board`'s `DEFAULT_WIDTH` doc comment notes the
/// same gap for `board`'s own, different, fallback width), so every run is
/// sized as if it were the non-tty case Python itself falls back to.
const BACKLOG_LIST_WIDTH: usize = 100;

/// `cli.py`'s `_fit_backlog_list_line`: truncate `line` to `width` cells at
/// a word boundary, with an ellipsis, via the same cell-width/break/truncate
/// primitives `mv_board::layout` already exposes (ported from
/// `unicodedata.east_asian_width` to `unicode-width` crate-wide).
fn fit_backlog_list_line(line: &str, width: usize) -> String {
    let line = mv_board::layout::sanitize_title(line);
    if mv_board::layout::cell_width(&line) <= width {
        return line;
    }
    if width <= 1 {
        return mv_board::layout::truncate(&line, width);
    }
    let (head, rest) = mv_board::layout::break_at_width(&line, width - 1);
    if rest.is_empty() {
        return mv_board::layout::truncate(&line, width);
    }
    let clipped = head.trim_end();
    if mv_board::layout::cell_width(clipped) >= width {
        return mv_board::layout::truncate(&line, width);
    }
    format!("{clipped}…")
}

/// `cli.py`'s `_backlog_list_state`: the `[asked]`/`[answered]`/`[done]`
/// marker, derived from `has_human_annotation` the same way the board uses
/// it for its own BACKLOG/BLOCKED split.
fn backlog_list_state(item: &BacklogItem) -> String {
    match item.state.as_str() {
        "open" => String::new(),
        "asked" => {
            if backlog::has_human_annotation(item) {
                " [answered]".to_string()
            } else {
                " [asked]".to_string()
            }
        }
        other => format!(" [{other}]"),
    }
}

fn today_str(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// `Config.project_for_path`'s own longest-prefix containment match, not
/// the exact-path equality `commands::current_project`/`board_data::
/// cwd_project` use elsewhere in this crate: those two exist to answer "is
/// cwd *exactly* a project root" (the first because `current_project_root`
/// needs an existing `.multiverse/` to walk up to; the second predates this
/// change). A read-only scope lookup has no such reason to be stricter than
/// Python, so this is its own small copy rather than a third caller of
/// either — see this task's `result.md` for why it is not shared.
fn cwd_project(config: &Config) -> Option<Project> {
    let cwd = std::env::current_dir().ok()?;
    let canonical_cwd = std::fs::canonicalize(&cwd).ok()?;
    config
        .projects
        .values()
        .filter_map(|project| {
            let root = std::fs::canonicalize(project.path.value()?).ok()?;
            (canonical_cwd == root || canonical_cwd.starts_with(&root))
                .then_some((project, root.components().count()))
        })
        .max_by_key(|(_, depth)| *depth)
        .map(|(project, _)| project.clone())
}

/// Every configured project, in `config.toml`'s own declaration order
/// rather than `config.projects`' alphabetical `BTreeMap` order — card
/// `20261102-b-624f17`: a fleet-wide `backlog list` from the hub project
/// used to show every project's items in the wrong order relative to
/// Python's `cmd_backlog`, which iterates `config.projects` in file
/// order. `project_order` already guarantees every key in `projects`
/// appears exactly once (`mv_config::load::Config::project_order`'s own
/// doc comment), so the lookup below cannot miss.
fn projects_in_declaration_order(config: &Config) -> Vec<Project> {
    config
        .project_order
        .iter()
        .filter_map(|name| config.projects.get(name).cloned())
        .collect()
}

/// `cmd_backlog`'s `list` scope resolution: named projects if given, else
/// `cmd_board`'s own hub rule (cwd inside the hub project, or outside every
/// project, widens to the whole fleet; cwd inside any other project narrows
/// to just that one).
fn resolve_list_projects(config: &Config, args: &ArgMatches) -> Result<Vec<Project>, Failure> {
    let named: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    if !named.is_empty() {
        let unknown: Vec<&str> = named
            .iter()
            .map(String::as_str)
            .filter(|name| !config.projects.contains_key(*name))
            .collect();
        if !unknown.is_empty() {
            let known = config
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Failure::Config(format!(
                "unknown project(s): {}; configured: {}",
                unknown.join(", "),
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known
                }
            )));
        }
        return Ok(named
            .iter()
            .map(|name| config.projects[name].clone())
            .collect());
    }
    match cwd_project(config) {
        Some(project) if project.name == mv_state::activity::HUB_PROJECT_NAME => {
            Ok(projects_in_declaration_order(config))
        }
        Some(project) => Ok(vec![project]),
        None => Ok(projects_in_declaration_order(config)),
    }
}

/// `cmd_backlog`'s `list` branch: named projects or the hub/fleet/cwd scope
/// `resolve_list_projects` works out, grouped by project, unfinished
/// (open+asked) items by default, `--done` for closed-only, `--all` for
/// both (`--open` is accepted but is the same as naming nothing).
fn list(args: &ArgMatches) -> Result<(), Failure> {
    let config = super::load_config(args)?;
    let projects = resolve_list_projects(&config, args)?;
    if projects.is_empty() {
        return Err(Failure::Config("no configured projects.".to_string()));
    }

    let done_only = args.get_flag("done");
    let show_all = args.get_flag("all");
    let show_unfinished = !(done_only || show_all);
    let today = today_str(mv_core::time::now_utc());

    let mut printed_any = false;
    let mut projects_shown = 0usize;
    let mut total = 0usize;

    for project in &projects {
        let Some(root) = project.path.value().map(PathBuf::from) else {
            continue;
        };
        let mut items: Vec<BacklogItem> = backlog::read_items(&root, false);
        if done_only {
            items.retain(|item| item.state == "done");
        } else if show_unfinished {
            items.retain(|item| item.state != "done");
        }
        if items.is_empty() {
            continue;
        }
        printed_any = true;
        projects_shown += 1;
        total += items.len();
        println!("\n{}", project.name);
        items.sort_by(|a, b| {
            (a.priority.as_str(), a.created_at.as_str())
                .cmp(&(b.priority.as_str(), b.created_at.as_str()))
        });
        for item in &items {
            let badge = if item.is_overdue(&today) {
                " OVERDUE"
            } else {
                ""
            };
            let due = if item.due.is_empty() {
                String::new()
            } else {
                format!(" due {}{badge}", item.due)
            };
            let state = backlog_list_state(item);
            let notes = if item.annotations.is_empty() {
                String::new()
            } else {
                format!(
                    " ({} note{})",
                    item.annotations.len(),
                    if item.annotations.len() == 1 { "" } else { "s" }
                )
            };
            let line = format!(
                "  {} {}{state} {}{due}{notes}",
                item.item, item.priority, item.title
            );
            let mut lines = line.lines();
            if let Some(head) = lines.next() {
                println!("{}", fit_backlog_list_line(head, BACKLOG_LIST_WIDTH));
            }
            for cont in lines {
                println!(
                    "{}",
                    fit_backlog_list_line(&format!("    {cont}"), BACKLOG_LIST_WIDTH)
                );
            }
        }
        println!(
            "  {} item{}",
            items.len(),
            if items.len() == 1 { "" } else { "s" }
        );
    }

    if !printed_any {
        println!("nothing in the backlog.");
    } else if projects_shown > 1 {
        println!("\n{total} item{} total", if total == 1 { "" } else { "s" });
    }

    Ok(())
}

fn done(args: &ArgMatches) -> Result<(), Failure> {
    done_for(args, dispatched_caller_name(args)?)
}

fn done_for(args: &ArgMatches, caller: Option<String>) -> Result<(), Failure> {
    if let Some(caller) = caller {
        return Err(Failure::Operation(format!(
            "backlog done was refused: the caller {caller:?} is a dispatched task, and a \
             specialist closing its own card is the wrong actor — report completion to your \
             orchestrator instead; it verifies by running and closes with evidence."
        )));
    }
    let token = args.get_one::<String>("item").expect("required");
    let evidence = args
        .get_one::<String>("evidence")
        .map(String::as_str)
        .unwrap_or("");
    let (_, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let item = backlog::done(&root, &id, evidence, None).map_err(fail_terminal)?;
    // `cli.py`: an evidence-less close is still allowed, but the omission
    // is surfaced here — the last point this command touches the screen —
    // rather than only discoverable later via `backlog show`.
    if item.evidence == backlog::NO_EVIDENCE_GIVEN {
        eprintln!(
            "warning: {} closed with no evidence — reason not recorded.",
            item.item
        );
    }
    println!("{} done.", item.item);
    Ok(())
}

fn reopen(args: &ArgMatches) -> Result<(), Failure> {
    reopen_for(args, dispatched_caller_name(args)?)
}

fn reopen_for(args: &ArgMatches, caller: Option<String>) -> Result<(), Failure> {
    if let Some(caller) = caller {
        return Err(Failure::Operation(format!(
            "backlog reopen was refused: the caller {caller:?} is a dispatched task, and a \
             specialist reopening a card is the wrong actor — report to your orchestrator \
             instead."
        )));
    }
    let token = args.get_one::<String>("item").expect("required");
    let (by, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let item = backlog::reopen(&root, &id, &by, None).map_err(fail_terminal)?;
    println!("{} reopened.", item.item);
    Ok(())
}

fn annotate(args: &ArgMatches) -> Result<(), Failure> {
    let token = args.get_one::<String>("item").expect("required");
    let note = args.get_one::<String>("note").expect("required");
    let (by, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let appended =
        backlog::append_annotation_if_absent(&root, &id, &by, note, None).map_err(fail)?;
    if appended {
        println!("recorded the annotation on {id}.");
    } else {
        println!("already recorded; nothing written for {id}.");
    }
    Ok(())
}

fn reprioritize(args: &ArgMatches) -> Result<(), Failure> {
    let token = args.get_one::<String>("item").expect("required");
    let priority = args.get_one::<String>("priority").expect("required");
    let (_, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let item = backlog::reprioritize(&root, &id, priority, None).map_err(fail_terminal)?;
    println!("{} is now {}.", item.item, item.priority);
    Ok(())
}

fn show(args: &ArgMatches) -> Result<(), Failure> {
    let token = args.get_one::<String>("item").expect("required");
    let (_, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let items = backlog::read_items(&root, false);
    let item = items
        .into_iter()
        .find(|i| i.item == id)
        .expect("resolve() only returns a known id");
    print_item(&item);
    Ok(())
}

fn ask(args: &ArgMatches) -> Result<(), Failure> {
    let question = args.get_one::<String>("question").expect("required");
    let options: Vec<String> = args
        .get_many::<String>("option")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    // `--recommend` is a required 1-based index into `--option` (clap's own
    // `value_parser!(u32)`), not option text to match by position — see
    // this task's `result.md`.
    let recommendation = *args.get_one::<u32>("recommend").expect("required");
    let why = args
        .get_one::<String>("why")
        .map(String::as_str)
        .unwrap_or("");
    let source = args.get_one::<String>("source").map(String::as_str);
    let role = args.get_one::<String>("role").map(String::as_str);
    let priority = default_priority(args);
    let (by, root) = current_project(args)?;
    let item = backlog::ask(
        &root,
        question,
        &options,
        recommendation,
        &by,
        why,
        source,
        role,
        &priority,
        None,
    )
    .map_err(fail)?;
    println!("asked {} in {}.", item.item, by);
    Ok(())
}

fn mark_asked(args: &ArgMatches) -> Result<(), Failure> {
    let token = args.get_one::<String>("item").expect("required");
    let options: Vec<String> = args
        .get_many::<String>("option")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let recommendation =
        recommendation_index(*args.get_one::<i64>("recommend").expect("required"))?;
    let (_, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let item = backlog::mark_asked(&root, &id, &options, recommendation, None).map_err(fail)?;
    println!("marked {} asked", item.item);
    Ok(())
}

/// `answer`'s target is a human's reply to a backlog `ask`, recorded the
/// way `has_human_annotation` recognizes one (`by == "human"`) — there is
/// no dedicated `mv_state::backlog::answer` function, so this is an
/// annotation under that fixed `by`, not the calling project's own name.
fn answer(args: &ArgMatches) -> Result<(), Failure> {
    let token = args.get_one::<String>("item").expect("required");
    let note = args.get_one::<String>("note").expect("required");
    let (_, root) = current_project(args)?;
    let id = resolve(&root, token)?;
    let item = backlog::read_items(&root, false)
        .into_iter()
        .find(|item| item.item == id)
        .expect("resolved item");
    if item.state != "asked" {
        return Err(Failure::Operation(format!(
            "{} is {:?}, not asked; answer only applies to a question waiting on a human.",
            item.item, item.state
        )));
    }
    let recorded =
        backlog::append_annotation_if_absent(&root, &id, "human", note, None).map_err(fail)?;
    if recorded {
        println!("recorded the answer on {id}.");
    } else {
        println!("already recorded; nothing written for {id}.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mv-cli-backlog-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_resolve_show_and_done_round_trip() {
        let root = temp_dir("roundtrip");
        let item = backlog::add(
            &root,
            "Repair fixture issue",
            "sandbox",
            "A reproducible failure",
            None,
            None,
            Some("builder"),
            "P2",
            None,
        )
        .unwrap();

        let resolved = resolve(&root, &item.item).unwrap();
        assert_eq!(resolved, item.item);
        let prefix = &item.item[..item.item.len() - 1];
        assert_eq!(resolve(&root, prefix).unwrap(), item.item);

        let items = backlog::read_items(&root, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, "A reproducible failure");
        assert_eq!(items[0].role, "builder");

        let done = backlog::done(&root, &item.item, "Verified in fixture", None).unwrap();
        assert_eq!(done.state, "done");
        assert_eq!(done.evidence, "Verified in fixture");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recommendation_index_parses_a_one_based_position() {
        assert_eq!(recommendation_index(2).unwrap(), 2);
        assert_eq!(recommendation_index(-1).unwrap_err().code(), 1);
        assert!(recommendation_index(i64::MAX).is_err());
    }

    #[test]
    fn resolving_an_unknown_item_refuses() {
        let root = temp_dir("unknown");
        assert!(resolve(&root, "no-such-item").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `cli.py`'s `show`/`done`/`reopen`/`annotate`/`reprioritize` branches
    /// all catch `UnknownItemError`/`AmbiguousItemError` as
    /// `EXIT_NEEDS_CONFIG` (3), never `EXIT_ERROR` (1) — found diverging
    /// from the frozen oracle via `conformance/diff_fuzz.py` seed 0
    /// (`backlog reopen` on an already-open item returned exit 1 here,
    /// exit 3 there, because this file's `resolve()` used the generic
    /// `fail()` mapping instead).
    #[test]
    fn resolve_errors_map_to_needs_config_not_generic_error() {
        let root = temp_dir("resolve-exit-code");
        let unknown = resolve(&root, "no-such-item").unwrap_err();
        assert_eq!(unknown.code(), crate::exit::NEEDS_CONFIG);

        backlog::add(&root, "one", "sandbox", "", None, None, None, "P2", None).unwrap();
        backlog::add(&root, "two", "sandbox", "", None, None, None, "P2", None).unwrap();
        let ambiguous = resolve(&root, "2026").unwrap_err();
        assert_eq!(ambiguous.code(), crate::exit::NEEDS_CONFIG);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The command-specific wrong-state errors (`reopen` on an open item,
    /// `done`/`reprioritize` on an already-done one) are the same
    /// `EXIT_NEEDS_CONFIG` family in the frozen oracle; everything else
    /// `mv_state::backlog` can raise stays `EXIT_ERROR`.
    #[test]
    fn fail_terminal_maps_wrong_state_errors_to_needs_config() {
        assert_eq!(
            fail_terminal(BacklogError::ItemNotDone {
                item_id: "x".into(),
                state: "open".into(),
            })
            .code(),
            crate::exit::NEEDS_CONFIG
        );
        assert_eq!(
            fail_terminal(BacklogError::TerminalItem {
                item_id: "x".into()
            })
            .code(),
            crate::exit::NEEDS_CONFIG
        );
        assert_eq!(
            fail_terminal(BacklogError::ItemNotOpen {
                item_id: "x".into(),
                state: "done".into(),
            })
            .code(),
            crate::exit::NEEDS_CONFIG
        );
        assert_eq!(
            fail_terminal(BacklogError::IdExhausted).code(),
            crate::exit::ERROR
        );
    }

    /// Card 20261102-b-77c317: `show`'s rendering had no field indentation
    /// at all (everything flush at column 0) and printed `role:`/`source:`
    /// lines Python's `show` branch never emits. Pins 2-space field
    /// indentation, 4-space annotation indentation, and the absence of
    /// `role:`/`source:` even though the item carries a non-empty role.
    #[test]
    fn render_item_indents_fields_and_drops_role_and_source_lines() {
        let root = temp_dir("render-item");
        let item = backlog::add(
            &root,
            "Repair fixture issue",
            "sandbox",
            "A reproducible failure",
            None,
            None,
            Some("builder"),
            "P2",
            None,
        )
        .unwrap();
        backlog::append_annotation_if_absent(&root, &item.item, "sandbox", "checked twice", None)
            .unwrap();
        let items = backlog::read_items(&root, false);
        let text = render_item(&items[0]);
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines[0], format!("{} P2", item.item));
        assert_eq!(lines[1], "  title: Repair fixture issue");
        assert_eq!(lines[2], "  why: A reproducible failure");
        assert!(lines.iter().any(|l| l.starts_with("  created: ")));
        assert!(lines.iter().any(|l| l.starts_with("  updated: ")));
        assert!(lines.contains(&"  annotations (1):"));
        assert!(lines
            .iter()
            .any(|l| l.starts_with("    [") && l.contains("sandbox: checked twice")));
        assert!(
            !text.contains("role:") && !text.contains("source:"),
            "show must not print role:/source: lines (Python's show predates both fields): {text}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_indented_aligns_continuation_lines_under_the_fields_own_content() {
        let text = render_indented("why", "first line\nsecond line");
        assert_eq!(text, "  why: first line\n       second line\n");
    }

    #[test]
    fn render_indented_strips_osc_52_from_untrusted_field_text() {
        let text = render_indented("title", "hello\u{1b}]52;c;QUJDRA==\u{07}world");
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{07}'));
        assert!(!text.contains("]52;"));
        assert!(text.contains("helloworld"));
    }

    #[test]
    fn fit_backlog_list_line_strips_osc_even_when_the_line_already_fits() {
        let line = fit_backlog_list_line("ok\u{1b}]52;c;EVIL\u{07} title", 100);
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains('\u{07}'));
        assert_eq!(line, "ok title");
    }

    fn backlog_subcommand_matches(argv: &[&str], name: &str) -> clap::ArgMatches {
        let matches = crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        matches
            .subcommand_matches("backlog")
            .unwrap()
            .subcommand_matches(name)
            .unwrap()
            .clone()
    }

    #[test]
    fn done_refuses_a_dispatched_caller_before_touching_the_backlog() {
        let args = backlog_subcommand_matches(&["backlog", "done", "20261102-b-aaaaaa"], "done");
        let error = done_for(&args, Some("20261102-builder-abc123".to_string())).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        let message = error.to_string();
        assert!(message.contains("backlog done was refused"));
        assert!(message.contains("20261102-builder-abc123"));
    }

    #[test]
    fn reopen_refuses_a_dispatched_caller_before_touching_the_backlog() {
        let args =
            backlog_subcommand_matches(&["backlog", "reopen", "20261102-b-aaaaaa"], "reopen");
        let error = reopen_for(&args, Some("20261102-builder-abc123".to_string())).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        let message = error.to_string();
        assert!(message.contains("backlog reopen was refused"));
        assert!(message.contains("20261102-builder-abc123"));
    }
}
