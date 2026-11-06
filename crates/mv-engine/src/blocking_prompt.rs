//! Port of `limits.classify_prompt` (frozen Python reference,
//! `multiverse/limits.py`) — detecting a pane parked on a vendor dialog
//! that is not a plain exhaustion banner, so a caller that only checks for
//! "blocked, not working" has something to check for that a plain gauge
//! reading cannot carry.
//!
//! Covers every captured shape Python's own `_PROMPTS`/`_detect_codex_prompt`
//! do: codex's model-switch offer, the codex/grok billing dialogs (read a
//! second way off [`crate::limits::classify`]'s own `Verdict`, same as
//! Python's `_blocking_prompt_from_interactive_verdict`), grok's and codex's
//! sandbox/permission-to-act dialogs (`tests/fixtures/sandbox-dialog/`), and
//! codex's own CLI update notice (`tests/fixtures/stall-cause/`). The last
//! two were ported later than the first three (task `20261103-fixer-08598b`,
//! cards `20261102-b-bdf745`/`20261102-b-9ef1da`): this checkout had no
//! `sandbox-dialog/`/`stall-cause/` fixtures until that task carried
//! sanitised captures over from the frozen Python reference — see those
//! directories' own `README.md` for why the committed text differs from the
//! Python reference's byte-for-byte (real project names and machine paths
//! removed; the regex-anchored shape — option markers, field labels, default
//! marker position — is preserved exactly).
//!
//! `detect_codex_billing_prompt`/`detect_grok_billing_prompt` still
//! inline the one slice of `_classify_codex`/`_classify_grok` that
//! `_blocking_prompt_from_interactive_verdict` needs (the exhaustion anchor
//! plus the attached-dialog check) rather than calling
//! [`crate::limits::classify`] directly — `classify`'s own `Verdict` carries
//! more than this module needs (cooldown math, the non-interactive 429
//! branch), and the two modules share the same anchors
//! (`codex_exhausted_re`, `codex_dialog_re`, `grok_exhausted_re`,
//! `grok_dialog_re`, `live_banner_region` — all `pub(crate)` so
//! `crate::limits` reuses the identical patterns rather than a second copy).

use std::sync::OnceLock;

use regex::Regex;

/// Who a [`BlockingPrompt`] is actually waiting on. Recognition only, never
/// a grant of authority to answer on their behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAudience {
    /// A billing dialog (codex out of credits, grok at its weekly limit).
    Operator,
    /// A vendor notice with a default action but no permission framing at
    /// all (codex's model-switch offer).
    DoNotAnswer,
    /// A sandbox/permission-to-act dialog. No detector in this port
    /// produces this variant yet — see the module doc comment — but it is
    /// kept so a future detector does not need a new enum landed alongside
    /// it.
    ProjectOwner,
}

impl PromptAudience {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operator => "operator",
            Self::DoNotAnswer => "do_not_answer",
            Self::ProjectOwner => "project_owner",
        }
    }
}

/// An interactive prompt that blocks the pane but is not itself an
/// exhaustion condition to trip a breaker over. `quote` is the dialog's own
/// words, verbatim and sensibly truncated — see `quote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingPrompt {
    pub vendor: String,
    pub rule: String,
    pub audience: PromptAudience,
    pub quote: String,
}

/// Python's `_QUOTE_MAX_LEN`.
const QUOTE_MAX_LEN: usize = 240;

/// Python's `_quote`: a verbatim, single-line, sensibly-truncated slice of
/// `text` starting at byte offset `start` (always a regex match start, so
/// always a char boundary).
fn quote(text: &str, start: usize) -> String {
    let collapsed = text[start..]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.chars().count() <= QUOTE_MAX_LEN {
        return collapsed;
    }
    let mut truncated: String = collapsed.chars().take(QUOTE_MAX_LEN - 1).collect();
    while truncated.ends_with(char::is_whitespace) {
        truncated.pop();
    }
    truncated.push('…');
    truncated
}

