//! Standing directives layered onto the Nexus seed.
//!
//! The caller supplies the fleet state directory resolved for its selected
//! config. Project history never lives here. Writes deliberately retain the
//! direct replacement semantics of the existing directives format.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const NO_DIRECTIVE_DIGEST: &str = "none";

#[derive(Debug, thiserror::Error)]
pub enum DirectivesError {
    #[error("standing directives may only be written from outside a Herdr pane")]
    InsidePane,
    #[error("invalid project name {0:?}")]
    InvalidProject(String),
    #[error("refusing symlinked or escaping directives path: {0}")]
    SymlinkRefusal(PathBuf),
    #[error("{path}: malformed until header {value:?}")]
    MalformedDirective { path: PathBuf, value: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_error(path: &Path, source: io::Error) -> DirectivesError {
    DirectivesError::Io {
        path: path.into(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct Directive {
    pub project: String,
    pub body: String,
    pub until: Option<SystemTime>,
    pub path: PathBuf,
    pub written_at: SystemTime,
}

impl Directive {
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.until.is_some_and(|until| until <= now)
    }
}

/// Resolve from an already selected fleet state directory, never a project root.
pub fn directives_root(state_dir: &Path) -> PathBuf {
    std::env::var_os("MULTIVERSE_DIRECTIVES")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("directives"))
}

pub fn directives_path(root: &Path, project: &str) -> Result<PathBuf, DirectivesError> {
    // Config owns cross-project case/Unicode collision checks.
    if !valid_project_name(project) {
        return Err(DirectivesError::InvalidProject(project.into()));
    }
    Ok(root.join(format!("{project}.md")))
}

pub(crate) fn valid_project_name(project: &str) -> bool {
    !project.is_empty()
        && !matches!(project, "." | "..")
        && !project
            .chars()
            .any(|c| matches!(c, '/' | '\\') || c.is_control())
}

fn verify_path(root: &Path, path: &Path) -> Result<(), DirectivesError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(DirectivesError::SymlinkRefusal(path.into()));
        }
        Ok(_) => {
            let resolved = fs::canonicalize(path).map_err(|e| io_error(path, e))?;
            let root = fs::canonicalize(root).map_err(|e| io_error(root, e))?;
            if resolved.parent() != Some(root.as_path()) {
                return Err(DirectivesError::SymlinkRefusal(path.into()));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error(path, e)),
    }
    Ok(())
}

pub fn read(root: &Path, project: &str) -> Result<Option<Directive>, DirectivesError> {
    let path = directives_path(root, project)?;
    verify_path(root, &path)?;
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&path, e)),
    };
    let written_at = file
        .metadata()
        .and_then(|m| m.modified())
        .map_err(|e| io_error(&path, e))?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).map_err(|e| io_error(&path, e))?;
    let (until, body) = match text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("until:"))
    {
        Some(value) => {
            let until =
                parse_until(value.trim()).ok_or_else(|| DirectivesError::MalformedDirective {
                    path: path.clone(),
                    value: value.trim().into(),
                })?;
            (
                Some(until),
                text.split_once('\n').map_or("", |(_, rest)| rest),
            )
        }
        None => (None, text.as_str()),
    };
    Ok(Some(Directive {
        project: project.into(),
        body: body.trim().into(),
        until,
        path,
        written_at,
    }))
}

pub fn active(
    root: &Path,
    project: &str,
    now: SystemTime,
) -> Result<Option<Directive>, DirectivesError> {
    Ok(read(root, project)?.filter(|directive| !directive.is_expired(now)))
}

/// A direct overwrite, without rename, locking, or an atomicity guarantee.
pub fn write(
    root: &Path,
    project: &str,
    body: &str,
    until: Option<&str>,
) -> Result<PathBuf, DirectivesError> {
    write_checked(
        root,
        project,
        body,
        until,
        std::env::var_os("HERDR_PANE_ID").is_some(),
    )
}

