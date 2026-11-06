//! Role wire/display name maps: Nexus, Scout, Builder, Archivist,
//! Critic, Seer, Council, Artisan, Watcher. New input never accepts
//! the retired names; history readers still map them in by the
//! migrator in [`crate::migrate`].
//!
//! ## The rename, and how this module knows the mapping
//!
//! Python's nine roles (`orchestrator, explorer, oracle, fixer,
//! reviewer, council, librarian, designer, observer` —
//! `.multiverse/input/spec-config.md` §G) are frozen; this port's
//! write/config surface uses the nine names this task's brief lists.
//! No single document in this lane's inputs spells out the pairing as
//! one table — it is this module's one inferred judgment call,
//! assembled from scattered but mutually consistent evidence:
//!
//! - `orchestrator` → `nexus`: `architecture.md` §4, "Nexus seed:
//!   `orchestrator.py` `multiverse_invocation()`".
//! - `fixer` → `builder`: `architecture.md` §10, "migrator
//!   `[roles.fixer]`→`[roles.builder]`"; §9 names this very lane
//!   ("L3") as what "a Builder (fixer) needs".
//! - `oracle` → `seer`: this task's own brief labels `architecture.md`
//!   — authored by the legacy Oracle role — as
//!   "the Seer's L3"; `config.example.toml`'s `seer` commentary ("Seer
//!   takes the hardest questions", `on_exhausted = "queue"`) matches
//!   Oracle's job as the fleet's slow, expensive question-answerer.
//! - `reviewer` → `critic`: `config.example.toml`'s `critic`
//!   commentary ("exclude the vendor that produced the work under
//!   review") is a reviewer's job; both are non-writing roles.
//! - `council` → `council`: unchanged in both lists, and both specs
//!   agree it is excluded from the ordinary dispatch chain (its own
//!   subcommand, a candidate pool rather than a fallback chain).
//! - `librarian` → `archivist`: semantic match — a librarian archives.
//! - `designer` → `artisan`: spec-config.md §G's `WRITING_ROLES =
//!   {FIXER, DESIGNER}` makes `designer` a writing role; `artisan` is
//!   the closest semantic match among the three new names left once
//!   the other eight are placed.
//! - `observer` → `watcher`: `config.example.toml`'s `watcher`
//!   commentary ("every entry must be multimodal... cannot see the
//!   screenshot") is exactly an observer's job.
//!
//! If a clearer source for this mapping surfaces later (the brief
//! mentions scout `44cce9` as a prior rename proposal this lane did
//! not read), re-verify against it before trusting this table for
//! anything that touches real fleet history.

use std::fmt;

/// One of Multiverse's nine roles, spelled the way new config/CLI
/// input must spell it. See the module docs for the mapping onto
/// Python's retired names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    Nexus,
    Scout,
    Builder,
    Archivist,
    Critic,
    Seer,
    Council,
    Artisan,
    Watcher,
}

impl Role {
    /// Every role, in the order this brief lists them.
    pub const ALL: [Role; 9] = [
        Role::Nexus,
        Role::Scout,
        Role::Builder,
        Role::Archivist,
        Role::Critic,
        Role::Seer,
        Role::Council,
        Role::Artisan,
        Role::Watcher,
    ];

