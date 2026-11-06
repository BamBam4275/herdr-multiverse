//! Load and validate `config.toml`, honoring `MULTIVERSE_CONFIG` and
//! the config state directory.
//!
//! Scope note: this module implements the schema and cross-cutting
//! checks described in `.multiverse/input/spec-config.md` §B/§C as
//! fully as this lane's budget allowed. It does not implement fleet
//! halt read/write (`FleetHalt`, `write_fleet_halt`/`clear_fleet_halt`)
//! — not named in this task's brief, and arguably a `mv-state`
//! concern since every other fleet-state writer lives there — or the
//! `review_risk_paths` glob-to-regex translator (a review-time
//! concern for whichever crate ends up dispatching Critic, not a
//! load-time validation). Both are called out again in this task's
//! `result.md`.
//!
//! Cutover-parity fixes (task `20261102-fixer-2bc5ba`, from
//! `.multiverse/input/critic-config.md`): per-role `on_exhausted`
//! defaults, a non-empty `review_risk_paths` default, a project's
//! local `.multiverse/roster.toml` merged in at load, a missing (not
//! yet cloned) project path accepted rather than refused, `~`
//! expansion and `abspath` on `MULTIVERSE_CONFIG`/
//! `MULTIVERSE_STATE_DIR`, and three of finding 10's strictness
//! mismatches (empty global chains, empty model strings, a stricter
//! `unavailable_until` parse) brought in line with Python. See this
//! task's `result.md` for the full map.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use toml::Value;

use crate::error::ConfigError;
use crate::placeholder::MaybePlaceholder;
use crate::roles::Role;

type Table = toml::value::Table;

// ---------------------------------------------------------------------
// Path resolution and the state-dir rule (spec-config.md §C)
// ---------------------------------------------------------------------

/// `~/.config/multiverse/config.toml`, under the given home directory.
///
/// Takes `home` explicitly rather than reading `$HOME` itself so the
/// precedence/canonicalization logic in this module is unit-testable
/// without mutating process-global environment state (`std::env::set_var`
/// is a data race across `cargo test`'s parallel threads). See
/// [`default_config_path_from_env`] for the real-environment wrapper.
pub fn default_config_path(home: &Path) -> PathBuf {
    home.join(".config").join("multiverse").join("config.toml")
}

/// Precedence for which config path to use: an explicit override (the
/// CLI's `--config`) beats `MULTIVERSE_CONFIG`, which beats the
/// default path (`config.py:2829-2835`), with `~`/`~/...` expanded
/// against `home` in every case — Python applies `.expanduser()`
/// unconditionally to whichever of the three it picked
/// (`config.py:2829-2835`), not only to the override.
///
/// Pure: the caller has already read `--config` and `MULTIVERSE_CONFIG`
/// (or passed `None` for either); this function only decides the
/// precedence between values it is handed.
pub fn config_path(
    cli_override: Option<&Path>,
    env_multiverse_config: Option<&Path>,
    home: &Path,
) -> PathBuf {
    let chosen = cli_override
        .or(env_multiverse_config)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_config_path(home));
    expand_user(&chosen, home)
}

/// Python's `Path.expanduser()` (`config.py:2834`, `2846`, `2849`): a
/// bare `~` or a `~/...` prefix becomes `home`. `~other-user/...` is
/// deliberately not supported — no caller here needs someone else's
/// home directory, and resolving one would mean consulting the system
/// user database for what is otherwise pure string handling over a
/// config value.
fn expand_user(path: &Path, home: &Path) -> PathBuf {
    match path.to_str() {
        Some("~") => home.to_path_buf(),
        Some(s) => match s.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

/// `path`, made absolute against `cwd` and lexically normalized —
/// deliberately **not** symlink-resolved (`config.py:2838-2846`,
/// contrasted in spec-config.md §C with `resolve_project_path`, which
/// does follow symlinks). A symlinked config path and its target are
/// different inputs to this function on purpose: [`config_state_dir`]
/// is what reconciles them back into one state directory, not this.
pub fn resolve_config_path(path: &Path, cwd: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    normalize_lexically(&absolute)
}

/// Collapse `.`/`..` components without touching the filesystem (no
/// symlink resolution) — the Rust equivalent of `os.path.abspath`'s
/// normalization step, not `Path::canonicalize`.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir) | None
                ) {
                    out.pop();
                } else if out.components().next_back().is_none() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The canonical fleet state directory for `configured` (an
/// already-[resolved][resolve_config_path] config path), honoring the
/// `a1ced2c` alias rule: a config path that is the *same file* as the
/// canonical default (reached via a symlink, `MULTIVERSE_CONFIG`, or
/// directly) shares the default's state directory, never the alias's
/// own parent — otherwise the same fleet's state silently splits
/// across the two routes (spec-config.md §C, §L.1).
///
/// `default` is `resolve_config_path` applied to the bare default path
/// — i.e. computed the same way `configured` was, so a device/inode
/// comparison between them is meaningful. `state_dir_override` is
/// `MULTIVERSE_STATE_DIR`, which wins outright over this entire rule.
///
/// Pure: takes already-resolved paths and the override value, so the
/// alias/no-alias/override/missing-file branches are all unit-testable
/// against real temporary files and symlinks without touching process
/// environment. See [`config_state_dir_from_env`] for the wrapper that
/// reads `MULTIVERSE_STATE_DIR`.
pub fn config_state_dir(
    configured: &Path,
    default: &Path,
    state_dir_override: Option<&Path>,
) -> PathBuf {
    if let Some(dir) = state_dir_override {
        return dir.to_path_buf();
    }
    let shares_the_default = same_file(configured, default).unwrap_or(false);
    let parent_of = if shares_the_default {
        default
    } else {
        configured
    };
    parent_of
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| parent_of.to_path_buf())
}

/// Device+inode equality — the same test Python's `Path.samefile` makes,
/// and what actually detects "reached the same file via a symlink".
/// `Err` (most commonly: one side does not exist) is treated by the
/// caller as "not the same file", matching `config_state_dir`'s
/// `OSError` fallthrough (`config.py:2849-2862` step 4).
fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let ma = fs::metadata(a)?;
    let mb = fs::metadata(b)?;
    Ok(ma.dev() == mb.dev() && ma.ino() == mb.ino())
}

/// `$HOME` if set to a non-empty value, else the home directory recorded
/// in the OS user-database entry for the running process's uid — the
/// same fallback Python's `Path.home()` takes via `pwd.getpwuid`, so a
/// caller that reaches this instead of a bare `$HOME` read behaves the
/// same whether `$HOME` is set or not. `None` only when neither source
/// resolves (no `$HOME` and no passwd/user-database entry).
///
/// The one home-resolution primitive every crate should call: an
/// `unsafe`-free wrapper over [`dirs::home_dir`], which does the passwd
/// lookup itself (via `dirs-sys`/`libc`) so this crate's own
/// `unsafe_code = "forbid"` lint never has to see the FFI call. A
/// caller that uses the result to build deny-list or allow-list rules
/// (card `20261106-reviewer-abf805`'s finding) must treat `None` as a
/// refusal, not silently fall back to a root-anchored path — see
/// [`home_dir_from_env`] for the one fallback that may still do that.
pub fn resolve_home_dir() -> Option<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home)),
        _ => dirs::home_dir(),
    }
}

/// [`resolve_home_dir`], falling back to `/` when neither `$HOME` nor
/// the passwd entry resolves. Only for callers that use `home` for a
/// best-effort path (a default config/project location, a cache
/// override) rather than a security boundary — see
/// [`resolve_home_dir`]'s own doc for the callers that must not use
/// this fallback.
pub fn home_dir_from_env() -> PathBuf {
    resolve_home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// `default_config_path`, reading `$HOME` from the real process
/// environment.
pub fn default_config_path_from_env() -> PathBuf {
    default_config_path(&home_dir_from_env())
}

/// `config_path` + `resolve_config_path`, reading `--config` (if any),
/// `$MULTIVERSE_CONFIG`, `$HOME`, and the real current directory. The
/// one entry point in this module mv-cli should call for an ordinary
/// run; everything else here stays pure and takes its inputs
/// explicitly so it can be unit-tested without mutating process state.
pub fn resolve_config_path_from_env(cli_override: Option<&Path>) -> io::Result<PathBuf> {
    let home = home_dir_from_env();
    let env = std::env::var_os("MULTIVERSE_CONFIG").map(PathBuf::from);
    let chosen = config_path(cli_override, env.as_deref(), &home);
    let cwd = std::env::current_dir()?;
    Ok(resolve_config_path(&chosen, &cwd))
}

/// `config_state_dir`, reading `$MULTIVERSE_STATE_DIR` from the real
/// process environment. `configured` must already be resolved (see
/// [`resolve_config_path_from_env`]).
///
/// `MULTIVERSE_STATE_DIR`, like `MULTIVERSE_CONFIG`, gets `~` expanded
/// and is made absolute against the current directory before
/// [`config_state_dir`] sees it (`config.py:2849`: `expanduser()` then
/// `os.path.abspath`) — a relative or `~`-prefixed override must not be
/// taken as relative to wherever the fleet state directory happens to
/// end up being read from next.
pub fn config_state_dir_from_env(configured: &Path) -> PathBuf {
    let home = home_dir_from_env();
    let default = resolve_config_path(&default_config_path(&home), &home);
    let over = std::env::var_os("MULTIVERSE_STATE_DIR")
        .map(PathBuf::from)
        .map(|p| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            resolve_config_path(&expand_user(&p, &home), &cwd)
        });
    config_state_dir(configured, &default, over.as_deref())
}

// ---------------------------------------------------------------------
// TOML value helpers — the shared "must be a {type}" validators
// (regression `f801276`: one message template, not ad hoc per field).
// ---------------------------------------------------------------------

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Integer(_) => "integer",
        Value::Float(_) => "float",
        Value::Boolean(_) => "boolean",
        Value::Datetime(_) => "datetime",
        Value::Array(_) => "array",
        Value::Table(_) => "table",
    }
}

fn bad_type(scope: &str, key: &str, expected: &str, got: &Value) -> ConfigError {
    ConfigError::new(format!(
        "{scope}: {key:?} must be a {expected}, got {value:?} ({actual})",
        value = got,
        actual = type_name(got)
    ))
}

fn optional_str(table: &Table, scope: &str, key: &str) -> Result<Option<String>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(bad_type(scope, key, "string", other)),
    }
}

fn require_str(table: &Table, scope: &str, key: &str) -> Result<String, ConfigError> {
    optional_str(table, scope, key)?
        .ok_or_else(|| ConfigError::new(format!("{scope}: {key:?} is required")))
}

fn optional_bool(table: &Table, scope: &str, key: &str) -> Result<Option<bool>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(other) => Err(bad_type(scope, key, "boolean", other)),
    }
}

fn bool_with_default(
    table: &Table,
    scope: &str,
    key: &str,
    default: bool,
) -> Result<bool, ConfigError> {
    Ok(optional_bool(table, scope, key)?.unwrap_or(default))
}

