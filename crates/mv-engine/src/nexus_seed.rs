//! Nexus assignment rendering and preservation of edited seeds.
//!
//! Routing and review policy are resolved by their owning modules before calling
//! this renderer. No selection, fleet access, or history mutation happens here.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use mv_state::activity::HUB_PROJECT_NAME;
use mv_state::backlog;

use crate::directives::{self, content_digest, NO_DIRECTIVE_DIGEST};

/// The current seed filename. A project already carrying
/// [`LEGACY_SEED_FILENAME`] is migrated onto this one the next time its
/// seed is written (`write_seed`/`write_home_seed`); nothing renames it
/// out from under a project that is never reseeded.
pub const SEED_FILENAME: &str = "nexus.md";
/// The filename every seed carried before the rename. Still accepted when
/// reading: a project that has not been reseeded since the rename keeps
/// this file until its next write.
pub const LEGACY_SEED_FILENAME: &str = "orchestrator.md";
const MARKER: &str = "<!-- multiverse:seed-digest ";

/// Where a writer puts a project's (or the home's) seed — always the
/// current name. The actual migration away from an existing
/// [`LEGACY_SEED_FILENAME`] sibling happens inside `write_seed`/
/// `write_home_seed`'s shared writer, so a caller never joins the filename
/// itself and never has to decide between the two names.
pub fn seed_path(root: &Path) -> PathBuf {
    root.join(".multiverse").join(SEED_FILENAME)
}

/// Where a read-only caller should look: the current name if it exists,
/// else the legacy name if that alone exists, else `None` — nothing has
/// ever seeded this project or home yet.
pub fn existing_seed_path(root: &Path) -> Option<PathBuf> {
    let current = seed_path(root);
    if current.is_file() {
        return Some(current);
    }
    let legacy = root.join(".multiverse").join(LEGACY_SEED_FILENAME);
    legacy.is_file().then_some(legacy)
}

/// If `path` is the canonical seed path and a legacy sibling exists,
/// preserve any hand edits to it in a backup (same rule `backup_if_edited`
/// already applies), then remove it so at most one seed file remains.
/// Never called for any other path, so a project still reading/writing its
/// own legacy file (never reseeded) is left alone until it is.
fn migrate_legacy_sibling(path: &Path) -> io::Result<Option<PathBuf>> {
    if path.file_name().and_then(|name| name.to_str()) != Some(SEED_FILENAME) {
        return Ok(None);
    }
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let legacy = parent.join(LEGACY_SEED_FILENAME);
    if !legacy.is_file() {
        return Ok(None);
    }
    let backup = backup_if_edited(&legacy)?;
    fs::remove_file(&legacy)?;
    Ok(backup)
}

/// Private vocabulary until mv-config's role API lands. Only new write-side
/// names are exposed; history readers elsewhere still accept retired names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SeedRole {
    /// Fast, read-only codebase reconnaissance that returns a compressed map.
    Scout,
    /// Implements a scoped, already-decided change inside an isolated worktree.
    Builder,
    /// External knowledge retrieval (library docs, API references, release notes).
    Archivist,
    /// Adversarial review of another agent's work, on a different vendor CLI.
    Critic,
    /// Architecture, risk, and hard debugging, escalated when being wrong is costly.
    Seer,
    /// Multi-vendor decision support convened on a question judged too consequential
    /// for a single model.
    Council,
    /// UI/UX design, the edits that implement it, and design review.
    Artisan,
    /// Visual and media analysis, converted into concise structured text.
    Watcher,
}

impl SeedRole {
    /// Lowercase slug used on the dispatch command line (`--role <key>`).
    pub fn key(self) -> &'static str {
        match self {
            Self::Scout => "scout",
            Self::Builder => "builder",
            Self::Archivist => "archivist",
            Self::Critic => "critic",
            Self::Seer => "seer",
            Self::Council => "council",
            Self::Artisan => "artisan",
            Self::Watcher => "watcher",
        }
    }

    /// Capitalized name shown to a human in the rendered seed's role table.
    pub fn display(self) -> &'static str {
        match self {
            Self::Scout => "Scout",
            Self::Builder => "Builder",
            Self::Archivist => "Archivist",
            Self::Critic => "Critic",
            Self::Seer => "Seer",
            Self::Council => "Council",
            Self::Artisan => "Artisan",
            Self::Watcher => "Watcher",
        }
    }
}

/// A role entry for the seed's dispatch table: which role, and whether it
/// currently has an account to route to.
#[derive(Debug, Clone)]
pub struct DispatchableRole {
    pub role: SeedRole,
    /// A configured role stays visible when selection fails.
    pub account: Result<String, String>,
}

/// How a project recorded its pre-halt handoff. Most real projects write
/// this as backlog text, not a file (card `20261102-b-0d8483`): six of the
/// seven projects checked at the 2026-11-01 cutover left their full handoff
/// as an open backlog item's own title/why, and one left nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handoff {
    /// The newest open backlog card whose title or why mentions "handoff"
    /// (case-insensitive) — the shape most real projects actually wrote.
    Card { item: String, title: String },
    /// No qualifying card; the conventional on-disk file exists instead.
    File(PathBuf),
    /// Neither a card nor a file: say so rather than pointing at a path
    /// nothing ever wrote.
    None,
}

/// Project-relative convention checked only when no backlog card qualifies.
const HANDOFF_FILE: &str = ".multiverse/handoff.md";

fn mentions_handoff(text: &str) -> bool {
    text.to_ascii_lowercase().contains("handoff")
}

/// A card titled "HANDOFF ..." *is* the standing handoff; a card that merely
/// mentions the word in passing (e.g. a cleanup checklist naming a file
/// called `handoff`) is not. Leading-title cards must outrank incidental
/// mentions regardless of recency, or a newer unrelated card with the word
/// anywhere in its why can bury the real, still-open handoff indefinitely.
fn title_leads_with_handoff(title: &str) -> bool {
    title
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("handoff")
}

/// Resolve `repository`'s pre-halt handoff the way projects actually write
/// it: among open cards whose title or why mentions "handoff", a card whose
/// title leads with "handoff" always outranks one that only mentions it in
/// passing; newest wins within whichever group qualifies. The conventional
/// file is checked second (existence checked, never assumed), an explicit
/// "there is none" third.
pub fn resolve_handoff(repository: &Path) -> Handoff {
    let candidates: Vec<_> = backlog::read_items(repository, true)
        .into_iter()
        .filter(|item| mentions_handoff(&item.title) || mentions_handoff(&item.why))
        .collect();
    let newest = candidates
        .iter()
        .filter(|item| title_leads_with_handoff(&item.title))
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .or_else(|| {
            candidates
                .iter()
                .max_by(|a, b| a.created_at.cmp(&b.created_at))
        });
    if let Some(item) = newest {
        return Handoff::Card {
            item: item.item.clone(),
            title: item.title.clone(),
        };
    }
    let file = repository.join(HANDOFF_FILE);
    if file.is_file() {
        return Handoff::File(file);
    }
    Handoff::None
}

