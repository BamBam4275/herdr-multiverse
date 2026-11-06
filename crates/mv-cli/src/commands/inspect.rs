//! `multiverse inspect <paths...> --reason TEXT [--lines START:END | --match TEXT]`:
//! a narrow, audited read of one or two files inside the current project.
//!
//! The actual selection and audit logic is `mv_engine::gateway`'s
//! (another lane's fully self-contained module — its public API needs no
//! `mv-herdr`/`mv-state` type). This file supplies the filesystem
//! capability, the `--match` matcher, and the project/config plumbing.
//!
//! `args.rs`'s original `inspect` definition only had `paths`/`--reason`;
//! `gateway::inspect` requires exactly one of a line selector or a text
//! matcher, so `--lines`/`--match` were added to this task's one permitted
//! edit there (see `result.md`).

use std::fs::{File, Permissions};
use std::io::{self, BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use rustix::fs::{mkdirat, openat, Mode, OFlags, CWD};

use mv_engine::gateway::{
    self, AuditSink, InspectAudit, InspectRequest, LineRange, Matcher, SecureFiles,
};

use super::doctor::{project_path, resolve_config_path, utc_timestamp};
use crate::exit::Failure;

/// Opens relative to `root` through `mv_core::secure_open::open_regular`
/// (card `20261102-b-f25484`) — the shared `openat`/`O_NOFOLLOW` chain also
/// used by `commands::result`'s `CheckedFiles`, rather than this command
/// carrying its own copy of the same walk.
struct RealFiles<'a> {
    root: &'a Path,
}

impl SecureFiles for RealFiles<'_> {
    fn open_regular(&self, relative: &Path) -> io::Result<Box<dyn BufRead>> {
        mv_core::secure_open::open_regular(self.root, relative, "inspected")
    }
}

struct ContainsMatcher(String);

impl Matcher for ContainsMatcher {
    fn is_match(&self, line: &str) -> bool {
        line.contains(&self.0)
    }
}

/// Appends one `event=inspect` line to the project's `.multiverse/inspect.log`.
/// Best effort: a failure to append must never mask the inspect result
/// itself (`gateway::inspect` already guarantees that; this sink only has
/// to report its own I/O error, not swallow it twice).
///
/// The `.multiverse` directory and `inspect.log` are opened with
/// `openat`/`O_NOFOLLOW` so a planted symlink is refused rather than
/// followed, and `fchmod` runs only on a directory or file this call
/// just created (never on an existing `.multiverse` or a symlink target).
struct FileAudit {
    path: PathBuf,
    project: String,
}

fn symlink_refusal(error: rustix::io::Errno, label: &str) -> io::Error {
    if error == rustix::io::Errno::LOOP {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing a symlink at {label}"),
        )
    } else {
        io::Error::from(error)
    }
}

fn open_audit_dir(root: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    match openat(
        root,
        name,
        OFlags::CLOEXEC | OFlags::RDONLY | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let file = File::from(fd);
            if !file.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing a non-directory at .multiverse",
                ));
            }
            Ok(file)
        }
        Err(error) if error == rustix::io::Errno::NOENT => {
            let created = match mkdirat(root, name, Mode::RWXU) {
                Ok(()) => true,
                Err(error) if error == rustix::io::Errno::EXIST => false,
                Err(error) => return Err(symlink_refusal(error, ".multiverse")),
            };
            let file = File::from(
                openat(
                    root,
                    name,
                    OFlags::CLOEXEC | OFlags::RDONLY | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|error| symlink_refusal(error, ".multiverse"))?,
            );
            if !file.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing a non-directory at .multiverse",
                ));
            }
            if created {
                file.set_permissions(Permissions::from_mode(0o700))?;
            }
            Ok(file)
        }
        Err(error) => Err(symlink_refusal(error, ".multiverse")),
    }
}

