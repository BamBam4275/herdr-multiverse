//! Conservative redaction for credential-shaped text.
//!
//! Ports `multiverse/redact.py` verbatim at the regex level: the patterns
//! lean on negative lookaround and a conditional backreference that the
//! `regex` crate cannot express, so this module is the one place in
//! `mv-core` that reaches for `fancy-regex` instead — a direct dependency
//! of this crate, not a workspace one, since no other crate needs it.

use std::sync::OnceLock;

use fancy_regex::{Captures, Regex};

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
        Regex::new(
            r"(?<![A-Za-z0-9])(?P<prefix>sk-ant-|sk-|ghp_|gho_|github_pat_|glpat-|xox[bpas]-)(?P<value>[A-Za-z0-9._~+/=-]{4,})",
        )
        .expect("static regex compiles")
    })
}

fn aws_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?<![A-Za-z0-9])(?:AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{14,16})(?![A-Za-z0-9])")
            .expect("static regex compiles")
    })
}

fn bearer_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?<![A-Za-z0-9])Bearer[ \t]+(?P<value>[A-Za-z0-9._~+/=-]{4,})(?![A-Za-z0-9._~+/=-])",
        )
        .expect("static regex compiles")
    })
}

fn credential_stem_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|_)(?:password|passwd|secret|token|api_?key)(?:_|$)")
            .expect("static regex compiles")
    })
}

fn camel_boundary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?<=[a-z0-9])(?=[A-Z])|(?<=[A-Z])(?=[A-Z][a-z])")
            .expect("static regex compiles")
    })
}

fn camel_credential_suffix_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(?:^|_)(?:password|passwd|secret|token|api_?key|secret_key|secret_access_key)$",
        )
        .expect("static regex compiles")
    })
}

/// Mirrors `_ASSIGNMENT_RE`, but with every group numbered rather than named:
/// `fancy-regex` 0.13 (and 0.20 — checked directly, not just changelog) mis-handles
/// a *named* group used as a conditional's test, matching the condition but
/// silently failing to consume the "then" branch; the identical pattern
/// written with numbered groups and a numbered conditional (`(?(1)\1)`)
/// behaves correctly, and `fancy-regex` forbids mixing named and numbered
/// backreferences in one pattern, so every group here stays numbered.
/// Group 1 is `keyquote`, 2 is `key`, 3 is `separator`, 4 is `quote`, 5 is
/// `quoted`, 6 is `bare` — [`replace_assignment`] reads them positionally.
fn assignment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?<![A-Za-z0-9_-])(["'])?([A-Za-z][A-Za-z0-9_-]{0,63})(?(1)\1)(\s*[:=]\s*)(?:(["'])([^"'\r\n]*)\4|([^\s,;]*))"#,
        )
        .expect("static regex compiles")
    })
}

/// Mirrors `_is_credential_key`: a delimited credential stem (`password`,
/// `secret`, `token`, `api_key`/`apikey`, …) or a camelCase key ending in
/// one, as opposed to a key that merely contains those letters
/// (`passwordlessEnabled`, `tokenizerCount`).
fn is_credential_key(key: &str) -> bool {
    let delimited = key.replace('-', "_");
    if credential_stem_re().is_match(&delimited).unwrap_or(false) {
        return true;
    }
    let normalized = camel_boundary_re().replace_all(&delimited, "_");
    camel_credential_suffix_re()
        .is_match(&normalized)
        .unwrap_or(false)
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

    let text = prefix_re().replace_all(&text, |caps: &Captures| {
        format!("{}{REDACTED_MARKER}", &caps["prefix"])
    });

    let text = aws_re().replace_all(&text, |caps: &Captures| {
        let whole = caps.get(0).expect("match always has group 0").as_str();
        format!("{}{REDACTED_MARKER}", &whole[..4])
    });

    let text = bearer_re().replace_all(&text, |_: &Captures| format!("Bearer {REDACTED_MARKER}"));

    let text = assignment_re().replace_all(&text, |caps: &Captures| {
        let whole = caps.get(0).expect("match always has group 0").as_str();
        let key = caps
            .get(2)
            .expect("assignment regex always has group 2 (key)")
            .as_str();
        if !is_credential_key(key) {
            return whole.to_string();
        }
        let keyquote = caps.get(1).map_or("", |m| m.as_str());
        let separator = caps
            .get(3)
            .expect("assignment regex always has group 3 (separator)")
            .as_str();
        let quote = caps.get(4).map_or("", |m| m.as_str());
        format!("{keyquote}{key}{keyquote}{separator}{quote}{REDACTED_MARKER}{quote}")
    });

    text.into_owned()
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
}
