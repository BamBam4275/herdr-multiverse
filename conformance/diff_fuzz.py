#!/usr/bin/env python3
"""Seeded differential fuzz harness: Python oracle vs Rust CLI.

Generates seeded sequences of backlog/account/halt/notify/activity writes and
Herdr-touching dispatch, sweep, stop, start, task reclamation, worktree prune,
and council commands. Each implementation runs in a fresh sandbox against the
same stateful `crates/mv-herdr` fake binary; no real Herdr or vendor CLI runs.
The sequence seed also initializes identical workspace/pane/agent state and
recorded/running/stranded task histories. Fake state persists across all steps.

After each write a read-only probe battery renders the resulting state. We
compare exit status, stdout/stderr, every ordered Herdr argv (including failed
calls), and the durable config/runtime/project .multiverse trees, including
briefs, results and status files. Locks, internal read indexes, Git metadata,
and cosmetic .gitignore scaffolding are excluded. Paths, timestamps, generated
ids and the intentional role rename are normalized; task ids retain distinct
per-sequence identities. Existing accepted dry-run rendering exceptions remain
limited to dry-run reports. Herdr calls stay ordered; only the documented
prompt-polling, repeated read-only polls, start-roster, and cd-quoting
allowlist below is folded. Council calls are compared in order within each
seat and within the non-seat stream, allowing concurrent seats to interleave.
Generated seed/brief prose uses the narrow tree-comparison exception table
below; other tree content retains the existing strict comparison.
COLUMNS=80 / NO_COLOR=1 pin terminal rendering width.

Unlike `scenarios/*.json`, there is no pre-recorded expectation — each
seed's sequence is the oracle for itself: Python's own behavior for that
exact sequence is what Rust is compared against. A divergence is reported
with the seed and the concrete argv sequence, which is already a minimal
repro (the sequence is not shrunk further).

Usage:

    python3 conformance/diff_fuzz.py                       # default budget
    python3 conformance/diff_fuzz.py --count 500 --length 10
    python3 conformance/diff_fuzz.py --seed 12345 --count 1 --keep-sandboxes

Exit status: 0 no divergence found (or no oracle configured — see below),
1 at least one divergence, 2 setup error (Rust build failed, configured
oracle path not found, ...).

Requires a frozen Python checkout to compare against: set
MV_CONFORMANCE_PYTHON_REPO or pass --python-repo. There is no personal
default. When neither is set, this prints a 'skipped: ...' message and
exits 0 rather than failing CI runs that have no oracle configured.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import random
import re
import shlex
import shutil
import stat
import subprocess
import sys
import time
from concurrent.futures import ProcessPoolExecutor
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from normalize import PathMap, normalize_elapsed, normalize_text  # noqa: E402
from role_translate import (  # noqa: E402
    ROLE_RENAME,
    translate_config_toml,
    translate_prose_roles,
    translate_role_argv,
)
from sandbox import Sandbox, make_sandbox  # noqa: E402
from rust_binary import build_fake_herdr_binary, build_rust_binary  # noqa: E402

_BASE_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"

CONFIG_TOML = (
    'max_concurrent = 20\n\n'
    '[projects.sandbox]\npath = "{{PROJECT:sandbox}}"\n\n'
    '[projects.sandbox.chains]\n'
    'explorer = ["explorer-seat"]\n'
    'fixer = ["explorer-seat"]\n'
    'reviewer = ["explorer-seat"]\n'
    'council = ["explorer-seat", "claude-seat"]\n\n'
    '[projects.sandbox2]\npath = "{{PROJECT:sandbox2}}"\n\n'
    '[activity.pairs]\nsandbox = ["sandbox2"]\nsandbox2 = ["sandbox"]\n\n'
    '[accounts.explorer-seat]\nkind = "codex"\nconfig_dir_var = "CODEX_HOME"\n'
    'config_dir = "{{DIR:codex-home}}"\npool = "codex:test"\n\n'
    '[accounts.claude-seat]\nkind = "claude"\nconfig_dir_var = "CLAUDE_CONFIG_DIR"\n'
    'config_dir = "{{DIR:claude-home}}"\npool = "claude:test"\ncommand = "fake-claude"\n\n'
    '[accounts.nexus-seat]\nkind = "claude"\nconfig_dir_var = "CLAUDE_CONFIG_DIR"\n'
    'config_dir = "{{DIR:nexus-home}}"\npool = "claude:nexus"\n'
    'command = "fake-nexus"\ninteractive = true\n\n'
    '[roles.council]\nwrite_scope = "none"\nskills = []\n\n'
    '[roles.council.models]\ncodex = "gpt-test"\nclaude = "claude-test"\n\n'
    '[roles.explorer]\nwrite_scope = "none"\neffort = "low"\n'
    'on_exhausted = "degrade"\nskills = []\n\n'
    '[roles.explorer.models]\ncodex = "gpt-test"\n\n'
    '[roles.fixer]\nwrite_scope = "any"\neffort = "high"\n'
    'on_exhausted = "degrade"\nskills = []\n\n'
    '[roles.fixer.models]\ncodex = "gpt-test"\n\n'
    '[roles.reviewer]\nwrite_scope = "any"\neffort = "medium"\n'
    'on_exhausted = "degrade"\nskills = []\n\n'
    '[roles.reviewer.models]\ncodex = "gpt-test"\n'
)

DRY_RUN_ROLES = ("explorer", "fixer", "reviewer")

TITLES = [
    "fix the thing", "investigate flake", "update docs",
    "review the lock", "audit breaker state", "chase the timeout",
]
WHYS = ["it breaks sometimes", "customer reported it", "found during review", ""]
NOTES = ["more evidence", "still reproduces", "confirmed root cause", "needs a second pass"]
EVIDENCE = ["handled in followup", "abandoned, superseded", "fixed upstream", ""]
PRIORITIES = ["P1", "P2", "P3"]
TRIP_KINDS = ["rolling", "weekly", "budget"]
LIST_FLAGS = [[], ["--open"], ["--done"], ["--all"]]


@dataclass
class Action:
    """One step of an abstract, impl-independent command sequence.

    `argv` may reference a previously-created backlog item as the literal
    string `"<item:N>"` (N indexing into the sequence's own running list of
    created items) — resolved per-impl from that impl's own captures dict,
    the same way `run_scenario.py`'s `{{capture:...}}` is, because the two
    implementations mint unrelated, unpredictable item ids for the same
    `add`.
    """

    cwd: str
    argv: list[str]
    capture_pattern: str | None = None  # applied to this impl's raw stdout
    capture_name: str | None = None
    probe: bool = False


def read_probes(items: list[str]) -> list[Action]:
    """Read-only commands that render the state after a write.

    Always run from `sandbox` (where backlog/tasks live and where the
    explorer/fixer/reviewer chains are configured) except `activity inbox
    --peek`, which is issued against both paired projects so cross-project
    mail is visible. `board` has no `--once`; `--no-reconcile --read-only
    --no-color` is the non-interactive form.
    """
    probes: list[Action] = []
    cwd = "sandbox"
    for flags in LIST_FLAGS:
        probes.append(Action(cwd, ["backlog", "list"] + flags, probe=True))
    if items:
        probes.append(Action(cwd, ["backlog", "show", items[-1]], probe=True))
        if len(items) > 1 and items[0] != items[-1]:
            probes.append(Action(cwd, ["backlog", "show", items[0]], probe=True))
    probes.append(Action(
        cwd, ["tasks", "--no-reconcile", "--read-only"],
        probe=True,
    ))
    probes.append(Action(
        cwd, ["tasks", "--status", "done", "--no-reconcile", "--read-only"],
        probe=True,
    ))
    probes.append(Action(
        cwd, ["workers"], probe=True,
    ))
    probes.append(Action(
        cwd, ["health"], probe=True,
    ))
    probes.append(Action(
        cwd, ["board", "--no-reconcile", "--read-only", "--no-color"],
        probe=True,
    ))
    probes.append(Action(cwd, ["review-policy"], probe=True))
    probes.append(Action(cwd, ["accounts", "usage"], probe=True))
    probes.append(Action(cwd, ["activity", "inbox", "--peek"], probe=True))
    probes.append(Action("sandbox2", ["activity", "inbox", "--peek"], probe=True))
    probes.append(Action(cwd, ["halt", "status"], probe=True))
    for role in DRY_RUN_ROLES:
        probes.append(Action(
            cwd,
            ["dispatch", "--dry-run", "--role", role, ".", "fuzz dry-run probe"],
            probe=True,
        ))
    return probes


def gen_sequence(rng: random.Random, length: int) -> list[Action]:
    writes: list[Action] = []
    items: list[str] = []  # "<item:N>" tokens for items created so far
    msg_counter = 0

    def add_item() -> None:
        n = len(items)
        token = f"<item:{n}>"
        items.append(token)
        argv = ["backlog", "add", rng.choice(TITLES)]
        why = rng.choice(WHYS)
        if why:
            argv += ["--why", why]
        if rng.random() < 0.5:
            argv += ["--priority", rng.choice(PRIORITIES)]
        writes.append(Action(
            "sandbox", argv,
            capture_pattern=r"added (\S+) to sandbox",
            capture_name=token,
        ))

    def pick_item() -> str | None:
        return rng.choice(items) if items else None

    def backlog_done() -> None:
        item = pick_item()
        if item is None:
            return add_item()
        argv = ["backlog", "done", item]
        evidence = rng.choice(EVIDENCE)
        if evidence:
            argv += ["--evidence", evidence]
        writes.append(Action("sandbox", argv))

    def backlog_reopen() -> None:
        item = pick_item()
        if item is None:
            return add_item()
        writes.append(Action("sandbox", ["backlog", "reopen", item]))

    def backlog_annotate() -> None:
        item = pick_item()
        if item is None:
            return add_item()
        writes.append(Action(
            "sandbox", ["backlog", "annotate", item, "--note", rng.choice(NOTES)]
        ))

    def backlog_reprioritize() -> None:
        item = pick_item()
        if item is None:
            return add_item()
        writes.append(Action(
            "sandbox",
            ["backlog", "reprioritize", item, "--priority", rng.choice(PRIORITIES)],
        ))

    def accounts_trip() -> None:
        argv = ["accounts", "trip", "codex:test", "--kind", rng.choice(TRIP_KINDS)]
        writes.append(Action("sandbox", argv))

    def accounts_reset() -> None:
        writes.append(Action("sandbox", ["accounts", "reset", "codex:test"]))

    def halt_on() -> None:
        writes.append(Action("sandbox", ["halt", "on", "--reason", "fuzz maintenance"]))

    def halt_off() -> None:
        writes.append(Action("sandbox", ["halt", "off"]))

    def notify_send() -> None:
        writes.append(Action(
            "sandbox",
            ["notify", "--project", "sandbox", "--title", "disk low",
             "--message", f"{rng.randint(1, 99)}% full"],
        ))

    def notify_flush() -> None:
        writes.append(Action("sandbox", ["notify", "--flush"]))

    def activity_send() -> None:
        nonlocal msg_counter
        msg_counter += 1
        src, dst = rng.choice([("sandbox", "sandbox2"), ("sandbox2", "sandbox")])
        writes.append(Action(
            src,
            ["activity", "send", dst, rng.choice(TITLES), "--id", f"fuzz-msg-{msg_counter}"],
        ))

    def dispatch() -> None:
        argv = ["dispatch", "--yes", "--role", rng.choice(DRY_RUN_ROLES),
                ".", rng.choice(TITLES), "--timeout", "2"]
        if rng.random() < 0.5:
            argv.append("--wait")
        writes.append(Action("sandbox", argv))

    def sweep() -> None:
        writes.append(Action("sandbox", ["sweep"] + rng.choice([[], ["--dry-run"], ["--tick"]])))

    def stop() -> None:
        writes.append(Action("sandbox", ["stop", rng.choice(["sandbox", "sandbox2"])]
                             + rng.choice([[], ["--force"]])))

    def start() -> None:
        writes.append(Action("sandbox", ["start", rng.choice(["sandbox", "sandbox2"])]
                             + rng.choice([[], ["--reseed"]])))

    def tasks() -> None:
        writes.append(Action("sandbox", ["tasks", rng.choice(["--reclaimable", "--close-stranded"])]
                             + rng.choice([[], ["--yes"]])))

    def worktrees() -> None:
        writes.append(Action("sandbox", ["worktrees", ".", "--prune"]
                             + rng.choice([[], ["--yes"], ["--dry-run"]])))

    def council() -> None:
        writes.append(Action("sandbox", ["council", ".", rng.choice(TITLES), "--yes", "--timeout", "2"]))

    generators = [
        (dispatch, 4), (sweep, 3), (stop, 2), (start, 2),
        (tasks, 4), (worktrees, 2), (council, 1),
        (add_item, 4),
        (backlog_done, 2),
        (backlog_reopen, 1),
        (backlog_annotate, 2),
        (backlog_reprioritize, 1),
        (accounts_trip, 2),
        (accounts_reset, 1),
        (halt_on, 1),
        (halt_off, 1),
        (notify_send, 2),
        (notify_flush, 1),
        (activity_send, 2),
    ]
    population = [g for g, w in generators for _ in range(w)]

    for _ in range(length):
        rng.choice(population)()

    actions: list[Action] = []
    known_items: list[str] = []
    for write in writes:
        actions.append(write)
        if write.capture_name:
            known_items.append(write.capture_name)
        actions.extend(read_probes(known_items))
    return actions


_CAPTURE_TOKEN_RE = re.compile(r"<item:(\d+)>")


def _substitute(argv: list[str], captures: dict[str, str]) -> list[str] | None:
    out = []
    for token in argv:
        if _CAPTURE_TOKEN_RE.fullmatch(token):
            if token not in captures:
                return None
            out.append(captures[token])
        else:
            out.append(token)
    return out


def _wrapper_script(sandbox: Sandbox, binary: str) -> Path:
    path = sandbox.root / "fake-herdr"
    path.write_text(f'#!/bin/sh\nexec {shlex.quote(binary)} "$@"\n', encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    bin_dir = sandbox.root / "bin"
    bin_dir.mkdir()
    (bin_dir / "herdr").symlink_to(path)
    return path


def _base_env(sandbox: Sandbox, fake_herdr: Path) -> dict[str, str]:
    return {
        "PATH": f"{sandbox.root / 'bin'}:{_BASE_PATH}",
        "HOME": str(sandbox.home),
        "MULTIVERSE_CONFIG": str(sandbox.state_dir / "config.toml"),
        "MULTIVERSE_STATE_DIR": str(sandbox.runtime_dir),
        "HERDR_PLUGIN_CONFIG_DIR": str(sandbox.state_dir),
        "HERDR_PLUGIN_STATE_DIR": str(sandbox.runtime_dir),
        "MULTIVERSE_ALLOW_TEMP_PROJECTS": "1",
        "HERDR_BIN_PATH": str(fake_herdr),
        "HERDR_PANE_ID": "w90:p1",
        # Pin board/workers width so a piped run is not a function of the
        # host tty. `normalize_text` does not fold box-drawing width.
        "COLUMNS": "80",
        "LINES": "24",
        "NO_COLOR": "1",
        "TERM": "dumb",
    }


@dataclass
class StepOutcome:
    argv: list[str]
    exit_code: int | None
    stdout: str
    stderr: str
    error: str | None = None  # harness-level failure (timeout, missing capture, ...)
    herdr_calls: list[list[str]] = field(default_factory=list)  # every call, in order
    probe: bool = False


@dataclass
class SequenceRun:
    outcomes: list[StepOutcome] = field(default_factory=list)
    tree: dict[str, str] = field(default_factory=dict)  # relpath -> normalized content
    sandbox_root: Path | None = None


# Only transient locks, private read caches, Git internals, and cosmetic
# scaffolding are excluded. Task briefs/results/status and activity mail are
# durable state and must be compared, including files inside task worktrees.
def _snapshot_tree(roots: dict[str, Path], paths) -> dict[str, str]:
    out: dict[str, str] = {}
    for label, root in roots.items():
        if not root.exists():
            continue
        for path in sorted(root.rglob("*")):
            if (not path.is_file() or path.name in (".gitignore", ".git")
                    or ".git" in path.relative_to(root).parts
                    or path.name.endswith((".lock", ".index", ".index.fp"))):
                continue
            rel = normalize_text(f"{label}/{path.relative_to(root)}", paths)
            try:
                raw = path.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                out[rel] = "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()
                continue
            out[rel] = normalize_text(raw, paths)
    return out


def seed_fleet(sandbox: Sandbox, seed: int) -> None:
    """Identical measured-shape state per implementation, rooted in its sandbox.

    Seed a recorded pane, a running pane, and a stranded task. The seeded
    records use historical role spellings deliberately, exercising readers.
    New dispatches and starts mutate this same file for the whole sequence.
    """
    rng = random.Random(seed)
    statuses = [rng.choice(["working", "idle", "blocked", "unknown"]) for _ in range(2)]
    state = {"workspaces": [], "panes": [], "agents": [], "tabs": [],
             "next_pane": 100, "next_workspace": 100, "sequence": 5000,
             "text": {}, "pending_text": []}
    fixtures = Path(__file__).resolve().parent.parent / "fixtures" / "herdr"
    for group in ("workspace", "pane", "agent", "tab"):
        records = json.loads((fixtures / f"{group}-list.json").read_text())["result"][group + "s"]
        template = records[0]
        for n, name in enumerate(("sandbox", "sandbox2"), 1):
            record = dict(template)
            record.update(workspace_id=f"w{n}", focused=False)
            if group == "workspace":
                record.update(label=name, active_tab_id=f"w{n}:t1", pane_count=2, tab_count=1, number=n)
            else:
                record.update(tab_id=f"w{n}:t1")
                if group == "tab":
                    record.update(label="1", number=1, pane_count=2)
                else:
                    record.update(cwd=str(sandbox.projects[name]), foreground_cwd=str(sandbox.projects[name]),
                                  pane_id=f"w{n}:p1", agent_status="done", agent="codex")
                    if group == "agent":
                        record.update(name=f"{name}-worker-cx", completion_seq=4999, state_change_seq=4999)
                    state[group + "s"].append(record)
                    record = dict(record)
                    record.update(pane_id=f"w{n}:p2", agent_status=statuses[n - 1])
                    if group == "agent":
                        record.update(name=f"{name}-other-cx")
            state[group + "s"].append(record)
    # A human control pane survives project stop/reclaim operations and is
    # the parent of --current dispatch splits on both implementations.
    state["workspaces"].append({"workspace_id": "w90", "label": "control",
                                "active_tab_id": "w90:t1", "pane_count": 1,
                                "tab_count": 1, "focused": True})
    state["tabs"].append({"workspace_id": "w90", "tab_id": "w90:t1",
                          "pane_count": 1, "focused": True})
    state["panes"].append({"workspace_id": "w90", "tab_id": "w90:t1",
                           "pane_id": "w90:p1", "cwd": str(sandbox.root),
                           "foreground_cwd": str(sandbox.root), "focused": True,
                           "agent_status": "unknown"})
    # Record templates contain only fields measured by mv-herdr::shapes.
    (sandbox.root / "fleet.json").write_text(json.dumps(state))
    for n, (name, root) in enumerate(sandbox.projects.items(), 1):
        mv = root / ".multiverse"
        mv.mkdir()
        (mv / ".gitignore").write_text("*\n")
        lines = []
        for slot, status in ((1, "done"), (2, None), (3, None)):
            task = f"20250101-fixer-{n:01x}{slot:05x}"
            task_dir = mv / "tasks" / task
            task_dir.mkdir(parents=True)
            (task_dir / "brief.md").write_text(f"# Task {task}\nRole: Fixer\n")
            stamp = "2020-01-01T00:00:00+00:00"
            lines.append(f"{stamp}\tevent=dispatch\ttask={task}\tproject={name}\trole=fixer"
                         "\taccount=explorer-seat\tpool=codex:test\toutcome=chain-primary\treason=\tbranch=main\n")
            lines.append(f"{stamp}\tevent=started\ttask={task}\tpane=w{n}:p{slot}\n")
            if status:
                (task_dir / "status").write_text(status)
                (task_dir / "result.md").write_text("Task completed.\n")
                lines.append(f"{stamp}\tevent=result\ttask={task}\tstatus=done\tstate=done\trevisions=0\tpane=w{n}:p{slot}\n")
        (mv / "routing.log").write_text("".join(lines))


def _register_task_ids(sandbox: Sandbox, task_ids: dict[str, str]) -> None:
    for root in sandbox.projects.values():
        log = root / ".multiverse" / "routing.log"
        if log.exists():
            for task in re.findall(r"\btask=(\S+)", log.read_text()):
                if task not in task_ids:
                    token = f"<TASK:{len(task_ids)}>"
                    task_ids[task] = token
                    sandbox.paths.add(task, token)


def _read_calls(path: Path, paths) -> list[list[str]]:
    if not path.exists():
        return []
    return [[normalize_text(arg, paths) for arg in json.loads(line)]
            for line in path.read_text().splitlines()]


def normalize_stdout(text: str, paths, argv: list[str]) -> str:
    """Keep table cell widths and fold board/tasks/workers elapsed clocks.

    Merely replacing an id leaves the old id's padding behind. Preserve
    the cell's rendered width instead, before normalize_text loses the id
    length. Only task-bearing rows of the three table renderers qualify
    for cell rewrites. Elapsed tokens are folded across those renderers;
    all other text and whitespace remain strict.
    """
    if argv[:1] in (["tasks"], ["board"], ["workers"]):
        task_tokens = {concrete: token for concrete, token in paths.entries
                       if token.startswith("<TASK:")}

        def renamed(value: str) -> str:
            # `translate_prose_roles` only rewrites whole words (a hyphen
            # keeps `explorer-seat` from matching) — the short agent-name
            # code embedded in an id-only cell (`sandbox-fix-ex`) needs the
            # same short-code rename `normalize_role_names` applies at
            # compare() time, or its length change leaves this cell's old
            # padding behind, same as an unreplaced task id would.
            return _SHORT_CODE_RE.sub(
                lambda m: _OLD_SHORT_CODE_RENAME[m.group(1)], translate_prose_roles(value)
            )

        def row(line: str) -> str:
            # Tasks start with a four-column status marker; workers start
            # with a padded project cell. A board card has an id-only cell
            # bounded by vertical rails. A task id inside prose is neither.
            table = re.match(r"^(?: {4}|  \* |\S+ {2,})??(\S+) {2,}[A-Za-z][\w-]* {2,}", line)
            # `[^\s│]+`, not `\S+`: a two-up row's blank line for the
            # shorter card (e.g. the agent-name line when its neighbor has
            # fewer rows) starts "│ │ " -- unexcluded, `\S+` happily
            # captures that inner "│" itself as group(1), so `re.search`'s
            # one match never reaches the real content past it.
            boxed = re.search(r"(?:^|│) +([^\s│]+) {2,}│", line)

            def changed(token: str) -> bool:
                return token in task_tokens or renamed(token) != token

            if not any(m and changed(m.group(1)) for m in (table, boxed)):
                return line
            cells = list(re.finditer(r"(?<!\S)(\S+)( +)(?=\S|$)", line))
            if not any(changed(m.group(1)) for m in cells):
                return line

            def cell(match: re.Match[str]) -> str:
                value, padding = match.groups()
                replacement = task_tokens.get(value, renamed(value))
                if replacement == value:
                    return match.group(0)
                width = len(value) + len(padding)
                # An overflowing cell has no recoverable field boundary.
                if width <= len(replacement):
                    return match.group(0)
                return replacement + " " * (width - len(replacement))

            return re.sub(r"(?<!\S)(\S+)( +)(?=\S|$)", cell, line)

        text = "\n".join(row(line) for line in text.split("\n"))
        text = normalize_elapsed(text)
    return normalize_text(text, paths)


def run_sequence(
    actions: list[Action],
    *,
    impl: str,
    python_repo: str | None,
    rust_bin: str | None,
    keep_sandbox: bool = False,
    fake_herdr_bin: str | None = None,
    fleet_seed: int = 0,
) -> SequenceRun:
    sandbox = make_sandbox()
    sandbox.add_dir("codex-home")
    sandbox.add_dir("claude-home")
    sandbox.add_dir("nexus-home")
    sandbox.add_project("sandbox", {"git": True, "files": {"README.md": "hi\n"}})
    sandbox.add_project("sandbox2", {"git": True, "files": {"README.md": "hi\n"}})
    config_toml = CONFIG_TOML
    if impl == "rust":
        config_toml = translate_config_toml(config_toml)
    (sandbox.state_dir / "config.toml").write_text(
        sandbox.resolve(config_toml), encoding="utf-8"
    )
    fake_herdr = _wrapper_script(sandbox, fake_herdr_bin or build_fake_herdr_binary())
    seed_fleet(sandbox, fleet_seed)
    task_ids: dict[str, str] = {}
    _register_task_ids(sandbox, task_ids)

    captures: dict[str, str] = {}
    outcomes: list[StepOutcome] = []

    for index, action in enumerate(actions):
        argv = _substitute(action.argv, captures)
        if argv is None:
            outcomes.append(StepOutcome(
                action.argv, None, "", "", error="missing capture", probe=action.probe,
            ))
            continue
        run_argv = translate_role_argv(argv) if impl == "rust" else argv

        env = _base_env(sandbox, fake_herdr)
        calls_path = sandbox.root / f"herdr-calls-{index}.jsonl"
        env["MV_FAKE_HERDR_STATE"] = str(sandbox.root / "fleet.json")
        env["MV_FAKE_HERDR_CALLS"] = str(calls_path)

        if impl == "python":
            assert python_repo is not None
            env["PYTHONPATH"] = python_repo
            env["PYTHONDONTWRITEBYTECODE"] = "1"
            command = [sys.executable, "-P", "-m", "multiverse.cli", *run_argv]
        else:
            assert rust_bin is not None
            command = [rust_bin, *run_argv]

        cwd = sandbox.projects[action.cwd]
        try:
            proc = subprocess.run(
                command, cwd=cwd, env=env, capture_output=True, text=True, timeout=30
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            outcomes.append(StepOutcome(
                argv, None, "", "", error=f"could not run: {exc}", probe=action.probe,
                herdr_calls=_read_calls(calls_path, sandbox.paths),
            ))
            continue

        if action.capture_pattern:
            match = re.search(action.capture_pattern, proc.stdout)
            if match and action.capture_name:
                captures[action.capture_name] = match.group(1)

        _register_task_ids(sandbox, task_ids)
        herdr_calls = _read_calls(calls_path, sandbox.paths)

        outcomes.append(StepOutcome(
            argv,
            proc.returncode,
            normalize_stdout(proc.stdout, sandbox.paths, argv),
            normalize_text(proc.stderr, sandbox.paths),
            herdr_calls=herdr_calls,
            probe=action.probe,
        ))

    tree = _snapshot_tree(
        {
            "state": sandbox.state_dir,
            "runtime": sandbox.runtime_dir,
            "sandbox/.multiverse": sandbox.projects["sandbox"] / ".multiverse",
            "sandbox2/.multiverse": sandbox.projects["sandbox2"] / ".multiverse",
        },
        sandbox.paths,
    )

    result = SequenceRun(outcomes=outcomes, tree=tree,
                          sandbox_root=sandbox.root if keep_sandbox else None)
    if not keep_sandbox:
        shutil.rmtree(sandbox.root, ignore_errors=True)
    return result


def _step_label(index: int, outcome: StepOutcome) -> str:
    kind = " probe" if outcome.probe else ""
    return f"step {index}{kind} {outcome.argv!r}"


def is_notify_argv(argv: list[str]) -> bool:
    return bool(argv) and argv[0] == "notify"


def is_notify_diff(diff: str) -> bool:
    """True when a compare() line is a notify write, probe, or on-disk state.

    The brief leaves notify to another lane; tree files (`notify-outbox.jsonl`,
    `notify-state.json`) must count, not only argv that starts with `notify`.
    """
    if " ['notify'" in diff or ' ["notify"' in diff:
        return True
    lowered = diff.lower()
    return "notify-outbox" in lowered or "notify-state" in lowered


# Card 20261103-b-42b7f8: the hub's four accepted, permanent `--dry-run`
# divergences (seeds 9001-9004). Each has a reason the hub already weighed:
# agent short-code and appended prompt-file line counts simply follow the
# role rename; the `WOULD RUN` pane-command section mirrors Rust's own real
# runner (verified against live Herdr, not Python's); prompt wrapping is a
# rendering-width artifact that also embeds a sandbox-root path broken
# mid-string by the wrap, which `normalize_text`'s literal substring
# replace can never re-stitch, so the wrapped prompt can carry leftover
# random tmp-dir text that would never match across *any* two runs, same
# implementation included. Normalized here, and only here, so any other
# `--dry-run` divergence still fails compare().
_OLD_SHORT_CODE_RENAME = {
    "orch": "nexu", "expl": "scou", "orac": "seer", "fix": "buil",
    "rev": "crit", "lib": "arch", "dsgn": "arti", "obs": "watc",
    # "coun" is omitted: the rename left it unchanged (role_translate.py).
}
_SHORT_CODE_RE = re.compile(
    r"(?<=-)(" + "|".join(re.escape(k) for k in _OLD_SHORT_CODE_RENAME) + r")(?=-)"
)
_ROLE_FILE_RE = re.compile(
    r"role-(" + "|".join(re.escape(k) for k in ROLE_RENAME) + r")\.md"
)
_LINE_COUNT_RE = re.compile(r"\(\d+ lines\)")
_SECTION_HEADER_RE = re.compile(r"(?m)^([A-Z][^\n]*)\n─+$")
# The reseed prompt intentionally replaced the oracle's human name with "the human".
_SEED_PROMPT_HUMAN_RE = re.compile(
    r"\bDo not assume [A-Z][a-z]+(?:[ '-][A-Z][a-z]+)* is present\."
)


def is_dry_run_argv(argv: list[str]) -> bool:
    return len(argv) >= 2 and argv[0] == "dispatch" and argv[1] == "--dry-run"


def _section_span(text: str, header: str) -> tuple[int, int] | None:
    """Byte span of `header`'s body: from just past its underline to the
    start of the next top-level section header, or end of string for the
    last section. `None` if `header` is not present (a differently-shaped
    report — not this card's four accepted categories — so callers leave
    `text` untouched and let compare() see the real difference).
    """
    headers = list(_SECTION_HEADER_RE.finditer(text))
    for i, match in enumerate(headers):
        if match.group(1) == header:
            start = match.end() + 1  # past the header underline's own "\n"
            end = headers[i + 1].start() if i + 1 < len(headers) else len(text)
            return start, end
    return None


def normalize_dry_run(text: str) -> str:
    """Fold card 20261103-b-42b7f8's four accepted `--dry-run` divergences
    out of `text` before comparison. Everything else in the report is left
    exactly as rendered.
    """
    text = _SHORT_CODE_RE.sub(lambda m: _OLD_SHORT_CODE_RENAME[m.group(1)], text)
    text = _ROLE_FILE_RE.sub(lambda m: f"role-{ROLE_RENAME[m.group(1)]}.md", text)
    text = _LINE_COUNT_RE.sub("(N lines)", text)
    for header in ("WOULD RUN", "PROMPT  (what the agent would receive)"):
        span = _section_span(text, header)
        if span is not None:
            start, end = span
            text = (
                text[:start]
                + "  <normalized -- card 20261103-b-42b7f8>\n\n"
                + text[end:]
            )
    return text


def normalize_role_names(text: str) -> str:
    text = _SEED_PROMPT_HUMAN_RE.sub("Do not assume the human is present.", text)
    text = translate_prose_roles(text)
    return _SHORT_CODE_RE.sub(lambda m: _OLD_SHORT_CODE_RENAME[m.group(1)], text)


_CD_PATH_RE = re.compile(
    r"^cd ('(?:[^']|'\"'\"')*'|(?:[A-Za-z0-9_@%+=:,./-]+|"
    r"<PROJECT:[A-Za-z0-9_-]+>|<TASK:\d+>|<ID>)+)(?= && |$)"
)


def normalize_herdr_calls(calls: list[list[str]], argv: list[str], *, rust: bool,
                          bindings: dict[str, str] | None = None) -> list[list[str]]:
    """The complete intentional Herdr-call allowlist (card 20261105-b-9833a6).

    1. Prompt delivery uses Rust pane-visible reads and agent-list polling
       where Python uses agent-visible reads/agent-get. Fold only contiguous
       polling for the agent/pane just launched or renamed, before/after its
       prompt. Start can re-point a known agent from an earlier sequence step;
       only its exact visible-read target resumes that delivery episode.
       A different target, source, flag, or intervening call is strict.
    2. Start takes one extra agent roster under the project lock, immediately
       after its second workspace-list/pane-list pair. Only Rust may omit
       that one call from comparison; all other fleet listings stay ordered.
    3. Rust always shell-quotes pane-run's initial cd path. Canonicalize just
       that word, preserving the entire remaining script byte-for-byte.
    4. Host load changes counts of identical consecutive read-only polls.
       Fold each run to one on both sides, in every command. Only exact
       agent-list/get and visible agent/pane reads qualify; writes, other
       reads, different targets, and intervening calls stay ordered.

    These are hardened beyond Python intentionally (runner's prompt landing
    checks and start's fresh duplicate-Nexus guard). No sorting, global read
    removal, shell-script tokenization, or other exceptions belong here.
    """
    collapsed: list[list[str]] = []
    for call in calls:
        polling = (call == ["agent", "list"]
                   or (len(call) == 3 and call[:2] == ["agent", "get"])
                   or (len(call) == 5 and call[:2] in (["pane", "read"], ["agent", "read"])
                       and call[3:] == ["--source", "visible"]))
        if polling and collapsed and collapsed[-1] == call:
            continue
        collapsed.append(call)
    calls = collapsed
    if argv[:1] not in (["dispatch"], ["council"], ["start"]):
        return calls
    if bindings is None:
        bindings = {}
    out: list[list[str]] = []
    active: tuple[str, str] | None = None
    roster_pairs = 0
    i = 0
    while i < len(calls):
        call = calls[i]
        if call == ["pane", "list"] and i and calls[i - 1] == ["workspace", "list"]:
            roster_pairs += 1
            if (rust and argv[:1] == ["start"] and roster_pairs == 2
                    and calls[i + 1:i + 2] == [["agent", "list"]]):
                out.append(call)
                active = None
                i += 2
                continue
        if (not active and argv[:1] == ["start"] and len(call) == 5
                and call[:2] == ["agent", "read"] and call[3:] == ["--source", "visible"]
                and call[2] in bindings):
            active = (call[2], bindings[call[2]])
        if active:
            agent, pane = active
            polling = (["agent", "get", agent],
                       ["agent", "read", agent, "--source", "visible"],
                       ["agent", "read", pane, "--source", "visible"],
                       ["pane", "read", pane, "--source", "visible"])
            end = i
            targeted = False
            while end < len(calls):
                if calls[end] in polling:
                    targeted = True
                elif calls[end] != ["agent", "list"]:
                    break
                end += 1
            if targeted:
                out.append(["<prompt-poll>", agent, pane])
                i = end
                continue
            if call[:3] != ["agent", "prompt", agent]:
                active = None
        if len(call) == 4 and call[:2] == ["pane", "run"]:
            match = _CD_PATH_RE.match(call[3])
            if match:
                path = shlex.split(match.group(1))[0]
                call = call[:3] + ["cd " + shlex.quote(path) + call[3][match.end():]]
        out.append(call)
        if len(call) == 4 and call[:2] == ["agent", "rename"]:
            active = (call[3], call[2])
        elif (len(call) >= 7 and call[:2] == ["agent", "start"]
              and "--pane" in call and not call[2].startswith("-")):
            pane_index = call.index("--pane") + 1
            if pane_index < len(call):
                active = (call[2], call[pane_index])
        if active:
            bindings[active[0]] = active[1]
        i += 1
    return out


def normalize_council_calls(calls: list[list[str]], argv: list[str], *, rust: bool,
                            bindings: dict[str, str]) -> tuple[list[list[str]], dict[str, list[list[str]]]]:
    """Compare concurrent seats independently, retaining each seat's order.

    Discover agent/pane bindings before grouping, so pane setup preceding
    agent start/rename belongs to that seat too. Untargeted calls retain
    their own order. Include global agent-list polls in each seat's polling
    normalization: only the existing allowlist may consume those polls.
    """
    targets: dict[str, str] = {}
    for call in calls:
        if len(call) == 4 and call[:2] == ["agent", "rename"]:
            targets[call[2]] = targets[call[3]] = call[3]
        elif (len(call) >= 7 and call[:2] == ["agent", "start"]
              and "--pane" in call and not call[2].startswith("-")):
            pane_index = call.index("--pane") + 1
            if pane_index < len(call):
                targets[call[pane_index]] = targets[call[2]] = call[2]

    def seat(call: list[str]) -> str | None:
        if len(call) >= 3 and call[0] in ("agent", "pane"):
            return targets.get(call[2])
        return None

    owners = [seat(call) for call in calls]
    non_seat = [call for call, owner in zip(calls, owners) if owner is None]
    # The normalizer preserves call objects unless it rewrites a targeted
    # pane-run or emits a prompt-poll marker. Identity distinguishes equal
    # global calls at different positions, including calls it consumes.
    global_ids = {id(call) for call in non_seat}
    poll_ids = {id(call) for call in non_seat if call == ["agent", "list"]}
    consumed: set[int] = set()
    seats: dict[str, list[list[str]]] = {}
    for name in sorted(set(targets.values())):
        projected = [call for call, owner in zip(calls, owners)
                     if owner == name or id(call) in poll_ids]
        normalized = normalize_herdr_calls(projected, argv, rust=rust, bindings=bindings)
        consumed.update(poll_ids - {id(call) for call in normalized})
        seats[name] = [call for call in normalized if id(call) not in global_ids]
    non_seat = [call for call in non_seat if id(call) not in consumed]
    return normalize_herdr_calls(non_seat, argv, rust=rust, bindings=bindings), seats


# Generated prompt exceptions (card 20261105-b-9833a6). Full paths only:
# path under a project's .multiverse       compared facts
# nexus.md / orchestrator.md               existence + seed-digest marker presence
# tasks/<one task directory>/brief.md      task id, scope paths, result/status paths
# Public-release prompt prose and digest values intentionally differ. No other
# files (including result/status, or similarly named files) get this allowance.
_PROMPT_TREE_EXCEPTIONS = (
    (re.compile(r"(?:[^/]+/)*\.multiverse/(?:nexus|orchestrator)\.md"), "seed"),
    (re.compile(r"(?:[^/]+/)*\.multiverse/tasks/[^/]+/brief\.md"), "brief"),
)
_SEED_DIGEST_MARKER_RE = re.compile(r"<!-- multiverse:seed-digest [0-9a-f]+ -->")


def _brief_facts(text: str) -> tuple[str, tuple[str, ...], str, str] | None:
    """Read shared structured fields, never facts mentioned in appended prose.

    Incomplete historical fixtures or malformed briefs fall back to strict
    comparison rather than silently losing missing fields.
    """
    header = text.split("\n## ", 1)[0]
    fields = []
    for pattern in (r"^# Task (.+)$", r"^Result path: *(.+)$", r"^Status path: *(.+)$"):
        matches = re.findall(pattern, header, re.MULTILINE)
        if len(matches) != 1 or not matches[0].strip():
            return None
        fields.append(matches[0].strip())
    scope = re.search(r"^## Write scope\n(.*?)(?=^## |\Z)", text, re.MULTILINE | re.DOTALL)
    if scope is None or not scope.group(1).strip():
        return None
    paths = tuple(line[2:].strip() for line in scope.group(1).splitlines()
                  if line.startswith("- "))
    return fields[0], paths, fields[1], fields[2]


def _tree_contents_match(relpath: str, python: str | None, rust: str | None) -> bool:
    if python is None or rust is None:
        return python == rust
    for pattern, kind in _PROMPT_TREE_EXCEPTIONS:
        if not pattern.fullmatch(relpath):
            continue
        if kind == "seed":
            return bool(_SEED_DIGEST_MARKER_RE.search(python)) == bool(
                _SEED_DIGEST_MARKER_RE.search(rust)
            )
        else:
            p_facts, r_facts = _brief_facts(python), _brief_facts(rust)
            if p_facts is not None and r_facts is not None:
                return p_facts == r_facts
        break
    return python == rust


def compare(py_run: SequenceRun, rs_run: SequenceRun) -> list[str]:
    diffs: list[str] = []
    p_bindings: dict[str, str] = {}
    r_bindings: dict[str, str] = {}
    if len(py_run.outcomes) != len(rs_run.outcomes):
        diffs.append(
            f"outcome count mismatch python={len(py_run.outcomes)} "
            f"rust={len(rs_run.outcomes)}"
        )
    for i, (p, r) in enumerate(zip(py_run.outcomes, rs_run.outcomes)):
        label = _step_label(i, p)
        p_calls = [[normalize_role_names(arg) for arg in call] for call in p.herdr_calls]
        normalize_calls = normalize_council_calls if p.argv[:1] == ["council"] else normalize_herdr_calls
        p_calls = normalize_calls(p_calls, p.argv, rust=False, bindings=p_bindings)
        r_calls = normalize_calls(r.herdr_calls, p.argv, rust=True, bindings=r_bindings)
        if p_calls != r_calls:
            diffs.append(f"{label}: herdr calls mismatch python={p_calls!r} rust={r_calls!r}")
        if p.error or r.error:
            diffs.append(f"{label}: harness error python={p.error!r} rust={r.error!r}")
            continue
        if p.exit_code != r.exit_code:
            diffs.append(
                f"{label}: exit mismatch python={p.exit_code} rust={r.exit_code}\n"
                f"  python stdout: {p.stdout!r}\n  rust stdout: {r.stdout!r}\n"
                f"  python stderr: {p.stderr!r}\n  rust stderr: {r.stderr!r}"
            )
        p_stdout = normalize_role_names(p.stdout)
        p_stderr = normalize_role_names(p.stderr)
        r_stdout = normalize_role_names(r.stdout)
        if is_dry_run_argv(p.argv):
            p_stdout = normalize_dry_run(p_stdout)
            r_stdout = normalize_dry_run(r_stdout)
        if p_stdout != r_stdout:
            diffs.append(
                f"{label}: stdout mismatch\n"
                f"  --- python ---\n{p_stdout}\n  --- rust ---\n{r_stdout}"
            )
        if p_stderr != normalize_role_names(r.stderr):
            diffs.append(
                f"{label}: stderr mismatch\n"
                f"  --- python ---\n{p_stderr}\n  --- rust ---\n{r.stderr}"
            )
    py_tree = {normalize_role_names(k): normalize_role_names(v) for k, v in py_run.tree.items()}
    rs_tree = {normalize_role_names(k): normalize_role_names(v) for k, v in rs_run.tree.items()}
    all_files = sorted(set(py_tree) | set(rs_tree))
    for relpath in all_files:
        p_content = py_tree.get(relpath)
        r_content = rs_tree.get(relpath)
        if not _tree_contents_match(relpath, p_content, r_content):
            diffs.append(
                f"tree {relpath!r} mismatch\n"
                f"  --- python ---\n{p_content!r}\n  --- rust ---\n{r_content!r}"
            )
    return diffs


def self_test() -> None:
    """Check comparison normalization without building Rust or using an oracle."""
    seed_path = "sandbox/.multiverse/nexus.md"
    py_seed = "<!-- multiverse:seed-digest abcdef0123456789 -->\nOld prose"
    rs_seed = "<!-- multiverse:seed-digest 0123456789abcdef -->\nNew prose"
    assert not compare(SequenceRun(tree={seed_path: py_seed}),
                       SequenceRun(tree={seed_path: rs_seed}))
    brief_path = "sandbox/.multiverse/tasks/<TASK:6>/brief.md"
    brief = ("# Task <TASK:6>\nResult path: result.md\nStatus path: status\n"
             "## Write scope\ndeclared:\n- src/a.rs\n\n## Goal\nOld prose\n")
    assert not compare(SequenceRun(tree={brief_path: brief}),
                       SequenceRun(tree={brief_path: brief.replace("Old prose", "New prose")}))
    assert compare(SequenceRun(tree={brief_path: brief}),
                   SequenceRun(tree={brief_path: brief.replace("src/a.rs", "src/b.rs")}))
    old = (
        "Orchestrator Explorer Fixer Librarian Reviewer Oracle Designer Observer\n"
        "orchestrator explorer fixer librarian reviewer oracle designer observer\n"
        "Do not assume Example Person is present.\n"
    )
    new = (
        "Nexus Scout Builder Archivist Critic Seer Artisan Watcher\n"
        "nexus scout builder archivist critic seer artisan watcher\n"
        "Do not assume the human is present.\n"
    )
    unchanged = "Council council preexplorer explorer2 explorer_name explorer-seat"
    assert normalize_role_names(unchanged) == unchanged
    assert normalize_role_names(new) == new
    assert normalize_role_names("(Explorer), reviewer.") == "(Scout), critic."
    py_run = SequenceRun(
        outcomes=[StepOutcome(["start"], 0, old, old, herdr_calls=[["agent", old]])],
        tree={"explorer/brief.md": old},
    )
    rs_run = SequenceRun(
        outcomes=[StepOutcome(["start"], 0, new, new, herdr_calls=[["agent", new]])],
        tree={"scout/brief.md": new},
    )
    assert not compare(py_run, rs_run), compare(py_run, rs_run)
    rs_run.outcomes[0].stdout += "unexpected difference\n"
    assert any("stdout mismatch" in diff for diff in compare(py_run, rs_run))
    assert normalize_role_names("Do not assume the critic is present.") == (
        "Do not assume the critic is present."
    )
    tables = []
    for role in ("explorer", "scout"):
        task = f"20261106-{role}-abcdef"
        paths = PathMap()
        paths.add(task, "<TASK:6>")
        tables.append(normalize_stdout(f"{task:27}{role:11}seat  running\n", paths, ["tasks"]))
    assert tables[0] == tables[1]
    assert tables[0] != tables[1].replace("seat  running", "seat   running")

    rename = ["agent", "rename", "w1:p1", "sandbox-scou-ex"]
    py_calls = [rename, ["agent", "read", "sandbox-scou-ex", "--source", "visible"],
                ["agent", "get", "sandbox-scou-ex"]]
    rs_calls = [rename, ["pane", "read", "w1:p1", "--source", "visible"], ["agent", "list"]]
    assert normalize_herdr_calls(py_calls, ["dispatch"], rust=False) == (
        normalize_herdr_calls(rs_calls, ["dispatch"], rust=True)
    )
    rs_calls[1] = ["pane", "read", "w1:p2", "--source", "visible"]
    assert normalize_herdr_calls(py_calls, ["dispatch"], rust=False) != (
        normalize_herdr_calls(rs_calls, ["dispatch"], rust=True)
    )
    roster = [["workspace", "list"], ["pane", "list"], ["agent", "list"],
              ["workspace", "list"], ["pane", "list"]]
    assert normalize_herdr_calls(roster + [["agent", "list"]], ["start"], rust=True) == roster
    assert normalize_herdr_calls([["pane", "run", "w1:p1", "cd '/tmp/project' && unset TOKEN"]],
                                 ["dispatch"], rust=True) == (
        [["pane", "run", "w1:p1", "cd /tmp/project && unset TOKEN"]]
    )


def run_seed(seed: int, length: int, python_repo: str, rust_bin: str,
             fake_herdr_bin: str, keep_sandboxes: bool):
    actions = gen_sequence(random.Random(seed), length)
    py_run = run_sequence(actions, impl="python", python_repo=python_repo,
                          rust_bin=None, keep_sandbox=keep_sandboxes,
                          fake_herdr_bin=fake_herdr_bin, fleet_seed=seed)
    rs_run = run_sequence(actions, impl="rust", python_repo=None,
                          rust_bin=rust_bin, keep_sandbox=keep_sandboxes,
                          fake_herdr_bin=fake_herdr_bin, fleet_seed=seed)
    return seed, [a.argv for a in actions if not a.probe], compare(py_run, rs_run), py_run.sandbox_root, rs_run.sandbox_root


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--self-test", action="store_true",
                        help="check comparison normalization without an oracle or Rust build")
    parser.add_argument(
        "--count", type=int, default=60,
        help="number of sequences to run (default 60); use --jobs 3 for a deeper sweep",
    )
    parser.add_argument("--length", type=int, default=6, help="commands per sequence")
    parser.add_argument("--seed", type=int, default=0, help="base seed; sequence N uses seed+N")
    parser.add_argument(
        "--python-repo",
        default=os.environ.get("MV_CONFORMANCE_PYTHON_REPO"),
        help="path to the frozen Python checkout (required to run; no "
        "default — set this flag or MV_CONFORMANCE_PYTHON_REPO)",
    )
    parser.add_argument("--rust-bin", help="explicit binary override (otherwise cargo build)")
    parser.add_argument("--keep-sandboxes", action="store_true")
    parser.add_argument("--jobs", type=int, choices=range(1, 4), default=1,
                        help="isolated sequence workers (1-3; default 1)")
    parser.add_argument("--max-report", type=int, default=150, help="stop after this many divergent seeds")
    args = parser.parse_args(argv)

    if args.self_test:
        self_test()
        print("normalization self-test passed")
        return 0

    if args.count < 0 or args.length < 0 or args.max_report < 1:
        parser.error("count/length must be nonnegative and max-report must be positive")

    if not args.python_repo:
        print("skipped: set MV_CONFORMANCE_PYTHON_REPO (or pass --python-repo) "
              "to run the differential fuzz harness against the frozen Python oracle")
        return 0

    if not Path(args.python_repo).is_dir():
        print(f"python oracle checkout not found at {args.python_repo!r}", file=sys.stderr)
        return 2

    try:
        os.environ["CARGO_BUILD_JOBS"] = "4"
        fake_herdr_bin = build_fake_herdr_binary()
        rust_bin = str(Path(args.rust_bin).resolve()) if args.rust_bin else build_rust_binary()
    except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
        print(f"Rust build failed: {exc}", file=sys.stderr)
        return 2

    start = time.monotonic()
    divergent = 0
    # Workers use independent sandboxes and never share a fake state file.
    # map preserves seed order in the report even when jobs finish out of order.
    with ProcessPoolExecutor(max_workers=args.jobs) as pool:
        runs = pool.map(run_seed, range(args.seed, args.seed + args.count),
                        [args.length] * args.count, [args.python_repo] * args.count,
                        [rust_bin] * args.count, [fake_herdr_bin] * args.count,
                        [args.keep_sandboxes] * args.count)
        for n, (seed, writes, diffs, py_root, rs_root) in enumerate(runs):
            if diffs:
                divergent += 1
                print(f"=== DIVERGENCE seed={seed} ===")
                print(f"repro: python3 conformance/diff_fuzz.py --seed {seed} --count 1 --length {args.length} --keep-sandboxes")
                print(f"sequence: {json.dumps(writes)}")
                notify_only = diffs and all(is_notify_diff(d) for d in diffs)
                if notify_only:
                    print("  (notify-only: left for another lane)")
                for d in diffs:
                    print(f"  - {d}")
                if py_root:
                    print(f"  python sandbox kept at: {py_root}")
                if rs_root:
                    print(f"  rust sandbox kept at: {rs_root}")
                if divergent >= args.max_report:
                    print(f"stopping after {divergent} divergent seeds (--max-report)")
                    pool.shutdown(wait=True, cancel_futures=True)
                    break
            else:
                print(f"seed={seed}: matched", flush=True)

    elapsed = time.monotonic() - start
    ran = n + 1 if args.count else 0
    print(f"ran {ran} sequence(s) ({args.length} commands each) in {elapsed:.1f}s; {divergent} divergent")
    return 1 if divergent else 0


if __name__ == "__main__":
    raise SystemExit(main())
