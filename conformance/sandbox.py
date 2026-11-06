"""Builds the isolated filesystem a scenario runs inside.

Every path a scenario touches — HOME, the fleet config/state dir, each
project's repo, any extra named directory (a fake `CODEX_HOME`, say) — lives
under one fresh `mkdtemp()` root per (scenario, implementation) run. Nothing
here ever reads or writes the real `~/.config/multiverse` or a real
project's `.multiverse`; that is the one invariant the whole harness exists
to protect (brief: "never write to the live fleet's ~/.config/multiverse or
any project's .multiverse").
"""

from __future__ import annotations

import os
import subprocess
import tempfile
from pathlib import Path

from normalize import PathMap


class Sandbox:
    """One scenario run's isolated tree, plus the token -> path map that
    `normalize.normalize_text` folds back out of captured output."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.home = root / "home"
        self.state_dir = self.home / ".config" / "multiverse"
        self.runtime_dir = root / "runtime"
        self.projects: dict[str, Path] = {}
        self.dirs: dict[str, Path] = {}
        self.paths = PathMap()

        self.home.mkdir(parents=True)
        self.state_dir.mkdir(parents=True)
        self.runtime_dir.mkdir(parents=True)
        self.paths.add(str(self.home), "<HOME>")
        self.paths.add(str(self.state_dir), "<STATE_DIR>")
        self.paths.add(str(self.runtime_dir), "<RUNTIME_DIR>")
        # Registered last (and so, by length, normalized last — see
        # `PathMap.ordered`): a catch-all for anything else under this run's
        # temp root that isn't a project/dir/home, e.g. `fake-herdr` itself
        # or the per-step herdr transcript/state files named in a "WOULD
        # RUN" argv dump.
        self.paths.add(str(self.root), "<SANDBOX_ROOT>")

    def add_project(self, name: str, spec: dict) -> Path:
        path = self.root / "projects" / name
        path.mkdir(parents=True)
        for relpath, content in (spec.get("files") or {}).items():
            target = path / relpath
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        for relpath in spec.get("fifos", []):
            target = path / relpath
            target.parent.mkdir(parents=True, exist_ok=True)
            os.mkfifo(target, 0o600)
        if spec.get("git"):
            _run_git(["init", "-q", "-b", "main"], cwd=path)
            _run_git(["config", "user.email", "scenario@example.invalid"], cwd=path)
            _run_git(["config", "user.name", "Conformance Scenario"], cwd=path)
            _run_git(["add", "-A"], cwd=path)
            _run_git(["commit", "-q", "-m", "initial"], cwd=path)
            for task_id, lane_spec in (spec.get("lanes") or {}).items():
                _add_lane_branch(path, task_id, lane_spec)
            for task_id in spec.get("orphan_branches", []):
                _run_git(["branch", f"multiverse/{task_id}", "HEAD"], cwd=path)
            worktrees = spec.get("worktrees", {})
            for task_id in worktrees:
                lane = path / ".multiverse" / "worktrees" / task_id
                lane.parent.mkdir(parents=True, exist_ok=True)
                _run_git(
                    ["worktree", "add", "-b", f"multiverse/{task_id}", str(lane), "HEAD"],
                    cwd=path,
                )
                lane_spec = worktrees[task_id] if isinstance(worktrees, dict) else {}
                for relpath, content in lane_spec.get("files", {}).items():
                    target = lane / relpath
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(content, encoding="utf-8")
            for task_id, archive_spec in spec.get("archived_lanes", {}).items():
                _run_git(["update-ref", f"refs/multiverse/archive/{task_id}", "HEAD"], cwd=path)
                if archive_spec.get("unrelated_root"):
                    root = _run_git_capture(
                        ["commit-tree", "HEAD^{tree}", "-m", "resurrected root"], cwd=path,
                    ).strip()
                    _run_git(["update-ref", f"refs/heads/multiverse/{task_id}", root], cwd=path)
        self.projects[name] = path
        self.paths.add(str(path), f"<PROJECT:{name}>")
        return path

    def add_dir(
        self,
        key: str,
        *,
        mode: int | None = None,
        files: dict | None = None,
    ) -> Path:
        path = self.root / "dirs" / key
        path.mkdir(parents=True)
        for relpath, content in (files or {}).items():
            target = path / relpath
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        if mode is not None:
            path.chmod(mode)
        self.dirs[key] = path
        self.paths.add(str(path), f"<DIR:{key}>")
        return path

    def resolve(self, text: str) -> str:
        """Substitute `{{HOME}}`, `{{STATE_DIR}}`, `{{PROJECT:name}}`,
        `{{DIR:key}}` tokens in scenario-authored text with this sandbox's
        concrete paths — the inverse of `normalize.normalize_text`."""
        text = text.replace("{{HOME}}", str(self.home))
        text = text.replace("{{STATE_DIR}}", str(self.state_dir))
        text = text.replace("{{RUNTIME_DIR}}", str(self.runtime_dir))
        for name, path in self.projects.items():
            text = text.replace(f"{{{{PROJECT:{name}}}}}", str(path))
        for key, path in self.dirs.items():
            text = text.replace(f"{{{{DIR:{key}}}}}", str(path))
        return text


def _run_git(args: list[str], *, cwd: Path) -> None:
    subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


def _run_git_capture(args: list[str], *, cwd: Path) -> str:
    result = subprocess.run(
        ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
    )
    return result.stdout


def _add_lane_branch(repo: Path, task_id: str, spec: dict) -> None:
    """A second branch for `review-policy --lane` (and anything else that
    reads `worktrees.lane_diff`) to diff against: a `multiverse/<task_id>`
    branch cut from the project's initial commit, plus the
    `refs/multiverse/base/<task_id>` ref that records the cut point, the
    same two things a real `Worktrees.create` dispatch writes
    (`worktrees.py`'s `branch_name`/`base_ref`/`_record_base`; mirrored by
    `mv_state::worktrees` and `review_policy.rs`'s own test helper,
    `repo_with_lane_branch`). `spec["files"]` is written and committed on
    the lane branch; `spec["merge"]` additionally `--no-ff` merges the lane
    into the project's original branch and deletes the lane branch
    afterward, simulating a lane `--prune` has already reclaimed — `
    lane_diff` must then recover the tip from the merge commit's second
    parent rather than the branch itself.
    """
    original_branch = _run_git_capture(["branch", "--show-current"], cwd=repo).strip()
    branch = f"multiverse/{task_id}"
    _run_git(["update-ref", f"refs/multiverse/base/{task_id}", "HEAD"], cwd=repo)
    _run_git(["checkout", "-q", "-b", branch], cwd=repo)
    for relpath, content in (spec.get("files") or {}).items():
        target = repo / relpath
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    _run_git(["add", "-A"], cwd=repo)
    _run_git(["commit", "-q", "-m", spec.get("message", "lane work")], cwd=repo)
    _run_git(["checkout", "-q", original_branch], cwd=repo)
    if spec.get("merge"):
        _run_git(
            ["merge", "--no-ff", "-m", f"Merge branch '{branch}'", branch],
            cwd=repo,
        )
        _run_git(["branch", "-D", branch], cwd=repo)


def make_sandbox(tmp_parent: Path | None = None) -> Sandbox:
    root = Path(tempfile.mkdtemp(prefix="mv-conformance-", dir=tmp_parent)).resolve()
    return Sandbox(root)
