//! Per-seat state read/write path, backed by
//! [`mv_state::seatstate_store`]. `cap`/`lend`/`prefer` are all wired
//! here (`accounts limit`/`accounts lend`/`accounts prefer`).
//!
//! Fail-open defaults, matching `seatstate.py`: a seat absent from the
//! store, or with a field the store couldn't parse into the right type,
//! reads as uncapped / lent / not-preferred — never as blocked. A seat name
//! is never validated here; an unknown seat simply reads as all-defaults,
//! same as an empty entry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use mv_config::load::SeatProvider;
use mv_config::roles::Role;
use mv_state::breaker_store::StoreError;
use mv_state::seatstate_store::{self, SeatEntry};

/// Missing/non-bool `lend` reads as **lent** (`true`) — a store that
/// doesn't mention a seat at all must not block it.
pub fn is_lent(config_dir: &Path, seat: &str) -> bool {
    entry(config_dir, seat).lend.unwrap_or(true)
}

/// `None` means uncapped (fail-open toward availability). A stored `cap`
/// of `0` is treated the same as uncapped — `seatstate.py`'s `set_cap`
/// rejects `cap<1` at the write side, so a `0` on disk can only be a
/// hand-edit, and the read side's job is to stay available, not enforce
/// the write-side invariant.
pub fn get_cap(config_dir: &Path, seat: &str) -> Option<u32> {
    entry(config_dir, seat).cap.filter(|&cap| cap >= 1)
}

/// Missing/non-bool `prefer` reads as **not preferred**.
pub fn is_preferred(config_dir: &Path, seat: &str) -> bool {
    entry(config_dir, seat).prefer.unwrap_or(false)
}

fn entry(config_dir: &Path, seat: &str) -> SeatEntry {
    seatstate_store::load(config_dir)
        .remove(seat)
        .unwrap_or_default()
}