fn handoff_instruction(handoff: &Handoff) -> String {
    match handoff {
        Handoff::Card { item, title } => format!(
            "Read the project's handoff — backlog card `{}` (\"{}\") — before dispatching.",
            cell(item),
            cell(title)
        ),
        Handoff::File(path) => format!(
            "Read the project's handoff card at `{}` before dispatching.",
            path.display()
        ),
        Handoff::None => "This project recorded no handoff card or file. Start from the \
             backlog and the recent routing log, and resume open work and decisions from there."
            .to_string(),
    }
}

/// Conventional path of the fleet-down/up wrap document a Nexus writes
/// before going idle (card `20261116-b-f0612d`). Local only, never checked
/// into git. Section completeness is prompt-enforced, not parsed here.
pub const PASSDOWN_FILE: &str = ".multiverse/PASSDOWN.md";

/// One-line sidecar deciding whether a PASSDOWN is pending. Its *content*
/// gates the resume hook below, never [`PASSDOWN_FILE`]'s mere existence —
/// this project's own hand-written `PASSDOWN.md` is a standing archive with
/// no sidecar, and keying on existence alone would re-run BRING-UP on every
/// `start`.
const PASSDOWN_STATE_FILE: &str = ".multiverse/passdown.state";

/// Resolve whether `repository` (or the home root, for the hub) is sitting
/// on a pending fleet-down wrap. `None` only when the sidecar is absent —
/// an ordinary project with a stale or archival `PASSDOWN.md` and no
/// sidecar resumes exactly as it did before this hook existed. An existing
/// sidecar that cannot be read, or whose content is neither `pending` nor
/// `wrapped` nor `up`, fails closed as pending (the sidecar contract's own
/// fail-closed row); `up` means BRING-UP already ran and is not pending.
/// Either way, `Some` additionally requires [`PASSDOWN_FILE`] to exist as a
/// file, so a sidecar with no document behind it resolves to `None` too.
pub fn resolve_passdown(repository: &Path) -> Option<PathBuf> {
    let pending = match fs::read_to_string(repository.join(PASSDOWN_STATE_FILE)) {
        Ok(text) => text.trim() != "up",
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    };
    if !pending {
        return None;
    }
    let passdown = repository.join(PASSDOWN_FILE);
    passdown.is_file().then_some(passdown)
}

/// Shared by `seed_body` and `home_seed_body`: the sentence placed before
/// the existing handoff/resume text when a PASSDOWN is pending. Ends with a
/// trailing space so a caller concatenating it directly before another
/// sentence (`seed_body`) needs no separator of its own; a caller giving it
/// its own paragraph (`home_seed_body`) trims it.
fn pending_passdown_block(path: &Path) -> String {
    format!(
        "A pending PASSDOWN is at `{}`. Read it now. Execute its BRING-UP section before \
         dispatching. When checks pass, replace `.multiverse/passdown.state` with the single \
         line `up`, then continue Every cycle. ",
        path.display()
    )
}

/// Delivered by `down` (Lane B, card `20261116-b-f0612d`) into a live Nexus
/// through the same `deliver_prompt_to` path `start --reseed` already uses.
/// Defined here rather than in Lane B's own module because every detail it
/// names — the sidecar values and [`PASSDOWN_FILE`]'s section names — is
/// this module's own contract.
pub const WRAP_PROMPT: &str = "The fleet is going down. Let any live lane finish, or commit \
    its work to its own lane branch — never main, never a merge. Write `.multiverse/PASSDOWN.md` \
    with `## STATE`, `## IN FLIGHT`, `## DECISIONS`, `## NEXT UP`, `## BRING-UP`, and \
    `## GOTCHAS` sections; `BRING-UP` names exact commands for this project, or says \"nothing \
    to restart\" when there is none. After a verified backup, stop this project's own remote \
    stack with `compose down`, never `-v`. Then replace `.multiverse/passdown.state` with the \
    single line `wrapped` and go idle. Do not close your own pane.";

/// Everything `seed_body`/`render_seed` needs to render a project's Nexus
/// seed: resolved config, resolved handoff, and the roles currently
/// dispatchable in this fleet.
pub struct SeedContext<'a> {
    pub project: &'a str,
    pub repository: &'a Path,
    /// How this project's pre-halt session left things, resolved once by
    /// `resolve_handoff` before the seed is rendered.
    pub handoff: Handoff,
    /// Resolved via directives_root for the selected config's fleet state.
    pub directives_root: &'a Path,
    pub roles: &'a [DispatchableRole],
    pub review_policy: &'a str,
    pub max_concurrent: usize,
    pub dispatch_min_quota_percent: u8,
    pub lend_interactive_to_workers: bool,
    /// Already body-stripped; None uses the shipped Nexus prompt.
    pub role_prompt: Option<&'a str>,
}

/// Read on every call: the plugin's script-derived invocation can change
/// between starts. Whitespace-only overrides have the token default.
pub fn multiverse_invocation() -> String {
    invocation(std::env::var("MULTIVERSE_INVOCATION").ok().as_deref())
}

fn invocation(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("multiverse")
        .into()
}

fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

fn directive_state_for(directives_root: &Path, project: &str) -> (String, Option<String>) {
    let result = directives::read(directives_root, project)
        .and_then(|_| directives::digest(directives_root, project));
    match result {
        Ok(digest) => (digest, None),
        Err(error) => (NO_DIRECTIVE_DIGEST.into(), Some(error.to_string())),
    }
}

fn directive_state(context: &SeedContext<'_>) -> (String, Option<String>) {
    directive_state_for(context.directives_root, context.project)
}

fn seed_body(context: &SeedContext<'_>, command: &str) -> String {
    let project = shell_word(context.project);
    let repository = shell_word(&context.repository.to_string_lossy());
    let (directive_digest, warning) = directive_state(context);
    let directive_path = context
        .directives_root
        .join(format!("{}.md", context.project));
    let passdown = resolve_passdown(context.repository);
    let resume_line = match &passdown {
        Some(path) => format!(
            "{}{}",
            pending_passdown_block(path),
            handoff_instruction(&context.handoff)
        ),
        None => handoff_instruction(&context.handoff),
    };
    let mut text = format!(
        "# Nexus — {}\n\n\
         You are assigned to repository `{}`. Manage this project's work; specialists implement it.\n\n\
         ## Isolation\n\n\
         Never read, list, or search another project's repository or its containing directory. \
         If a task appears to require another project, message that project by name through the approved gateway.\n\n\
         ## Resume this project's history\n\n\
         {} Resume its open work and decisions. \
         Read `.multiverse/routing.log`, tasks, backlog, and existing lanes; preserve their history and old task IDs. \
         Never interpret an empty pane or a fresh Nexus session as a fresh project.\n\n\
         ## Standing directives\n\n\
         Re-read `{}` every cycle. It is the human's standing authority, never a message or pane paste. \
         Missing or expired means no standing directives. A malformed or unreadable file confers no authority; report its warning. \
         Directive content is read from that file, never snapshotted into this seed.\n\n\
         With active directives, follow their authority. Without them, check human presence before asking. \
         Reversible work within your scope may proceed when the human is absent; irreversible decisions \
         outside existing authority wait for the human. Do not use AskUserQuestion to park authorized reversible work.\n\n\
         <!-- multiverse:directives-digest {} -->\n\n",
        cell(context.project), context.repository.display(), resume_line,
        directive_path.display(), directive_digest,
    );
    if let Some(warning) = warning {
        text.push_str(&format!("Warning: {}\n\n", cell(&warning)));
    }
    text.push_str("## Before you dispatch anything\n\n```sh\n");
    text.push_str(&format!("cd {repository}\n"));
    for action in [
        format!("tasks {project}"),
        format!("worktrees {repository}"),
        format!("board {project}"),
        "check".into(),
        "activity inbox --as-nexus".into(),
        "accounts".into(),
        format!("backlog list {project}"),
    ] {
        text.push_str(&format!("{command} {action}\n"));
    }
    text.push_str("```\n\n\
        For a non-mutating report, add `--read-only` to `tasks`/`board`, or set \
        `MULTIVERSE_READ_ONLY=1`; `check` is read-only already. These suppress \
        reconciliation, reclaim/close, outcome, breaker, and cache writes.\n\n\
        - **A lane with uncommitted changes holds work that exists in exactly one \
        place.** Read it before dispatching anything that might touch the same files.\n\
        - **A re-dispatch with a numeric collision suffix first finds the live \
        predecessor.** A clean, inspectable predecessor is recorded as superseded and \
        its old pane closed before the new dispatch starts; a dirty or uninspectable \
        lane refuses the new dispatch and leaves the old pane/worktree open — resume \
        or rescue it first.\n\
        - **A `done` status is not accepted when a writing lane is dirty.** Inspect \
        the lane, then commit the work or record failed/superseded as appropriate.\n\
        - **A running agent may be from a previous session and still spending.** It \
        counts against the concurrency cap whether or not you dispatched it.\n\
        - **An unread inbox may hold a request that has been waiting for hours** — \
        nothing wakes you to go read it.\n\n\
        ## Standing idle work\n\n\
        Check docs against code, the hosted copy against its source, OpenAPI against implementation, \
        frontend against its spec, security boundaries, and human-readable comments. Dispatch concrete gaps.\n\n\
        ## What you may dispatch here\n\n");
    let mut roles: Vec<_> = context.roles.iter().collect();
    // Stable order regardless of config/caller iteration order, so the
    // rendered table (and its digest) doesn't change on every call.
    roles.sort_by_key(|route| route.role.key());
    let available: Vec<_> = roles.iter().filter(|route| route.account.is_ok()).collect();
    if available.is_empty() {
        text.push_str("Nothing. No configured role is currently routable.\n\n");
    } else {
        text.push_str("| Role | Seat | Dispatch |\n| --- | --- | --- |\n");
        let mut has_critic = false;
        for route in available {
            // Filtered to `account.is_ok()` above.
            let account = route.account.as_ref().expect("available route");
            let dispatch = match route.role {
                SeedRole::Council => format!("{command} council {repository} '<question>' --yes"),
                SeedRole::Critic => {
                    has_critic = true;
                    format!(
                        "{command} dispatch {repository} '<goal>' --role critic --reviewing <author-task-id> --yes"
                    )
                }
                _ => format!(
                    "{command} dispatch {repository} '<goal>' --role {} --yes",
                    route.role.key()
                ),
            };
            text.push_str(&format!(
                "| {} | {} | `{}` |\n",
                route.role.display(),
                cell(account),
                cell(&dispatch)
            ));
        }
        text.push('\n');
        if has_critic {
            text.push_str(
                "`--reviewing <author-task-id>` resolves that task's author from this \
                 project's own routing log and excludes their vendor — it refuses rather \
                 than reviewing same-vendor when nothing else is eligible. If the author \
                 was never a recorded task, use `--exclude-vendor <vendor>` instead. \
                 Repeatable, up to `batch_max_lanes` — several lanes get one brief, one \
                 section, and one MERGE/REVISE/BLOCK verdict slot each.\n\n",
            );
        }
    }
    for route in roles {
        if let Err(reason) = &route.account {
            text.push_str(&format!(
                "- {}: configured but unavailable — {}\n",
                route.role.display(),
                cell(reason)
            ));
        }
    }
    text.push_str(&format!(
        "\n## Review policy\n\n{}\n\n\
         Nexus starts through `start`, never worker dispatch. Council convenes through `council`.\n\n\
         ## Tuning your own routing\n\n\
         Project-local `.multiverse/roster.toml` overrides only `[chains]`. Retain config comments. \
         `lend_interactive_to_workers = {}`; do not lend a reserved interactive seat unless this gate permits it.\n\n\
         ## The rules you are held to\n\n\
         Fleet `max_concurrent = {}`; `dispatch_min_quota_percent = {}`. \
         You manage the work; specialists do it. Stay within each lane's declared write scope. \
         Never infer task completion from a Herdr done/idle/unknown label: read result and status files. \
         Dispatch is what spends: `--dry-run` prints the card and creates nothing; a real dispatch needs `--yes`.\n\n\
         ## When a seat runs out\n\n\
         Capture the pane text before acting. Respect quota gauges, breaker cooldowns, and one half-open trial. \
         Use `{command} notify` as the only notification sender; never call a push endpoint directly. \
         Respect 30-minute deduplication, six alerts per project per hour, and twenty fleet alerts per hour.\n\n",
        context.review_policy.trim(), context.lend_interactive_to_workers,
        context.max_concurrent, context.dispatch_min_quota_percent,
    ));
    let shipped = include_str!("../../../prompts/role-nexus.md");
    // Drop the file's leading title/commentary section (above the `---`
    // divider); only the role body below it belongs in a rendered seed.
    // `context.role_prompt`, when given, is expected to already be stripped.
    let prompt = context.role_prompt.unwrap_or_else(|| {
        shipped
            .split_once("\n---\n")
            .map_or(shipped, |(_, body)| body)
            .trim()
    });
    if !prompt.is_empty() {
        text.push_str(prompt);
        text.push('\n');
    }
    text
}

