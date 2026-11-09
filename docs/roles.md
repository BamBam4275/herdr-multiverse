# Roles

Herdr Multiverse dispatches work to a small set of named roles. Each role is
a **process boundary** — a write scope, a default interaction pattern, and
an escalation path — not a promise about which model or vendor CLI runs it.
The actual vendor CLI, account, model, quota pool, and approval policy come
from configuration; no role silently broadens its own scope at runtime.

The contracts below match the shipped prompts under `prompts/role-*.md`.
See the [README roles table](../README.md#roles) for the short version.

`--role` on `dispatch`, `backlog add`, and `backlog ask` accepts the worker
keys `scout`, `builder`, `archivist`, `critic`, `seer`, `council`,
`artisan`, and `watcher`. `nexus` is unknown as a `--role` (exit 3).
`council` is also a dedicated subcommand. `dispatch --dry-run` plans a
real dispatch for any of the worker roles (seat selection, breaker and
degrade/fallback policy) against the project's configured chain, and
`dispatch --yes` actually runs it, against the real Herdr runner;
`council --yes` convenes a real roster the same way. Both carry scope
cuts short of full Python parity — see the README's
[Status](../README.md#status) section for what each still skips. This
page is the contract the shipped prompts already encode, independent of
how much of the engine is wired to them.

## Nexus

- **Purpose:** long-lived coordinator and task router. Plans, schedules,
  delegates, monitors, reconciles, and verifies — does not do the
  specialists' work.
- **Default interaction:** reads project and config state; starts explicit
  role work in Herdr panes.
- **Writes / side effects:** orchestration metadata, task briefs, and the
  routing log; may open panes. Must not silently spend quota or modify a
  project's worktree itself.
- **Automatic or explicit:** launched by `multiverse start`, never
  dispatched as a `--role`.
- **Escalates to:** a human for irreversible decisions; Council for
  high-stakes disagreement; Critic for review before a change is accepted.

## Scout

- **Purpose:** fast codebase reconnaissance that returns a compressed map,
  not file contents.
- **Default interaction:** read-only vendor CLI session in the target
  worktree.
- **Writes / side effects:** only the result and status paths named in the
  brief. Nothing in the repository itself.
- **Automatic or explicit:** dispatched with `--role scout`.
- **Escalates to:** Nexus.

## Builder

- **Purpose:** implement a scoped, already-decided change inside an
  isolated worktree. Execution, not discovery.
- **Default interaction:** vendor CLI with the assigned task worktree as
  the working directory.
- **Writes / side effects:** exactly the files or directories the brief
  names, plus result and status. Never another lane's files.
- **Automatic or explicit:** dispatched with `--role builder` against a
  specific brief and write scope.
- **Escalates to:** Critic for review, then Nexus for reconciliation.

## Archivist

- **Purpose:** external knowledge retrieval — library documentation, API
  references, official examples, release notes, web lookups — so Nexus
  does not spend its own context reading the pages.
- **Default interaction:** read-only network, browser, or documentation
  lookup.
- **Writes / side effects:** only the result and status paths named in the
  brief, including anything cloned or downloaded to read.
- **Automatic or explicit:** dispatched with `--role archivist`.
- **Escalates to:** Nexus.

## Critic

- **Purpose:** adversarial review of another agent's work, running on a
  different vendor CLI from whoever wrote the code under review.
- **Default interaction:** read-only review of a Builder's worktree plus
  test and log output.
- **Writes / side effects:** only the result and status paths named in the
  brief. Never the code under review.
- **Automatic or explicit:** dispatched with `--role critic`, including
  `--reviewing` for a dedicated review pass.
- **Escalates to:** Council for disputed findings, or Nexus for routine
  sign-off.

## Seer

- **Purpose:** architecture, risk, and hard debugging — called when the
  cost of being wrong is high and the answer is not obvious.
- **Default interaction:** read-only analysis of the question in the brief.
- **Writes / side effects:** only the result and status paths named in the
  brief. Never implements its own recommendation in code.
- **Automatic or explicit:** dispatched with `--role seer`. Queue for this
  role rather than degrading to a weaker one.
- **Escalates to:** Nexus.

## Council

- **Purpose:** high-stakes, multi-vendor decision support. The disagreement
  is the product.
- **Default interaction:** explicit, opt-in parallel sessions across
  multiple vendor CLIs, via `multiverse council` (which requires `--yes`).
- **Writes / side effects:** consumes the highest quota of any role; writes
  only the result and status named in the brief; never implements.
- **Automatic or explicit:** explicit only. Council never dispatches itself.
- **Escalates to:** Nexus.

## Artisan

- **Purpose:** UI/UX design, the edits that implement it, and design
  review — anything a user looks at and polish matters for.
- **Default interaction:** reads existing behavior and assets, then
  designs *and* ships the scoped presentation change. On a review card,
  judgment only.
- **Writes / side effects:** exactly the files the brief names (the visual
  and interaction surface). On a review card, nothing at all.
- **Automatic or explicit:** dispatched with `--role artisan`.
- **Escalates to:** Critic for review, then Nexus.

## Watcher

- **Purpose:** visual and media analysis — images, PDFs, diagrams,
  screenshots — converted into concise structured text so the raw bytes
  never enter Nexus's own context.
- **Default interaction:** read-only inspection of the media named in the
  brief.
- **Writes / side effects:** only the result and status paths named in the
  brief. No skills are granted to this role.
- **Automatic or explicit:** dispatched with `--role watcher`.
- **Escalates to:** Nexus.

## Legacy on-disk names

New write and config surfaces accept only the names above. `--role fixer`
(and the other retired names) is unknown. History already on disk — task
ids such as `20261031-fixer-ab12cd`, `role=` fields in `routing.log`,
existing briefs — still reads. The mapping is:

| History / on-disk name | Current name |
|---|---|
| `orchestrator` | Nexus |
| `explorer` | Scout |
| `fixer` | Builder |
| `librarian` | Archivist |
| `reviewer` | Critic |
| `oracle` | Seer |
| `designer` | Artisan |
| `observer` | Watcher |
| `council` | Council |

Council was not renamed. Nexus seeds are written to
`.multiverse/nexus.md`. A project still carrying the legacy
`.multiverse/orchestrator.md` keeps reading and writing that file until
its next seed write, which migrates it onto `nexus.md` — backing up the
legacy file first if a human had edited it, then removing it, so at most
one seed file remains. Nothing renames the file out from under a project
that is never reseeded. Read-only commands accept either name.

The one-shot config migrator that rewrites `[roles.fixer]` (and the rest)
to the current keys, preserving comments, lives in `mv-config::migrate`
and is wired to `multiverse config migrate --dry-run`/`--backup`. See
[`docs/configuration.md`](configuration.md#migrations) for the command,
and [`docs/migrating-from-python.md`](migrating-from-python.md) for an
operator-facing walkthrough.

## Notes

- Quota weight, default vendor/account mapping, and the exact escalation
  trigger for each role are configuration, not a fixed contract — see
  [`docs/configuration.md`](configuration.md) for how roles are wired to
  accounts and vendor CLIs.
- A role's write scope is enforced by which worktree and files its task
  brief names, not by the role name alone. A Builder brief that names an
  exclusive file set is the actual boundary; this page describes the
  default shape of that boundary per role.
