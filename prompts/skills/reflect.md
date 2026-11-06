# reflect — skill playbook

Turns repeated workflow friction into reusable skills, role-prompt changes,
tests, or config. Invoke it deliberately, not as a side effect of other work.

---

## Skill: reflect

Notice what keeps going wrong, and turn it into something that stops it going
wrong again. The unit of output is a **change to the system**, not an
observation about it.

### The trigger is repetition, not annoyance

One awkward step is noise. The same awkward step three times is a missing
capability. Look for:

- Something explained again that was explained before.
- A correction you have now made more than once.
- A manual step performed every time, identically.
- A mistake whose fix is always the same.
- A question asked of the human that a config value would have answered.

### Where a fix belongs

Match the fix to the shape of the problem — the common failure is writing prose
where a constraint was needed.

- **A repeated judgement call** → a *skill*, if it is a playbook someone follows.
- **A repeated correction about how a role behaves** → the *role prompt*. It
  applies every time, so it belongs where it is read every time.
- **A repeated value someone types** → *config*. Values are not documentation.
- **A repeated failure the tests miss** → a *test*. Prose telling someone not to
  do it will not stop them; a failing test will.
- **A repeated whole workflow** → a new *role*, but only after the same shape
  has recurred enough that you can describe its lane in a sentence.

### The bar for proposing one

State three things or do not propose it: **what recurred**, with instances;
**what change would prevent it**; and **what it costs** — every skill is
something a future reader must load and understand.

The failure mode here is proposing a system for a thing that happened twice.
Structure has upkeep, and unread structure is worse than none.

### What not to do

- Do not add a skill that restates an existing one. Extend that one.
- Do not fix a one-off. Say it was a one-off.
- Do not silently apply the change. Propose it, unless your write scope covers
  the file and the brief asked you to.