/// A bounded, optional non-negative integer: `None` if absent, `Err`
/// if present but not an integer or below `min`.
fn optional_u32(
    table: &Table,
    scope: &str,
    key: &str,
    min: u32,
) -> Result<Option<u32>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Integer(i)) => {
            if *i < 0 || *i < i64::from(min) {
                Err(ConfigError::new(format!(
                    "{scope}: {key:?} must be an integer >= {min}, got {i}"
                )))
            } else if *i > i64::from(u32::MAX) {
                Err(ConfigError::new(format!(
                    "{scope}: {key:?} must be an integer <= {}, got {i}",
                    u32::MAX
                )))
            } else {
                Ok(Some(*i as u32))
            }
        }
        Some(other) => Err(bad_type(scope, key, "integer", other)),
    }
}

fn u32_with_default(
    table: &Table,
    scope: &str,
    key: &str,
    min: u32,
    default: u32,
) -> Result<u32, ConfigError> {
    Ok(optional_u32(table, scope, key, min)?.unwrap_or(default))
}

fn optional_str_list(
    table: &Table,
    scope: &str,
    key: &str,
) -> Result<Option<Vec<String>>, ConfigError> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => out.push(s.clone()),
                    other => {
                        return Err(ConfigError::new(format!(
                            "{scope}: {key:?} must be a list of strings, got {other:?} in it"
                        )))
                    }
                }
            }
            Ok(Some(out))
        }
        Some(other) => Err(bad_type(scope, key, "array of strings", other)),
    }
}

fn require_table<'a>(value: &'a Value, scope: &str) -> Result<&'a Table, ConfigError> {
    value
        .as_table()
        .ok_or_else(|| ConfigError::new(format!("{scope} must be a table, got {value:?}")))
}

/// `edit_distance(a, b) <= 2`-style fuzzy match used for the
/// unknown-key "did you mean" suggestion (regression `f288e9b`).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(row[j]).min(row[j - 1])
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// Warn (never refuse) about any key in `table` that is not in `known`,
/// with a "did you mean" suggestion when a known key is close enough.
/// Mirrors `_unknown_key_warnings` (regression `f288e9b`): a misspelled
/// top-level/account/project/role/board/review-policy key still loads.
fn warn_unknown_keys(table: &Table, scope: &str, known: &[&str], warnings: &mut Vec<String>) {
    for key in table.keys() {
        if known.contains(&key.as_str()) {
            continue;
        }
        let suggestion = known
            .iter()
            .map(|k| (*k, levenshtein(key, k)))
            .min_by_key(|(_, dist)| *dist)
            .filter(|(_, dist)| *dist <= 2)
            .map(|(k, _)| k);
        let message = match suggestion {
            Some(s) => format!("{scope}: unknown key {key:?} (did you mean {s:?}?)"),
            None => format!("{scope}: unknown key {key:?}"),
        };
        warnings.push(message);
    }
}

// ---------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------

/// `[accounts.<name>]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub kind: String,
    pub config_dir_var: String,
    pub config_dir: MaybePlaceholder,
    pub pool: String,
    pub vendor: String,
    pub command: String,
    pub short: String,
    pub model: Option<String>,
    pub health_url: String,
    pub startup_grace_seconds: Option<u32>,
    pub interactive: bool,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub unavailable_until: Option<String>,
    pub usage_warn_sessions_per_day: u32,
    pub max_brief_chars: Option<u32>,
    pub roles_allowed: Option<Vec<Role>>,
    pub max_runtime_minutes: Option<u32>,
    pub awaiting_lease_minutes_override: Option<u32>,
}

/// The provider selected in a Codex seat's own `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatProvider {
    pub name: String,
    pub base_url: Option<String>,
    pub auth_env_key: Option<String>,
}

/// Read only the selected provider's public connection metadata. Missing,
/// unreadable, or malformed seat config leaves provider information unknown.
/// Environment key names are reported; credentials are never read.
pub fn seat_provider(account: &Account) -> Option<SeatProvider> {
    if account.kind != "codex" {
        return None;
    }
    let dir = account.config_dir.value()?;
    let content = fs::read_to_string(Path::new(dir).join("config.toml")).ok()?;
    let config: Value = content.parse().ok()?;
    let name = config.get("model_provider")?.as_str()?.to_string();
    let provider = config.get("model_providers")?.get(&name)?.as_table()?;
    Some(SeatProvider {
        name,
        base_url: provider
            .get("base_url")
            .and_then(Value::as_str)
            .map(str::to_owned),
        auth_env_key: provider
            .get("env_key")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

const ACCOUNT_KEYS: &[&str] = &[
    "kind",
    "config_dir_var",
    "config_dir",
    "pool",
    "vendor",
    "command",
    "short",
    "model",
    "health_url",
    "startup_grace_seconds",
    "interactive",
    "available",
    "unavailable_reason",
    "unavailable_until",
    "usage_warn_sessions_per_day",
    "max_brief_chars",
    "roles_allowed",
    "max_runtime_minutes",
    "awaiting_lease_minutes_override",
];

/// The env var each known `kind` must select its account with
/// (`config.py:328-332`). An unknown `kind` is passed through
/// unchecked (`test_an_unknown_kind_is_not_checked_against_the_table`).
fn expected_config_dir_var(kind: &str) -> Option<&'static str> {
    match kind {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        "grok" => Some("GROK_HOME"),
        _ => None,
    }
}

fn parse_account(
    name: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Result<Account, ConfigError> {
    let scope = format!("accounts.{name}");
    let table = require_table(value, &scope)?;
    warn_unknown_keys(table, &scope, ACCOUNT_KEYS, warnings);

    let kind = require_str(table, &scope, "kind")?;
    let config_dir_var = require_str(table, &scope, "config_dir_var")?;
    let config_dir_raw = require_str(table, &scope, "config_dir")?;
    let config_dir = MaybePlaceholder::parse(config_dir_raw);
    if let Some(path) = config_dir.value() {
        validate_absolute_usable_string(&scope, "config_dir", path)?;
    }
    if let Some(expected) = expected_config_dir_var(&kind) {
        if config_dir_var != expected {
            return Err(ConfigError::new(format!(
                "{scope}: kind {kind:?} selects its account via {expected}, not {config_dir_var:?}"
            )));
        }
    }

    let pool = optional_str(table, &scope, "pool")?.unwrap_or_else(|| name.to_string());
    let vendor = optional_str(table, &scope, "vendor")?
        .unwrap_or_else(|| pool.split(':').next().unwrap_or(&pool).to_string());
    let command = optional_str(table, &scope, "command")?.unwrap_or_default();
    let short = optional_str(table, &scope, "short")?
        .unwrap_or_else(|| name.chars().take(2).collect::<String>().to_lowercase());
    let model = optional_str(table, &scope, "model")?.filter(|s| !s.is_empty());
    let health_url = optional_str(table, &scope, "health_url")?.unwrap_or_default();
    let startup_grace_seconds = optional_u32(table, &scope, "startup_grace_seconds", 0)?;
    let interactive = bool_with_default(table, &scope, "interactive", false)?;
    let available = bool_with_default(table, &scope, "available", true)?;
    let unavailable_reason = optional_str(table, &scope, "unavailable_reason")?;
    let unavailable_until = optional_str(table, &scope, "unavailable_until")?;
    if let Some(ref ts) = unavailable_until {
        validate_timestamp(&scope, "unavailable_until", ts)?;
    }
    let usage_warn_sessions_per_day =
        u32_with_default(table, &scope, "usage_warn_sessions_per_day", 1, 60)?;
    let max_brief_chars = optional_u32(table, &scope, "max_brief_chars", 1)?;
    let roles_allowed = match optional_str_list(table, &scope, "roles_allowed")? {
        None => None,
        Some(keys) => {
            let mut roles = Vec::with_capacity(keys.len());
            for key in keys {
                if Role::is_legacy_key(&key) && Role::from_config_key(&key).is_err() {
                    return Err(ConfigError::new(format!(
                        "{scope}: roles_allowed: {key:?} is a retired role key; run the config \
                         migrator (`multiverse config migrate`) before loading this file"
                    )));
                }
                roles.push(
                    Role::from_config_key(&key)
                        .map_err(|e| ConfigError::new(format!("{scope}: roles_allowed: {e}")))?,
                );
            }
            Some(roles)
        }
    };
    let max_runtime_minutes = optional_u32(table, &scope, "max_runtime_minutes", 1)?;
    let awaiting_lease_minutes_override =
        optional_u32(table, &scope, "awaiting_lease_minutes_override", 1)?;

    if interactive && kind == "claude" && command.trim().is_empty() {
        return Err(ConfigError::new(format!(
            "{scope}: interactive claude accounts need a non-empty command"
        )));
    }

    Ok(Account {
        name: name.to_string(),
        kind,
        config_dir_var,
        config_dir,
        pool,
        vendor,
        command,
        short,
        model,
        health_url,
        startup_grace_seconds,
        interactive,
        available,
        unavailable_reason,
        unavailable_until,
        usage_warn_sessions_per_day,
        max_brief_chars,
        roles_allowed,
        max_runtime_minutes,
        awaiting_lease_minutes_override,
    })
}

/// Rejects empty/whitespace, relative, `~`-prefixed, and
/// NUL-containing strings (`test_configured_paths_require_absolute_usable_strings`).
fn validate_absolute_usable_string(scope: &str, key: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::new(format!(
            "{scope}: {key:?} must not be empty"
        )));
    }
    if value.contains('\0') {
        return Err(ConfigError::new(format!(
            "{scope}: {key:?} must not contain a NUL byte"
        )));
    }
    if value.starts_with('~') {
        return Err(ConfigError::new(format!(
            "{scope}: {key:?} must be an absolute path, not {value:?} (~ is not expanded)"
        )));
    }
    if !value.starts_with('/') {
        return Err(ConfigError::new(format!(
            "{scope}: {key:?} must be an absolute path, got {value:?}"
        )));
    }
    Ok(())
}

/// Accepts an RFC-3339 timestamp (with offset, or trailing `Z`) or a
/// naive one with no offset at all — Python's `datetime.fromisoformat`
/// reads both, treating a naive value as UTC.
fn validate_timestamp(scope: &str, key: &str, value: &str) -> Result<(), ConfigError> {
    if is_valid_isoformat_timestamp(value) {
        return Ok(());
    }
    Err(ConfigError::new(format!(
        "{scope}: {key:?} is not a valid ISO-8601 timestamp: {value:?}"
    )))
}

/// Python's `datetime.fromisoformat` (3.11+, `config.py:1771-1779`) is
/// more permissive than RFC-3339 in two ways finding 10 calls out: it
/// accepts a space in place of the `T` date/time separator, and a
/// fractional-seconds component with no offset at all. Tries RFC-3339
/// first (handles `Z`/offset forms), then a naive `T`-separated form
/// with and without fractional seconds — each tried once on the
/// original string and once more with a single space normalized to
/// `T`, since Python treats either separator identically.
fn is_valid_isoformat_timestamp(value: &str) -> bool {
    use time::format_description::well_known::Rfc3339;

    let normalized = value.replacen(' ', "T", 1);
    for candidate in [value, normalized.as_str()] {
        if time::OffsetDateTime::parse(candidate, &Rfc3339).is_ok() {
            return true;
        }
        let seconds_only =
            time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
        if time::PrimitiveDateTime::parse(candidate, seconds_only).is_ok() {
            return true;
        }
        let with_fraction = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]"
        );
        if time::PrimitiveDateTime::parse(candidate, with_fraction).is_ok() {
            return true;
        }
    }
    false
}

