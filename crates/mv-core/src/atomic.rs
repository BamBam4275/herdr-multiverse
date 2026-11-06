//! Atomic file writes: same-directory temp file, fsync, mode, rename,
//! directory fsync.
//!
//! Every store in the Python original duplicates this shape under a
//! different name (`seatstate._atomic_write`, `breaker._atomic_write`,
//! `limits._atomic_write_gauge_store`, `mail._write_file_fsync` +
//! `_fsync_dir`, `accounts._append`'s file-creation branch): write to a
//! temp file in the target's own directory so the rename is same-filesystem
//! and therefore atomic, fsync the temp file's contents, set its mode,
//! rename it over the destination, then fsync the directory so the rename
//! itself — a new directory entry — survives a crash (POSIX does not make
//! fsyncing a file imply anything about the directory entry that names it).

use std::fs::{self, File};
use std::io;
use std::path::Path;

use tempfile::NamedTempFile;

/// Write `contents` to `path` durably and atomically: temp file in the same
/// directory, fsync, `chmod 0600`, rename over `path`, then fsync the
/// directory so the rename survives a crash.
///
/// `path`'s parent directory must already exist — this function does not
/// create it, the same way none of the Python `_atomic_write` variants it
/// mirrors do; callers that need the directory created (`ensure_state_dir`
/// and friends) do so first.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic write path has no parent directory",
        )
    })?;

    let mut tmp = NamedTempFile::new_in(dir)?;
    io::Write::write_all(&mut tmp, contents)?;
    tmp.as_file().sync_all()?;
    set_owner_read_write_only(tmp.as_file())?;
    tmp.persist(path).map_err(|e| e.error)?;

    fsync_dir(dir)?;
    Ok(())
}

/// fsync `dir` so a create, rename, or unlink inside it is durable once this
/// returns. Pairs with [`write_atomic`], which calls this after its rename.
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Creates a brand-new file at `path`, mode `0600`, and writes `contents`
/// to it — refusing (`ErrorKind::AlreadyExists`) if anything is already
/// there, including a symlink, since `O_CREAT | O_EXCL` rejects the open
/// on the directory entry itself regardless of what it resolves to.
///
/// For a pre-overwrite backup copy made *before* the live file is
/// touched, where a partial write is simply re-run rather than
/// load-bearing: unlike [`write_atomic`] this is not a crash-atomic
/// temp-file-then-rename, and the mode is set at creation — not chmod'd
/// after — so there is no window where the file is readable by group or
/// other.
pub fn create_owner_read_write_only(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut file = open_new_owner_read_write_only(path)?;
    io::Write::write_all(&mut file, contents)?;
    file.sync_all()
}

#[cfg(unix)]
fn open_new_owner_read_write_only(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_new_owner_read_write_only(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(unix)]
fn set_owner_read_write_only(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_read_write_only(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_the_file_with_the_given_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");

        write_atomic(&path, b"{\"a\":1}\n").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"{\"a\":1}\n");
    }

    #[test]
    #[cfg(unix)]
    fn writes_the_file_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");

        write_atomic(&path, b"{}").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn replaces_existing_contents_wholesale_with_no_leftover_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.json");

        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second-and-longer-replacement").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"second-and-longer-replacement");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "store.json")
            .collect();
        assert!(leftovers.is_empty(), "leftover entries: {leftovers:?}");
    }

    #[test]
    fn fsync_dir_succeeds_on_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        fsync_dir(dir.path()).unwrap();
    }

    #[test]
    fn rejects_a_path_with_no_parent() {
        let err = write_atomic(Path::new(""), b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    #[cfg(unix)]
    fn create_owner_read_write_only_writes_contents_at_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.toml");

        create_owner_read_write_only(&path, b"the original").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"the original");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn create_owner_read_write_only_refuses_an_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.toml");
        fs::write(&path, "already here").unwrap();

        let err = create_owner_read_write_only(&path, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&path).unwrap(), "already here");
    }

    #[test]
    #[cfg(unix)]
    fn create_owner_read_write_only_refuses_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere.toml");
        fs::write(&target, "victim").unwrap();
        let link = dir.path().join("backup.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = create_owner_read_write_only(&link, b"new").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&target).unwrap(), "victim");
    }
}
