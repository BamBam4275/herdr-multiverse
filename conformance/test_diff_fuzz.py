"""Unit coverage for the differential fuzz harness; no oracle required."""

from __future__ import annotations

import random
import tempfile
import json
import shutil
from pathlib import Path
import unittest

import diff_fuzz
from normalize import PathMap
from sandbox import make_sandbox
from diff_fuzz import (
    Action,
    SequenceRun,
    StepOutcome,
    compare,
    gen_sequence,
    is_dry_run_argv,
    is_notify_argv,
    is_notify_diff,
    normalize_dry_run,
    read_probes,
)
from role_translate import translate_config_toml, translate_role_argv


REQUIRED_PROBE_PREFIXES = (
    ("backlog", "list"),
    ("backlog", "list", "--open"),
    ("backlog", "list", "--done"),
    ("backlog", "list", "--all"),
    ("tasks", "--no-reconcile", "--read-only"),
    ("tasks", "--status", "done"),
    ("workers",),
    ("health",),
    ("board", "--no-reconcile", "--read-only", "--no-color"),
    ("review-policy",),
    ("accounts", "usage"),
    ("activity", "inbox", "--peek"),
    ("halt", "status"),
    ("dispatch", "--dry-run"),
)


class ReadProbeTests(unittest.TestCase):
    def test_required_read_only_commands_are_present(self):
        probes = read_probes([])
        argvs = [tuple(p.argv) for p in probes]
        for prefix in REQUIRED_PROBE_PREFIXES:
            self.assertTrue(
                any(argv[: len(prefix)] == prefix for argv in argvs),
                f"missing probe prefix {prefix} in {argvs}",
            )
        self.assertTrue(all(p.probe for p in probes))
        self.assertFalse(any(p.argv[:1] == ["backlog"] and p.argv[1:2] == ["show"] for p in probes))

    def test_show_covers_newest_and_oldest_item(self):
        probes = read_probes(["<item:0>", "<item:1>"])
        shows = [p.argv for p in probes if p.argv[:2] == ["backlog", "show"]]
        self.assertEqual(shows, [["backlog", "show", "<item:1>"], ["backlog", "show", "<item:0>"]])

    def test_herdr_touching_probes_use_the_persistent_fleet(self):
        probes = {tuple(p.argv[:1]): p for p in read_probes([])}
        for name in ("workers", "health", "board", "tasks"):
            matches = [p for p in read_probes([]) if p.argv[0] == name]
            self.assertTrue(matches, name)
            for probe in matches:
                self.assertFalse(hasattr(probe, "herdr_transcript"))

    def test_dry_run_covers_a_few_roles_and_translates_for_rust(self):
        roles = [
            p.argv[p.argv.index("--role") + 1]
            for p in read_probes([])
            if p.argv[:2] == ["dispatch", "--dry-run"]
        ]
        self.assertEqual(roles, ["explorer", "fixer", "reviewer"])
        translated = translate_role_argv(
            ["dispatch", "--dry-run", "--role", "fixer", ".", "fuzz dry-run probe"]
        )
        self.assertEqual(translated[translated.index("--role") + 1], "builder")


class SequenceTests(unittest.TestCase):
    def test_each_write_is_followed_by_the_probe_battery(self):
        seq = gen_sequence(random.Random(0), 5)
        self.assertTrue(seq)
        self.assertFalse(seq[0].probe)
        writes = [a for a in seq if not a.probe]
        self.assertEqual(len(writes), 5)
        i = 0
        known = []
        for write in writes:
            self.assertIs(seq[i], write)
            i += 1
            if write.capture_name:
                known.append(write.capture_name)
            expected = read_probes(known)
            got = seq[i : i + len(expected)]
            self.assertEqual([a.argv for a in got], [a.argv for a in expected])
            i += len(expected)
        self.assertEqual(i, len(seq))

    def test_live_commands_are_generated(self):
        writes = [a.argv for seed in range(150)
                  for a in gen_sequence(random.Random(seed), 10) if not a.probe]
        for prefix in (["dispatch", "--yes"], ["sweep"], ["stop"], ["start"],
                       ["tasks", "--reclaimable"], ["tasks", "--close-stranded"],
                       ["worktrees", ".", "--prune"], ["council", "."]):
            self.assertTrue(any(a[:len(prefix)] == prefix for a in writes), prefix)

    def test_notify_remains_in_the_write_population(self):
        saw_notify = False
        for seed in range(80):
            writes = [a for a in gen_sequence(random.Random(seed), 8) if not a.probe]
            if any(is_notify_argv(a.argv) for a in writes):
                saw_notify = True
                break
        self.assertTrue(saw_notify)

    def test_config_toml_carries_chains_and_translates_for_rust(self):
        self.assertIn("explorer = [\"explorer-seat\"]", diff_fuzz.CONFIG_TOML)
        self.assertIn("[roles.fixer]", diff_fuzz.CONFIG_TOML)
        rust = translate_config_toml(diff_fuzz.CONFIG_TOML)
        self.assertIn("scout = [\"explorer-seat\"]", rust)
        self.assertIn("[roles.builder]", rust)
        self.assertNotIn("[roles.fixer]", rust)


