# Releasing BackupSage

The release checklist is **executable**: `scripts/release-check.sh` is the
single source of truth, and this page deliberately adds no checks of its
own — read the script for the authoritative step list (build, full test
suite, golden-fixture contract gate, clippy, fmt, audit, metadata
agreement). CI runs it from a clean checkout on every `v*` tag push and on
manual dispatch (`.github/workflows/release-check.yml`).

## Sequence

1. On `main`, run the checklist pre-tag:

   ```bash
   scripts/release-check.sh
   ```

   Tag agreement is skipped when HEAD carries no tag; everything else is
   enforced.

2. Version bump (if not already merged): set `version` in `Cargo.toml`,
   run `cargo build` to refresh `Cargo.lock`, land it via the normal
   branch → PR → merge cycle.

3. Re-run with the intended tag name to enforce metadata agreement before
   the tag exists:

   ```bash
   scripts/release-check.sh --tag vX.Y.Z
   ```

4. Tag (annotated) and push:

   ```bash
   git tag -a vX.Y.Z -m "vX.Y.Z — <one-line theme>"
   git push origin vX.Y.Z
   ```

   The tag push triggers the Release check workflow — the release is not
   done until that run is green.

5. Publish the GitHub release for the tag, title including the version
   (e.g. `v1.0.2 — Correctness and debt`). Re-running the checklist (or
   `workflow_dispatch`) afterwards also verifies the release title and
   tag agree with `Cargo.toml`.

## Invariants

- Exit codes and JSON shapes are frozen by golden fixtures
  (`docs/CONTRACT.md`); the checklist fails on any contract drift.
- The executable checklist also requires the public
  [compatibility policy](COMPATIBILITY.md), which defines the v1.x window and
  breaking-change procedure for those contracts and on-disk formats.
- The checklist is read-only against user data and archives — it writes
  only build artifacts and test temp directories.
- Every failing step names itself and says what to fix; a release with
  any red step does not ship.
