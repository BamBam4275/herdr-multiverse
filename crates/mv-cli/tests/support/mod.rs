use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

// Cargo does not refresh another package's executable for `cargo test -p
// mv-cli`. Build the repository's fake once, using Cargo's artifact path rather
// than assuming target/debug (custom target directories are supported).
fn fake_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY.get_or_init(|| {
        let output = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "mv-herdr",
                "--bin",
                "fake-herdr",
                "--locked",
                "--message-format=json",
            ])
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .output()
            .expect("build repository fake-herdr");
        assert_code(&output, 0);
        // Python's standard JSON reader avoids adding a dev-dependency outside
        // this lane's tests-only scope. Python is also required by the repo's
        // plugin and conformance checks. No shell interpolation is involved.
        let mut reader = Command::new("python3")
            .args([
                "-c",
                "import json, sys\nartifacts = [json.loads(line) for line in sys.stdin]\nprint(next(a['executable'] for a in artifacts if a.get('reason') == 'compiler-artifact' and a.get('target', {}).get('name') == 'fake-herdr' and a.get('executable')))",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("read Cargo JSON artifacts with python3");
        reader.stdin.take().unwrap().write_all(&output.stdout).unwrap();
        let artifact = reader.wait_with_output().unwrap();
        assert_code(&artifact, 0);
        let path = PathBuf::from(stdout(&artifact).trim());
        assert!(path.is_file(), "fake-herdr must exist; never use real Herdr");
        path
    })
}

