#!/usr/bin/env bash
# The executable release checklist (#56, child of #11).
#
# This script IS the checklist — docs/RELEASE.md points here and adds no
# steps of its own, so prose and automation cannot drift. CI runs it on
# every v* tag push and on manual dispatch (.github/workflows/release-check.yml).
#
# Usage:
#   scripts/release-check.sh              # pre-tag run: tag agreement is
#                                         # enforced only if HEAD is tagged
#   scripts/release-check.sh --tag vX.Y.Z # enforce agreement with this tag
#
# Read-only against user data and archives: the only writes are cargo build
# artifacts and the test suite's own temp directories.

set -euo pipefail
cd "$(dirname "$0")/.."

TAG=""
if [[ "${1:-}" == "--tag" ]]; then
    TAG="${2:?--tag needs a value, e.g. --tag v1.0.2}"
elif [[ -n "${1:-}" ]]; then
    echo "unknown argument '${1}' — usage: scripts/release-check.sh [--tag vX.Y.Z]" >&2
    exit 1
fi
if [[ $# -gt 2 ]]; then
    echo "unexpected extra arguments: ${*:3} — usage: scripts/release-check.sh [--tag vX.Y.Z]" >&2
    exit 1
fi

STEP=0
step() {
    STEP=$((STEP + 1))
    echo
    echo "━━ release check · step ${STEP}: $1"
}
fail() {
    echo "✗ release check FAILED at step ${STEP}: $1" >&2
    exit 1
}

step "working tree is clean (untracked files are allowed)"
# No grep -q here: under pipefail an early grep exit can SIGPIPE git status
# and turn a dirty tree into a false pass — let grep drain its input.
if [[ -n "$(git status --porcelain=v1 | grep -v '^??' || true)" ]]; then
    git status --short | grep -v '^??' >&2 || true
    fail "modified or staged files present — release checks must run on committed state"
fi

step "cargo build --locked"
cargo build --locked || fail "build broken (or Cargo.lock out of date — run cargo build and commit the lockfile)"

step "cargo test --locked (full suite, includes the golden-fixture contract)"
cargo test --locked || fail "test suite red — fix before releasing"

step "golden-fixture contract suite, named gate"
cargo test --locked --test contract || fail "public JSON / exit-code contract drifted — see tests/contract.rs and docs/CONTRACT.md"

step "compatibility policy present"
test -s docs/COMPATIBILITY.md || fail "missing public-contract compatibility policy — see docs/COMPATIBILITY.md"

step "cargo clippy --all-targets --locked -- -D warnings"
cargo clippy --all-targets --locked -- -D warnings || fail "clippy warnings present"

step "cargo fmt --check"
cargo fmt --check || fail "unformatted code — run cargo fmt"

step "cargo audit"
if ! command -v cargo-audit > /dev/null; then
    fail "cargo-audit not installed — install with: cargo install cargo-audit --locked"
fi
cargo audit || fail "unresolved security advisories — see docs/SECURITY.md for the disposition process"

step "metadata agreement (Cargo.toml · binary --version · tag · release notes)"
# Assignments carry '|| true' so a failing substitution reaches the named
# fail() below instead of dying silently under set -e + pipefail.
VER="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/' || true)"
[[ -n "$VER" ]] || fail "could not read version from Cargo.toml"
echo "   Cargo.toml version: ${VER}"

BINVER="$(cargo run --locked --quiet -- --version | awk '{print $2}' || true)"
[[ -n "$BINVER" ]] || fail "could not get --version from the built binary"
echo "   binary reports:     ${BINVER}"
[[ "$BINVER" == "$VER" ]] || fail "binary --version (${BINVER}) disagrees with Cargo.toml (${VER})"

if [[ -z "$TAG" ]]; then
    TAG="$(git tag --points-at HEAD | grep '^v' | head -n1 || true)"
fi
if [[ -n "$TAG" ]]; then
    echo "   tag under check:    ${TAG}"
    [[ "$TAG" == "v${VER}" ]] || fail "tag ${TAG} disagrees with Cargo.toml version ${VER} (expected v${VER})"
else
    echo "   no tag at HEAD and no --tag given — tag agreement skipped (pre-tag run)"
fi

if command -v gh > /dev/null && gh auth status > /dev/null 2>&1; then
    # Looking a release up BY the expected tag can never expose a release
    # published under the wrong tag — scan the release list for any release
    # whose title claims this version under a different tag.
    MISTAGGED="$(gh release list --limit 50 --json tagName,name \
        --jq ".[] | select((.name | contains(\"${VER}\")) and .tagName != \"v${VER}\") | .tagName" \
        2> /dev/null || true)"
    [[ -z "$MISTAGGED" ]] || fail "release under tag ${MISTAGGED} mentions version ${VER} — tag and title disagree (expected v${VER})"

    RELNAME="$(gh release view "v${VER}" --json name --jq .name 2> /dev/null || true)"
    if [[ -n "$RELNAME" ]]; then
        echo "   GitHub release:     '${RELNAME}' (v${VER})"
        case "$RELNAME" in
            *"${VER}"*) ;;
            *) fail "release title '${RELNAME}' does not mention version ${VER}" ;;
        esac
    else
        echo "   no GitHub release for v${VER} yet — notes agreement skipped (created after tagging)"
    fi
else
    echo "   gh unavailable/unauthenticated — release-notes agreement skipped"
fi

echo
echo "✓ release check passed — all ${STEP} steps green (version ${VER})"
