//! The boundary between a planned dispatch and the Herdr calls that
//! carry it out.
//!
//! Public result reads deliberately prefer project-root status. Lifecycle
//! reconciliation has a different (lane-first) rule and must not reuse this
//! lookup. [`SecureFiles`] is the filesystem capability supplied by the core
//! adapter; lexical checks alone do not prevent symlink-swap disclosure.

use std::io::{self, BufRead, Read};
use std::path::{Component, Path, PathBuf};

pub use mv_state::tasks::TaskStatus;

/// Capability rooted at one already-opened project directory. Implementors
/// must open EVERY path component relative to directory descriptors with
/// O_NOFOLLOW, verify regular files by fstat, and reject link counts other
/// than one. Return NotFound only for genuine absence, never for an unsafe
/// path or permission failure. The returned reader must own the opened file,
/// so later renames/symlink swaps cannot change what is read. No path-based
/// check-then-open fallback is safe. This narrow seam is needed while the
/// mv-core secure-file implementation has not landed; there is intentionally
/// no insecure default adapter.
pub trait SecureFiles {
    fn open_regular(&self, relative: &Path) -> io::Result<Box<dyn BufRead>>;
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Refusal(pub String);

#[derive(Debug, PartialEq, Eq)]
pub struct TaskResult {
    pub status: TaskStatus,
    pub text: String,
    /// Project-relative directory actually used, useful for diagnostics.
    pub source: PathBuf,
}

fn read_text(files: &impl SecureFiles, path: &Path) -> io::Result<String> {
    let mut text = String::new();
    files.open_regular(path)?.read_to_string(&mut text)?;
    Ok(text)
}

/// `_lane_task_id` (`tasks.py:1105-1129`): the task id that actually names
/// `task_id`'s writer-lane worktree. Ordinarily that is `task_id` itself;
/// for a task dispatched `multiverse dispatch --into` another task's lane,
/// no worktree is ever created for `task_id`'s own name, and the owner's id
/// is recorded instead, on `task_id`'s own `event=dispatch` routing-log line,
/// as `branch=multiverse/<owner-task-id>` (`accounts.Selection.log_line`).
/// Scans every record rather than stopping at the first match, so a
/// fix/retry round's later dispatch line — same `task_id`, a fresh `branch=`
/// — wins, matching `tasks.read_log`'s own last-dispatch-wins field merge.
/// Falls back to `task_id` when the log is unreadable, has no dispatch
/// record for `task_id`, `branch=` is empty or lacks the branch prefix (the
/// ordinary, non-`--into` case), or names something that does not pass
/// `task_id` validation itself — `gateway.py`'s own `_lane_paths` treats any
/// of these as "nothing to fall back to", not as the fallback itself.
fn lane_task_id(files: &impl SecureFiles, task_id: &str) -> String {
    let Ok(log) = read_text(files, Path::new(".multiverse/routing.log")) else {
        return task_id.to_string();
    };
    let mut branch = String::new();
    for line in log.lines() {
        let Ok(record) = mv_state::log::parse(line) else {
            continue;
        };
        if record.event() != "dispatch" {
            continue;
        }
        if record.fields.get("task").map(String::as_str) != Some(task_id) {
            continue;
        }
        branch = record.fields.get("branch").cloned().unwrap_or_default();
    }
    match branch.strip_prefix("multiverse/") {
        Some(owner) if mv_state::paths::is_valid_task_id(owner) => owner.to_string(),
        _ => task_id.to_string(),
    }
}

/// Root status is authoritative even when unrecognized, nonterminal, unsafe,
/// or accompanied by an absent/empty result. Only absent root status permits
/// fallback to the task's own writer lane. Redaction is required on delivery,
/// never a rewrite of the agent-owned result file.
pub fn read_result(
    files: &impl SecureFiles,
    task_id: &str,
    redact: &impl Fn(&str) -> String,
) -> Result<TaskResult, Refusal> {
    if !mv_state::paths::is_valid_task_id(task_id) {
        return Err(Refusal("invalid task id".into()));
    }
    let root = Path::new(".multiverse/tasks").join(task_id);
    let (source, raw_status) = match read_text(files, &root.join("status")) {
        Ok(text) => (root, text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let lane = Path::new(".multiverse/worktrees")
                .join(lane_task_id(files, task_id))
                .join(".multiverse/tasks")
                .join(task_id);
            let status = read_text(files, &lane.join("status"))
                .map_err(|e| Refusal(format!("cannot read task status: {e}")))?;
            (lane, status)
        }
        Err(e) => return Err(Refusal(format!("cannot read task status: {e}"))),
    };
    let status = TaskStatus::parse(&raw_status)
        .ok_or_else(|| Refusal("task has no terminal status".into()))?;
    let result = read_text(files, &source.join("result.md"))
        .map_err(|e| Refusal(format!("cannot read task result: {e}")))?;
    if result.trim().is_empty() {
        return Err(Refusal("task result is empty".into()));
    }
    Ok(TaskResult {
        status,
        text: redact(&result),
        source,
    })
}

pub const INSPECT_BYTE_LIMIT: usize = 64 * 1024;
pub const TRUNCATION_NOTICE: &str = "\n[output truncated at 64 KiB]\n";

#[derive(Debug, Clone, Copy)]
pub struct LineRange {
    pub first: usize,
    pub last: usize,
}

/// A compiled regex adapter. Regex compilation/diagnostics are supplied by
/// the owning config/command adapter rather than adding a shared manifest
/// dependency from this bounded implementation lane.
pub trait Matcher {
    fn is_match(&self, line: &str) -> bool;
}

pub struct InspectRequest<'a> {
    /// Absolute, resolved project root and caller cwd. Relative file arguments
    /// are resolved against cwd, not against the project root.
    pub project_root: &'a Path,
    pub cwd: &'a Path,
    pub paths: &'a [PathBuf],
    pub lines: Option<LineRange>,
    pub matcher: Option<&'a dyn Matcher>,
    pub reason: &'a str,
}

#[derive(Debug, Clone)]
pub struct InspectAudit {
    pub project: String,
    pub paths: Vec<String>,
    pub bytes: usize,
    pub reason: String,
    pub outcome: String,
}

/// Append exactly one UTC-stamped event=inspect record to the project's
/// .multiverse/inspect.log. Best effort, no fsync, no inspected content.
/// The state adapter owns timestamp formatting and the append operation.
pub trait AuditSink {
    fn append(&mut self, record: &InspectAudit) -> io::Result<()>;
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn normalized(path: &Path) -> Result<PathBuf, Refusal> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => result.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                if !result.pop() {
                    return Err(Refusal("path escapes project root".into()));
                }
            }
            Component::Prefix(_) => return Err(Refusal("unsupported path prefix".into())),
        }
    }
    Ok(result)
}

fn relative_path(request: &InspectRequest<'_>, path: &Path) -> Result<PathBuf, Refusal> {
    if path.as_os_str().is_empty() || path.to_string_lossy().contains(['*', '?', '[', ']']) {
        return Err(Refusal(
            "inspect requires literal file paths; globs are not allowed".into(),
        ));
    }
    if !request.cwd.is_absolute() || !request.project_root.is_absolute() {
        return Err(Refusal(
            "inspect requires absolute project root and cwd".into(),
        ));
    }
    let root = normalized(request.project_root)?;
    let cwd = normalized(request.cwd)?;
    if !cwd.starts_with(&root) {
        return Err(Refusal("caller cwd is outside project root".into()));
    }
    let outside = || {
        Refusal(format!(
            "{}: resolves outside the project",
            sanitize(&path.to_string_lossy())
        ))
    };
    let absolute = normalized(&cwd.join(path)).map_err(|_| outside())?;
    let relative = absolute.strip_prefix(&root).map_err(|_| outside())?;
    if relative.as_os_str().is_empty() {
        return Err(Refusal(
            "inspect requires regular files, not directories".into(),
        ));
    }
    Ok(relative.to_owned())
}

/// A UTF-8 budget shared across all requested files, including headings,
/// prefixes and the reserved truncation notice.
struct OutputBudget {
    text: String,
    truncated: bool,
}