/// `accounts limit <seat> <cap>` / `accounts limit <seat> --off`'s write
/// path. `cap: None` clears the limit (fail-open toward availability, same
/// posture as the read side); `Some(0)` is rejected rather than stored as a
/// no-op uncapped entry, matching `seatstate.py`'s `set_cap`, which never
/// lets a caller write the cap-vs-uncapped distinction ambiguously.
#[derive(Debug, thiserror::Error)]
pub enum SetCapError {
    /// The requested cap was below 1 — `seatstate.py`'s own floor for a
    /// stored cap, kept so the uncapped/capped distinction on disk is
    /// never ambiguous.
    #[error("seat capacity must be at least 1, got {0}")]
    InvalidCap(u32),
    /// The seatstate store could not be read or written.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// `accounts limit <seat> <cap>` / `accounts limit <seat> --off`'s entry
/// point: validates `cap` (see [`SetCapError::InvalidCap`]) before handing
/// off to the locked write.
pub fn set_cap(config_dir: &Path, seat: &str, cap: Option<u32>) -> Result<(), SetCapError> {
    if let Some(cap) = cap {
        if cap < 1 {
            return Err(SetCapError::InvalidCap(cap));
        }
    }
    seatstate_store::with_locked(config_dir, |map| {
        map.entry(seat.to_owned()).or_default().cap = cap;
        seatstate_store::prune_if_empty(map, seat);
    })?;
    Ok(())
}

/// `accounts lend <seat> on|off`'s write path — a human's own toggle, kept
/// outside config so it needs no edit/reload.
pub fn set_lend(config_dir: &Path, seat: &str, lend: bool) -> Result<(), StoreError> {
    seatstate_store::with_locked(config_dir, |map| {
        map.entry(seat.to_owned()).or_default().lend = Some(lend);
    })?;
    Ok(())
}

/// `accounts prefer <seat> on|off`'s write path — mirrors [`set_lend`]:
/// a human's own toggle, kept outside config so it needs no edit/reload.
pub fn set_prefer(config_dir: &Path, seat: &str, prefer: bool) -> Result<(), StoreError> {
    seatstate_store::with_locked(config_dir, |map| {
        map.entry(seat.to_owned()).or_default().prefer = Some(prefer);
    })?;
    Ok(())
}

/// Count all open sessions, including idle/done ones. Unknown or malformed
/// listing data stays unknown rather than becoming a fabricated zero.
pub fn open_count_from_payload(payload: &serde_json::Value, short: &str) -> Option<u32> {
    let agents = payload.get("result")?.get("agents")?.as_array()?;
    let mut count = 0;
    for agent in agents {
        let name = agent.get("name")?.as_str()?;
        let base = name
            .rsplit_once('-')
            .filter(|(_, suffix)| suffix.parse::<u32>().is_ok())
            .map_or(name, |(base, _)| base);
        if name.ends_with(&format!("-{short}")) || base.ends_with(&format!("-{short}")) {
            count += 1;
        }
    }
    Some(count)
}

/// Count every live agent matching `short`, including idle/done and Nexus
/// panes. Herdr errors fail open to an unknown count. Kept with this
/// `&str -> Option<u32>` signature so existing fn-pointer call sites stay
/// valid; `seat show` uses [`live_seat_open_counts`] (card
/// `20261115-b-4ded07`).
pub fn live_open_count(short: &str) -> Option<u32> {
    let result = mv_herdr::run::run(&mv_herdr::argv::agent_list(), None).ok()?;
    if !result.ok() {
        return None;
    }
    open_count_from_payload(result.payload.as_ref()?, short)
}

/// Whether a routing-log `role=` field (current or retired config key)
/// names Nexus — mirrors `commands::dispatch`'s own `is_nexus_role`, so a
/// busy Nexus pane is identified the same way on every surface that reads
/// a seat's open count (card `20261115-b-234610`).
pub fn is_nexus_role(role: &str) -> bool {
    Role::from_any_config_key(role).is_ok_and(|role| role == Role::Nexus)
}

/// Every live agent name whose *newest* routing-log dispatch, across the
/// given project roots, is Nexus-role — the same per-name dedup
/// `commands::dispatch`'s `latest_tasks_by_agent` applies (a name is
/// reissued once its previous occupant is no longer live, so an older
/// dispatch under the same name must never outvote a newer one). Feeds
/// [`seat_open_counts_from_payload`]'s worker/Nexus split so `check`/
/// `accounts`/`review_policy`/`seat show` agree with `dispatch`/`council`
/// about what counts against a seat's own cap (cards `20261115-b-234610`,
/// `20261115-b-4ded07`).
pub fn nexus_agent_names(project_roots: &[PathBuf]) -> HashSet<String> {
    let mut latest: HashMap<String, (u64, bool)> = HashMap::new();
    for root in project_roots {
        let mut tasks = mv_state::tasks::sweep_tasks(root);
        tasks.sort_by_key(|task| task.dispatch_ordinal);
        for task in tasks {
            let Some(agent) = task.agent else {
                continue;
            };
            latest.insert(agent, (task.dispatch_ordinal, is_nexus_role(&task.role)));
        }
    }
    latest
        .into_iter()
        .filter(|(_, (_, nexus))| *nexus)
        .map(|(name, _)| name)
        .collect()
}

/// A seat's live open count, split into worker vs. Nexus panes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeatOpenCounts {
    pub workers: u32,
    pub nexus: u32,
}

/// Role-aware sibling of [`open_count_from_payload`]: same trailing
/// `-<short>` name match, but a matched name present in `nexus_agents`
/// ([`nexus_agent_names`]) counts toward `nexus` rather than `workers` — so
/// a busy Nexus pane never inflates a worker seat's cap count, and an
/// account's own `nexus_slots` cap can be read against `nexus` directly
/// (card `20261115-b-234610`).
pub fn seat_open_counts_from_payload(
    payload: &serde_json::Value,
    short: &str,
    nexus_agents: &HashSet<String>,
) -> Option<SeatOpenCounts> {
    let agents = payload.get("result")?.get("agents")?.as_array()?;
    let mut counts = SeatOpenCounts::default();
    for agent in agents {
        let name = agent.get("name")?.as_str()?;
        let base = name
            .rsplit_once('-')
            .filter(|(_, suffix)| suffix.parse::<u32>().is_ok())
            .map_or(name, |(base, _)| base);
        if name.ends_with(&format!("-{short}")) || base.ends_with(&format!("-{short}")) {
            if nexus_agents.contains(name) {
                counts.nexus += 1;
            } else {
                counts.workers += 1;
            }
        }
    }
    Some(counts)
}