fn open_audit_log(dir: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    let create_flags = OFlags::CLOEXEC
        | OFlags::WRONLY
        | OFlags::APPEND
        | OFlags::CREATE
        | OFlags::EXCL
        | OFlags::NOFOLLOW;
    match openat(dir, name, create_flags, Mode::RUSR | Mode::WUSR) {
        Ok(fd) => {
            let file = File::from(fd);
            file.set_permissions(Permissions::from_mode(0o600))?;
            Ok(file)
        }
        Err(error) if error == rustix::io::Errno::EXIST => {
            let file = File::from(
                openat(
                    dir,
                    name,
                    OFlags::CLOEXEC | OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|error| symlink_refusal(error, "inspect.log"))?,
            );
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "refusing a non-regular inspect.log",
                ));
            }
            Ok(file)
        }
        Err(error) => Err(symlink_refusal(error, "inspect.log")),
    }
}

impl AuditSink for FileAudit {
    fn append(&mut self, record: &InspectAudit) -> io::Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "inspect.log path has no parent",
            )
        })?;
        let file_name = self.path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "inspect.log path has no file name",
            )
        })?;
        let grandparent = parent.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "inspect.log path has no project root",
            )
        })?;
        let dir_name = parent.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "inspect.log parent has no name",
            )
        })?;

        let root = File::from(
            openat(
                CWD,
                grandparent,
                OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let dir = open_audit_dir(&root, dir_name)?;
        let mut file = open_audit_log(&dir, file_name)?;
        let line = format!(
            "{}\tevent=inspect\tproject={}\tpaths={}\tbytes={}\treason={}\toutcome={}\n",
            utc_timestamp(),
            self.project,
            record.paths.join(","),
            record.bytes,
            record.reason,
            record.outcome,
        );
        file.write_all(line.as_bytes())
    }
}