/// `[roles.<key>]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSettings {
    pub role: Role,
    pub models: BTreeMap<String, String>,
    pub write_scope: String,
    pub effort: Option<String>,
    pub on_exhausted: FallbackPolicy,
    pub skills: Vec<String>,
}

const ROLE_KEYS: &[&str] = &["models", "write_scope", "effort", "on_exhausted", "skills"];

/// `on_exhausted` (spec-config.md §B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPolicy {
    Degrade,
    DegradeOnce,
    Queue,
    Warn,
    Shrink,
}

impl FallbackPolicy {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "degrade" => Some(Self::Degrade),
            "degrade_once" => Some(Self::DegradeOnce),
            "queue" => Some(Self::Queue),
            "warn" => Some(Self::Warn),
            "shrink" => Some(Self::Shrink),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Degrade => "degrade",
            Self::DegradeOnce => "degrade_once",
            Self::Queue => "queue",
            Self::Warn => "warn",
            Self::Shrink => "shrink",
        }
    }
}

/// `DEFAULT_POLICY`'s per-role default for `on_exhausted`
/// (`config.py:246,652-668`): most roles degrade quietly, but Nexus
/// never swaps seats (it holds the whole session's context), Seer and
/// Critic queue rather than hand a hard question or a same-vendor
/// review to a worse seat, Council shrinks its own candidate pool
/// instead of substituting a seat into it, and the two writer roles
/// degrade once before giving up outright.
fn default_on_exhausted(role: Role) -> FallbackPolicy {
    match role {
        Role::Nexus => FallbackPolicy::Warn,
        Role::Seer | Role::Critic => FallbackPolicy::Queue,
        Role::Council => FallbackPolicy::Shrink,
        Role::Builder | Role::Artisan => FallbackPolicy::DegradeOnce,
        Role::Scout | Role::Archivist | Role::Watcher => FallbackPolicy::Degrade,
    }
}

fn parse_role_settings(
    role: Role,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Result<RoleSettings, ConfigError> {
    let scope = format!("roles.{}", role.config_key());
    let table = require_table(value, &scope)?;

    // Q14 retired this role's `kind`/`model` scalars in favor of
    // `[roles.<key>.models]` keyed by kind (`config.py:1905-1946`):
    // `kind` now belongs to the account, and a single `model` can't say
    // which vendor it is for once a chain spans vendors. Checked before
    // `warn_unknown_keys` so these name the Q14 fix instead of reading
    // as an ordinary unknown-key typo.
    if table.contains_key("kind") {
        return Err(ConfigError::new(format!(
            "{scope}: declares `kind`, which now belongs to the account (Q14); move it to \
             each [accounts.<name>] this role routes to, and replace `model` here with a \
             [{scope}.models] table keyed by kind"
        )));
    }
    if table.contains_key("model") {
        return Err(ConfigError::new(format!(
            "{scope}: declares a single `model`, which no longer says enough (Q14); replace \
             it with a [{scope}.models] table, e.g.\n    [{scope}.models]\n    <kind> = \"<model>\""
        )));
    }

    warn_unknown_keys(table, &scope, ROLE_KEYS, warnings);

    let models_value = table
        .get("models")
        .ok_or_else(|| ConfigError::new(format!("{scope}: models is required")))?;
    let models_table = require_table(models_value, &format!("{scope}.models"))?;
    if models_table.is_empty() {
        return Err(ConfigError::new(format!(
            "{scope}: models must name at least one agent kind"
        )));
    }
    let mut models = BTreeMap::new();
    for (kind, model_value) in models_table {
        let model = match model_value {
            Value::String(s) => s.clone(),
            other => return Err(bad_type(&format!("{scope}.models"), kind, "string", other)),
        };
        if model.is_empty() {
            return Err(ConfigError::new(format!(
                "{scope}.models.{kind} must be a non-empty string"
            )));
        }
        models.insert(kind.clone(), model);
    }

    let write_scope =
        optional_str(table, &scope, "write_scope")?.unwrap_or_else(|| "none".to_string());
    let effort = optional_str(table, &scope, "effort")?;
    let on_exhausted = match optional_str(table, &scope, "on_exhausted")? {
        None => default_on_exhausted(role),
        Some(s) => FallbackPolicy::parse(&s).ok_or_else(|| {
            ConfigError::new(format!(
                "{scope}: on_exhausted must be one of degrade, degrade_once, queue, warn, shrink; got {s:?}"
            ))
        })?,
    };
    let skills = optional_str_list(table, &scope, "skills")?.unwrap_or_default();

    Ok(RoleSettings {
        role,
        models,
        write_scope,
        effort,
        on_exhausted,
        skills,
    })
}

/// `[projects.<name>]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: MaybePlaceholder,
    pub chains: BTreeMap<Role, Vec<String>>,
    pub short: Option<String>,
    pub lane_setup: Option<String>,
    pub min_slots: u32,
    pub max_slots: Option<u32>,
}

const PROJECT_KEYS: &[&str] = &[
    "path",
    "chains",
    "short",
    "lane_setup",
    "min_slots",
    "max_slots",
];

/// `^[a-z][a-z0-9_-]{1,3}$` — 2 to 4 characters (`PROJECT_SHORT_RE`).
fn is_valid_project_short(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes.len() > 4 {
        return false;
    }
    let first_ok = bytes[0].is_ascii_lowercase();
    let rest_ok = bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-');
    first_ok && rest_ok
}

fn parse_project(
    name: &str,
    value: &Value,
    warnings: &mut Vec<String>,
) -> Result<Project, ConfigError> {
    let scope = format!("projects.{name}");
    let table = require_table(value, &scope)?;
    warn_unknown_keys(table, &scope, PROJECT_KEYS, warnings);

    let path_raw = require_str(table, &scope, "path")?;
    let path = MaybePlaceholder::parse(path_raw);
    if let Some(p) = path.value() {
        validate_absolute_usable_string(&scope, "path", p)?;
    }

    let mut chains = BTreeMap::new();
    if let Some(chains_value) = table.get("chains") {
        let chains_table = require_table(chains_value, &format!("{scope}.chains"))?;
        for (key, list_value) in chains_table {
            if Role::is_legacy_key(key) && Role::from_config_key(key).is_err() {
                return Err(ConfigError::new(format!(
                    "{scope}.chains: {key:?} is a retired role key; run the config migrator \
                     (`multiverse config migrate`) before adding new chains"
                )));
            }
            let role = Role::from_config_key(key)
                .map_err(|e| ConfigError::new(format!("{scope}.chains: {e}")))?;
            let accounts = match list_value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::String(s) => out.push(s.clone()),
                            other => {
                                return Err(bad_type(
                                    &format!("{scope}.chains"),
                                    key,
                                    "array of strings",
                                    other,
                                ))
                            }
                        }
                    }
                    out
                }
                other => {
                    return Err(bad_type(
                        &format!("{scope}.chains"),
                        key,
                        "array of strings",
                        other,
                    ))
                }
            };
            // An empty global chain is stored as-is, matching Python
            // (`config.py:2065-2071` only refuses an empty chain in a
            // project-local `roster.toml`, not here) — finding 10.
            chains.insert(role, accounts);
        }
    }

    let short = optional_str(table, &scope, "short")?;
    if let Some(ref s) = short {
        if !is_valid_project_short(s) {
            return Err(ConfigError::new(format!(
                "{scope}: short {s:?} must match ^[a-z][a-z0-9_-]{{1,3}}$"
            )));
        }
    }
    let lane_setup = optional_str(table, &scope, "lane_setup")?;
    let min_slots = u32_with_default(table, &scope, "min_slots", 0, 0)?;
    let max_slots = optional_u32(table, &scope, "max_slots", 1)?;
    if let Some(max) = max_slots {
        if max < min_slots {
            return Err(ConfigError::new(format!(
                "{scope}: max_slots ({max}) must be >= min_slots ({min_slots})"
            )));
        }
    }

    Ok(Project {
        name: name.to_string(),
        path,
        chains,
        short,
        lane_setup,
        min_slots,
        max_slots,
    })
}

/// Recovers `[projects.<name>]`'s declaration order from the raw
/// `content`, since `projects` itself (a `BTreeMap`, like the
/// `toml::Table` it was parsed from — this crate does not enable
/// `toml`'s `preserve_order` feature) always iterates alphabetically
/// and has already lost that order by the time this runs.
///
/// Scans line by line for a top-level project table header — not a
/// nested one like `[projects.<name>.chains]` — in the order those
/// headers appear in the file (`config.toml`'s declaration order is
/// what `project_order` promises). Any project present in `projects`
/// but never matched by this textual scan (for example one assembled
/// only from dotted keys, `projects.name.path = "..."`, which this
/// scan does not attempt to parse) is appended afterward in `projects`'
/// own order, so every key in `projects` still appears here exactly
/// once.
fn project_declaration_order(content: &str, projects: &BTreeMap<String, Project>) -> Vec<String> {
    let mut order = Vec::with_capacity(projects.len());
    let mut seen = BTreeMap::new();
    for line in content.lines() {
        let line = line.trim();
        if !line.starts_with('[') || !line.ends_with(']') || line.starts_with("[[") {
            continue;
        }
        let inner = &line[1..line.len() - 1];
        let mut parts = inner.splitn(3, '.');
        let (Some(head), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if head.trim() != "projects" || parts.next().is_some() {
            continue;
        }
        let name = name.trim().trim_matches(['"', '\'']).to_string();
        if projects.contains_key(&name) && seen.insert(name.clone(), ()).is_none() {
            order.push(name);
        }
    }
    for name in projects.keys() {
        if seen.insert(name.clone(), ()).is_none() {
            order.push(name.clone());
        }
    }
    order
}

/// `[review_policy]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPolicy {
    pub require_cross_vendor_for: Vec<String>,
    pub rereview: String,
    pub batch_low_risk: bool,
    pub batch_max_lanes: u32,
    pub max_rereviews: u32,
    pub scope: String,
    pub max_diff_lines: u32,
    pub effort: String,
    pub output: String,
    pub timeout_minutes: u32,
    pub min_quota_percent: u32,
    pub run_tests: bool,
    pub risk_paths: BTreeMap<String, Vec<String>>,
    pub budget_per_window: u32,
}

const DEFAULT_CROSS_VENDOR_RISK_AREAS: &[&str] = &[
    "security",
    "auth",
    "migration",
    "deletion",
    "secrets",
    "release",
];

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            require_cross_vendor_for: DEFAULT_CROSS_VENDOR_RISK_AREAS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            rereview: "diff".to_string(),
            batch_low_risk: true,
            batch_max_lanes: 6,
            max_rereviews: 2,
            scope: "diff".to_string(),
            max_diff_lines: 1500,
            effort: "medium".to_string(),
            output: "terse".to_string(),
            timeout_minutes: 30,
            min_quota_percent: 20,
            run_tests: false,
            risk_paths: default_risk_paths(),
            budget_per_window: 3,
        }
    }
}

