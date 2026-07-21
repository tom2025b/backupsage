# BackupSage Future Roadmap Design

**Date:** 2026-07-21

**Status:** Approved direction
**Repository:** `tom2025b/backupsage`

## Decision

Replace the original oversized `v1.1` and `v2.0` buckets with seven
risk-gated releases:

`v1.0.1 safety -> v1.0.2 correctness -> v1.1 intelligence/plans -> v1.2 controlled actions -> v1.3 media/scale -> v2.0 local web -> v2.1 remote sources`

The roadmap is organized as GitHub milestones with bounded capability issues.
Items that still span multiple independently reviewable changes are parent
tracking issues and must be decomposed into child issues before implementation.
Milestones have exit gates rather than speculative due dates. A GitHub Project
is deferred until the issue volume or contributor count makes a board useful.

Two alternatives were considered and rejected:

1. Keep only `v1.1` and `v2.0`. This hides safety dependencies inside very
   large milestones and makes progress hard to assess.
2. Use capability-only milestones such as Safety, Media, and Web. This is
   flexible, but it does not answer which capabilities may safely ship
   together.

## Product direction

BackupSage should become archive intelligence and recovery tooling, not a
general photo manager. Its differentiator is offline, cross-backup knowledge:
search, duplicate analysis, source diffs, copy coverage, integrity checks,
reviewable plans, and safe recovery.

The permanent invariant is:

> BackupSage never rewrites or deletes from an archive.

Archive extraction is copy-only. Mutating operations target directories only
and require a versioned plan, precondition revalidation, no-clobber behavior,
and a durable execution journal.

## Why actions move later

`organize` deserves stronger dry-run protection than deduplication. A naive
organize operation moves the only copy and changes its pathname, so conflicts
and partial runs are not naturally idempotent. Deduplication can retain a
verified keeper and converges toward an already-deleted end state, although it
still needs quarantine and replica-floor protections. Both use the same plan
contract, but organize must not be the first mutation used to invent that
contract.

## Release milestones

### v1.0.1 - Safety baseline

**Outcome:** the current read-only promise becomes true at every output path.

Issues:

1. **Protect index destinations and preserve the last-good index.** Reject
   archive, source-content, sidecar, symlink, hardlink, and foreign-file
   aliases before mutation. Build in a same-directory temporary database and
   promote only a completed index. Failed re-indexing leaves the prior index
   intact.
2. **Make dedup report writes no-clobber and alias-aware.** A report output may
   not replace an archive, input index, master catalog, SQLite sidecar, symlink,
   hardlink, or any pre-existing file unless a future explicit overwrite
   contract permits it.
3. **Validate master catalog identity before read-write open.** Existing files
   must carry the BackupSage master signature before migrations or DDL run.
   Arbitrary SQLite databases and per-source indexes are rejected unchanged.
4. **Sanitize untrusted text and preserve lossless paths.** Escape control bytes
   in archive paths, entry names, link targets, snippets, labels, warnings, and
   errors. Add non-UTF-8 fixtures and a backward-compatible JSON representation
   that retains raw path bytes alongside a safe display value.
5. **Remediate current Rust advisories and publish v1.0.1.** Upgrade `tar` past
   the PAX-header desynchronization advisory and `anyhow` past its current
   unsoundness advisory, document remaining unmaintained transitive crates,
   run the release gates, then create the first safe tag and GitHub release.

Exit gates:

- Every destructive-path regression leaves protected fixture digests and
  identities unchanged.
- A corrupt or interrupted re-index preserves the last completed index.
- Terminal fixtures cannot emit raw C0/C1, DEL, ESC, CSI, or OSC controls, and
  machine-readable fixtures round-trip non-UTF-8 path bytes.
- Full tests, Clippy, formatting, and `cargo audit` complete with every
  remaining warning explicitly documented.
- No v1.0 tag is cut from the current unsafe baseline.

### v1.0.2 - Correctness and debt

**Outcome:** close known specification gaps before adding new workflows.

Issues:

1. **Add a sparse-tar differential end-to-end corpus.** Cover old GNU sparse
   and PAX 0.0/0.1/1.0 variants, compressed forms, malformed maps, and a
   sentinel entry after each sparse member. Compare logical bytes, sizes, and
   BLAKE3 results with GNU tar.
2. **Make near-duplicate groups keeper-safe.** Do not let union-find
   transitivity imply that every member is within the configured threshold of
   the keeper. Report explicit edges or keeper-star-safe groups and verify the
   result against a brute-force oracle.
