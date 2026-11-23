//! Read-only Docker reporting: labelled Multiverse workloads for `workers`
//! and `stop`, plus a separate host-wide advisory report for every `sweep` pass.
//!
//! This module never starts, stops, removes, or prunes containers. Ownership is
//! read from labels, never inferred from an image or a matching name prefix.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use mv_herdr::argv::HerdrCommand;
use mv_herdr::error::HerdrError;
use serde_json::Value;

/// Docker label holding a workload's `project/task` owner, e.g.
/// `multiverse.owner=demo/20261102-builder-abcdef`.
pub const OWNER_LABEL: &str = "multiverse.owner";
/// Docker label marking a workload as one this module may ever report on
/// for cleanup purposes; absent or not `"1"` means "not ours to touch".
pub const TEMP_LABEL: &str = "multiverse.temp";

/// Why a labelled-workload probe or parse failed.
#[derive(Debug, thiserror::Error)]
pub enum DockerReportError {
    /// The `docker ps` subprocess itself could not be run.
    #[error("Docker report probe failed: {0}")]
    Probe(#[from] HerdrError),
    /// `docker ps` ran but returned a non-zero exit status.
    #[error("Docker report probe exited with status {0}")]
    Exit(i32),
    /// A line of `docker ps` output was not the JSON object this module
    /// expects.
    #[error("Docker report line {line}: {message}")]
    Malformed { line: usize, message: String },
}

/// One row from `docker ps`, kept only when it carries [`OWNER_LABEL`] or
/// [`TEMP_LABEL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub owner: Option<String>,
    pub temporary: bool,
}

impl Container {
    /// Full project/task ownership, including Nexus one-offs whose task is
    /// `orch`. This is attribution, never proof that cleanup is safe.
    pub fn owner_parts(&self) -> Option<(&str, &str)> {
        let (project, task) = self.owner.as_deref()?.split_once('/')?;
        (crate::directives::valid_project_name(project) && mv_core::id::is_valid_identifier(task))
            .then_some((project, task))
    }
}

/// Labelled Multiverse workloads found by one `docker ps` probe, for
/// `workers` and `stop`. Not the host-wide advisory scan — see
/// [`HostReport`] for that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockerReport {
    pub containers: Vec<Container>,
    pub warnings: Vec<String>,
}

/// Default bound for the synchronous probe subprocess. Kept short because the
/// production call site runs it on every report request.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Explicit executable seam for isolated probes and tests. The runner supplies
/// capture, no-shell argv execution, and a bounded synchronous timeout.
pub fn collect_with(binary: &Path) -> Result<DockerReport, DockerReportError> {
    collect_with_timeout(binary, DEFAULT_TIMEOUT)
}

/// Same as [`collect_with`], but with an explicit timeout bound. Lets a test
/// that only exercises a trivial fake binary use a longer bound than
/// production, so host contention doesn't turn the fake's own scheduling
/// delay into a probe timeout, without loosening the real default.
pub fn collect_with_timeout(
    binary: &Path,
    timeout: Duration,
) -> Result<DockerReport, DockerReportError> {
    let command = HerdrCommand {
        argv: vec![
            binary.to_string_lossy().into_owned(),
            "ps".into(),
            "--all".into(),
            "--no-trunc".into(),
            "--format".into(),
            "{{json .}}".into(),
        ],
        purpose: "report Multiverse Docker workloads".into(),
        stdin: None,
        expects_json: false, // Docker emits JSON lines, not a Herdr envelope.
        is_herdr: false,
    };
    let output = mv_herdr::run::run(&command, Some(timeout))?;
    if output.returncode != 0 {
        return Err(DockerReportError::Exit(output.returncode));
    }
    parse_ps(&output.stdout)
}

/// Production entry point: probes the real `docker` binary on `PATH`.
pub fn collect() -> Result<DockerReport, DockerReportError> {
    collect_with(Path::new("docker"))
}

/// Parse `docker ps --all --no-trunc --format '{{json .}}'`. Unknown Docker
/// fields are ignored. A failed/malformed probe never becomes an empty report.
pub fn parse_ps(text: &str) -> Result<DockerReport, DockerReportError> {
    let mut report = DockerReport::default();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let malformed = |message| DockerReportError::Malformed {
            line: index + 1,
            message,
        };
        let row: Value =
            serde_json::from_str(line).map_err(|_| malformed("invalid JSON object".into()))?;
        let row = row
            .as_object()
            .ok_or_else(|| malformed("expected an object".into()))?;
        let labels = match row.get("Labels") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::String(labels)) => {
                let mut result = BTreeMap::new();
                for label in labels.split(',').filter(|s| !s.is_empty()) {
                    let (key, value) = label.split_once('=').unwrap_or((label, ""));
                    result.insert(key.to_string(), value.to_string());
                }
                result
            }
            Some(Value::Object(labels)) => labels
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.into()))
                        .ok_or_else(|| malformed("label values must be strings".into()))
                })
                .collect::<Result<BTreeMap<_, String>, _>>()?,
            Some(_) => return Err(malformed("Labels must be a string or object".into())),
        };
        let owner = labels.get(OWNER_LABEL).cloned();
        let temporary = labels.get(TEMP_LABEL).is_some_and(|value| value == "1");
        if owner.is_none() && !temporary {
            continue;
        }
        let field = |name: &str| -> Result<String, DockerReportError> {
            row.get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| malformed(format!("missing or invalid {name}")))
        };
        let container = Container {
            id: field("ID")?,
            name: field("Names")?,
            image: field("Image")?,
            status: field("Status")?,
            owner,
            temporary,
        };
        if container.owner_parts().is_none() {
            report.warnings.push(format!(
                "{}: temporary workload has no valid project/task owner label",
                clean(&container.name)
            ));
        }
        if !temporary {
            report.warnings.push(format!(
                "{}: owned workload has no multiverse.temp=1 label",
                clean(&container.name)
            ));
        }
        report.containers.push(container);
    }
    report
        .containers
        .sort_by(|a, b| (&a.owner, &a.name, &a.id).cmp(&(&b.owner, &b.name, &b.id)));
    report.warnings.sort();
    Ok(report)
}

fn clean(value: &str) -> String {
    value.replace(['\r', '\n', '\t'], " ")
}

/// Tab-separated listing of `report`'s containers and warnings, for direct
/// display by `workers`/`stop`.
pub fn render(report: &DockerReport) -> String {
    if report.containers.is_empty() {
        return "No Multiverse Docker workloads.\n".into();
    }
    let mut text = String::from("OWNER\tNAME\tIMAGE\tSTATUS\tTEMP\n");
    for container in &report.containers {
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            clean(container.owner.as_deref().unwrap_or("(unowned)")),
            clean(&container.name),
            clean(&container.image),
            clean(&container.status),
            if container.temporary { "yes" } else { "no" }
        ));
    }
    for warning in &report.warnings {
        text.push_str(&format!("Warning: {}\n", clean(warning)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_not_names_determine_attribution_and_order() {
        let data = concat!(
            "{\"ID\":\"production\",\"Names\":\"mvt-demo-abcdef-test\",\"Labels\":\"\"}\n",
            "{\"ID\":\"b\",\"Names\":\"mvt-demo-abcdef-test\",\"Image\":\"test:1\",\"Status\":\"Exited (0)\",\"Labels\":\"multiverse.owner=demo/20261102-builder-abcdef,multiverse.temp=1,extra=a=b\"}\n",
            "{\"ID\":\"a\",\"Names\":\"one-off\",\"Image\":\"test:2\",\"Status\":\"Up 2 minutes\",\"Labels\":{\"multiverse.owner\":\"demo/orch\",\"multiverse.temp\":\"1\"}}\n",
        );
        let report = parse_ps(data).unwrap();
        assert_eq!(report.containers.len(), 2);
        assert!(report.warnings.is_empty());
        assert_eq!(
            report.containers[0].owner_parts(),
            Some(("demo", "20261102-builder-abcdef"))
        );
        assert!(report.containers.iter().all(|c| c.id != "production"));
        assert!(render(&report).contains("Exited (0)"));
    }

    #[test]
    fn malformed_probe_is_not_no_workloads_and_unsafe_ownership_warns() {
        assert_eq!(
            render(&parse_ps("").unwrap()),
            "No Multiverse Docker workloads.\n"
        );
        for text in [
            "not JSON",
            "[]",
            "{\"Labels\":42}",
            "{\"Labels\":\"multiverse.temp=1\"}",
        ] {
            assert!(parse_ps(text).is_err());
        }
        let report = parse_ps("{\"ID\":\"x\",\"Names\":\"unowned\",\"Image\":\"test:1\",\"Status\":\"Up\",\"Labels\":\"multiverse.temp=1,multiverse.owner=demo/../escape\"}").unwrap();
        assert!(report.containers[0].owner_parts().is_none());
        assert_eq!(report.warnings.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_probe_uses_only_read_argv_and_propagates_failure() {
        use crate::directives::tests::TempDir;
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let binary = dir.0.join("fake-docker");
        std::fs::write(&binary, "#!/bin/sh\n[ \"$#\" -eq 5 ] || exit 9\n[ \"$1\" = ps ] || exit 9\n[ \"$2\" = --all ] || exit 9\n[ \"$3\" = --no-trunc ] || exit 9\n[ \"$4\" = --format ] || exit 9\n[ \"$5\" = '{{json .}}' ] || exit 9\nprintf '%s\\n' '{\"ID\":\"fake\",\"Names\":\"test\",\"Image\":\"test:1\",\"Status\":\"Up\",\"Labels\":\"multiverse.owner=demo/orch,multiverse.temp=1\"}'\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A generous bound: this only runs a trivial echo-only fake binary, so
        // the real 10s production default stays tight while this test tolerates
        // scheduling delay under host contention instead of flaking on it.
        let timeout = Duration::from_secs(60);
        assert_eq!(
            collect_with_timeout(&binary, timeout)
                .unwrap()
                .containers
                .len(),
            1
        );
        std::fs::write(&binary, "#!/bin/sh\nexit 7\n").unwrap();
        assert!(matches!(
            collect_with_timeout(&binary, timeout),
            Err(DockerReportError::Exit(7))
        ));
        assert!(collect_with_timeout(&dir.0.join("missing"), timeout).is_err());
    }
}

/// Host-wide advisory scan, independent of the labelled workload inventory
/// used by `workers` and `stop`. No cleanup operation is ever issued here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostReport {
    pub lines: Vec<String>,
    pub reclaimable: Vec<String>,
}

/// A bounded, read-only subprocess seam shared by the host disk report.
/// Overrides allow isolated CLI rehearsals without changing PATH.
pub fn host_probe(program: &str, args: &[&str]) -> Option<String> {
    let key = format!("MULTIVERSE_{}_BIN", program.to_ascii_uppercase());
    let binary = std::env::var(&key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| program.to_owned());
    let command = HerdrCommand {
        argv: std::iter::once(binary)
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect(),
        purpose: format!("read host {program} report"),
        stdin: None,
        expects_json: false,
        is_herdr: false,
    };
    let result = mv_herdr::run::run(&command, Some(DEFAULT_TIMEOUT)).ok()?;
    (result.returncode == 0).then_some(result.stdout)
}

/// Production entry point for the host-wide advisory scan: probes the real
/// `docker` binary on `PATH` via [`host_probe`].
pub fn report() -> HostReport {
    report_with(&mut host_probe)
}

/// Sweep also accepts Python's existing hub cache. Reading it stays
/// advisory: expired, future, and incomplete records fall back to a probe.
/// Rendering uses this pass's clock and task owners, even on a cache hit.
pub fn report_for_sweep_with(
    config: &mv_config::load::Config,
    now: time::OffsetDateTime,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> HostReport {
    let owners = config
        .projects
        .values()
        .filter_map(|project| {
            project
                .path
                .value()
                .filter(|root| !root.is_empty())
                .map(|root| (project.name.as_str(), Path::new(root)))
        })
        .flat_map(|(name, root)| {
            mv_state::tasks::sweep_tasks(root)
                .into_iter()
                .filter(|task| !task.recorded)
                .map(move |task| format!("{name}/{}", task.task_id))
        })
        .collect();
    if let Some(root) = config
        .projects
        .get("multiverse")
        .and_then(|project| project.path.value())
        .filter(|root| !root.is_empty())
    {
        if let Some(report) = cached_report(Path::new(root), now, &owners) {
            return report;
        }
    }
    report_with_context(now, Some(&owners), probe)
}

fn cached_report(
    root: &Path,
    now: time::OffsetDateTime,
    owners: &std::collections::BTreeSet<String>,
) -> Option<HostReport> {
    let text = std::fs::read_to_string(root.join(".multiverse/dockerreport-cache.json")).ok()?;
    let facts: Value = serde_json::from_str(&text).ok()?;
    let when = mv_core::time::parse_flexible(facts.get("when")?.as_str()?).ok()?;
    if now < when || now >= when + time::Duration::minutes(30) {
        return None;
    }
    if !facts.get("available")?.as_bool()? {
        return Some(HostReport {
            lines: vec!["docker: unavailable".into()],
            ..HostReport::default()
        });
    }
    let containers = facts.get("containers")?.as_array()?;
    if !containers.iter().all(|row| {
        row.is_object()
            && ["Names", "State", "CreatedAt", "Labels", "Mounts"]
                .iter()
                .all(|field| row.get(*field).is_none_or(Value::is_string))
    }) {
        return None;
    }
    let mut lines = container_lines(containers, now, Some(owners));
    for key in ["images", "volumes"] {
        for line in facts.get(key)?.as_array()? {
            lines.push(line.as_str()?.to_owned());
        }
    }
    if let Some(summary) = facts.get("summary").filter(|value| !value.is_null()) {
        lines.push(summary.as_str()?.to_owned());
    }
    let age = mv_state::tasks::format_elapsed(now - when);
    Some(HostReport {
        lines: lines
            .into_iter()
            .map(|line| format!("{line} (as of {age} ago)"))
            .collect(),
        ..HostReport::default()
    })
}

/// Fresh host facts rendered with Python's sweep wording and silence rules.
/// A failed core probe collapses the whole report; volume failures preserve
/// the core facts and name only that unavailable section.
pub fn report_with(probe: &mut impl FnMut(&str, &[&str]) -> Option<String>) -> HostReport {
    report_with_context(mv_core::time::now_utc(), None, probe)
}

/// Same as [`report_with`], but with an explicit clock and the set of
/// task owners whose temp containers are still in progress (so
/// `container_lines` doesn't flag them as stale leftovers). `None` means
/// "no in-progress context" — every temp container with an owner is then
/// eligible to be reported stale.
pub fn report_with_context(
    now: time::OffsetDateTime,
    in_progress_owners: Option<&std::collections::BTreeSet<String>>,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> HostReport {
    let unavailable = || HostReport {
        lines: vec!["docker: unavailable".into()],
        ..HostReport::default()
    };
    let Some(containers) = probe(
        "docker",
        &["ps", "-a", "--no-trunc", "--format", "{{json .}}"],
    )
    .map(|text| host_rows(&text)) else {
        return unavailable();
    };
    let Some(images) =
        probe("docker", &["images", "--format", "{{json .}}"]).map(|text| host_rows(&text))
    else {
        return unavailable();
    };
    let mut lines = container_lines(&containers, now, in_progress_owners);
    let mut ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in images {
        let repo = host_field(&row, "Repository");
        let id = host_field(&row, "ID");
        if repo != "<none>" && !id.is_empty() {
            let kept = ids.entry(repo).or_default();
            if !kept.contains(&id) {
                kept.push(id);
            }
        }
    }
    for (repo, ids) in ids.into_iter().filter(|(_, ids)| ids.len() > 2) {
        let mut args = vec!["image", "inspect", "--format", "{{json .RepoDigests}}"];
        args.extend(ids.iter().map(String::as_str));
        let Some(text) = probe("docker", &args) else {
            return unavailable();
        };
        let digests: Vec<Value> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        if !digests.is_empty()
            && !digests
                .iter()
                .any(|value| value.as_array().is_some_and(|values| !values.is_empty()))
        {
            lines.push(format!(
                "docker: {repo} keeps {} images (deployed + previous is the rule)",
                ids.len()
            ));
        }
    }
    let Some(usage) =
        probe("docker", &["system", "df", "--format", "{{json .}}"]).map(|text| host_rows(&text))
    else {
        return unavailable();
    };
    let find = |kind: &str| {
        usage
            .iter()
            .rev()
            .find(|row| host_field(row, "Type") == kind)
    };
    let images = find("Images");
    let cache = find("Build Cache");
    let volumes = usage.iter().rev().find(|row| {
        matches!(
            host_field(row, "Type").as_str(),
            "Volumes" | "Local Volumes"
        )
    });
    let field = |row: Option<&Value>, key| {
        row.map(|row| host_field(row, key))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".into())
    };
    let mut summary = Vec::new();
    if images
        .and_then(|row| row.get("Reclaimable"))
        .is_some_and(|v| !v.is_null())
        || cache
            .and_then(|row| row.get("Reclaimable"))
            .is_some_and(|v| !v.is_null())
    {
        summary.push(format!(
            "images {} reclaimable",
            field(images, "Reclaimable")
        ));
        summary.push(format!(
            "build cache {} reclaimable",
            field(cache, "Reclaimable")
        ));
    }
    if volumes.is_some() {
        summary.push(format!(
            "volumes {} total, {} reclaimable",
            field(volumes, "Size"),
            field(volumes, "Reclaimable")
        ));
    }
    if let Some(text) = probe("docker", &["volume", "ls", "--format", "{{json .}}"]) {
        let mut mounts: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
        for row in &containers {
            let labels = host_labels(row);
            let project = labels
                .get("com.docker.compose.project")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    labels
                        .get(OWNER_LABEL)
                        .map(|owner| owner.split('/').next().unwrap_or("").to_owned())
                        .unwrap_or_default()
                });
            if !project.is_empty() {
                for mount in host_field(row, "Mounts")
                    .split(',')
                    .filter(|mount| !mount.is_empty())
                {
                    mounts
                        .entry(mount.into())
                        .or_default()
                        .insert(project.clone());
                }
            }
        }
        let mut volume_lines = Vec::new();
        for row in host_rows(&text) {
            let name = host_field(&row, "Name");
            let labels = host_labels(&row);
            let attribution = if let Some(project) = labels
                .get("com.docker.compose.project")
                .filter(|s| !s.is_empty())
            {
                format!("project={project} (compose label)")
            } else if let Some(projects) = mounts.get(&name) {
                format!(
                    "project={} (via mount)",
                    projects.iter().cloned().collect::<Vec<_>>().join(",")
                )
            } else {
                "unattributed".into()
            };
            volume_lines.push(format!("docker: volume {name} {attribution}"));
        }
        volume_lines.sort();
        lines.extend(volume_lines);
    } else {
        lines.push("docker: volumes unavailable".into());
    }
    if !summary.is_empty() {
        lines.push(format!("docker: {}", summary.join(", ")));
    }
    HostReport {
        lines,
        ..HostReport::default()
    }
}

fn host_rows(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
        .collect()
}

fn host_field(row: &Value, key: &str) -> String {
    clean(row.get(key).and_then(Value::as_str).unwrap_or(""))
}

fn host_labels(row: &Value) -> BTreeMap<String, String> {
    host_field(row, "Labels")
        .split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (key.to_owned(), value.to_owned())
        })
        .collect()
}

fn docker_timestamp(value: &str) -> Option<time::OffsetDateTime> {
    let (base, _) = value.rsplit_once(' ')?;
    let (date, offset) = base.rsplit_once(' ')?;
    if offset.len() != 5 || !offset.is_ascii() {
        return None;
    }
    mv_core::time::parse_flexible(&format!(
        "{}{}:{}",
        date.replacen(' ', "T", 1),
        &offset[..3],
        &offset[3..]
    ))
    .ok()
}

fn container_lines(
    rows: &[Value],
    now: time::OffsetDateTime,
    in_progress_owners: Option<&std::collections::BTreeSet<String>>,
) -> Vec<String> {
    // Fixed render order, not discovery order: 0=stopped, 1=loose running,
    // 2=random-named unlabeled, 3=stale temp. Each row lands in at most one
    // group; within a group, rows are sorted by name below.
    let mut groups: [Vec<(String, String)>; 4] = std::array::from_fn(|_| Vec::new());
    for row in rows {
        let name = host_field(row, "Names");
        let labels = host_labels(row);
        if labels.contains_key("com.docker.compose.project") {
            continue;
        }
        let owner = labels.get(OWNER_LABEL);
        let suffix = owner
            .map(|owner| format!(" owner={owner}"))
            .unwrap_or_else(|| " unlabeled".into());
        let created = docker_timestamp(&host_field(row, "CreatedAt"));
        let age = created
            .map(|created| mv_state::tasks::format_elapsed(now - created))
            .unwrap_or_else(|| "unknown age".into());
        let (group, line) = if labels.get(TEMP_LABEL).is_some_and(|s| s == "1")
            && owner.is_some()
            && in_progress_owners.is_some()
        {
            if in_progress_owners.is_some_and(|owners| owners.contains(owner.unwrap())) {
                continue;
            }
            (
                3,
                format!("docker: stale temp container {name} ({age}){suffix}"),
            )
        } else {
            match host_field(row, "State").to_ascii_lowercase().as_str() {
                "exited" | "created" => (
                    0,
                    format!("docker: stopped container {name} ({age}){suffix}"),
                ),
                "running" => {
                    let Some(created) = created else { continue };
                    let elapsed = now - created;
                    let random = name.split_once('_').is_some_and(|(a, b)| {
                        !a.is_empty()
                            && !b.is_empty()
                            && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_lowercase())
                    });
                    if owner.is_none() && random {
                        (
                            2,
                            format!("docker: random-named container {name} up {age} unlabeled"),
                        )
                    } else if elapsed >= time::Duration::hours(1) {
                        (
                            1,
                            format!("docker: loose container {name} up {age}{suffix}"),
                        )
                    } else {
                        continue;
                    }
                }
                _ => continue,
            }
        };
        groups[group].push((name, line));
    }
    groups
        .into_iter()
        .flat_map(|mut group| {
            group.sort_by(|a, b| a.0.cmp(&b.0));
            group.into_iter().map(|(_, line)| line)
        })
        .collect()
}

#[cfg(test)]
mod host_tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    fn now() -> time::OffsetDateTime {
        mv_core::time::parse_flexible("2026-11-06T12:00:00Z").unwrap()
    }

    fn container(name: &str, state: &str, labels: &str, created: &str) -> Value {
        json!({"Names": name, "State": state, "Labels": labels, "CreatedAt": created})
    }

    #[test]
    fn containers_match_python_order_wording_and_silence_rules() {
        let old = "2026-11-06 10:00:00 +0000 UTC";
        let recent = "2026-11-06 11:59:30 +0000 UTC";
        let owners = BTreeSet::from(["demo/live".to_owned()]);
        let rows = vec![
            container("z-stop", "exited", "", old),
            container(
                "a-stop",
                "created",
                "multiverse.owner=demo/task",
                "bad date",
            ),
            container("loose", "running", "multiverse.owner=demo/orch", old),
            container("happy_otter", "running", "", recent),
            container(
                "temp",
                "running",
                "multiverse.owner=demo/finished,multiverse.temp=1",
                recent,
            ),
            container(
                "live",
                "exited",
                "multiverse.owner=demo/live,multiverse.temp=1",
                old,
            ),
            container("compose", "exited", "com.docker.compose.project=demo", old),
            container("young", "running", "", recent),
            container("unknown", "running", "", "bad date"),
            container("dead", "dead", "", old),
        ];
        assert_eq!(
            container_lines(&rows, now(), Some(&owners)),
            [
                "docker: stopped container a-stop (unknown age) owner=demo/task",
                "docker: stopped container z-stop (2 hr) unlabeled",
                "docker: loose container loose up 2 hr owner=demo/orch",
                "docker: random-named container happy_otter up 30 sec unlabeled",
                "docker: stale temp container temp (30 sec) owner=demo/finished",
            ]
        );
        assert!(docker_timestamp("bad").is_none());
        assert_eq!(
            docker_timestamp("2026-11-06 04:00:00 -0800 PST"),
            Some(now())
        );
        assert_eq!(
            container_lines(
                &[container(
                    "future",
                    "exited",
                    "",
                    "2026-11-06 13:00:00 +0000 UTC"
                )],
                now(),
                None
            ),
            ["docker: stopped container future (0 sec) unlabeled"]
        );
        assert_eq!(
            container_lines(
                &[container(
                    "temp",
                    "exited",
                    "multiverse.owner=demo/live,multiverse.temp=1",
                    old
                )],
                now(),
                None
            ),
            ["docker: stopped container temp (2 hr) owner=demo/live"]
        );
    }

    #[test]
    fn host_scan_reports_only_excess_local_images_attributed_volumes_and_one_summary() {
        let mut calls = Vec::new();
        let report = report_with_context(now(), None, &mut |program, args| {
            assert_eq!(program, "docker");
            calls.push(args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>());
            Some(match args {
                ["ps", "-a", "--no-trunc", "--format", "{{json .}}"] => concat!(
                    "{\"Names\":\"compose\",\"State\":\"running\",\"Labels\":\"com.docker.compose.project=demo\",\"Mounts\":\"shared\"}\n",
                    "{\"Names\":\"owned\",\"Labels\":\"multiverse.owner=other/orch\",\"Mounts\":\"shared\"}\n",
                ).into(),
                ["images", "--format", "{{json .}}"] => {
                    let mut rows = Vec::new();
                    for (repo, ids) in [("local", vec!["a", "b", "c", "c"]), ("pulled", vec!["d", "e", "f"]), ("normal", vec!["x", "y"]), ("<none>", vec!["g", "h", "i"])] {
                        for id in ids { rows.push(json!({"Repository": repo, "ID": id}).to_string()); }
                    }
                    rows.join("\n")
                }
                ["image", "inspect", "--format", "{{json .RepoDigests}}", "a", "b", "c"] => "[]\n[]\n[]\n".into(),
                ["image", "inspect", "--format", "{{json .RepoDigests}}", "d", "e", "f"] => "[\"repo@sha256:d\"]\n".into(),
                ["system", "df", "--format", "{{json .}}"] => concat!(
                    "{\"Type\":\"Images\",\"Reclaimable\":\"2GB (66%)\"}\n",
                    "{\"Type\":\"Build Cache\",\"Reclaimable\":\"4GB (100%)\"}\n",
                    "{\"Type\":\"Local Volumes\",\"Size\":\"3GB\",\"Reclaimable\":\"1GB\"}\n",
                    "{\"Type\":\"Containers\",\"Size\":\"8GB\",\"Reclaimable\":\"7GB\"}\n",
                ).into(),
                ["volume", "ls", "--format", "{{json .}}"] => concat!(
                    "{\"Name\":\"shared\"}\n",
                    "{\"Name\":\"orphan\"}\n",
                    "{\"Name\":\"labeled\",\"Labels\":\"com.docker.compose.project=label\"}\n",
                ).into(),
                _ => panic!("unexpected or mutating Docker probe: {args:?}"),
            })
        });
        assert_eq!(calls.len(), 6);
        assert_eq!(report.lines, [
            "docker: local keeps 3 images (deployed + previous is the rule)",
            "docker: volume labeled project=label (compose label)",
            "docker: volume orphan unattributed",
            "docker: volume shared project=demo,other (via mount)",
            "docker: images 2GB (66%) reclaimable, build cache 4GB (100%) reclaimable, volumes 3GB total, 1GB reclaimable",
        ]);
        assert!(report.reclaimable.is_empty());
    }

    #[test]
    fn core_failures_collapse_but_volume_failure_preserves_core_facts() {
        for failing in ["ps", "images", "system"] {
            let report = report_with(&mut |_, args| (args[0] != failing).then(String::new));
            assert_eq!(report.lines, ["docker: unavailable"]);
        }
        let report = report_with(&mut |_, args| match args[0] {
            "volume" => None,
            "system" => Some("{\"Type\":\"Build Cache\",\"Reclaimable\":\"1GB\"}".into()),
            _ => Some(String::new()),
        });
        assert_eq!(
            report.lines,
            [
                "docker: volumes unavailable",
                "docker: images unknown reclaimable, build cache 1GB reclaimable"
            ]
        );
        assert_eq!(
            report_with(&mut |_, _| Some("not JSON\n[]\n{}\n".into())).lines,
            ["docker: volume  unattributed"]
        );
        assert!(report_with(&mut |_, _| Some("not JSON\n[]\n".into()))
            .lines
            .is_empty());
    }

    #[test]
    fn cached_facts_render_fresh_ages_and_expire_at_the_python_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".multiverse/dockerreport-cache.json");
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        let facts = json!({
            "when": "2026-11-06T11:50:00+00:00", "available": true,
            "containers": [container("old", "exited", "", "2026-11-06 10:00:00 +0000 UTC")],
            "images": [], "volumes": [], "summary": "docker: images 1GB reclaimable, build cache unknown reclaimable",
        });
        std::fs::write(&path, facts.to_string()).unwrap();
        assert_eq!(cached_report(dir.path(), now(), &BTreeSet::new()).unwrap().lines, [
            "docker: stopped container old (2 hr) unlabeled (as of 10 min ago)",
            "docker: images 1GB reclaimable, build cache unknown reclaimable (as of 10 min ago)",
        ]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), facts.to_string());
        assert!(cached_report(
            dir.path(),
            now() + time::Duration::minutes(20),
            &BTreeSet::new()
        )
        .is_none());
        assert!(cached_report(
            dir.path(),
            now() - time::Duration::minutes(11),
            &BTreeSet::new()
        )
        .is_none());
        let mut partial = facts.clone();
        partial.as_object_mut().unwrap().remove("volumes");
        std::fs::write(&path, partial.to_string()).unwrap();
        assert!(cached_report(dir.path(), now(), &BTreeSet::new()).is_none());
        partial["volumes"] = json!([]);
        partial["containers"][0]["Names"] = json!(42);
        std::fs::write(&path, partial.to_string()).unwrap();
        assert!(cached_report(dir.path(), now(), &BTreeSet::new()).is_none());
        std::fs::write(
            &path,
            json!({"when":"2026-11-06T11:50:00Z", "available":false}).to_string(),
        )
        .unwrap();
        assert_eq!(
            cached_report(dir.path(), now(), &BTreeSet::new())
                .unwrap()
                .lines,
            ["docker: unavailable"]
        );
    }

    #[test]
    fn failed_image_inspect_does_not_publish_a_partial_report() {
        assert_eq!(
            report_with(&mut |_, args| match args[0] {
                "images" => Some(
                    ["a", "b", "c"]
                        .map(|id| json!({"Repository":"local", "ID":id}).to_string())
                        .join("\n")
                ),
                "image" => None,
                _ => Some(String::new()),
            })
            .lines,
            ["docker: unavailable"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_probe_captures_only_fake_docker_output() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("fake-docker");
        std::fs::write(&binary, "#!/bin/sh\n[ \"$1\" = ps ] || exit 9\nprintf '%s\\n' '{\"Names\":\"fake\",\"State\":\"exited\"}'\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let text = host_probe(binary.to_str().unwrap(), &["ps"]).unwrap();
        assert_eq!(
            container_lines(&host_rows(&text), now(), None),
            ["docker: stopped container fake (unknown age) unlabeled"]
        );
        std::fs::write(&binary, "#!/bin/sh\nexit 7\n").unwrap();
        assert!(host_probe(binary.to_str().unwrap(), &["ps"]).is_none());
    }
}

/// Card `20261115-b-9815fe`: one host process observed by the shared,
/// bounded cwd probe below — used both by `sweep`'s own closed-lane report
/// and the lane-exit reaper (`worktrees --prune --reap` /
/// `tasks --reclaimable --reap`). Finding a process here is read-only
/// evidence; nothing in this module signals it except [`reap`] itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCwd {
    pub pid: u32,
    pub cwd: PathBuf,
    pub command: String,
    pub cpu_percent: String,
}

/// Which host-cwd lookup a snapshot should use. Decided once, from `cfg!`,
/// at the real entry points ([`process_cwds`], [`reap`]) — but carried as an
/// explicit parameter through the `_with` functions below so `reap_tests`
/// can select [`HostOs::Linux`] on any host, driving that branch's fakes
/// (`linux_cwd`) without ever needing a real `/proc` entry or a Linux box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Macos,
    Linux,
    Other,
}

