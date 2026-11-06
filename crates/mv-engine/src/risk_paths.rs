//! Glob-to-regex translation for `[review_policy].review_risk_paths`
//! (card `20261102-b-9cc8c7`): a review-dispatch-time concern, so it lives
//! here rather than in `mv-config`, which only parses the table's shape.
//!
//! Ports `config.py`'s `_segment_to_regex`/`_glob_to_regex`. `PurePosixPath
//! .match` gives `**` the wrong meaning for a risk-path glob — it does not
//! let a standalone `**` absorb zero directories — so Python builds its own
//! anchored regex instead, and this is the same translation.
//!
//! [`risk_areas_touched`] (card `20261015-fixer-ea95a9`, RISK AUTO-CLASSIFY
//! continued) adds the content-pattern half: `config.py`'s
//! `DEFAULT_RISK_RULES`' `content_patterns`/`RISK_CONTENT_PATTERNS` and
//! `_added_lines`, ported verbatim. Not configurable from
//! `[review_policy]`, same as Python: a regex matched against diff bodies
//! is not something a human should casually hand-edit, so an area a fleet
//! overrides via `review_risk_paths`'s path globs keeps this table's
//! content signal regardless of what it did to that area's path globs.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use regex::Regex;

/// One path segment's glob translated to regex source: `*` and `?` keep
/// their shell meaning (never crossing `/`), everything else is literal.
fn segment_to_regex(segment: &str) -> String {
    let mut out = String::new();
    for ch in segment.chars() {
        match ch {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            _ => out.push_str(&regex::escape(&ch.to_string())),
        }
    }
    out
}

/// Translate a risk-path glob to an anchored regex, so `**` gets its real
/// glob meaning instead of a plain path-match's: a `**` that stands alone
/// as a whole path segment matches zero or more *whole* segments, anywhere
/// in the pattern — leading, trailing, or in the middle — so
/// `prisma/migrations/**` reaches `prisma/migrations/20240101_init/
/// migration.sql` the way a human reading the glob expects. `*` and `?`
/// stay scoped to a single segment, same as a shell glob, via
/// `segment_to_regex`.
///
/// Matching is a full match against the whole candidate path (anchored at
/// both ends), mirroring Python's `pattern.fullmatch(path)`.
pub fn glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let mut segments: Vec<&str> = Vec::new();
    for seg in glob.split('/') {
        if seg == "**" && segments.last() == Some(&"**") {
            continue; // consecutive '**' are equivalent to one
        }
        segments.push(seg);
    }

    if segments == ["**"] {
        return Regex::new(r"^.*$");
    }

    let mut pieces = String::from("^");
    let mut prev_absorbed_slash = false;
    for (i, seg) in segments.iter().enumerate() {
        if *seg == "**" {
            if i == 0 {
                // Leading '**/': zero or more "segment/" chunks, each
                // carrying its own trailing slash — so a following literal
                // needs no separate connector, and zero reps leaves none.
                pieces.push_str(r"(?:[^/]+/)*");
                prev_absorbed_slash = true;
            } else {
                // Trailing or mid-pattern '**': zero or more "/segment"
                // chunks, each carrying its own leading slash instead, so
                // the literal before it needs no separate connector.
                pieces.push_str(r"(?:/[^/]+)*");
                prev_absorbed_slash = false;
            }
            continue;
        }
        if i > 0 && !prev_absorbed_slash {
            pieces.push('/');
        }
        pieces.push_str(&segment_to_regex(seg));
        prev_absorbed_slash = false;
    }
    pieces.push('$');
    Regex::new(&pieces)
}

/// Whether `path` matches the risk-path `glob`. `false` on a glob so
/// malformed it cannot compile to a regex at all, rather than panicking —
/// callers see that the same way as a glob that simply does not match.
pub fn glob_matches(glob: &str, path: &str) -> bool {
    glob_to_regex(glob)
        .map(|pattern| pattern.is_match(path))
        .unwrap_or(false)
}