impl OutputBudget {
    fn append(&mut self, value: &str) {
        let available =
            (INSPECT_BYTE_LIMIT - TRUNCATION_NOTICE.len()).saturating_sub(self.text.len());
        let mut end = available.min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        self.truncated |= end < value.len();
    }

    fn finish(mut self) -> String {
        if self.truncated {
            self.text.push_str(TRUNCATION_NOTICE);
        }
        self.text
    }
}

fn inspect_inner(
    files: &impl SecureFiles,
    request: &InspectRequest<'_>,
) -> Result<String, Refusal> {
    if !(1..=2).contains(&request.paths.len()) {
        return Err(Refusal("inspect requires one or two paths".into()));
    }
    if request.lines.is_some() == request.matcher.is_some() {
        return Err(Refusal(
            "inspect requires exactly one of --lines or --match".into(),
        ));
    }
    if request.reason.trim().is_empty() {
        return Err(Refusal("inspect requires a nonblank --reason".into()));
    }
    if request
        .lines
        .is_some_and(|r| r.first == 0 || r.last < r.first)
    {
        return Err(Refusal("line range must be positive and increasing".into()));
    }
    // Validate and open BOTH before returning any contents. The filesystem
    // capability rejects directories, nonregular files, hard links/symlinks.
    let mut opened = Vec::new();
    for path in request.paths {
        let relative = relative_path(request, path)?;
        let reader = files.open_regular(&relative).map_err(|e| {
            Refusal(format!(
                "cannot inspect {}: {e}",
                sanitize(&path.to_string_lossy())
            ))
        })?;
        opened.push((relative, reader));
    }
    let mut output = OutputBudget {
        text: String::new(),
        truncated: false,
    };
    for (path, mut reader) in opened {
        output.append(&format!("== {} ==\n", path.to_string_lossy()));
        let mut number = 0;
        loop {
            number += 1;
            if request.lines.is_some_and(|r| number > r.last) {
                break;
            }
            // Match the complete line before applying the shared output cap;
            // a regex can match a suffix beyond the first 64 KiB.
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|e| Refusal(format!("cannot read inspected file: {e}")))?;
            if read == 0 {
                break;
            }
            let selected = request
                .lines
                .is_some_and(|r| number >= r.first && number <= r.last)
                || request
                    .matcher
                    .is_some_and(|matcher| matcher.is_match(&line));
            if selected {
                output.append(&line);
                if output.truncated {
                    return Ok(output.finish());
                }
            }
        }
    }
    Ok(output.finish())
}