/// `DEFAULT_RISK_RULES`' path-glob half (`config.py:864-927`), applied
/// only when `[review_policy.review_risk_paths]` is absent entirely —
/// present means replace the whole table, never merge
/// (`config.py:1155-1157`).
fn default_risk_paths() -> BTreeMap<String, Vec<String>> {
    let areas: [(&str, &[&str]); 6] = [
        (
            "migration",
            &["migrations/**", "**/migrations/**", "prisma/migrations/**"],
        ),
        ("auth", &["**/auth/**"]),
        ("mcp", &["**/mcp/**", "**/*mcp*"]),
        ("deletion", &["**/*delete*", "**/*retire*"]),
        ("secrets", &["**/*secret*", "**/.env*", "**/*env*"]),
        (
            "release",
            &[
                "deploy/**",
                "**/deploy/**",
                "**/*deploy*",
                "**/*release*",
                "**/*rollback*",
            ],
        ),
    ];
    areas
        .into_iter()
        .map(|(area, globs)| {
            (
                area.to_string(),
                globs.iter().map(|g| g.to_string()).collect(),
            )
        })
        .collect()
}

const REVIEW_POLICY_KEYS: &[&str] = &[
    "require_cross_vendor_for",
    "rereview",
    "batch_low_risk",
    "batch_max_lanes",
    "max_rereviews",
    "review_scope",
    "review_max_diff_lines",
    "review_effort",
    "review_output",
    "review_timeout_minutes",
    "review_min_quota_percent",
    "review_run_tests",
    "review_risk_paths",
    "review_budget_per_window",
];

fn parse_review_policy(
    value: &Value,
    warnings: &mut Vec<String>,
) -> Result<ReviewPolicy, ConfigError> {
    let scope = "review_policy";
    let table = require_table(value, scope)?;
    warn_unknown_keys(table, scope, REVIEW_POLICY_KEYS, warnings);

    let defaults = ReviewPolicy::default();

    let require_cross_vendor_for = optional_str_list(table, scope, "require_cross_vendor_for")?
        .unwrap_or(defaults.require_cross_vendor_for);
    if require_cross_vendor_for.iter().any(String::is_empty) {
        return Err(ConfigError::new(format!(
            "{scope}: require_cross_vendor_for must be a list of non-empty strings"
        )));
    }
    for area in &require_cross_vendor_for {
        if !DEFAULT_CROSS_VENDOR_RISK_AREAS.contains(&area.as_str()) {
            warnings.push(format!(
                "{scope}: require_cross_vendor_for names an unrecognized risk area {area:?}"
            ));
        }
    }

    let rereview = optional_str(table, scope, "rereview")?.unwrap_or(defaults.rereview);
    if rereview != "diff" && rereview != "full" {
        return Err(ConfigError::new(format!(
            "{scope}: rereview must be \"diff\" or \"full\", got {rereview:?}"
        )));
    }
    let batch_low_risk =
        bool_with_default(table, scope, "batch_low_risk", defaults.batch_low_risk)?;
    let batch_max_lanes =
        u32_with_default(table, scope, "batch_max_lanes", 1, defaults.batch_max_lanes)?;
    let max_rereviews = u32_with_default(table, scope, "max_rereviews", 0, defaults.max_rereviews)?;
    let review_scope = optional_str(table, scope, "review_scope")?.unwrap_or(defaults.scope);
    if review_scope != "diff" && review_scope != "repo" {
        return Err(ConfigError::new(format!(
            "{scope}: review_scope must be \"diff\" or \"repo\", got {review_scope:?}"
        )));
    }
    let max_diff_lines = u32_with_default(
        table,
        scope,
        "review_max_diff_lines",
        1,
        defaults.max_diff_lines,
    )?;
    let effort = match optional_str(table, scope, "review_effort")? {
        None => defaults.effort,
        Some(s) if s.is_empty() => {
            return Err(ConfigError::new(format!(
                "{scope}: review_effort must be a non-empty string, got \"\""
            )))
        }
        Some(s) => s,
    };
    let output = optional_str(table, scope, "review_output")?.unwrap_or(defaults.output);
    if output != "terse" && output != "full" {
        return Err(ConfigError::new(format!(
            "{scope}: review_output must be \"terse\" or \"full\", got {output:?}"
        )));
    }
    let timeout_minutes = u32_with_default(
        table,
        scope,
        "review_timeout_minutes",
        1,
        defaults.timeout_minutes,
    )?;
    let min_quota_percent = u32_with_default(
        table,
        scope,
        "review_min_quota_percent",
        0,
        defaults.min_quota_percent,
    )?;
    if min_quota_percent > 100 {
        return Err(ConfigError::new(format!(
            "{scope}: review_min_quota_percent must be 0-100, got {min_quota_percent}"
        )));
    }
    let run_tests = bool_with_default(table, scope, "review_run_tests", defaults.run_tests)?;

    let risk_paths = if let Some(rp_value) = table.get("review_risk_paths") {
        let rp_table = require_table(rp_value, &format!("{scope}.review_risk_paths"))?;
        let mut parsed = BTreeMap::new();
        for (area, globs_value) in rp_table {
            let globs = match globs_value {
                Value::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        match item {
                            Value::String(s) => out.push(s.clone()),
                            other => {
                                return Err(bad_type(
                                    &format!("{scope}.review_risk_paths"),
                                    area,
                                    "array of strings",
                                    other,
                                ))
                            }
                        }
                    }
                    out
                }
                other => {
                    return Err(bad_type(
                        &format!("{scope}.review_risk_paths"),
                        area,
                        "array of strings",
                        other,
                    ))
                }
            };
            parsed.insert(area.clone(), globs);
        }
        parsed
    } else {
        // Present means replace; absent means the documented defaults
        // apply (`config.py:1155-1157`) — finding 2.
        defaults.risk_paths.clone()
    };

    let budget_per_window = u32_with_default(
        table,
        scope,
        "review_budget_per_window",
        0,
        defaults.budget_per_window,
    )?;

    Ok(ReviewPolicy {
        require_cross_vendor_for,
        rereview,
        batch_low_risk,
        batch_max_lanes,
        max_rereviews,
        scope: review_scope,
        max_diff_lines,
        effort,
        output,
        timeout_minutes,
        min_quota_percent,
        run_tests,
        risk_paths,
        budget_per_window,
    })
}

/// `dashboard_tab` is `bool | list[str]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardTab {
    Enabled(bool),
    Projects(Vec<String>),
}

impl Default for DashboardTab {
    fn default() -> Self {
        Self::Projects(vec!["multiverse".to_string()])
    }
}

/// `[board]` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board {
    pub refresh_seconds: u32,
    pub dashboard_tab: DashboardTab,
    /// Whether `dashboard_tab` was written explicitly in `[board]`,
    /// as opposed to falling back to [`DashboardTab::default`]. Python
    /// (`config.py:2482-2503`) only validates `dashboard_tab` project
    /// names when the key is present in the raw table; see
    /// [`validate_dashboard_tab_projects`].
    dashboard_tab_explicit: bool,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            refresh_seconds: 300,
            dashboard_tab: DashboardTab::default(),
            dashboard_tab_explicit: false,
        }
    }
}

const BOARD_KEYS: &[&str] = &["refresh_seconds", "dashboard_tab"];

fn parse_board(value: &Value, warnings: &mut Vec<String>) -> Result<Board, ConfigError> {
    let scope = "board";
    let table = require_table(value, scope)?;
    warn_unknown_keys(table, scope, BOARD_KEYS, warnings);

    let defaults = Board::default();
    let refresh_seconds =
        u32_with_default(table, scope, "refresh_seconds", 1, defaults.refresh_seconds)?;
    let dashboard_tab_explicit = table.get("dashboard_tab").is_some();
    let dashboard_tab = match table.get("dashboard_tab") {
        None => defaults.dashboard_tab,
        Some(Value::Boolean(b)) => DashboardTab::Enabled(*b),
        Some(Value::Array(_)) => DashboardTab::Projects(
            optional_str_list(table, scope, "dashboard_tab")?
                .expect("matched Array above so the list is present"),
        ),
        Some(other) => {
            return Err(bad_type(
                scope,
                "dashboard_tab",
                "boolean or array of strings",
                other,
            ))
        }
    };

    Ok(Board {
        refresh_seconds,
        dashboard_tab,
        dashboard_tab_explicit,
    })
}

fn parse_pairs(value: &Value, scope: &str) -> Result<BTreeMap<String, Vec<String>>, ConfigError> {
    let table = require_table(value, scope)?;
    let mut pairs = BTreeMap::new();
    for (sender, recipients_value) in table {
        let recipients = match recipients_value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(s) => out.push(s.clone()),
                        other => return Err(bad_type(scope, sender, "array of strings", other)),
                    }
                }
                out
            }
            other => return Err(bad_type(scope, sender, "array of strings", other)),
        };
        pairs.insert(sender.clone(), recipients);
    }
    Ok(pairs)
}

const TOP_LEVEL_KEYS: &[&str] = &[
    "accounts",
    "roles",
    "projects",
    "lend_interactive_to_workers",
    "max_concurrent",
    "awaiting_lease_minutes",
    "dispatch_min_quota_percent",
    "host_disk_warn_gb",
    "host_disk_critical_gb",
    "board",
    "review_policy",
    "activity",
    "mail",
];

/// The whole loaded, validated `config.toml` (spec-config.md §B).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub accounts: BTreeMap<String, Account>,
    pub roles: BTreeMap<Role, RoleSettings>,
    pub projects: BTreeMap<String, Project>,
    /// `projects`' keys in the order `[projects.<name>]` tables were
    /// declared in `config.toml`, not `projects`' own alphabetical
    /// `BTreeMap` order. Python iterates `config.projects` in
    /// declaration order for fleet-wide views (e.g. `backlog list`
    /// across every project); a caller that needs that order — rather
    /// than alphabetical — should iterate this `Vec` and index into
    /// `projects`, instead of calling `projects.values()`.
    pub project_order: Vec<String>,
    pub lend_interactive_to_workers: bool,
    pub max_concurrent: u32,
    pub awaiting_lease_minutes: u32,
    pub dispatch_min_quota_percent: u32,
    pub host_disk_warn_gb: u32,
    pub host_disk_critical_gb: u32,
    pub board: Board,
    pub review_policy: ReviewPolicy,
    pub activity_pairs: BTreeMap<String, Vec<String>>,
    pub mail_pairs: BTreeMap<String, Vec<String>>,
    /// Unknown-key and non-default-risk-area warnings collected while
    /// loading — never fatal, matching `Config.config_warnings`
    /// (regression `f288e9b`).
    pub warnings: Vec<String>,
    pub source_path: PathBuf,
}

