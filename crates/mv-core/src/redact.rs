//! Conservative redaction for credential-shaped text.
//!
//! Ports `multiverse/redact.py` at the match level. The Python patterns
//! use negative lookaround and a conditional backreference; those
//! features force `fancy-regex`'s backtracking VM, whose `replace_all`
//! unwraps on `RuntimeError::BacktrackLimitExceeded` (card
//! `20261123-b-7f6462`). Every pattern here is rewritten without
//! lookaround or backreferences so the `regex` crate's linear engine
//! can run it. Boundary characters are never consumed: each pattern
//! matches the bare token, and a char-aware check of the character
//! before `m.start()` and after `m.end()` stands in for the lookbehind
//! / lookahead. A failed check skips the match and the search resumes
//! one character later, so an adjacent token still has its leading
//! boundary.

use std::sync::OnceLock;

use regex::{Captures, Regex};

const REDACTED_MARKER: &str = "…[redacted]";

fn pem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)(?P<begin>-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----).*?(?P<end>-----END [A-Z0-9 ]*PRIVATE KEY-----)",
        )
        .expect("static regex compiles")
    })
}

fn prefix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(sk-ant-|sk-|ghp_|gho_|github_pat_|glpat-|xox[bpas]-)([A-Za-z0-9._~+/=-]{4,})")
            .expect("static regex compiles")
    })
}

fn aws_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{14,16}").expect("static regex compiles")
    })
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)Bearer[ \t]+([A-Za-z0-9._~+/=-]{4,})").expect("static regex compiles")
    })
}

fn credential_stem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|_)(?:password|passwd|secret|token|api_?key|key)(?:_|$)")
            .expect("static regex compiles")
    })
}

/// Database/indexing terms that end or begin with `key` but are not
/// credential-shaped: the generic `key` stem added to [`credential_stem_re`]
/// and [`camel_credential_suffix_re`] would otherwise also catch
/// `foreign_key`, `primary_key`, `sort_key`, `cache_key`, `partition_key`,
/// and `key_count`. Checked before either stem regex in [`is_credential_key`].
fn non_credential_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:foreign|primary|sort|cache|partition)_key$|^key_count$")
            .expect("static regex compiles")
    })
}

fn camel_credential_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|_)(?:password|passwd|secret|token|api_?key|secret_key|secret_access_key|key)$",
        )
        .expect("static regex compiles")
    })
}

/// Assignment of a credential-shaped key to a value.
///
/// Written without lookbehind or a matching-quote conditional so the
/// `regex` crate's linear engine can run it. Groups 1/2/3 are the
/// double-quoted, single-quoted, or bare key. Group 4 is the separator.
/// Groups 5/6/7 are the double-quoted, single-quoted, or bare value.
/// The leading `(?<![A-Za-z0-9_-])` boundary is checked in
/// [`replace_bounded`], not consumed by the pattern.
fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:"([A-Za-z][A-Za-z0-9_-]{0,63})"|'([A-Za-z][A-Za-z0-9_-]{0,63})'|([A-Za-z][A-Za-z0-9_-]{0,63}))(\s*[:=]\s*)(?:"([^"'\r\n]*)"|'([^"'\r\n]*)'|([^\s,;]*))"#,
        )
        .expect("static regex compiles")
    })
}

/// Insert `_` at the camelCase word boundaries the previous
/// lookaround regex `(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])`
/// matched: between a lowercase letter/digit and the following
/// uppercase one (`apiKey` -> `api_Key`), or between two consecutive
/// uppercase letters where the second starts a new word (`myAPIKey` ->
/// `my_API_Key`). [`is_credential_key`] then runs the snake_case stem
/// regexes against the result.
fn insert_camel_boundaries(key: &str) -> String {
    let bytes = key.as_bytes();
    let mut out = String::with_capacity(key.len() + 8);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            let prev = bytes[i - 1];
            let lower_or_digit_then_upper =
                (prev.is_ascii_lowercase() || prev.is_ascii_digit()) && b.is_ascii_uppercase();
            let upper_run_then_word = prev.is_ascii_uppercase()
                && b.is_ascii_uppercase()
                && bytes.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if lower_or_digit_then_upper || upper_run_then_word {
                out.push('_');
            }
        }
        out.push(char::from(b));
    }
    out
}