class FleetSeedTests(unittest.TestCase):
    def test_fleet_seed_is_identical_after_paths_are_normalized(self):
        sandboxes = [make_sandbox(), make_sandbox()]
        try:
            states = []
            trees = []
            for sandbox in sandboxes:
                for name in ("sandbox", "sandbox2"):
                    sandbox.add_project(name, {})
                diff_fuzz.seed_fleet(sandbox, 40000)
                states.append(diff_fuzz.normalize_text((sandbox.root / "fleet.json").read_text(), sandbox.paths))
                trees.append(diff_fuzz._snapshot_tree(
                    {name: root / ".multiverse" for name, root in sandbox.projects.items()}, sandbox.paths))
                state = json.loads((sandbox.root / "fleet.json").read_text())
                self.assertEqual([p["agent_status"] for p in state["panes"] if p["pane_id"] != "w90:p1"],
                                 [a["agent_status"] for a in state["agents"]])
            self.assertEqual(states[0], states[1])
            self.assertEqual(trees[0], trees[1])
        finally:
            for sandbox in sandboxes:
                shutil.rmtree(sandbox.root)

    def test_task_ids_keep_distinct_identities(self):
        sandbox = make_sandbox()
        try:
            for name in ("sandbox", "sandbox2"):
                sandbox.add_project(name, {})
            diff_fuzz.seed_fleet(sandbox, 1)
            ids = {}
            diff_fuzz._register_task_ids(sandbox, ids)
            self.assertEqual(len(ids), 6)
            tree = diff_fuzz._snapshot_tree({"project": sandbox.projects["sandbox"] / ".multiverse"}, sandbox.paths)
            briefs = [p for p in tree if p.endswith("/brief.md")]
            self.assertEqual(len(briefs), 3)
            self.assertTrue(all("<TASK:" in p for p in briefs))
        finally:
            shutil.rmtree(sandbox.root)


class DryRunNormalizationTests(unittest.TestCase):
    """Card 20261103-b-42b7f8's four accepted `--dry-run` divergences."""

    def test_is_dry_run_argv(self):
        self.assertTrue(is_dry_run_argv(["dispatch", "--dry-run", "--role", "fixer"]))
        self.assertFalse(is_dry_run_argv(["dispatch", "--role", "fixer"]))
        self.assertFalse(is_dry_run_argv(["backlog", "add", "x"]))
        self.assertFalse(is_dry_run_argv([]))

    def test_short_code_follows_the_role_rename(self):
        text = "  agent       sandbox-expl-ex\n  agent       sandbox-dsgn-cx\n"
        normalized = normalize_dry_run(text)
        self.assertIn("sandbox-scou-ex", normalized)
        self.assertIn("sandbox-arti-cx", normalized)
        self.assertNotIn("expl", normalized)
        self.assertNotIn("dsgn", normalized)

    def test_role_file_name_follows_the_role_rename(self):
        text = "    role   prompts/role-explorer.md  (163 lines)\n"
        self.assertIn("prompts/role-scout.md", normalize_dry_run(text))

    def test_prompt_file_line_counts_are_normalized_regardless_of_file(self):
        text = (
            "    role   prompts/role-scout.md  (171 lines)\n"
            "    stage  prompts/column-explore.md  (96 lines)\n"
        )
        normalized = normalize_dry_run(text)
        self.assertNotIn("171 lines", normalized)
        self.assertNotIn("96 lines", normalized)
        self.assertEqual(normalized.count("(N lines)"), 2)

    def test_would_run_section_is_normalized_wholesale(self):
        text = (
            "CARD  <ID>\n"
            "──────────\n"
            "  role        Scout\n"
            "\n"
            "WOULD RUN\n"
            "──────────\n"
            "  # create the pane\n"
            "  fake-herdr pane split --current\n"
            "\n"
            "READY\n"
            "──────────\n"
            "  Every value is resolved.\n"
        )
        normalized = normalize_dry_run(text)
        self.assertNotIn("pane split", normalized)
        self.assertIn("role        Scout", normalized)
        self.assertIn("READY", normalized)
        self.assertIn("Every value is resolved.", normalized)

    def test_prompt_section_is_normalized_wholesale(self):
        # The wrapped form a long sandbox path takes mid-string — see
        # conformance/README.md "A normalization gap worth knowing about":
        # `normalize_text`'s literal substring replace cannot repair a path
        # split by the CLI's own word-wrap, so leftover tmp-dir text here
        # would never match across any two runs, same implementation
        # included.
        text = (
            "PROMPT  (what the agent would receive)\n"
            "──────────\n"
            "  Read `/tmp/mv-conformance-aaaa/projects/sandbox/.multiverse/ta\n"
            "  sks/<ID>/brief.md` and execute it.\n"
            "\n"
            "WOULD CREATE  (paths only — nothing was written)\n"
            "──────────\n"
            "  <PROJECT:sandbox>/.multiverse/tasks/<ID>/brief.md\n"
        )
        normalized = normalize_dry_run(text)
        self.assertNotIn("mv-conformance-aaaa", normalized)
        self.assertIn("WOULD CREATE", normalized)

    def test_compare_ignores_only_the_four_accepted_differences(self):
        py_stdout = (
            "CARD  <ID>\n"
            "──────────\n"
            "  agent       sandbox-expl-ex\n"
            "\n"
            "WOULD RUN\n"
            "──────────\n"
            "  python-only pane command\n"
            "\n"
            "READY\n"
            "──────────\n"
            "  Every value is resolved.\n"
        )
        rs_stdout = (
            "CARD  <ID>\n"
            "──────────\n"
            "  agent       sandbox-scou-ex\n"
            "\n"
            "WOULD RUN\n"
            "──────────\n"
            "  rust-only pane command\n"
            "\n"
            "READY\n"
            "──────────\n"
            "  Every value is resolved.\n"
        )
        py_run = SequenceRun(outcomes=[
            StepOutcome(
                ["dispatch", "--dry-run", "--role", "explorer"],
                0, py_stdout, "", probe=True,
            ),
        ])
        rs_run = SequenceRun(outcomes=[
            StepOutcome(
                ["dispatch", "--dry-run", "--role", "explorer"],
                0, rs_stdout, "", probe=True,
            ),
        ])
        self.assertEqual(compare(py_run, rs_run), [])

    def test_compare_still_fails_a_dry_run_difference_outside_the_four(self):
        py_stdout = "CARD  <ID>\n──────────\n  role        Explorer\n"
        rs_stdout = "CARD  <ID>\n──────────\n  role        Scout (unexpected)\n"
        py_run = SequenceRun(outcomes=[
            StepOutcome(
                ["dispatch", "--dry-run", "--role", "explorer"],
                0, py_stdout, "", probe=True,
            ),
        ])
        rs_run = SequenceRun(outcomes=[
            StepOutcome(
                ["dispatch", "--dry-run", "--role", "explorer"],
                0, rs_stdout, "", probe=True,
            ),
        ])
        diffs = compare(py_run, rs_run)
        self.assertEqual(len(diffs), 1)
        self.assertIn("stdout mismatch", diffs[0])


class PromptTreeComparisonTests(unittest.TestCase):
    brief = (
        "# Task <TASK:6>\nRole: Scout\n"
        "Result path: .multiverse/tasks/<TASK:6>/result.md\n"
        "Status path: .multiverse/tasks/<TASK:6>/status\n\n"
        "## Goal\nInspect the project.\n\n"
        "## Write scope\ndeclared:\n- src/a.rs\n- docs/*.md\n\n"
        "## Definition of done\nReport findings.\n\n"
        "## Role\nOld prompt prose.\n"
    )
    seed = "# Nexus\n<!-- multiverse:seed-digest 0123456789abcdef -->\nOld prose.\n"
    brief_path = "sandbox/.multiverse/tasks/<TASK:6>/brief.md"

    def compare_file(self, path, python, rust):
        return compare(SequenceRun(tree={path: python}), SequenceRun(tree={path: rust}))

    def test_seed_prose_and_digest_values_are_allowed(self):
        changed = self.seed.replace("Old prose.", "New standing directives.")
        changed = changed.replace("0123456789abcdef", "fedcba9876543210")
        for name in ("nexus", "orchestrator"):
            for prefix in ("sandbox/.multiverse/", "sandbox/.multiverse/worktrees/lane/.multiverse/"):
                with self.subTest(prefix=prefix, name=name):
                    self.assertEqual(self.compare_file(prefix + name + ".md", self.seed, changed), [])

    def test_seed_missing_or_malformed_marker_still_diverges(self):
        path = "sandbox/.multiverse/nexus.md"
        for changed in ("New prose.", self.seed.replace(" -->", ""),
                        self.seed.replace("0123456789abcdef", "invalid")):
            with self.subTest(changed=changed):
                self.assertTrue(self.compare_file(path, self.seed, changed))
                self.assertTrue(self.compare_file(path, changed, self.seed))

    def test_seed_comparison_tracks_marker_presence_only(self):
        self.assertEqual(self.compare_file("sandbox/.multiverse/nexus.md",
                                           "Old unmarked prose.", "New unmarked prose."), [])

    def test_prompt_file_existence_stays_strict(self):
        for path, content in ((self.brief_path, self.brief),
                              ("sandbox/.multiverse/nexus.md", self.seed),
                              ("sandbox/.multiverse/orchestrator.md", self.seed)):
            with self.subTest(path=path):
                run = SequenceRun(tree={path: content})
                self.assertTrue(compare(run, SequenceRun()))
                self.assertTrue(compare(SequenceRun(), run))

    def test_brief_prose_changes_are_allowed(self):
        changed = self.brief.replace("Inspect the project.", "Rewritten goal prose.")
        changed = changed.replace("Old prompt prose.", "New handoff-card instructions.\n"
                                  "Result path: ignored-in-role-prose\n- ignored-path")
        self.assertEqual(self.compare_file(self.brief_path, self.brief, changed), [])
        readonly = self.brief.replace("declared:\n- src/a.rs\n- docs/*.md", "none")
        self.assertEqual(self.compare_file(self.brief_path, readonly,
                                          readonly.replace("Old prompt prose.", "New prose.")), [])

    def test_changed_structured_facts_still_diverge(self):
        for old, new in (("# Task <TASK:6>", "# Task <TASK:7>"),
                         ("src/a.rs", "src/b.rs"),
                         ("- docs/*.md\n", ""),
                         ("declared:\n- src/a.rs\n- docs/*.md", "none"),
                         ("result.md", "other-result.md"),
                         ("/status\n", "/other-status\n")):
            with self.subTest(field=old):
                diffs = self.compare_file(self.brief_path, self.brief, self.brief.replace(old, new))
                self.assertEqual(len(diffs), 1)
                self.assertIn("tree", diffs[0])

    def test_incomplete_briefs_fall_back_to_strict_comparison(self):
        for old in ("# Task <TASK:6>",
                    "Result path: .multiverse/tasks/<TASK:6>/result.md",
                    "Status path: .multiverse/tasks/<TASK:6>/status",
                    "## Write scope"):
            with self.subTest(field=old):
                malformed = self.brief.replace(old, "missing")
                self.assertIsNone(diff_fuzz._brief_facts(malformed))
                self.assertTrue(self.compare_file(self.brief_path, malformed,
                                                  malformed + "different prose"))

    def test_neighboring_files_remain_strict(self):
        for path in ("sandbox/nexus.md", "sandbox/.multiverse/nexus.md.bak",
                     "sandbox/.multiverse/subdir/nexus.md",
                     "sandbox/.multiverse/brief.md",
                     "sandbox/.multiverse/tasks/<TASK:6>/nested/brief.md",
                     "sandbox/.multiverse/tasks/<TASK:6>/result.md",
                     "sandbox/.multiverse/tasks/<TASK:6>/status",
                     "sandbox/.multiverse/activity.jsonl", "state/config.toml"):
            with self.subTest(path=path):
                for content in (self.brief, self.seed):
                    self.assertTrue(self.compare_file(path, content, content + "changed prose\n"))

    def test_prompt_exceptions_do_not_apply_to_stdout(self):
        py_run = SequenceRun(outcomes=[StepOutcome(["start"], 0, self.seed, "")])
        rs_run = SequenceRun(outcomes=[StepOutcome(["start"], 0, self.seed + "changed", "")])
        self.assertTrue(any("stdout mismatch" in d for d in compare(py_run, rs_run)))


class CompareTests(unittest.TestCase):
    def test_ordered_herdr_calls_are_compared_even_on_exit_mismatch(self):
        for exit_code in (0, 1):
            py_run = SequenceRun(outcomes=[StepOutcome(
                ["sweep"], 0, "", "", herdr_calls=[["agent", "list"], ["pane", "close", "w1:p1"]])])
            rs_run = SequenceRun(outcomes=[StepOutcome(
                ["sweep"], exit_code, "", "", herdr_calls=[["pane", "close", "w1:p1"], ["agent", "list"]])])
            self.assertTrue(any("herdr calls mismatch" in d for d in compare(py_run, rs_run)))

    def test_matching_harness_errors_fail_comparison(self):
        run = SequenceRun(outcomes=[StepOutcome(["start"], None, "", "", error="timeout")])
        self.assertTrue(compare(run, run))

    def test_snapshot_includes_task_artifacts_and_activity(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            files = {"tasks/20250101-fixer-aaaaaa/status": "done",
                     "tasks/20250101-fixer-aaaaaa/result.md": "evidence",
                     "activity.jsonl": "message", "routing.log.lock": "", "routing.log.index": "cache"}
            for name, content in files.items():
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            tree = diff_fuzz._snapshot_tree({"project": root}, PathMap())
            self.assertEqual(set(tree), {"project/" + name.replace("20250101-fixer-aaaaaa", "<ID>")
                                        for name in files if not name.endswith((".lock", ".index"))})

    def test_snapshot_distinguishes_binary_artifacts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / "result.bin"
            path.write_bytes(b"\xffone")
            first = diff_fuzz._snapshot_tree({"project": root}, PathMap())
            path.write_bytes(b"\xfftwo")
            self.assertNotEqual(first, diff_fuzz._snapshot_tree({"project": root}, PathMap()))

    def test_probe_stdout_mismatch_is_labelled(self):
        py_run = SequenceRun(outcomes=[
            StepOutcome(["backlog", "add", "x"], 0, "added", "", probe=False),
            StepOutcome(["health"], 0, "", "", probe=True),
        ])
        rs_run = SequenceRun(outcomes=[
            StepOutcome(["backlog", "add", "x"], 0, "added", "", probe=False),
            StepOutcome(["health"], 0, "RATE w1:p1", "", probe=True),
        ])
        diffs = compare(py_run, rs_run)
        self.assertEqual(len(diffs), 1)
        self.assertIn("probe", diffs[0])
        self.assertIn("health", diffs[0])

    def test_notify_tree_diffs_count_as_notify_only(self):
        self.assertTrue(
            is_notify_diff("tree 'runtime/notify-outbox.jsonl' mismatch")
        )
        self.assertTrue(
            is_notify_diff("tree 'runtime/notify-state.json' mismatch")
        )
        self.assertTrue(
            is_notify_diff("step 4 ['notify', '--project', 'sandbox']: stdout mismatch")
        )
        self.assertFalse(is_notify_diff("step 9 probe ['board']: stdout mismatch"))

    def test_outcome_count_mismatch_is_reported(self):
        py_run = SequenceRun(outcomes=[
            StepOutcome(["halt", "status"], 0, "ok", "", probe=True),
        ])
        rs_run = SequenceRun()
        diffs = compare(py_run, rs_run)
        self.assertTrue(any("outcome count mismatch" in d for d in diffs))


class TableNormalizationTests(unittest.TestCase):
    def render(self, role, token, command, *, extra_space=False):
        task = f"20261106-{role}-abcdef"
        paths = PathMap()
        paths.add(task, token)
        # The id and role occupy fixed-width cells, as in tasks/workers
        # and the board's boxed id line. Keep a second unrelated separator.
        row = f"    {task:27}{role:11}seat  running\n"
        if extra_space:
            row = row.replace("seat  running", "seat   running")
        return diff_fuzz.normalize_stdout(row, paths, [command])

    def test_role_rename_padding_preserves_fixed_width_cells(self):
        for command in ("tasks", "workers", "board"):
            for old, new in diff_fuzz.ROLE_RENAME.items():
                if old == "orchestrator":  # this id overflows the table cell
                    continue
                with self.subTest(command=command, role=old):
                    self.assertEqual(self.render(old, "<TASK:6>", command),
                                     self.render(new, "<TASK:6>", command))

    def test_task_identity_and_unrelated_spacing_remain_strict(self):
        baseline = self.render("explorer", "<TASK:6>", "tasks")
        self.assertNotEqual(baseline, self.render("scout", "<TASK:7>", "tasks"))
        self.assertNotEqual(baseline, self.render("scout", "<TASK:6>", "tasks", extra_space=True))
        self.assertNotEqual(baseline, self.render("scout", "<TASK:6>", "dispatch"))

    def test_prose_and_non_task_rows_are_unchanged(self):
        text = "Role: Explorer  seat\n    explorer  seat\n"
        self.assertEqual(diff_fuzz.normalize_stdout(text, PathMap(), ["tasks"]), text)

    def test_agent_short_code_padding_is_preserved_without_a_task_token(self):
        # A board card's agent-name line (`sandbox-fix-ex`) carries no task
        # token at all -- `row()`'s gate only fired for task ids before this
        # fix, so the embedded short code (`-fix-` -> `-buil-`) reached
        # compare()'s blind rename still at its old width and left that
        # code's old padding behind, same as an unreplaced task id would.
        # Both sides render the same fixed cell width; only the short
        # code's own length differs, exactly as Python's old code and
        # Rust's current one would for an identical card.
        inner = 34
        for old, new in diff_fuzz._OLD_SHORT_CODE_RENAME.items():
            with self.subTest(code=old):
                old_value = f"sandbox-{old}-ex"
                new_value = f"sandbox-{new}-ex"
                old_line = f"│ {old_value}{' ' * (inner - len(old_value))}│\n"
                new_line = f"│ {new_value}{' ' * (inner - len(new_value))}│\n"
                got = diff_fuzz.normalize_stdout(old_line, PathMap(), ["board"])
                want = diff_fuzz.normalize_stdout(new_line, PathMap(), ["board"])
                self.assertEqual(got, want)
                self.assertEqual(got.index("│", 1), new_line.index("│", 1))

    def test_agent_short_code_padding_survives_a_blank_neighboring_card(self):
        # A two-up IN PROGRESS row pads the shorter card's line with blank
        # content, e.g. "| |                | | sandbox-fix-ex  |  |" when
        # the left card has fewer rows than the right one. `boxed`'s regex
        # used to let `\S+` capture the blank card's own closing `|` as
        # its single match, so `re.search` never reached the real content
        # on the right -- the exact shape `conformance/diff_fuzz.py`'s own
        # fuzz seed 50016 hit.
        for old, new in diff_fuzz._OLD_SHORT_CODE_RENAME.items():
            with self.subTest(code=old):
                old_value = f"sandbox-{old}-ex"
                new_value = f"sandbox-{new}-ex"
                inner = 34
                blank_left = " " * inner
                old_line = f"│ │ {blank_left} │ │ {old_value}{' ' * (inner - len(old_value))} │  │\n"
                new_line = f"│ │ {blank_left} │ │ {new_value}{' ' * (inner - len(new_value))} │  │\n"
                got = diff_fuzz.normalize_stdout(old_line, PathMap(), ["board"])
                want = diff_fuzz.normalize_stdout(new_line, PathMap(), ["board"])
                self.assertEqual(got, want)

    def test_stale_bookkeeping_is_prose_even_with_a_task_id(self):
        for role in ("fixer", "builder", "explorer", "scout"):
            task = f"20261106-{role}-abcdef"
            paths = PathMap()
            paths.add(task, "<TASK:6>")
            text = f"    {task} ({role}, agent='seat', pane='w1:p1')\n"
            with self.subTest(role=role):
                self.assertEqual(diff_fuzz.normalize_stdout(text, paths, ["tasks", "--reclaimable"]),
                                 text.replace(task, "<TASK:6>"))
                prose = f"Task {task}  has role {role}  pending\n"
                self.assertEqual(diff_fuzz.normalize_stdout(prose, paths, ["tasks"]),
                                 prose.replace(task, "<TASK:6>"))

    def test_flagged_tasks_and_workers_project_cells_are_tables(self):
        for command, prefix in (("tasks", "  * "), ("workers", f"{'sandbox':17}")):
            rows = []
            for role in ("explorer", "scout"):
                task = f"20261106-{role}-abcdef"
                paths = PathMap()
                paths.add(task, "<TASK:6>")
                text = f"{prefix}{task:27}{role:11}{'seat':17}{'running':11}0m\n"
                rows.append(diff_fuzz.normalize_stdout(text, paths, [command]))
            self.assertEqual(*rows)

    def test_boxed_board_ids_keep_the_border_in_the_same_column(self):
        rendered = []
        for role in ("explorer", "scout"):
            task = f"20261106-{role}-abcdef"
            paths = PathMap()
            paths.add(task, "<TASK:6>")
            rendered.append(diff_fuzz.normalize_stdout(f"│ {task:30} │\n", paths, ["board"]))
        self.assertEqual(*rendered)
        self.assertEqual(rendered[0].index("│", 1), 33)

    def test_compare_detects_an_actual_changed_table_field(self):
        py = self.render("explorer", "<TASK:6>", "tasks")
        rs = self.render("scout", "<TASK:6>", "tasks")
        def run(text):
            return SequenceRun(outcomes=[StepOutcome(["tasks"], 0, text, "")])
        self.assertEqual(compare(run(py), run(rs)), [])
        self.assertTrue(compare(run(py), run(rs.replace("running", "stalled"))))


class ElapsedNormalizationTests(unittest.TestCase):
    def render(self, text, command):
        return diff_fuzz.normalize_stdout(text, PathMap(), [command])

    def test_all_elapsed_forms_and_unit_boundaries(self):
        for command in ("board", "workers", "tasks"):
            for elapsed in ("0s", "59s", "1m", "59m", "1h", "60033h", "2h10m",
                            "1 sec", "2 min", "3 hr"):
                with self.subTest(command=command, elapsed=elapsed):
                    self.assertEqual(self.render(f"seat {elapsed}, running", command),
                                     "seat <ELAPSED>, running")

    def test_elapsed_padding_keeps_board_borders_aligned(self):
        for elapsed in ("9s", "10s", "1m", "2h10m"):
            self.assertEqual(self.render(f"│ │ seat {elapsed:20}│", "board"),
                             f"│ │ seat {'<ELAPSED>':20}│")

    def test_existing_clock_and_historical_card_ages_still_normalize(self):
        for clock in ("03:16 · 0m", "03:17 · 1m"):
            self.assertEqual(self.render(clock, "board"), "<TIME> · <ELAPSED>")
        self.assertEqual(self.render("Dec 31 2019 · 6y", "board"),
                         "Dec 31 2019 · <ELAPSED>")
        self.assertEqual(self.render("┌" + "─" * 20 + " 03:16 · 9m ┐", "board"),
                         self.render("┌" + "─" * 19 + " 03:17 · 10m ┐", "board"))

    def test_resets_normalizes_whole_duration_and_keeps_reason(self):
        for elapsed in ("3m", "2h10m", "6d23h", "3 hr", "2 min"):
            with self.subTest(elapsed=elapsed):
                self.assertEqual(self.render(f"(manual) resets in {elapsed}, fenced", "board"),
                                 "(manual) resets in <ELAPSED>, fenced")
        self.assertEqual(self.render("resets in unknown", "board"), "resets in unknown")

    def test_other_text_numbers_identifiers_and_whitespace_remain_strict(self):
        text = "seat-3m /tmp/3m v1.3m 3ms 3m_extra 3h10mx 4 held / 20 cap  80%"
        self.assertEqual(self.render(text, "board"), text)
        baseline = self.render("seat 3m, running 2", "board")
        for changed in ("seat 4m, stalled 2", "other 4m, running 2",
                        "seat 4m, running 3", "seat  4m, running 2"):
            self.assertNotEqual(baseline, self.render(changed, "board"))
        self.assertNotEqual(self.render("dispatched 3 hr ago", "tasks"),
                            self.render("dispatched 4 hr  ago", "tasks"))

    def test_duration_prose_in_writes_and_durable_state_is_unchanged(self):
        text = "wait 3m then retry in 1 hr"
        self.assertEqual(self.render(text, "dispatch"), text)
        self.assertEqual(diff_fuzz.normalize_text(text, PathMap()), text)

    def test_timing_only_output_compares_equal_but_status_change_diverges(self):
        for command in ("board", "workers", "tasks"):
            def run(text):
                return SequenceRun(outcomes=[StepOutcome(
                    [command], 0, self.render(text, command), "")])
            py = run("seat 59s, running")
            self.assertEqual(compare(py, run("seat 1m, running")), [])
            self.assertTrue(compare(py, run("seat 1m, stalled")))


class CouncilCallTests(unittest.TestCase):
    def seat(self, name, pane):
        return [
            ["pane", "rename", pane, name],
            ["pane", "run", pane, "cd <PROJECT:sandbox> && unset TOKEN"],
            ["agent", "start", name, "--kind", "codex", "--pane", pane],
            ["pane", "read", pane, "--source", "visible"],
            ["agent", "prompt", name, "Read <TASK:6>/brief.md"],
            ["agent", "get", name],
            ["pane", "close", pane],
        ]

    def diffs(self, py, rs, command="council"):
        def run(calls):
            return SequenceRun(outcomes=[StepOutcome(
                [command, ".", "--yes"], 0, "", "", herdr_calls=calls)])
        return compare(run(py), run(rs))

    def test_seat_interleaving_preserves_each_seats_order(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        roster = [["workspace", "list"], ["pane", "list"]]
        interleaved = [call for pair in zip(b, a) for call in pair]
        self.assertEqual(self.diffs(roster + a + b, roster + interleaved), [])
        for seed in range(20):
            rng = random.Random(seed)
            pending = [list(a), list(b)]
            shuffled = []
            while any(pending):
                group = rng.choice([calls for calls in pending if calls])
                shuffled.append(group.pop(0))
            self.assertEqual(self.diffs(roster + a + b, roster + shuffled), [])
        for command in ("dispatch", "start", "tasks", "workers", "board"):
            with self.subTest(command=command):
                self.assertTrue(self.diffs(roster + a + b, roster + interleaved, command))

    def test_missing_extra_changed_or_reordered_seat_calls_diverge(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        for changed in (a[:4] + a[5:], a + [a[-1]],
                        a[:4] + [a[4][:-1] + ["different prompt"]] + a[5:],
                        a[:4] + [a[6], a[4], a[5]]):
            with self.subTest(changed=changed):
                self.assertTrue(self.diffs(a + b, b + changed))
        self.assertTrue(self.diffs(a + b, a))

    def test_non_seat_calls_keep_order_and_multiplicity(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        roster = [["workspace", "list"], ["pane", "list"]]
        for changed in (list(reversed(roster)), roster[:1], roster + [roster[-1]]):
            self.assertTrue(self.diffs(roster + a + b, changed + b + a))

    def test_non_seat_calls_may_interleave_without_changing_their_order(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        roster = [["workspace", "list"], ["pane", "list"]]
        py = a[:4] + [a[3]] + a[4:] + b
        rs = b + a[:4] + [roster[0], a[3], roster[1]] + a[4:]
        self.assertEqual(self.diffs(roster + py, rs), [])

    def test_agent_rename_also_binds_a_seats_pane(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        for calls in (a, b):
            calls[2] = ["agent", "rename", calls[2][-1], calls[2][2]]
        self.assertEqual(self.diffs(a + b, b + a), [])
        changed = [call.copy() for call in a]
        changed[1][2] = "w1:p999"
        self.assertTrue(self.diffs(a + b, b + changed))

    def test_interleaved_prompt_polls_keep_existing_allowances(self):
        a = self.seat("sandbox-coun-cx", "w1:p100")
        b = self.seat("sandbox-coun-cl", "w1:p101")
        py_a = a[:3] + [["agent", "read", a[2][2], "--source", "visible"]] + a[4:]
        py_b = b[:3] + [["agent", "read", b[2][2], "--source", "visible"]] + b[4:]
        interleaved = b[:3] + a[:3] + [b[3], a[3], ["agent", "list"],
                                              b[3], a[3], b[4], a[4]] + b[5:] + a[5:]
        self.assertEqual(self.diffs(py_a + py_b, interleaved), [])


class HerdrAllowlistTests(unittest.TestCase):
    rename = ["agent", "rename", "w1:p1", "sandbox-scou-ex"]
    prompt = ["agent", "prompt", "sandbox-scou-ex", "read the brief"]
    py_poll = [["agent", "read", "sandbox-scou-ex", "--source", "visible"],
               ["agent", "get", "sandbox-scou-ex"]]
    rs_poll = [["pane", "read", "w1:p1", "--source", "visible"],
               ["agent", "list"],
               ["pane", "read", "w1:p1", "--source", "visible"]]

    def diffs(self, py, rs, command="dispatch"):
        def run(calls):
            return SequenceRun(outcomes=[StepOutcome([command], 0, "", "", herdr_calls=calls)])
        return compare(run(py), run(rs))

    def test_prompt_polling_equivalence_for_each_launch_command(self):
        for command in ("dispatch", "council", "start"):
            with self.subTest(command=command):
                self.assertEqual(self.diffs(
                    [self.rename, *self.py_poll, self.prompt, *self.py_poll],
                    [self.rename, *self.rs_poll, self.prompt, *self.rs_poll], command), [])

    def test_agent_start_also_establishes_the_exact_poll_target(self):
        start = ["agent", "start", "sandbox-scou-ex", "--kind", "codex", "--pane", "w1:p1"]
        self.assertEqual(self.diffs([start, *self.py_poll], [start, *self.rs_poll]), [])

    def test_start_can_repoint_an_agent_launched_in_an_earlier_step(self):
        def run(poll):
            return SequenceRun(outcomes=[
                StepOutcome(["start"], 0, "", "", herdr_calls=[self.rename]),
                StepOutcome(["start", "--reseed"], 0, "", "", herdr_calls=[
                    self.py_poll[0], *poll, self.prompt, *poll]),
            ])
        self.assertEqual(compare(run(self.py_poll), run(self.rs_poll)), [])
        wrong_pane = [["pane", "read", "w1:p2", "--source", "visible"]]
        self.assertTrue(compare(run(self.py_poll), run(wrong_pane)))

    def test_start_accepts_exactly_the_extra_under_lock_roster(self):
        initial = [["workspace", "list"], ["pane", "list"], ["agent", "list"]]
        locked = [["workspace", "list"], ["pane", "list"]]
        py = initial + locked
        rs = py + [["agent", "list"]]
        self.assertEqual(self.diffs(py, rs, "start"), [])
        for command in ("dispatch", "council", "sweep", "workers"):
            self.assertTrue(self.diffs(py, rs, command))
        self.assertTrue(self.diffs(rs, py, "start"))
        self.assertEqual(self.diffs(py, rs + [["agent", "list"]], "start"), [])
        self.assertEqual(self.diffs(initial, initial + [["agent", "list"]], "start"), [])

    def test_identical_consecutive_read_only_polls_collapse_on_both_sides(self):
        polls = [["agent", "list"], ["agent", "get", "seat"],
                 ["pane", "read", "w1:p1", "--source", "visible"],
                 ["agent", "read", "seat", "--source", "visible"]]
        for command in ("dispatch", "council", "start", "sweep", "tasks", "workers", "board"):
            for poll in polls:
                with self.subTest(command=command, poll=poll):
                    self.assertEqual(self.diffs([poll] * 38, [poll] * 380, command), [])
                    self.assertEqual(self.diffs([poll] * 380, [poll], command), [])
                    self.assertTrue(self.diffs([], [poll] * 380, command))

    def test_poll_runs_stop_at_different_calls_and_step_boundaries(self):
        poll = ["agent", "list"]
        get = ["agent", "get", "seat"]
        close = ["pane", "close", "w1:p1"]
        self.assertEqual(self.diffs([poll, poll, close, poll],
                                   [poll, close, poll, poll], "tasks"), [])
        for changed in ([poll, close], [poll, poll, close], [get, poll, get]):
            self.assertTrue(self.diffs([poll, get, poll], changed, "tasks"))
        def run(first, second):
            return SequenceRun(outcomes=[StepOutcome(["tasks"], 0, "", "", herdr_calls=calls)
                                         for calls in (first, second)])
        self.assertTrue(compare(run([poll], [poll]), run([poll, poll], [])))

    def test_non_poll_reads_and_changed_targets_sources_or_flags_remain_strict(self):
        for call in (["workspace", "list"], ["pane", "list"],
                     ["agent", "get", "seat", "--extra"],
                     ["agent", "list", "--extra"],
                     ["pane", "read", "w1:p1"],
                     ["agent", "read", "seat", "--source", "recent"],
                     ["pane", "read", "w1:p1", "--source", "visible", "--lines", "10"]):
            with self.subTest(call=call):
                self.assertTrue(self.diffs([call], [call, call], "tasks"))
        self.assertTrue(self.diffs([["agent", "get", "seat"]] * 2,
                                   [["agent", "get", "other"]] * 3, "tasks"))

    def test_repeated_or_reordered_writes_always_diverge(self):
        writes = [["pane", "split", "--current"], ["pane", "close", "w1:p1"],
                  ["pane", "run", "w1:p1", "echo hi"],
                  ["pane", "rename", "w1:p1", "seat"],
                  ["agent", "rename", "w1:p1", "seat"],
                  ["agent", "start", "seat", "--kind", "codex", "--pane", "w1:p1"],
                  ["agent", "prompt", "seat", "wait 3m"]]
        poll = ["agent", "list"]
        for command in ("dispatch", "council", "start", "tasks"):
            for write in writes:
                with self.subTest(command=command, write=write):
                    self.assertTrue(self.diffs([poll, write, poll],
                                               [poll, write, write, poll], command))
            self.assertTrue(self.diffs(writes, list(reversed(writes)), command))

    def test_cd_quoting_only_changes_the_initial_directory_word(self):
        for path in ("/tmp/project", "<PROJECT:sandbox>", "/tmp/a'b", "/tmp/a b",
                     "<PROJECT:sandbox>/.multiverse/worktrees/<TASK:6>"):
            with self.subTest(path=path):
                quoted = "'" + path.replace("'", "'\"'\"'") + "'"
                # The oracle quotes safe real paths before the harness
                # replaces their components with angle-bracket tokens.
                py_path = path if path.startswith("<PROJECT:") else diff_fuzz.shlex.quote(path)
                py_script = "cd " + py_path + " && unset TOKEN"
                rs_script = "cd " + quoted + " && unset TOKEN"
                py = [["pane", "run", "w1:p1", py_script]]
                rs = [["pane", "run", "w1:p1", rs_script]]
                self.assertEqual(self.diffs(py, rs), [])
                self.assertTrue(self.diffs(py, [["pane", "run", "w1:p1", rs_script + " "]]))
                self.assertTrue(self.diffs(py, [["pane", "run", "w1:p2", rs_script]]))
                self.assertTrue(self.diffs(py, [["pane", "run", "w1:p1", rs_script + "; echo bad"]]))
        self.assertTrue(self.diffs([["pane", "run", "w1:p1", "cd $HOME"]],
                                   [["pane", "run", "w1:p1", "cd '$HOME'"]]))

    def test_other_poll_targets_sources_flags_and_order_remain_strict(self):
        py = [self.rename, *self.py_poll, self.prompt]
        for changed in (
            [["pane", "read", "w1:p2", "--source", "visible"]],
            [["pane", "read", "w1:p1", "--source", "recent"]],
            [["pane", "read", "w1:p1", "--source", "visible", "--lines", "10"]],
            [["agent", "get", "other-agent"]],
            [["pane", "close", "w1:p1"], *self.rs_poll],
            [],
        ):
            with self.subTest(changed=changed):
                self.assertTrue(self.diffs(py, [self.rename, *changed, self.prompt]))
        self.assertTrue(self.diffs(py, [self.rename, self.prompt, *self.rs_poll]))
        for command in ("sweep", "tasks", "workers", "board"):
            self.assertTrue(self.diffs([self.rename, *self.py_poll],
                                       [self.rename, *self.rs_poll], command))

    def test_polling_does_not_escape_the_launch_episode(self):
        close = ["pane", "close", "w1:p1"]
        self.assertTrue(self.diffs([self.rename, close, *self.py_poll],
                                   [self.rename, close, *self.rs_poll]))
        self.assertTrue(self.diffs(self.py_poll, self.rs_poll))


if __name__ == "__main__":
    unittest.main()