/// Reads and validates the config at `path`. Refuses with a
/// [`ConfigError`] naming `path` if it is missing or malformed
/// (`config.py:2875-2895`).
pub fn load_config(path: &Path) -> Result<Config, ConfigError> {
    let content = fs::read_to_string(path).map_err(|e| {
        ConfigError::at(
            path,
            format!(
                "could not read config (see config.example.toml, or set MULTIVERSE_CONFIG): {e}"
            ),
        )
    })?;
    load_config_str(&content, path)
}

/// `load_config`'s parse/validate core, given the file's contents
/// directly — split out so tests do not need a real file on disk for
/// every case.
pub fn load_config_str(content: &str, path: &Path) -> Result<Config, ConfigError> {
    let root: Table = content
        .parse()
        .map_err(|e| ConfigError::at(path, format!("malformed TOML: {e}")))?;

    let mut warnings = Vec::new();
    warn_unknown_keys(&root, "<config>", TOP_LEVEL_KEYS, &mut warnings);

    let mut accounts = BTreeMap::new();
    if let Some(accounts_value) = root.get("accounts") {
        let accounts_table = require_table(accounts_value, "accounts")
            .map_err(|e| ConfigError::at(path, e.message()))?;
        for (name, value) in accounts_table {
            let account = parse_account(name, value, &mut warnings)
                .map_err(|e| ConfigError::at(path, e.message()))?;
            accounts.insert(name.clone(), account);
        }
    }

    let mut roles = BTreeMap::new();
    if let Some(roles_value) = root.get("roles") {
        let roles_table =
            require_table(roles_value, "roles").map_err(|e| ConfigError::at(path, e.message()))?;
        for (key, value) in roles_table {
            if Role::is_legacy_key(key) && Role::from_config_key(key).is_err() {
                return Err(ConfigError::at(
                    path,
                    format!(
                        "roles.{key}: {key:?} is a retired role key; run the config migrator \
                         (`multiverse config migrate`) to rewrite it before loading this file"
                    ),
                ));
            }
            let role = Role::from_config_key(key)
                .map_err(|e| ConfigError::at(path, format!("roles.{key}: {e}")))?;
            let settings = parse_role_settings(role, value, &mut warnings)
                .map_err(|e| ConfigError::at(path, e.message()))?;
            roles.insert(role, settings);
        }
    }

    let mut projects = BTreeMap::new();
    if let Some(projects_value) = root.get("projects") {
        let projects_table = require_table(projects_value, "projects")
            .map_err(|e| ConfigError::at(path, e.message()))?;
        for (name, value) in projects_table {
            let project = parse_project(name, value, &mut warnings)
                .map_err(|e| ConfigError::at(path, e.message()))?;
            projects.insert(name.clone(), project);
        }
    }
    let project_order = project_declaration_order(content, &projects);

    let lend_interactive_to_workers =
        bool_with_default(&root, "<config>", "lend_interactive_to_workers", false)
            .map_err(|e| ConfigError::at(path, e.message()))?;
    let max_concurrent = u32_with_default(&root, "<config>", "max_concurrent", 1, 3)
        .map_err(|e| ConfigError::at(path, e.message()))?;
    let awaiting_lease_minutes =
        u32_with_default(&root, "<config>", "awaiting_lease_minutes", 1, 60)
            .map_err(|e| ConfigError::at(path, e.message()))?;
    let dispatch_min_quota_percent =
        u32_with_default(&root, "<config>", "dispatch_min_quota_percent", 0, 5)
            .map_err(|e| ConfigError::at(path, e.message()))?;
    if dispatch_min_quota_percent > 100 {
        return Err(ConfigError::at(
            path,
            format!("dispatch_min_quota_percent must be 0-100, got {dispatch_min_quota_percent}"),
        ));
    }
    let host_disk_warn_gb = u32_with_default(&root, "<config>", "host_disk_warn_gb", 1, 30)
        .map_err(|e| ConfigError::at(path, e.message()))?;
    let host_disk_critical_gb = u32_with_default(&root, "<config>", "host_disk_critical_gb", 1, 15)
        .map_err(|e| ConfigError::at(path, e.message()))?;
    if host_disk_critical_gb >= host_disk_warn_gb {
        return Err(ConfigError::at(
            path,
            format!(
                "host_disk_critical_gb ({host_disk_critical_gb}) must be < host_disk_warn_gb ({host_disk_warn_gb})"
            ),
        ));
    }

    let board = match root.get("board") {
        Some(v) => parse_board(v, &mut warnings).map_err(|e| ConfigError::at(path, e.message()))?,
        None => Board::default(),
    };

    let review_policy = match root.get("review_policy") {
        Some(v) => {
            parse_review_policy(v, &mut warnings).map_err(|e| ConfigError::at(path, e.message()))?
        }
        None => ReviewPolicy::default(),
    };

    let activity_pairs = match root.get("activity") {
        Some(v) => {
            let t = require_table(v, "activity").map_err(|e| ConfigError::at(path, e.message()))?;
            match t.get("pairs") {
                Some(p) => parse_pairs(p, "activity.pairs")
                    .map_err(|e| ConfigError::at(path, e.message()))?,
                None => BTreeMap::new(),
            }
        }
        None => BTreeMap::new(),
    };
    let mail_pairs = match root.get("mail") {
        Some(v) => {
            let t = require_table(v, "mail").map_err(|e| ConfigError::at(path, e.message()))?;
            match t.get("pairs") {
                Some(p) => {
                    parse_pairs(p, "mail.pairs").map_err(|e| ConfigError::at(path, e.message()))?
                }
                None => BTreeMap::new(),
            }
        }
        None => BTreeMap::new(),
    };

    let config = Config {
        accounts,
        roles,
        projects,
        project_order,
        lend_interactive_to_workers,
        max_concurrent,
        awaiting_lease_minutes,
        dispatch_min_quota_percent,
        host_disk_warn_gb,
        host_disk_critical_gb,
        board,
        review_policy,
        activity_pairs,
        mail_pairs,
        warnings,
        source_path: path.to_path_buf(),
    };

    validate_cross_cutting(config, path)
}

/// Everything that needs the whole config parsed first
/// (spec-config.md §B, "cross-cutting checks").
fn validate_cross_cutting(mut config: Config, path: &Path) -> Result<Config, ConfigError> {
    at(validate_account_shorts(&config), path)?;
    at(validate_pool_config_dir_collapse(&config), path)?;
    at(validate_interactive_seats(&config), path)?;
    at(validate_project_paths_and_shorts(&config), path)?;
    merge_project_rosters(&mut config)?;
    at(validate_chains(&config), path)?;
    at(validate_slots(&config), path)?;
    at(validate_activity_mail_referential(&config), path)?;
    at(validate_dashboard_tab_projects(&config, path), path)?;
    Ok(config)
}

/// Merges each project's `<path>/.multiverse/roster.toml` chain
/// overrides into that project's global `chains`, **replacing** a
/// named role's chain entirely rather than merging the two lists
/// (finding 4; Python's `_load_local_chain_overrides` +
/// `chains.update(local_overrides)`, `config.py:2004-2071,2396-2402`).
/// Runs before [`validate_chains`] so a locally overridden chain is
/// checked by the exact same referential/interactive-seat rule a
/// global one is, with the exact same message
/// ([`crate::roster`]'s module docs). A project with a placeholder
/// path is skipped — there is no real directory to read a roster
/// from — and a project with no roster file, or no override for a
/// given role, is unaffected for that role.
fn merge_project_rosters(config: &mut Config) -> Result<(), ConfigError> {
    let names: Vec<String> = config.projects.keys().cloned().collect();
    for name in names {
        let raw = match config.projects[&name].path.value() {
            Some(raw) => raw.to_string(),
            None => continue,
        };
        let roster_path = Path::new(&raw).join(".multiverse").join("roster.toml");
        if let Some(roster) = crate::roster::load_roster(&roster_path)? {
            config
                .projects
                .get_mut(&name)
                .expect("name was just read from this map's own keys")
                .chains
                .extend(roster.chains);
        }
    }
    Ok(())
}

fn at(result: Result<(), ConfigError>, path: &Path) -> Result<(), ConfigError> {
    result.map_err(|e| ConfigError::at(path, e.message()))
}

fn validate_account_shorts(config: &Config) -> Result<(), ConfigError> {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for account in config.accounts.values() {
        if let Some(prior) = seen.insert(&account.short, &account.name) {
            return Err(ConfigError::new(format!(
                "accounts {prior:?} and {:?} both resolve to short {:?}",
                account.name, account.short
            )));
        }
    }
    Ok(())
}

fn validate_pool_config_dir_collapse(config: &Config) -> Result<(), ConfigError> {
    let mut pool_dirs: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    for account in config.accounts.values() {
        let dir = account.config_dir.as_raw();
        if let Some((prior_name, prior_dir)) = pool_dirs.insert(&account.pool, (&account.name, dir))
        {
            if prior_dir != dir {
                return Err(ConfigError::new(format!(
                    "accounts {prior_name:?} and {:?} share pool {:?} but point at different config_dir values",
                    account.name, account.pool
                )));
            }
        }
    }
    Ok(())
}

fn validate_interactive_seats(config: &Config) -> Result<(), ConfigError> {
    let interactive: Vec<&Account> = config.accounts.values().filter(|a| a.interactive).collect();
    if interactive.len() > 1 {
        let names: Vec<&str> = interactive.iter().map(|a| a.name.as_str()).collect();
        return Err(ConfigError::new(format!(
            "only one account may be interactive; got {}",
            names.join(", ")
        )));
    }
    Ok(())
}

fn validate_project_paths_and_shorts(config: &Config) -> Result<(), ConfigError> {
    let mut seen_paths: BTreeMap<PathBuf, &str> = BTreeMap::new();
    let mut seen_shorts: BTreeMap<&str, &str> = BTreeMap::new();
    for project in config.projects.values() {
        if let Some(short) = &project.short {
            if let Some(prior) = seen_shorts.insert(short, &project.name) {
                return Err(ConfigError::new(format!(
                    "projects {prior:?} and {:?} both use short {:?}",
                    project.name, short
                )));
            }
        }
        if let Some(raw) = project.path.value() {
            let resolved = resolve_project_path(raw);
            // A path that exists and is not a directory cannot be a
            // repo root; a path that does not exist yet is allowed — a
            // repo may simply not be cloned yet (`config.py:2642-2650`,
            // finding 3).
            if let Ok(meta) = fs::metadata(&resolved) {
                if !meta.is_dir() {
                    return Err(ConfigError::new(format!(
                        "projects.{}: path {:?} is not a directory",
                        project.name, raw
                    )));
                }
            }
            if let Some(prior) = seen_paths.insert(resolved.clone(), &project.name) {
                return Err(ConfigError::new(format!(
                    "projects {prior:?} and {:?} resolve to the same path {:?}",
                    project.name, resolved
                )));
            }
        }
    }
    Ok(())
}

/// Best-effort equivalent of Python's `Path(path).resolve()`
/// (`config.py:1666-1678`, called without `expanduser()` here since
/// `raw` is already required to be absolute by
/// [`validate_absolute_usable_string`]): symlink-resolved and
/// normalized when `raw` exists, otherwise just lexically normalized —
/// there is no filesystem entry left to resolve symlinks against, the
/// same case Python's `Path.resolve(strict=False)` falls back to.
fn resolve_project_path(raw: &str) -> PathBuf {
    let path = Path::new(raw);
    fs::canonicalize(path).unwrap_or_else(|_| normalize_lexically(path))
}

