# simplify — skill playbook

Typically granted to a reviewing role, not an implementing one. That
assignment is the design, not an accident: a Builder holding this skill
would quietly widen a bounded change into a refactor, which is exactly the
boundary its write-scope discipline draws.

---

## Skill: simplify

Behaviour-preserving simplification. Every word in that phrase is load-bearing:
if behaviour changes, this is not simplification — it is an unrequested rewrite
wearing simplification's clothes.

### The bar

A change earns its place only if the result is easier to *read*, and the
observable behaviour is identical. "Fewer lines" is not the goal; a dense
one-liner replacing eight clear ones has made things worse.

### What to look for

- **Duplication that has diverged.** Two copies drifting apart is a bug waiting
  to happen; two copies that are genuinely identical are usually fine.
- **A layer that only forwards.** A wrapper adding no behaviour, a config
  indirection with one value, an interface with one implementation.
- **Conditionals that cannot both be true**, or a branch nothing reaches.
- **Names that lie** — a function whose name describes half of what it does.
- **Reimplemented standard behaviour**, where the language or an existing
  dependency already does it.

### What to leave alone

- **Deliberate redundancy.** Some repetition is a safety property. If a rule
  appears in three places because the cost of an agent missing it is a modified
  repository, that is design, not duplication.
- **Anything you cannot show is behaviour-preserving.** Uncertain means leave it
  and say why.
- **Style you merely disagree with.** Match the surrounding code; do not
  normalize it toward your own taste.
- **Comments explaining *why*.** Delete restatements of the code, never the
  reasoning behind it — that reasoning is usually the most expensive thing on
  the page and the hardest to reconstruct.

### How to report it

Every simplification needs three things: **what** you changed, **why it is
easier to read**, and **what shows behaviour is unchanged** — the test that
covers it, or an argument if none does. A simplification with no evidence is a
proposal, and should be reported as one.

### Prefer proposing to applying

Unless your brief's write scope explicitly covers the file, list what you would
simplify and why. A critic that simplifies as it reviews has stopped being a
critic.
