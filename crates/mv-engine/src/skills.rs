//! Skill grant algebra: `*` / `!name` / `!*` / `[]`, denial wins,
//! sorted result.
//!
//! An unknown skill fails here (and at config load, not at render
//! time). `render` omits the heading entirely when nothing is granted.
//! Playbook files are looked up under `prompts/skills/{name}.md` from
//! the same source [`crate::prompts`] resolved, so the two cannot drift
//! (`skills.py:50-59`).

use std::collections::HashSet;
use std::fmt;

use crate::prompts::{self, MissingPromptError, Source};

/// The grant token meaning "every skill" (`skills.py:62`).
pub const ALL: &str = "*";

/// Prefix marking a denial (`skills.py:65`).
pub const DENY: &str = "!";

/// One playbook: an id, a one-line purpose, and where its text lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skill {
    pub name: &'static str,
    pub purpose: &'static str,
}

impl Skill {
    /// Relative path under `prompts/`: `skills/{name}.md`.
    pub fn relative_path(&self) -> String {
        format!("skills/{}.md", self.name)
    }
}

/// The nine skills, ported from `skills.py:87-124`.
pub const SKILLS: &[Skill] = &[
    Skill {
        name: "codemap",
        purpose: "Build and use a hierarchical repository map instead of re-reading the tree.",
    },
    Skill {
        name: "deepwork",
        purpose: "Run a large, risky, multi-phase change through explicit review gates.",
    },
    Skill {
        name: "verification-planning",
        purpose: "Decide what evidence would prove this change correct, before making it.",
    },
    Skill {
        name: "simplify",
        purpose: "Behaviour-preserving simplification for readability and maintainability.",
    },
    Skill {
        name: "worktrees",
        purpose: "Use a git worktree as an isolated lane for risky or parallel work.",
    },
    Skill {
        name: "clonedeps",
        purpose: "Clone a dependency's source locally to read its internals directly.",
    },
    Skill {
        name: "reflect",
        purpose: "Turn repeated friction into a reusable skill, role, or config change.",
    },
    Skill {
        name: "multiverse",
        purpose: "Inspect and safely improve this Multiverse setup itself.",
    },
    Skill {
        name: "docker",
        purpose: "Start and clean up temporary Docker containers without touching production.",
    },
];

/// A grant names a skill that does not exist (`skills.py:127-141`).
///
/// Raised rather than ignored. A typo in a grant list fails *open* if
/// ignored — the role silently runs without the playbook it was meant
/// to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSkillError {
    pub name: String,
    pub where_: String,
}

impl UnknownSkillError {
    pub fn new(name: impl Into<String>, where_: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            where_: where_.into(),
        }
    }
}

impl fmt::Display for UnknownSkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let known = known_names().join(", ");
        write!(
            f,
            "{} grants unknown skill {:?}. Known skills: {known}.",
            self.where_, self.name
        )
    }
}

impl std::error::Error for UnknownSkillError {}

/// Failures from [`playbook`] / [`render`]: an unknown name, or a known
/// skill whose file is missing from the resolved source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillError {
    #[error(transparent)]
    Unknown(#[from] UnknownSkillError),
    #[error(transparent)]
    Missing(#[from] MissingPromptError),
}

fn known_names() -> Vec<&'static str> {
    let mut names: Vec<_> = SKILLS.iter().map(|skill| skill.name).collect();
    names.sort_unstable();
    names
}

fn skill_named(name: &str) -> Option<&'static Skill> {
    SKILLS.iter().find(|skill| skill.name == name)
}

fn all_names() -> HashSet<&'static str> {
    SKILLS.iter().map(|skill| skill.name).collect()
}

/// The effective skill names for a grant list, sorted (`skills.py:144-180`).
///
/// `where_` names the config location, for error messages only. An empty
/// grant list means no skills — a real answer, not "unset".
pub fn resolve(
    grants: impl IntoIterator<Item = impl AsRef<str>>,
    where_: &str,
) -> Result<Vec<&'static str>, UnknownSkillError> {
    let mut granted: HashSet<&'static str> = HashSet::new();
    let mut denied: HashSet<&'static str> = HashSet::new();
    let catalog = all_names();

    for raw in grants {
        let token = raw.as_ref().trim();
        if token.is_empty() {
            continue;
        }
        if token == ALL {
            granted.extend(catalog.iter().copied());
            continue;
        }
        if token == DENY_ALL {
            denied.extend(catalog.iter().copied());
            continue;
        }
        if let Some(name) = token.strip_prefix(DENY) {
            let name = name.trim();
            let skill = skill_named(name).ok_or_else(|| UnknownSkillError::new(name, where_))?;
            denied.insert(skill.name);
            continue;
        }
        let skill = skill_named(token).ok_or_else(|| UnknownSkillError::new(token, where_))?;
        granted.insert(skill.name);
    }

    // Denials win regardless of position, so `["*", "!reflect"]` and
    // `["!reflect", "*"]` are the same grant (`skills.py:178-180`).
    let mut names: Vec<_> = granted.difference(&denied).copied().collect();
    names.sort_unstable();
    Ok(names)
}

