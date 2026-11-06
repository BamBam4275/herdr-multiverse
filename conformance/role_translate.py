"""Old->new role-name translation applied only when replaying against Rust.

Conformance scenarios were recorded from the frozen Python oracle, before
the role rename (`orchestrator->nexus`, `explorer->scout`, `fixer->builder`,
`librarian->archivist`, `reviewer->critic`, `oracle->seer`,
`designer->artisan`, `observer->watcher`; `council` is unchanged). This
lane does not re-record or relax those fixtures (see README.md "Status").
Instead, the input surfaces Rust's config/CLI write paths accept only the
current names on — `[roles.<name>]` config tables (and chain entries under
a `*.chains` table), and every CLI `--role <name>` flag, on any subcommand
(`dispatch`, `backlog add`, `backlog ask`, and any future one) — are
rewritten old->new here, in memory, right before a scenario replays against
Rust. Python replay never calls this: the oracle only understands the old
names.

`backlog add`/`ask --role` is itself free text in the frozen Python oracle
(validated only against `activity._IDENTIFIER_RE`, never a closed RoleId
enum), so an old name was never rejected *there*. Rust's write surfaces,
by contrast, use new role names only everywhere, backlog's `--role`
included (history readers are the one place old names keep working,
untouched by this module — see README.md). Translating `--role` here
regardless of which subcommand it is attached to keeps every recorded
scenario's old-name input valid on both sides without relaxing Rust's own
new-names-only rule on writes.

See README.md's "Role rename: what gets translated at replay time" section
for the full rule, including what this module deliberately does NOT touch
(on-disk history fixtures and recorded `expect` text).
"""

from __future__ import annotations

import re

# Intentional release renames; share this table across input and output translation.
ROLE_RENAME: dict[str, str] = {
    "orchestrator": "nexus",
    "explorer": "scout",
    "fixer": "builder",
    "librarian": "archivist",
    "reviewer": "critic",
    "oracle": "seer",
    "designer": "artisan",
    "observer": "watcher",
    # "council" is omitted: the rename left it unchanged.
}

_ROLE_TABLE_HEADER_RE = re.compile(r"^\[roles\.([A-Za-z0-9_-]+)((?:\.[A-Za-z0-9_-]+)*)\]\s*$")
_CHAINS_HEADER_RE = re.compile(r"^\[[A-Za-z0-9_.-]+\.chains\]\s*$")
_ANY_TABLE_HEADER_RE = re.compile(r"^\[.*\]\s*$")
_CHAIN_KEY_RE = re.compile(r"^([A-Za-z0-9_-]+)(\s*=.*)$")


def translate_config_toml(text: str) -> str:
    """Rewrite retired `[roles.<old>]` / `[roles.<old>.sub]` table headers
    and `<old> = [...]` chain entries (inside a `*.chains` table only) to
    their current names. Line-oriented and deliberately narrow: an account
    name, pool id, or any other free-text value that merely contains a
    role-shaped word (`[accounts.explorer-seat]`, `pool = "codex:b"`) is
    never touched, because only these two syntactic positions are a closed
    role-name vocabulary.
    """
    out = []
    in_chains_table = False
    for line in text.split("\n"):
        header = _ROLE_TABLE_HEADER_RE.match(line)
        if header:
            role, rest = header.group(1), header.group(2)
            out.append(f"[roles.{ROLE_RENAME.get(role, role)}{rest}]")
            in_chains_table = False
            continue
        if _ANY_TABLE_HEADER_RE.match(line):
            in_chains_table = bool(_CHAINS_HEADER_RE.match(line))
            out.append(line)
            continue
        if in_chains_table:
            key_match = _CHAIN_KEY_RE.match(line)
            if key_match and key_match.group(1) in ROLE_RENAME:
                out.append(f"{ROLE_RENAME[key_match.group(1)]}{key_match.group(2)}")
                continue
        out.append(line)
    return "\n".join(out)


def translate_role_argv(argv: list[str]) -> list[str]:
    """Rewrite any `--role <old>` value in a step's argv to the current
    name. Not scoped to a single subcommand: every CLI surface that takes
    `--role` (`dispatch`, `backlog add`, `backlog ask`, and any future
    one) is a write surface that accepts only current role names, so an
    old name recorded against any of them needs the same old->new rewrite
    before it reaches Rust.
    """
    if not argv:
        return argv
    out = list(argv)
    for i, token in enumerate(out):
        if token == "--role" and i + 1 < len(out):
            out[i + 1] = ROLE_RENAME.get(out[i + 1], out[i + 1])
    return out


