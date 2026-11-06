//! Read-only Docker reporting: labelled Multiverse workloads for `workers`
//! and `stop`, plus a separate host-wide advisory report for every `sweep` pass.
//!
//! This module never starts, stops, removes, or prunes containers. Ownership is
//! read from labels, never inferred from an image or a matching name prefix.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use mv_herdr::argv::HerdrCommand;
use mv_herdr::error::HerdrError;
use serde_json::Value;

pub const OWNER_LABEL: &str = "multiverse.owner";
pub const TEMP_LABEL: &str = "multiverse.temp";

#[derive(Debug, thiserror::Error)]
pub enum DockerReportError {
    #[error("Docker report probe failed: {0}")]
    Probe(#[from] HerdrError),
    #[error("Docker report probe exited with status {0}")]
    Exit(i32),
    #[error("Docker report line {line}: {message}")]
    Malformed { line: usize, message: String },
}

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
    };
    let output = mv_herdr::run::run(&command, Some(timeout))?;
    if output.returncode != 0 {
        return Err(DockerReportError::Exit(output.returncode));
    }
    parse_ps(&output.stdout)
}

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
    };
    let result = mv_herdr::run::run(&command, Some(DEFAULT_TIMEOUT)).ok()?;
    (result.returncode == 0).then_some(result.stdout)
}

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
