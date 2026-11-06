//! Role and stage prompt loading, plus the shell templates that ship
//! beside them.
//!
//! Filenames are not hardcoded: `build.rs` embeds every file under
//! `prompts/` and `templates/`, and lookups go by role/column/template
//! name (`role-{key}.md`, `column-{column.lower()}.md`). A sibling
//! directory or `MULTIVERSE_PROMPTS_DIR` override wins over the
//! embedded copy so a live edit loop does not need a rebuild.
//!
//! Mirrors `multiverse/prompts.py`. `Prompt::body` splits on the first
//! `"\n---\n"`; a missing file is [`MissingPromptError`], never an empty
//! prompt.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// Env var that, when set to a non-empty path, is used as `prompts/`
/// instead of the sibling-or-embedded resolution. Blank is treated as
/// unset, matching `HERDR_BIN_PATH`.
pub const PROMPTS_ENV: &str = "MULTIVERSE_PROMPTS_DIR";

/// Optional override for `templates/`. Blank is treated as unset.
pub const TEMPLATES_ENV: &str = "MULTIVERSE_TEMPLATES_DIR";

/// §12's three stages, in pipeline order (`prompts.py:137`).
pub const COLUMNS: [&str; 3] = ["Explore", "Execute", "Review"];

/// Where a prompt or template is loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Compiled-in files from `build.rs`.
    Embedded,
    /// A directory on disk: env override, sibling of the binary, or
    /// plugin root.
    Directory(PathBuf),
}

/// A card names a role or stage with no prompt file.
///
/// Raised rather than silently defaulting to an empty prompt: an agent
/// that receives no role prompt still runs, and produces plausible work
/// with none of the discipline the role was supposed to carry
/// (`prompts.py:74-89`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no {kind} prompt for {name:?} at {path}. Add it, or use a {kind} that has one.")]
pub struct MissingPromptError {
    pub kind: String,
    pub name: String,
    pub path: PathBuf,
}

impl MissingPromptError {
    pub fn new(kind: impl Into<String>, name: impl Into<String>, path: PathBuf) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            path,
        }
    }
}

/// A prompt, and where it came from — the source path is shown in the
/// dry-run so a card's instructions are traceable (`prompts.py:92-114`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub source: PathBuf,
    pub text: String,
}

impl Prompt {
    /// How many lines the file has, including the title and preamble.
    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }

    /// The prompt without its Markdown title and front-matter preamble.
    ///
    /// Everything before the first `"\n---\n"` separator is dropped when
    /// one is present; otherwise the whole file is returned, stripped.
    pub fn body(&self) -> String {
        match self.text.split_once("\n---\n") {
            Some((_, rest)) => rest.trim().to_string(),
            None => self.text.trim().to_string(),
        }
    }
}

/// The role prompt for `role`, looked up as `role-{key}.md`.
///
/// `role` is a config key or display name (`Nexus` and `nexus` both
/// resolve to `role-nexus.md`). Old on-disk history names are not
/// aliases here: `--role fixer` is unknown on the write side, and the
/// renamed files live under the new keys.
pub fn role_prompt(role: &str) -> Result<Prompt, MissingPromptError> {
    role_prompt_from(role, &current_prompts_source())
}

/// Like [`role_prompt`], from an explicit source (tests, dry-run
/// overrides, a caller that already resolved the directory).
pub fn role_prompt_from(role: &str, source: &Source) -> Result<Prompt, MissingPromptError> {
    let key = config_key(role);
    load(&format!("role-{key}.md"), "role", &key, source)
}

/// The stage system prompt for `column`, looked up as
/// `column-{column.lower()}.md`. Case-insensitive, matching
/// `prompts.py:140-143`.
pub fn column_prompt(column: &str) -> Result<Prompt, MissingPromptError> {
    column_prompt_from(column, &current_prompts_source())
}

/// Like [`column_prompt`], from an explicit source.
pub fn column_prompt_from(column: &str, source: &Source) -> Result<Prompt, MissingPromptError> {
    let file = format!("column-{}.md", column.to_ascii_lowercase());
    load(&file, "column", column, source)
}

/// A shipped template (`claude-account.sh.template`,
/// `install-wrappers.sh`, or whatever else lands under `templates/`).
pub fn template(name: &str) -> Result<Prompt, MissingPromptError> {
    template_from(name, &current_templates_source())
}