def _title(word: str) -> str:
    return word[:1].upper() + word[1:]


# The role's current, title-cased display name — `commands::dispatch`'s own
# `Role::display_name()`, in its retired->current direction.
_DISPLAY_RENAME: dict[str, str] = {_title(old): _title(new) for old, new in ROLE_RENAME.items()}

_ALT_OLD_LOWER = "|".join(re.escape(k) for k in ROLE_RENAME)
_ALT_OLD_TITLE = "|".join(re.escape(k) for k in _DISPLAY_RENAME)

# Task `20261103-fixer-fa9c70`: `dispatch.rs`'s `--dry-run` card and a real
# dispatch now render the role's *current* name on every surface (no more
# byte-for-byte parity with Python's retired-name output) — these four
# patterns are the exact, fixed positions that rendering comes from
# (`commands::dispatch`'s `render_card`, `RealDispatchStore::record_dispatch`'s
# "WOULD LOG" preview, and `render_chain_exhausted`). Matching only these
# literal anchors — not a blanket word-boundary rewrite of every scenario's
# `expect` text — is deliberate: a `history-*` scenario's `expect` text can
# contain this same old word (`Role: Designer`, `fixer`, `oracle` as a
# `tasks` column) on purpose, because a history reader renders an old
# on-disk record verbatim and must keep doing so forever. A blanket rewrite
# would silently break those.
_ROLE_DISPLAY_LINE_RE = re.compile(r"(?<=role        )(" + _ALT_OLD_TITLE + r")\b")
_ROLE_FIELD_RE = re.compile(r"(?<=\trole=)(" + _ALT_OLD_LOWER + r")(?=\t)")
_CHAIN_FOR_RE = re.compile(r"(?<=every account in the )(" + _ALT_OLD_TITLE + r")(?= chain for )")
_FALLBACK_POLICY_RE = re.compile(
    r"(?<=\n)(" + _ALT_OLD_TITLE + r")(?='s fallback policy is degrade_once)"
)

# review-policy common-noun sentences: Python still prints retired
# lowercase "reviewer"/"orchestrator" here; Rust now prints the current
# config_key. Matching only these literal phrases keeps a history-*
# scenario's `Role: Designer` / `fixer` column untouched (see
# `translate_expect_text` below). Hyphens count as identifier characters
# so `reviewer-seat` is not rewritten.
_REVIEW_POLICY_NOUN_RES: list[tuple[re.Pattern[str], str]] = [
    (re.compile(r"(?<![A-Za-z0-9_-])reviewer seats:"), "critic seats:"),
    (re.compile(r"(?<![A-Za-z0-9_-])a reviewer chain(?![A-Za-z0-9_-])"), "a critic chain"),
    (
        re.compile(r"(?<![A-Za-z0-9_-])a reviewer still running(?![A-Za-z0-9_-])"),
        "a critic still running",
    ),
    (re.compile(r"(?<![A-Za-z0-9_-])reviewer dispatch budget:"), "critic dispatch budget:"),
    (
        re.compile(r"(?<![A-Za-z0-9_-])the orchestrator instead(?![A-Za-z0-9_-])"),
        "the nexus instead",
    ),
    (
        re.compile(r"(?<![A-Za-z0-9_-])the orchestrator already ran(?![A-Za-z0-9_-])"),
        "the nexus already ran",
    ),
]


def _preserve_case(old: str, new: str) -> str:
    if old.isupper():
        return new.upper()
    if old.islower():
        return new.lower()
    if old.istitle():
        return new[:1].upper() + new[1:].lower()
    return new[:1].upper() + new[1:].lower()