3. **Add explicit content indexing modes.** Support `full`, `search-only`, and
   `metadata-only`. Document that contentless FTS reduces stored content but
   still exposes tokens and frequencies; it is not encryption or a privacy
   boundary.
4. **Close the v1 debt register and release-contract gaps.** Reconcile stale
   implementation checklists, freeze regression fixtures for public JSON, and
   make the release checklist executable in CI.

Exit gates:

- Sparse behavior has end-to-end evidence rather than only unit fixtures.
- Every automatic future deletion candidate is directly safe relative to its
  selected keeper.
- Index mode and schema metadata make privacy behavior unambiguous.
- Public JSON fixtures and the executable release checklist cover the debt
  register's remaining contract work.

### v1.1 - Read-only intelligence and immutable plans

**Outcome:** users can understand change, coverage, integrity, and proposed
actions without changing their data.

Issues:

1. **Freeze the immutable action-plan contract.** Version the schema and pin
   source identity, archive BLAKE3, index UUID, entry IDs, expected hashes,
   destination rules, and requested policy. Applying stale input must fail.
2. **Add source snapshot diff.** Compare two indexes and report added, removed,
   moved, byte-identical, and content-changed entries in terminal and JSON.
3. **Add coverage and replica-health reporting.** Surface only-copy files,
   replica counts, missing/offline sources, and configurable minimum-copy
   targets without claiming unavailable sources are absent.
4. **Add read-only integrity checks.** Verify index completeness, source
   identity, archive fingerprints, directory stability, schema compatibility,
   and optional deep hashes with explicit skipped/inconclusive states.
5. **Generate organize, extract, and dedup plans without applying them.** Make
   dry-run the default, persist reviewable manifests, and keep planning usable
   even when action execution is not installed. The intended surfaces are
   `organize ... --manifest PLAN`, `extract ... --manifest PLAN`, and
   `dedup ... --write-plan PLAN`.

Exit gates:

- All commands have stable JSON fixtures and honest incomplete-data states.
- Plans are deterministic for the same indexes, policy, and tool version.
- A plan must be saved as a regular file and be reviewable before any later
  apply command accepts it; apply does not accept a freshly generated in-memory
  plan or standard input.
- No v1.1 command moves, deletes, overwrites, or extracts user data.

### v1.2 - Controlled directory actions

**Outcome:** reviewed plans can be executed with recovery paths; archives stay
immutable.

Issues:

1. **Build the transactional plan executor.** Accept only a persisted plan,
   revalidate every precondition, use no-clobber filesystem operations, persist
   an append-only journal, and support safe resume after interruption. Applying
   never regenerates or silently changes the reviewed intent.
2. **Execute directory organize plans.** Implement the `YYYY/YYYY-MM` layout,
   `unknown-date/`, deterministic conflict names, dry-run by default, and an
   explicit `organize --execute PLAN` gate. The first release rejects
   cross-filesystem moves; same-filesystem atomic rename plus the journal is the
   supported crash-safety boundary.
3. **Execute copy-only archive extraction plans.** Stream chosen winners to a
   destination-local temporary file, flush it, verify its content hash, and
   atomically promote it without replacing an existing destination. Invoke it
   as `extract --execute PLAN`; never modify the archive.
4. **Apply directory dedup plans through quarantine.** Revalidate each planned
   loser with `dedup --apply-plan PLAN`, move it to a recoverable quarantine
   first, and record enough journal data for `dedup --restore JOURNAL`.
   Reclaiming space is a separate, explicit `dedup --purge-quarantine JOURNAL`
   step with its own preview. Automatically generated actions cover only
   byte-identical files; near-duplicate files require explicit per-file user
   selection. Hardlink replacement is not part of v1.2.
5. **Add keep policies and replica protections.** Support reviewed policies
   including `--keep-policy newest|oldest`, protected/reference roots, and
   minimum replica floors. Policy decisions are recorded in the plan rather
   than recomputed at apply time.

Exit gates:

- Kill-and-resume tests prove execution is idempotent at the action-ID level.
- Changed inputs, stale plans, path aliases, and conflicts fail closed.
- Organize rejects cross-device plans before the first move; extraction proves
  destination-local temp, flush, hash verification, and no-clobber promotion.
- Quarantined files can be restored before purge, purge requires a second
  reviewed command, and near duplicates are never selected automatically.
- No code path opens an archive for write or delete.

### v1.3 - Media formats and scale

**Outcome:** optional format support and larger catalogs arrive without
weakening deterministic results or default portability.

