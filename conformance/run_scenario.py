#!/usr/bin/env python3
"""Run one conformance scenario against the Python oracle or a Rust binary.

See README.md for the scenario format. Usage:

    python3 run_scenario.py scenarios/doctor-missing-accounts.json
    python3 run_scenario.py scenarios/doctor-missing-accounts.json --impl rust --rust-bin ../target/debug/multiverse

Rust is built from this checkout by default. Exit status: 0 pass, 1 parity
failure, 2 setup error. Python replay is optional and requires its checkout.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import stat
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from normalize import normalize_text  # noqa: E402
from role_translate import translate_config_toml, translate_expect_text, translate_role_argv  # noqa: E402
from sandbox import Sandbox, make_sandbox  # noqa: E402
from rust_binary import build_fake_herdr_binary, build_rust_binary  # noqa: E402

_BASE_PATH = "/usr/bin:/bin:/usr/sbin:/sbin"
_CAPTURE_TOKEN_RE = re.compile(r"\{\{capture:([A-Za-z0-9_]+)\}\}")

# Built at most once per harness process — `herdr_seed` scenarios are rare
# enough that rebuilding per-scenario would still be cheap (cargo's own
# incremental cache), but there is no reason to pay even that twice.
_FAKE_HERDR_BIN: str | None = None


def _fake_herdr_binary() -> str:
    global _FAKE_HERDR_BIN
    if _FAKE_HERDR_BIN is None:
        _FAKE_HERDR_BIN = build_fake_herdr_binary()
    return _FAKE_HERDR_BIN


@dataclass
class StepResult:
    index: int
    argv: list[str]
    status: str


@dataclass
class ScenarioResult:
    ok: bool
    failures: list[str] = field(default_factory=list)
    sandbox_root: Path | None = None
    steps: list[StepResult] = field(default_factory=list)


def _substitute_captures(text: str, captures: dict[str, str]) -> str:
    def repl(match: re.Match) -> str:
        name = match.group(1)
        if name not in captures:
            raise KeyError(f"no capture named {name!r} yet (check step order)")
        return captures[name]

    return _CAPTURE_TOKEN_RE.sub(repl, text)


def _render(value, sandbox: Sandbox, captures: dict[str, str]):
    if isinstance(value, str):
        return sandbox.resolve(_substitute_captures(value, captures))
    if isinstance(value, list):
        return [_render(item, sandbox, captures) for item in value]
    if isinstance(value, dict):
        return {key: _render(item, sandbox, captures) for key, item in value.items()}
    return value


def _wrapper_script(sandbox: Sandbox) -> Path:
    """A one-line exec wrapper so `HERDR_BIN_PATH` names a single executable
    token, regardless of where this checkout's Python interpreter lives."""
    path = sandbox.root / "fake-herdr"
    fake_herdr_py = Path(__file__).resolve().parent / "fake_herdr.py"
    path.write_text(
        f"#!/bin/sh\nexec {shlex.quote(sys.executable)} "
        f"{shlex.quote(str(fake_herdr_py))} \"$@\"\n",
        encoding="utf-8",
    )
    path.chmod(path.stat().st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    # Guard a CLI that accidentally invokes bare `herdr` as well. It must
    # still hit the transcript player, never a host binary or socket.
    bin_dir = sandbox.root / "bin"
    bin_dir.mkdir()
    (bin_dir / "herdr").symlink_to(path)
    return path


def _stateful_herdr_target(sandbox: Sandbox, binary: str) -> Path:
    """Point `HERDR_BIN_PATH` straight at the real, compiled `fake-herdr`
    binary for a `"herdr_seed"` scenario. Unlike `_wrapper_script`, no shell
    shim is needed — it is already a single native executable — only the
    same bare-`herdr`-on-PATH guard."""
    bin_dir = sandbox.root / "bin"
    bin_dir.mkdir(exist_ok=True)
    (bin_dir / "herdr").symlink_to(binary)
    return Path(binary)


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
    }


def _snapshot(root: Path) -> dict[str, tuple[int, str]]:
    """relpath -> (mode, sha256) for every file under `root`, if it exists."""
    snap: dict[str, tuple[int, str]] = {}
    if not root.exists():
        return snap
    for path in sorted(root.rglob("*")):
        if path.is_file():
            rel = str(path.relative_to(root))
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            snap[rel] = (path.stat().st_mode & 0o777, digest)
    return snap