/// Content digest of the rendered seed body, used to decide whether an
/// on-disk seed is stale against the current config/handoff/roles.
pub fn seed_digest(context: &SeedContext<'_>) -> String {
    content_digest(seed_body(context, &multiverse_invocation()).as_bytes())
}

/// Wrap a rendered body (always starting with `# Nexus — …\n`) in the
/// seed-digest marker that `seed_is_stale`-style checks read back. Shared by
/// every renderer so the marker convention cannot drift between them.
fn finalize_seed(body: String) -> String {
    let (title, rest) = body.split_once('\n').expect("seed has a title");
    format!(
        "{title}\n{MARKER}{} -->\n{rest}",
        content_digest(body.as_bytes())
    )
}

/// Render the full project Nexus seed, ready to write to disk: the body
/// from `seed_body` wrapped with `finalize_seed`'s digest marker.
pub fn render_seed(context: &SeedContext<'_>) -> String {
    finalize_seed(seed_body(context, &multiverse_invocation()))
}

fn marker_digest(line: &str) -> Option<&str> {
    let digest = line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix(MARKER)?
        .strip_suffix(" -->")?;
    (digest.len() == 16 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digest)
}

fn stored_digest(text: &str) -> Option<&str> {
    text.lines().find_map(marker_digest)
}

fn without_marker(text: &str) -> String {
    let mut removed = false;
    text.split_inclusive('\n')
        .filter(|line| {
            if !removed && marker_digest(line).is_some() {
                removed = true;
                false
            } else {
                true
            }
        })
        .collect()
}

/// None is never started. Existing unreadable files are errors, never None.
fn digest_is_stale(path: &Path, digest: &str) -> io::Result<Option<bool>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(stored_digest(&text) != Some(digest))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether the seed at `path` needs rewriting: `None` if it doesn't exist
/// yet, `Some(true)`/`Some(false)` if its stored digest disagrees/agrees
/// with `context`'s current digest.
pub fn seed_is_stale(path: &Path, context: &SeedContext<'_>) -> io::Result<Option<bool>> {
    digest_is_stale(path, &seed_digest(context))
}

/// Preserve human edits based on the seed's own digest, not today's config.
/// A create-new backup avoids clobbering an earlier backup in the same second.
pub fn backup_if_edited(path: &Path) -> io::Result<Option<PathBuf>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if stored_digest(&text) == Some(content_digest(without_marker(&text).as_bytes()).as_str()) {
        return Ok(None);
    }
    let stamp = mv_core::time::now_utc().unix_timestamp();
    for suffix in 0u64.. {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".bak-{stamp}"));
        if suffix > 0 {
            name.push(format!("-{suffix}"));
        }
        let backup = PathBuf::from(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&backup) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                return Ok(Some(backup));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("backup suffix space exhausted")
}

/// Shared by every seed writer: back up a hand-edited file, create the
/// parent dir at 0700, then write the rendered text at 0600. Also migrates
/// a legacy sibling onto `path` ([`migrate_legacy_sibling`]), so every
/// writer gets that behavior for free. Returns whichever backup path (new
/// file's hand edit, or migrated legacy's) the command layer should report
/// to the operator; a write that triggers both keeps the new file's.
fn write_rendered_seed(path: &Path, rendered: &str) -> io::Result<Option<PathBuf>> {
    let backup = backup_if_edited(path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(rendered.as_bytes())?;
    file.sync_all()?;
    let legacy_backup = migrate_legacy_sibling(path)?;
    Ok(backup.or(legacy_backup))
}

/// Render and write a project's seed to `path`, preserving any hand edits
/// and migrating a legacy-named sibling (see `write_rendered_seed`).
pub fn write_seed(path: &Path, context: &SeedContext<'_>) -> io::Result<Option<PathBuf>> {
    write_rendered_seed(path, &render_seed(context))
}

/// First-turn prompt delivered into a freshly started or reseeded project
/// Nexus: points it at its seed and, on `reseed`, tells it to continue
/// straight into its standing duties rather than wait to be asked.
pub fn seed_prompt(project: &str, path: &Path, reseed: bool) -> String {
    // `path` is always `seed_path(repository)` (`repository/.multiverse/<SEED_FILENAME>`),
    // so its grandparent recovers the repository root without widening this
    // function's signature past what every existing caller already passes.
    let passdown = path
        .parent()
        .and_then(Path::parent)
        .and_then(resolve_passdown);
    let mut prompt = match &passdown {
        Some(path_to_passdown) => format!(
            "Read `{}` now and follow it. You are the Nexus for {project}. A pending PASSDOWN \
             is at `{}`. Execute its BRING-UP section before dispatching anything. Report, in \
             two lines, which repository you own and which roles you may route.",
            path.display(),
            path_to_passdown.display()
        ),
        None => format!(
            "Read `{}` now and follow it. You are the Nexus for {project}. Read the project's \
             handoff card and resume its history. Report, in two lines, which repository you \
             own and which roles you may route.",
            path.display()
        ),
    };
    if reseed {
        prompt.push_str(" Then continue directly into one pass of your Every cycle duties. This is a one-time handoff, not a recurring job. Do not assume the human is present.");
    }
    prompt
}

/// The plain install's single home Nexus — the user's cross-project chat,
/// not a project Nexus. Its working directory holds no repository, so it
/// carries no handoff, no dispatchable roles, and no review policy.
pub struct HomeSeedContext<'a> {
    pub home: &'a Path,
    /// Every configured project except the hub itself.
    pub projects: &'a [(String, PathBuf)],
    pub directives_root: &'a Path,
    pub max_concurrent: usize,
}