Issues:

1. **Add opt-in HEIC perceptual hashing.** Put `libheif` behind a non-default
   `heif` feature, report runtime codec capabilities, enforce decode limits,
   normalize orientation/colorspace, and version the hash pipeline.
2. **Add deterministic RAW preview hashing.** Prefer embedded full-size
   previews; otherwise use a frozen `rawler` rendering profile. Keep the
   feature optional and record the decoder/hash algorithm versions.
3. **Support thresholds above three with benchmarked MIH.** Compare candidate
   layouts against a brute-force recall oracle and real bucket distributions
   before selecting an index scheme.
4. **Add incremental directory re-index.** Treat full reconciliation as the
   source of truth; use filesystem watchers only as dirty-path accelerators,
   detect overflow, record generations, and expose `--force-full`.
5. **Parallelize federated search deterministically.** Fan search out across
   indexes with bounded concurrency, preserve stable ordering and skip
   semantics, and benchmark memory and file descriptor use.
6. **Add video creation metadata before video fingerprints.** Extract
   well-sourced creation dates first; keep video perceptual hashing as a
   separately benchmarked future capability.

The current four 16-bit exact bands guarantee recall through Hamming distance
3. A naive eight 8-bit exact-band scheme guarantees through distance 7, but
its uniform expected bucket size grows from `N/65536` to `N/256`. Across all
bands, expected candidate comparisons are roughly 512 times higher, not merely
twice as high. Bucket-cap truncation can then silently destroy recall. The
implementation issue must benchmark alternatives such as query-probed 16-bit
bands; the roadmap does not preselect 8x8.

Exit gates:

- HEIC and RAW remain opt-in, pass decode-limit/adversarial fixtures, and record
  every native/algorithm capability needed to compare hashes honestly.
- Threshold expansion matches the brute-force oracle on generated and real
  corpora without bucket-cap recall loss at the published scale target.
- Incremental indexes converge in query results with a forced full
  reconciliation after watcher overflow and interrupted updates.
- Parallel federated search preserves serial result ordering and skipped-source
  semantics while meeting its documented descriptor and memory bounds.
- Video timestamp provenance is visible; video fingerprinting remains outside
  the release unless separately designed and benchmarked.

### v2.0 - Local web UI

**Outcome:** a loopback-first web application renders the same core behavior
without duplicating the indexing or analysis pipeline.

Issues:

1. **Split the workspace into core, CLI, and web crates.** Move logic only when
   the web consumer needs the boundary; keep rendering and transport outside
   core.
2. **Publish `/api/v1` contracts.** Version API schemas separately from index
   schemas and pHash algorithms; use structured errors, request IDs, cursor
   pagination, ETags, and idempotency keys for plan application.
3. **Add a persistent job service.** Model long-running index/check/plan/apply
   jobs with durable state, cancellation, progress events, and restart
   recovery instead of tying work to an HTTP request.
4. **Build the secure loopback-first Axum UI.** Bind to loopback by default.
   Any LAN mode requires authentication plus origin, CSRF, and CORS controls;
   browser input selects registered source IDs rather than arbitrary server
   paths.
5. **Add web end-to-end, packaging, and upgrade tests.** Test CLI/web contract
   parity, interrupted jobs, schema upgrades, browser security boundaries, and
   packaged asset loading.

`report.rs` freezes the dedup report shape, not the entire future web API. The
web milestone therefore extends the contract deliberately instead of treating
the server as rendering-only plumbing.

Exit gates:

- CLI and web contract fixtures agree for every shared operation and schema
  version negotiation is tested across supported upgrades.
- Long-running jobs survive server restart, expose cancellation and progress,
  and never depend on an open browser request.
- Default bind is loopback; LAN tests prove authentication, origin, CSRF, and
  CORS enforcement and reject arbitrary browser-supplied filesystem paths.
- Packaged installs load all assets and pass the end-to-end recovery workflow.

### v2.1 - Remote sources

**Outcome:** S3 and SSH sources behave honestly under latency, change, and
partial failure.

Issues:

1. **Define capability-based source interfaces.** Model list, stat, sequential
   read, optional range read, stable version identity, and reopen semantics.
   `Read` alone is insufficient for remote directories and retries.
2. **Add version-pinned S3 and SSH adapters.** Pin object version/metadata for a
   job, make conditional reads explicit, and compute BLAKE3 rather than treating
   an ETag as a content hash.