/// Mirrors `_is_credential_key`: a delimited credential stem (`password`,
/// `secret`, `token`, `api_key`/`apikey`, a generic `key`, …) or a camelCase
/// key ending in one, as opposed to a key that merely contains those letters
/// (`passwordlessEnabled`, `tokenizerCount`) or a database/indexing term that
/// happens to end or begin with `key` (`foreign_key`, `key_count`).
fn is_credential_key(key: &str) -> bool {
    let delimited = key.replace('-', "_");
    if non_credential_key_re().is_match(&delimited) {
        return false;
    }
    if credential_stem_re().is_match(&delimited) {
        return true;
    }
    let normalized = insert_camel_boundaries(&delimited);
    if non_credential_key_re().is_match(&normalized) {
        return false;
    }
    camel_credential_suffix_re().is_match(&normalized)
}

fn char_before(text: &str, index: usize) -> Option<char> {
    text.get(..index)?.chars().next_back()
}

fn char_after(text: &str, index: usize) -> Option<char> {
    text.get(index..)?.chars().next()
}

fn next_char_index(text: &str, index: usize) -> usize {
    match text.get(index..).and_then(|rest| rest.chars().next()) {
        Some(c) => index + c.len_utf8(),
        None => index + 1,
    }
}

fn is_ascii_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_bearer_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-')
}

fn leading_ascii_alnum_ok(text: &str, start: usize, _end: usize) -> bool {
    !char_before(text, start).is_some_and(|c| c.is_ascii_alphanumeric())
}

fn aws_boundary_ok(text: &str, start: usize, end: usize) -> bool {
    leading_ascii_alnum_ok(text, start, end)
        && !char_after(text, end).is_some_and(|c| c.is_ascii_alphanumeric())
}

fn bearer_boundary_ok(text: &str, start: usize, end: usize) -> bool {
    leading_ascii_alnum_ok(text, start, end)
        && !char_after(text, end).is_some_and(is_bearer_token_char)
}

fn leading_ident_ok(text: &str, start: usize, _end: usize) -> bool {
    !char_before(text, start).is_some_and(is_ascii_ident)
}

/// Replace matches of `re` that pass `boundary_ok`. Matches that fail
/// the check are left in place; the search resumes one character later
/// so a later token can still see a leading boundary the previous
/// candidate did not consume.
fn replace_bounded(
    text: &str,
    re: &Regex,
    boundary_ok: impl Fn(&str, usize, usize) -> bool,
    replace: impl Fn(&Captures) -> String,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut search_at = 0;
    while search_at < text.len() {
        let Some(caps) = re.captures_at(text, search_at) else {
            break;
        };
        let m = caps.get(0).expect("match always has group 0");
        let start = m.start();
        let end = m.end();
        if start == end {
            search_at = next_char_index(text, start);
            continue;
        }
        if !boundary_ok(text, start, end) {
            search_at = next_char_index(text, start);
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(&replace(&caps));
        last = end;
        search_at = end;
    }
    out.push_str(&text[last..]);
    out
}

fn replace_prefix(caps: &Captures) -> String {
    format!("{}{REDACTED_MARKER}", &caps[1])
}

fn replace_aws(caps: &Captures) -> String {
    let id = caps.get(0).expect("match always has group 0").as_str();
    format!("{}{REDACTED_MARKER}", &id[..4])
}

fn replace_bearer(_: &Captures) -> String {
    format!("Bearer {REDACTED_MARKER}")
}

fn replace_assignment(caps: &Captures) -> String {
    let (keyquote, key) = if let Some(key) = caps.get(1) {
        ("\"", key.as_str())
    } else if let Some(key) = caps.get(2) {
        ("'", key.as_str())
    } else {
        (
            "",
            caps.get(3)
                .expect("assignment regex always has a key group")
                .as_str(),
        )
    };
    if !is_credential_key(key) {
        return caps
            .get(0)
            .expect("match always has group 0")
            .as_str()
            .to_string();
    }
    let separator = &caps[4];
    let quote = if caps.get(5).is_some() {
        "\""
    } else if caps.get(6).is_some() {
        "'"
    } else {
        ""
    };
    format!("{keyquote}{key}{keyquote}{separator}{quote}{REDACTED_MARKER}{quote}")
}

/// Mask common credentials while leaving identifiers and ordinary text intact.
///
/// The patterns intentionally require credential-specific prefixes or key
/// names; generic hex strings such as git SHAs, task ids, and UUIDs are not
/// secrets. Key names may be bare, colon- or equals-separated, quoted (as in
/// JSON), and compound (`SECRET_TOKEN=`, `X-Api-Key:`, `client_secret=`,
/// `clientSecret=`) when a credential stem is delimited or ends a camelCase
/// key. Applying this function again is safe: its own redaction marker is
/// not a credential-shaped value, so the result is idempotent.
pub fn redact_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    let text = pem_re().replace_all(text, |caps: &Captures| {
        format!("{}\n{REDACTED_MARKER}\n{}", &caps["begin"], &caps["end"])
    });

    let text = replace_bounded(
        text.as_ref(),
        prefix_re(),
        leading_ascii_alnum_ok,
        replace_prefix,
    );
    let text = replace_bounded(&text, aws_re(), aws_boundary_ok, replace_aws);
    let text = replace_bounded(&text, bearer_re(), bearer_boundary_ok, replace_bearer);
    replace_bounded(&text, assignment_re(), leading_ident_ok, replace_assignment)
}