fn validate_chains(config: &Config) -> Result<(), ConfigError> {
    for project in config.projects.values() {
        for (role, accounts) in &project.chains {
            let scope = format!("projects.{}.chains.{}", project.name, role.config_key());
            validate_chain_entry(config, &scope, *role, accounts)?;
        }
    }
    Ok(())
}

/// The rule every chain entry must satisfy, wherever it is declared:
/// the global `[projects.<name>.chains]` table or a project-local
/// `.multiverse/roster.toml` override ([`crate::roster`]). Shared so
/// the two call sites cannot drift (spec-config.md §B: "Every refusal
/// above still applies to a local chain").
pub(crate) fn validate_chain_entry(
    config: &Config,
    scope: &str,
    role: Role,
    accounts: &[String],
) -> Result<(), ConfigError> {
    // No emptiness check here: an empty *global* chain is allowed
    // (finding 10, matching Python), and an empty *roster* chain is
    // already refused at `roster::load_roster_str`'s own parse step,
    // before a roster override ever reaches this shared validator.
    let role_settings = config.roles.get(&role);
    for account_name in accounts {
        let account = config.accounts.get(account_name).ok_or_else(|| {
            ConfigError::new(format!("{scope}: unknown account {account_name:?}"))
        })?;
        if let Some(settings) = role_settings {
            if !settings.models.contains_key(&account.kind) {
                return Err(ConfigError::new(format!(
                    "{scope}: account {account_name:?} is kind {:?}, which role {role} has no model for",
                    account.kind
                )));
            }
        }
        if account.interactive && role != Role::Nexus && !config.lend_interactive_to_workers {
            return Err(ConfigError::new(format!(
                "{scope}: {account_name:?} is the interactive seat; set lend_interactive_to_workers \
                 = true to let a worker role use it"
            )));
        }
    }
    Ok(())
}

fn validate_slots(config: &Config) -> Result<(), ConfigError> {
    let summed: u32 = config.projects.values().map(|p| p.min_slots).sum();
    if summed > config.max_concurrent {
        return Err(ConfigError::new(format!(
            "projects' min_slots sum to {summed}, which exceeds max_concurrent ({})",
            config.max_concurrent
        )));
    }
    Ok(())
}

fn validate_activity_mail_referential(config: &Config) -> Result<(), ConfigError> {
    for (label, pairs) in [
        ("activity.pairs", &config.activity_pairs),
        ("mail.pairs", &config.mail_pairs),
    ] {
        for (sender, recipients) in pairs {
            if !config.projects.contains_key(sender) {
                return Err(ConfigError::new(format!(
                    "{label}: unknown sender project {sender:?}"
                )));
            }
            for recipient in recipients {
                if !config.projects.contains_key(recipient) {
                    return Err(ConfigError::new(format!(
                        "{label}: {sender:?} names unknown recipient project {recipient:?}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Refuses a `dashboard_tab` entry naming a project this config does
/// not have — but only when `dashboard_tab` was written explicitly.
///
/// Python (`config.py:2482-2503`) hard-refuses with a `ConfigError`
/// only when `dashboard_tab` is present in the raw `[board]` table and
/// names an unknown project; when the key is absent and the default
/// (`dashboard_tab = ["multiverse"]`) applies, Python never validates
/// it at all. `Board::default()`'s own default names a project that
/// most configs — including this repo's own `config.example.toml`,
/// whose only project is `example` — do not define, so validating the
/// default here would make the shipped example fail to load with zero
/// edits; skipping the default case entirely (no warning, no error)
/// matches Python and keeps the default harmless, while an explicit,
/// wrong `dashboard_tab` still fails loudly.
fn validate_dashboard_tab_projects(config: &Config, _path: &Path) -> Result<(), ConfigError> {
    if !config.board.dashboard_tab_explicit {
        return Ok(());
    }
    if let DashboardTab::Projects(names) = &config.board.dashboard_tab {
        if let Some(name) = names
            .iter()
            .find(|name| !config.projects.contains_key(*name))
        {
            return Err(ConfigError::new(format!(
                "board.dashboard_tab names unknown project {name:?}"
            )));
        }
    }
    Ok(())
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Config({} accounts, {} roles, {} projects, from {})",
            self.accounts.len(),
            self.roles.len(),
            self.projects.len(),
            self.source_path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::tempdir;

    #[test]
    fn seat_provider_reads_only_the_selected_codex_provider_metadata() {
        let dir = tempdir().unwrap();
        let config = load_config_str(&format!(
            "[accounts.local]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = {:?}\n",
            dir.path().to_str().unwrap()
        ), Path::new("config.toml")).unwrap();
        let account = &config.accounts["local"];
        fs::write(
            dir.path().join("config.toml"),
            r#"
            model_provider = "local"
            [model_providers.local]
            name = "A display label"
            base_url = "http://localhost:1234/v1"
            env_key = "LOCAL_API_KEY"
            [model_providers.other]
            base_url = "http://localhost:9999/v1"
        "#,
        )
        .unwrap();
        assert_eq!(
            seat_provider(account),
            Some(SeatProvider {
                name: "local".into(),
                base_url: Some("http://localhost:1234/v1".into()),
                auth_env_key: Some("LOCAL_API_KEY".into()),
            })
        );
        let mut other_kind = account.clone();
        other_kind.kind = "claude".into();
        assert_eq!(seat_provider(&other_kind), None);
    }

    #[test]
    fn seat_provider_tolerates_missing_malformed_and_partial_seat_config() {
        let dir = tempdir().unwrap();
        let config = load_config_str(&format!(
            "[accounts.local]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = {:?}\n",
            dir.path().to_str().unwrap()
        ), Path::new("config.toml")).unwrap();
        let account = &config.accounts["local"];
        assert_eq!(seat_provider(account), None);
        for content in ["[", "", "model_provider = 3", "model_provider = \"absent\""] {
            fs::write(dir.path().join("config.toml"), content).unwrap();
            assert_eq!(seat_provider(account), None);
        }
        fs::write(
            dir.path().join("config.toml"),
            "model_provider = \"local\"\n[model_providers.local]\n",
        )
        .unwrap();
        assert_eq!(
            seat_provider(account),
            Some(SeatProvider {
                name: "local".into(),
                base_url: None,
                auth_env_key: None,
            })
        );
        let mut placeholder = account.clone();
        placeholder.config_dir = MaybePlaceholder::parse("<<PLACEHOLDER: config dir>>");
        assert_eq!(seat_provider(&placeholder), None);
    }

    /// The shipped `config.example.toml` must load cleanly with zero
    /// edits — it is what a brand-new install points at before the user
    /// fills in a single placeholder (this task's brief: "Done: load
    /// `config.example.toml`").
    #[test]
    fn the_shipped_example_config_loads() {
        let content = include_str!("../../../config.example.toml");
        let config = load_config_str(content, Path::new("config.example.toml")).unwrap();

        assert_eq!(config.accounts.len(), 5);
        assert!(config.accounts["claude-personal"]
            .config_dir
            .is_placeholder());
        assert_eq!(config.roles.len(), 1);
        assert!(config.roles.contains_key(&Role::Scout));
        assert_eq!(config.projects.len(), 1);
        assert!(config.projects["example"].path.is_placeholder());
        assert_eq!(
            config.projects["example"].chains.get(&Role::Scout),
            Some(&vec!["claude-personal".to_string()])
        );
        // The default `dashboard_tab` (`["multiverse"]`) names a
        // project this example does not define (its only project is
        // `example`), but the key is absent from the example's
        // `[board]` table, so `validate_dashboard_tab_projects` skips
        // it entirely — no warning, matching Python's own behavior of
        // never validating an unset `dashboard_tab`.
        assert_eq!(config.warnings, Vec::<String>::new());
    }

    /// An *explicit* `dashboard_tab` naming an unknown project is a
    /// hard refusal, not a warning — unlike the default case above
    /// (Python `config.py:2482-2503`: the `if "dashboard_tab" in
    /// board_table` guard only fires when the key was actually
    /// written).
    #[test]
    fn explicit_dashboard_tab_naming_an_unknown_project_is_refused() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = minimal_config(&project_root).replacen(
            "dashboard_tab = false",
            "dashboard_tab = [\"nonexistent\"]",
            1,
        );
        let config_path = dir.path().join("config.toml");

        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(
            err.message().contains("board.dashboard_tab")
                && err.message().contains("\"nonexistent\""),
            "unexpected message: {}",
            err.message()
        );
    }

    // -- path resolution --------------------------------------------

    #[test]
    fn default_config_path_is_under_dot_config() {
        let home = Path::new("/home/user");
        assert_eq!(
            default_config_path(home),
            PathBuf::from("/home/user/.config/multiverse/config.toml")
        );
    }

    #[test]
    fn config_path_precedence_cli_beats_env_beats_default() {
        let home = Path::new("/home/user");
        let cli = Path::new("/explicit/config.toml");
        let env = Path::new("/env/config.toml");
        assert_eq!(config_path(Some(cli), Some(env), home), cli);
        assert_eq!(config_path(None, Some(env), home), env);
        assert_eq!(config_path(None, None, home), default_config_path(home));
    }

    #[test]
    fn resolve_config_path_is_absolute_and_lexically_normalized() {
        let cwd = Path::new("/home/user/project");
        assert_eq!(
            resolve_config_path(Path::new("relative/config.toml"), cwd),
            PathBuf::from("/home/user/project/relative/config.toml")
        );
        assert_eq!(
            resolve_config_path(Path::new("/a/b/../c/./config.toml"), cwd),
            PathBuf::from("/a/c/config.toml")
        );
    }

    #[test]
    fn resolve_config_path_does_not_follow_symlinks() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("real.toml");
        fs::write(&target, "").unwrap();
        let link = dir.path().join("alias.toml");
        symlink(&target, &link).unwrap();

        let resolved = resolve_config_path(&link, Path::new("/"));
        // Lexically normalized, not canonicalized: the symlink's own
        // path comes back, not its target.
        assert_eq!(resolved, link);
    }

    #[test]
    fn state_dir_for_the_bare_default_path_is_its_own_parent() {
        let default = Path::new("/home/user/.config/multiverse/config.toml");
        let dir = config_state_dir(default, default, None);
        assert_eq!(dir, PathBuf::from("/home/user/.config/multiverse"));
    }

    #[test]
    fn state_dir_for_a_symlinked_default_matches_the_canonical_default() {
        let dir = tempdir().unwrap();
        let config_home = dir.path().join("config-home");
        fs::create_dir_all(&config_home).unwrap();
        let default = config_home.join("config.toml");
        fs::write(&default, "").unwrap();

        let alias_home = dir.path().join("alias-home");
        fs::create_dir_all(&alias_home).unwrap();
        let alias = alias_home.join("config.toml");
        symlink(&default, &alias).unwrap();

        let state_dir = config_state_dir(&alias, &default, None);
        assert_eq!(state_dir, config_home);
    }

    #[test]
    fn state_dir_for_an_unrelated_config_is_its_own_parent() {
        let dir = tempdir().unwrap();
        let default = dir.path().join("default/config.toml");
        fs::create_dir_all(default.parent().unwrap()).unwrap();
        fs::write(&default, "").unwrap();

        let other = dir.path().join("other/config.toml");
        fs::create_dir_all(other.parent().unwrap()).unwrap();
        fs::write(&other, "").unwrap();

        let state_dir = config_state_dir(&other, &default, None);
        assert_eq!(state_dir, other.parent().unwrap());
    }

    #[test]
    fn state_dir_override_wins_outright() {
        let default = Path::new("/home/user/.config/multiverse/config.toml");
        let over = Path::new("/elsewhere/state");
        let dir = config_state_dir(default, default, Some(over));
        assert_eq!(dir, over);
    }

    #[test]
    fn state_dir_falls_through_to_configured_parent_when_default_is_missing() {
        let dir = tempdir().unwrap();
        let configured = dir.path().join("configured/config.toml");
        fs::create_dir_all(configured.parent().unwrap()).unwrap();
        fs::write(&configured, "").unwrap();
        let missing_default = dir.path().join("nope/config.toml");

        let state_dir = config_state_dir(&configured, &missing_default, None);
        assert_eq!(state_dir, configured.parent().unwrap());
    }

    // -- schema / validation -----------------------------------------

    fn minimal_config(project_path: &Path) -> String {
        format!(
            r#"
[board]
dashboard_tab = false

[accounts.claude-personal]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: absolute path to the personal Claude config dir>>"

[roles.scout]
write_scope = "none"
[roles.scout.models]
claude = "<<PLACEHOLDER: model id>>"

[projects.example]
path = "{path}"

[projects.example.chains]
scout = ["claude-personal"]
"#,
            path = project_path.display()
        )
    }

    #[test]
    fn loads_a_minimal_config_like_the_example() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = minimal_config(&project_root);
        let config_path = dir.path().join("config.toml");

        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(config.accounts.len(), 1);
        assert_eq!(config.roles.len(), 1);
        assert_eq!(config.projects.len(), 1);
        assert!(config.warnings.is_empty());

        let account = &config.accounts["claude-personal"];
        assert!(account.config_dir.is_placeholder());
        assert_eq!(account.pool, "claude-personal");
        assert_eq!(account.vendor, "claude-personal");
        assert_eq!(account.short, "cl");

        let project = &config.projects["example"];
        assert_eq!(
            project.chains.get(&Role::Scout),
            Some(&vec!["claude-personal".to_string()])
        );
    }

    #[test]
    fn unknown_top_level_key_warns_rather_than_refuses() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        // Prepended, not appended: a bare key after the last `[table]`
        // header would parse as belonging to that table instead of the
        // document root.
        let content = format!("max_concurrennt = 5\n{}", minimal_config(&project_root));
        let config_path = dir.path().join("config.toml");

        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(config.warnings.len(), 1);
        assert!(config.warnings[0].contains("max_concurrennt"));
        assert!(config.warnings[0].contains("max_concurrent"));
    }

    #[test]
    fn malformed_toml_names_the_file() {
        let config_path = Path::new("/tmp/does-not-matter/config.toml");
        let err = load_config_str("not = [valid", config_path).unwrap_err();
        assert!(err
            .message()
            .starts_with("/tmp/does-not-matter/config.toml: malformed TOML:"));
    }

    #[test]
    fn a_retired_role_key_is_refused_naming_the_migrator() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[roles.fixer]
write_scope = "declared"
[roles.fixer.models]
claude = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("fixer"));
        assert!(err.message().contains("migrator"));
        assert!(err.message().contains("multiverse config migrate"));
    }

    #[test]
    fn a_retired_role_key_in_a_chain_is_also_refused() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[accounts.a]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
fixer = ["a"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("fixer"));
        assert!(err.message().contains("migrator"));
        assert!(err.message().contains("multiverse config migrate"));
    }

    #[test]
    fn a_retired_role_key_in_roles_allowed_is_also_refused_naming_the_migrator() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[accounts.a]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
roles_allowed = ["fixer"]

[projects.example]
path = "{path}"
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("fixer"));
        assert!(err.message().contains("migrator"));
        assert!(err.message().contains("multiverse config migrate"));
    }

    #[test]
    fn duplicate_account_shorts_are_refused() {
        let dir = tempdir().unwrap();
        let content = r#"
[accounts.alpha]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
short = "ax"

[accounts.beta]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
short = "ax"
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("ax"));
    }

    /// `optional_u32` cast `i as u32` with no upper bound, so a bounded
    /// integer field silently wrapped mod 2^32 instead of being refused —
    /// a hostile `max_concurrent` far above `u32::MAX` landed on a small,
    /// plausible-looking value rather than failing to load.
    #[test]
    fn an_integer_field_far_above_u32_max_is_refused_not_wrapped() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            "max_concurrent = 4294967297\n{}",
            minimal_config(&project_root)
        );
        let config_path = dir.path().join("config.toml");

        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(
            err.message().contains("max_concurrent"),
            "unexpected message: {}",
            err.message()
        );
    }

    #[test]
    fn two_accounts_sharing_a_pool_need_the_same_config_dir() {
        let dir = tempdir().unwrap();
        let content = r#"
[accounts.alpha]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/one"
pool = "shared"

[accounts.beta]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/two"
pool = "shared"
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("shared"));
    }

    #[test]
    fn only_one_account_may_be_interactive() {
        let dir = tempdir().unwrap();
        let content = r#"
[accounts.alpha]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
command = "claude-interactive"
interactive = true

[accounts.beta]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
command = "claude-interactive"
interactive = true
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("interactive"));
    }

    #[test]
    fn interactive_claude_account_needs_a_command() {
        let dir = tempdir().unwrap();
        let content = r#"
[accounts.alpha]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
interactive = true
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("command"));
    }

    #[test]
    fn worker_chain_may_not_name_the_interactive_seat_unless_lending() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[accounts.human]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