3. **Add verified spooling, caching, extraction, and failure tests.** Compressed
   tar streams use full streaming or a verified local spool because remote
   byte ranges cannot randomly address decompressed members. Test disconnects,
   source changes, cache corruption, and resume.

Exit gates:

- Capability negotiation prevents range/retry assumptions a source cannot
  satisfy, and every job pins a stable remote version or fails as inconclusive.
- S3 and SSH fixtures produce the same index and BLAKE3 results as local input;
  ETags are never treated as content hashes.
- Disconnect, version-change, partial-spool, cache-corruption, and resume tests
  fail closed without modifying either the remote source or a prior good cache.

## Dependency gates

1. `v1.0.1` blocks every other milestone.
2. `v1.0.2` blocks action-plan generation and execution.
3. The v1.1 plan contract blocks every v1.2 executor.
4. v1.3 optimizations may be researched earlier but ship only after action
   invariants are stable.
5. The workspace split happens in v2.0, when the second frontend exists.
6. Remote adapters ship after the local core/API/job boundaries are proven.

## Modular boundaries

- **Output safety:** path identity, protected inputs, no-clobber promotion,
  journals, and precondition checks. No feature implements its own weaker copy.
- **Source/index core:** archive and directory enumeration, streaming reads,
  stable source identity, index schemas, and reconciliation.
- **Analysis:** search, diff, coverage, integrity, exact duplicates, near
  candidates, and keep-policy evaluation. It never mutates files.
- **Planning/execution:** immutable manifests are produced by analysis and
  consumed by a separate executor. Apply never silently recomputes intent.
- **Media adapters:** optional HEIC/RAW/video decoders feed normalized metadata
  and versioned hashes into the core pipeline.
- **API/web:** transport and rendering wrap core operations through versioned
  contracts and a durable job model.
- **Remote adapters:** capability-aware implementations feed the source core;
  caching and spooling are explicit, verified layers.

## Research basis

The design was checked against primary project documentation and advisories:

- Multi-index hashing guarantee and partition model:
  [Norouzi, Punjani, and Fleet (CVPR 2012)](https://www.cs.toronto.edu/~fleet/research/Papers/chunkingCVPR12.pdf)
- SQLite FTS5 contentless-table behavior:
  [SQLite FTS5 documentation](https://www.sqlite.org/fts5.html)
- Archive parser safety:
  [`tar` PAX-header advisory GHSA-3pv8-6f4r-ffg2](https://github.com/advisories/GHSA-3pv8-6f4r-ffg2)
- Current error-library advisory:
  [`anyhow` RUSTSEC-2026-0190](https://osv.dev/vulnerability/RUSTSEC-2026-0190)
- Optional media dependencies:
  [`libheif-rs`](https://docs.rs/libheif-rs/latest/libheif_rs/) and
  [`rawler`](https://docs.rs/rawler/latest/rawler/)
- Read-only backup UX precedents:
  [Borg diff](https://borgbackup.readthedocs.io/en/stable/usage/diff.html),
  [Borg check](https://borgbackup.readthedocs.io/en/latest/usage/check.html),
  and [restic restore](https://restic.readthedocs.io/en/stable/050_restore.html)
- Web and remote foundations:
  [Axum](https://docs.rs/axum/latest/axum/) and
  [`object_store`](https://docs.rs/object_store/latest/object_store/)

## GitHub tracking model

Create seven open milestones with no due dates and the exact titles above.
Create one parent tracking issue for each numbered item, assign it to its
milestone, and add `roadmap` plus the smallest relevant labels. Before code work
starts on a parent that spans multiple reviewable changes, create linked child
issues with their own acceptance tests; the parent closes only when every child
and its milestone exit gate are satisfied. Use these added labels:

- `roadmap`
- `safety-critical`
- `release`
- `correctness`
- `performance`
- `privacy`
- `area:indexing`
- `area:actions`
- `area:media`
- `area:api`
- `area:web`
- `area:remote`

Each issue body contains its reason, scope, acceptance checklist, dependencies,
and the archive-immutability invariant where relevant. No issue gets a due date
or is assigned to a person during roadmap creation.

## Explicit non-goals

- Never rewrite or delete an archive.
- Never apply an unreviewed or stale plan.
- Never advertise contentless FTS as encryption or full privacy.
- Never make native HEIC/RAW libraries default dependencies.
- Never assume an object-store ETag is a cryptographic content hash.
- Never ship a larger MIH threshold based only on the mathematical guarantee;
  bucket distribution, recall, and cap behavior require measurements.
