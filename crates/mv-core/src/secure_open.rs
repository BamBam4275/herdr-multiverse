//! Shared `openat`/`O_NOFOLLOW` secure-file reader (card `20261102-b-f25484`).
//!
//! `mv-cli`'s `inspect` and `result` commands each read an arbitrary,
//! caller- or task-named path inside a project root and must not trust a
//! lexical check-then-open (`std::fs::symlink_metadata` followed by a
//! separate `File::open`): a symlink swapped in between those two calls
//! would be followed anyway. This walks every path component, including
//! the final one, as a directory-descriptor-relative `openat` instead —
//! each call both checks and opens its component atomically against the
//! parent directory's own descriptor, so there is no window between the
//! two. Both commands' adapters now call this one implementation rather
//! than each carrying its own copy of the same chain.

use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use rustix::fs::{openat, Mode, OFlags, CWD};

/// Open `relative` under `root`, refusing any symlink, non-directory
/// intermediate, non-regular final component, or hard-linked file along
/// the way. `label` names what is being opened, used only in refusal text
/// (e.g. `"inspected"`, `"result"` — "refusing a symlink on the `{label}`
/// path").
///
/// No `O_DIRECTORY` on the intermediate opens: combined with `O_NOFOLLOW`
/// its failure errno for "last component is a symlink" is not consistent
/// across platforms (ELOOP vs. ENOTDIR). Opening plain, then checking the
/// resulting fd's own `fstat`, gives one refusal path regardless of
/// platform.
pub fn open_regular(root: &Path, relative: &Path, label: &str) -> io::Result<Box<dyn BufRead>> {
    let open_flags = OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::RDONLY | OFlags::NONBLOCK;
    let mut dir_fd =
        openat(CWD, root, open_flags, Mode::empty()).map_err(|e| symlink_refusal(e, label))?;

    let mut components = relative.components().peekable();
    loop {
        let component = components.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} path has no components"),
            )
        })?;
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must stay below its root",
            ));
        };
        let opened = openat(&dir_fd, name, open_flags, Mode::empty())
            .map_err(|e| symlink_refusal(e, label))?;

        if components.peek().is_some() {
            let file = File::from(opened);
            if !file.metadata()?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing a non-directory in the {label} path"),
                ));
            }
            dir_fd = file.into();
            continue;
        }

        let file = File::from(opened);
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing a non-regular file",
            ));
        }
        if metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing a hard-linked file",
            ));
        }
        return Ok(Box::new(BufReader::new(file)));
    }
}

/// `openat` with `O_NOFOLLOW` reports a symlink component as `ELOOP`, not
/// as a dedicated "it's a symlink" error — translate that one errno into
/// the same refusal a path-based symlink check would have raised, and
/// leave every other errno (including genuine not-found) as `io::Error`'s
/// own mapping so a missing path still reads as `NotFound`.
fn symlink_refusal(error: rustix::io::Errno, label: &str) -> io::Error {
    if error == rustix::io::Errno::LOOP {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing a symlink on the {label} path"),
        )
    } else {
        io::Error::from(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Box<dyn BufRead>` has no `Debug` impl, so `Result::unwrap_err`
    /// cannot be used directly on `open_regular`'s return type.
    fn expect_refused(result: io::Result<Box<dyn BufRead>>) -> io::Error {
        match result {
            Ok(_) => panic!("expected the path to be refused"),
            Err(error) => error,
        }
    }

    #[test]
    fn reads_a_plain_file_through_the_openat_chain() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), "hello\n").unwrap();

        let mut reader = open_regular(dir.path(), Path::new("sub/file.txt"), "demo")
            .expect("plain file through a real directory chain should open");
        let mut text = String::new();
        io::Read::read_to_string(&mut reader, &mut text).unwrap();
        assert_eq!(text, "hello\n");
    }

    #[test]
    fn refuses_a_symlinked_final_component_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret elsewhere").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).unwrap();

        let error = expect_refused(open_regular(dir.path(), Path::new("link.txt"), "demo"));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("demo"));
    }

    #[test]
    fn refuses_a_symlinked_intermediate_directory_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret elsewhere").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join("alias")).unwrap();

        let error = expect_refused(open_regular(
            dir.path(),
            Path::new("alias/secret.txt"),
            "demo",
        ));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn refuses_a_hard_linked_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("original.txt"), "data").unwrap();
        std::fs::hard_link(
            dir.path().join("original.txt"),
            dir.path().join("linked.txt"),
        )
        .unwrap();

        let error = expect_refused(open_regular(dir.path(), Path::new("linked.txt"), "demo"));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("hard-linked"));
    }

    #[test]
    fn refuses_a_directory_in_place_of_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let error = expect_refused(open_regular(dir.path(), Path::new("sub"), "demo"));
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn refuses_a_fifo_without_waiting_for_a_writer() {
        let dir = tempfile::tempdir().unwrap();
        assert!(std::process::Command::new("mkfifo")
            .arg(dir.path().join("pipe"))
            .status()
            .unwrap()
            .success());
        let root = dir.path().to_path_buf();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let error = expect_refused(open_regular(&root, Path::new("pipe"), "demo"));
            send.send(error.kind()).ok();
        });
        assert_eq!(
            receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("FIFO open hung"),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn a_missing_path_reads_as_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let error = expect_refused(open_regular(dir.path(), Path::new("missing.txt"), "demo"));
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