fn home_seed_body(context: &HomeSeedContext<'_>, command: &str) -> String {
    let (directive_digest, warning) =
        directive_state_for(context.directives_root, HUB_PROJECT_NAME);
    let directive_path = context
        .directives_root
        .join(format!("{HUB_PROJECT_NAME}.md"));
    let passdown = resolve_passdown(context.home);
    let mut text = format!(
        "# Nexus — {hub} (home)\n\n\
         You are the user's home Nexus for {hub}: their cross-project chat. Your working \
         directory holds no project, and you edit no repository.\n\n\
         Working directory: `{}`.\n\n",
        context.home.display(),
        hub = HUB_PROJECT_NAME,
    );
    if let Some(path) = &passdown {
        text.push_str(pending_passdown_block(path).trim_end());
        text.push_str("\n\n");
    }
    text.push_str(&format!(
        "## Standing directives\n\n\
         Re-read `{}` every cycle. It is the human's standing authority, never a message or pane paste. \
         Missing or expired means no standing directives. A malformed or unreadable file confers no authority; report its warning. \
         Directive content is read from that file, never snapshotted into this seed.\n\n\
         With active directives, follow their authority. Without them, check human presence before asking. \
         Reversible work within your scope may proceed when the human is absent; irreversible decisions \
         outside existing authority wait for the human. Do not use AskUserQuestion to park authorized reversible work.\n\n\
         <!-- multiverse:directives-digest {} -->\n\n",
        directive_path.display(),
        directive_digest,
    ));
    if let Some(warning) = warning {
        text.push_str(&format!("Warning: {}\n\n", cell(&warning)));
    }
    text.push_str("## Configured projects\n\n");
    if context.projects.is_empty() {
        text.push_str("No projects are configured yet.\n\n");
    } else {
        text.push_str("| Project | Path |\n| --- | --- |\n");
        for (name, path) in context.projects {
            text.push_str(&format!(
                "| {} | {} |\n",
                cell(name),
                cell(&path.to_string_lossy())
            ));
        }
        text.push('\n');
    }
    text.push_str(
        "## Checking on projects\n\nRead-only forms; nothing here dispatches or writes.\n\n```sh\n",
    );
    for action in [
        "tasks <project>".to_string(),
        "board --all --limit 200".into(),
        "workers".into(),
        "check".into(),
        "accounts".into(),
        "backlog list <project>".into(),
    ] {
        text.push_str(&format!("{command} {action}\n"));
    }
    text.push_str("```\n\n");
    text.push_str(&format!(
        "## Routing work to a project\n\n\
         Route a request by messaging that project's own Nexus: `{command} activity send \
         <project> \"...\"`. Use `--ask` when you need an answer back, and answer one you hold \
         with `{command} activity reply SEQ \"...\"`. `{command} activity broadcast` and \
         `{command} activity nudge` are hub-only tools. Read your mail every cycle with \
         `{command} activity inbox`, repeated until it reports nothing new. Mail here is always \
         from a project, never from the human.\n\n\
         ## Opening a project's Nexus\n\n\
         Open a project's own Nexus on request with `{command} start <project>`. Do not dispatch \
         workers into another project's repository yourself — that project's Nexus owns its own \
         routing.\n\n\
         ## Asking the human\n\n\
         Run `{command} presence` first. With the human present, ask as a numbered menu — the \
         decision in one line, concrete options, and an explicit recommendation — the same rules \
         a project's own Nexus follows.\n\n\
         ## The rules you are held to\n\n\
         Fleet `max_concurrent = {}`.\n\n",
        context.max_concurrent,
    ));
    let shipped = include_str!("../../../prompts/role-nexus-home.md");
    let prompt = shipped
        .split_once("\n---\n")
        .map_or(shipped, |(_, body)| body)
        .trim();
    if !prompt.is_empty() {
        text.push_str(prompt);
        text.push('\n');
    }
    text
}

/// Content digest of the rendered home seed body, [`seed_digest`]'s twin
/// for the cross-project home Nexus.
pub fn home_seed_digest(context: &HomeSeedContext<'_>) -> String {
    content_digest(home_seed_body(context, &multiverse_invocation()).as_bytes())
}

/// Render the full home Nexus seed, ready to write to disk — [`render_seed`]'s
/// twin for the cross-project home Nexus.
pub fn render_home_seed(context: &HomeSeedContext<'_>) -> String {
    finalize_seed(home_seed_body(context, &multiverse_invocation()))
}

/// Twin of `seed_is_stale`, sharing its marker convention.
pub fn home_seed_is_stale(path: &Path, context: &HomeSeedContext<'_>) -> io::Result<Option<bool>> {
    digest_is_stale(path, &home_seed_digest(context))
}

/// Render and write the home seed to `path` — [`write_seed`]'s twin for the
/// cross-project home Nexus.
pub fn write_home_seed(path: &Path, context: &HomeSeedContext<'_>) -> io::Result<Option<PathBuf>> {
    write_rendered_seed(path, &render_home_seed(context))
}