impl HostOs {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            HostOs::Macos
        } else if cfg!(target_os = "linux") {
            HostOs::Linux
        } else {
            HostOs::Other
        }
    }
}

/// Production Linux cwd lookup: the real `/proc/<pid>/cwd` symlink target.
/// Injectable via [`process_cwds_with`]'s `linux_cwd` seam, the same way
/// [`host_probe`] is injectable for `ps`/`lsof`, so a test can drive the
/// Linux branch with a fake instead of a real `/proc` entry.
fn proc_cwd_link(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// `ps`+`lsof` (macOS) / `/proc/<pid>/cwd` (Linux) snapshot of every host
/// process and its working directory, through the shared [`host_probe`]
/// subprocess seam already used for `sweep`'s own closed-lane report.
/// `None` means the probe itself failed (unsupported OS, subprocess
/// failure) — never read as "no processes". This process's own pid is
/// always excluded: a reaper invoked with its cwd inside the very lane it
/// is pruning must never find itself among the candidates.
pub fn process_cwds() -> Option<Vec<ProcessCwd>> {
    process_cwds_with(&mut host_probe, &mut proc_cwd_link, HostOs::current())
}

/// Same as [`process_cwds`], with injectable `ps`/`lsof` and `/proc/<pid>/cwd`
/// seams — the latter via `linux_cwd` — so [`reap`]'s own pid-identity
/// recheck can share this one implementation instead of duplicating the
/// parsing, and so a test can pick `os` independently of the host it runs
/// on. `None` on any probe failure or an unsupported OS — the probe gives
/// no further detail worth a dedicated error type here, and every caller
/// already treats this the same way [`process_cwds`] does: unknown, never
/// "no processes".
pub fn process_cwds_with(
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
    linux_cwd: &mut impl FnMut(u32) -> Option<PathBuf>,
    os: HostOs,
) -> Option<Vec<ProcessCwd>> {
    let stdout = probe("ps", &["-axo", "pid=,pcpu=,comm="])?;
    let mut metadata = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(cpu) = parts.next() else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        if command.is_empty() {
            continue;
        }
        metadata.insert(pid, (command, cpu.to_string()));
    }
    let mut cwds: HashMap<u32, PathBuf> = HashMap::new();
    match os {
        HostOs::Macos => {
            let stdout = probe("lsof", &["-a", "-d", "cwd", "-Fpcn"])?;
            let mut pid = None;
            for line in stdout.lines() {
                if let Some(rest) = line.strip_prefix('p') {
                    pid = rest.parse().ok();
                } else if let Some(rest) = line.strip_prefix("n/") {
                    if let Some(pid) = pid {
                        cwds.insert(pid, PathBuf::from(format!("/{rest}")));
                    }
                }
            }
        }
        HostOs::Linux => {
            for pid in metadata.keys().copied() {
                if let Some(cwd) = linux_cwd(pid) {
                    cwds.insert(pid, cwd);
                }
            }
        }
        HostOs::Other => return None,
    }
    let own_pid = std::process::id();
    let mut rows: Vec<ProcessCwd> = cwds
        .into_iter()
        .filter(|(pid, _)| *pid != own_pid)
        .filter_map(|(pid, cwd)| {
            let (command, cpu_percent) = metadata.get(&pid)?.clone();
            Some(ProcessCwd {
                pid,
                cwd,
                command,
                cpu_percent,
            })
        })
        .collect();
    rows.sort_by_key(|p| p.pid);
    Some(rows)
}

