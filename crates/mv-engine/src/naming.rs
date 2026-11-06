//! Herdr names: Python's `naming.agent_name` budget and disambiguation.
//!
//! The caller supplies the role's short code so current write surfaces and
//! historical previews can use their respective role vocabularies. New
//! launches write the current four-character codes (`nexu`, `scou`, …);
//! a lookup of a live Nexus also tries Python's retired `orch` code so a
//! cutover finds panes a running Python fleet already started.

/// Current write-side short code for Nexus. Python wrote [`NEXUS_LEGACY_SHORT`]
/// into this four-character slot; after cutover the same budget holds `nexu`.
pub const NEXUS_SHORT: &str = "nexu";
/// Python-era Orchestrator short code, still present on panes a running
/// Python fleet started. Same length as [`NEXUS_SHORT`], so slug/hash
/// truncation is identical across the rename.
pub const NEXUS_LEGACY_SHORT: &str = "orch";

use std::collections::HashSet;

use super::RunnerError;

const MAX_LEN: usize = 32;

fn slugify(text: &str, max_len: usize, leading_letter: bool) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for c in text.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(c);
            separator = false;
        } else {
            separator = true;
        }
    }
    if leading_letter && slug.starts_with(|c: char| c.is_ascii_digit()) {
        slug.insert(0, 'p');
    }
    slug.truncate(max_len);
    slug.trim_end_matches(['-', '_']).to_string()
}

// BLAKE2s with an eight-byte digest, as hashlib.blake2s(digest_size=8).
// Only its first two digest bytes identify a project; truncating a 32-byte
// digest instead would change the parameter block and every project tag.
fn disambiguator(project: &str) -> String {
    const IV: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const SIGMA: [[usize; 16]; 10] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
        [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
        [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
        [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
        [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
        [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
        [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
        [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
        [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    ];
    fn mix(v: &mut [u32; 16], positions: [usize; 4], x: u32, y: u32) {
        let [a, b, c, d] = positions;
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
        v[d] = (v[d] ^ v[a]).rotate_right(16);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(12);
        v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
        v[d] = (v[d] ^ v[a]).rotate_right(8);
        v[c] = v[c].wrapping_add(v[d]);
        v[b] = (v[b] ^ v[c]).rotate_right(7);
    }
    let bytes = project.as_bytes();
    let mut h = IV;
    h[0] ^= 0x0101_0008;
    let blocks = bytes.len().div_ceil(64).max(1);
    for index in 0..blocks {
        let start = index * 64;
        let end = (start + 64).min(bytes.len());
        let mut block = [0u8; 64];
        block[..end - start].copy_from_slice(&bytes[start..end]);
        let mut m = [0u32; 16];
        for (word, chunk) in m.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_le_bytes(*chunk);
        }
        let mut v = [0u32; 16];
        v[..8].copy_from_slice(&h);
        v[8..].copy_from_slice(&IV);
        let count = end as u64;
        v[12] ^= count as u32;
        v[13] ^= (count >> 32) as u32;
        if index + 1 == blocks {
            v[14] = !v[14];
        }
        for s in SIGMA {
            for (positions, offset) in [
                ([0, 4, 8, 12], 0),
                ([1, 5, 9, 13], 2),
                ([2, 6, 10, 14], 4),
                ([3, 7, 11, 15], 6),
                ([0, 5, 10, 15], 8),
                ([1, 6, 11, 12], 10),
                ([2, 7, 8, 13], 12),
                ([3, 4, 9, 14], 14),
            ] {
                mix(&mut v, positions, m[s[offset]], m[s[offset + 1]]);
            }
        }
        for i in 0..8 {
            h[i] ^= v[i] ^ v[i + 8];
        }
    }
    let digest = h[0].to_le_bytes();
    format!("{:02x}{:02x}", digest[0], digest[1])
}

/// Build `<project>-<role>[-<seat>][-<instance>]`, preserving role/seat
/// and hashing every project name that slugification or truncation loses.
pub fn agent_name(
    project: &str,
    role: &str,
    instance: Option<&str>,
    seat: &str,
    short: &str,
) -> Result<String, RunnerError> {
    let invalid =
        |reason: String| RunnerError::Refused(format!("invalid herdr agent name: {reason}"));
    let mut tail = role.to_string();
    for (kind, value) in [
        ("seat code", (!seat.is_empty()).then_some(seat)),
        ("instance", instance),
    ] {
        if let Some(value) = value {
            let part = slugify(value, MAX_LEN, false);
            if part.is_empty() {
                return Err(invalid(format!(
                    "{kind} {value:?} contains no usable characters"
                )));
            }
            tail.push('-');
            tail.push_str(&part);
        }
    }
    let budget = MAX_LEN
        .checked_sub(tail.len() + 1)
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            invalid("role, seat and instance leave no room for a project prefix".into())
        })?;
    let slug = if !short.is_empty() {
        let slug = slugify(short, budget, true);
        if slug.is_empty() {
            return Err(invalid(format!(
                "project short {short:?} contains no usable characters"
            )));
        }
        slug
    } else {
        let full = slugify(project, MAX_LEN, true);
        if full == project && full.len() <= budget {
            full
        } else {
            let tag = disambiguator(project);
            let keep = budget.saturating_sub(tag.len() + 1).min(full.len());
            let head = full[..keep].trim_end_matches(['-', '_']);
            let slug = if !head.is_empty() {
                format!("{head}-{tag}")
            } else if tag.starts_with(|c: char| c.is_ascii_digit()) {
                format!("p{tag}")
            } else {
                tag
            };
            if slug.len() > budget {
                return Err(invalid(format!("project {project:?} needs {} characters to stay distinguishable, but only {budget} remain", slug.len())));
            }
            slug
        }
    };
    let name = if slug.is_empty() {
        tail
    } else {
        format!("{slug}-{tail}")
    };
    if name.len() > MAX_LEN
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
    {
        return Err(invalid(format!(
            "{name:?} must match ^[a-z][a-z0-9_-]{{0,31}}$"
        )));
    }
    Ok(name)
}

/// Retry with the original project metadata so each instance has its own
/// project budget; appending to an already truncated base can overflow.
pub fn unique_agent_name(
    project: &str,
    role: &str,
    seat: &str,
    short: &str,
    taken: &HashSet<String>,
) -> Result<String, RunnerError> {
    for instance in 1..100 {
        let suffix = instance.to_string();
        let name = agent_name(
            project,
            role,
            (instance > 1).then_some(suffix.as_str()),
            seat,
            short,
        )?;
        if !taken.contains(&name) {
            return Ok(name);
        }
    }
    Err(RunnerError::Refused("no free agent name".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_agent_name_vectors() {
        // Expected bytes evaluated from the supplied py-naming.py.
        for (project, role, instance, seat, short, expected) in [
            ("", "expl", None, "sb", "", "expl-sb"),
            ("sandbox", "expl", None, "sb", "", "sandbox-expl-sb"),
            (
                "My Project",
                "fix",
                None,
                "cl",
                "",
                "my-project-3d7c-fix-cl",
            ),
            (
                "my_project",
                "fix",
                None,
                "cl",
                "",
                "my-project-e995-fix-cl",
            ),
            ("123", "expl", Some("2"), "7", "", "p123-714c-expl-7-2"),
            ("日本語", "expl", None, "", "", "p79a3-expl"),
            ("---", "expl", None, "", "", "p398e-expl"),
            (
                "Anything",
                "buil",
                Some("2"),
                "CL__Main",
                "app",
                "app-buil-cl-main-2",
            ),
            ("İstanbul", "expl", None, "", "", "i-stanbul-6c15-expl"),
            ("foo---bar", "expl", None, "", "", "foo-bar-f31a-expl"),
        ] {
            assert_eq!(
                agent_name(project, role, instance, seat, short).unwrap(),
                expected
            );
        }
        for (project, expected) in [
            ("a".repeat(70), "aaaaaaaaaaaaaaaaaaa-563f-buil-cl"),
            ("x".repeat(64), "xxxxxxxxxxxxxxxxxxx-6c4a-buil-cl"),
            ("a".repeat(200), "aaaaaaaaaaaaaaaaaaa-2461-buil-cl"),
        ] {
            assert_eq!(
                agent_name(&project, "buil", None, "cl", "").unwrap(),
                expected
            );
        }
    }

    #[test]
    fn collision_rebudgets_the_project_without_truncating_role_or_seat() {
        let project = "a".repeat(70);
        let base = agent_name(&project, "buil", None, "cl", "").unwrap();
        let taken = HashSet::from([base, "aaaaaaaaaaaaaaaaa-563f-buil-cl-2".into()]);
        assert_eq!(
            unique_agent_name(&project, "buil", "cl", "", &taken).unwrap(),
            "aaaaaaaaaaaaaaaaa-563f-buil-cl-3"
        );
    }

    #[test]
    fn rejects_empty_suffixes_and_an_insufficient_project_budget() {
        assert!(agent_name("p", "buil", Some("!!!"), "", "").is_err());
        assert!(agent_name("p", "buil", None, "!!!", "").is_err());
        assert!(agent_name("p", "buil", None, "", "!!!").is_err());
        assert!(agent_name("My Project", "buil", None, &"s".repeat(23), "").is_err());
        assert!(agent_name("p", "buil", None, &"s".repeat(30), "").is_err());
        assert!(agent_name("p", "BAD", None, "", "").is_err());
    }

    #[test]
    fn python_computed_names_for_real_looking_projects() {
        // Expected bytes from `python3 -c` importing
        // `multiverse.naming.agent_name` / `roles.short_name` in the
        // frozen Python reference. `orch` is what Python writes into a
        // live Nexus pane; `nexu` is the same call with SHORT_NAMES
        // patched to the current code (same length, so the slug/hash is
        // identical).
        for (project, role, seat, short, expected) in [
            ("sandbox", "orch", "ne", "", "sandbox-orch-ne"),
            ("sandbox", "nexu", "ne", "", "sandbox-nexu-ne"),
            ("sandbox", "orch", "al", "", "sandbox-orch-al"),
            ("sandbox", "nexu", "al", "", "sandbox-nexu-al"),
            ("sandbox", "nexu", "cl", "sb", "sb-nexu-cl"),
            ("multiverse", "orch", "cl", "", "multiverse-orch-cl"),
            ("workshop", "orch", "cl", "", "workshop-orch-cl"),
            ("My Project", "orch", "cl", "", "my-project-3d7c-orch-cl"),
            ("My Project", "nexu", "cl", "", "my-project-3d7c-nexu-cl"),
            (
                "sample-workspace",
                "nexu",
                "cl",
                "",
                "sample-workspace-nexu-cl",
            ),
            (
                "WidgetFarmConverter",
                "orch",
                "cl",
                "",
                "widgetfarmconverter-81e1-orch-cl",
            ),
            (
                "WidgetFarmConverter",
                "nexu",
                "cl",
                "",
                "widgetfarmconverter-81e1-nexu-cl",
            ),
            ("demo", "nexu", "gr", "dm", "dm-nexu-gr"),
            ("sandbox", "fix", "cl", "", "sandbox-fix-cl"),
            ("sandbox", "buil", "cl", "", "sandbox-buil-cl"),
            ("sandbox", "expl", "gr", "", "sandbox-expl-gr"),
            ("sandbox", "scou", "gr", "", "sandbox-scou-gr"),
            ("workshop", "buil", "cl", "", "workshop-buil-cl"),
        ] {
            assert_eq!(
                agent_name(project, role, None, seat, short).unwrap(),
                expected,
                "{project} {role} {seat} {short}"
            );
        }
        let long = "a".repeat(70);
        assert_eq!(
            agent_name(&long, "orch", None, "cl", "").unwrap(),
            "aaaaaaaaaaaaaaaaaaa-563f-orch-cl"
        );
        assert_eq!(
            agent_name(&long, "nexu", None, "cl", "").unwrap(),
            "aaaaaaaaaaaaaaaaaaa-563f-nexu-cl"
        );
    }
}
