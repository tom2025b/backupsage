# ADR 0001 — Single output-safety boundary for every file the tool creates

Date: 2026-07-23 · Status: accepted · Milestone: v1.0.1 (issues #3, #36, #37)

## Context

v1.0.0 had three independent write paths, each with its own (absent) safety
story: index creation deleted whatever lived at the destination plus its
`-wal`/`-shm` sidecars before the source was read; `dedup -o` used a bare
truncating `std::fs::write` that follows symlinks; `master::open_at` opened
any path read-write and ran DDL before inspecting it. An alias (symlink,
hardlink, dot-dot spelling) could destroy an archive — violating the
permanent invariant that BackupSage never rewrites or deletes from an
archive. The roadmap spec requires one shared boundary: "path identity,
protected inputs, no-clobber promotion, journals, and precondition checks.
No feature implements its own weaker copy."

## Decision

One module, `src/outpath.rs`, owns destination safety:

- **Path identity** is (device, inode) of whatever the path resolves to
  (`FileId`), so hardlinks and alias spellings compare equal; the
  destination's parent is canonicalized before checks.
- **ProtectedSet** collects the run's inputs — source archives, whole
  source-directory trees, input indexes, the master catalog, and the
  `-wal`/`-shm` sidecars of every protected database. `check_dest` /
  `check_db_dest` refuse identity collisions, any symlink at the
  destination (dangling included), and containment in a protected tree.
- **Same-directory staging + promotion**: outputs build at
  `.name.tmp.<pid>` beside the final path. Reports promote **no-clobber**
  (`hard_link` + unlink — fails if anything exists at the final name,
  never follows symlinks). Indexes promote **replace** (`rename`) only
  after the build completed and only when the pre-build ownership probe
  proved the existing file is a completed BackupSage index of the same
  source. Failure or interrupt drops the staging files and leaves the
  prior output untouched.
- Safety rejections are ordinary errors (exit 1); the 0/1/2 exit-code
  contract is unchanged.

## Alternatives considered

- **Per-feature checks at each call site** — rejected by the spec ("no
  feature implements its own weaker copy"); drift is exactly how v1.0.0
  got three different unsafe writers.
- **O_TMPFILE / renameat2(RENAME_NOREPLACE)** — stronger atomicity, but
  Linux-kernel- and filesystem-specific with no std wrapper;
  `create_new` staging + `hard_link` promotion gives the same no-clobber
  guarantee portably on Unix.
- **An `--overwrite` flag instead of strict no-clobber** — explicitly
  deferred; the spec reserves overwrite semantics for a future explicit
  contract (v1.1+ plans).
- **Journals in v1.0.1** — listed in the boundary's long-term shape but
  deliberately deferred to the v1.2 executor; no v1.0.1 operation
  mutates user data, so there is nothing to journal yet.

## Consequences

- Index re-runs are crash-safe: the last completed index survives any
  failure; readers never see a partial database at the final path.
- Destinations that used to "work" by silently destroying data now fail
  closed (e.g. `-i` inside the source directory, `-i` at a foreign
  index); two v1.0.0 tests that pinned the old behavior were updated.
- `store::create_v3` refuses existing paths outright; every creator must
  stage. New write paths added later must route through `outpath` — code
  review should treat any direct `File::create`/`fs::write` of a
  user-visible output as a defect.
- The staged-name namespace (`.{name}.tmp.{pid}`) is reserved: crashed
  runs may leave debris there, and a later run with the same pid may
  clear it. Sidecar debris of a replaced index is removed at promote
  time (ownership was verified before the build).