const DENY_ALL: &str = "!*";

/// The playbook text for `name`, without its human-facing preamble.
pub fn playbook(name: &str) -> Result<String, SkillError> {
    playbook_from(name, &prompts::current_prompts_source())
}

/// Like [`playbook`], from an explicit prompt source.
pub fn playbook_from(name: &str, source: &Source) -> Result<String, SkillError> {
    let skill = skill_named(name).ok_or_else(|| UnknownSkillError::new(name, "playbook()"))?;
    let prompt = prompts::load(&skill.relative_path(), "skill", skill.name, source)?;
    Ok(prompt.body())
}

/// The granted playbooks as one Markdown section, or empty for no grants.
///
/// `()` → `""` — no heading emitted at all (`skills.py:198-212`).
pub fn render(names: &[&str]) -> Result<String, SkillError> {
    render_from(names, &prompts::current_prompts_source())
}

/// Like [`render`], from an explicit prompt source.
pub fn render_from(names: &[&str], source: &Source) -> Result<String, SkillError> {
    if names.is_empty() {
        return Ok(String::new());
    }
    let mut blocks = vec![
        "# Skills available to you".to_string(),
        String::new(),
        "Each of these is a playbook you may follow when the task calls for it. \
         They are granted to your role deliberately; a skill absent here is one \
         you were not given, not one you should improvise."
            .to_string(),
        String::new(),
    ];
    for name in names {
        blocks.push(playbook_from(name, source)?);
        blocks.push(String::new());
    }
    Ok(blocks.join("\n").trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_default(grants: &[&str]) -> Result<Vec<&'static str>, UnknownSkillError> {
        resolve(grants, "skills")
    }

    // --- The grant table (tests/test_skills.py:21-81) ----------------

    #[test]
    fn star_grants_everything() {
        let granted = resolve_default(&["*"]).unwrap();
        let expected: HashSet<_> = SKILLS.iter().map(|skill| skill.name).collect();
        assert_eq!(granted.into_iter().collect::<HashSet<_>>(), expected);
    }

    #[test]
    fn an_explicit_list_grants_only_those() {
        assert_eq!(resolve_default(&["simplify"]).unwrap(), vec!["simplify"]);
    }

    #[test]
    fn empty_grants_nothing() {
        assert_eq!(resolve_default(&[]).unwrap(), Vec::<&str>::new());
    }

    #[test]
    fn deny_removes_from_star() {
        let granted = resolve_default(&["*", "!reflect"]).unwrap();
        assert!(!granted.contains(&"reflect"));
        assert!(granted.contains(&"codemap"));
    }

    #[test]
    fn star_minus_reflect_matches_python_order() {
        // Evidence named in architecture L6: grant `["*", "!reflect"]`
        // matches Python `sorted(granted - denied)`.
        assert_eq!(
            resolve_default(&["*", "!reflect"]).unwrap(),
            vec![
                "clonedeps",
                "codemap",
                "deepwork",
                "docker",
                "multiverse",
                "simplify",
                "verification-planning",
                "worktrees",
            ]
        );
    }

    #[test]
    fn deny_all_grants_nothing() {
        assert_eq!(resolve_default(&["!*"]).unwrap(), Vec::<&str>::new());
    }

    #[test]
    fn deny_wins_over_an_explicit_grant() {
        assert_eq!(
            resolve_default(&["simplify", "!*"]).unwrap(),
            Vec::<&str>::new()
        );
        assert_eq!(
            resolve_default(&["simplify", "!simplify"]).unwrap(),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn order_never_changes_the_result() {
        assert_eq!(
            resolve_default(&["*", "!reflect"]).unwrap(),
            resolve_default(&["!reflect", "*"]).unwrap()
        );
        assert_eq!(
            resolve_default(&["!*", "simplify"]).unwrap(),
            resolve_default(&["simplify", "!*"]).unwrap()
        );
    }

    #[test]
    fn grants_are_sorted_and_deduplicated() {
        assert_eq!(
            resolve_default(&["simplify", "simplify"]).unwrap(),
            vec!["simplify"]
        );
        assert_eq!(
            resolve_default(&["worktrees", "codemap"]).unwrap(),
            vec!["codemap", "worktrees"]
        );
    }

    #[test]
    fn an_unknown_skill_raises() {
        let err = resolve_default(&["simplfy"]).unwrap_err();
        assert!(err.to_string().contains("simplfy"), "{err}");
    }

    #[test]
    fn an_unknown_denial_also_raises() {
        assert!(resolve_default(&["*", "!codemapp"]).is_err());
    }

    #[test]
    fn the_error_names_the_known_skills() {
        let err = resolve_default(&["nope"]).unwrap_err();
        assert!(err.to_string().contains("codemap"), "{err}");
        assert!(
            err.to_string().contains("skills grants unknown skill"),
            "{err}"
        );
    }

    #[test]
    fn blank_tokens_are_ignored() {
        assert_eq!(
            resolve_default(&["codemap", "", "  "]).unwrap(),
            vec!["codemap"]
        );
    }

    #[test]
    fn loading_a_playbook_for_an_unknown_skill_raises() {
        let err = playbook_from("not-a-skill", &Source::Embedded).unwrap_err();
        assert!(matches!(err, SkillError::Unknown(_)));
    }

    // --- Every declared skill ships a playbook -----------------------

    #[test]
    fn every_skill_has_a_playbook_file() {
        for skill in SKILLS {
            playbook_from(skill.name, &Source::Embedded)
                .unwrap_or_else(|err| panic!("{}: {err}", skill.name));
        }
    }

    #[test]
    fn every_playbook_has_content_after_its_preamble() {
        for skill in SKILLS {
            let body = playbook_from(skill.name, &Source::Embedded).unwrap();
            assert!(
                body.lines().count() > 5,
                "{} playbook is empty after its preamble",
                skill.name
            );
            assert!(
                body.contains("## Skill:"),
                "{} playbook has no skill heading",
                skill.name
            );
        }
    }

    #[test]
    fn render_is_empty_when_nothing_is_granted() {
        assert_eq!(render_from(&[], &Source::Embedded).unwrap(), "");
    }

    #[test]
    fn render_includes_each_granted_playbook() {
        let rendered = render_from(&["codemap", "simplify"], &Source::Embedded).unwrap();
        assert!(rendered.contains("## Skill: codemap"));
        assert!(rendered.contains("## Skill: simplify"));
        assert!(!rendered.contains("## Skill: worktrees"));
        assert!(rendered.starts_with("# Skills available to you"));
    }

    #[test]
    fn render_omits_the_heading_when_nothing_granted() {
        let rendered = render_from(&[], &Source::Embedded).unwrap();
        assert!(!rendered.contains("Skills available to you"));
        assert!(rendered.is_empty());
    }

    #[test]
    fn docker_skill_is_a_task_scoped_container_recipe() {
        let body = playbook_from("docker", &Source::Embedded).unwrap();
        assert!(body.contains("MULTIVERSE_TASK_ID"));
        assert!(body.contains("mvt-<project>-<task6>-<purpose>"));
        assert!(body.contains("multiverse.owner"));
        assert!(body.contains("multiverse.temp=1"));
        assert!(body.contains("docker compose -p"));
        assert!(body.contains("docker rm -f"));
        assert!(body.contains("Never kill by image"));
    }

    #[test]
    fn docker_skill_uses_the_configured_project_key_for_owner_labels() {
        let body = playbook_from("docker", &Source::Embedded).unwrap();
        assert!(body.contains("multiverse.owner=multiverse/${TASK_ID}"));
        assert!(!body.contains("multiverse.owner=Multiverse/${TASK_ID}"));
    }

    #[test]
    fn worktrees_skill_names_the_lane_python_path() {
        let body = playbook_from("worktrees", &Source::Embedded).unwrap();
        assert!(body.contains("../../../.venv/bin/python"));
        assert!(body.contains("Do NOT create one"));
    }

    #[test]
    fn worktrees_skill_names_the_restore_command() {
        let body = playbook_from("worktrees", &Source::Embedded).unwrap();
        assert!(body.contains("multiverse worktrees <path> --restore\n  <task-id>"));
        assert!(body.contains("refuses if a branch of that name already"));
        assert!(body.contains("refuses an unknown id, naming the ref it looked"));
    }

    #[test]
    fn worktrees_skill_still_carries_the_behaviour_in_the_tree_rule() {
        let body = playbook_from("worktrees", &Source::Embedded).unwrap();
        assert!(body
            .contains("authoritative question about it is not \"does the branch exist\" but \"is"));
        assert!(body
            .contains("it does not tell you whether that work landed — the behaviour-in-the-tree"));
    }

    #[test]
    fn worktrees_skill_names_the_prune_dry_run_preview() {
        let body = playbook_from("worktrees", &Source::Embedded).unwrap();
        assert!(body.contains("--prune --dry-run"));
        assert!(body.contains("PREDICT THE SPLIT"));
    }
}