def _run_step(
    step: dict,
    *,
    sandbox: Sandbox,
    captures: dict[str, str],
    step_index: int,
    impl: str,
    python_repo: str | None,
    rust_bin: str | None,
    fake_herdr: Path,
    stateful_herdr_state: Path | None,
    failures: list[str],
) -> bool:
    """Returns whether the step actually ran a command. `False` means a
    later step's `{{capture:...}}` reference has nothing to substitute, so
    the caller should stop the scenario's remaining steps rather than raise.

    `stateful_herdr_state` is set only for a `"herdr_seed"` scenario: `fake_herdr`
    is then the real, compiled stateful binary rather than the strict
    transcript player, so there is no per-step `"transcript"` to script or
    exhaustion-check — the live pane/agent/workspace state it reads and
    mutates is this one shared `MV_FAKE_HERDR_STATE` file instead."""
    cwd = sandbox.projects[step["cwd"]]
    if "cwd_subdir" in step:
        cwd = cwd / step["cwd_subdir"]
    try:
        argv = _render(step["argv"], sandbox, captures)
        extra_env = _render(step.get("env", {}), sandbox, captures)
        transcript = _render(step.get("transcript", []), sandbox, captures)
    except KeyError as exc:
        failures.append(f"step {step_index}: {exc}")
        return False
    if stateful_herdr_state is not None and transcript:
        failures.append(
            f"step {step_index}: scenario uses \"herdr_seed\" (stateful fake "
            f"Herdr); per-step \"transcript\" entries are not supported — seed "
            f"state with the scenario-level \"herdr_seed\" list instead"
        )
        return False
    if impl == "rust":
        argv = translate_role_argv(argv)

    env = _base_env(sandbox, fake_herdr)
    isolate_keys = [
        "PATH", "HOME", "MULTIVERSE_CONFIG", "MULTIVERSE_STATE_DIR", "HERDR_BIN_PATH",
        "HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR",
    ]
    if stateful_herdr_state is not None:
        env["MV_FAKE_HERDR_STATE"] = str(stateful_herdr_state)
        isolate_keys.append("MV_FAKE_HERDR_STATE")
    else:
        transcript_path = sandbox.root / f"herdr-transcript-{step_index}.json"
        state_path = sandbox.root / f"herdr-state-{step_index}.json"
        transcript_path.write_text(json.dumps(transcript), encoding="utf-8")
        env["MV_CONFORMANCE_TRANSCRIPT"] = str(transcript_path)
        env["MV_CONFORMANCE_STATE"] = str(state_path)
        isolate_keys += ["MV_CONFORMANCE_TRANSCRIPT", "MV_CONFORMANCE_STATE"]

    isolated_env = env.copy()
    env.update(extra_env)
    # Scenario overrides may add pane/task/vendor inputs, but cannot retarget
    # the filesystem or executable beyond this isolated run.
    for key in isolate_keys:
        if env[key] != isolated_env[key]:
            failures.append(f"step {step_index}: cannot override isolated {key}")
            return False

    if impl == "python":
        assert python_repo is not None
        env["PYTHONPATH"] = python_repo
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        command = [sys.executable, "-P", "-m", "multiverse.cli", *argv]
    else:
        assert rust_bin is not None
        command = [rust_bin, *argv]

    try:
        proc = subprocess.run(
            command, cwd=cwd, env=env, capture_output=True, text=True, timeout=30
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        # A bad explicit override or command timeout is a scenario failure;
        # batch replay must continue and include it in the matrix.
        failures.append(f"step {step_index}: could not run {command!r}: {exc}")
        return False

    for name, pattern in step.get("capture", {}).items():
        match = re.search(pattern, proc.stdout)
        if not match:
            failures.append(
                f"step {step_index}: capture {name!r} pattern {pattern!r} did "
                f"not match stdout"
            )
            continue
        captures[name] = match.group(1)

    expect = step.get("expect", {})
    if "exit" in expect and proc.returncode != expect["exit"]:
        failures.append(
            f"step {step_index}: exit {proc.returncode}, expected {expect['exit']}\n"
            f"  stdout: {proc.stdout!r}\n  stderr: {proc.stderr!r}"
        )

    for stream_name, raw in (("stdout", proc.stdout), ("stderr", proc.stderr)):
        actual = normalize_text(raw, sandbox.paths).rstrip()
        if stream_name in expect:
            wanted = expect[stream_name].rstrip()
            if impl == "rust":
                wanted = translate_expect_text(wanted)
            if actual != wanted:
                failures.append(
                    f"step {step_index}: {stream_name} mismatch\n"
                    f"  --- expected ---\n{wanted}\n  --- actual ---\n{actual}"
                )
        contains_key = f"{stream_name}_contains"
        if contains_key in expect:
            # Needles are written in already-normalized form (`<ID>`,
            # `<PROJECT:name>`, ...), matching what `actual` has just been
            # folded down to — not `{{...}}` sandbox-template syntax.
            for needle in expect[contains_key]:
                wanted_needle = translate_expect_text(needle) if impl == "rust" else needle
                if wanted_needle not in actual:
                    failures.append(
                        f"step {step_index}: {stream_name} missing expected "
                        f"substring: {wanted_needle!r}\n  --- actual ---\n{actual}"
                    )

    if stateful_herdr_state is None:
        state = json.loads(state_path.read_text()) if state_path.exists() else {"index": 0}
        if state["index"] != len(transcript):
            failures.append(
                f"step {step_index}: herdr transcript not exhausted — "
                f"{state['index']} of {len(transcript)} scripted calls were made"
            )
        for error in state.get("errors", []):
            failures.append(f"step {step_index}: fake-herdr: {error}")

    return True


def run_scenario(
    scenario: dict,
    *,
    impl: str,
    python_repo: str | None,
    rust_bin: str | None,
    keep_sandbox: bool = False,
) -> ScenarioResult:
    sandbox = make_sandbox()
    failures: list[str] = []
    captures: dict[str, str] = {}
    steps: list[StepResult] = []

    try:
        for key, spec in scenario.get("dirs", {}).items():
            sandbox.add_dir(key, mode=spec.get("mode"), files=spec.get("files"))
        for name, spec in scenario.get("projects", {}).items():
            sandbox.add_project(name, spec)

        for name, content in scenario.get("runtime_files", {}).items():
            target = sandbox.runtime_dir / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")

        config_toml = scenario.get("home", {}).get("config_toml", "")
        if impl == "rust":
            config_toml = translate_config_toml(config_toml)
        (sandbox.state_dir / "config.toml").write_text(
            sandbox.resolve(config_toml), encoding="utf-8"
        )

        herdr_seed = scenario.get("herdr_seed")
        stateful_herdr_state: Path | None = None
        run_steps = True
        if herdr_seed:
            if impl != "rust":
                failures.append(
                    "scenario declares \"herdr_seed\" (stateful fake Herdr "
                    "pane/agent lifecycle); only --impl rust supports it — "
                    "there is no stateful fake Herdr for the Python oracle"
                )
                run_steps = False
                fake_herdr = _wrapper_script(sandbox)
            else:
                try:
                    fake_herdr_bin = _fake_herdr_binary()
                except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
                    failures.append(f"could not build fake-herdr: {exc}")
                    run_steps = False
                    fake_herdr = _wrapper_script(sandbox)
                else:
                    fake_herdr = _stateful_herdr_target(sandbox, fake_herdr_bin)
                    stateful_herdr_state = sandbox.root / "fake-herdr-state.json"
                    for seed_call in _render(herdr_seed, sandbox, captures):
                        proc = subprocess.run(
                            [str(fake_herdr), *seed_call],
                            env={"MV_FAKE_HERDR_STATE": str(stateful_herdr_state)},
                            capture_output=True, text=True,
                        )
                        if proc.returncode != 0:
                            failures.append(
                                f"herdr_seed {seed_call!r} failed (exit "
                                f"{proc.returncode}): {proc.stderr.strip()}"
                            )
                            run_steps = False
                            break
        else:
            fake_herdr = _wrapper_script(sandbox)

        # "unchanged" assertions compare the tree from right before the first
        # step to right after the last one — there is no partial-scenario
        # snapshot point, so every root named by an `unchanged` entry is
        # captured once, here, before any step runs.
        before_snapshots = {
            entry["root"]: _snapshot(sandbox.projects[entry["root"]])
            for entry in scenario.get("expect_tree", [])
            if entry.get("unchanged")
        }

        for index, step in enumerate(scenario.get("steps", []) if run_steps else []):
            before = len(failures)
            ran = _run_step(
                step,
                sandbox=sandbox,
                captures=captures,
                step_index=index,
                impl=impl,
                python_repo=python_repo,
                rust_bin=rust_bin,
                fake_herdr=fake_herdr,
                stateful_herdr_state=stateful_herdr_state,
                failures=failures,
            )
            steps.append(StepResult(
                index=index, argv=step["argv"],
                status="PASS" if ran and len(failures) == before else "FAIL",
            ))
            # Continue independent commands even after failures. A dependent
            # command cannot launch without its capture, and is shown explicitly.
            if not ran and any("no capture named" in item for item in failures[before:]):
                steps[-1].status = "NOT RUN (missing capture)"

        for entry in scenario.get("expect_tree", []):
            root_name = entry["root"]
            root_path = sandbox.projects[root_name]
            if entry.get("unchanged"):
                before = before_snapshots.get(root_name, {})
                after = _snapshot(root_path)
                if before != after:
                    failures.append(
                        f"expect_tree: {root_name!r} changed when it should not have\n"
                        f"  before: {sorted(before)}\n  after: {sorted(after)}"
                    )
            for relpath, rule in entry.get("files", {}).items():
                target = root_path / relpath
                if rule.get("absent"):
                    if target.exists():
                        failures.append(f"expect_tree: {relpath} should not exist")
                    continue
                if not target.exists():
                    failures.append(f"expect_tree: {relpath} does not exist")
                    continue
                raw = target.read_text(encoding="utf-8", errors="replace")
                actual = normalize_text(raw, sandbox.paths).rstrip()
                if "equals" in rule:
                    wanted = sandbox.resolve(rule["equals"]).rstrip()
                    if actual != wanted:
                        failures.append(
                            f"expect_tree: {relpath} content mismatch\n"
                            f"  --- expected ---\n{wanted}\n  --- actual ---\n{actual}"
                        )
                if "regex" in rule and not re.search(rule["regex"], actual, re.MULTILINE):
                    failures.append(
                        f"expect_tree: {relpath} did not match /{rule['regex']}/\n"
                        f"  --- actual ---\n{actual}"
                    )
                if "mode" in rule:
                    actual_mode = target.stat().st_mode & 0o777
                    if actual_mode != rule["mode"]:
                        failures.append(
                            f"expect_tree: {relpath} mode {oct(actual_mode)}, "
                            f"expected {oct(rule['mode'])}"
                        )
    finally:
        if not keep_sandbox:
            shutil.rmtree(sandbox.root, ignore_errors=True)

    return ScenarioResult(
        ok=not failures,
        failures=failures,
        sandbox_root=sandbox.root if keep_sandbox else None,
        steps=steps,
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scenario", type=Path)
    parser.add_argument("--impl", choices=["python", "rust"], default="rust")
    parser.add_argument(
        "--python-repo",
        default=os.environ.get("MV_CONFORMANCE_PYTHON_REPO"),
        help="path to the frozen Python checkout (required for --impl python; "
        "no default — set this flag or MV_CONFORMANCE_PYTHON_REPO)",
    )
    parser.add_argument(
        "--rust-bin",
        help="explicit binary override (otherwise cargo build -p mv-cli --locked)",
    )
    parser.add_argument("--keep-sandbox", action="store_true")
    args = parser.parse_args(argv)

    if args.impl == "rust":
        try:
            args.rust_bin = (
                str(Path(args.rust_bin).resolve()) if args.rust_bin else build_rust_binary()
            )
        except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
            print(f"Rust build failed: {exc}", file=sys.stderr)
            if isinstance(exc, subprocess.CalledProcessError):
                print(exc.stdout, file=sys.stderr)
                print(exc.stderr, file=sys.stderr)
            return 2

    if args.impl == "python" and not args.python_repo:
        print(
            f"{args.scenario.stem}: PYTHON not run — no checkout configured "
            "(--python-repo or MV_CONFORMANCE_PYTHON_REPO). There is no "
            "personal default; point this at your Python checkout."
        )
        return 2

    scenario = json.loads(args.scenario.read_text())
    result = run_scenario(
        scenario,
        impl=args.impl,
        python_repo=args.python_repo,
        rust_bin=args.rust_bin,
        keep_sandbox=args.keep_sandbox,
    )

    label = f"{scenario['id']} [{args.impl}]"
    for step in result.steps:
        print(f"{label} step {step.index} {json.dumps(step.argv)}: {step.status}")
    if result.ok:
        print(f"{label}: PASS")
        return 0

    print(f"{label}: FAIL")
    for failure in result.failures:
        print(f"  - {failure}")
    if result.sandbox_root:
        print(f"  sandbox kept at: {result.sandbox_root}")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