    /// The current `[roles.<key>]` / `chains.<key>` table key.
    pub fn config_key(self) -> &'static str {
        match self {
            Role::Nexus => "nexus",
            Role::Scout => "scout",
            Role::Builder => "builder",
            Role::Archivist => "archivist",
            Role::Critic => "critic",
            Role::Seer => "seer",
            Role::Council => "council",
            Role::Artisan => "artisan",
            Role::Watcher => "watcher",
        }
    }

    /// The human-facing name, e.g. in board output.
    pub fn display_name(self) -> &'static str {
        match self {
            Role::Nexus => "Nexus",
            Role::Scout => "Scout",
            Role::Builder => "Builder",
            Role::Archivist => "Archivist",
            Role::Critic => "Critic",
            Role::Seer => "Seer",
            Role::Council => "Council",
            Role::Artisan => "Artisan",
            Role::Watcher => "Watcher",
        }
    }

    /// This role's retired Python config key. [`Role::Council`] returns
    /// `"council"`: the name did not change.
    pub fn legacy_config_key(self) -> &'static str {
        match self {
            Role::Nexus => "orchestrator",
            Role::Scout => "explorer",
            Role::Builder => "fixer",
            Role::Archivist => "librarian",
            Role::Critic => "reviewer",
            Role::Seer => "oracle",
            Role::Council => "council",
            Role::Artisan => "designer",
            Role::Watcher => "observer",
        }
    }

    /// Parse a role from the *current* config key only.
    ///
    /// New user-written config, CLI `--role` flags, and anything else
    /// that is not reading historical state must go through this, not
    /// [`Role::from_any_config_key`] — "new input accepts only new
    /// keys" (this task's brief; `architecture.md` §10, "No aliases
    /// on *new input*: `--role fixer` is unknown").
    pub fn from_config_key(key: &str) -> Result<Role, UnknownRoleError> {
        Role::ALL
            .into_iter()
            .find(|r| r.config_key() == key)
            .ok_or_else(|| UnknownRoleError::new(key))
    }

    /// Parse a role from either the current key or one of Python's
    /// retired keys (`fixer`, `explorer`, …).
    ///
    /// For reading history only: an existing `routing.log`, a task id,
    /// or a `config.toml` that has not been migrated yet. Prefer
    /// [`Role::from_config_key`] for anything a human is writing now.
    pub fn from_any_config_key(key: &str) -> Result<Role, UnknownRoleError> {
        Role::ALL
            .into_iter()
            .find(|r| r.config_key() == key || r.legacy_config_key() == key)
            .ok_or_else(|| UnknownRoleError::new(key))
    }

    /// Whether `key` is one of the nine retired Python keys, including
    /// the four (`council`) that did not change — used by the migrator
    /// to decide whether a `[roles.<key>]` table or a chain entry needs
    /// rewriting at all.
    pub fn is_legacy_key(key: &str) -> bool {
        Role::ALL.into_iter().any(|r| r.legacy_config_key() == key)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// `key` does not name any of Multiverse's nine roles.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown role {key:?} — known roles are: {known}")]
pub struct UnknownRoleError {
    pub key: String,
    pub known: String,
}

impl UnknownRoleError {
    fn new(key: &str) -> Self {
        let known = Role::ALL
            .iter()
            .map(|r| r.config_key())
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            key: key.to_string(),
            known,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_round_trips_its_own_config_key() {
        for role in Role::ALL {
            assert_eq!(Role::from_config_key(role.config_key()), Ok(role));
        }
    }

    #[test]
    fn from_config_key_rejects_a_retired_name() {
        let err = Role::from_config_key("fixer").unwrap_err();
        assert_eq!(err.key, "fixer");
        assert!(err.known.contains("builder"));
    }

    #[test]
    fn from_any_config_key_accepts_both_spellings() {
        assert_eq!(Role::from_any_config_key("fixer"), Ok(Role::Builder));
        assert_eq!(Role::from_any_config_key("builder"), Ok(Role::Builder));
    }

    #[test]
    fn council_s_name_is_unchanged_by_the_rename() {
        assert_eq!(Role::Council.legacy_config_key(), "council");
        assert_eq!(Role::Council.config_key(), "council");
    }

    #[test]
    fn unknown_key_is_rejected_by_both_parsers() {
        assert!(Role::from_config_key("archaeologist").is_err());
        assert!(Role::from_any_config_key("archaeologist").is_err());
    }

    #[test]
    fn is_legacy_key_recognizes_every_retired_spelling() {
        for role in Role::ALL {
            assert!(Role::is_legacy_key(role.legacy_config_key()));
        }
        assert!(!Role::is_legacy_key("archaeologist"));
    }

    #[test]
    fn display_name_is_title_case_of_the_config_key() {
        assert_eq!(Role::Builder.to_string(), "Builder");
    }
}