/// Python's `_LIVE_BANNER_TAIL_LINES`.
const LIVE_BANNER_TAIL_LINES: usize = 5;

/// Python's `_live_banner_region`: the trailing slice of `text` a live
/// vendor dialog would actually be sitting in, with Herdr's own
/// box-border-only lines and pane-footer row removed first so they cannot
/// crowd a real banner out of the window.
pub(crate) fn live_banner_region(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && trimmed != "┃" && !trimmed.starts_with("Tab:next answer")
        })
        .collect();
    let start = lines.len().saturating_sub(LIVE_BANNER_TAIL_LINES);
    lines[start..].join("\n")
}

pub(crate) fn codex_exhausted_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"Usage limit reached[ \t]*\r?\n[ \t]*",
            r"Request a limit increase from your owner to continue using codex\.",
        ))
        .expect("static regex compiles")
    })
}

/// Python's `_CODEX_DIALOG_RE`: the numbered options codex renders for its
/// own exhaustion banner — absence of this is what lets a future,
/// dialog-free codex banner close its own pane instead of parking it.
pub(crate) fn codex_dialog_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"Request increase\?[\s\S]*?1\.\s*Yes[\s\S]*?2\.\s*No")
            .expect("static regex compiles")
    })
}

/// Python's `_GROK_EXHAUSTED_RE`: anchored on the past-tense sentence, not
/// the "Weekly limit" phrase alone — that phrase also appears in grok's
/// healthy, still-serving quota gauge.
pub(crate) fn grok_exhausted_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"You hit your weekly limit\.").expect("static regex compiles"))
}

/// Python's `_GROK_DIALOG_RE`: never auto-answer this — two of the three
/// options spend real money.
pub(crate) fn grok_dialog_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"1\s+Upgrade tier[\s\S]*?2\s+Buy more credits").expect("static regex compiles")
    })
}

/// Python's `_CODEX_MODEL_SWITCH_RE`: anchored on the full captured
/// sequence (offer sentence, all three numbered options in order,
/// including the "(never show again)" suffix on option 3), not on
/// "Approaching rate limits"/"Switch to ... model" alone — either phrase in
/// isolation is ordinary enough to misfire on genuine exhaustion text or
/// ordinary narration about switching models.
fn codex_model_switch_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"Approaching rate limits[\s\S]*?",
            r"Switch to [^\n]+ for lower credit usage\?[\s\S]*?",
            r"1\.\s*Switch to[\s\S]*?",
            r"2\.\s*Keep current model[\s\S]*?",
            r"3\.\s*Keep current model \(never show again\)",
        ))
        .expect("static regex compiles")
    })
}

/// codex's model-switch offer, or `None`. `PromptAudience::DoNotAnswer`:
/// there is no correct answer for an orchestrator to give; the seat stays
/// on the model it started with.
fn detect_codex_model_switch(text: &str) -> Option<BlockingPrompt> {
    let found = codex_model_switch_re().find(text)?;
    Some(BlockingPrompt {
        vendor: "codex".to_owned(),
        rule: "codex_model_switch_offer".to_owned(),
        audience: PromptAudience::DoNotAnswer,
        quote: quote(text, found.start()),
    })
}

/// codex's exhaustion banner, read a second way as a
/// [`BlockingPrompt`] only when its own attached dialog is also present —
/// Python's `_blocking_prompt_from_interactive_verdict`, inlined for codex
/// (see the module doc comment for why no full `Verdict` is built here).
/// Both checks search only [`live_banner_region`], matching
/// `_classify_codex`; the quote itself starts from the exhaustion anchor's
/// position in the *full* text, matching `_blocking_prompt_from_interactive_verdict`'s
/// own anchor re-search.
fn detect_codex_billing_prompt(text: &str) -> Option<BlockingPrompt> {
    let region = live_banner_region(text);
    codex_exhausted_re().find(&region)?;
    codex_dialog_re().find(&region)?;
    let anchor = codex_exhausted_re().find(text)?;
    Some(BlockingPrompt {
        vendor: "codex".to_owned(),
        rule: "codex_usage_limit_reached".to_owned(),
        audience: PromptAudience::Operator,
        quote: quote(text, anchor.start()),
    })
}