fn write_checked(
    root: &Path,
    project: &str,
    body: &str,
    until: Option<&str>,
    in_pane: bool,
) -> Result<PathBuf, DirectivesError> {
    if in_pane {
        return Err(DirectivesError::InsidePane);
    }
    let path = directives_path(root, project)?;
    if let Some(value) = until {
        parse_until(value.trim()).ok_or_else(|| DirectivesError::MalformedDirective {
            path: path.clone(),
            value: value.into(),
        })?;
    }
    verify_path(root, &path)?;
    fs::create_dir_all(root).map_err(|e| io_error(root, e))?;
    set_mode(root, 0o700).map_err(|e| io_error(root, e))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|e| io_error(&path, e))?;
    set_mode(&path, 0o600).map_err(|e| io_error(&path, e))?;
    let header = until.map_or(String::new(), |value| {
        format!("until: {}\n\n", value.trim())
    });
    file.write_all(format!("{header}{}\n", body.trim()).as_bytes())
        .map_err(|e| io_error(&path, e))?;
    Ok(path)
}

pub fn digest(root: &Path, project: &str) -> Result<String, DirectivesError> {
    let path = directives_path(root, project)?;
    verify_path(root, &path)?;
    match fs::read(&path) {
        Ok(bytes) => Ok(content_digest(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(NO_DIRECTIVE_DIGEST.into()),
        Err(e) => Err(io_error(&path, e)),
    }
}

fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

// Adapt the directive format's date-only, naive, and reduced-precision inputs
// to the shared timestamp parser. Naive timestamps continue to mean UTC.
fn parse_until(value: &str) -> Option<SystemTime> {
    let (date, rest) = value.split_once(['T', ' ']).unwrap_or((value, "00:00:00"));
    let mut parts = date.split('-');
    let year: u32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=9999).contains(&year) {
        return None;
    }
    let offset_start = rest.find(['+', '-', 'Z']);
    let (clock, zone) = offset_start.map_or((rest, ""), |i| rest.split_at(i));
    let zone = if zone.is_empty() || zone == "Z" {
        "+00:00".to_owned()
    } else {
        let mut fields = zone[1..].split(':');
        let hour: u32 = fields.next()?.parse().ok()?;
        let minute: u32 = fields.next()?.parse().ok()?;
        if fields.next().is_some() {
            return None;
        }
        format!("{}{:02}:{:02}", &zone[..1], hour, minute)
    };
    let mut fields = clock.split(':');
    let hour: u32 = fields.next()?.parse().ok()?;
    let minute: u32 = fields.next().unwrap_or("0").parse().ok()?;
    let seconds = fields.next().unwrap_or("0");
    if fields.next().is_some() {
        return None;
    }
    let (second, fraction) = seconds.split_once(['.', ',']).unwrap_or((seconds, "0"));
    let second: u32 = second.parse().ok()?;
    if second > 59 || fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let normalized =
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{fraction}{zone}");
    mv_core::time::parse_flexible(&normalized)
        .ok()
        .map(Into::into)
}

/// SHA-256's first eight bytes, matching Python's hexdigest()[:16]. Kept
/// private to these renderers so no manifest/lockfile changes escape this lane.
pub(crate) fn content_digest(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&(bytes.len() as u64 * 8).to_be_bytes());
    let mut hash: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (word, value) in hash.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(value);
        }
    }
    format!("{:08x}{:08x}", hash[0], hash[1])
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    pub(crate) struct TempDir(pub PathBuf);
    impl TempDir {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "mv-engine-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_existing_directives_and_expiration() {
        let dir = TempDir::new();
        let path = write_checked(
            &dir.0,
            "demo",
            "  Continue history.  ",
            Some("2026-11-02T01:00:00+01:00"),
            false,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "until: 2026-11-02T01:00:00+01:00\n\nContinue history.\n"
        );
        let directive = read(&dir.0, "demo").unwrap().unwrap();
        assert_eq!(directive.body, "Continue history.");
        assert!(
            SystemTime::now()
                .duration_since(directive.written_at)
                .unwrap()
                < Duration::from_secs(5)
        );
        let boundary = parse_until("2026-11-02T00:00:00Z").unwrap();
        assert_eq!(directive.until, Some(boundary));
        assert!(active(&dir.0, "demo", boundary).unwrap().is_none());
        assert!(active(&dir.0, "demo", boundary - Duration::from_secs(1))
            .unwrap()
            .is_some());
    }

    #[test]
    fn missing_errors_and_empty_headers_are_distinct() {
        let dir = TempDir::new();
        assert!(read(&dir.0, "demo").unwrap().is_none());
        assert_eq!(digest(&dir.0, "demo").unwrap(), "none");
        for header in [
            "until:",
            "until:   ",
            "until: nonsense",
            "until: 2026-02-30",
        ] {
            fs::write(dir.0.join("demo.md"), format!("{header}\n\nbody")).unwrap();
            assert!(matches!(
                read(&dir.0, "demo"),
                Err(DirectivesError::MalformedDirective { .. })
            ));
        }
        fs::remove_file(dir.0.join("demo.md")).unwrap();
        fs::create_dir(dir.0.join("demo.md")).unwrap();
        assert!(matches!(
            read(&dir.0, "demo"),
            Err(DirectivesError::Io { .. })
        ));
    }

    #[test]
    fn validates_writes_and_naive_dates() {
        let dir = TempDir::new();
        assert!(matches!(
            write_checked(&dir.0, "demo", "body", None, true),
            Err(DirectivesError::InsidePane)
        ));
        assert!(!dir.0.join("demo.md").exists());
        for project in ["../escape", "", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(read(&dir.0, project).is_err());
        }
        assert!(directives_path(&dir.0, "Project café").is_ok());
        for invalid in [
            "",
            "2026-01-01T-1:00",
            "2026-01-01T24:00",
            "2026-01-01T00:00:00+99:00",
        ] {
            assert!(parse_until(invalid).is_none());
        }
        assert_eq!(
            parse_until("2026-11-02T01:02:03.123"),
            parse_until("2026-11-02T01:02:03.123+00:00")
        );
        write_checked(&dir.0, "demo", "never expires", None, false).unwrap();
        assert!(read(&dir.0, "demo").unwrap().unwrap().until.is_none());
    }

    #[test]
    fn shared_parser_preserves_directive_timestamp_forms() {
        let midnight = parse_until("2026-11-02T00:00:00+00:00").unwrap();
        for value in [
            "2026-11-02",
            "2026-11-02T00",
            "2026-11-02 00:00",
            "2026-11-02T01:00:00+01:00",
            "2026-11-02T00:00:00Z",
        ] {
            assert_eq!(parse_until(value), Some(midnight), "{value}");
        }
        assert_eq!(
            parse_until("1969-12-31T23:59:59,123456789123"),
            Some(UNIX_EPOCH - Duration::from_nanos(876_543_211))
        );
        for value in ["0000-01-01", "2026-11-02T00:00:60", "2026-02-29"] {
            assert!(parse_until(value).is_none(), "{value}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_even_dangling_or_inside_root_symlinks_and_sets_modes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = TempDir::new();
        let path = write_checked(&dir.0, "demo", "original", None, false).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
        symlink(&path, dir.0.join("alias.md")).unwrap();
        symlink(dir.0.join("missing"), dir.0.join("dangling.md")).unwrap();
        for project in ["alias", "dangling"] {
            assert!(matches!(
                read(&dir.0, project),
                Err(DirectivesError::SymlinkRefusal(_))
            ));
            assert!(write_checked(&dir.0, project, "changed", None, false).is_err());
            assert!(digest(&dir.0, project).is_err());
        }
        assert_eq!(fs::read_to_string(path).unwrap(), "original\n");
    }

    #[test]
    fn sha256_matches_published_vectors_including_multiple_blocks() {
        assert_eq!(content_digest(b""), "e3b0c44298fc1c14");
        assert_eq!(content_digest(b"abc"), "ba7816bf8f01cfea");
        assert_eq!(
            content_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8"
        );
    }
}