/// Role-aware sibling of [`live_open_count`]: one Herdr probe per call,
/// split by [`seat_open_counts_from_payload`]. Herdr errors fail open to an
/// unknown count, same as [`live_open_count`]. `seat show` reports
/// [`SeatOpenCounts::workers`] (card `20261115-b-4ded07`).
pub fn live_seat_open_counts(
    short: &str,
    nexus_agents: &HashSet<String>,
) -> Option<SeatOpenCounts> {
    let result = mv_herdr::run::run(&mv_herdr::argv::agent_list(), None).ok()?;
    if !result.ok() {
        return None;
    }
    seat_open_counts_from_payload(result.payload.as_ref()?, short, nexus_agents)
}

/// JSON values are individually escaped before joining with the historical
/// report's separators, preserving quotes and punctuation inside names.
pub fn show_json(
    seat: &str,
    kind: &str,
    model: Option<&str>,
    provider: Option<&SeatProvider>,
    cap: Option<u32>,
    open: Option<u32>,
) -> String {
    let value = serde_json::json!({
        "auth_env_key": provider.and_then(|p| p.auth_env_key.as_deref()),
        "base_url": provider.and_then(|p| p.base_url.as_deref()),
        "cap": cap, "kind": kind, "model": model, "open": open,
        "provider": provider.map(|p| p.name.as_str()), "seat": seat,
    });
    let fields = value
        .as_object()
        .expect("report is an object")
        .iter()
        .map(|(key, value)| {
            format!(
                "{}: {value}",
                serde_json::to_string(key).expect("key serializes")
            )
        })
        .collect::<Vec<_>>();
    format!("{{{}}}", fields.join(", "))
}