/// grok's weekly-limit banner, read the same second way — see
/// [`detect_codex_billing_prompt`].
fn detect_grok_billing_prompt(text: &str) -> Option<BlockingPrompt> {
    let region = live_banner_region(text);
    grok_exhausted_re().find(&region)?;
    grok_dialog_re().find(&region)?;
    let anchor = grok_exhausted_re().find(text)?;
    Some(BlockingPrompt {
        vendor: "grok".to_owned(),
        rule: "grok_weekly_limit_reached".to_owned(),
        audience: PromptAudience::Operator,
        quote: quote(text, anchor.start()),
    })
}

/// Python's `_GROK_SANDBOX_RE`: the offer sentence's option list, anchored on
/// the exact captured shape — all four numbered options in order, including
/// the "(always-approve mode)" phrase unique to option 1 being the
/// *persistent* one on this vendor (the opposite of codex's own default
/// below). The named group starts the quote at the option list itself,
/// skipping the tool call it is attached to (which can be arbitrarily long),
/// so the default marker is never the part [`quote`]'s truncation drops.
fn grok_sandbox_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?P<options>1\s*\(●\)\s*Yes, and don't ask again for anything ",
            r"\(always-approve mode\)[\s\S]*?",
            r"2\s*\(○\)\s*Yes, proceed[\s\S]*?",
            r"3\s*\(○\)\s*No, reject)",
            r"[\s\S]*?Ctrl\+o:always-approve",
        ))
        .expect("static regex compiles")
    })
}

/// grok's tool-approval sandbox dialog, or `None`.
/// `PromptAudience::ProjectOwner` — whoever owns the project can decide; the
/// quote starts at the option list so truncation keeps the default marker.
fn detect_grok_sandbox_permission(text: &str) -> Option<BlockingPrompt> {
    let options = grok_sandbox_re().captures(text)?.name("options")?;
    Some(BlockingPrompt {
        vendor: "grok".to_owned(),
        rule: "grok_sandbox_permission_request".to_owned(),
        audience: PromptAudience::ProjectOwner,
        quote: quote(text, options.start()),
    })
}

/// Python's `_CODEX_SANDBOX_RE`: the "run the following command?" framing
/// with its `Environment:`/`Reason:` fields, then the three numbered options
/// in order — the *run* shape only; the README's own file-scoped *edit*
/// variant has neither field and must not match this pattern. The named
/// group starts the quote at the option list, whose default (option 1,
/// single-shot proceed) is the opposite of grok's own default above.
fn codex_sandbox_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"Would you like to run the following command\?[\s\S]*?",
            r"Environment:[\s\S]*?Reason:[\s\S]*?",
            r"(?P<options>›\s*1\.\s*Yes, proceed[\s\S]*?",
            r"2\.\s*Yes, and don't ask again for commands that start with[\s\S]*?",
            r"3\.\s*No, and tell Codex what to do differently)",
            r"[\s\S]*?Press enter to confirm or esc to cancel",
        ))
        .expect("static regex compiles")
    })
}

/// codex's run-command sandbox dialog, or `None`. The *run* shape only —
/// the file-scoped *edit* variant is a different capture and must not
/// match. Same `ProjectOwner` audience as grok's.
fn detect_codex_sandbox_permission(text: &str) -> Option<BlockingPrompt> {
    let options = codex_sandbox_re().captures(text)?.name("options")?;
    Some(BlockingPrompt {
        vendor: "codex".to_owned(),
        rule: "codex_sandbox_permission_request".to_owned(),
        audience: PromptAudience::ProjectOwner,
        quote: quote(text, options.start()),
    })
}