/// Like [`template`], from an explicit source.
pub fn template_from(name: &str, source: &Source) -> Result<Prompt, MissingPromptError> {
    load_from(
        name,
        "template",
        name,
        source,
        embedded_template,
        "templates",
    )
}

/// Python's `_resolve_prompts_dir`: `sibling` if it is a directory,
/// otherwise the embedded fallback (`prompts.py:54-67`).
pub fn resolve_prompts_dir(sibling: &Path) -> Source {
    if sibling.is_dir() {
        Source::Directory(sibling.to_path_buf())
    } else {
        Source::Embedded
    }
}

/// Env, then a `prompts/` directory next to the executable or at the
/// plugin root, then the embedded copy.
pub fn current_prompts_source() -> Source {
    resolve_override(PROMPTS_ENV, "prompts")
}

/// Same resolution for `templates/`, keyed on [`TEMPLATES_ENV`].
pub fn current_templates_source() -> Source {
    resolve_override(TEMPLATES_ENV, "templates")
}

pub(crate) fn load(
    relative: &str,
    kind: &str,
    name: &str,
    source: &Source,
) -> Result<Prompt, MissingPromptError> {
    load_from(relative, kind, name, source, embedded_prompt, "prompts")
}

fn load_from(
    relative: &str,
    kind: &str,
    name: &str,
    source: &Source,
    embedded: fn(&str) -> Option<&'static str>,
    virtual_root: &str,
) -> Result<Prompt, MissingPromptError> {
    match source {
        Source::Directory(dir) => {
            let path = dir.join(relative);
            match fs::read_to_string(&path) {
                Ok(text) => Ok(Prompt { source: path, text }),
                Err(_) => Err(MissingPromptError::new(kind, name, path)),
            }
        }
        Source::Embedded => match embedded(relative) {
            Some(text) => Ok(Prompt {
                source: PathBuf::from(format!("{virtual_root}/{relative}")),
                text: text.to_string(),
            }),
            None => Err(MissingPromptError::new(
                kind,
                name,
                PathBuf::from(format!("{virtual_root}/{relative}")),
            )),
        },
    }
}

fn config_key(role: &str) -> String {
    role.trim().to_ascii_lowercase()
}

fn resolve_override(env_var: &str, dir_name: &str) -> Source {
    if let Some(dir) = env_dir(env_var) {
        return Source::Directory(dir);
    }
    if let Some(dir) = sibling_dir(dir_name) {
        return Source::Directory(dir);
    }
    Source::Embedded
}

fn env_dir(var: &str) -> Option<PathBuf> {
    env_dir_from(env::var(var).ok().as_deref())
}

