# worktrees — skill playbook

Git worktrees as safe, isolated coding lanes for risky or parallel work. With
one worktree per writer, two agents *cannot* touch the same working copy, so
write isolation is physics rather than a rule you are trusted to follow.

---

## Skill: worktrees

A git worktree is a second checkout of the same repository at a different path,
on its own branch, sharing one `.git`. It is the cheapest way to make parallel
work safe.

### When to use one

- **Any write that could go wrong** and would be awkward to unpick in place.
- **Parallel work** — two writers on one working copy is the failure that costs
  real time, and it fails in the most confusing way: neither agent did anything
  obviously wrong, and the tree is wrong anyway.
- **A risky refactor** you might want to abandon wholesale.

### When not to

A single small edit you would revert with one command. The worktree costs a
directory and a branch; use it where that is cheaper than the mistake.

### The rules that matter

- **Stay inside your worktree.** Its path is your working directory. If you find
  yourself writing a path that climbs out of it, stop — that is the isolation
  failing, and it fails silently.
- **One writer, one worktree.** Never share. The entire guarantee comes from
  exclusivity.
- **Commit before you finish.** A worktree can be removed; committed work
  survives. Uncommitted work in a discarded worktree is simply gone.
- **Reclaim a finished lane with `multiverse worktrees <path> --prune`, never with
  `git worktree remove` or `git branch -D` directly.** Prune records the
  task's outcome before it deletes the status file; raw removal deletes the
  status file first and records nothing, silently and unrecoverably — the
  outcome can then only come from a human who still remembers what the task
  did. Prune reclaims the *name* of a lane whose commits are already
  reachable from HEAD and whose outcome is recorded — at that point the
  branch is only a label, not a copy of anything, so deleting it (`git
  branch -d`, never `-D`) discards nothing. An unmerged branch is the one
  whose name is load-bearing, and prune always keeps it: the order is merge,
  reconcile, then prune. The orphan sweep asks a second question of a branch
  ancestry keeps: whether patch-id (`git cherry`) says the work has landed
  anyway, under a different sha, via a rebase, squash, or cherry-pick. That
  verdict never authorises `-d` or `-D` — patch-id has a false-"landed" mode
  ancestry does not, a commit that landed and was later reverted still reads
  as landed — so it only ever authorises *archiving*, and it is not the only
  thing that can: a later, completed Scout or Seer assessment that named
  this exact branch and sha in its own `retire` sidecar (next to that
  assessment's `result.md` — the Q21 `revise` pattern, not a sentence in the
  prose) authorises the same archive, and a self-authored line, a line from a
  non-assessment task, or one from an assessment that did not finish `done`
  authorises nothing. Either way, `refs/multiverse/archive/<task-id>`
  is written at the branch's tip, then the branch is retired with a
  compare-and-swap, never a bare delete. There are three outcomes now, not
  two: **reclaimed** (ancestry, `-d`, nothing lost because nothing unique
  remained), **archived** (patch-id or an assessment's `retire` line, branch
  retired but the commit kept alive under the archive ref), and **kept**
  (neither authoriser applies, the branch stays). Once a branch is gone —
  reclaimed or archived —
  the authoritative question about it is not "does the branch exist" but "is
  the behaviour present" — grep the tree for what the task was supposed to
  produce. The routing log answers what the agent *reported*, which can
  differ from what actually landed; don't treat it
  as settling the question. `git log --merges --grep=<task-id>` only finds
  a lane that landed via a merge commit — a fast-forward landing leaves no
  trace there.
- **To get an archived lane's *work* back** — a different question from
  whether it landed — `multiverse worktrees <path>` lists every archived
  lane with its task id, short sha, and the restore command, printing
  nothing when there are none, and `multiverse worktrees <path> --restore
  <task-id>` recreates only the branch, at its archived tip: no worktree,
  no checkout, no merge. It refuses if a branch of that name already
  exists, naming it, and refuses an unknown id, naming the ref it looked
  for, leaving the archive ref in place either way so the restore is
  itself reversible. A restored branch tells you what an agent once wrote;
  it does not tell you whether that work landed — the behaviour-in-the-tree
  rule above still stands.
- **`--prune --dry-run` previews every candidate — the reason for each
  refusal included — and changes nothing.** PREDICT THE SPLIT BEFORE YOU RUN
  IT, then compare: a preview you predicted against tells you where your
  model of the system is wrong; one you did not only tells you what will
  happen. One project's preview once predicted 22 and got 21 — the one-off
  was a real gate misfire, and the prediction is the only reason it read as
  anything but a normal result. If you count candidates from a preview,
  exclude the per-section totals (`removed N of M`, `reclaimed N of M`) —
  the command's own `--help` text has the pattern.
- **Never `git stash` bare.** The stash stack is shared across every worktree of
  the repository, so a bare `stash`/`stash pop` can pop somebody else's work.
  Make a temporary commit instead. If you must stash, tag it
  (`git stash push -u -m "<unique-tag>"`), find your own entry by that tag, and
  apply it rather than popping.
- **Never push to main, force-push, or merge** unless you were asked to in the
  brief.

### Running the suite from inside your lane

A lane has no virtualenv of its own — `.venv/` is gitignored, so it does not
come across into a worktree. Do NOT create one just to run tests. The
project's own venv is three levels up, and its interpreter run from inside
your lane imports YOUR lane's code, not the shared checkout's, because the
current directory is what `sys.path` searches first:

    PYTHONDONTWRITEBYTECODE=1 ../../../.venv/bin/python -m pytest -n 4

Only if that interpreter genuinely does not exist should you create one, and
then delete it before you finish.

Three cases, because part of this is easy to over-generalize the wrong way:

- `../../../.venv/bin/python -m pytest -n 4` — safe. `-m` puts the current
  directory first on `sys.path`, so the lane's module wins the import.
- `../../../.venv/bin/python path/to/script.py` — **not** safe on its own. A
  script run by path puts the *script's* directory first instead, so an
  ad-hoc driver can silently import the main checkout's module. Add
  `PYTHONPATH=.`, or keep the script inside the lane.
- The `multiverse` CLI on `PATH` — never the lane's code, either way. It is a
  shared editable install that resolves to the main working tree, so running
  `multiverse ...` from inside a lane exercises main's code, not yours.

### Cancellation is not rollback

If you are stopped mid-change, what remains on disk is whatever the filesystem
held at that instant — there is no graceful unwind. Someone will have to inspect
and reconcile it. Leaving a clear note about what you had done, and what you had
not, is worth more than one extra edit.

### Why the discipline is stated even though the isolation is structural

You may be dispatched into a worktree without being told. The rules above are
what keep the guarantee real from the inside — a writer that reaches outside its
lane defeats an isolation nobody else can see it defeating.
