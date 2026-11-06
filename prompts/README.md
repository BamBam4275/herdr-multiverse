# prompts/

Every file under this directory is loaded by `multiverse` at runtime (or
embedded with `include_str!` into the shipped binary) and assembled into a
dispatched agent's `brief.md`, or into the Nexus seed file.
Nothing here is documentation for a human reader of this repository — it is
the actual text an agent reads before it starts work. Edit it as you would
edit a prompt, not a doc page: wording here changes agent behavior directly.

This index exists so that index is available without a rebuild and without
depending on anything outside this directory. It does not link to `docs/` —
that's project documentation for humans, not an input to prompt assembly,
and the two can drift at different speeds.

## Column prompts (stage contracts)

Loaded by stage, not by role — several different roles can work the same
column and inherit the same contract.

| File | Stage | Contract |
|---|---|---|
| `column-explore.md` | Explore | Read-only discovery. Produce a map, not a plan or a fix. |
| `column-execute.md` | Execute | Write only inside the brief's declared scope. Implement the plan, don't revise it. |
| `column-review.md` | Review | Findings only. Never fix what you find. |

## Role prompts

Loaded by role, concatenated after the column prompt in a dispatched agent's
brief. Each one states, near the top, its purpose, what it may write, and its
stop conditions (`done` / `blocked` / `failed`); the rest of the file is the
detail behind that summary.

| File | Role |
|---|---|
| `role-nexus.md` | Nexus — long-lived coordinator and task router |
| `role-scout.md` | Scout — repository and context reconnaissance |
| `role-builder.md` | Builder — bounded implementation in an isolated worktree |
| `role-archivist.md` | Archivist — external knowledge retrieval (library docs, APIs, web) |
| `role-critic.md` | Critic — adversarial, cross-vendor review |
| `role-seer.md` | Seer — architecture, risk, and hard-debugging escalations |
| `role-council.md` | Council — high-stakes, multi-vendor decision support |
| `role-artisan.md` | Artisan — UI/UX design and the edits that implement it |
| `role-watcher.md` | Watcher — visual and media analysis (images, PDFs, diagrams) |

Nexus is never dispatched as a task; it is the one role launched directly by
`multiverse start`, not routed to through a brief.

## Skills (`skills/`)

A skill is an opt-in playbook, granted per role in configuration and
rendered into a brief only when granted. `*` grants all, `!name` denies one,
`!*` denies all; denial always wins.

| File | Skill | When it applies |
|---|---|---|
| `skills/codemap.md` | `codemap` | Reading or maintaining a repository map before a cold scan |
| `skills/clonedeps.md` | `clonedeps` | A dependency's behavior isn't settled by its documentation |
| `skills/deepwork.md` | `deepwork` | A change too large or risky to hold in one pass |
| `skills/docker.md` | `docker` | Starting a short-lived, owned, auto-cleaned Docker workload |
| `skills/multiverse.md` | `multiverse` | Inspecting or proposing changes to this Multiverse setup itself |
| `skills/reflect.md` | `reflect` | Turning a recurring friction into a durable fix |
| `skills/simplify.md` | `simplify` | Behavior-preserving simplification during review |
| `skills/verification-planning.md` | `verification-planning` | Deciding evidence before making a non-trivial change |
| `skills/worktrees.md` | `worktrees` | Working inside, or reasoning about, a dispatched git worktree |

## Templates (`../templates/`)

Not prompts — shell scaffolding a human runs once to set up per-account
wrappers. See `templates/install-wrappers.sh` and
`templates/claude-account.sh.template`.

## Conventions these files share

- **Purpose first.** A role prompt opens with what the role is for, what it
  may write, and what ends its task (`done` / `blocked` / `failed`) before
  any detail.
- **Isolation first, in the column prompts.** Each column prompt states the
  "stay inside this repository" rule before anything else, because an agent
  that reaches outside its worktree in the first few lines it reads is the
  failure this ordering exists to prevent.
- **No local paths, no internal project names.** These files ship to every
  installation of Multiverse; nothing in them should assume one person's
  machine, accounts, or private projects.
- **`\n---\n` splits a human preamble from the agent-facing body.** The text
  before the first `---` line is commentary for someone reading the file in
  this repository; only the text after it is sent to an agent.