/// Kills and waits on the wrapped child when dropped without having been
/// consumed, so a failed `assert!` between spawning a background process and
/// its final `wait` never leaves it running. Call
/// [`KillOnDrop::wait_with_output`] on the normal, non-panicking path — it
/// takes the child out before `Drop` runs, so the kill-on-drop is purely a
/// panic-path safety net, not a double `wait`.
pub struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub fn wait_with_output(mut self) -> std::io::Result<Output> {
        self.0
            .take()
            .expect("child not already taken")
            .wait_with_output()
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// See [`Sandbox::dispatch_background_with_slow_pane_layout`].
pub struct SlowPaneLayoutDispatch {
    pub child: KillOnDrop,
    pub layout_started: PathBuf,
    pub layout_finished: PathBuf,
    layout_release: PathBuf,
}

impl SlowPaneLayoutDispatch {
    /// Lets the paused `pane layout` call proceed. Call this only after
    /// observing whatever the test needed to observe while it was paused —
    /// releasing early is the point, not a courtesy.
    pub fn release_pane_layout(&self) {
        fs::write(&self.layout_release, b"").unwrap();
    }
}

pub struct Sandbox {
    pub root: PathBuf,
    pub project: PathBuf,
    pub recipient: PathBuf,
}

impl Sandbox {
    pub fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "mv-cli-integration-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create a fresh sandbox, never reuse one");
        let root = root.canonicalize().unwrap();
        let sandbox = Self {
            project: root.join("project with spaces"),
            recipient: root.join("recipient"),
            root,
        };
        for directory in [
            "home",
            "config",
            "runtime",
            "codex-home",
            "tmp",
            "project with spaces",
            "recipient",
        ] {
            fs::create_dir(sandbox.root.join(directory)).unwrap();
        }
        for project in [&sandbox.project, &sandbox.recipient] {
            let mut git = sandbox.command("git");
            git.current_dir(project).args(["init", "-q", "-b", "main"]);
            assert_code(&git.output().unwrap(), 0);
            fs::write(project.join("README.md"), "CLI integration fixture\n").unwrap();
            let mut git = sandbox.command("git");
            git.current_dir(project).args(["add", "README.md"]);
            assert_code(&git.output().unwrap(), 0);
            let mut git = sandbox.command("git");
            git.current_dir(project).args([
                "-c",
                "user.name=Integration Test",
                "-c",
                "user.email=integration@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ]);
            assert_code(&git.output().unwrap(), 0);
        }
        let config = format!(
            r#"[accounts.test-seat]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = '{codex_home}'
pool = "codex:integration"

[projects.sandbox]
path = '{project}'

[projects.sandbox.chains]
scout = ["test-seat"]
builder = ["test-seat"]

[projects.recipient]
path = '{recipient}'

[roles.scout]
write_scope = "none"
effort = "low"
on_exhausted = "queue"
skills = []

[roles.scout.models]
codex = "gpt-test"

[roles.builder]
write_scope = "declared"
effort = "low"
on_exhausted = "queue"
skills = []

[roles.builder.models]
codex = "gpt-test"

[activity.pairs]
sandbox = ["recipient"]
recipient = ["sandbox"]
"#,
            codex_home = sandbox.root.join("codex-home").display(),
            project = sandbox.project.display(),
            recipient = sandbox.recipient.display(),
        );
        fs::write(sandbox.root.join("config/config.toml"), config).unwrap();
        mv_config::load::load_config(&sandbox.root.join("config/config.toml"))
            .expect("integration fixture must be valid under the real config schema");
        // Also isolate the legacy default for a future default-config caller.
        fs::create_dir_all(sandbox.root.join("home/.config/multiverse")).unwrap();
        fs::copy(
            sandbox.root.join("config/config.toml"),
            sandbox.root.join("home/.config/multiverse/config.toml"),
        )
        .unwrap();
        sandbox
    }

    pub fn command(&self, executable: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(executable);
        // Retain only executable lookup. Inherited pane/task, auth, config,
        // socket and Git variables must never connect this fixture to a fleet.
        command.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command
            .current_dir(&self.project)
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_STATE_HOME", self.root.join("runtime"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.root.join("config"))
            .env("HERDR_PLUGIN_STATE_DIR", self.root.join("runtime"))
            .env("MULTIVERSE_CONFIG", self.root.join("config/config.toml"))
            .env("MULTIVERSE_STATE_DIR", self.root.join("runtime"))
            .env("MULTIVERSE_ALLOW_TEMP_PROJECTS", "1")
            .env("HERDR_BIN_PATH", fake_binary())
            .env("MV_FAKE_HERDR_STATE", self.root.join("fake-herdr.json"))
            .env("HERDR_PANE_ID", "w52:p1")
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.join("home/.gitconfig"));
        command
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        self.cli_in(&self.project, args)
    }

    pub fn cli_in(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_multiverse"))
            .current_dir(cwd)
            .args(args)
            .output()
            .expect("run built multiverse binary")
    }

    /// Like [`Sandbox::cli`], but with `PATH` replaced by an empty
    /// directory so a test can force "the notify hook (`curl`) is not on
    /// PATH" without touching the host's real `curl` (card
    /// `20261115-b-51cb75`).
    pub fn cli_without_curl(&self, args: &[&str]) -> Output {
        let empty_path = self.root.join("no-curl-path");
        fs::create_dir_all(&empty_path).unwrap();
        self.command(env!("CARGO_BIN_EXE_multiverse"))
            .current_dir(&self.project)
            .env("PATH", &empty_path)
            .args(args)
            .output()
            .expect("run built multiverse binary")
    }

    pub fn herdr(&self, args: &[&str]) -> Output {
        let output = self
            .command(fake_binary())
            .args(args)
            .output()
            .expect("run repository fake-herdr only");
        assert_code(&output, 0);
        output
    }

    pub fn dispatch(&self, task: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "dispatch",
            self.project.to_str().unwrap(),
            task,
            "--role",
            "scout",
            "--timeout",
            "5",
        ];
        args.extend_from_slice(extra);
        self.cli(&args)
    }

    /// Spawns `dispatch` in the background against a `HERDR_BIN_PATH`
    /// wrapper that pauses on the very first `pane layout` call — the
    /// earliest Herdr round trip dispatch makes once pane placement starts —
    /// then execs the repository's real `fake-herdr` for every call,
    /// including that one, so the dispatch itself still completes normally.
    /// The pause is a signal the test controls, not a sleep: the wrapper
    /// drops a marker file the instant it starts pausing (`layout_started`),
    /// then polls for a release marker ([`SlowPaneLayoutDispatch::release_pane_layout`])
    /// every 100ms before dropping a second marker the instant it stops
    /// (`layout_finished`) and exec'ing the real call. A test can therefore
    /// observe whether the layout call is still in progress, and control
    /// exactly how long it stays that way, without racing a wall-clock
    /// bound: it can assert a concurrent `activity send` into the same
    /// project completed while `layout_finished` does not yet exist, to
    /// prove dispatch's own project lock (`routing.log.lock`) was not still
    /// held across that Herdr call (card `20261115-b-c8bcb5`). The poll is
    /// itself bounded to 5s so that a regression which keeps the lock held
    /// (and so keeps the concurrent `activity send` blocked, so the test
    /// never reaches the point of releasing the pane layout call) still
    /// resolves as a failed assertion — `layout_finished` already present by
    /// the time the blocked call returns — rather than hanging forever.
    pub fn dispatch_background_with_slow_pane_layout(&self, task: &str) -> SlowPaneLayoutDispatch {
        let script_path = self.root.join("slow-herdr.sh");
        let layout_started = self.root.join("layout-started");
        let layout_finished = self.root.join("layout-finished");
        let layout_release = self.root.join("layout-release");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = pane ] && [ \"$2\" = layout ]; then\n  : > \"{started}\"\n  i=0\n  while [ ! -f \"{release}\" ] && [ \"$i\" -lt 50 ]; do\n    sleep 0.1\n    i=$((i + 1))\n  done\n  : > \"{finished}\"\nfi\nexec \"{real}\" \"$@\"\n",
                started = layout_started.display(),
                release = layout_release.display(),
                finished = layout_finished.display(),
                real = fake_binary().display(),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let child = self
            .command(env!("CARGO_BIN_EXE_multiverse"))
            .env("HERDR_BIN_PATH", &script_path)
            .args([
                "dispatch",
                self.project.to_str().unwrap(),
                task,
                "--role",
                "scout",
                "--timeout",
                "5",
                "--yes",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn background dispatch");
        SlowPaneLayoutDispatch {
            child: KillOnDrop::new(child),
            layout_started,
            layout_finished,
            layout_release,
        }
    }

    pub fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    files.insert(path.strip_prefix(root).unwrap().to_owned(), Vec::new());
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(&self.root, &self.root, &mut files);
        files
    }

    pub fn task_dirs(&self) -> Vec<PathBuf> {
        // Read-only roles dispatch directly in the project root.
        fs::read_dir(self.project.join(".multiverse/tasks"))
            .expect("dispatch must create a task directory")
            .map(|entry| entry.unwrap().path())
            .collect()
    }

    pub fn seed_task(&self) -> &'static str {
        let task = "20250101-fixer-abcdef";
        let directory = self.project.join(".multiverse/tasks").join(task);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("brief.md"), "# Historical Fixer task\n").unwrap();
        fs::write(directory.join("status"), "done\n").unwrap();
        fs::write(
            directory.join("result.md"),
            "# Historical result\n\nVerified fixture.\n",
        )
        .unwrap();
        fs::write(
            self.project.join(".multiverse/routing.log"),
            format!(
                "2025-01-01T00:00:00+00:00\tevent=dispatch\ttask={task}\tproject=sandbox\trole=fixer\taccount=test-seat\tpool=codex:integration\toutcome=chain-primary\n\
                 2025-01-01T00:01:00+00:00\tevent=started\ttask={task}\tpane=w52:p100\n\
                 2025-01-01T00:02:00+00:00\tevent=result\ttask={task}\tstatus=done\tstate=done\trevisions=0\n"
            ),
        )
        .unwrap();
        task
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub fn assert_code(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("UTF-8 stderr")
}

/// Proves `KillOnDrop` actually kills a still-running child when dropped
/// without `wait_with_output` ever being called — the panic path a failed
/// `assert!` would take.
#[test]
fn kill_on_drop_kills_a_still_running_child() {
    let child = Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn a long-running child to kill");
    let pid = child.id();
    drop(KillOnDrop::new(child));
    let alive = || {
        Command::new("ps")
            .args(["-p", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive() {
        assert!(
            std::time::Instant::now() < deadline,
            "child {pid} was still alive 5s after KillOnDrop dropped it"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
