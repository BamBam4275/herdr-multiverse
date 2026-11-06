//! Host-wide disk-floor reporting for every `sweep` pass.
//!
//! Probes never reclaim anything. Low-space notices go to the configured
//! `multiverse` hub's project history, not fleet runtime state. The separate
//! notify episode/transport policy remains outside this advisory scan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use mv_config::load::Config;
use serde_json::{json, Map, Value};
use time::OffsetDateTime;

use crate::dockerreport::{self, HostReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Critical,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "LOW",
            Self::Critical => "CRITICAL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskUsage {
    pub filesystem: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl DiskUsage {
    pub fn level(&self, warn_gb: u32, critical_gb: u32) -> Level {
        if self.available_bytes < u64::from(critical_gb) * (1 << 30) {
            Level::Critical
        } else if self.available_bytes < u64::from(warn_gb) * (1 << 30) {
            Level::Warn
        } else {
            Level::Ok
        }
    }

    fn line(&self, level: Level) -> String {
        let free_gb = self.available_bytes / (1 << 30);
        let free_percent = (u128::from(self.available_bytes) * 100) / u128::from(self.total_bytes);
        format!(
            "disk: {free_gb} GB free ({free_percent}%) {}",
            level.label()
        )
    }
}

/// Strips C0/C1 control bytes — including ESC, so no CSI or OSC sequence
/// survives intact — from a `df` field before it is ever stored, printed,
/// or handed to `activity::send_message` as a hub notice body
/// (critic-codex-areas.md M2). `df`'s filesystem column names a mount the
/// operator does not control; left unsanitized, it reaches the terminal
/// (and could carry, for instance, an OSC 52 clipboard write) verbatim.
/// Python never prints a filesystem identifier at all (`shutil.disk_usage`
/// reports only numeric capacity), so there is no reference behavior to
/// match beyond "never forward raw control bytes."
fn sanitize_field(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

/// POSIX `df -Pk` uses 1024-byte blocks on both Linux and macOS, regardless
/// of the user's preferred human-readable units. Mount names may contain
/// spaces; only the fixed leading columns matter here.
pub fn parse_df(text: &str) -> Option<DiskUsage> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    lines.next()?;
    let fields: Vec<&str> = lines.next()?.split_whitespace().collect();
    if fields.len() < 6 || lines.next().is_some() {
        return None;
    }
    let total_bytes = fields[1].parse::<u64>().ok()?.checked_mul(1024)?;
    let used_bytes = fields[2].parse::<u64>().ok()?.checked_mul(1024)?;
    let available_bytes = fields[3].parse::<u64>().ok()?.checked_mul(1024)?;
    fields[4].strip_suffix('%')?.parse::<u32>().ok()?;
    if total_bytes == 0 || used_bytes > total_bytes || available_bytes > total_bytes {
        return None;
    }
    Some(DiskUsage {
        filesystem: sanitize_field(fields[0]),
        total_bytes,
        available_bytes,
    })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiskReport {
    pub lines: Vec<String>,
    pub level: Option<Level>,
    pub notice_sent: bool,
}

/// Once per host pass, including while halted: an exhausted disk is still
/// actionable when dispatch is paused. Dry-run probes but never sends mail.
pub fn sweep_report(config: &Config, apply: bool, now: OffsetDateTime) -> Vec<String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    sweep_report_with(
        config,
        apply,
        now,
        &home,
        Path::new("/private/tmp/claude-501"),
        &mut dockerreport::host_probe,
    )
}

pub fn sweep_report_with(
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    home: &Path,
    temp_root: &Path,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> Vec<String> {
    let docker = dockerreport::report_for_sweep_with(config, now, probe);
    let disk = report_with(config, apply, now, home, temp_root, &docker, probe);
    docker.lines.into_iter().chain(disk.lines).collect()
}

#[allow(clippy::too_many_arguments)]
pub fn report_with(
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    home: &Path,
    temp_root: &Path,
    _docker: &HostReport,
    probe: &mut impl FnMut(&str, &[&str]) -> Option<String>,
) -> DiskReport {
    let mut readings = Vec::new();
    let mut projects: Vec<_> = config.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    for root in projects
        .into_iter()
        .filter_map(|project| project.path.value())
        .filter(|root| !root.is_empty())
    {
        if let Some(usage) = probe("df", &["-Pk", root]).and_then(|text| parse_df(&text)) {
            readings.push(usage);
        }
    }
    let mut report = DiskReport::default();
    // Python chooses one reading by free bytes, even across different mounts.
    if let Some(usage) = readings.iter().min_by_key(|usage| usage.available_bytes) {
        let level = usage.level(config.host_disk_warn_gb, config.host_disk_critical_gb);
        report.level = Some(level);
        if level != Level::Ok {
            report.lines.push(usage.line(level));
        }
    }
    if report.level == Some(Level::Critical) {
        if let Some(text) = probe("docker", &["system", "df"]) {
            if let Some(line) = text
                .lines()
                .find(|line| line.trim().to_ascii_lowercase().starts_with("build cache"))
            {
                if let Some(reclaimable) = line.split_whitespace().last() {
                    report.lines.push(format!(
                        "disk: docker Build Cache reclaimable {reclaimable}"
                    ));
                }
            }
        }
        if let Some(text) = probe(
            "du",
            &[
                "-sh",
                &home.join("Library/Caches/go-build").to_string_lossy(),
            ],
        ) {
            if let Some(size) = text.split_whitespace().next() {
                report
                    .lines
                    .push(format!("disk: ~/Library/Caches/go-build {size}"));
            }
        }
        if let Ok(entries) = std::fs::read_dir(temp_root) {
            let mut paths: Vec<_> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect();
            paths.sort();
            for path in paths {
                if let Some(text) = probe("du", &["-sh", &path.to_string_lossy()]) {
                    if let Some(size) = text.split_whitespace().next() {
                        report
                            .lines
                            .push(format!("disk: {} {size}", path.display()));
                    }
                }
            }
        }
    }
    if apply && report.level.is_some_and(|level| level != Level::Ok) {
        if let Some(hub) = config
            .projects
            .get("multiverse")
            .and_then(|project| project.path.value())
            .filter(|path| !path.is_empty())
        {
            // Stable hourly identity makes repeated sweeps idempotent under
            // the recipient log's existing atomic append-if-absent lock.
            let level = report.level.unwrap_or(Level::Ok);
            // Keep the existing notice identity while sweep's visible
            // severity follows Python's LOW spelling.
            let notice_level = if level == Level::Warn {
                "WARN"
            } else {
                level.label()
            };
            let id = format!(
                "host-disk-{}-{}",
                notice_level.to_ascii_lowercase(),
                now.unix_timestamp().div_euclid(3600)
            );
            let body = format!("Host disk {notice_level}\n{}", report.lines.join("\n"));
            report.notice_sent = matches!(
                mv_state::activity::send_message(
                    &mv_state::activity::ProjectLog::open("multiverse", Path::new(hub)),
                    &id,
                    "multiverse",
                    "multiverse",
                    &body,
                    false,
                    None,
                    true,
                    &mv_core::redact::redact_text,
                ),
                Ok(mv_state::activity::SendOutcome::Sent { .. })
            );
        }
    }
    report
}

/// Card `20261019-fixer-29a027`: report-only stale `/private/tmp`/`$TMPDIR`
/// scratch over 5 GB. Never deletes. Python's `stale_temp_report`
/// (`cli.py:12182-12390`).
const STALE_TEMP_AFTER: Duration = Duration::from_secs(2 * 3600);
const STALE_TEMP_WARN_GB: f64 = 5.0;
const STALE_TEMP_README_DEPTH: usize = 3;
const STALE_TEMP_README_MARKER: &str = "cached build artifacts from the Go build system";
const STALE_TEMP_MAX_ENTRIES: usize = 50_000;
const PRIVATE_TMP: &str = "/private/tmp";
/// A result younger than this is reused instead of rescanned
/// (`cli.py`'s `STALE_TEMP_CACHE_SECONDS`).
const STALE_TEMP_CACHE_SECONDS: i64 = 30 * 60;
const STALE_TEMP_CACHE_FILENAME: &str = "stale-temp-cache.json";
/// One cold scan's wall-clock deadline (`cli.py`'s
/// `STALE_TEMP_SCAN_BUDGET_SECONDS`).
const STALE_TEMP_SCAN_BUDGET: Duration = Duration::from_secs(5);
/// Python's `STALE_TEMP_LSOF_TIMEOUT_SECONDS` — also the cap when the
/// remaining scan budget is smaller (`cli.py:11977-11981`).
const STALE_TEMP_LSOF_TIMEOUT: Duration = Duration::from_secs(2);

/// Production wrapper: real `$TMPDIR` and `/private/tmp`, `du -sk` via the
/// host probe, `lsof +D` fail-open as "not open".
pub fn stale_temp_report(
    config: &Config,
    recorded_tasks: &BTreeMap<String, String>,
    now: OffsetDateTime,
    running_worktrees: &[PathBuf],
) -> Vec<String> {
    let tmpdir = std::env::var_os("TMPDIR").map(PathBuf::from);
    let started = Instant::now();
    stale_temp_report_with(
        config,
        recorded_tasks,
        now,
        tmpdir.as_deref(),
        Some(Path::new(PRIVATE_TMP)),
        running_worktrees,
        &mut |path| du_size_kb(path),
        &mut |path| {
            let remaining = STALE_TEMP_SCAN_BUDGET.saturating_sub(started.elapsed());
            let timeout = remaining
                .min(STALE_TEMP_LSOF_TIMEOUT)
                .max(Duration::from_millis(10));
            stale_temp_is_open(path, timeout)
        },
        STALE_TEMP_SCAN_BUDGET,
    )
}

fn du_size_kb(path: &Path) -> Option<u64> {
    let text = dockerreport::host_probe("du", &["-sk", &path.to_string_lossy()])?;
    text.split_whitespace().next()?.parse().ok()
}

fn stale_temp_is_open(path: &Path, timeout: Duration) -> bool {
    let command = mv_herdr::argv::HerdrCommand {
        argv: vec![
            "lsof".into(),
            "+D".into(),
            path.to_string_lossy().into_owned(),
        ],
        purpose: "read-only stale-temp open-file probe".into(),
        stdin: None,
        expects_json: false,
    };
    let Ok(result) = mv_herdr::run::run(&command, Some(timeout)) else {
        return false;
    };
    result
        .stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        > 1
}

/// Injectable stale-temp scan. `size_kb` is KiB (`du -sk`); `is_open` is
/// Python's `_stale_temp_is_open` (fail-open: true means skip the dir).
/// `scan_budget` is Python's `STALE_TEMP_SCAN_BUDGET_SECONDS` (5 s in
/// production).
#[allow(clippy::too_many_arguments)]
pub fn stale_temp_report_with(
    config: &Config,
    recorded_tasks: &BTreeMap<String, String>,
    now: OffsetDateTime,
    tmpdir: Option<&Path>,
    private_tmp: Option<&Path>,
    running_worktrees: &[PathBuf],
    size_kb: &mut impl FnMut(&Path) -> Option<u64>,
    is_open: &mut impl FnMut(&Path) -> bool,
    scan_budget: Duration,
) -> Vec<String> {
    let Some(hub_path) = hub_project_path(config) else {
        return Vec::new();
    };
    let mut cached = load_stale_temp_cache(hub_path, now);
    if cached
        .as_ref()
        .is_some_and(|cache| stale_temp_cache_overlaps_running(cache, running_worktrees))
    {
        cached = None;
    }
    if let Some(cache) = cached.as_ref() {
        if !cache.partial {
            let age = (now - cache.scanned_at).whole_seconds();
            if age <= STALE_TEMP_CACHE_SECONDS {
                return lines_from_totals(&cache.totals, false);
            }
        }
    }

    let cursor = cached
        .as_ref()
        .filter(|cache| cache.partial)
        .and_then(|cache| cache.cursor.clone());
    let mut stale_by_path: BTreeMap<PathBuf, String> = cached
        .as_ref()
        .filter(|cache| cache.partial)
        .map(|cache| cache.pending.iter().cloned().collect())
        .unwrap_or_default();

    let deadline = Instant::now() + scan_budget;
    let mut partial = false;
    let mut candidates = Vec::new();
    let mut locations = Vec::new();
    for location in [tmpdir, private_tmp].into_iter().flatten() {
        if !locations.contains(&location) {
            locations.push(location);
        }
    }
    for location in locations {
        match stale_temp_top_dirs(location, deadline) {
            Ok(found) => candidates.extend(found),
            Err(BudgetTimeout) => {
                partial = true;
                break;
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    if let Some(cursor_path) = cursor.as_ref().map(PathBuf::from) {
        if let Some(index) = candidates.iter().position(|path| path == &cursor_path) {
            candidates = candidates.split_off(index + 1);
        }
    }

    let mut last_finished = cursor;
    if !partial {
        for candidate in candidates {
            if Instant::now() >= deadline {
                partial = true;
                break;
            }
            if candidate.file_name().and_then(|n| n.to_str()) == Some("claude-501") {
                last_finished = Some(candidate.to_string_lossy().into_owned());
                continue;
            }
            if stale_temp_is_under(&candidate, running_worktrees) {
                last_finished = Some(candidate.to_string_lossy().into_owned());
                continue;
            }
            let name = candidate
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let lane_project = stale_temp_lane_project(&name, recorded_tasks);
            if !stale_temp_name_matches(&name, lane_project.as_deref(), config) {
                last_finished = Some(candidate.to_string_lossy().into_owned());
                continue;
            }
            let newest = match stale_temp_newest_mtime(&candidate, deadline) {
                Ok(Some(newest)) => newest,
                Ok(None) => {
                    last_finished = Some(candidate.to_string_lossy().into_owned());
                    continue;
                }
                Err(BudgetTimeout) => {
                    partial = Instant::now() >= deadline;
                    if partial {
                        break;
                    }
                    last_finished = Some(candidate.to_string_lossy().into_owned());
                    continue;
                }
            };
            if age_secs(now, newest) < STALE_TEMP_AFTER.as_secs() as i64 {
                last_finished = Some(candidate.to_string_lossy().into_owned());
                continue;
            }
            if lane_project.is_none() {
                match stale_temp_readme_scan(&candidate, deadline) {
                    ReadmeScan::GoCache => {}
                    ReadmeScan::Partial => {
                        partial = true;
                        break;
                    }
                    ReadmeScan::Skip => {
                        last_finished = Some(candidate.to_string_lossy().into_owned());
                        continue;
                    }
                }
            }
            if is_open(&candidate) {
                last_finished = Some(candidate.to_string_lossy().into_owned());
                continue;
            }
            if Instant::now() >= deadline {
                partial = true;
                break;
            }
            let project = stale_temp_project_for(&name, lane_project.as_deref(), config);
            stale_by_path.insert(candidate.clone(), project);
            last_finished = Some(candidate.to_string_lossy().into_owned());
        }
    }

    let mut sizes: BTreeMap<PathBuf, u64> = BTreeMap::new();
    if !partial && !stale_by_path.is_empty() {
        if Instant::now() >= deadline {
            partial = true;
        } else {
            for path in stale_by_path.keys() {
                if Instant::now() >= deadline {
                    partial = true;
                    break;
                }
                match size_kb(path) {
                    Some(kb) => {
                        sizes.insert(path.clone(), kb);
                    }
                    None => {
                        partial = true;
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    partial = true;
                    break;
                }
            }
        }
    }

    let mut by_project: BTreeMap<String, Vec<(u64, PathBuf)>> = BTreeMap::new();
    for (path, project) in &stale_by_path {
        let kb = sizes.get(path).copied().unwrap_or(0);
        by_project
            .entry(project.clone())
            .or_default()
            .push((kb, path.clone()));
    }
    let mut totals: BTreeMap<String, StaleTempTotal> = BTreeMap::new();
    for (name, mut entries) in by_project {
        let gb: f64 = entries.iter().map(|(kb, _)| *kb as f64).sum::<f64>() / (1024.0 * 1024.0);
        if gb < STALE_TEMP_WARN_GB {
            continue;
        }
        entries.sort_by_key(|a| std::cmp::Reverse(a.0));
        let top_dirs: Vec<String> = entries
            .iter()
            .take(3)
            .filter_map(|(_, path)| path.file_name()?.to_str().map(str::to_string))
            .collect();
        let paths: Vec<PathBuf> = entries.into_iter().map(|(_, path)| path).collect();
        totals.insert(
            name,
            StaleTempTotal {
                gb,
                top_dirs,
                paths,
            },
        );
    }

    if partial {
        save_stale_temp_cache(
            hub_path,
            now,
            &BTreeMap::new(),
            last_finished.as_deref(),
            true,
            &stale_by_path.into_iter().collect::<Vec<_>>(),
        );
    } else {
        save_stale_temp_cache(hub_path, now, &totals, None, false, &[]);
    }
    lines_from_totals(&totals, partial)
}

struct BudgetTimeout;

struct StaleTempTotal {
    gb: f64,
    top_dirs: Vec<String>,
    paths: Vec<PathBuf>,
}

struct StaleTempCache {
    scanned_at: OffsetDateTime,
    totals: BTreeMap<String, StaleTempTotal>,
    cursor: Option<String>,
    partial: bool,
    pending: Vec<(PathBuf, String)>,
}

enum ReadmeScan {
    GoCache,
    Skip,
    Partial,
}

fn hub_project_path(config: &Config) -> Option<&Path> {
    config
        .projects
        .get(mv_state::activity::HUB_PROJECT_NAME)
        .and_then(|project| project.path.value())
        .filter(|path| !path.is_empty())
        .map(Path::new)
}

fn stale_temp_cache_path(hub_path: &Path) -> PathBuf {
    hub_path.join(".multiverse").join(STALE_TEMP_CACHE_FILENAME)
}

fn lines_from_totals(totals: &BTreeMap<String, StaleTempTotal>, partial: bool) -> Vec<String> {
    let mut lines: Vec<String> = totals
        .iter()
        .map(|(name, total)| {
            format!(
                "{name}: stale temp {:.1} GB — {}",
                total.gb,
                total.top_dirs.join(", ")
            )
        })
        .collect();
    if partial {
        lines.push("stale temp scan partial — next sweep will continue".into());
    }
    lines
}

fn stale_temp_cache_overlaps_running(cache: &StaleTempCache, running: &[PathBuf]) -> bool {
    cache
        .totals
        .values()
        .flat_map(|total| total.paths.iter())
        .chain(cache.pending.iter().map(|(path, _)| path))
        .any(|path| stale_temp_is_under(path, running))
}

fn json_number_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|n| n as f64))
        .or_else(|| value.as_u64().map(|n| n as f64))
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn load_stale_temp_cache(hub_path: &Path, now: OffsetDateTime) -> Option<StaleTempCache> {
    let path = stale_temp_cache_path(hub_path);
    let raw = std::fs::read_to_string(&path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let object = parsed.as_object()?;
    let scanned_at = object.get("scanned_at")?.as_str()?;
    let scanned_at = mv_core::time::parse_flexible(scanned_at).ok()?;
    let partial = match object.get("partial") {
        Some(value) => value.as_bool()?,
        None => false,
    };
    let cursor = match object.get("cursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if !value.is_empty() => Some(value.clone()),
        Some(_) => return None,
    };
    if (now - scanned_at).whole_seconds() > STALE_TEMP_CACHE_SECONDS && !partial {
        return None;
    }
    let totals_raw = object.get("totals")?.as_object()?;
    let mut totals = BTreeMap::new();
    for (name, entry) in totals_raw {
        let entry = entry.as_object()?;
        let gb = json_number_as_f64(entry.get("gb")?)?;
        let top_dirs = entry
            .get("top_dirs")?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        let paths = match entry.get("paths") {
            Some(value) => value
                .as_array()?
                .iter()
                .map(|value| value.as_str().map(PathBuf::from))
                .collect::<Option<Vec<_>>>()?,
            None => Vec::new(),
        };
        totals.insert(
            name.clone(),
            StaleTempTotal {
                gb,
                top_dirs,
                paths,
            },
        );
    }
    let mut pending = Vec::new();
    if let Some(pending_raw) = object.get("pending") {
        for entry in pending_raw.as_array()? {
            let entry = entry.as_object()?;
            let raw_path = entry.get("path")?.as_str()?;
            if raw_path.is_empty() {
                return None;
            }
            let project = entry.get("project")?.as_str()?;
            pending.push((PathBuf::from(raw_path), project.to_string()));
        }
    }
    Some(StaleTempCache {
        scanned_at,
        totals,
        cursor,
        partial,
        pending,
    })
}

fn save_stale_temp_cache(
    hub_path: &Path,
    now: OffsetDateTime,
    totals: &BTreeMap<String, StaleTempTotal>,
    cursor: Option<&str>,
    partial: bool,
    pending: &[(PathBuf, String)],
) {
    let dir = hub_path.join(".multiverse");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut totals_json = Map::new();
    for (name, total) in totals {
        totals_json.insert(
            name.clone(),
            json!({
                "gb": total.gb,
                "paths": total.paths.iter().map(|path| path.to_string_lossy().into_owned()).collect::<Vec<_>>(),
                "top_dirs": total.top_dirs,
            }),
        );
    }
    let pending_json: Vec<Value> = pending
        .iter()
        .map(|(path, project)| {
            json!({
                "path": path.to_string_lossy(),
                "project": project,
            })
        })
        .collect();
    let payload = json!({
        "cursor": cursor,
        "partial": partial,
        "pending": pending_json,
        "scanned_at": mv_core::time::format_seconds_utc(now),
        "totals": Value::Object(totals_json),
    });
    let Ok(body) = serde_json::to_string_pretty(&payload) else {
        return;
    };
    let _ = mv_core::atomic::write_atomic(
        &stale_temp_cache_path(hub_path),
        format!("{body}\n").as_bytes(),
    );
}

fn stale_temp_top_dirs(location: &Path, deadline: Instant) -> Result<Vec<PathBuf>, BudgetTimeout> {
    if Instant::now() >= deadline {
        return Err(BudgetTimeout);
    }
    let Ok(entries) = std::fs::read_dir(location) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return Err(BudgetTimeout);
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

fn stale_temp_is_under(path: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let Ok(candidate) = path.canonicalize() else {
        return false;
    };
    roots.iter().any(|root| {
        root.canonicalize()
            .map(|root| candidate == root || candidate.starts_with(&root))
            .unwrap_or(false)
    })
}

fn stale_temp_lane_project(
    name: &str,
    recorded_tasks: &BTreeMap<String, String>,
) -> Option<String> {
    recorded_tasks.iter().find_map(|(task_id, project)| {
        if name == task_id || name_contains_task_id(name, task_id) {
            Some(project.clone())
        } else {
            None
        }
    })
}

fn name_contains_task_id(name: &str, task_id: &str) -> bool {
    let Some(idx) = name.find(task_id) else {
        return false;
    };
    let before_ok = idx == 0 || matches!(name.as_bytes()[idx - 1], b'.' | b'-' | b'_');
    let after = idx + task_id.len();
    let after_ok =
        after == name.len() || matches!(name.as_bytes().get(after), Some(b'.' | b'-' | b'_'));
    before_ok && after_ok
}

fn stale_temp_name_matches(name: &str, lane_project: Option<&str>, config: &Config) -> bool {
    if lane_project.is_some() || name.starts_with("go-build") {
        return true;
    }
    let lowered = name.to_ascii_lowercase();
    config.projects.values().any(|project| {
        let mut candidates = vec![project.name.to_ascii_lowercase()];
        if let Some(short) = &project.short {
            candidates.push(short.to_ascii_lowercase());
        }
        candidates
            .iter()
            .any(|candidate| lowered.starts_with(&format!("{candidate}-")))
    })
}

fn stale_temp_project_for(name: &str, lane_project: Option<&str>, config: &Config) -> String {
    if let Some(project) = lane_project {
        return project.to_string();
    }
    let lowered = name.to_ascii_lowercase();
    let mut projects: Vec<_> = config.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    for project in projects {
        let mut candidates = vec![project.name.to_ascii_lowercase()];
        if let Some(short) = &project.short {
            candidates.push(short.to_ascii_lowercase());
        }
        if candidates
            .iter()
            .any(|candidate| lowered.starts_with(&format!("{candidate}-")))
        {
            return project.name.clone();
        }
    }
    "unknown".into()
}

fn stale_temp_newest_mtime(
    path: &Path,
    deadline: Instant,
) -> Result<Option<SystemTime>, BudgetTimeout> {
    if Instant::now() >= deadline {
        return Err(BudgetTimeout);
    }
    let Some(meta) = std::fs::metadata(path).ok() else {
        return Ok(None);
    };
    let Some(mut newest) = meta.modified().ok() else {
        return Ok(None);
    };
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(Some(newest));
    };
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return Err(BudgetTimeout);
        }
        if let Ok(stat) = entry.metadata() {
            if stat.is_dir() {
                if let Ok(mtime) = stat.modified() {
                    newest = newest.max(mtime);
                }
            }
        }
    }
    Ok(Some(newest))
}

fn age_secs(now: OffsetDateTime, mtime: SystemTime) -> i64 {
    let mtime_secs = mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now.unix_timestamp() - mtime_secs
}

fn stale_temp_readme_scan(path: &Path, deadline: Instant) -> ReadmeScan {
    stale_temp_readme(path, 0, &mut 0, deadline)
}

fn stale_temp_readme(
    path: &Path,
    depth: usize,
    count: &mut usize,
    deadline: Instant,
) -> ReadmeScan {
    if Instant::now() >= deadline {
        return ReadmeScan::Partial;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return ReadmeScan::Skip;
    };
    let mut subdirs = Vec::new();
    let mut found = false;
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return ReadmeScan::Partial;
        }
        *count += 1;
        if *count > STALE_TEMP_MAX_ENTRIES {
            return ReadmeScan::Skip;
        }
        let name = entry.file_name();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if name == "README" && !is_dir {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if text.contains(STALE_TEMP_README_MARKER) {
                    found = true;
                }
            }
        }
        if depth < STALE_TEMP_README_DEPTH && is_dir {
            subdirs.push(entry.path());
        }
    }
    for dir in subdirs {
        match stale_temp_readme(&dir, depth + 1, count, deadline) {
            ReadmeScan::GoCache => found = true,
            ReadmeScan::Partial => return ReadmeScan::Partial,
            ReadmeScan::Skip => {}
        }
    }
    if found {
        ReadmeScan::GoCache
    } else {
        ReadmeScan::Skip
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: &Path) -> Config {
        mv_config::load::load_config_str(&format!(
            "[projects.multiverse]\npath = {:?}\n[projects.demo]\npath = {:?}\n[projects.placeholder]\npath = \"<<PLACEHOLDER: project>>\"\n",
            root.join("hub").to_string_lossy(), root.join("demo").to_string_lossy(),
        ), &root.join("config.toml")).unwrap()
    }

    fn df(available_gb: u32) -> String {
        format!("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/fake 104857600 1048576 {} 1% /test mount\n", u64::from(available_gb) * (1 << 20))
    }

    #[test]
    fn posix_df_handles_mac_and_linux_columns_and_rejects_bad_readings() {
        let usage = parse_df(&df(15)).unwrap();
        assert_eq!(usage.available_bytes, 15 << 30);
        assert_eq!(usage.total_bytes, 100 << 30);
        assert_eq!(usage.level(30, 15), Level::Warn);
        assert_eq!(parse_df(&df(30)).unwrap().level(30, 15), Level::Ok);
        assert_eq!(parse_df(&df(14)).unwrap().level(30, 15), Level::Critical);
        assert_eq!(parse_df(&df(0)).unwrap().level(30, 15), Level::Critical);
        assert!(parse_df(
            "Filesystem 1K-blocks Used Available Use% Mounted on\n/dev/fake 10 4 5 40% /\n"
        )
        .is_some());
        for bad in [
            "",
            "not a df report",
            "header\n/dev/fake bad 1 1 1% /",
            "header\n/dev/fake 0 0 0 0% /",
            "header\n/dev/fake 10 1 11 1% /",
            "header\n/dev/fake 10 11 1 1% /",
            "header\n/dev/fake 10 1 -1 1% /",
            "header\n/dev/fake 18446744073709551615 1 1 1% /",
        ] {
            assert!(parse_df(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn parse_df_strips_control_bytes_from_the_filesystem_field_critic_m2() {
        // critic-codex-areas.md M2's own repro: a mount source carrying an
        // OSC 52 clipboard-write sequence must never survive into a field
        // this crate later prints or mails verbatim.
        let malicious = "\u{1b}]52;c;SGk=\u{07}";
        let text = format!(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n{malicious} 104857600 1048576 1048576 1% /mnt\n"
        );
        let usage = parse_df(&text).unwrap();
        assert!(
            !usage.filesystem.chars().any(|c| c.is_control()),
            "got {:?}",
            usage.filesystem
        );
        assert!(!usage.line(Level::Warn).chars().any(|c| c.is_control()));
    }

    #[test]
    fn critical_scan_uses_one_minimum_reading_and_python_reclaim_wording() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let tmp = dir.path().join("tmp");
        std::fs::create_dir_all(tmp.join("b")).unwrap();
        std::fs::create_dir_all(tmp.join("a")).unwrap();
        std::fs::write(tmp.join("not-a-directory"), "ignored").unwrap();
        let mut calls = Vec::new();
        let report = sweep_report_with(
            &config,
            false,
            OffsetDateTime::UNIX_EPOCH,
            dir.path(),
            &tmp,
            &mut |program, args| {
                calls.push((
                    program.to_owned(),
                    args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                ));
                Some(match (program, args) {
                    ("docker", ["system", "df"]) => "Build Cache 10 0 4GB 3GB (75%)\n".into(),
                    ("docker", ["system", "df", ..]) => {
                        "{\"Type\":\"Images\",\"Reclaimable\":\"4GB (50%)\"}".into()
                    }
                    ("docker", _) => String::new(),
                    ("df", _) if args[1].ends_with("demo") => {
                        df(12).replace("/dev/fake", "/dev/other")
                    }
                    ("df", _) => df(10),
                    ("du", ["-sh", _]) => "1.5G cache\n".into(),
                    _ => panic!("unexpected probe {program} {args:?}"),
                })
            },
        );
        assert_eq!(calls.iter().filter(|(p, _)| p == "docker").count(), 5);
        assert_eq!(calls.iter().filter(|(p, _)| p == "df").count(), 2);
        assert_eq!(calls.iter().filter(|(p, _)| p == "du").count(), 3);
        assert_eq!(
            report,
            vec![
                "docker: images 4GB (50%) reclaimable, build cache unknown reclaimable".to_owned(),
                "disk: 10 GB free (10%) CRITICAL".into(),
                "disk: docker Build Cache reclaimable (75%)".into(),
                "disk: ~/Library/Caches/go-build 1.5G".into(),
                format!("disk: {} 1.5G", tmp.join("a").display()),
                format!("disk: {} 1.5G", tmp.join("b").display()),
            ]
        );
        assert!(!dir.path().join("hub/.multiverse").exists());
    }

    #[test]
    fn hub_notice_is_apply_only_idempotent_and_can_escalate() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let now = mv_core::time::now_utc();
        let scan = |available, apply, when| {
            report_with(
                &config,
                apply,
                when,
                dir.path(),
                dir.path(),
                &HostReport::default(),
                &mut |program, _| (program == "df").then(|| df(available)),
            )
        };
        let preview = scan(20, false, now);
        assert_eq!(preview.level, Some(Level::Warn));
        assert!(!preview.notice_sent);
        assert!(!dir.path().join("hub/.multiverse").exists());
        assert!(scan(20, true, now).notice_sent);
        assert!(!scan(20, true, now).notice_sent);
        assert!(scan(10, true, now).notice_sent);
        assert!(scan(10, true, now + time::Duration::hours(1)).notice_sent);
        assert!(!scan(50, true, now).notice_sent);
        let log = mv_state::activity::ProjectLog::open("multiverse", &dir.path().join("hub"));
        let messages = log.inbox_messages().unwrap();
        assert_eq!(messages.len(), 3);
        assert!(messages[0].body.contains("Host disk WARN"));
        assert!(messages[1].body.contains("Host disk CRITICAL"));
        assert!(!dir.path().join("demo/.multiverse").exists());
    }

    #[test]
    fn a_healthy_filesystem_produces_no_line_on_an_unattended_sweep_critic_l() {
        // Python's `_host_disk_report` returns no lines at all for a
        // healthy reading (`cli.py:11702-11709`): an unattended sweep warns,
        // it never narrates a filesystem needing no action.
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let now = mv_core::time::now_utc();
        let healthy = report_with(
            &config,
            false,
            now,
            dir.path(),
            dir.path(),
            &HostReport::default(),
            &mut |program, _| (program == "df").then(|| df(50)),
        );
        assert_eq!(healthy.level, Some(Level::Ok));
        assert!(
            healthy.lines.is_empty(),
            "a healthy filesystem must print nothing on sweep, got {:?}",
            healthy.lines
        );
    }

    #[test]
    fn unknown_disk_is_not_healthy_and_warn_skips_expensive_probes() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let now = mv_core::time::now_utc();
        let failed = report_with(
            &config,
            true,
            now,
            dir.path(),
            dir.path(),
            &HostReport::default(),
            &mut |program, _| {
                assert_eq!(program, "df");
                None
            },
        );
        assert_eq!(failed, DiskReport::default());
        let warn = report_with(
            &config,
            false,
            now,
            dir.path(),
            dir.path(),
            &HostReport::default(),
            &mut |program, _| {
                assert_eq!(program, "df", "noncritical disks must not scan cache sizes");
                Some(df(20))
            },
        );
        assert_eq!(warn.level, Some(Level::Warn));
        assert_eq!(warn.lines, ["disk: 20 GB free (20%) LOW"]);
        assert!(!dir.path().join("hub/.multiverse").exists());
    }

    #[test]
    fn disk_line_floors_units_and_uses_the_first_minimum_reading() {
        let usage = DiskUsage {
            filesystem: "ignored".into(),
            total_bytes: 101 << 30,
            available_bytes: (19 << 30) + (1 << 29),
        };
        assert_eq!(usage.line(Level::Warn), "disk: 19 GB free (19%) LOW");
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let report = report_with(
            &config,
            false,
            OffsetDateTime::UNIX_EPOCH,
            dir.path(),
            dir.path(),
            &HostReport::default(),
            &mut |program, args| {
                assert_eq!(program, "df");
                Some(if args[1].ends_with("demo") {
                    df(20)
                } else {
                    df(20).replace("104857600", "209715200")
                })
            },
        );
        assert_eq!(report.lines, ["disk: 20 GB free (20%) LOW"]);
    }

    #[test]
    fn a_missing_hub_and_a_failed_root_do_not_suppress_other_disk_readings() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(dir.path());
        let mut hub = config.projects.remove("multiverse").unwrap();
        hub.name = "unavailable".into();
        config.projects.insert("unavailable".into(), hub);
        let report = report_with(
            &config,
            true,
            mv_core::time::now_utc(),
            dir.path(),
            dir.path(),
            &HostReport::default(),
            &mut |program, args| {
                if program == "df" && args[1].ends_with("demo") {
                    Some(df(10))
                } else {
                    None
                }
            },
        );
        assert_eq!(report.level, Some(Level::Critical));
        assert_eq!(report.lines.len(), 1);
        assert!(!report.notice_sent);
        assert!(!dir.path().join("hub/.multiverse").exists());
    }

    #[cfg(unix)]
    #[test]
    fn df_subprocess_is_always_an_explicit_fake() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("fake-df");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\n[ \"$1\" = -Pk ] || exit 9\nprintf '%s' '{}'\n",
                df(5)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let text = dockerreport::host_probe(binary.to_str().unwrap(), &["-Pk", "isolated"]);
        assert_eq!(parse_df(&text.unwrap()).unwrap().available_bytes, 5 << 30);
    }

    const GO_CACHE_README: &str =
        "This directory holds cached build artifacts from the Go build system.\n";

    fn stale_config(root: &Path) -> Config {
        mv_config::load::load_config_str(
            &format!(
                "[projects.multiverse]\npath = {:?}\n[projects.sandbox]\npath = {:?}\nshort = \"sbx\"\n",
                root.join("hub").to_string_lossy(),
                root.join("sandbox").to_string_lossy(),
            ),
            &root.join("config.toml"),
        )
        .unwrap()
    }

    fn backdate(path: &Path, hours: u64) {
        let ts = SystemTime::now() - Duration::from_secs(hours * 3600);
        std::fs::File::open(path).unwrap().set_modified(ts).unwrap();
    }

    #[test]
    fn stale_temp_report_flags_an_untouched_go_cache_over_threshold_and_never_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let config = stale_config(dir.path());
        let private_tmp = dir.path().join("private_tmp");
        let cache_dir = private_tmp.join("sandbox-gocache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("README"), GO_CACHE_README).unwrap();
        backdate(&cache_dir, 3);
        let now = mv_core::time::now_utc();
        let lines = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            now,
            None,
            Some(&private_tmp),
            &[],
            &mut |path| (path == cache_dir).then_some(6_000_000),
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert_eq!(
            lines,
            [format!(
                "sandbox: stale temp {:.1} GB — sandbox-gocache",
                6_000_000.0 / (1024.0 * 1024.0)
            )]
        );
        assert!(cache_dir.is_dir());
        assert!(cache_dir.join("README").exists());
    }

    #[test]
    fn stale_temp_report_skips_a_fresh_or_undersize_go_cache() {
        let dir = tempfile::tempdir().unwrap();
        let config = stale_config(dir.path());
        let private_tmp = dir.path().join("private_tmp");
        let fresh = private_tmp.join("sandbox-fresh");
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("README"), GO_CACHE_README).unwrap();
        let small = private_tmp.join("sandbox-small");
        std::fs::create_dir_all(&small).unwrap();
        std::fs::write(small.join("README"), GO_CACHE_README).unwrap();
        backdate(&small, 3);
        let now = mv_core::time::now_utc();
        let lines = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            now,
            None,
            Some(&private_tmp),
            &[],
            &mut |path| {
                if path == fresh {
                    Some(6_000_000)
                } else if path == small {
                    Some(1_000_000)
                } else {
                    None
                }
            },
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn stale_temp_report_without_a_hub_project_is_unscanned() {
        let dir = tempfile::tempdir().unwrap();
        let config = mv_config::load::load_config_str(
            &format!(
                "[projects.sandbox]\npath = {:?}\n",
                dir.path().join("sandbox").to_string_lossy()
            ),
            &dir.path().join("config.toml"),
        )
        .unwrap();
        let lines = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            mv_core::time::now_utc(),
            None,
            Some(dir.path()),
            &[],
            &mut |_| Some(9_000_000),
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert!(lines.is_empty());
    }

    fn plant_stale_go_cache(private_tmp: &Path, name: &str) -> PathBuf {
        let cache_dir = private_tmp.join(name);
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("README"), GO_CACHE_README).unwrap();
        backdate(&cache_dir, 3);
        cache_dir
    }

    #[test]
    fn stale_temp_report_stops_a_slow_tree_at_the_scan_budget() {
        let dir = tempfile::tempdir().unwrap();
        let config = stale_config(dir.path());
        std::fs::create_dir_all(dir.path().join("hub")).unwrap();
        let private_tmp = dir.path().join("private_tmp");
        for name in ["go-build-a", "go-build-b", "go-build-c", "go-build-d"] {
            plant_stale_go_cache(&private_tmp, name);
        }
        let now = mv_core::time::now_utc();
        let mut open_calls = 0usize;
        let started = Instant::now();
        let lines = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            now,
            None,
            Some(&private_tmp),
            &[],
            &mut |_| Some(6_000_000),
            &mut |_| {
                open_calls += 1;
                std::thread::sleep(Duration::from_millis(80));
                false
            },
            Duration::from_millis(100),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(800),
            "slow tree ran past the budget: {elapsed:?} open_calls={open_calls}"
        );
        assert!(
            open_calls < 4,
            "budget must stop before every candidate is probed: open_calls={open_calls}"
        );
        assert!(
            lines.iter().any(|line| line.contains("partial")),
            "incomplete scan must say so: {lines:?}"
        );
        let cached: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("hub/.multiverse/stale-temp-cache.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cached["partial"], true);
    }

    #[test]
    fn stale_temp_report_reuses_a_fresh_cache_and_does_not_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let config = stale_config(dir.path());
        std::fs::create_dir_all(dir.path().join("hub")).unwrap();
        let private_tmp = dir.path().join("private_tmp");
        let cache_dir = plant_stale_go_cache(&private_tmp, "sandbox-gocache");
        let now = mv_core::time::now_utc();
        let size_calls = std::cell::Cell::new(0usize);
        let mut size = |path: &Path| {
            if path == cache_dir {
                size_calls.set(size_calls.get() + 1);
                Some(6_000_000)
            } else {
                None
            }
        };
        let first = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            now,
            None,
            Some(&private_tmp),
            &[],
            &mut size,
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert_eq!(size_calls.get(), 1, "cold scan sizes the matching dir once");
        assert_eq!(
            first,
            [format!(
                "sandbox: stale temp {:.1} GB — sandbox-gocache",
                6_000_000.0 / (1024.0 * 1024.0)
            )]
        );
        let cache_path = dir.path().join("hub/.multiverse/stale-temp-cache.json");
        let cached: Value =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).unwrap()).unwrap();
        assert_eq!(cached["partial"], false);
        assert!(cached.get("scanned_at").and_then(Value::as_str).is_some());
        assert!(cached.get("totals").and_then(Value::as_object).is_some());
        assert!(cached.get("pending").and_then(Value::as_array).is_some());
        assert!(cached.get("cursor").is_some());

        let second = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            now,
            None,
            Some(&private_tmp),
            &[],
            &mut size,
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert_eq!(
            size_calls.get(),
            1,
            "a second pass within 30 min must not rescan"
        );
        assert_eq!(second, first);

        let expired = now + time::Duration::seconds(STALE_TEMP_CACHE_SECONDS + 1);
        let third = stale_temp_report_with(
            &config,
            &BTreeMap::new(),
            expired,
            None,
            Some(&private_tmp),
            &[],
            &mut size,
            &mut |_| false,
            STALE_TEMP_SCAN_BUDGET,
        );
        assert_eq!(size_calls.get(), 2, "a cache older than 30 min is rebuilt");
        assert_eq!(third, first);
    }
}