fn env_dir_from(raw: Option<&str>) -> Option<PathBuf> {
    match raw {
        Some(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

fn sibling_dir(dir_name: &str) -> Option<PathBuf> {
    if let Ok(exe) = env::current_exe() {
        if let Some(parent) = exe.parent() {
            let next_to_exe = parent.join(dir_name);
            if next_to_exe.is_dir() {
                return Some(next_to_exe);
            }
            if let Some(root) = parent.parent() {
                let plugin = root.join(dir_name);
                if plugin.is_dir() {
                    return Some(plugin);
                }
            }
        }
    }
    if let Ok(root) = env::var("HERDR_PLUGIN_ROOT") {
        if !root.trim().is_empty() {
            let dir = PathBuf::from(root).join(dir_name);
            if dir.is_dir() {
                return Some(dir);
            }
        }
    }
    None
}

fn embedded_prompt(relative: &str) -> Option<&'static str> {
    include!(concat!(env!("OUT_DIR"), "/embedded_prompts.rs"))
}

fn embedded_template(relative: &str) -> Option<&'static str> {
    include!(concat!(env!("OUT_DIR"), "/embedded_templates.rs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = env::temp_dir().join(format!(
                "mv-engine-prompts-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => return path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temporary project: {error}"),
            }
        }
    }

    // --- The shipped files (Python test_prompts.py, new role names) --

    #[test]
    fn scout_role_prompt_ships() {
        let prompt = role_prompt_from("scout", &Source::Embedded).unwrap();
        assert_eq!(
            prompt.source.file_name().unwrap().to_str().unwrap(),
            "role-scout.md"
        );
        assert!(prompt.line_count() > 20);
    }

    #[test]
    fn display_name_and_config_key_resolve_the_same_file() {
        let by_key = role_prompt_from("nexus", &Source::Embedded).unwrap();
        let by_display = role_prompt_from("Nexus", &Source::Embedded).unwrap();
        assert_eq!(by_key.source, by_display.source);
        assert_eq!(by_key.text, by_display.text);
    }

    #[test]
    fn every_current_role_has_an_embedded_prompt() {
        for role in [
            "nexus",
            "scout",
            "builder",
            "archivist",
            "critic",
            "seer",
            "council",
            "artisan",
            "watcher",
        ] {
            let prompt = role_prompt_from(role, &Source::Embedded)
                .unwrap_or_else(|err| panic!("{role}: {err}"));
            assert!(
                prompt.line_count() > 20,
                "{role} prompt is too short: {}",
                prompt.line_count()
            );
            assert!(
                prompt.body().starts_with("You are"),
                "{role} body should start with identity, got {:?}",
                prompt.body().chars().take(40).collect::<String>()
            );
        }
    }

    #[test]
    fn explore_column_prompt_ships() {
        let prompt = column_prompt_from("Explore", &Source::Embedded).unwrap();
        assert_eq!(
            prompt.source.file_name().unwrap().to_str().unwrap(),
            "column-explore.md"
        );
        assert!(prompt.line_count() > 20);
    }

    #[test]
    fn column_lookup_is_case_insensitive() {
        assert_eq!(
            column_prompt_from("explore", &Source::Embedded)
                .unwrap()
                .source,
            column_prompt_from("Explore", &Source::Embedded)
                .unwrap()
                .source
        );
    }

    #[test]
    fn every_column_has_an_embedded_prompt() {
        for column in COLUMNS {
            let prompt = column_prompt_from(column, &Source::Embedded).unwrap();
            assert!(prompt.line_count() > 20, "{column}");
        }
    }

    // --- Resolver ----------------------------------------------------

    #[test]
    fn prompts_dir_prefers_the_sibling_when_present() {
        let sibling = temp_dir();
        assert_eq!(
            resolve_prompts_dir(&sibling),
            Source::Directory(sibling.clone())
        );
        fs::remove_dir_all(&sibling).unwrap();
    }

    #[test]
    fn prompts_dir_falls_back_to_embedded_when_sibling_is_absent() {
        let missing = PathBuf::from("/no/such/prompts-directory-for-mv-engine-tests");
        assert_eq!(resolve_prompts_dir(&missing), Source::Embedded);
    }

    #[test]
    fn fallback_location_can_actually_load_a_prompt() {
        let dir = temp_dir();
        fs::write(dir.join("role-scout.md"), "# R\n\n---\n\nYou are a role.\n").unwrap();
        let prompt = role_prompt_from("scout", &Source::Directory(dir.clone())).unwrap();
        assert_eq!(prompt.body(), "You are a role.");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_prompt_raises_rather_than_defaulting_to_empty() {
        let err = column_prompt_from("NoSuchStage", &Source::Embedded).unwrap_err();
        assert!(err.to_string().contains("column-nosuchstage.md"), "{}", err);
        assert_eq!(err.kind, "column");
        assert_eq!(err.name, "NoSuchStage");
    }

    #[test]
    fn body_strips_the_human_facing_preamble() {
        let prompt = role_prompt_from("scout", &Source::Embedded).unwrap();
        assert!(prompt.body().starts_with("You are"));
        assert!(
            prompt.text.contains("# Scout — role prompt"),
            "preamble title lives in the raw file"
        );
        assert!(
            !prompt.body().contains("# Scout — role prompt"),
            "body must drop the human-facing title before ---"
        );
        assert!(prompt.text.contains("column-explore.md"));
        assert!(!prompt.body().contains("column-explore.md"));
    }

    #[test]
    fn body_without_a_separator_returns_the_stripped_file() {
        let dir = temp_dir();
        fs::write(dir.join("role-scout.md"), "  just instructions  \n").unwrap();
        let prompt = role_prompt_from("scout", &Source::Directory(dir.clone())).unwrap();
        assert_eq!(prompt.body(), "just instructions");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn env_override_wins_over_embedded() {
        let dir = temp_dir();
        fs::write(
            dir.join("role-nexus.md"),
            "# N\n\n---\n\nYou are an override.\n",
        )
        .unwrap();
        let prompt = role_prompt_from("nexus", &Source::Directory(dir.clone())).unwrap();
        assert_eq!(prompt.body(), "You are an override.");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn blank_env_is_treated_as_unset() {
        assert_eq!(env_dir_from(Some("   ")), None);
        assert_eq!(env_dir_from(Some("")), None);
        assert_eq!(env_dir_from(None), None);
        assert_eq!(
            env_dir_from(Some("/tmp/prompts")),
            Some(PathBuf::from("/tmp/prompts"))
        );
    }

    #[test]
    fn old_role_names_do_not_alias_to_renamed_prompt_files() {
        let err = role_prompt_from("fixer", &Source::Embedded).unwrap_err();
        assert!(err.to_string().contains("role-fixer.md"), "{err}");
        assert!(role_prompt_from("builder", &Source::Embedded).is_ok());
    }

    // --- Templates ---------------------------------------------------

    #[test]
    fn templates_are_embedded_without_hardcoded_names() {
        let account = template_from("claude-account.sh.template", &Source::Embedded).unwrap();
        assert!(account.text.contains("{{CONFIG_DIR}}"));
        let wrappers = template_from("install-wrappers.sh", &Source::Embedded).unwrap();
        assert!(wrappers.text.contains("install-wrappers.sh"));
    }

    #[test]
    fn missing_template_is_a_hard_error() {
        let err = template_from("no-such.sh", &Source::Embedded).unwrap_err();
        assert!(err.to_string().contains("no-such.sh"), "{err}");
        assert_eq!(err.kind, "template");
    }

    // --- Content pins mapped from Python explorer/fixer tests --------

    #[test]
    fn scout_role_prompt_states_the_read_only_constraint() {
        let body = role_prompt_from("scout", &Source::Embedded).unwrap().body();
        assert!(body.contains("Write scope: none"));
    }

    #[test]
    fn scout_and_seer_prompts_document_the_retire_sidecar() {
        for role in ["scout", "seer"] {
            let body = role_prompt_from(role, &Source::Embedded).unwrap().body();
            assert!(body.contains("retire"), "{role}");
            assert!(body.contains("<stranded-task-id> <"), "{role}");
        }
    }

    #[test]
    fn builder_role_prompt_says_finishing_means_committed() {
        let body = role_prompt_from("builder", &Source::Embedded)
            .unwrap()
            .body();
        assert!(body.contains("committed and reported, not merely reported"));
        assert!(body.to_ascii_lowercase().contains("prunable"));
    }

    #[test]
    fn column_prompt_states_the_stage_is_read_only() {
        let body = column_prompt_from("Explore", &Source::Embedded)
            .unwrap()
            .body();
        assert!(body.to_ascii_lowercase().contains("read-only"));
    }

    #[test]
    fn column_prompt_carries_the_corrected_state_semantics() {
        let body = column_prompt_from("Explore", &Source::Embedded)
            .unwrap()
            .body();
        assert!(body.contains("does not mean the session ended"));
    }

    #[test]
    fn builder_and_scout_prompts_carry_the_load_generation_rule() {
        for role in ["builder", "scout"] {
            let body = role_prompt_from(role, &Source::Embedded)
                .unwrap()
                .body()
                .to_ascii_lowercase();
            assert!(body.contains("3 cpu-hog processes"), "{role}");
            assert!(body.contains("10-minute wall-clock cap"), "{role}");
            assert!(body.contains("uptime"), "{role}");
            assert!(body.contains("above 16"), "{role}");
            assert!(body.contains("trap on exit"), "{role}");
            assert!(body.contains("every 30s"), "{role}");
            assert!(body.contains("passes 24"), "{role}");
            assert!(body.contains("sysctl -n vm.loadavg"), "{role}");
        }
    }

    #[test]
    fn builder_and_artisan_prompts_run_the_full_suite_once() {
        for role in ["builder", "artisan"] {
            let body = role_prompt_from(role, &Source::Embedded)
                .unwrap()
                .body()
                .to_ascii_lowercase();
            assert!(body.contains("-n auto"), "{role}");
            assert!(body.contains("pytest-xdist"), "{role}");
            assert!(body.contains("green run is final"), "{role}");
            assert!(body.contains("code change or a failure"), "{role}");
            assert!(body.contains("wait on its own completion"), "{role}");
            assert!(body.contains("polling with fresh runs"), "{role}");
            assert!(body.contains("pytest -n 4"), "{role}");
            assert!(body.contains("go test -p=2"), "{role}");
            assert!(body.contains("gomaxprocs=4"), "{role}");
        }
    }
}