/// First-turn prompt delivered into a freshly started or reseeded home
/// Nexus — [`seed_prompt`]'s twin for the cross-project home Nexus.
pub fn home_seed_prompt(path: &Path, reseed: bool) -> String {
    // `path` is always `seed_path(home)` (`home/.multiverse/<SEED_FILENAME>`),
    // so its grandparent recovers the home root the same way `seed_prompt`
    // recovers a project's repository root.
    let passdown = path
        .parent()
        .and_then(Path::parent)
        .and_then(resolve_passdown);
    let mut prompt = match &passdown {
        Some(path_to_passdown) => format!(
            "Read `{}` now and follow it. You are the home Nexus for {HUB_PROJECT_NAME}: the \
             user's cross-project chat, not a project Nexus. A pending PASSDOWN is at `{}`. \
             Execute its BRING-UP section before dispatching anything. Report, in two lines, \
             which projects you can see and how you route to each.",
            path.display(),
            path_to_passdown.display()
        ),
        None => format!(
            "Read `{}` now and follow it. You are the home Nexus for {HUB_PROJECT_NAME}: the \
             user's cross-project chat, not a project Nexus. Report, in two lines, which \
             projects you can see and how you route to each.",
            path.display()
        ),
    };
    if reseed {
        prompt.push_str(" Then continue directly into one pass of your Every cycle duties. This is a one-time handoff, not a recurring job. Do not assume the human is present.");
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directives::tests::TempDir;
    use time::macros::datetime;

    fn context(root: &Path) -> SeedContext<'_> {
        SeedContext {
            project: "demo",
            repository: Path::new("/projects/demo"),
            handoff: Handoff::File(PathBuf::from(".multiverse/handoff.md")),
            directives_root: root,
            roles: &[],
            review_policy: "Require risk-based review.",
            max_concurrent: 4,
            dispatch_min_quota_percent: 20,
            lend_interactive_to_workers: false,
            role_prompt: Some("Nexus duties."),
        }
    }

    #[test]
    fn render_keeps_history_pointer_new_names_and_unavailable_roles() {
        let dir = TempDir::new();
        let mut context = context(&dir.0);
        let routes = [
            DispatchableRole {
                role: SeedRole::Builder,
                account: Ok("worker".into()),
            },
            DispatchableRole {
                role: SeedRole::Scout,
                account: Err("quota exhausted".into()),
            },
        ];
        context.roles = &routes;
        let seed = seed_body(&context, "plugin/bin/multiverse");
        assert!(seed.contains(".multiverse/handoff.md"));
        assert!(seed.contains(".multiverse/routing.log"));
        assert!(seed.contains("| Builder | worker |"));
        assert!(seed.contains("Scout: configured but unavailable — quota exhausted"));
        assert!(seed
            .contains("plugin/bin/multiverse dispatch '/projects/demo' '<goal>' --role builder"));
        assert!(!seed.contains("--role fixer"));
        assert!(!seed.contains("--role explorer"));
        assert!(!seed.contains("multiverse tasks 'demo'\nmultiverse"));
    }

    #[test]
    fn digest_covers_routes_prompt_handoff_directives_and_selected_config() {
        let a = TempDir::new();
        let b = TempDir::new();
        let mut context = context(&a.0);
        let baseline = seed_digest(&context);
        context.role_prompt = Some("changed prompt");
        assert_ne!(seed_digest(&context), baseline);
        context.role_prompt = Some("Nexus duties.");
        context.handoff = Handoff::File(PathBuf::from(".multiverse/cards/resume.md"));
        assert_ne!(seed_digest(&context), baseline);
        context.handoff = Handoff::File(PathBuf::from(".multiverse/handoff.md"));
        let routes = [DispatchableRole {
            role: SeedRole::Critic,
            account: Ok("review-seat".into()),
        }];
        context.roles = &routes;
        assert_ne!(seed_digest(&context), baseline);
        context.roles = &[];
        fs::write(a.0.join("demo.md"), "private authority A").unwrap();
        let changed = seed_digest(&context);
        assert_ne!(changed, baseline);
        assert!(!render_seed(&context).contains("private authority A"));
        fs::write(b.0.join("demo.md"), "private authority B").unwrap();
        context.directives_root = &b.0;
        assert_ne!(seed_digest(&context), changed);
    }

    #[test]
    fn malformed_directives_warn_without_aborting_another_project() {
        let dir = TempDir::new();
        fs::write(dir.0.join("demo.md"), "until:\n\ninvalid").unwrap();
        let seed = render_seed(&context(&dir.0));
        assert!(seed.contains("Warning:"));
        assert!(seed.contains("multiverse:directives-digest none"));
        let mut other = context(&dir.0);
        other.project = "other";
        assert!(!render_seed(&other).contains("Warning:"));
    }

    #[test]
    fn config_staleness_does_not_back_up_but_human_edits_do() {
        let dir = TempDir::new();
        let path = dir.0.join(SEED_FILENAME);
        let mut context = context(&dir.0);
        assert_eq!(seed_is_stale(&path, &context).unwrap(), None);
        assert!(write_seed(&path, &context).unwrap().is_none());
        let original = fs::read_to_string(&path).unwrap();
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(false));
        assert_eq!(
            stored_digest(&original),
            Some(content_digest(without_marker(&original).as_bytes()).as_str())
        );
        context.max_concurrent += 1;
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(true));
        assert!(backup_if_edited(&path).unwrap().is_none());
        fs::write(&path, format!("{original}Human edit.\n")).unwrap();
        let first = backup_if_edited(&path).unwrap().unwrap();
        let second = write_seed(&path, &context).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            fs::read_to_string(first).unwrap(),
            format!("{original}Human edit.\n")
        );
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(false));
        fs::write(&path, "legacy seed without a marker").unwrap();
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(true));
        assert!(backup_if_edited(&path).unwrap().is_some());
    }

    #[test]
    fn write_seed_writes_the_new_filename_for_a_fresh_project() {
        let dir = TempDir::new();
        let context = context(&dir.0);
        let path = seed_path(&dir.0);
        assert!(write_seed(&path, &context).unwrap().is_none());
        assert!(path.is_file());
        assert!(!dir
            .0
            .join(".multiverse")
            .join(LEGACY_SEED_FILENAME)
            .is_file());
    }

    #[test]
    fn write_seed_migrates_an_existing_legacy_seed_onto_the_new_filename() {
        let dir = TempDir::new();
        let context = context(&dir.0);
        let state_dir = dir.0.join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        let legacy = state_dir.join(LEGACY_SEED_FILENAME);
        fs::write(&legacy, "hand-edited legacy seed, no digest marker\n").unwrap();
        let path = seed_path(&dir.0);
        let backup = write_seed(&path, &context)
            .unwrap()
            .expect("the legacy file's hand edit is preserved");
        assert!(path.is_file());
        assert!(!legacy.is_file());
        assert_eq!(
            fs::read_to_string(backup).unwrap(),
            "hand-edited legacy seed, no digest marker\n"
        );
    }

    #[test]
    fn existing_seed_path_prefers_the_new_name_and_falls_back_to_legacy() {
        let dir = TempDir::new();
        assert_eq!(existing_seed_path(&dir.0), None);
        let state_dir = dir.0.join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        let legacy = state_dir.join(LEGACY_SEED_FILENAME);
        fs::write(&legacy, "legacy\n").unwrap();
        assert_eq!(existing_seed_path(&dir.0), Some(legacy.clone()));
        let current = seed_path(&dir.0);
        fs::write(&current, "current\n").unwrap();
        assert_eq!(existing_seed_path(&dir.0), Some(current.clone()));
        fs::remove_file(&legacy).unwrap();
        assert_eq!(existing_seed_path(&dir.0), Some(current));
    }

    #[test]
    fn unreadable_existing_seed_is_not_absent_and_reseed_resumes() {
        let dir = TempDir::new();
        let path = dir.0.join("directory");
        fs::create_dir(&path).unwrap();
        assert!(seed_is_stale(&path, &context(&dir.0)).is_err());
        assert!(backup_if_edited(&path).is_err());
        assert!(!seed_prompt("demo", Path::new(SEED_FILENAME), false).contains("one-time handoff"));
        assert!(seed_prompt("demo", Path::new(SEED_FILENAME), true).contains("continue directly"));
        assert_eq!(invocation(None), "multiverse");
        assert_eq!(invocation(Some("  \n")), "multiverse");
        assert_eq!(
            invocation(Some(" plugin/bin/multiverse ")),
            "plugin/bin/multiverse"
        );
    }

    #[test]
    fn shipped_prompt_remains_presence_gated() {
        let dir = TempDir::new();
        let mut context = context(&dir.0);
        context.role_prompt = None;
        let seed = render_seed(&context);
        assert!(seed.contains("Reversible work within your scope may proceed"));
        assert!(seed.contains("## Handling Council output"));
        assert!(seed.contains("human's standing authority"));
    }

    // --- resolve_handoff: fixtures shaped like the real 2026-11-01 cutover
    // cards (card `20261102-b-0d8483`) --------------------------------------

    #[test]
    fn resolve_handoff_picks_the_backlog_card_whose_title_mentions_handoff() {
        let dir = TempDir::new();
        let card = backlog::add(
            &dir.0,
            "HANDOFF 2026-11-01 22:3xZ (pre role-rename): state for the next Nexus",
            "demo",
            "",
            None,
            None,
            None,
            "P1",
            Some(datetime!(2026-11-01 22:33:32 UTC)),
        )
        .unwrap();
        assert_eq!(
            resolve_handoff(&dir.0),
            Handoff::Card {
                item: card.item,
                title: card.title,
            }
        );
    }

    #[test]
    fn resolve_handoff_matches_on_why_when_the_title_does_not_mention_it() {
        let dir = TempDir::new();
        let card = backlog::add(
            &dir.0,
            "SACT state for the multiverse upgrade",
            "demo",
            "HANDOFF for the multiverse role rename, written by the orchestrator.",
            None,
            None,
            None,
            "P1",
            None,
        )
        .unwrap();
        assert_eq!(
            resolve_handoff(&dir.0),
            Handoff::Card {
                item: card.item,
                title: card.title,
            }
        );
    }

    #[test]
    fn resolve_handoff_ignores_a_done_card_even_if_it_mentions_handoff() {
        let dir = TempDir::new();
        let card = backlog::add(
            &dir.0,
            "HANDOFF 2026-10-09 for Mike's return",
            "demo",
            "",
            None,
            None,
            None,
            "P1",
            None,
        )
        .unwrap();
        backlog::done(&dir.0, &card.item, "superseded", None).unwrap();
        assert_eq!(resolve_handoff(&dir.0), Handoff::None);
    }

    #[test]
    fn resolve_handoff_picks_the_newest_qualifying_card_not_the_first_written() {
        let dir = TempDir::new();
        backlog::add(
            &dir.0,
            "HANDOFF 2026-09-23 for the next session",
            "demo",
            "",
            None,
            None,
            None,
            "P1",
            Some(datetime!(2026-09-23 19:04:10 UTC)),
        )
        .unwrap();
        let latest = backlog::add(
            &dir.0,
            "HANDOFF 2026-11-01 (role-rename upgrade): resume from tail of notes",
            "demo",
            "",
            None,
            None,
            None,
            "P1",
            Some(datetime!(2026-11-01 22:39:12 UTC)),
        )
        .unwrap();
        assert_eq!(
            resolve_handoff(&dir.0),
            Handoff::Card {
                item: latest.item,
                title: latest.title,
            }
        );
    }

    #[test]
    fn resolve_handoff_prefers_a_leading_handoff_title_over_a_newer_incidental_mention() {
        // Reproduces card `20261102-b-b766c1`: multiverse's own standing
        // handoff card is older than an unrelated cleanup card that merely
        // lists a file named "handoff" among others it wants dropped.
        let dir = TempDir::new();
        let standing = backlog::add(
            &dir.0,
            "HANDOFF for the next multiverse session",
            "demo",
            "",
            None,
            None,
            None,
            "P1",
            Some(datetime!(2026-09-23 00:00:00 UTC)),
        )
        .unwrap();
        backlog::add(
            &dir.0,
            "Release 4/7: public-tree hygiene: drop internal files \
             (SESSION-LOG, evidence/, config.local.*, handoff, OPEN-QUESTIONS, reference/)",
            "demo",
            "",
            None,
            None,
            None,
            "P2",
            Some(datetime!(2026-11-01 00:00:00 UTC)),
        )
        .unwrap();
        assert_eq!(
            resolve_handoff(&dir.0),
            Handoff::Card {
                item: standing.item,
                title: standing.title,
            }
        );
    }

    #[test]
    fn resolve_handoff_falls_back_to_the_conventional_file_when_no_card_qualifies() {
        let dir = TempDir::new();
        backlog::add(
            &dir.0,
            "Offboarding block: also match email",
            "demo",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        fs::create_dir_all(dir.0.join(".multiverse")).unwrap();
        let file = dir.0.join(HANDOFF_FILE);
        fs::write(&file, "landing notes").unwrap();
        assert_eq!(resolve_handoff(&dir.0), Handoff::File(file));
    }

    #[test]
    fn resolve_handoff_is_none_when_neither_a_card_nor_a_file_exists() {
        let dir = TempDir::new();
        backlog::add(
            &dir.0,
            "lab/wsl-rotate.sh never reclaims a stale lock",
            "demo",
            "",
            None,
            None,
            None,
            "P3",
            None,
        )
        .unwrap();
        assert_eq!(resolve_handoff(&dir.0), Handoff::None);
    }

    #[test]
    fn handoff_instruction_renders_distinctly_for_each_variant() {
        let card = Handoff::Card {
            item: "20261101-b-5fd767".to_string(),
            title: "Handoff 2026-11-01".to_string(),
        };
        assert!(handoff_instruction(&card).contains("backlog card `20261101-b-5fd767`"));
        let file = Handoff::File(PathBuf::from(".multiverse/handoff.md"));
        assert!(handoff_instruction(&file).contains(".multiverse/handoff.md"));
        assert!(handoff_instruction(&Handoff::None).contains("recorded no handoff"));
    }

    // --- HomeSeedContext --------------------------------------------------

    fn home_context<'a>(root: &'a Path, projects: &'a [(String, PathBuf)]) -> HomeSeedContext<'a> {
        HomeSeedContext {
            home: Path::new("/state/home"),
            projects,
            directives_root: root,
            max_concurrent: 4,
        }
    }

    #[test]
    fn home_seed_names_routing_and_every_project_without_isolation_text() {
        let dir = TempDir::new();
        let projects = [
            ("alpha".to_string(), PathBuf::from("/projects/alpha")),
            ("beta".to_string(), PathBuf::from("/projects/beta")),
        ];
        let seed = render_home_seed(&home_context(&dir.0, &projects));
        assert!(seed.contains("activity send"));
        assert!(seed.contains("start <"));
        assert!(seed.contains("alpha"));
        assert!(seed.contains("/projects/alpha"));
        assert!(seed.contains("beta"));
        assert!(seed.contains("/projects/beta"));
        assert!(!seed.contains("Never read, list, or search another project's repository"));
        assert!(!seed.contains("assigned to repository"));
    }

    #[test]
    fn home_seed_digest_changes_with_the_project_list() {
        let dir = TempDir::new();
        let none: [(String, PathBuf); 0] = [];
        let baseline = home_seed_digest(&home_context(&dir.0, &none));
        let one = [("alpha".to_string(), PathBuf::from("/projects/alpha"))];
        assert_ne!(home_seed_digest(&home_context(&dir.0, &one)), baseline);
        let two = [
            ("alpha".to_string(), PathBuf::from("/projects/alpha")),
            ("beta".to_string(), PathBuf::from("/projects/beta")),
        ];
        assert_ne!(home_seed_digest(&home_context(&dir.0, &two)), baseline);
        assert_ne!(
            home_seed_digest(&home_context(&dir.0, &two)),
            home_seed_digest(&home_context(&dir.0, &one))
        );
    }

    #[test]
    fn write_home_seed_matches_write_seed_mode_and_backup_behavior() {
        let dir = TempDir::new();
        let projects = [("alpha".to_string(), PathBuf::from("/projects/alpha"))];
        let context = home_context(&dir.0, &projects);
        let parent = dir.0.join("nested").join("home");
        let path = parent.join(SEED_FILENAME);
        assert_eq!(home_seed_is_stale(&path, &context).unwrap(), None);
        assert!(write_home_seed(&path, &context).unwrap().is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        let original = fs::read_to_string(&path).unwrap();
        assert_eq!(home_seed_is_stale(&path, &context).unwrap(), Some(false));

        fs::write(&path, format!("{original}Human edit.\n")).unwrap();
        let backup = write_home_seed(&path, &context).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(backup).unwrap(),
            format!("{original}Human edit.\n")
        );
        assert_eq!(home_seed_is_stale(&path, &context).unwrap(), Some(false));
    }

    // --- resolve_passdown and the pending-PASSDOWN resume hook (card
    // `20261116-b-f0612d`, Lane A) -----------------------------------------

    fn write_sidecar(repository: &Path, text: &str) {
        let dir = repository.join(".multiverse");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("passdown.state"), text).unwrap();
    }

    fn write_passdown(repository: &Path) {
        let dir = repository.join(".multiverse");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("PASSDOWN.md"), "## STATE\n\nnothing in flight\n").unwrap();
    }

    #[test]
    fn resolve_passdown_is_none_when_passdown_exists_but_the_sidecar_is_missing() {
        // This project's own `.multiverse/PASSDOWN.md` is exactly this
        // shape today: a hand-written archive with no sidecar. Keying on
        // existence instead of the sidecar would make every `start` of
        // this project re-run BRING-UP.
        let dir = TempDir::new();
        write_passdown(&dir.0);
        assert_eq!(resolve_passdown(&dir.0), None);
    }

    #[test]
    fn resolve_passdown_is_some_when_pending_or_wrapped() {
        let dir = TempDir::new();
        write_passdown(&dir.0);
        write_sidecar(&dir.0, "pending");
        assert_eq!(
            resolve_passdown(&dir.0),
            Some(dir.0.join(".multiverse").join("PASSDOWN.md"))
        );
        write_sidecar(&dir.0, "wrapped");
        assert_eq!(
            resolve_passdown(&dir.0),
            Some(dir.0.join(".multiverse").join("PASSDOWN.md"))
        );
    }

    #[test]
    fn resolve_passdown_fails_closed_on_an_unreadable_or_unrecognized_sidecar() {
        let dir = TempDir::new();
        write_passdown(&dir.0);
        // A directory where the sidecar file should be: `read_to_string`
        // errors with something other than `NotFound`.
        fs::create_dir_all(dir.0.join(".multiverse").join("passdown.state")).unwrap();
        assert!(resolve_passdown(&dir.0).is_some());
        fs::remove_dir(dir.0.join(".multiverse").join("passdown.state")).unwrap();
        write_sidecar(&dir.0, "garbage\n");
        assert!(resolve_passdown(&dir.0).is_some());
    }

    #[test]
    fn resolve_passdown_is_none_once_the_sidecar_reads_up() {
        let dir = TempDir::new();
        write_passdown(&dir.0);
        write_sidecar(&dir.0, "up");
        assert_eq!(resolve_passdown(&dir.0), None);
    }

    #[test]
    fn resolve_passdown_is_none_when_pending_but_passdown_itself_is_missing() {
        let dir = TempDir::new();
        write_sidecar(&dir.0, "pending");
        assert_eq!(resolve_passdown(&dir.0), None);
    }

    #[test]
    fn render_seed_contains_the_absolute_passdown_path_iff_pending() {
        let directives = TempDir::new();
        let repository = TempDir::new();
        let mut context = context(&directives.0);
        context.repository = &repository.0;
        let passdown = repository.0.join(".multiverse").join("PASSDOWN.md");

        let seed = render_seed(&context);
        assert!(!seed.contains("A pending PASSDOWN is at"));
        assert!(!seed.contains(&passdown.to_string_lossy().to_string()));

        write_passdown(&repository.0);
        write_sidecar(&repository.0, "pending");
        let seed = render_seed(&context);
        assert!(seed.contains("A pending PASSDOWN is at"));
        assert!(seed.contains(&passdown.to_string_lossy().to_string()));
        assert!(seed.contains("Execute its BRING-UP section"));
    }

    #[test]
    fn seed_is_stale_flips_when_a_sidecar_appears() {
        let directives = TempDir::new();
        let repository = TempDir::new();
        let mut context = context(&directives.0);
        context.repository = &repository.0;
        let path = repository.0.join(SEED_FILENAME);

        assert!(write_seed(&path, &context).unwrap().is_none());
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(false));

        write_passdown(&repository.0);
        write_sidecar(&repository.0, "pending");
        assert_eq!(seed_is_stale(&path, &context).unwrap(), Some(true));
    }

    #[test]
    fn seed_prompt_names_passdown_only_when_pending() {
        let repository = TempDir::new();
        let path = seed_path(&repository.0);
        let passdown = repository.0.join(".multiverse").join("PASSDOWN.md");

        let prompt = seed_prompt("demo", &path, false);
        assert!(!prompt.contains("PASSDOWN"));

        write_passdown(&repository.0);
        write_sidecar(&repository.0, "wrapped");
        let prompt = seed_prompt("demo", &path, false);
        assert!(prompt.contains(&passdown.to_string_lossy().to_string()));
        assert!(prompt.contains("BRING-UP"));
    }

    #[test]
    fn home_seed_names_pending_passdown_only_when_pending() {
        let directives = TempDir::new();
        let home = TempDir::new();
        let none: [(String, PathBuf); 0] = [];
        let mut context = home_context(&directives.0, &none);
        context.home = &home.0;
        let path = seed_path(&home.0);

        // The shipped `role-nexus-home.md` prompt's own "Fleet down / Fleet
        // up" section mentions "pending PASSDOWN" in static prose and is
        // always appended here (unlike `seed_body`, `home_seed_body` has no
        // `role_prompt` override to suppress it) — so this checks for the
        // dynamic block's exact lead sentence, never present in that static
        // text, rather than the bare phrase.
        let seed = render_home_seed(&context);
        assert!(!seed.contains("A pending PASSDOWN is at"));
        let prompt = home_seed_prompt(&path, false);
        assert!(!prompt.contains("PASSDOWN"));

        write_passdown(&home.0);
        write_sidecar(&home.0, "pending");
        let seed = render_home_seed(&context);
        assert!(seed.contains("A pending PASSDOWN is at"));
        let prompt = home_seed_prompt(&path, false);
        assert!(prompt.contains("PASSDOWN"));
        assert!(prompt.contains("BRING-UP"));
    }
}