/// Port of `config.py`'s `DEFAULT_RISK_RULES`' `content_patterns` half, one
/// row per area that has one — `migration`, `auth`, `deletion`, `secrets`,
/// and `release` are the only areas Python gives a content signal; `mcp`
/// stays path-only in both.
const DEFAULT_RISK_CONTENT_PATTERNS: &[(&str, &[&str])] = &[
    (
        "migration",
        &[
            r"\balter\s+table\b",
            r"\bcreate\s+table\b",
            r"\bdrop\s+table\b",
            r"\badd_column\b",
            r"\bdrop_column\b",
            r"\bdef\s+upgrade\s*\(",
            r"\bdef\s+downgrade\s*\(",
        ],
    ),
    (
        "auth",
        &[
            r"\bdef\s+login\b",
            r"\bdef\s+authenticate\b",
            r"\bcheck_password\b",
            r"\bverify_password\b",
            r"\bis_authenticated\b",
            r"\bhas_permission\b",
            r"\brequire_permission\b",
            r"@login_required",
        ],
    ),
    (
        "deletion",
        &[
            r"rm\s+-[a-z]*r[a-z]*f[a-z]*\b",
            r"rm\s+-[a-z]*f[a-z]*r[a-z]*\b",
            r"docker\s+volume\s+rm\b",
            r"docker\s+system\s+prune\b",
            r"\bdrop\s+database\b",
            r"\btruncate\s+table\b",
        ],
    ),
    (
        "secrets",
        &[
            r"api[_-]?key\s*=",
            r"secret[_-]?key\s*=",
            r"access[_-]?key\s*=",
            r"private[_-]?key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----",
        ],
    ),
    (
        "release",
        &[
            r"\bkubectl\s+(apply|rollout|delete)\b",
            r"\bhelm\s+(upgrade|install|rollback)\b",
            r"\bdocker\s+push\b",
        ],
    ),
];

/// [`DEFAULT_RISK_CONTENT_PATTERNS`], compiled once — mirrors `config.py`'s
/// module-level `RISK_CONTENT_PATTERNS`, a dict built once at import time.
fn content_patterns() -> &'static HashMap<&'static str, Vec<Regex>> {
    static PATTERNS: OnceLock<HashMap<&'static str, Vec<Regex>>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        DEFAULT_RISK_CONTENT_PATTERNS
            .iter()
            .map(|(area, patterns)| {
                let compiled = patterns
                    .iter()
                    .map(|pattern| {
                        Regex::new(&format!("(?i){pattern}")).expect("static regex compiles")
                    })
                    .collect();
                (*area, compiled)
            })
            .collect()
    })
}

/// Port of `config.py`'s `RISK_QUESTIONS`: one question per
/// [`crate::lane_diff`]-adjacent risk area a `[review_policy]` can name in
/// `require_cross_vendor_for` — what the `dispatch --dry-run` reviewer card
/// (card `20261102-b-82436e`) asks the reviewer to specifically check for,
/// alongside the diff itself (py-dispatch.py:1065-1068).
const RISK_QUESTIONS: &[(&str, &str)] = &[
    (
        "security",
        "Does this change how input is validated, sanitized, or trusted?",
    ),
    (
        "auth",
        "Does this change who is allowed to do what, or how identity is checked?",
    ),
    (
        "migration",
        "Does this change data already on disk or in a database, and is that change reversible?",
    ),
    (
        "deletion",
        "Can this delete or discard something a human cannot get back?",
    ),
    (
        "secrets",
        "Does this touch a credential, token, or key — reading, writing, or logging one?",
    ),
    (
        "release",
        "Does this change what gets shipped, published, or rolled out, and to whom?",
    ),
];

/// Port of `config.py`'s `render_risk_questions`: one question per
/// `require_cross_vendor_for` entry, in that list's own order. An area
/// outside `RISK_QUESTIONS` (a fleet's own invented label, already
/// surfaced as a warning by config loading) gets a generic question rather
/// than being dropped — the label itself is still useful to a reviewer even
/// without a bespoke phrasing. The generic question quotes the area the way
/// Python's `f"...{area!r}..."` does — single-quoted, not Rust's own
/// double-quoted `{area:?}` — so a `dispatch --dry-run` card matches
/// Python's byte for byte.
pub fn risk_questions(require_cross_vendor_for: &[String]) -> Vec<String> {
    require_cross_vendor_for
        .iter()
        .map(|area| {
            RISK_QUESTIONS
                .iter()
                .find(|(known, _)| known == area)
                .map(|(_, question)| question.to_string())
                .unwrap_or_else(|| format!("Does this touch '{area}'? If so, how?"))
        })
        .collect()
}

