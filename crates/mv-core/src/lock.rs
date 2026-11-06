//! Reentrant, thread-local-aware `flock` (LOCK_EX|LOCK_NB, bounded poll
//! and timeout) over `rustix::fs`.
//!
//! Mirrors `accounts._locked_path` (`accounts.py:2801-2869`): `flock` locks
//! an *open file description*, not a process, so a second exclusive
//! acquisition from this same process on a fresh descriptor would block
//! against the first one forever rather than seeing "I already hold this."
//! A thread-local set of currently-held lock paths makes re-entering a lock
//! this thread already holds a no-op instead of a self-deadlock — only the
//! outermost [`acquire`] call opens a descriptor and flocks it, and only
//! that call's [`LockGuard`] actually unlocks on drop. This is per-*thread*,
//! not per-process or global: two independent threads (or two processes)
//! still block on each other for real.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{flock, FlockOperation};
use rustix::io::Errno;

/// The one conservative, process-wide default timeout every named lock in
/// the Python original uses unless a caller asks for a shorter one in
/// tests. Mirrors `accounts.LOCK_ACQUIRE_TIMEOUT_SECONDS`.
pub const LOCK_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(30);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A named lock could not be acquired before its bounded deadline.
///
/// Mirrors `accounts.LockAcquireTimeoutError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("timed out acquiring {lock_name} lock at {path} after {timeout:?}")]
pub struct LockAcquireTimeoutError {
    pub lock_name: String,
    pub path: PathBuf,
    pub timeout: Duration,
}

/// Everything that can go wrong acquiring a lock: either the bounded
/// deadline passed, or the filesystem itself refused (missing parent,
/// permission denied, too many open files, …).
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(transparent)]
    Timeout(#[from] LockAcquireTimeoutError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

thread_local! {
    static HELD_LOCKS: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// An acquired lock. Dropping it releases the OS-level flock — unless this
/// guard was a reentrant acquire of a lock this thread already held, in
/// which case dropping it does nothing, and the outermost guard's drop is
/// the one that actually unlocks.
pub struct LockGuard {
    held: Option<HeldLock>,
}

struct HeldLock {
    file: File,
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(held) = self.held.take() {
            let _ = flock(&held.file, FlockOperation::Unlock);
            HELD_LOCKS.with(|locks| {
                locks.borrow_mut().remove(&held.path);
            });
        }
    }
}

/// Acquire an exclusive, reentrant lock on `path`, polling every 50ms up to
/// `timeout`. `lock_name` names the lock in a timeout's error message only
/// (`"project"`, `"worktree"`, …) — it is not part of the lock's identity,
/// which is `path` alone.
///
/// Creates `path`'s parent directory (mode `0700`) if it does not already
/// exist, the same way `_locked_path` does before ever trying to open the
/// lock file itself.
pub fn acquire(path: &Path, lock_name: &str, timeout: Duration) -> Result<LockGuard, LockError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_owner_only(parent)?;
    }

    let path = path.to_path_buf();
    let already_held = HELD_LOCKS.with(|locks| locks.borrow().contains(&path));
    if already_held {
        return Ok(LockGuard { held: None });
    }

    HELD_LOCKS.with(|locks| {
        locks.borrow_mut().insert(path.clone());
    });

    match acquire_os_lock(&path, lock_name, timeout) {
        Ok(file) => Ok(LockGuard {
            held: Some(HeldLock { file, path }),
        }),
        Err(err) => {
            HELD_LOCKS.with(|locks| {
                locks.borrow_mut().remove(&path);
            });
            Err(err)
        }
    }
}

/// [`acquire`] with the fleet's one conservative default,
/// [`LOCK_ACQUIRE_TIMEOUT`].
pub fn acquire_default(path: &Path, lock_name: &str) -> Result<LockGuard, LockError> {
    acquire(path, lock_name, LOCK_ACQUIRE_TIMEOUT)
}

fn acquire_os_lock(path: &Path, lock_name: &str, timeout: Duration) -> Result<File, LockError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;

    let deadline = Instant::now() + timeout;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            // `EWOULDBLOCK` and `EAGAIN` are the same value on every target
            // this workspace builds for, so matching both is a redundant
            // pattern the compiler rejects; `EAGAIN` alone covers both.
            // `EACCES` is included because some systems with mandatory
            // locking enabled can also report a held `flock` that way.
            Err(Errno::AGAIN) | Err(Errno::ACCESS) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(LockAcquireTimeoutError {
                        lock_name: lock_name.to_string(),
                        path: path.to_path_buf(),
                        timeout,
                    }
                    .into());
                }
                thread::sleep(POLL_INTERVAL.min(remaining));
            }
            Err(errno) => return Err(io::Error::from(errno).into()),
        }
    }
}

