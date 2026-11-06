use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
            git.current_dir(project).args(["init", "-q"]);
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
