# codemap — skill playbook

Hierarchical repository maps so agents understand codebases without
re-reading everything — the direct antidote to the cold-start problem that
hits a Scout dispatch hardest.

---

## Skill: codemap

You start cold. You inherit no conversation, no prior exploration, no memory of
this repository. A codemap is how a previous agent's reading becomes yours.

### Read one before you scan

Look for `.multiverse/codemap.md` in the repo root before running a single
search. If it exists, it is a map someone already paid to build: directory
purposes, entry points, where the important seams are.

Treat it as **a map, not the territory**. It tells you where to look. It can be
stale, and code always wins over a description of code — so verify the specific
thing you are about to rely on, and do not re-verify the whole thing.

### Say what you did with it — one line, always

State in your result which of these happened:

- **no codemap** — none existed, so you scanned from cold;
- **used the codemap** — and briefly, what it saved you looking for;
- **codemap was wrong** — it said something the code contradicts. Name the line.
  This is the most valuable of the three and the least likely to be volunteered.

One line. It is not ceremony: without it nobody can tell whether this skill is
earning its keep, because a run that silently used a map and a run that silently
ignored one produce identical-looking results. That is exactly what happened the
first time a map existed — two earlier runs had reported "no codemap existed",
the third said nothing at all, and there was no way to tell from the output
whether it had read the map or skipped it.

A stale map that nobody reports stays stale, and each agent after you pays for
it again.

### What a codemap is

A hierarchy, shallow to deep:

- **What this repository is** — one paragraph. What it does, what it is not.
- **Directory purposes** — one line each, for directories that matter. Skip
  vendored code, build output, anything generated.
- **Entry points** — where execution actually begins. The CLI, the server, the
  test runner.
- **Seams** — the few places where a change ripples. Config loading, the
  dispatch path, the schema. These are the lines worth spending words on.
- **What is not here** — the single most useful section, and the one always
  omitted. "No local backend", "no integration tests", "the API client is
  generated, do not hand-edit."

### What a codemap is not

Not a file listing — `ls` already exists and is never out of date. Not contents.
Not every symbol. A map that takes as long to read as the code is not a map.

If it is longer than a couple of hundred lines, it has stopped being a map and
become an inventory. Cut it.

### Writing or updating one

Only when your write scope permits, and only as a deliberate task — never as a
side effect of doing something else.

Update the part you actually learned about. A map corrected in the one place you
touched stays trustworthy; a map rewritten wholesale from a single task's view
becomes confidently wrong about the parts you never saw.

### Why this exists

Every cold agent that re-scans a repository spends its budget rediscovering
what the last one already knew. The map is the handover. Building the
tooling to generate one automatically is only worth it once measurement
shows re-scanning is genuinely the cost — but reading one, when it exists,
is free.