#[cfg(unix)]
fn set_owner_only(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_owner_only(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn reentrant_acquire_on_the_same_thread_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/lock");

        let outer = acquire(&path, "test", Duration::from_secs(5)).unwrap();
        let inner = acquire(&path, "test", Duration::from_secs(5)).unwrap();
        drop(inner);
        drop(outer);
    }

    #[test]
    fn a_second_thread_blocks_until_the_first_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");

        let guard = acquire(&path, "test", Duration::from_secs(5)).unwrap();

        let (tx, rx) = mpsc::channel();
        let path_for_thread = path.clone();
        let handle = thread::spawn(move || {
            tx.send(()).unwrap();
            let _second = acquire(&path_for_thread, "test", Duration::from_secs(5)).unwrap();
        });

        rx.recv().unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(!handle.is_finished(), "second thread acquired a held lock");

        drop(guard);
        handle.join().unwrap();
    }

    #[test]
    fn a_second_acquirer_times_out_while_the_first_holds_it() {
        // Reentrancy is per-thread, so a second acquire from a *different*
        // thread is what actually contends for the OS-level flock — a
        // second acquire from this same thread would just re-enter.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");

        let guard = acquire(&path, "test", Duration::from_secs(5)).unwrap();

        let path_for_thread = path.clone();
        let result =
            thread::spawn(move || acquire(&path_for_thread, "test", Duration::from_millis(100)))
                .join()
                .unwrap();

        match result {
            Err(LockError::Timeout(timeout_err)) => {
                assert_eq!(timeout_err.lock_name, "test");
                assert_eq!(timeout_err.path, path);
            }
            Ok(_guard) => panic!("expected a timeout error, acquired the lock instead"),
            Err(other) => panic!("expected a timeout error, got {other:?}"),
        }

        drop(guard);
    }

    #[test]
    fn releasing_lets_a_later_acquirer_through() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");

        let guard = acquire(&path, "test", Duration::from_secs(5)).unwrap();
        drop(guard);

        acquire(&path, "test", Duration::from_secs(5)).unwrap();
    }

    /// Env var a re-exec'd child checks: when set, its value is the lock
    /// path to hold, and the child skips the test body entirely.
    const HOLD_MARKER_VAR: &str = "MV_CORE_LOCK_TEST_HOLD_PATH";

    #[test]
    fn a_second_process_times_out_while_another_process_holds_it() {
        // This test doubles as its own helper child: re-exec'd with
        // `HOLD_MARKER_VAR` set, it holds the lock instead of asserting
        // anything, so the parent can observe real cross-process
        // contention rather than two threads in one process.
        if let Ok(path) = std::env::var(HOLD_MARKER_VAR) {
            let _guard = acquire(Path::new(&path), "test", Duration::from_secs(10)).unwrap();
            thread::sleep(Duration::from_secs(10));
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");

        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(exe)
            .arg("--exact")
            .arg("lock::tests::a_second_process_times_out_while_another_process_holds_it")
            .arg("--nocapture")
            .env(HOLD_MARKER_VAR, &path)
            .spawn()
            .expect("failed to spawn helper child process");

        // Poll for the child to actually hold the lock rather than
        // sleeping a fixed guess: try to acquire it ourselves — an
        // uncontended acquire returns immediately, so sleep between
        // attempts ourselves — retrying until it fails (meaning the child
        // now holds it) or we give up and fail the test outright.
        let child_has_lock = (0..150).any(|_| {
            let acquired = match acquire(&path, "test", Duration::from_millis(1)) {
                Ok(_guard) => false,
                Err(LockError::Timeout(_)) => true,
                Err(other) => panic!("unexpected error while polling: {other}"),
            };
            if !acquired {
                thread::sleep(Duration::from_millis(20));
            }
            acquired
        });
        assert!(child_has_lock, "child process never acquired the lock");

        match acquire(&path, "test", Duration::from_millis(300)) {
            Err(LockError::Timeout(timeout_err)) => {
                assert_eq!(timeout_err.path, path);
            }
            Ok(_guard) => panic!("acquired a lock a separate process holds"),
            Err(other) => panic!("expected a timeout error, got {other}"),
        }

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn creates_a_missing_parent_directory_at_mode_0700() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/lock");

        let _guard = acquire(&path, "test", Duration::from_secs(5)).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}
