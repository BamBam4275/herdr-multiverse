# clonedeps — skill playbook

Clones dependency source locally so agents can inspect library internals
instead of relying on recalled API knowledge.

---

## Skill: clonedeps

When a library's behaviour matters and the documentation does not settle it,
read the library. Recalled API knowledge is the single most confident-sounding
source of wrong answers available to you, because it was true of *some* version.

### When to reach for it

- Behaviour that documentation does not cover, or covers ambiguously.
- A version-specific question — "does this happen in the version we pin?"
- A bug that only makes sense if the library does something you cannot see.
- An error message you cannot find any explanation for. Grep the source for the
  literal string; it is usually one match and it usually answers the question.

### When not to

- Standard, stable usage you are confident about.
- The answer is in the local `node_modules`, `site-packages`, or vendor
  directory you already have — **look there first.** An installed dependency is
  the version actually in use, which a fresh clone of `main` is not.
- You need general knowledge rather than this library's specifics.

### How

1. **Check what is already on disk.** The installed copy beats a clone: it is
   the exact version, already pinned.
2. **Match the version.** If you clone, check out the tag matching the
   dependency's pinned version. Reading `main` and reporting on it as if it were
   the installed version is worse than not looking — it is a wrong answer with
   evidence attached.
3. **Clone somewhere disposable**, never inside the project. A dependency's
   source is not part of this repository and must never end up committed to it.
4. **Read narrowly.** Find the symbol, read its neighbourhood, stop. A
   dependency is somebody's whole codebase and you have one question.

### Reporting

Quote the smallest decisive thing — a signature, a condition, one branch — and
give the path and version you read it in. `requests/adapters.py:412 (v2.31.0)`
is a finding. "The library appears to retry" is a guess with a citation stapled
to it.

State plainly when the source contradicts the documentation. That is a genuinely
valuable finding and the reason this skill exists.