fn parse_line_range(raw: &str) -> Option<LineRange> {
    match raw.split_once(':').or_else(|| raw.split_once('-')) {
        Some((first, last)) => Some(LineRange {
            first: first.trim().parse().ok()?,
            last: last.trim().parse().ok()?,
        }),
        None => {
            let n: usize = raw.trim().parse().ok()?;
            Some(LineRange { first: n, last: n })
        }
    }
}

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        return Err(Failure::Config(format!(
            "no config file found at {}; run `multiverse doctor`",
            config_path.display()
        )));
    }
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;

    let cwd =
        std::env::current_dir().map_err(|error| Failure::Operation(format!("inspect: {error}")))?;
    let project = config
        .projects
        .values()
        .find(|project| {
            project_path(project)
                .and_then(|path| std::fs::canonicalize(path).ok())
                .is_some_and(|path| cwd == path || cwd.starts_with(path))
        })
        .ok_or_else(|| {
            Failure::Config("inspect must run from inside a configured project".to_string())
        })?;
    let project_root = project_path(project)
        .and_then(|path| std::fs::canonicalize(path).ok())
        .ok_or_else(|| Failure::Config(format!("{}: path is not configured", project.name)))?;

    let paths: Vec<PathBuf> = args
        .get_many::<String>("paths")
        .map(|values| values.map(PathBuf::from).collect())
        .unwrap_or_default();
    let reason = args
        .get_one::<String>("reason")
        .cloned()
        .unwrap_or_default();

    let lines = match args.get_one::<String>("lines") {
        Some(raw) => Some(parse_line_range(raw).ok_or_else(|| {
            Failure::Operation("inspect: --lines must be START:END, N, or N-M".to_string())
        })?),
        None => None,
    };
    let matcher = args
        .get_one::<String>("match")
        .cloned()
        .map(ContainsMatcher);

    let request = InspectRequest {
        project_root: &project_root,
        cwd: &cwd,
        paths: &paths,
        lines,
        matcher: matcher.as_ref().map(|m| m as &dyn Matcher),
        reason: &reason,
    };
    let files = RealFiles {
        root: &project_root,
    };
    let mut audit = FileAudit {
        path: project_root.join(".multiverse/inspect.log"),
        project: mv_core::redact::redact_text(&project.name)
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect(),
    };
    let redact = |text: &str| mv_core::redact::redact_text(text);

    match gateway::inspect(&files, &request, &redact, &mut audit) {
        Ok(text) => {
            print!("{text}");
            Ok(())
        }
        Err(refusal) => Err(Failure::Operation(refusal.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parse_line_range_accepts_a_single_number_or_a_range() {
        assert!(matches!(
            parse_line_range("4"),
            Some(LineRange { first: 4, last: 4 })
        ));
        assert!(matches!(
            parse_line_range("2-5"),
            Some(LineRange { first: 2, last: 5 })
        ));
        assert!(matches!(
            parse_line_range("1:2"),
            Some(LineRange { first: 1, last: 2 })
        ));
        assert!(parse_line_range("garbage").is_none());
        assert!(parse_line_range("").is_none());
    }

    #[test]
    fn contains_matcher_matches_a_substring() {
        let matcher = ContainsMatcher("needle".to_string());
        assert!(matcher.is_match("a needle in a haystack"));
        assert!(!matcher.is_match("nothing here"));
    }

    #[test]
    fn real_files_reads_a_plain_file_through_the_openat_chain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), "hello\n").unwrap();

        let files = RealFiles { root: dir.path() };
        let mut reader = files
            .open_regular(Path::new("sub/file.txt"))
            .expect("plain file through a real directory chain should open");
        let mut text = String::new();
        io::Read::read_to_string(&mut reader, &mut text).unwrap();
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn real_files_refuses_a_symlinked_final_component_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret elsewhere").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).unwrap();

        let files = RealFiles { root: dir.path() };
        let error = match files.open_regular(Path::new("link.txt")) {
            Ok(_) => panic!("expected the symlink to be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn real_files_refuses_a_symlinked_intermediate_directory_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret elsewhere").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("alias")).unwrap();

        let files = RealFiles { root: dir.path() };
        let error = match files.open_regular(Path::new("alias/secret.txt")) {
            Ok(_) => panic!("expected the symlinked directory to be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn file_audit_creates_the_log_at_owner_only_dir_and_file_modes() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(".multiverse/inspect.log");
        let mut audit = FileAudit {
            path: log_path.clone(),
            project: "sandbox".to_string(),
        };
        let record = InspectAudit {
            project: "sandbox".to_string(),
            paths: vec!["README.md".to_string()],
            bytes: 4,
            reason: "checking".to_string(),
            outcome: "ok".to_string(),
        };

        audit.append(&record).unwrap();

        let dir_mode = std::fs::metadata(log_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    fn sample_audit_record() -> InspectAudit {
        InspectAudit {
            project: "sandbox".to_string(),
            paths: vec!["README.md".to_string()],
            bytes: 4,
            reason: "checking".to_string(),
            outcome: "ok".to_string(),
        }
    }

    #[test]
    fn file_audit_refuses_a_symlinked_inspect_log_and_leaves_the_target() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "keep me\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join(".multiverse/inspect.log")).unwrap();

        let mut audit = FileAudit {
            path: dir.path().join(".multiverse/inspect.log"),
            project: "sandbox".to_string(),
        };
        let error = match audit.append(&sample_audit_record()) {
            Ok(()) => panic!("expected a planted inspect.log symlink to be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("inspect.log"));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep me\n");
        let victim_mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_ne!(victim_mode, 0o600, "must not chmod the symlink target");
    }

    #[test]
    fn file_audit_refuses_a_symlinked_multiverse_dir() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("inspect.log"), "keep me\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join(".multiverse")).unwrap();

        let mut audit = FileAudit {
            path: dir.path().join(".multiverse/inspect.log"),
            project: "sandbox".to_string(),
        };
        let error = match audit.append(&sample_audit_record()) {
            Ok(()) => panic!("expected a planted .multiverse symlink to be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains(".multiverse"));
        assert_eq!(
            std::fs::read_to_string(outside.join("inspect.log")).unwrap(),
            "keep me\n"
        );
    }

    #[test]
    fn file_audit_does_not_chmod_an_existing_multiverse_dir_or_log() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(".multiverse");
        std::fs::create_dir_all(&state).unwrap();
        let log_path = state.join("inspect.log");
        std::fs::write(&log_path, "").unwrap();
        std::fs::set_permissions(&state, Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&log_path, Permissions::from_mode(0o644)).unwrap();

        let mut audit = FileAudit {
            path: log_path.clone(),
            project: "sandbox".to_string(),
        };
        audit.append(&sample_audit_record()).unwrap();

        let dir_mode = std::fs::metadata(&state).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o755);
        assert_eq!(file_mode, 0o644);
        assert!(std::fs::read_to_string(&log_path)
            .unwrap()
            .contains("event=inspect"));
    }
}