/// Refusals and successes each attempt exactly one sanitized audit append.
/// Audit failures never mask the requested operation's outcome.
pub fn inspect(
    files: &impl SecureFiles,
    request: &InspectRequest<'_>,
    redact: &impl Fn(&str) -> String,
    audit: &mut impl AuditSink,
) -> Result<String, Refusal> {
    let result = inspect_inner(files, request);
    let record = InspectAudit {
        project: sanitize(&redact(&request.project_root.to_string_lossy())),
        paths: request
            .paths
            .iter()
            .map(|p| sanitize(&redact(&p.to_string_lossy())))
            .collect(),
        bytes: result.as_ref().map_or(0, String::len),
        reason: sanitize(&redact(request.reason)),
        outcome: match &result {
            Ok(_) => "ok".into(),
            // Refusal text is built entirely from already-sanitized path
            // components and static wording, never from `request.reason`;
            // redacting it a second time has false-positived on paths ending
            // in a credential-stem word followed by ": " (`passwd: resolves
            // outside the project` reads as an assignment to redact's
            // generic key=value scanner).
            Err(refusal) => format!("refused: {}", sanitize(&refusal.to_string())),
        },
    };
    if let Err(error) = audit.append(&record) {
        eprintln!("warning: could not write inspect audit log: {error}");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::io::Cursor;

    #[derive(Clone)]
    enum Entry {
        Text(String),
        Unsafe,
    }
    #[derive(Default)]
    struct Files {
        entries: BTreeMap<PathBuf, Entry>,
        opened: RefCell<Vec<PathBuf>>,
    }
    impl Files {
        fn text(&mut self, path: impl AsRef<Path>, text: &str) {
            self.entries
                .insert(path.as_ref().to_owned(), Entry::Text(text.into()));
        }
        fn unsafe_file(&mut self, path: impl AsRef<Path>) {
            self.entries.insert(path.as_ref().to_owned(), Entry::Unsafe);
        }
    }
    impl SecureFiles for Files {
        fn open_regular(&self, path: &Path) -> io::Result<Box<dyn BufRead>> {
            assert!(!path.is_absolute());
            assert!(!path.components().any(|c| c == Component::ParentDir));
            self.opened.borrow_mut().push(path.to_owned());
            match self.entries.get(path) {
                Some(Entry::Text(text)) => Ok(Box::new(Cursor::new(text.clone().into_bytes()))),
                Some(Entry::Unsafe) => Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe file",
                )),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }
    }
    fn redact(text: &str) -> String {
        text.replace("SECRET", "[redacted]")
    }
    fn root() -> PathBuf {
        PathBuf::from(".multiverse/tasks/20261001-fixer-abcdef")
    }
    fn lane() -> PathBuf {
        PathBuf::from(
            ".multiverse/worktrees/20261001-fixer-abcdef/.multiverse/tasks/20261001-fixer-abcdef",
        )
    }
    const ID: &str = "20261001-fixer-abcdef";

    #[test]
    fn root_copy_wins_and_redaction_does_not_modify_files() {
        let mut files = Files::default();
        for directory in [root(), lane()] {
            files.text(directory.join("status"), " DONE \n");
            files.text(
                directory.join("result.md"),
                &format!("{} SECRET", directory.display()),
            );
        }
        let result = read_result(&files, ID, &redact).unwrap();
        assert_eq!(result.source, root());
        assert_eq!(result.status, TaskStatus::Done);
        assert!(result.text.contains("[redacted]"));
        assert!(!result.text.contains("worktrees"));
        assert!(read_text(&files, &root().join("result.md"))
            .unwrap()
            .contains("SECRET"));
    }

    #[test]
    fn only_absent_root_status_permits_lane_fallback() {
        let mut files = Files::default();
        files.text(lane().join("status"), "blocked");
        files.text(lane().join("result.md"), "lane result");
        assert_eq!(read_result(&files, ID, &redact).unwrap().source, lane());
        for status in ["running", "", "unknown", "failed"] {
            files.text(root().join("status"), status);
            assert!(read_result(&files, ID, &redact).is_err());
        }
        files.unsafe_file(root().join("status"));
        assert!(read_result(&files, ID, &redact).is_err());
    }

    #[test]
    fn into_dispatch_lane_resolves_from_the_routing_logs_branch_field() {
        let mut files = Files::default();
        let owner = "20261001-fixer-owner1";
        files.text(
            ".multiverse/routing.log",
            &format!(
                "2026-10-01T00:00:00+00:00\tevent=dispatch\ttask={ID}\tbranch=multiverse/{owner}\n"
            ),
        );
        let owner_lane = PathBuf::from(format!(
            ".multiverse/worktrees/{owner}/.multiverse/tasks/{ID}"
        ));
        files.text(owner_lane.join("status"), "done");
        files.text(owner_lane.join("result.md"), "child result");
        let result = read_result(&files, ID, &redact).unwrap();
        assert_eq!(result.source, owner_lane);
        assert_eq!(result.text, "child result");
    }

    #[test]
    fn ordinary_dispatch_lane_ignores_a_branch_naming_its_own_task_id() {
        let mut files = Files::default();
        files.text(
            ".multiverse/routing.log",
            &format!(
                "2026-10-01T00:00:00+00:00\tevent=dispatch\ttask={ID}\tbranch=multiverse/{ID}\n"
            ),
        );
        files.text(lane().join("status"), "done");
        files.text(lane().join("result.md"), "own result");
        assert_eq!(read_result(&files, ID, &redact).unwrap().source, lane());
    }

    #[test]
    fn a_branch_without_the_multiverse_prefix_falls_back_to_the_tasks_own_lane() {
        let mut files = Files::default();
        files.text(
            ".multiverse/routing.log",
            &format!("2026-10-01T00:00:00+00:00\tevent=dispatch\ttask={ID}\tbranch=main\n"),
        );
        files.text(lane().join("status"), "done");
        files.text(lane().join("result.md"), "own result");
        assert_eq!(read_result(&files, ID, &redact).unwrap().source, lane());
    }

    #[test]
    fn a_later_dispatch_lines_branch_wins_over_an_earlier_one() {
        let mut files = Files::default();
        let stale_owner = "20261001-fixer-stale1";
        let current_owner = "20261001-fixer-currnt";
        files.text(
            ".multiverse/routing.log",
            &format!(
                "2026-10-01T00:00:00+00:00\tevent=dispatch\ttask={ID}\tbranch=multiverse/{stale_owner}\n\
                 2026-10-01T01:00:00+00:00\tevent=dispatch\ttask={ID}\tbranch=multiverse/{current_owner}\n"
            ),
        );
        let owner_lane = PathBuf::from(format!(
            ".multiverse/worktrees/{current_owner}/.multiverse/tasks/{ID}"
        ));
        files.text(owner_lane.join("status"), "done");
        files.text(owner_lane.join("result.md"), "current result");
        assert_eq!(read_result(&files, ID, &redact).unwrap().source, owner_lane);
    }

    #[test]
    fn empty_missing_and_unsafe_results_are_refused() {
        let mut files = Files::default();
        files.text(root().join("status"), "done");
        assert!(read_result(&files, ID, &redact).is_err());
        for text in ["", " \n\t"] {
            files.text(root().join("result.md"), text);
            assert!(read_result(&files, ID, &redact).is_err());
        }
        files.unsafe_file(root().join("result.md"));
        assert!(read_result(&files, ID, &redact).is_err());
    }

    #[test]
    fn traversal_and_non_identifier_ids_never_reach_the_filesystem() {
        let files = Files::default();
        for id in [
            "../escape",
            "/absolute",
            ".hidden",
            "a/b",
            "",
            "a b",
            &"x".repeat(65),
        ] {
            assert!(read_result(&files, id, &redact).is_err());
        }
        assert!(files.opened.borrow().is_empty());
    }

    #[test]
    fn each_terminal_agent_status_is_accepted_including_old_history_ids() {
        for status in ["done", "blocked", "failed"] {
            let mut files = Files::default();
            files.text(root().join("status"), status);
            files.text(root().join("result.md"), "result");
            assert_eq!(
                read_result(&files, ID, &redact).unwrap().status.as_str(),
                status
            );
        }
    }

    #[derive(Default)]
    struct Audit {
        records: Vec<InspectAudit>,
        fail: bool,
    }
    impl AuditSink for Audit {
        fn append(&mut self, record: &InspectAudit) -> io::Result<()> {
            self.records.push(record.clone());
            if self.fail {
                Err(io::Error::other("audit unavailable"))
            } else {
                Ok(())
            }
        }
    }
    fn request(paths: &[PathBuf]) -> InspectRequest<'_> {
        InspectRequest {
            project_root: Path::new("/project"),
            cwd: Path::new("/project/src"),
            paths,
            lines: Some(LineRange { first: 2, last: 3 }),
            matcher: None,
            reason: "debugging parser",
        }
    }
    struct Contains(&'static str);
    impl Matcher for Contains {
        fn is_match(&self, text: &str) -> bool {
            text.contains(self.0)
        }
    }

    #[test]
    fn inspect_paths_are_cwd_relative_and_lines_are_one_based_inclusive() {
        let mut files = Files::default();
        files.text("src/code.rs", "first\nsecond SECRET\nthird\nfourth\n");
        let paths = [PathBuf::from("code.rs")];
        let mut audit = Audit::default();
        let output = inspect(&files, &request(&paths), &redact, &mut audit).unwrap();
        assert_eq!(output, "== src/code.rs ==\nsecond SECRET\nthird\n");
        assert_eq!(audit.records.len(), 1);
        assert_eq!(audit.records[0].bytes, output.len());
        assert_eq!(audit.records[0].outcome, "ok");
        assert!(!format!("{:?}", audit.records[0]).contains("second"));
    }

    #[test]
    fn inspect_exactly_one_selector_and_reason_and_one_or_two_paths() {
        let files = Files::default();
        let matcher = Contains("needle");
        for paths in [vec![], vec!["a".into(), "b".into(), "c".into()]] {
            let mut audit = Audit::default();
            assert!(inspect(&files, &request(&paths), &redact, &mut audit).is_err());
            assert_eq!(audit.records.len(), 1);
            assert!(audit.records[0].outcome.starts_with("refused: "));
        }
        let paths = [PathBuf::from("file")];
        for case in 0..5 {
            let mut req = request(&paths);
            match case {
                0 => req.lines = None,
                1 => req.matcher = Some(&matcher),
                2 => req.reason = " \t\n",
                3 => req.lines = Some(LineRange { first: 0, last: 1 }),
                4 => req.lines = Some(LineRange { first: 3, last: 2 }),
                _ => unreachable!(),
            }
            let mut audit = Audit::default();
            assert!(inspect(&files, &req, &redact, &mut audit).is_err());
            assert_eq!(audit.records.len(), 1);
        }
    }

    #[test]
    fn inspect_refuses_globs_escapes_directories_and_unsafe_files() {
        for path in [
            "*.rs",
            "file?",
            "file[1]",
            "../../escape",
            "/outside/file",
            ".",
            "linked",
        ] {
            let mut files = Files::default();
            // Directory, symlink, FIFO and hard-link refusals are reported by
            // the secure-file capability; no contents can be read on refusal.
            files.unsafe_file("src");
            files.unsafe_file("src/linked");
            let paths = [PathBuf::from(path)];
            let mut audit = Audit::default();
            assert!(
                inspect(&files, &request(&paths), &redact, &mut audit).is_err(),
                "{path}"
            );
            assert_eq!(audit.records.len(), 1);
            assert_eq!(audit.records[0].bytes, 0);
        }
    }

    #[test]
    fn match_selector_matches_full_long_line_and_shares_utf8_budget_across_files() {
        let mut files = Files::default();
        files.text("src/first", &format!("{}needle\n", "é".repeat(20_000)));
        files.text("src/second", &format!("{}needle\n", "é".repeat(40_000)));
        let paths = ["first".into(), "second".into()];
        let matcher = Contains("needle");
        let mut req = request(&paths);
        req.lines = None;
        req.matcher = Some(&matcher);
        let mut audit = Audit::default();
        let output = inspect(&files, &req, &redact, &mut audit).unwrap();
        assert!(output.contains("src/first"));
        assert!(output.contains("src/second"));
        assert!(output.ends_with(TRUNCATION_NOTICE));
        assert!(output.len() <= INSPECT_BYTE_LIMIT);
        assert!(output.len() > INSPECT_BYTE_LIMIT - 4);
        assert_eq!(audit.records.len(), 1);
        assert_eq!(audit.records[0].bytes, output.len());
    }

    #[test]
    fn inspected_content_is_unredacted_within_the_byte_budget() {
        let mut files = Files::default();
        files.text("src/file", &"SECRET".repeat(20_000));
        let paths = ["file".into()];
        let mut req = request(&paths);
        req.lines = Some(LineRange { first: 1, last: 1 });
        let output = inspect(&files, &req, &redact, &mut Audit::default()).unwrap();
        assert!(output.len() <= INSPECT_BYTE_LIMIT);
        assert!(output.ends_with(TRUNCATION_NOTICE));
        assert!(output.contains("SECRET"));
    }

    #[test]
    fn audit_is_sanitized_once_on_refusal_and_failure_does_not_mask_success() {
        let mut files = Files::default();
        files.text("src/file", "ok\n");
        let paths = ["file".into()];
        let mut req = request(&paths);
        req.reason = "SECRET\nevent=other\tfield=forged";
        let mut audit = Audit {
            fail: true,
            ..Default::default()
        };
        assert!(inspect(&files, &req, &redact, &mut audit).is_ok());
        assert_eq!(audit.records.len(), 1);
        assert!(!audit.records[0].reason.contains(['\n', '\t']));
        assert!(!audit.records[0].reason.contains("SECRET"));
        req.lines = None;
        assert!(inspect(&files, &req, &redact, &mut audit).is_err());
        assert_eq!(audit.records.len(), 2);
        assert!(audit.records[1].outcome.starts_with("refused: "));
    }
}