/// Python's `_CODEX_UPDATE_PROMPT_RE`: the "Update available!" banner, then
/// all three numbered options in order, with the ">" marker pinned on
/// option 1's own text ("Update now (runs `brew upgrade") so a future
/// capture that moves the marker fails here rather than silently
/// misreporting the default. "Press enter to continue" — not "to confirm"
/// — is the closing anchor that makes this read as a notice rather than a
/// request in the first place.
fn codex_update_prompt_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"Update available![\s\S]*?",
            r"(?P<options>>\s*1\.\s*Update now \(runs `brew upgrade[\s\S]*?",
            r"2\.\s*Skip[\s\S]*?",
            r"3\.\s*Skip until next version)",
            r"[\s\S]*?Press enter to continue",
        ))
        .expect("static regex compiles")
    })
}

/// codex's CLI update notice, or `None`. `PromptAudience::DoNotAnswer`: the
/// default runs `brew upgrade` against the human's machine, which an
/// orchestrator has no standing to trigger.
fn detect_codex_update_prompt(text: &str) -> Option<BlockingPrompt> {
    let options = codex_update_prompt_re().captures(text)?.name("options")?;
    Some(BlockingPrompt {
        vendor: "codex".to_owned(),
        rule: "codex_cli_update_prompt".to_owned(),
        audience: PromptAudience::DoNotAnswer,
        quote: quote(text, options.start()),
    })
}

/// Python's `_detect_codex_prompt`: codex has three captured dialog shapes,
/// each from its own fixture and its own regex, tried in order, first match
/// wins. They never overlap: the model-switch offer, the sandbox
/// run-command prompt, and the CLI update notice share no anchor text with
/// one another.
fn detect_codex_prompt(text: &str) -> Option<BlockingPrompt> {
    for detector in [
        detect_codex_model_switch,
        detect_codex_sandbox_permission,
        detect_codex_update_prompt,
    ] {
        if let Some(prompt) = detector(text) {
            return Some(prompt);
        }
    }
    None
}

