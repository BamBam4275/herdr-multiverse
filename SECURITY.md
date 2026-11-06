# Security Policy

## Supported Versions

Herdr Multiverse is pre-1.0 and under active rewrite from a Python reference
implementation to Rust. Until the first `1.0.0` release, only the latest
tagged release receives security fixes.

| Version        | Supported          |
| -------------- | ------------------- |
| latest release | :white_check_mark: |
| older releases | :x:                 |

## Reporting a Vulnerability

Please **do not** open a public issue for a security vulnerability.

Report it privately through GitHub's
[private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability):

1. Go to the [Security tab](https://github.com/BamBam4275/herdr-multiverse/security)
   of this repository.
2. Select **Report a vulnerability**.
3. Fill in as much detail as you can: affected version, the action or
   command involved, reproduction steps, and the observed impact.

If private vulnerability reporting is unavailable to you, open a
[GitHub Security Advisory](https://github.com/BamBam4275/herdr-multiverse/security/advisories/new)
draft instead, or contact the maintainer, [@BamBam4275](https://github.com/BamBam4275),
through their GitHub profile.

### Response Times

| Stage                         | Target            |
| ------------------------------ | ------------------ |
| Initial acknowledgement        | Within 3 business days |
| Triage and severity assessment | Within 14 days |
| Fix or mitigation plan         | Depends on severity; coordinated disclosure timeline agreed with the reporter |

### Scope

This policy covers:

- The `herdr-multiverse` binary and its workspace crates
- Release build hooks and installer scripts (`scripts/`)
- Published release artifacts and their checksums
- The `herdr-plugin.toml` manifest and the actions/panes it exposes
- Handling of task briefs, worktrees, and agent prompts written under
  `.multiverse/`

It does **not** cover:

- Vulnerabilities in the native vendor CLIs (Claude, Codex, Grok) that
  Herdr Multiverse launches as subprocesses — report those upstream
- Vulnerabilities in Herdr itself — report those to the
  [Herdr project](https://herdr.dev/tui/docs/)
- Issues that require an attacker to already have local code execution as
  the same user running Herdr Multiverse

Herdr Multiverse does not receive, store, or proxy Claude/Codex/Grok API
keys; credentials remain owned by each native vendor CLI. See the README's
[Security and permissions](README.md#security-and-permissions) section for a
summary of the trust boundaries.
