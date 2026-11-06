# verification-planning — skill playbook

Plans a project-specific evidence path before non-trivial changes. Run it
automatically before non-trivial work rather than only on request — it is a
habit, not a tool.

---

## Skill: verification-planning

Before a non-trivial change, decide **what evidence would prove it correct** —
and decide it *first*, while you can still be honest about it. Evidence chosen
after the fact is chosen to pass.

### The four questions

1. **What would prove this works?** Name the specific observation, not the
   activity. "The suite passes" is an activity.
   "`test_no_chain_configured_asks_rather_than_guessing` fails before the
   change and passes after" is evidence.
2. **What would prove this is wrong?** If nothing could, you have no test — you
   have a description. A change with no falsifier is a change nobody can check.
3. **What does this project already use?** Evidence has to be *project-specific*.
   Find the existing harness — the test runner, the linter, the fixture, the
   mutation tool — and use it. Inventing a parallel one produces evidence nobody
   else will ever run.
4. **What would a passing check still miss?** Coverage says a line executed. It
   says nothing about whether an assertion would notice if that line were wrong.
   Name the gap out loud rather than letting the green tick imply it away.

### Write the plan before the change

State it in your result, in three lines or fewer:

```
Evidence: <the specific observation that would prove this correct>
Command:  <how to produce it, using this project's own tooling>
Gap:      <what this still would not catch>
```

Then make the change and report what the evidence actually said — including
when it said something you did not expect.

### The failure this exists to prevent

Work that is *finished* but not *checked*, reported with confidence. It reads
identically to work that is both, and the difference only surfaces later, in
someone else's debugging session.

A close second: evidence that cannot fail. A test that passes before your change
and after it has told you nothing, and it is worse than no test, because it
looks like one.

### When to skip it

A change whose correctness is visible in the diff — a typo, a comment, a
rename the compiler checks for you. Say you are skipping it and why, in a few
words. The habit is the point; the ceremony is not.