/// Detect a blocking interactive prompt, or return `None`. Python's
/// `classify_prompt` — see the module doc comment for the shapes covered.
///
/// Same fail-open contract as Python: empty/whitespace-only text, an
/// unrecognised `kind`, and text with no known prompt shape all return
/// `None`.
pub fn classify_prompt(text: &str, kind: &str) -> Option<BlockingPrompt> {
    if text.trim().is_empty() {
        return None;
    }
    let prompt = match kind {
        "codex" => detect_codex_prompt(text),
        "grok" => detect_grok_sandbox_permission(text),
        _ => None,
    };
    if prompt.is_some() {
        return prompt;
    }
    match kind {
        "codex" => detect_codex_billing_prompt(text),
        "grok" => detect_grok_billing_prompt(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEX_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-out-of-credits.txt");
    const CODEX_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-rolling-429.txt");
    const CODEX_MODEL_SWITCH: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt");
    const CLAUDE_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/claude-rolling-429.txt");
    const GROK_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-reached.txt");
    const GROK_GAUGE: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt");
    const GROK_SANDBOX: &str =
        include_str!("../../../tests/fixtures/sandbox-dialog/grok-reviewer-tool-approval.txt");
    const CODEX_SANDBOX: &str =
        include_str!("../../../tests/fixtures/sandbox-dialog/codex-reviewer-loopback-approval.txt");
    const CODEX_UPDATE_PROMPT: &str =
        include_str!("../../../tests/fixtures/stall-cause/codex-cli-update-prompt.txt");

    #[test]
    fn empty_or_whitespace_text_never_classifies() {
        assert_eq!(classify_prompt("", "codex"), None);
        assert_eq!(classify_prompt("   \n  ", "grok"), None);
    }

    #[test]
    fn unrecognised_kind_never_classifies() {
        assert_eq!(classify_prompt(CODEX_EXHAUSTED, "unknown-vendor"), None);
        assert_eq!(classify_prompt(CODEX_MODEL_SWITCH, "claude"), None);
    }

    #[test]
    fn codex_model_switch_offer_is_a_do_not_answer_prompt() {
        let prompt = classify_prompt(CODEX_MODEL_SWITCH, "codex").unwrap();
        assert_eq!(prompt.vendor, "codex");
        assert_eq!(prompt.rule, "codex_model_switch_offer");
        assert_eq!(prompt.audience, PromptAudience::DoNotAnswer);
        assert!(prompt.quote.starts_with("Approaching rate limits"));
    }

    #[test]
    fn codex_out_of_credits_dialog_is_an_operator_prompt() {
        let prompt = classify_prompt(CODEX_EXHAUSTED, "codex").unwrap();
        assert_eq!(prompt.vendor, "codex");
        assert_eq!(prompt.rule, "codex_usage_limit_reached");
        assert_eq!(prompt.audience, PromptAudience::Operator);
        assert!(prompt.quote.starts_with("Usage limit reached"));
    }

    #[test]
    fn grok_weekly_limit_dialog_is_an_operator_prompt() {
        let prompt = classify_prompt(GROK_EXHAUSTED, "grok").unwrap();
        assert_eq!(prompt.vendor, "grok");
        assert_eq!(prompt.rule, "grok_weekly_limit_reached");
        assert_eq!(prompt.audience, PromptAudience::Operator);
        assert!(prompt.quote.starts_with("You hit your weekly limit."));
    }

    #[test]
    fn a_plain_429_with_no_attached_dialog_is_not_a_blocking_prompt() {
        // The 429 banner states no cooldown and carries no dialog at all —
        // `classify`'s own non-interactive branch, which `classify_prompt`
        // never surfaces as a `BlockingPrompt`.
        assert_eq!(classify_prompt(CODEX_ROLLING, "codex"), None);
    }

    #[test]
    fn claude_is_not_a_recognised_kind() {
        // No `claude` entry in `_PROMPTS`/`_blocking_prompt_from_interactive_verdict`
        // — unrecognised, not merely unmatched.
        assert_eq!(classify_prompt(CLAUDE_ROLLING, "claude"), None);
    }

    #[test]
    fn grok_healthy_gauge_never_classifies_despite_sharing_weekly_limit_wording() {
        assert_eq!(classify_prompt(GROK_GAUGE, "grok"), None);
    }

    #[test]
    fn quote_collapses_whitespace_and_truncates_at_the_same_length_as_python() {
        let long = format!("Usage limit reached\n{}", "x".repeat(400));
        assert_eq!(quote(&long, 0).chars().count(), QUOTE_MAX_LEN);
        assert!(quote(&long, 0).ends_with('…'));
        assert_eq!(quote("a   b\nc", 0), "a b c");
    }

    // --- Cards 20261102-b-bdf745 / 20261102-b-9ef1da: sandbox + update -----

    #[test]
    fn grok_sandbox_fixture_is_recognised_as_its_own_kind() {
        let prompt = classify_prompt(GROK_SANDBOX, "grok").unwrap();
        assert_eq!(prompt.vendor, "grok");
        assert_eq!(prompt.rule, "grok_sandbox_permission_request");
        assert_eq!(prompt.audience, PromptAudience::ProjectOwner);
        // The default marker is part of the fact (constraint 4): grok's
        // marked option is the *persistent*, blanket one, not the
        // single-shot proceed.
        assert!(prompt.quote.contains("always-approve mode"));
    }

    #[test]
    fn codex_sandbox_fixture_is_recognised_as_its_own_kind() {
        let prompt = classify_prompt(CODEX_SANDBOX, "codex").unwrap();
        assert_eq!(prompt.vendor, "codex");
        assert_eq!(prompt.rule, "codex_sandbox_permission_request");
        assert_eq!(prompt.audience, PromptAudience::ProjectOwner);
        // codex's marked option is the single-shot proceed — the inverse of
        // grok's own default above.
        assert!(prompt.quote.contains("1. Yes, proceed"));
        assert!(!prompt.quote.contains("always-approve"));
    }

    #[test]
    fn the_two_sandbox_fixtures_are_not_read_as_the_same_kind() {
        let grok_prompt = classify_prompt(GROK_SANDBOX, "grok").unwrap();
        let codex_prompt = classify_prompt(CODEX_SANDBOX, "codex").unwrap();
        assert_ne!(grok_prompt.rule, codex_prompt.rule);
        assert_ne!(grok_prompt.quote, codex_prompt.quote);
    }

    #[test]
    fn the_update_prompt_is_recognised_as_its_own_kind() {
        let prompt = classify_prompt(CODEX_UPDATE_PROMPT, "codex").unwrap();
        assert_eq!(prompt.vendor, "codex");
        assert_eq!(prompt.rule, "codex_cli_update_prompt");
        // Not a permission-to-act dialog: never the sandbox dialogs'
        // audience, and never answered on anyone's behalf — its default
        // mutates the human's machine, outside any project.
        assert_eq!(prompt.audience, PromptAudience::DoNotAnswer);
        assert!(prompt.quote.contains("brew upgrade"));
    }

    #[test]
    fn the_update_prompt_default_runs_a_package_upgrade() {
        // Pinned because it is the reason this fixture is kept at all: if a
        // future capture normalises the marker onto a different option,
        // that is a real change in what a bare Enter does and must fail
        // here.
        let prompt = classify_prompt(CODEX_UPDATE_PROMPT, "codex").unwrap();
        assert!(prompt.quote.starts_with('>'));
        assert!(prompt.quote.contains("Update now"));
        assert!(prompt.quote.contains("brew upgrade"));
        assert!(prompt.quote.contains("Press enter to continue"));
    }

    #[test]
    fn the_model_switch_fixture_still_recognises_as_its_original_kind() {
        // Guard against the widened `detect_codex_prompt` dispatch
        // swallowing or reordering the pre-existing model-switch detection.
        let prompt = classify_prompt(CODEX_MODEL_SWITCH, "codex").unwrap();
        assert_eq!(prompt.rule, "codex_model_switch_offer");
        assert_eq!(prompt.audience, PromptAudience::DoNotAnswer);
    }

    #[test]
    fn none_of_the_three_new_kinds_are_recognised_for_the_wrong_vendor() {
        for vendor in ["grok", "claude", "chatgpt", "brand-new-vendor"] {
            if vendor != "codex" {
                assert_eq!(classify_prompt(CODEX_SANDBOX, vendor), None);
                assert_eq!(classify_prompt(CODEX_UPDATE_PROMPT, vendor), None);
            }
            if vendor != "grok" {
                assert_eq!(classify_prompt(GROK_SANDBOX, vendor), None);
            }
        }
    }

    #[test]
    fn an_invented_near_miss_sandbox_dialog_is_not_recognised_at_all() {
        // The described (but never captured) file-scoped *edit* variant of
        // codex's sandbox dialog: no `Reason:`/`$ command` line, a
        // `Destination:` path instead, different option 2 wording. Built to
        // resemble `codex_sandbox_permission_request` closely enough that a
        // loose pattern would fire on it; `codex_sandbox_re` must not,
        // because no fixture in this repo has ever captured this shape.
        let near_miss = "Would you like to make this change?\n\n\
            Destination: /home/agent/project/main.go\n\n\
            › 1. Yes, proceed (y)\n  \
            2. Yes, and don't ask again for these files (f)\n  \
            3. No, and tell Codex what to do differently (esc)\n\n\
            Press enter to confirm or esc to cancel\n";
        for vendor in ["codex", "grok", "claude", "chatgpt"] {
            assert_eq!(classify_prompt(near_miss, vendor), None);
        }
    }

    #[test]
    fn classify_prompt_is_a_pure_function_for_all_three_new_kinds() {
        assert_eq!(
            classify_prompt(GROK_SANDBOX, "grok"),
            classify_prompt(GROK_SANDBOX, "grok")
        );
        assert_eq!(
            classify_prompt(CODEX_SANDBOX, "codex"),
            classify_prompt(CODEX_SANDBOX, "codex")
        );
        assert_eq!(
            classify_prompt(CODEX_UPDATE_PROMPT, "codex"),
            classify_prompt(CODEX_UPDATE_PROMPT, "codex")
        );
    }
}
