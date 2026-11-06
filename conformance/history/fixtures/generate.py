#!/usr/bin/env python3
"""Regenerates this directory's synthetic .multiverse/ fixtures.

Every file under this directory (other than this script and the README) is
generated output, not hand-edited — in particular the `crc32=` trailers,
which must match the exact byte-prefix-up-to-the-trailing-tab rule
documented in `spec-on-disk-contract.md` and verified against real
on-disk routing.log data while building `runner/`. Re-run this after
changing a scenario below; do not hand-patch a generated file.

Stdlib only, matching this repo's existing conformance/ convention
(see conformance/README.md's "no third-party dependencies" note).

Each top-level directory this script creates is one independent scenario,
picked up by `runner/`'s `check_root` as either a project (has a nested
`.multiverse/`) or, for `fleet-state/`, the three fleet stores directly.
"""
import json
import os
import shutil
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))


def crc_line(fields):
    prefix = "\t".join(fields)
    crc = zlib.crc32(prefix.encode("utf-8")) & 0xFFFFFFFF
    return f"{prefix}\tcrc32={crc:08x}"


def write(path, content):
    full = os.path.join(HERE, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w", encoding="utf-8") as f:
        f.write(content)


def write_bytes(path, content):
    full = os.path.join(HERE, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "wb") as f:
        f.write(content)


def main():
    for name in os.listdir(HERE):
        if name in ("generate.py", "README.md"):
            continue
        shutil.rmtree(os.path.join(HERE, name), ignore_errors=True)

    # --- legacy-and-v1-envelope: both routing.log eras, both must parse
    # clean. The pre-rename fleet never had a `v=`/`seq=`/`crc32=`
    # envelope at all (spec-on-disk-contract.md's live census: 4,398 of
    # 9,241 lines in one project's log are this shape).
    write(
        "legacy-and-v1-envelope/multiverse-state/routing.log",
        "\n".join(
            [
                "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tproject=demo\trole=fixer\taccount=claude\tpool=claude:default\toutcome=chain-primary\treason=first entry",
                "2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1",
                crc_line(
                    [
                        "2026-01-01T00:10:00+00:00",
                        "v=1",
                        "seq=1",
                        "event=dispatch",
                        "task=20260101-scout-bbbbbb",
                        "project=demo",
                        "role=scout",
                        "account=claude",
                        "pool=claude:default",
                        "outcome=chain-primary",
                        "reason=first entry",
                    ]
                ),
                crc_line(
                    [
                        "2026-01-01T00:12:00+00:00",
                        "v=1",
                        "seq=2",
                        "event=result",
                        "task=20260101-scout-bbbbbb",
                        "status=done",
                        "state=done",
                        "revisions=0",
                        "pane=w1:p2",
                    ]
                ),
            ]
        )
        + "\n",
    )

    # --- old-role-names: a pre-rename task id (`20261031-fixer-ab12cd`,
    # the exact shape named in this lane's brief) dispatched and resolved
    # under the old `fixer` role name. Must parse clean; AGENTS.md
    # requires history readers to keep accepting this name.
    write(
        "old-role-names/multiverse-state/routing.log",
        "\n".join(
            [
                crc_line(
                    [
                        "2026-10-31T00:00:00+00:00",
                        "v=1",
                        "seq=1",
                        "event=dispatch",
                        "task=20261031-fixer-ab12cd",
                        "project=demo",
                        "role=fixer",
                        "account=claude",
                        "pool=claude:default",
                        "outcome=chain-primary",
                        "reason=first entry in Fixer chain for demo",
                    ]
                ),
                crc_line(
                    [
                        "2026-10-31T00:10:00+00:00",
                        "v=1",
                        "seq=2",
                        "event=result",
                        "task=20261031-fixer-ab12cd",
                        "status=done",
                        "state=done",
                        "revisions=0",
                        "pane=w2:p1",
                    ]
                ),
            ]
        )
        + "\n",
    )
    write("old-role-names/multiverse-state/tasks/20261031-fixer-ab12cd/brief.md", "# Task 20261031-fixer-ab12cd\nRole: Fixer\n")
    write("old-role-names/multiverse-state/tasks/20261031-fixer-ab12cd/result.md", "# Result — task 20261031-fixer-ab12cd\nStatus: done\n")
    write("old-role-names/multiverse-state/tasks/20261031-fixer-ab12cd/status", "done")

    # --- damaged-crc: one good record, one whose crc32 trailer does not
    # match its own bytes. The runner must report exactly the one damaged
    # line, by line number, and still process the good one.
    good = crc_line(
        [
            "2026-02-01T00:00:00+00:00",
            "v=1",
            "seq=1",
            "event=dispatch",
            "task=20260201-fixer-cccccc",
            "project=demo",
            "role=fixer",
            "account=claude",
            "pool=claude:default",
            "outcome=chain-primary",
            "reason=ok line",
        ]
    )
    bad_prefix = "\t".join(
        [
            "2026-02-01T00:05:00+00:00",
            "v=1",
            "seq=2",
            "event=result",
            "task=20260201-fixer-cccccc",
            "status=done",
            "state=done",
            "revisions=0",
            "pane=w3:p1",
        ]
    )
    write("damaged-crc/multiverse-state/routing.log", good + "\n" + bad_prefix + "\tcrc32=deadbeef\n")

    # --- backlog-asked: the audit's P1 correction
    # (audit-mail-backlog-board-disk.md) — a valid ask, one with an
    # out-of-range recommendation, one with non-JSON options, and (for
    # contrast) an ordinary open-state record with a free-text role.
    write(
        "backlog-asked/multiverse-state/routing.log",
        "\n".join(
            [
                crc_line(
                    [
                        "2026-03-01T00:00:00+00:00",
                        "v=1",
                        "seq=1",
                        "event=backlog",
                        "item=20260301-b-aaaaaa",
                        "state=asked",
                        "priority=P2",
                        'title="valid ask"',
                        "options=" + json.dumps(["opt one", "opt two"]),
                        "recommendation=1",
                        'why="ok"',
                        "by=demo",
                    ]
                ),
                crc_line(
                    [
                        "2026-03-01T00:05:00+00:00",
                        "v=1",
                        "seq=2",
                        "event=backlog",
                        "item=20260301-b-bbbbbb",
                        "state=asked",
                        "priority=P2",
                        'title="bad recommendation"',
                        "options=" + json.dumps(["opt one", "opt two"]),
                        "recommendation=9",
                        'why="ok"',
                        "by=demo",
                    ]
                ),
                crc_line(
                    [
                        "2026-03-01T00:10:00+00:00",
                        "v=1",
                        "seq=3",
                        "event=backlog",
                        "item=20260301-b-cccccc",
                        "state=asked",
                        "priority=P2",
                        'title="bad options"',
                        "options=not-json",
                        "recommendation=1",
                        'why="ok"',
                        "by=demo",
                    ]
                ),
                crc_line(
                    [
                        "2026-03-01T00:15:00+00:00",
                        "v=1",
                        "seq=4",
                        "event=backlog",
                        "item=20260301-b-dddddd",
                        "state=open",
                        "priority=P1",
                        'title="free text role"',
                        'why="ok"',
                        "role=builder",
                        "by=demo",
                    ]
                ),
            ]
        )
        + "\n",
    )

    # --- status-variants: terminal, tolerated-garbage, and empty, per
    # spec-on-disk-contract.md's live census of `donedone`-shaped values.
    write("status-variants/multiverse-state/tasks/20260101-fixer-aaaaaa/status", "done")
    write("status-variants/multiverse-state/tasks/20260101-fixer-bbbbbb/status", "donedone")
    write("status-variants/multiverse-state/tasks/20260101-fixer-cccccc/status", "")
    write_bytes(
        "status-variants/multiverse-state/tasks/20260101-fixer-dddddd/status",
        b"\xff\xfe not valid utf-8",
    )

    # --- task-dir-noise: a write-scope-violation scratch file sitting
    # next to the sanctioned ones, as in the synthetic example
    # `20000101-explorer-000001/repro_example.py`
    # (spec-on-disk-contract.md §B). Must not error; must not be read as
    # one of the sanctioned files.
    write("task-dir-noise/multiverse-state/tasks/20260101-fixer-eeeeee/brief.md", "# Task 20260101-fixer-eeeeee\n")
    write("task-dir-noise/multiverse-state/tasks/20260101-fixer-eeeeee/result.md", "# Result — task 20260101-fixer-eeeeee\n")
    write("task-dir-noise/multiverse-state/tasks/20260101-fixer-eeeeee/status", "done")
    write("task-dir-noise/multiverse-state/tasks/20260101-fixer-eeeeee/equalize_live.py", "# not a sanctioned task file\n")

    # --- activity-cursor-ahead: a cursor past the log's own high-water
    # seq — audit-mail-backlog-board-disk.md's P1 "log reset/replacement"
    # case, not an empty inbox.
    write(
        "activity-cursor-ahead/multiverse-state/routing.log",
        crc_line(
            [
                "2026-04-01T00:00:00+00:00",
                "v=1",
                "seq=5",
                "event=started",
                "task=20260401-fixer-eeeeee",
                "pane=w1:p1",
            ]
        )
        + "\n",
    )
    write("activity-cursor-ahead/multiverse-state/activity.cursor", "10\n")

    # --- activity-cursor-malformed: fail-closed, not a reset to zero.
    write("activity-cursor-malformed/multiverse-state/routing.log", "")
    write("activity-cursor-malformed/multiverse-state/activity.cursor", "not-a-number\n")

    # --- activity-markers-tolerant: corrupt .lastseen/.lastack must not
    # be findings — only cursor is the strict exception
    # (tests/test_activity.py:1049 per spec-on-disk-contract.md §E).
    write("activity-markers-tolerant/multiverse-state/activity.lastseen", "garbage, no tab\n")
    write("activity-markers-tolerant/multiverse-state/activity.lastack", "also\tgarbage\n")

    # --- worktrees-nested-task: a worktree whose nested task dir mirrors
    # the project-level tasks/<id>/ contract exactly, under its own id.
    write(
        "worktrees-nested-task/multiverse-state/worktrees/20260101-fixer-ffffff/multiverse-state/tasks/20260101-fixer-ffffff/brief.md",
        "# Task 20260101-fixer-ffffff\n",
    )
    write(
        "worktrees-nested-task/multiverse-state/worktrees/20260101-fixer-ffffff/multiverse-state/tasks/20260101-fixer-ffffff/status",
        "done",
    )

    # --- worktrees-adopted-followup: a worktree whose nested tasks/ holds
    # a SECOND task id alongside its own, the way worktree *adoption*
    # looks in a sampled project:
    # a worktree held its fixer's own id plus two Explorer ids and a
    # Librarian id — later follow-ups reusing the same checkout. An earlier version of this
    # checker treated this shape as a finding; real data proved that
    # wrong (45 false positives on first run against the live corpus) —
    # it is tolerated now, see worktrees.rs's module/inline comments.
    write(
        "worktrees-adopted-followup/multiverse-state/worktrees/20260101-fixer-111111/multiverse-state/tasks/20260101-fixer-111111/status",
        "done",
    )
    write(
        "worktrees-adopted-followup/multiverse-state/worktrees/20260101-fixer-111111/multiverse-state/tasks/20260101-scout-222222/status",
        "done",
    )

    # --- retire-format: a bare task id, a `multiverse/`-prefixed branch
    # ref (the observed on-disk shape — a sampled project's
    # `.multiverse/tasks/<task-id>/retire` uses this form for both lines),
    # and one line that is not `<id> <40-hex sha>` in either shape.
    write(
        "retire-format/multiverse-state/tasks/20260101-fixer-333333/retire",
        "20260101-fixer-444444 "
        + ("a" * 40)
        + "\nmultiverse/20260101-fixer-555555 "
        + ("b" * 40)
        + "\nnot-the-right-shape\n",
    )

    # --- fleet-state: breaker/seat/gauge, with conforming entries
    # plus wrong known-field types and non-object entries. Missing fields
    # are valid: the production readers use Option fields, not required keys.
    write(
        "fleet-state/breaker-state.json",
        json.dumps(
            {
                "claude:default": {"open": True},
                "grok": {"open": False, "close_reason": "manual", "closed_at": "2026-01-01T00:00:00+00:00"},
                "damaged-open": {"open": "true"},
                "damaged-entry": [],
                "damaged-timestamp": {"tripped_at": 1},
                "damaged-trip-count": {"trip_count": -1},
                "damaged-manual": {"manual": "true"},
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
    )
    write(
        "fleet-state/seat-state.json",
        json.dumps(
            {
                "claude": {"cap": 2},
                "grok": {"lend": True, "prefer": False},
                "damaged-cap": {"cap": -1},
                "damaged-cap-overflow": {"cap": 2**32},
                "damaged-lend": {"lend": "true"},
                "damaged-prefer": {"prefer": 1},
                "damaged-entry": False,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
    )
    write(
        "fleet-state/gauge-state.json",
        json.dumps(
            {
                "claude:default": {
                    "label": "Claude default",
                    "window_minutes": 300,
                    "percent_left": 42,
                    "resets_at": "2026-01-01T00:00:00+00:00",
                    "observed_at": "2026-01-01T00:00:00+00:00",
                    "last_probe_at": "2026-01-01T00:00:00+00:00",
                    "vendor": "claude",
                },
                "damaged-percent": {"percent_left": "42"},
                "damaged-window": {"window_minutes": -1},
                "damaged-label": {"label": 42},
                "damaged-entry": [],
                "damaged-secondary-percent": {"secondary_percent_left": {}},
                "damaged-secondary-window": {"secondary_window_minutes": 2**32},
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
    )

    # Nested corpus roots are passed explicitly to the runner by the tests.
    # Cover the field omissions behind the real false positives: legacy
    # breakers without open and partial gauges without probe metadata.
    tolerant = {
        "breaker-state.json": {
            "legacy-active": {"tripped_at": "2026-01-01T00:00:00+00:00"},
            "legacy-metadata": {"close_reason": "manual"},
            "empty": {},
            "nulls-and-extra": {
                "open": None, "manual": None, "trip_count": None,
                "tripped_at": None, "future_evidence": [1, 2],
            },
        },
        "gauge-state.json": {
            "partial-primary": {"label": "Primary", "percent_left": 42},
            "empty": {},
            "nulls": {
                "vendor": None, "label": None, "percent_left": None,
                "window_minutes": None, "resets_at": None,
                "observed_at": None, "last_probe_at": None,
                "percent_left_secondary": None, "label_secondary": None,
                "resets_at_secondary": None, "window_minutes_secondary": None,
            },
            "secondary": {
                "percent_left_secondary": 80.5, "label_secondary": "Weekly",
                "window_minutes_secondary": 10080,
            },
            "legacy-secondary-and-extra": {
                "secondary_percent_left": 80, "future_window": {"value": 1},
                "percent_left": -1,
            },
        },
        "seat-state.json": {
            "empty": {},
            "cap-only": {"cap": 0},
            "flags-only": {"lend": False, "prefer": True},
            "nulls-and-extra": {
                "cap": None, "lend": None, "prefer": None, "future_flag": [],
            },
        },
    }
    for filename, entries in tolerant.items():
        write("fleet-tolerant/fleet-state/" + filename,
              json.dumps(entries, indent=2, sort_keys=True) + "\n")
        # Syntax and top-level object shape remain strict for all stores.
        write("fleet-invalid-json/fleet-state/" + filename, "{\n")
        write("fleet-invalid-top-level/fleet-state/" + filename, "[]\n")

    print("fixtures regenerated under", HERE)


if __name__ == "__main__":
    main()