command = "claude-interactive"
interactive = true

[roles.scout]
[roles.scout.models]
claude = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["human"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("lend_interactive_to_workers"));
    }

    #[test]
    fn lending_allows_a_worker_chain_to_name_the_interactive_seat() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
lend_interactive_to_workers = true

[accounts.human]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
command = "claude-interactive"
interactive = true

[roles.scout]
[roles.scout.models]
claude = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["human"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();
        assert!(config.lend_interactive_to_workers);
    }

    #[test]
    fn chain_naming_an_unknown_account_is_refused() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["ghost"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("ghost"));
    }

    #[test]
    fn chain_naming_an_account_whose_kind_the_role_cannot_run_is_refused() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[accounts.codex-one]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "<<PLACEHOLDER: x>>"

[roles.scout]
[roles.scout.models]
claude = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["codex-one"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("codex"));
    }

    #[test]
    fn summed_min_slots_above_max_concurrent_is_refused() {
        let dir = tempdir().unwrap();
        let project_a = dir.path().join("a");
        let project_b = dir.path().join("b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();
        let content = format!(
            r#"
max_concurrent = 3

[projects.a]
path = "{a}"
min_slots = 2

[projects.b]
path = "{b}"
min_slots = 2
"#,
            a = project_a.display(),
            b = project_b.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("min_slots"));
    }

    #[test]
    fn summed_min_slots_at_or_below_max_concurrent_is_accepted() {
        let dir = tempdir().unwrap();
        let project_a = dir.path().join("a");
        let project_b = dir.path().join("b");
        fs::create_dir_all(&project_a).unwrap();
        fs::create_dir_all(&project_b).unwrap();
        let content = format!(
            r#"
max_concurrent = 4

[projects.a]
path = "{a}"
min_slots = 2

[projects.b]
path = "{b}"
min_slots = 2
"#,
            a = project_a.display(),
            b = project_b.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(config.projects.len(), 2);
    }

    #[test]
    fn two_projects_reaching_the_same_directory_via_a_symlink_collide() {
        let dir = tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir_all(&real).unwrap();
        let alias = dir.path().join("alias");
        symlink(&real, &alias).unwrap();

        let content = format!(
            r#"
[projects.one]
path = "{real}"

[projects.two]
path = "{alias}"
"#,
            real = real.display(),
            alias = alias.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("same path"));
    }

    /// `project_order` must follow the file's own
    /// `[projects.<name>]` order, not `projects`' alphabetical
    /// `BTreeMap` order — the gap this field exists to close.
    #[test]
    fn project_order_follows_declaration_order_not_alphabetical_order() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let content = r#"
[projects.hub]
path = "<<PLACEHOLDER: path>>"

[projects.demo-app]
path = "<<PLACEHOLDER: path>>"

[projects.api-service]
path = "<<PLACEHOLDER: path>>"

[projects.api-service.chains]
"#;

        let config = load_config_str(content, &config_path).unwrap();
        assert_eq!(config.project_order, vec!["hub", "demo-app", "api-service"]);
        // Alphabetical order, for contrast with the assertion above —
        // this is what iterating `projects` directly still gives.
        assert_eq!(
            config.projects.keys().collect::<Vec<_>>(),
            vec!["api-service", "demo-app", "hub"]
        );
    }

    #[test]
    fn a_project_path_that_is_not_a_directory_is_refused() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("not-a-dir");
        fs::write(&file_path, "").unwrap();
        let content = format!(
            r#"
[projects.one]
path = "{path}"
"#,
            path = file_path.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("not-a-dir"));
    }

    #[test]
    fn a_placeholder_project_path_is_skipped_entirely() {
        let dir = tempdir().unwrap();
        let content = r#"
[projects.one]
path = "<<PLACEHOLDER: absolute path to the project root>>"
"#;
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(content, &config_path).unwrap();
        assert!(config.projects["one"].path.is_placeholder());
    }

    #[test]
    fn duplicate_project_shorts_are_refused() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let content = format!(
            r#"
[projects.one]
path = "{a}"
short = "exam"

[projects.two]
path = "{b}"
short = "exam"
"#,
            a = a.display(),
            b = b.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("exam"));
    }

    #[test]
    fn review_policy_defaults_match_the_documented_defaults() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = load_config_str("", &config_path).unwrap();
        let rp = config.review_policy;
        assert_eq!(rp.rereview, "diff");
        assert!(rp.batch_low_risk);
        assert_eq!(rp.batch_max_lanes, 6);
        assert_eq!(rp.max_rereviews, 2);
        assert_eq!(rp.scope, "diff");
        assert_eq!(rp.max_diff_lines, 1500);
        assert_eq!(rp.effort, "medium");
        assert_eq!(rp.output, "terse");
        assert_eq!(rp.timeout_minutes, 30);
        assert_eq!(rp.min_quota_percent, 20);
        assert!(!rp.run_tests);
        assert_eq!(rp.budget_per_window, 3);
    }

    #[test]
    fn an_empty_config_loads_with_every_default() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = load_config_str("", &config_path).unwrap();
        assert_eq!(config.max_concurrent, 3);
        assert_eq!(config.awaiting_lease_minutes, 60);
        assert_eq!(config.dispatch_min_quota_percent, 5);
        assert_eq!(config.host_disk_warn_gb, 30);
        assert_eq!(config.host_disk_critical_gb, 15);
        assert!(!config.lend_interactive_to_workers);
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn activity_pairs_deny_by_default_and_check_referential_integrity() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        fs::create_dir_all(&a).unwrap();
        let content = format!(
            r#"
[projects.a]
path = "{a}"

[activity]
pairs = {{ a = ["ghost"] }}
"#,
            a = a.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("ghost"));
    }

    #[test]
    fn host_disk_critical_must_be_below_warn() {
        let dir = tempdir().unwrap();
        let content = "host_disk_warn_gb = 10\nhost_disk_critical_gb = 10\n";
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("host_disk_critical_gb"));
    }

    // -- finding 1: per-role `on_exhausted` defaults ------------------

    #[test]
    fn on_exhausted_defaults_match_python_per_role() {
        let dir = tempdir().unwrap();
        let cases: &[(Role, &str, FallbackPolicy)] = &[
            (Role::Nexus, "nexus", FallbackPolicy::Warn),
            (Role::Scout, "scout", FallbackPolicy::Degrade),
            (Role::Builder, "builder", FallbackPolicy::DegradeOnce),
            (Role::Archivist, "archivist", FallbackPolicy::Degrade),
            (Role::Critic, "critic", FallbackPolicy::Queue),
            (Role::Seer, "seer", FallbackPolicy::Queue),
            (Role::Council, "council", FallbackPolicy::Shrink),
            (Role::Artisan, "artisan", FallbackPolicy::DegradeOnce),
            (Role::Watcher, "watcher", FallbackPolicy::Degrade),
        ];
        for (role, key, expected) in cases {
            let content =
                format!("[roles.{key}]\n[roles.{key}.models]\nclaude = \"<<PLACEHOLDER: x>>\"\n");
            let config_path = dir.path().join("config.toml");
            let config = load_config_str(&content, &config_path).unwrap();
            assert_eq!(
                config.roles[role].on_exhausted, *expected,
                "role {role} should default on_exhausted to {expected:?}"
            );
        }
    }

    // -- finding 2: review_risk_paths default -------------------------

    #[test]
    fn review_risk_paths_default_matches_python_when_absent() {
        fn strings(items: &[&str]) -> Vec<String> {
            items.iter().map(|s| s.to_string()).collect()
        }

        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let config = load_config_str("", &config_path).unwrap();
        let rp = &config.review_policy.risk_paths;
        assert_eq!(
            rp.get("migration"),
            Some(&strings(&[
                "migrations/**",
                "**/migrations/**",
                "prisma/migrations/**"
            ]))
        );
        assert_eq!(rp.get("auth"), Some(&strings(&["**/auth/**"])));
        assert_eq!(rp.get("mcp"), Some(&strings(&["**/mcp/**", "**/*mcp*"])));
        assert_eq!(
            rp.get("deletion"),
            Some(&strings(&["**/*delete*", "**/*retire*"]))
        );
        assert_eq!(
            rp.get("secrets"),
            Some(&strings(&["**/*secret*", "**/.env*", "**/*env*"]))
        );
        assert_eq!(
            rp.get("release"),
            Some(&strings(&[
                "deploy/**",
                "**/deploy/**",
                "**/*deploy*",
                "**/*release*",
                "**/*rollback*"
            ]))
        );
    }

    #[test]
    fn review_risk_paths_present_replaces_rather_than_merges() {
        let dir = tempdir().unwrap();
        let content = "[review_policy.review_risk_paths]\ncustom = [\"**/*weird*\"]\n";
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(content, &config_path).unwrap();
        assert_eq!(
            config.review_policy.risk_paths.keys().collect::<Vec<_>>(),
            vec!["custom"]
        );
    }

    // -- finding 3: a missing project path is allowed, like Python ----

    #[test]
    fn a_project_path_that_does_not_exist_yet_is_accepted() {
        let dir = tempdir().unwrap();
        let not_cloned = dir.path().join("not-cloned-yet");
        let content = format!(
            r#"
[projects.one]
path = "{path}"
"#,
            path = not_cloned.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(
            config.projects["one"].path.value(),
            Some(not_cloned.to_str().unwrap())
        );
    }

    #[test]
    fn two_projects_both_naming_the_same_not_yet_cloned_path_still_collide() {
        let dir = tempdir().unwrap();
        let not_cloned = dir.path().join("not-cloned-yet");
        let content = format!(
            r#"
[projects.one]
path = "{path}"

[projects.two]
path = "{path}"
"#,
            path = not_cloned.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("same path"));
    }

    // -- finding 4: a project's local roster.toml is merged in --------

    #[test]
    fn load_config_merges_a_projects_local_roster_toml_chain_override() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        let multiverse_dir = project_root.join(".multiverse");
        fs::create_dir_all(&multiverse_dir).unwrap();
        fs::write(
            multiverse_dir.join("roster.toml"),
            "[chains]\nscout = [\"grok-work\"]\n",
        )
        .unwrap();

        let content = format!(
            r#"
[accounts.claude-personal]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"

[accounts.grok-work]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "<<PLACEHOLDER: x>>"

[roles.scout]
[roles.scout.models]
claude = "<<PLACEHOLDER: x>>"
grok = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["claude-personal"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();

        assert_eq!(
            config.projects["example"].chains.get(&Role::Scout),
            Some(&vec!["grok-work".to_string()])
        );
    }

    #[test]
    fn a_role_not_named_in_the_projects_roster_toml_keeps_the_global_chain() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        let multiverse_dir = project_root.join(".multiverse");
        fs::create_dir_all(&multiverse_dir).unwrap();
        fs::write(multiverse_dir.join("roster.toml"), "[chains]\n").unwrap();

        let content = format!(
            r#"
[accounts.claude-personal]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"

[projects.example]
path = "{path}"
[projects.example.chains]
scout = ["claude-personal"]
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(
            config.projects["example"].chains.get(&Role::Scout),
            Some(&vec!["claude-personal".to_string()])
        );
    }

    #[test]
    fn a_retired_role_key_in_a_projects_roster_toml_is_refused_naming_the_migrator() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        let multiverse_dir = project_root.join(".multiverse");
        fs::create_dir_all(&multiverse_dir).unwrap();
        fs::write(
            multiverse_dir.join("roster.toml"),
            "[chains]\nfixer = [\"claude-work\"]\n",
        )
        .unwrap();

        let content = format!(
            r#"
[projects.example]
path = "{path}"
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(&content, &config_path).unwrap_err();
        assert!(err.message().contains("fixer"));
        assert!(err.message().contains("multiverse config migrate"));
    }

    // -- finding 6: `~` expansion / abspath on env var overrides ------

    #[test]
    fn config_path_expands_a_tilde_cli_override() {
        let home = Path::new("/home/user");
        assert_eq!(
            config_path(Some(Path::new("~/custom/config.toml")), None, home),
            PathBuf::from("/home/user/custom/config.toml")
        );
    }

    #[test]
    fn config_path_expands_a_tilde_env_override() {
        let home = Path::new("/home/user");
        assert_eq!(
            config_path(None, Some(Path::new("~/env/config.toml")), home),
            PathBuf::from("/home/user/env/config.toml")
        );
    }

    #[test]
    fn config_path_expands_a_bare_tilde() {
        let home = Path::new("/home/user");
        assert_eq!(
            config_path(Some(Path::new("~")), None, home),
            PathBuf::from("/home/user")
        );
    }

    // -- finding 8/9/10/11: schema strictness parity ------------------

    #[test]
    fn stale_q14_kind_key_is_refused_naming_the_fix() {
        let dir = tempdir().unwrap();
        let content = r#"
[roles.scout]
kind = "claude"
[roles.scout.models]
claude = "<<PLACEHOLDER: x>>"
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("kind"));
        assert!(err.message().contains("Q14"));
    }

    #[test]
    fn stale_q14_model_key_is_refused_naming_the_fix() {
        let dir = tempdir().unwrap();
        let content = r#"
[roles.scout]
model = "<<PLACEHOLDER: x>>"
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("model"));
        assert!(err.message().contains("Q14"));
    }

    #[test]
    fn an_empty_model_string_is_refused() {
        let dir = tempdir().unwrap();
        let content = r#"
[roles.scout]
[roles.scout.models]
claude = ""
"#;
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("non-empty"));
    }

    #[test]
    fn an_empty_global_chain_is_accepted_like_python() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(&project_root).unwrap();
        let content = format!(
            r#"
[projects.example]
path = "{path}"
[projects.example.chains]
scout = []
"#,
            path = project_root.display()
        );
        let config_path = dir.path().join("config.toml");
        let config = load_config_str(&content, &config_path).unwrap();
        assert_eq!(
            config.projects["example"].chains.get(&Role::Scout),
            Some(&Vec::<String>::new())
        );
    }

    #[test]
    fn unavailable_until_accepts_a_space_separator_and_fractional_seconds() {
        let dir = tempdir().unwrap();
        for value in [
            "2026-11-02 12:00:00",
            "2026-11-02T12:00:00.123456",
            "2026-11-02 12:00:00.5",
        ] {
            let content = format!(
                r#"
[accounts.a]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: x>>"
unavailable_until = "{value}"
"#
            );
            let config_path = dir.path().join("config.toml");
            let config = load_config_str(&content, &config_path)
                .unwrap_or_else(|e| panic!("{value:?} should parse: {e}"));
            assert_eq!(
                config.accounts["a"].unavailable_until.as_deref(),
                Some(value)
            );
        }
    }

    #[test]
    fn require_cross_vendor_for_refuses_an_empty_string() {
        let dir = tempdir().unwrap();
        let content = "[review_policy]\nrequire_cross_vendor_for = [\"\"]\n";
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("non-empty"));
    }

    #[test]
    fn review_effort_refuses_an_empty_string() {
        let dir = tempdir().unwrap();
        let content = "[review_policy]\nreview_effort = \"\"\n";
        let config_path = dir.path().join("config.toml");
        let err = load_config_str(content, &config_path).unwrap_err();
        assert!(err.message().contains("non-empty"));
    }

    /// Guards `HOME` for [`resolve_home_dir_falls_back_to_the_passwd_home_when_home_is_unset`]
    /// (a lock plus restore-on-drop), kept local since no other test in
    /// this file touches `HOME`.
    struct HomeEnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeEnvGuard {
        fn unset() -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("HOME");
            std::env::remove_var("HOME");
            Self {
                previous,
                _lock: held,
            }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Card `20261106-reviewer-abf805`'s finding: `HOME` unset must not
    /// resolve to `/` — that is what made `dispatch.rs`'s claude deny-list
    /// protect `/.ssh/**`/`/.gitconfig` instead of the real user's home.
    #[test]
    fn resolve_home_dir_falls_back_to_the_passwd_home_when_home_is_unset() {
        let _home = HomeEnvGuard::unset();
        let resolved = resolve_home_dir().expect("the test process must have a passwd entry");
        assert_ne!(resolved, PathBuf::from("/"));
        assert_eq!(home_dir_from_env(), resolved);
    }
}
