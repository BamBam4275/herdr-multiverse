# deepwork — skill playbook

A structured workflow for large, risky, multi-phase coding sessions with
review gates. Invoke it deliberately, because the workflow itself is the
expensive part — you do not want it on a two-line fix.

---

## Skill: deepwork

For a change too large to hold in one pass: multi-phase, risky, or crossing
system boundaries. The value is the **gates**, not the plan. A plan without
gates is a longer way to make the same mistake.

### Phase 0 — decide it is deepwork

It is, if any of these hold: the change spans several components · a mistake
would be expensive or hard to unpick · you cannot state the finished state in a
sentence · earlier attempts have already failed.

If none hold, say so and do the work directly. Ceremony on a small change is
its own kind of waste.

### Phase 1 — state the target

One paragraph: what is true when this is done, observably. Then, just as
explicitly: **what is out of scope.** Large changes fail by accretion, and the
out-of-scope list is what you point at when the third adjacent improvement
suggests itself.

### Phase 2 — split into gated steps

Each step must be independently checkable and leave the repository *working*.
A step that only makes sense once the next one lands is not a step; merge them.

For each: what changes, what proves it, what it would break.

### Phase 3 — the gates

**Stop at every gate.** A gate is a point where you report and wait, not a point
where you note progress and continue. Passing your own gate unreviewed defeats
the entire skill.

Gate at least:

- after the first step that touches shared code,
- before anything irreversible — a migration, a deletion, a rename that spans
  callers,
- when evidence disagrees with the plan.

At a gate, report: what is done, what the evidence said, what is next, and what
you are now less sure of than when you started.

### Phase 4 — reconcile

When the plan and reality diverge, **say so and stop**. Do not quietly re-plan
mid-flight; the human chose this workflow to be told about exactly that. A plan
revised silently is a plan nobody agreed to.

### The failure this prevents

A long autonomous run that is confidently wrong from step two onward, discovered
at step nine. Gates make being wrong cheap by making it early.