/// Alias matching the Python module's `redact = redact_text`.
pub fn redact(text: &str) -> String {
    redact_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_additional_token_prefixes_without_touching_identifiers() {
        let identifiers = "0123456789abcdef0123456789abcdef01234567 20261016-fixer-abcdef";
        for token in [
            concat!("glpat-", "abcdefghijklmnop"),
            concat!("ASIA", "1234567890ABCD"),
            concat!("ASIA", "1234567890ABCDEF"),
        ] {
            let text = format!("{token} {identifiers}");
            let redacted = redact_text(&text);
            assert!(
                !redacted.contains(token),
                "{token:?} leaked into {redacted:?}"
            );
            assert!(redacted.contains(REDACTED_MARKER));
            assert!(redacted.contains(identifiers));
            assert_eq!(redact_text(&redacted), redacted);
        }
    }

    #[test]
    fn masks_camel_case_credentials_but_not_ordinary_camel_case_fields() {
        for key in [
            "apiKey",
            "accessToken",
            "refreshToken",
            "clientSecret",
            "dbPassword",
            "clientAPIKey",
            "myApiKey",
            "awsSecretAccessKey",
        ] {
            let text = format!(r#"{key}=supersecretvalue "{key}": "supersecretvalue""#);
            let redacted = redact_text(&text);
            assert!(!redacted.contains("supersecretvalue"));
            assert!(redacted.contains(&format!("{key}={REDACTED_MARKER}")));
            assert!(redacted.contains(&format!(r#""{key}": "{REDACTED_MARKER}""#)));
            assert_eq!(redact_text(&redacted), redacted);
        }

        let ordinary = "passwordlessEnabled=true tokenizerCount=500 apiKeyboard=standard";
        assert_eq!(redact_text(ordinary), ordinary);
    }

    #[test]
    fn masks_generic_key_credential_stems_without_touching_identifiers() {
        for key in [
            "PRIVATE_KEY",
            "SSH_KEY",
            "ACCESS_KEY",
            "DEPLOY_KEY",
            "SIGNING_KEY",
            "ENCRYPTION_KEY",
        ] {
            let text = format!("{key}=fake1234567890abcdef");
            let redacted = redact_text(&text);
            assert!(!redacted.contains("fake1234567890abcdef"));
            assert!(redacted.contains(&format!("{key}={REDACTED_MARKER}")));
            assert_eq!(redact_text(&redacted), redacted);
        }

        for key in ["privateKey", "accessKey"] {
            let text = format!(r#"{key}=fake1234567890abcdef "{key}": "fake1234567890abcdef""#);
            let redacted = redact_text(&text);
            assert!(!redacted.contains("fake1234567890abcdef"));
            assert!(redacted.contains(&format!("{key}={REDACTED_MARKER}")));
            assert!(redacted.contains(&format!(r#""{key}": "{REDACTED_MARKER}""#)));
            assert_eq!(redact_text(&redacted), redacted);
        }
    }

    #[test]
    fn preserves_non_credential_key_suffixed_names() {
        // Every value below is the same credential-shaped string
        // `masks_generic_key_credential_stems_without_touching_identifiers`
        // uses to prove the generic `key` stem redacts it; here each name is
        // paired with that value to pin that the denylist, not an
        // innocuous-looking value, is what keeps these names untouched.
        let text = concat!(
            "foreign_key=fake1234567890abcdef primary_key=fake1234567890abcdef ",
            "sort_key=fake1234567890abcdef cache_key=fake1234567890abcdef ",
            "partition_key=fake1234567890abcdef key_count=fake1234567890abcdef ",
            "FOREIGN_KEY=fake1234567890abcdef PRIMARY_KEY=fake1234567890abcdef ",
            "SORT_KEY=fake1234567890abcdef CACHE_KEY=fake1234567890abcdef ",
            "PARTITION_KEY=fake1234567890abcdef KEY_COUNT=fake1234567890abcdef ",
            "foreignKey=fake1234567890abcdef primaryKey=fake1234567890abcdef"
        );
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn preserves_ordinary_camel_case_fields_that_merely_contain_credential_letters() {
        for key in [
            "secretName",
            "tokenSupply",
            "secretSauce",
            "passwordPolicy",
            "accessTokenCount",
            "clientSecretName",
            "apiKeyDescription",
            "passwordlessEnabled",
            "tokenizerCount",
            "apiKeyboard",
        ] {
            let text = format!(r#"{key}: ordinary-value "{key}"="ordinary-value""#);
            assert_eq!(redact_text(&text), text);
        }
    }

    #[test]
    fn masks_supported_credentials_without_touching_identifiers() {
        let text = concat!(
            "sk-",
            "fake-1234567890 ",
            "ghp_",
            "fake1234567890 ",
            "xoxb-",
            "fake1234567890 ",
            "AKIA",
            "1234567890ABCDEF ",
            "Bearer ",
            "fake-bearer-token ",
            "password=supersecret api_key='key-secret'\n",
            "-----BEGIN RSA ",
            "PRIVATE KEY-----\nprivate-material\n",
            "-----END RSA ",
            "PRIVATE KEY-----",
        );
        let redacted = redact_text(text);

        for leaked in [
            "fake-1234567890",
            "fake1234567890",
            "1234567890ABCDEF",
            "fake-bearer-token",
            "supersecret",
            "key-secret",
            "private-material",
        ] {
            assert!(
                !redacted.contains(leaked),
                "{leaked:?} leaked into {redacted:?}"
            );
        }
        assert!(redacted.contains(&format!("sk-{REDACTED_MARKER}")));
        assert!(redacted.contains(&format!("AKIA{REDACTED_MARKER}")));
        assert!(redacted.contains(&format!("Bearer {REDACTED_MARKER}")));

        let identifiers = [
            "20261016-fixer-abcdef",
            "0123456789abcdef0123456789abcdef01234567",
            "550e8400-e29b-41d4-a716-446655440000",
        ]
        .join(" ");
        assert_eq!(redact_text(&identifiers), identifiers);
    }

    #[test]
    fn masks_compound_and_colon_credential_assignments() {
        let text = concat!(
            "SECRET_TOKEN=abc123xyz API_TOKEN=abc123xyz AWS_SECRET_ACCESS_KEY=",
            "abc123xyzdef DB_PASSWORD=hunter2hunter2 client_secret=abc123xyzdef\n",
            "password: hunter2hunter2\n",
            "\"password\": \"hunter2hunter2\"\n",
            "X-Api-Key: abc123xyzdef\n",
            "passwordless=notasecret tokenizer_count=500",
        );
        let redacted = redact_text(text);

        for leaked in ["abc123xyz", "abc123xyzdef", "hunter2hunter2"] {
            assert!(!redacted.contains(leaked));
        }
        for expected in [
            "SECRET_TOKEN=…[redacted]",
            "API_TOKEN=…[redacted]",
            "AWS_SECRET_ACCESS_KEY=…[redacted]",
            "DB_PASSWORD=…[redacted]",
            "client_secret=…[redacted]",
            "password: …[redacted]",
            "\"password\": \"…[redacted]\"",
            "X-Api-Key: …[redacted]",
        ] {
            assert!(
                redacted.contains(expected),
                "missing {expected:?} in {redacted:?}"
            );
        }

        assert!(redacted.contains("passwordless=notasecret"));
        assert!(redacted.contains("tokenizer_count=500"));
        assert_eq!(redact_text(&redacted), redacted);
    }

    #[test]
    fn is_idempotent_and_counts_each_masked_occurrence() {
        let value = concat!(
            "sk-",
            "fake-1234567890 token=supersecret and ",
            "sk-",
            "other-0987654321",
        );
        let redacted = redact_text(value);
        assert_eq!(redact_text(&redacted), redacted);
        assert_eq!(redacted.matches("sk-…[redacted]").count(), 2);
    }

    #[test]
    fn masks_short_or_empty_values_for_credential_keys() {
        let text = r#"token=x password="no" api_key='' ordinary=x"#;
        let redacted = redact_text(text);
        assert!(redacted.contains("token=…[redacted]"));
        assert!(redacted.contains(r#"password="…[redacted]""#));
        assert!(redacted.contains("api_key='…[redacted]'"));
        assert!(redacted.contains("ordinary=x"));
    }

    #[test]
    fn empty_input_round_trips_to_empty_output() {
        assert_eq!(redact_text(""), "");
    }

    #[test]
    fn redact_is_an_alias_for_redact_text() {
        assert_eq!(
            redact("token=supersecret"),
            redact_text("token=supersecret")
        );
    }

    #[test]
    fn preserves_boundary_characters_around_masked_tokens() {
        assert_eq!(
            redact_text("(sk-fake-1234567890)"),
            format!("(sk-{REDACTED_MARKER})")
        );
        assert_eq!(
            redact_text("(AKIA1234567890ABCDEF)"),
            format!("(AKIA{REDACTED_MARKER})")
        );
        assert_eq!(
            redact_text("see Bearer fake-bearer-token!"),
            format!("see Bearer {REDACTED_MARKER}!")
        );
    }

    /// Card `20261123-b-7f6462` fix-round: a consuming trailing
    /// `($|[^A-Za-z0-9])` stole the next token's leading boundary, so
    /// `redact_text("AKIA… AKIA…")` leaked the second key. Separators
    /// that main's fancy-regex version also treats as boundaries must
    /// mask both sides; a secret glued inside a longer alnum word must
    /// still not match.
    #[test]
    fn adjacent_secrets_mask_both_sides_of_a_shared_boundary() {
        let akia_a = concat!("AKIA", "ABCDEFGHIJKLMNOP");
        let akia_b = concat!("AKIA", "QRSTUVWXYZ234567");
        let asia_a = concat!("ASIA", "1234567890ABCD");
        let asia_b = concat!("ASIA", "1234567890ABCDEF");
        let bearer_a = concat!("Bearer ", "tok-aaaa-1111");
        let bearer_b = concat!("Bearer ", "tok-bbbb-2222");
        let sk_a = concat!("sk-", "aaaa1111bbbb");
        let sk_b = concat!("sk-", "cccc2222dddd");
        let ghp_a = concat!("ghp_", "aaaa1111bbbb");
        let ghp_b = concat!("ghp_", "cccc2222dddd");
        let xoxb_a = concat!("xoxb-", "aaaa1111bbbb");
        let xoxb_b = concat!("xoxb-", "cccc2222dddd");
        let assign_a = "password=secret1";
        let assign_b = "password=secret2";
        let assign_dq_a = r#""password"="secret1""#;
        let assign_dq_b = r#""password"="secret2""#;
        let assign_sq_a = "'password'='secret1'";
        let assign_sq_b = "'password'='secret2'";

        let masked_akia = format!("AKIA{REDACTED_MARKER}");
        let masked_asia = format!("ASIA{REDACTED_MARKER}");
        let masked_bearer = format!("Bearer {REDACTED_MARKER}");
        let masked_sk = format!("sk-{REDACTED_MARKER}");
        let masked_ghp = format!("ghp_{REDACTED_MARKER}");
        let masked_xoxb = format!("xoxb-{REDACTED_MARKER}");
        let masked_assign = format!("password={REDACTED_MARKER}");
        let masked_assign_dq = format!("\"password\"=\"{REDACTED_MARKER}\"");
        let masked_assign_sq = format!("'password'='{REDACTED_MARKER}'");

        for sep in [" ", ",", ";", "\n"] {
            assert_eq!(
                redact_text(&format!("{akia_a}{sep}{akia_b}")),
                format!("{masked_akia}{sep}{masked_akia}"),
                "AKIA sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{asia_a}{sep}{asia_b}")),
                format!("{masked_asia}{sep}{masked_asia}"),
                "ASIA sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{bearer_a}{sep}{bearer_b}")),
                format!("{masked_bearer}{sep}{masked_bearer}"),
                "Bearer sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{sk_a}{sep}{sk_b}")),
                format!("{masked_sk}{sep}{masked_sk}"),
                "sk- sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{ghp_a}{sep}{ghp_b}")),
                format!("{masked_ghp}{sep}{masked_ghp}"),
                "ghp_ sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{xoxb_a}{sep}{xoxb_b}")),
                format!("{masked_xoxb}{sep}{masked_xoxb}"),
                "xoxb- sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{assign_a}{sep}{assign_b}")),
                format!("{masked_assign}{sep}{masked_assign}"),
                "assignment unquoted sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{assign_dq_a}{sep}{assign_dq_b}")),
                format!("{masked_assign_dq}{sep}{masked_assign_dq}"),
                "assignment double-quoted sep={sep:?}"
            );
            assert_eq!(
                redact_text(&format!("{assign_sq_a}{sep}{assign_sq_b}")),
                format!("{masked_assign_sq}{sep}{masked_assign_sq}"),
                "assignment single-quoted sep={sep:?}"
            );
        }

        // No separator: AWS keys form one alnum word and must not match,
        // matching fancy-regex lookaround. Prefix/Bearer/unquoted
        // assignment greedily consume the second token as part of the
        // first value. Quoted assignments have explicit delimiters, so
        // both still mask.
        assert_eq!(
            redact_text(&format!("{akia_a}{akia_b}")),
            format!("{akia_a}{akia_b}")
        );
        assert_eq!(
            redact_text(&format!("{asia_a}{asia_b}")),
            format!("{asia_a}{asia_b}")
        );
        assert_eq!(redact_text(&format!("{sk_a}{sk_b}")), masked_sk);
        assert_eq!(redact_text(&format!("{ghp_a}{ghp_b}")), masked_ghp);
        assert_eq!(redact_text(&format!("{xoxb_a}{xoxb_b}")), masked_xoxb);
        assert_eq!(
            redact_text(&format!("{bearer_a}{bearer_b}")),
            format!("{masked_bearer} tok-bbbb-2222")
        );
        assert_eq!(
            redact_text(&format!("{assign_a}{assign_b}")),
            masked_assign.as_str()
        );
        assert_eq!(
            redact_text(&format!("{assign_dq_a}{assign_dq_b}")),
            format!("{masked_assign_dq}{masked_assign_dq}")
        );
        assert_eq!(
            redact_text(&format!("{assign_sq_a}{assign_sq_b}")),
            format!("{masked_assign_sq}{masked_assign_sq}")
        );

        // Embedded in a longer alnum/ident word: same negative as
        // fancy-regex lookbehind / lookahead.
        assert_eq!(redact_text(&format!("xx{akia_a}")), format!("xx{akia_a}"));
        assert_eq!(redact_text(&format!("{akia_a}xx")), format!("{akia_a}xx"));
        assert_eq!(redact_text(&format!("xx{asia_a}")), format!("xx{asia_a}"));
        assert_eq!(redact_text(&format!("{asia_b}xx")), format!("{asia_b}xx"));
        assert_eq!(redact_text(&format!("xx{sk_a}")), format!("xx{sk_a}"));
        assert_eq!(redact_text(&format!("xx{ghp_a}")), format!("xx{ghp_a}"));
        assert_eq!(redact_text(&format!("xx{xoxb_a}")), format!("xx{xoxb_a}"));
        assert_eq!(
            redact_text(&format!("xx{bearer_a}")),
            format!("xx{bearer_a}")
        );
        assert_eq!(
            redact_text(&format!("xx{assign_a}")),
            format!("xx{assign_a}")
        );
    }

    /// Card `20261123-b-7f6462`: `fancy-regex` `replace_all` panicked on
    /// this corpus (prefix / aws / bearer lookbehinds exceeded the
    /// 1_000_000 backtrack cap at ~832 KiB). After the linear rewrite
    /// the same input returns, and neither planted secret leaks.
    #[test]
    fn large_adversarial_input_masks_planted_secrets_without_panicking() {
        let block = concat!(
            "Role:        Builder\n",
            "Account:     grok\n",
            "Repo:        /tmp/repo\n",
            "let foo = bar;\n",
            "fn render() { let x = 1; }\n",
        );
        let mut text = block.repeat(8_000);
        text.push_str("password=planted-secret-value-abcdef\n");
        text.push_str("sk-plantedtoken1234567890\n");
        text.push_str("ordinary_field=keep-me\n");

        let redacted = redact_text(&text);
        assert!(
            !redacted.contains("planted-secret-value-abcdef"),
            "assignment secret leaked"
        );
        assert!(
            !redacted.contains("plantedtoken1234567890"),
            "prefix secret leaked"
        );
        assert!(redacted.contains("ordinary_field=keep-me"));
        assert!(redacted.contains(&format!("password={REDACTED_MARKER}")));
        assert!(redacted.contains(&format!("sk-{REDACTED_MARKER}")));
        assert!(redacted.contains("let foo = bar;"));
        assert_eq!(redact_text(&redacted), redacted);
    }
}
