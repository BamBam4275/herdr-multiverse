"""Clock/path normalization shared by the scenario runner.

Both the Python oracle and (eventually) the Rust binary are driven inside a
fresh temporary sandbox on every run, so two kinds of noise never come out
byte-identical across runs even when behavior is identical:

  - Absolute paths: every sandbox lives under a freshly minted `mkdtemp()`
    directory, and on macOS that path also has a `/tmp` vs `/private/tmp`
    alias depending on whether something along the way called `realpath`.
  - Generated values: task ids (`task_id_for`, `dispatch.py`) and backlog
    item ids (`_mint_item_id`, `backlog.py`) are content-hashed or drawn from
    `secrets.token_hex`; routing-log timestamps and `crc32=` fields change on
    every run for the same reason.

`normalize_text` collapses both kinds to stable placeholders so a scenario's
`expect` block can be written once and compared forever after. See
README.md "Normalization rules" for the authoritative list — keep the two in
sync if you add a pattern here.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field

# --- generic, path-independent patterns -----------------------------------

# Dispatch task ids (`dispatch.task_id_for`) and backlog item ids
# (`backlog._mint_item_id`) share the `YYYYMMDD-<slug>-<hex>` shape closely
# enough that one placeholder for both is simpler than telling them apart,
# and no scenario here needs to tell them apart.
_ID_RE = re.compile(r"\b\d{8}-[a-z][a-z0-9]*(?:-[a-z0-9]+)?-[0-9a-f]{6}\b")

# `isoformat(timespec="seconds")` with a `Z` or `+HH:MM` offset form.
_TIMESTAMP_RE = re.compile(
    r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})\b"
)

_CRC32_RE = re.compile(r"crc32=[0-9a-f]{8}\b")

_PID_RE = re.compile(r"\bpid=\d+\b")

# Board header `as of HH:MM` is wall-clock of the render, not ISO, so the
# generic timestamp pattern misses it. `resets in 6d23h` / card clocks
# (`03:16 · 0m`) are elapsed-time displays against that same clock.
_AS_OF_CLOCK_RE = re.compile(r"\bas of \d{1,2}:\d{2}\b")
# Compact board clocks and the spelled-out tasks/workers AGE column.
# Boundaries exclude durations embedded in identifiers, paths or decimals.
_ELAPSED = r"\d+(?: (?:sec|min|hr)|(?:d\d+h|h\d+m)|[smhdwy])"
_ELAPSED_RE = re.compile(r"(?<![\w./+-])" + _ELAPSED + r"(?![\w./+-])")
_RESETS_IN_RE = re.compile(r"\bresets in " + _ELAPSED + r"(?![\w./+-])")
_CARD_CLOCK_RE = re.compile(r"(─+ )?\b\d{1,2}:\d{2} · \d+[mhdwy]\b")

# `multiverse doctor`'s first checklist line names the interpreter actually
# running it (`sys.version_info`), which is a fact about the host, not about
# the CLI under test. Drop successful checks, since the compiled CLI has no
# Python runtime requirement. Leave failed checks visible (with a normalized
# version) so normalization cannot hide a failed prerequisite.
_PYTHON_SUCCESS_LINE_RE = re.compile(
    r"^✓ Python (?:\d+\.\d+|<PYVER>) \(requires 3\.11\+\)\r?\n", re.MULTILINE
)
_PYTHON_VERSION_RE = re.compile(r"\bPython \d+\.\d+(?= \(requires )")

# `multiverse check` shells out to the real `pmset` binary (cli.py
# `idle_sleep_warning`) with no env override to stub it outside pytest
# (tests/conftest.py's `no_real_pmset_calls` fixture only reaches inside the
# Python process). The resulting warning line is a fact about the host
# running the harness, not about the CLI under test, so it is dropped
# wholesale rather than normalized in place.
_PMSET_WARNING_LINE_RE = re.compile(
    r"^warning: this Mac's AC-power idle-sleep.*$", re.MULTILINE
)


@dataclass
class PathMap:
    """Concrete sandbox paths for one scenario run, each tagged with a
    stable placeholder token.

    Longest-path-first matters: a project path nested under `home` (it never
    is, here, but a future scenario might) must not have its parent replaced
    first and leave a dangling placeholder-inside-placeholder.
    """

    entries: list[tuple[str, str]] = field(default_factory=list)

    def add(self, path: str, token: str) -> None:
        resolved = os.path.realpath(path)
        for concrete in {path, resolved}:
            concrete = concrete.rstrip("/")
            if concrete and concrete not in {p for p, _ in self.entries}:
                self.entries.append((concrete, token))

    def ordered(self) -> list[tuple[str, str]]:
        return sorted(self.entries, key=lambda pair: len(pair[0]), reverse=True)


def _card_clock(match: re.Match[str]) -> str:
    replacement = "<TIME> · <ELAPSED>"
    rail = match.group(1)
    if rail:
        return "─" * max(1, len(match.group(0)) - len(replacement) - 1) + " " + replacement
    return replacement


def normalize_elapsed(text: str) -> str:
    """Fold duration tokens only in board/workers/tasks display output.

    Keep fixed-width trailing cell padding, including the border's column,
    across changes in the duration's printed width. Other whitespace stays
    literal. Callers must not apply this to durable files or Herdr scripts.
    """
    def replace(match: re.Match[str]) -> str:
        elapsed, padding = match.groups()
        if padding:
            return "<ELAPSED>" + " " * max(1, len(elapsed) + len(padding) - len("<ELAPSED>"))
        return "<ELAPSED>"

    text = _CARD_CLOCK_RE.sub(_card_clock, text)
    return re.sub("(" + _ELAPSED_RE.pattern + r")( {2,}(?=│))?", replace, text)


def normalize_text(text: str, paths: PathMap) -> str:
    """Fold sandbox paths and generated values to stable placeholders.

    Order matters: paths first (while ids embedded in those paths are still
    literal), then the content-level patterns, then the host-only line drops.

    >>> normalize_text("fleet\\n✓ Python 3.15 (requires 3.11+)\\nconfig", PathMap())
    'fleet\\nconfig'
    >>> normalize_text("fleet\\nconfig", PathMap())
    'fleet\\nconfig'
    >>> normalize_text("✗ Python 3.9 (requires 3.11+)\\n", PathMap())
    '✗ Python <PYVER> (requires 3.11+)\\n'
    >>> normalize_text("✓ Python 3.15 (requires 3.12+)\\n", PathMap())
    '✓ Python <PYVER> (requires 3.12+)\\n'
    """
    for concrete, token in paths.ordered():
        text = text.replace(concrete, token)
    text = _ID_RE.sub("<ID>", text)
    text = _TIMESTAMP_RE.sub("<TIMESTAMP>", text)
    text = _CRC32_RE.sub("crc32=<CRC32>", text)
    text = _PID_RE.sub("pid=<PID>", text)
    text = _AS_OF_CLOCK_RE.sub("as of <HHMM>", text)
    text = _RESETS_IN_RE.sub("resets in <ELAPSED>", text)
    text = _CARD_CLOCK_RE.sub(_card_clock, text)
    text = _PYTHON_SUCCESS_LINE_RE.sub("", text)
    text = _PYTHON_VERSION_RE.sub("Python <PYVER>", text)
    text = _PMSET_WARNING_LINE_RE.sub("", text)
    # The pmset line's removal can leave a run of blank lines where it sat
    # between two others; collapse but do not otherwise touch whitespace.
    text = re.sub(r"\n{3,}", "\n\n", text)
    return text


def normalize_path_value(path: str, paths: PathMap) -> str:
    """Normalize a single path string (e.g. a relative-to-root file path
    used as a dict key) the same way `normalize_text` would."""
    return normalize_text(path, paths)