/// Human block used by `seat show`, matching the historical CLI wording.
pub fn show_human(
    seat: &str,
    kind: &str,
    model: Option<&str>,
    provider: Option<&SeatProvider>,
    cap: Option<u32>,
    open: Option<u32>,
) -> String {
    let model = model
        .filter(|model| !model.is_empty())
        .unwrap_or("(none — this seat takes its model from the role)");
    let mut lines = vec![
        format!("seat: {seat}"),
        format!("kind: {kind}"),
        format!("model: {model}"),
    ];
    if let Some(provider) = provider {
        lines.push(format!("provider: {}", provider.name));
        lines.push(format!(
            "base_url: {}",
            provider
                .base_url
                .as_deref()
                .filter(|url| !url.is_empty())
                .unwrap_or("(not set)")
        ));
        lines.push(format!(
            "auth: {}",
            provider
                .auth_env_key
                .as_deref()
                .filter(|key| !key.is_empty())
                .unwrap_or("none")
        ));
    }
    lines.push(format!(
        "cap: {}",
        cap.map_or_else(|| "none (uncapped)".to_owned(), |cap| cap.to_string())
    ));
    lines.push(match (open, cap) {
        (None, _) => "open: ? (Herdr unreachable)".to_owned(),
        (Some(open), Some(cap)) => format!("open: {open}/{cap}"),
        (Some(open), None) => format!("open: {open}"),
    });
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_state::seatstate_store::SeatMap;

    #[test]
    fn seat_report_json_matches_the_wire_schema_and_escapes_names() {
        assert_eq!(
            show_json("explorer-seat", "codex", None, None, None, None),
            r#"{"auth_env_key": null, "base_url": null, "cap": null, "kind": "codex", "model": null, "open": null, "provider": null, "seat": "explorer-seat"}"#
        );
        let report = show_json(
            "seat\", : name",
            "codex",
            Some("model"),
            None,
            Some(2),
            Some(1),
        );
        let parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(parsed["seat"], "seat\", : name");
    }

    #[test]
    fn seat_report_json_populates_and_escapes_provider_metadata() {
        let provider = SeatProvider {
            name: "local\"provider".into(),
            base_url: Some("http://localhost:1234/v1".into()),
            auth_env_key: Some("LOCAL_API_KEY".into()),
        };
        assert_eq!(
            show_json(
                "local",
                "codex",
                Some("model"),
                Some(&provider),
                Some(1),
                Some(0)
            ),
            r#"{"auth_env_key": "LOCAL_API_KEY", "base_url": "http://localhost:1234/v1", "cap": 1, "kind": "codex", "model": "model", "open": 0, "provider": "local\"provider", "seat": "local"}"#
        );
    }

    #[test]
    fn open_counts_include_done_sessions_and_uniqueness_suffixes() {
        let payload = serde_json::json!({"result": {"agents": [
            {"name": "mv-orch-cx", "agent_status": "done"},
            {"name": "mv-builder-cx-2", "agent_status": "working"},
            {"name": "mv-critic-cxl", "agent_status": "idle"}
        ]}});
        assert_eq!(open_count_from_payload(&payload, "cx"), Some(2));
        assert_eq!(open_count_from_payload(&payload, "cl"), Some(0));
        assert_eq!(open_count_from_payload(&serde_json::json!({}), "cx"), None);
        assert_eq!(
            open_count_from_payload(&serde_json::json!({"result": {"agents": [{}]}}), "cx"),
            None
        );
    }

    #[test]
    fn a_seat_missing_from_the_store_is_lent_uncapped_and_not_preferred() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_lent(dir.path(), "claude-1"));
        assert_eq!(get_cap(dir.path(), "claude-1"), None);
        assert!(!is_preferred(dir.path(), "claude-1"));
    }

    #[test]
    fn an_explicit_lend_false_is_not_lent() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = SeatMap::new();
        map.insert(
            "claude-1".to_string(),
            SeatEntry {
                lend: Some(false),
                ..Default::default()
            },
        );
        seatstate_store::save(dir.path(), &map).unwrap();
        assert!(!is_lent(dir.path(), "claude-1"));
    }

    #[test]
    fn a_zero_cap_on_disk_reads_as_uncapped_not_zero_capacity() {
        let dir = tempfile::tempdir().unwrap();
        // Only reachable by hand-editing the file; `set_cap` rejects
        // cap<1 at the write side (Wave 2, not implemented here).
        std::fs::write(
            seatstate_store::store_path(dir.path()),
            br#"{"claude-1": {"cap": 0}}"#,
        )
        .unwrap();
        assert_eq!(get_cap(dir.path(), "claude-1"), None);
    }

    #[test]
    fn cap_lend_prefer_read_back_independently() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = SeatMap::new();
        map.insert(
            "codex-1".to_string(),
            SeatEntry {
                cap: Some(3),
                lend: None,
                prefer: Some(true),
            },
        );
        seatstate_store::save(dir.path(), &map).unwrap();
        assert_eq!(get_cap(dir.path(), "codex-1"), Some(3));
        assert!(is_lent(dir.path(), "codex-1"));
        assert!(is_preferred(dir.path(), "codex-1"));
    }

    #[test]
    fn a_corrupt_store_fails_open_for_every_seat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(seatstate_store::store_path(dir.path()), b"{not json").unwrap();
        assert!(is_lent(dir.path(), "anything"));
        assert_eq!(get_cap(dir.path(), "anything"), None);
        assert!(!is_preferred(dir.path(), "anything"));
    }

    #[test]
    fn set_cap_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        set_cap(dir.path(), "codex-1", Some(1)).unwrap();
        assert_eq!(get_cap(dir.path(), "codex-1"), Some(1));
        set_cap(dir.path(), "codex-1", None).unwrap();
        assert_eq!(get_cap(dir.path(), "codex-1"), None);
        assert!(!seatstate_store::load(dir.path()).contains_key("codex-1"));
    }

    #[test]
    fn set_cap_rejects_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            set_cap(dir.path(), "codex-1", Some(0)),
            Err(SetCapError::InvalidCap(0))
        ));
        assert!(seatstate_store::load(dir.path()).is_empty());
    }

    #[test]
    fn set_lend_round_trips_without_disturbing_cap() {
        let dir = tempfile::tempdir().unwrap();
        set_cap(dir.path(), "seat-a", Some(2)).unwrap();
        set_lend(dir.path(), "seat-a", false).unwrap();
        assert!(!is_lent(dir.path(), "seat-a"));
        assert_eq!(get_cap(dir.path(), "seat-a"), Some(2));
        set_lend(dir.path(), "seat-a", true).unwrap();
        assert!(is_lent(dir.path(), "seat-a"));
    }

    /// Card `20261115-b-234610`, proof: a busy Nexus pane's agent name is
    /// identified off its own routing-log dispatch, the same signal
    /// `commands::dispatch`'s `is_nexus_role` reads.
    #[test]
    fn nexus_agent_names_identifies_a_nexus_role_dispatch() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".multiverse")).unwrap();
        std::fs::write(
            project.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-nexus-aaaaaa\t\
             role=nexus\tagent=mv-nexus-1-se\tbranch=main\n",
        )
        .unwrap();
        let names = nexus_agent_names(&[project.path().to_path_buf()]);
        assert!(names.contains("mv-nexus-1-se"));
    }

    #[test]
    fn nexus_agent_names_excludes_a_worker_role_dispatch() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".multiverse")).unwrap();
        std::fs::write(
            project.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-builder-aaaaaa\t\
             role=builder\tagent=mv-builder-1-se\tbranch=main\n",
        )
        .unwrap();
        let names = nexus_agent_names(&[project.path().to_path_buf()]);
        assert!(!names.contains("mv-builder-1-se"));
    }

    /// A reissued agent name (the Nexus pane under it ended, and the same
    /// name was later given to a worker dispatch) must read by its *newest*
    /// dispatch only — an older Nexus record must never outvote the
    /// worker dispatch that reused the name.
    #[test]
    fn nexus_agent_names_keeps_only_the_newest_dispatch_for_a_reissued_name() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".multiverse")).unwrap();
        std::fs::write(
            project.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-nexus-aaaaaa\t\
             role=nexus\tagent=mv-shared-1-se\tbranch=main\n\
             2020-01-02T00:00:00+00:00\tevent=dispatch\ttask=20250102-builder-bbbbbb\t\
             role=builder\tagent=mv-shared-1-se\tbranch=main\n",
        )
        .unwrap();
        let names = nexus_agent_names(&[project.path().to_path_buf()]);
        assert!(
            !names.contains("mv-shared-1-se"),
            "the later worker dispatch must win over the earlier Nexus one"
        );
    }

    #[test]
    fn seat_open_counts_from_payload_splits_worker_and_nexus_panes() {
        let payload = serde_json::json!({"result": {"agents": [
            {"name": "mv-nexus-1-se", "agent_status": "working"},
            {"name": "mv-builder-2-se", "agent_status": "working"},
            {"name": "mv-critic-1-cx", "agent_status": "working"}
        ]}});
        let mut nexus_agents = HashSet::new();
        nexus_agents.insert("mv-nexus-1-se".to_string());
        let counts = seat_open_counts_from_payload(&payload, "se", &nexus_agents).unwrap();
        assert_eq!(
            counts,
            SeatOpenCounts {
                workers: 1,
                nexus: 1
            }
        );
        let other = seat_open_counts_from_payload(&payload, "cx", &nexus_agents).unwrap();
        assert_eq!(
            other,
            SeatOpenCounts {
                workers: 1,
                nexus: 0
            }
        );
    }

    #[test]
    fn set_prefer_round_trips_without_disturbing_cap() {
        let dir = tempfile::tempdir().unwrap();
        set_cap(dir.path(), "seat-a", Some(2)).unwrap();
        set_prefer(dir.path(), "seat-a", true).unwrap();
        assert!(is_preferred(dir.path(), "seat-a"));
        assert_eq!(get_cap(dir.path(), "seat-a"), Some(2));
        set_prefer(dir.path(), "seat-a", false).unwrap();
        assert!(!is_preferred(dir.path(), "seat-a"));
    }
}
