<!--
Thanks for contributing to Herdr Multiverse. Please fill out this template —
it's the checklist reviewers use to evaluate role-semantics, permission, and
release impact.
-->

## Summary

<!-- What does this change do, and why? -->

## Related issue(s)

<!-- Link related issues: "Closes #123" or "Relates to #123" -->

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Documentation
- [ ] Refactor / internal change
- [ ] Release / CI / tooling

## Checklist

- [ ] I ran `cargo fmt --all -- --check`, `cargo clippy --workspace
      --all-targets --all-features -- -D warnings`, and `cargo test
      --workspace --all-features` locally
- [ ] I added or updated tests that cover this change
- [ ] I added a `CHANGELOG.md` entry under `[Unreleased]` (or this change
      doesn't need one, e.g. docs-only)
- [ ] I updated relevant docs (`README.md`, `docs/roles.md`,
      `docs/configuration.md`, `docs/architecture.md`) if behavior, role
      scope, or configuration changed

## Security / permission review

- [ ] This change does not alter `herdr-plugin.toml` actions, panes, or
      events
- [ ] This change does not alter what a role can read, write, or execute
- [ ] This change does not alter release build hooks, checksum
      verification, or installer scripts
- [ ] If any box above is unchecked, I described the permission or
      side-effect impact below

<!-- If this PR changes a screenshot-visible surface (README visuals, board
     rendering, TUI), attach a screenshot or GIF. -->

## Additional context

<!-- Anything else reviewers should know -->