# Card `20261103-b-71fb3f`: these three sentences are verbatim boilerplate
# that every generated brief/result template carries, byte-for-byte
# identical in both `crates/mv-engine/src/brief.rs:297-299,511,516,561` and
# the frozen oracle's `multiverse/brief.py:467,520,525,576`. Each uses
# "orchestrator" as the plain English noun for whoever reads the result,
# not the Orchestrator role name, so both implementations already agree
# and there is nothing to translate. Matching only these three literal,
# multi-word anchors — not a context-free "is this a static noun"
# heuristic — keeps the blanket rewrite below intact everywhere else,
# the same way `translate_expect_text`'s anchors stay narrow.
#
# Card `20261103-fixer-2faef1` finding 5: the first anchor used to be the
# bare suffix `'s\.` with no anchor on what comes *before* "orchestrator",
# so it suppressed the rewrite for *any* sentence ending "orchestrator's." —
# including a genuine role-name mention — not only this template's own
# "never the orchestrator's." A fixed-width lookbehind pins it to that
# exact lead-in; `test_dynamic_orchestrator_role_word_near_static_noun_text`
# already covers the one real case this template co-occurs with ("escalate
# to the orchestrator; never the orchestrator's."), and still passes.
_STATIC_ORCHESTRATOR_NOUN_ANCHORS = (
    # Python lookbehind must be fixed-width, so this is a literal single
    # space, not `\s+` — matching the one space the source template's own
    # line-continuation escape leaves between "the" and "orchestrator's."
    r"(?<=never the )orchestrator's\.",
    # Findings section — both source templates hard-wrap this sentence
    # with a literal embedded newline between "a" and "path", so the
    # anchor must match whitespace there, not a literal space.
    r"orchestrator\s+can\s+read\s+a\s+file\s+it\s+has\s+a\s+path\s+to",
    r"orchestrator will otherwise assume coverage",  # What I did not cover section
)
_STATIC_ORCHESTRATOR_NOUN_LOOKAHEAD = "(?:" + "|".join(_STATIC_ORCHESTRATOR_NOUN_ANCHORS) + ")"

# Live Python prose (diff_fuzz stdout/stderr): rewrite every old role word,
# case-insensitively, preserving the source's own case. Identifier-shaped
# tokens (`explorer-seat`) are left alone because a hyphen is part of the
# token, not a word boundary. Not used on recorded `expect` text — a
# history reader must keep showing the on-disk name.
_PROSE_ROLE_RE = re.compile(
    r"(?<![A-Za-z0-9_-])(?!" + _STATIC_ORCHESTRATOR_NOUN_LOOKAHEAD + r")("
    + _ALT_OLD_LOWER + r")(?![A-Za-z0-9_-])",
    re.IGNORECASE,
)


# Dash-padded card labels have a fixed rendered width. Both rails share
# the role's length change; the optional project short after them stays put.
_BOX_LABEL_RE = re.compile(r"(└)(─+)( [^─│┌┐└┘\r\n]+? )(─+)")


def translate_prose_roles(text: str) -> str:
    """Rewrite old role words in live Python prose old->new, preserving case.

    Used by `diff_fuzz.py` on the oracle's stdout/stderr so a pure rename
    (`Reviewer`/`reviewer`/`orchestrator`/...) is not reported as a
    divergence against Rust's current names. Hyphenated account/pool ids
    that merely start with a role word are not rewritten. Box-card footer
    rails are rebalanced so translating a label keeps its rendered width.
    """

    def repl(match: re.Match[str]) -> str:
        old = match.group(1)
        new = ROLE_RENAME[old.lower()]
        return _preserve_case(old, new)

    def box_label(match: re.Match[str]) -> str:
        corner, lead, label, trail = match.groups()
        translated = _PROSE_ROLE_RE.sub(repl, label)
        if translated == label:
            return match.group(0)
        rails = len(lead) + len(trail) + len(label) - len(translated)
        if rails < 2:
            # There is no padding to recover on an overflowing label.
            return match.group(0)
        return corner + "─" * (rails // 2) + translated + "─" * (rails - rails // 2)

    text = _BOX_LABEL_RE.sub(box_label, text)
    return _PROSE_ROLE_RE.sub(repl, text)


def translate_expect_text(text: str) -> str:
    """Rewrite a scenario's recorded `expect` text old->new at the handful
    of fixed positions `dispatch.rs`'s own card/refusal renderer produces a
    role word from — see the module-level comment above for why this is
    narrow rather than a blanket rewrite, and `conformance/README.md`'s
    "Role rename" section for the fuller rule. Used only when replaying
    against Rust (`run_scenario.py`, `impl == "rust"`); Python replay never
    calls this, since the frozen oracle only ever produced the old word.
    """
    text = _ROLE_DISPLAY_LINE_RE.sub(lambda m: _DISPLAY_RENAME[m.group(1)], text)
    text = _ROLE_FIELD_RE.sub(lambda m: ROLE_RENAME[m.group(1)], text)
    text = _CHAIN_FOR_RE.sub(lambda m: _DISPLAY_RENAME[m.group(1)], text)
    text = _FALLBACK_POLICY_RE.sub(lambda m: _DISPLAY_RENAME[m.group(1)], text)
    for pattern, replacement in _REVIEW_POLICY_NOUN_RES:
        text = pattern.sub(replacement, text)
    return text