/// Processes from one [`process_cwds`] snapshot whose canonical cwd is at
/// or under `lane_root`'s own canonical path. Shared by `sweep`'s own
/// closed-lane matching (many lane roots, one snapshot) and the reaper
/// (one root): both ask exactly this question, and `Path::starts_with`
/// compares path components, not raw strings, so a sibling directory like
/// `<lane>-other` is never mistaken for a child of `<lane>`. Neither a
/// `lane_root` nor a process `cwd` that fails to canonicalize (already
/// removed, or a dead process whose `/proc` entry raced away) matches
/// anything.
pub fn processes_under(lane_root: &Path, processes: &[ProcessCwd]) -> Vec<ProcessCwd> {
    let Ok(root) = lane_root.canonicalize() else {
        return Vec::new();
    };
    processes
        .iter()
        .filter(|process| {
            process
                .cwd
                .canonicalize()
                .map(|cwd| cwd == root || cwd.starts_with(&root))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

/// A pid-less label for one process, shared by every caller that prints a
/// reap candidate: `sweep`'s own closed-lane report, and `--reap`'s own
/// "kept"/"would reap"/"reaped" lines on `worktrees --prune` and
/// `tasks --reclaimable`. Matches `sweep.rs`'s own pre-port formatting
/// (command trimmed to 160 chars, whitespace collapsed) so none of those
/// call sites changes its printed shape by switching to this one.
pub fn process_label(process: &ProcessCwd) -> String {
    let command: String = process
        .command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let command: String = command.chars().take(160).collect();
    format!("pid {} {command}", process.pid)
}

/// Decision 6 (card `20261115-b-9815fe`): never a reap target even when its
/// cwd matches a lane under prune/reclaim — a shell or Herdr itself by
/// name, pid 1, or any ancestor of this very process. A leaked `cargo
/// test` may share a shell's pgid with that shell; killing the shell by
/// name takes down whatever else still runs under it, and this process's
/// own ancestor chain is never a lane leftover no matter whose cwd it
/// reports.
const PROTECTED_COMMANDS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "ksh",
    "csh",
    "tcsh",
    "nu",
    "herdr",
    "herdr-server",
];

fn command_name(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

/// This process's own ancestor pids, immediate parent first, walked via
/// `ps -o ppid=` one hop at a time — the same subprocess seam [`host_probe`]
/// already uses, bounded to 64 hops so a cycle a stub test probe returns
/// (never a cycle a real `ps` would) cannot loop forever. Stops at the
/// first hop the probe cannot answer, or at pid 1.
pub fn own_ancestor_pids() -> Vec<u32> {
    own_ancestor_pids_with(std::process::id(), &mut host_probe)
}

/// Same as [`own_ancestor_pids`], with an injectable `ps` probe and a
/// starting pid, so a test can walk a fabricated process tree instead of
/// this process's real one.
pub fn own_ancestor_pids_with(
    mut pid: u32,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> Vec<u32> {
    let mut ancestors = Vec::new();
    for _ in 0..64 {
        let Some(out) = probe("ps", &["-o", "ppid=", "-p", &pid.to_string()]) else {
            break;
        };
        let Some(ppid) = out.trim().parse::<u32>().ok() else {
            break;
        };
        if ancestors.contains(&ppid) {
            break;
        }
        ancestors.push(ppid);
        if ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    ancestors
}

/// Card `20261123-b-ccc2cc`: one process's parent pid and full argument
/// list (`ps -o ppid=,command=`), which a [`process_cwds`] snapshot does
/// not carry — its `comm=` column names only the executable, so neither an
/// orphaned leftover (parent pid 1) nor an ssh ControlMaster's own `[mux]`
/// title is visible there. `None` when the pid is gone or `ps` fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessParent {
    pub ppid: u32,
    pub args: String,
}

/// [`ProcessParent`] for `pid` via the real `ps`.
pub fn process_parent(pid: u32) -> Option<ProcessParent> {
    process_parent_with(pid, &mut host_probe)
}

/// Same as [`process_parent`], with the same injectable `ps` probe
/// [`own_ancestor_pids_with`] takes.
pub fn process_parent_with(
    pid: u32,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> Option<ProcessParent> {
    let out = probe("ps", &["-o", "ppid=,command=", "-p", &pid.to_string()])?;
    let line = out.lines().map(str::trim).find(|line| !line.is_empty())?;
    let (ppid, args) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    Some(ProcessParent {
        ppid: ppid.parse().ok()?,
        args: args.trim().to_string(),
    })
}

/// Whether `process` must never be signaled — checked by every call site
/// before it invokes [`reap`], not by `reap` itself: `own_ancestor_pids` is
/// one `ps` walk a caller makes once per pass, not once per candidate.
pub fn protected(process: &ProcessCwd, ancestors: &[u32]) -> bool {
    process.pid == 1
        || PROTECTED_COMMANDS.contains(&command_name(&process.command))
        || ancestors.contains(&process.pid)
}

/// Default grace between a reaper's SIGTERM and its pid-identity recheck
/// before SIGKILL. Short on purpose: a lingering lane process is almost
/// always a leftover child (`sleep`, a background test run) with no
/// cleanup of its own to do, so a short wait catches the common
/// already-gone case without holding up `--prune`/`--reclaimable`
/// noticeably.
pub const DEFAULT_REAP_GRACE: Duration = Duration::from_secs(2);

/// What [`reap`] found once its grace window ended. Never a reason on its
/// own to assume a *lane* is now clean — a caller re-checks that lane's
/// own lingering-process list before deciding anything else followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReapOutcome {
    /// Gone by the time the grace window ended — SIGTERM alone was enough,
    /// or it had already exited before this call ran.
    Reaped,
    /// Still the same process (same command, same cwd) after the grace
    /// window, so SIGKILL was sent and the pid is now gone.
    Killed,
    /// Still the same process after SIGKILL too — signaling it failed
    /// (no permission, or the OS never delivered it).
    StillAlive,
    /// The pid answered after the grace window, but as a different
    /// process (different command or cwd): something else reused it
    /// during the wait. The original is already gone; left alone.
    IdentityChanged,
}

/// SIGTERM, wait `grace`, re-check the pid still identifies the same
/// process (command and cwd unchanged) before SIGKILL — the one function
/// in this module that signals anything. Wired to the real `kill`
/// subprocess and a real sleep; [`reap_with`] takes injectable seams for a
/// test that exercises the state machine without a real target process.
pub fn reap(target: &ProcessCwd, grace: Duration) -> ReapOutcome {
    reap_with(
        target,
        grace,
        &mut std::thread::sleep,
        &mut host_probe,
        &mut proc_cwd_link,
        HostOs::current(),
    )
}

/// Same as [`reap`], with injectable sleep, `ps`/`lsof`, and `/proc/<pid>/cwd`
/// seams — the last via `linux_cwd`, mirroring [`process_cwds_with`] — plus
/// an explicit `os`, so a test can drive the SIGTERM/grace/SIGKILL state
/// machine against a fake process on either platform's cwd lookup without
/// signaling anything real.
pub fn reap_with(
    target: &ProcessCwd,
    grace: Duration,
    sleep: &mut impl FnMut(Duration),
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
    linux_cwd: &mut impl FnMut(u32) -> Option<PathBuf>,
    os: HostOs,
) -> ReapOutcome {
    let pid = target.pid.to_string();
    if probe("kill", &["-TERM", &pid]).is_none() {
        // No such process (ESRCH) or no permission to signal it at all —
        // either way there is nothing left for this call to wait on.
        return ReapOutcome::Reaped;
    }
    sleep(grace);
    match process_cwds_with(probe, linux_cwd, os)
        .and_then(|processes| processes.into_iter().find(|p| p.pid == target.pid))
    {
        None => ReapOutcome::Reaped,
        Some(current) if current.command == target.command && current.cwd == target.cwd => {
            if probe("kill", &["-KILL", &pid]).is_some() {
                ReapOutcome::Killed
            } else {
                ReapOutcome::StillAlive
            }
        }
        Some(_) => ReapOutcome::IdentityChanged,
    }
}

#[cfg(test)]
mod reap_tests {
    use super::*;

    fn process(pid: u32) -> ProcessCwd {
        ProcessCwd {
            pid,
            cwd: PathBuf::from("/lane"),
            command: "sleep 300".into(),
            cpu_percent: "0.0".into(),
        }
    }

    #[test]
    fn processes_under_matches_only_real_descendants_never_a_sibling_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("lane");
        let sibling = dir.path().join("lane-other");
        std::fs::create_dir_all(lane.join("nested")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let processes = vec![
            ProcessCwd {
                pid: 1,
                cwd: lane.clone(),
                command: "a".into(),
                cpu_percent: "0".into(),
            },
            ProcessCwd {
                pid: 2,
                cwd: lane.join("nested"),
                command: "b".into(),
                cpu_percent: "0".into(),
            },
            ProcessCwd {
                pid: 3,
                cwd: sibling,
                command: "c".into(),
                cpu_percent: "0".into(),
            },
        ];
        let matched = processes_under(&lane, &processes);
        assert_eq!(
            matched.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn process_cwds_never_includes_this_process_own_pid() {
        // Card `20261116-b-acc032`: exercised with `os` forced to both
        // values so this test proves the Linux seam (`linux_cwd`) on any
        // host, not just whichever platform happens to run it.
        let own = std::process::id();
        for os in [HostOs::Macos, HostOs::Linux] {
            let mut probe = |program: &str, _args: &[&str]| -> Option<String> {
                match program {
                    "ps" => Some(format!("{own} 0.0 sleep\n4242 0.0 sleep\n")),
                    "lsof" => Some(format!("p{own}\nn/lane\np4242\nn/lane\n")),
                    _ => None,
                }
            };
            let mut linux_cwd = |pid: u32| -> Option<PathBuf> {
                (pid == own || pid == 4242).then(|| PathBuf::from("/lane"))
            };
            let rows = process_cwds_with(&mut probe, &mut linux_cwd, os).unwrap();
            assert_eq!(
                rows.iter().map(|p| p.pid).collect::<Vec<_>>(),
                vec![4242],
                "{os:?}"
            );
        }
    }

    #[test]
    fn reap_signals_term_only_once_when_the_process_is_already_gone() {
        let mut calls = Vec::new();
        let outcome = reap_with(
            &process(99),
            Duration::from_secs(0),
            &mut |_| {},
            &mut |program, args| {
                calls.push((
                    program.to_owned(),
                    args.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                ));
                None
            },
            &mut |_| None,
            HostOs::current(),
        );
        assert_eq!(outcome, ReapOutcome::Reaped);
        assert_eq!(
            calls,
            vec![("kill".to_owned(), vec!["-TERM".to_owned(), "99".to_owned()])]
        );
    }

    #[test]
    fn reap_waits_then_sigkills_a_process_still_alive_after_term() {
        // Forced to both `os` values: see
        // `process_cwds_never_includes_this_process_own_pid` above.
        for os in [HostOs::Macos, HostOs::Linux] {
            let target = process(99);
            let mut slept = Vec::new();
            let outcome = reap_with(
                &target,
                Duration::from_millis(5),
                &mut |d| slept.push(d),
                &mut |program, args| match (program, args) {
                    ("kill", ["-TERM", "99"]) => Some(String::new()),
                    ("kill", ["-KILL", "99"]) => Some(String::new()),
                    ("ps", _) => Some("99 1.0 sleep 300\n".into()),
                    ("lsof", _) => Some("p99\nn/lane\n".into()),
                    _ => None,
                },
                &mut |pid| (pid == 99).then(|| PathBuf::from("/lane")),
                os,
            );
            assert_eq!(outcome, ReapOutcome::Killed, "{os:?}");
            assert_eq!(slept, vec![Duration::from_millis(5)], "{os:?}");
        }
    }

    #[test]
    fn reap_leaves_a_pid_alone_once_it_identifies_a_different_process() {
        for os in [HostOs::Macos, HostOs::Linux] {
            let target = process(99);
            let outcome = reap_with(
                &target,
                Duration::from_millis(0),
                &mut |_| {},
                &mut |program, args| match (program, args) {
                    ("kill", ["-TERM", "99"]) => Some(String::new()),
                    ("ps", _) => Some("99 1.0 unrelated-daemon\n".into()),
                    ("lsof", _) => Some("p99\nn/elsewhere\n".into()),
                    _ => panic!("must never SIGKILL once identity no longer matches: {args:?}"),
                },
                &mut |pid| (pid == 99).then(|| PathBuf::from("/elsewhere")),
                os,
            );
            assert_eq!(outcome, ReapOutcome::IdentityChanged, "{os:?}");
        }
    }

    #[test]
    fn reap_reports_still_alive_when_sigkill_itself_fails() {
        for os in [HostOs::Macos, HostOs::Linux] {
            let target = process(99);
            let outcome = reap_with(
                &target,
                Duration::from_millis(0),
                &mut |_| {},
                &mut |program, args| match (program, args) {
                    ("kill", ["-TERM", "99"]) => Some(String::new()),
                    ("kill", ["-KILL", "99"]) => None,
                    ("ps", _) => Some("99 1.0 sleep 300\n".into()),
                    ("lsof", _) => Some("p99\nn/lane\n".into()),
                    _ => None,
                },
                &mut |pid| (pid == 99).then(|| PathBuf::from("/lane")),
                os,
            );
            assert_eq!(outcome, ReapOutcome::StillAlive, "{os:?}");
        }
    }

    #[test]
    fn protected_refuses_a_shell_or_herdr_by_name_even_with_a_matching_cwd() {
        let ancestors = Vec::new();
        for command in ["zsh", "/bin/bash", "herdr", "herdr-server -daemon"] {
            let candidate = ProcessCwd {
                pid: 55,
                cwd: PathBuf::from("/lane"),
                command: command.into(),
                cpu_percent: "0".into(),
            };
            assert!(protected(&candidate, &ancestors), "{command}");
        }
        assert!(!protected(&process(55), &ancestors));
    }

    #[test]
    fn protected_refuses_pid_1_and_this_process_own_ancestors() {
        assert!(protected(&process(1), &[]));
        assert!(protected(&process(42), &[42, 7]));
        assert!(!protected(&process(42), &[7]));
    }

    #[test]
    fn own_ancestor_pids_with_walks_ppid_to_pid_1_and_stops() {
        let ancestors = own_ancestor_pids_with(500, &mut |program, args| match (program, args) {
            ("ps", ["-o", "ppid=", "-p", "500"]) => Some("200\n".into()),
            ("ps", ["-o", "ppid=", "-p", "200"]) => Some("1\n".into()),
            _ => panic!("must stop walking past pid 1: {args:?}"),
        });
        assert_eq!(ancestors, vec![200, 1]);
    }

    #[test]
    fn process_parent_with_reads_the_parent_pid_and_full_argument_list() {
        let parent = process_parent_with(77, &mut |program, args| match (program, args) {
            ("ps", ["-o", "ppid=,command=", "-p", "77"]) => {
                Some("    1 ssh: /tmp/cm.sock [mux]\n".into())
            }
            _ => None,
        });
        assert_eq!(
            parent,
            Some(ProcessParent {
                ppid: 1,
                args: "ssh: /tmp/cm.sock [mux]".into(),
            })
        );
        assert_eq!(process_parent_with(77, &mut |_, _| None), None);
        assert_eq!(
            process_parent_with(77, &mut |_, _| Some("\n".into())),
            None,
            "a gone pid prints nothing"
        );
    }

    #[test]
    fn own_ancestor_pids_with_stops_on_an_unreadable_hop() {
        let ancestors = own_ancestor_pids_with(500, &mut |_, _| None);
        assert!(ancestors.is_empty());
    }

    #[test]
    fn process_label_trims_and_collapses_whitespace() {
        let candidate = ProcessCwd {
            pid: 4200,
            cwd: PathBuf::from("/lane"),
            command: "  cargo   test  --locked  ".into(),
            cpu_percent: "1.0".into(),
        };
        assert_eq!(process_label(&candidate), "pid 4200 cargo test --locked");
    }
}