/// Port of `config.py`'s `_added_lines`: just the lines a diff
/// *introduces* — a single leading `+`, the `+++` file-header line
/// excluded — joined back into one blob a content pattern can be searched
/// against. Context and removed (`-`) lines are deliberately excluded: a
/// risk pattern is about what the change brings in, not what a file
/// already contained before it, or lines it is actively taking back out.
pub fn added_lines(diff_text: &str) -> String {
    diff_text
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .map(|line| &line[1..])
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which of `risk_paths`' areas this lane hits, in the map's own key order
/// — port of `config.py`'s `risk_areas_touched` (card `20260930-b-eb20d3`,
/// RISK AUTO-CLASSIFY; content matching added by card
/// `20261015-fixer-ea95a9`).
///
/// A file matching any one glob in an area's list is enough to count that
/// whole area as touched, via [`glob_matches`]. `diff_text` (the lane's
/// unified diff, e.g. `LaneDiff::body_text`): when non-empty, an area with
/// no path hit is still counted touched if one of
/// `DEFAULT_RISK_CONTENT_PATTERNS`' regexes for it matches a line the
/// diff *adds* (see [`added_lines`]) — this is what catches a risky change
/// whose path gives no hint at all (`src/login.rs` implementing
/// authentication, a script's new `rm -rf` line), independent of which
/// paths matched. Left at `""`, no content pattern ever matches, so a
/// path-only caller keeps exactly the glob-only behavior.
///
/// An empty result means the lane hits no configured risk area at all —
/// `review-policy --lane` reads that as low risk, batch list only.
pub fn risk_areas_touched(
    risk_paths: &BTreeMap<String, Vec<String>>,
    touched_files: &[String],
    diff_text: &str,
) -> Vec<String> {
    let added = added_lines(diff_text);
    risk_paths
        .iter()
        .filter(|(area, globs)| {
            let path_hit = touched_files
                .iter()
                .any(|path| globs.iter().any(|glob| glob_matches(glob, path)));
            if path_hit {
                return true;
            }
            !added.is_empty()
                && content_patterns()
                    .get(area.as_str())
                    .is_some_and(|patterns| patterns.iter().any(|pattern| pattern.is_match(&added)))
        })
        .map(|(area, _)| area.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_double_star_matches_everything() {
        assert!(glob_matches("**", "anything/at/all.rs"));
        assert!(glob_matches("**", "top.rs"));
    }

    #[test]
    fn leading_double_star_absorbs_zero_or_more_directories() {
        assert!(glob_matches("**/migrations/*.sql", "migrations/init.sql"));
        assert!(glob_matches(
            "**/migrations/*.sql",
            "prisma/migrations/init.sql"
        ));
        assert!(glob_matches(
            "**/migrations/*.sql",
            "a/b/c/migrations/init.sql"
        ));
    }

    #[test]
    fn trailing_double_star_absorbs_zero_or_more_directories() {
        assert!(glob_matches("prisma/migrations/**", "prisma/migrations"));
        assert!(glob_matches(
            "prisma/migrations/**",
            "prisma/migrations/20240101_init/migration.sql"
        ));
    }

    #[test]
    fn star_and_question_mark_stay_within_one_segment() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/nested/main.rs"));
        assert!(glob_matches("src/lib?.rs", "src/liba.rs"));
        assert!(!glob_matches("src/lib?.rs", "src/lib.rs"));
    }

    #[test]
    fn literal_characters_are_escaped() {
        assert!(glob_matches("a.b/c+d", "a.b/c+d"));
        assert!(!glob_matches("a.b/c+d", "aXb/c+d"));
    }

    #[test]
    fn consecutive_double_stars_collapse_to_one() {
        assert!(glob_matches("a/**/**/b", "a/x/y/b"));
        assert!(glob_matches("a/**/**/b", "a/b"));
    }

    #[test]
    fn full_match_is_anchored_at_both_ends() {
        assert!(!glob_matches("src/*.rs", "lib/src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/main.rs.bak"));
    }

    // -- card 20261015-fixer-ea95a9: content-pattern risk classification --

    fn risk_paths(entries: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        entries
            .iter()
            .map(|(area, globs)| {
                (
                    (*area).to_owned(),
                    globs.iter().map(|g| (*g).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn added_lines_keeps_only_introduced_lines_header_excluded() {
        let diff = "diff --git a/f b/f\n\
                     --- a/f\n\
                     +++ b/f\n\
                     @@ -1,2 +1,2 @@\n\
                     -old line\n\
                     +new line\n\
                     \u{20}context line\n";
        assert_eq!(added_lines(diff), "new line");
    }

    #[test]
    fn a_path_with_no_hint_still_classifies_via_a_content_pattern() {
        // `src/login.rs` matches no configured `auth` glob, but its added
        // line defines `login`, the same gap Python's card closed.
        let paths = risk_paths(&[("auth", &["**/auth/**"])]);
        let diff = "+++ b/src/login.rs\n+fn login_handler() {\n+    def login():\n";
        let touched = vec!["src/login.rs".to_string()];
        assert_eq!(risk_areas_touched(&paths, &touched, diff), vec!["auth"]);
    }

    #[test]
    fn an_rm_rf_line_classifies_as_deletion_regardless_of_path() {
        let paths = risk_paths(&[("deletion", &["**/*delete*"])]);
        let diff = "+++ b/scripts/cleanup.sh\n+rm -rf /tmp/scratch\n";
        let touched = vec!["scripts/cleanup.sh".to_string()];
        assert_eq!(risk_areas_touched(&paths, &touched, diff), vec!["deletion"]);
    }

    #[test]
    fn a_removed_or_context_line_never_trips_a_content_pattern() {
        // `rm -rf` only ever appears on a removed or context line — the
        // diff is *taking it out*, not adding it.
        let paths = risk_paths(&[("deletion", &["**/*delete*"])]);
        let diff = "+++ b/scripts/cleanup.sh\n-rm -rf /tmp/scratch\n unchanged\n";
        let touched = vec!["scripts/cleanup.sh".to_string()];
        assert!(risk_areas_touched(&paths, &touched, diff).is_empty());
    }

    #[test]
    fn empty_diff_text_never_matches_content_only_path_behavior_unchanged() {
        let paths = risk_paths(&[("auth", &["**/auth/**"])]);
        let touched = vec!["src/login.rs".to_string()];
        assert!(risk_areas_touched(&paths, &touched, "").is_empty());
    }

    #[test]
    fn a_secrets_pattern_fires_on_an_added_private_key_header() {
        let paths = risk_paths(&[("secrets", &["**/*secret*"])]);
        let diff = "+++ b/config/app.rs\n+-----BEGIN RSA PRIVATE KEY-----\n";
        let touched = vec!["config/app.rs".to_string()];
        assert_eq!(risk_areas_touched(&paths, &touched, diff), vec!["secrets"]);
    }

    #[test]
    fn a_migration_pattern_fires_on_an_added_create_table_statement() {
        let paths = risk_paths(&[("migration", &["migrations/**"])]);
        let diff = "+++ b/src/schema.rs\n+CREATE TABLE users (id INT);\n";
        let touched = vec!["src/schema.rs".to_string()];
        assert_eq!(
            risk_areas_touched(&paths, &touched, diff),
            vec!["migration"]
        );
    }

    #[test]
    fn a_release_pattern_fires_on_an_added_docker_push() {
        let paths = risk_paths(&[("release", &["**/deploy/**"])]);
        let diff = "+++ b/ci/publish.sh\n+docker push registry.example/app:latest\n";
        let touched = vec!["ci/publish.sh".to_string()];
        assert_eq!(risk_areas_touched(&paths, &touched, diff), vec!["release"]);
    }

    #[test]
    fn a_path_hit_alone_still_classifies_with_no_diff_text_needed() {
        let paths = risk_paths(&[("mcp", &["**/mcp/**"])]);
        let touched = vec!["src/mcp/server.rs".to_string()];
        assert_eq!(risk_areas_touched(&paths, &touched, ""), vec!["mcp"]);
    }

    #[test]
    fn area_order_follows_the_map_not_which_signal_fired() {
        let paths = risk_paths(&[
            ("auth", &["**/auth/**"]),
            ("deletion", &["**/*delete*"]),
            ("migration", &["migrations/**"]),
        ]);
        let diff = "+++ b/src/login.rs\n+def login():\n+++ b/scripts/x.sh\n+rm -rf /tmp\n";
        let touched = vec!["src/login.rs".to_string(), "scripts/x.sh".to_string()];
        assert_eq!(
            risk_areas_touched(&paths, &touched, diff),
            vec!["auth", "deletion"]
        );
    }

    // -- card 20261102-b-82436e: dispatch --dry-run's risk-question block --

    #[test]
    fn a_known_area_gets_its_own_bespoke_question() {
        let areas = vec!["auth".to_string(), "secrets".to_string()];
        assert_eq!(
            risk_questions(&areas),
            vec![
                "Does this change who is allowed to do what, or how identity is checked?"
                    .to_string(),
                "Does this touch a credential, token, or key — reading, writing, or logging one?"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn an_unknown_area_gets_a_generic_single_quoted_question() {
        let areas = vec!["infra".to_string()];
        assert_eq!(
            risk_questions(&areas),
            vec!["Does this touch 'infra'? If so, how?".to_string()]
        );
    }

    #[test]
    fn questions_follow_the_input_list_order_not_risk_questions_declaration_order() {
        let areas = vec!["release".to_string(), "security".to_string()];
        let questions = risk_questions(&areas);
        assert_eq!(questions.len(), 2);
        assert!(questions[0].starts_with("Does this change what gets shipped"));
        assert!(questions[1].starts_with("Does this change how input is validated"));
    }

    #[test]
    fn empty_input_yields_no_questions() {
        assert!(risk_questions(&[]).is_empty());
    }
}
