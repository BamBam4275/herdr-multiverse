# Single source of truth for the gitleaks version this repo targets.
#
# Sourced (". scripts/release/gitleaks-version.sh") by
# scripts/release/export-fresh.sh and scripts/hooks/pre-push; read by the
# "secrets scan" job in .github/workflows/ci.yml, which has no shell
# `source` of its own and instead cats this file into $GITHUB_ENV.
#
# POSIX sh only: plain assignments, no command substitution, so every
# reader can treat this as data without executing anything unexpected.
#
# GITLEAKS_LINUX_X64_SHA256 is the sha256 of
# gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz from this release. It is
# pinned here literally -- computed locally from a direct download and
# cross-checked against the release's own gitleaks_*_checksums.txt, which
# must never be the only source trusted, since both files come from the
# same release and a tampered release could ship both consistently wrong.

GITLEAKS_VERSION=8.30.1
GITLEAKS_LINUX_X64_SHA256=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb
