# ADR 0010 — Historical snapshot diff evidence

Date: 2026-09-12 · Status: accepted · Issue: #92 (child of #13)

## Context

v1.1 needs source diffs before action planning (#16 explicitly depends on diff,
coverage and keeper semantics). The frozen plan contract (#12) already exists.
The diff algorithm and report can be tested independently of index loading and
CLI rendering, following the #88/#89 sequence. #92 introduces the pure engine;
#93 adds the read-only index adapter, source status probes, command and end-to-end
fixtures. Neither child closes #13 alone.

## Decision

`diff::compare` takes two concrete in-memory snapshots and returns a typed,
version-1 report. It has no filesystem I/O, source interfaces, index discovery,
master writes or action executor. The caller supplies completion, schema, hash
algorithm, index UUID and live-source currency evidence. Missing or incompatible
schema/hash/identity evidence cannot silently become a compatible empty index.
Unavailable snapshots cannot carry rows; a future cached-index adapter must
explicitly qualify usable historical rows separately from live availability.

The effective namespace is keyed by exact raw path bytes. The greatest file ID
wins repeated occurrences of the same raw path within an index. Earlier rows
remain in the report's exclusions with their side and identity. The stored v3
`SHADOWED` flag is retained as evidence but does not decide namespace membership:
v3 computed it from display paths, which can collapse distinct non-UTF-8 names.
No existing index is rewritten to correct those flags.

The comparison rules, applied in order, are:

1. Pair effective entries at exactly the same raw path. Cross-index file IDs
   never establish correspondence. For regular files with trustworthy BLAKE3
   hashes, unequal hashes mean `content_changed`. Equal hashes and sizes plus
   known mtime and mode mean `byte_identical` when those metadata fields match,
   or `metadata_only_changed` otherwise. These names refer to indexed metadata
   only: v3 has no complete ownership, ACL or xattr evidence.
2. Missing hashes, read errors, inconsistent equal-hash sizes or missing metadata
   produce `inconclusive` with a reason, rather than equality. Equal trusted
   hashes with unequal sizes use `inconsistent_content_evidence` (added by #93):
   the evidence is present but contradicts itself, which `missing_content_evidence`
   would misdescribe. `PAX_UNPARSED`
   makes metadata unknown. Its content hash is also untrusted because the indexer's
   crafted PAX residual can hide sparse records and hash condensed fragments, so
   it can prove neither equality, a content change, nor a move.
   FTS truncation and image-decoding flags do not weaken
   a full file hash. A supported sparse entry's logical-stream hash can establish
   equality; unsupported sparse entries remain unknown.
3. Hardlink and symlink byte comparisons are inconclusive. In particular, copied
   v3 hardlink hashes used display-name lookup and link header sizes need not be
   target sizes. This engine does not independently resolve those links. Their
   raw link targets remain reviewable in the report.
4. An unmatched path is `moved` only when both snapshots are compatible and
   complete, the full hash appears exactly once in **all** rows on each side,
   sizes agree, both paths are unmatched effective entries, and the content is
   not empty: every empty file shares one hash, so size 0 carries no identity
   evidence and never establishes a move. Matched paths
   and shadowed rows participate in multiplicity. Any unknown content anywhere
   prevents proving global uniqueness and therefore suppresses moves. This is
   deliberately conservative, including for snapshots containing links.
   `unique_content_correspondence` describes an inference from snapshot bytes,
   not proof of a filesystem rename or authorization for an action.
5. Remaining unmatched paths are `added`/`removed` only when the opposite
   snapshot is compatible and complete. Otherwise they are inconclusive.
   An entry observed in an incomplete index may still be absent from a complete
   peer. Paired comparisons in partial indexes describe the observed entries;
   the overall comparison remains incomplete because later rows may be missing.

Snapshot completion and live-source currency are independent report fields.
An offline or stale source does not invalidate the bytes captured in a completed
historical index. A completed comparison does **not** verify current source bytes.
`stat_matches`, `directory_unverified` and `not_checked` deliberately do not claim
verification. Report comparison state is unavailable if either index is
unavailable, otherwise incompatible if either binding is unsupported, otherwise
incomplete for partial input or inconclusive rows, otherwise complete. The full
input states are always retained, including when more than one caveat applies.

Report paths are derived display text plus always-present lowercase hex raw
bytes. Frontends must sanitize terminal text. Changes sort by old raw path (new
raw path for additions), then file ID and after-side ID; exclusions sort by side,
raw path and ID. Summaries derive from emitted rows. JSON uses typed struct
serialization and a trailing newline, with no timestamps or environment fields.
Invalid or repeated per-index IDs are errors; silently dropping such rows could
invent uniqueness.

## Validation and consequences

`tests/diff.rs` pins complete, incomplete, unavailable, incompatible, offline/stale,
raw-path/shadow and sparse/link/unknown reports as exact JSON bytes. Additional
assertions cover all six classifications, both diff directions, metadata
uncertainty, input permutations, hash multiplicity and malformed IDs. These are
engine fixtures over explicit evidence, not claims of end-to-end index or CLI
coverage; that remains #93's acceptance gate.

The future adapter must preserve all raw rows and map index problems explicitly.
It must not use existing master verification as a shortcut because that method
persists status updates. The source abstraction planned for v2.1 (#33), Borg
modules and v1.2 execution remain outside this change.

The milestone's required two-arm mutation proof at the plan persistence/write
boundary belongs to #16, once that command and its protected-data tests exist.
Algorithm fixtures here do not substitute for that proof. #12's stale roadmap
checkbox is corrected now; all four remaining milestone parents stay open.

— codex

**Signed:** codex · 2026-09-26T12:14:55-04:00

## Addendum (#93): the `diff` command

Date: 2026-09-26 · Status: accepted · Issue: #93 (child of #13)

`backupsage diff BEFORE_DB AFTER_DB [--json]` loads both indexes through
`diff_input` and compares them with the unchanged engine rules above.

### Read-only and consistent loading

Diffing two indexes must never modify either one, and must never call a
mixed snapshot complete. `diff_input` reads each index with
`SQLITE_OPEN_READ_ONLY` and `mode=ro&readonly_shm=1`, inside **one read
transaction**. Its shared lock makes a SQLite writer's commit fail with
"database is locked" (or wait for the read to end), so the metadata and every
row come from one snapshot.

The first revision of #93 used `immutable=1` instead. The #93 cross-family
review reproduced two P1s against it, and this section replaces that design:
- `immutable=1` disables locking and change detection. A writer committing
  mid-read yielded 2,108 old and 1,892 new rows, reported `complete` with
  exit 0.
- A hardlinked second name hid a pending `-wal` from the sidecar check.

Measured against the bundled SQLite 3.51.3:

| Open | Rollback-journal index | WAL-mode index, no sidecar | Pending `-wal` | Hot `-journal` |
|---|---|---|---|---|
| read-only (± `mode=ro`) | reads; creates nothing; its read transaction blocks a writer's commit | **creates** `-shm` and `-wal` | creates `-shm` | refuses |
| `+ readonly_shm=1` | as above | creates an empty `-wal`, then refuses | — | — |
| `immutable=1` | reads with **no locking**: a mid-read commit yields a mixed snapshot | reads, creates nothing | **ignores** the committed frames | reads **torn** pages |

So every layout SQLite could only read by writing beside the index, or by
trusting a name that hides its journal, is `unavailable` before SQLite opens
anything:
- **`pending_journal`:** a `-wal` or `-journal` exists beside the given name or
  the symlink-resolved one. Only a recovering, writing open could read it.
- **`wal_mode_index`:** header bytes 18 or 19 are 2. Reading would create
  `-shm`/`-wal`. BackupSage writes rollback-journal indexes, since the indexer
  folds WAL before promotion, so this only refuses converted or externally
  written indexes.
- **`index_multiply_linked`:** the file has more than one hard link. SQLite
  names sidecars after the path it opens, so a journal beside another name is
  invisible from this one, and no check can list the other names. This also
  refuses indexes inside hardlink-deduplicated backup trees (for example
  `cp -al` or rsnapshot). Copying the index gives it a single link.
- **`index_busy`:** a writer held the index for more than 2 s.

After the read, a before/after stamp of the file (device, inode, link count,
size, mtime and ctime) must match, and no sidecar may have appeared. Otherwise
the index is `index_changed_during_read`. This is an extra fail-closed layer
for writers that ignore SQLite locking, not the guarantee. A missing path is
never opened, so it is never created. The only other file-system access is a
`stat` of the recorded source, for currency.

**Scope.** BackupSage's own index publication renames a finished file over
the old one. A diff that races it reads one coherent version: the review's run
returned all 4,000 old rows. A torn read needs an **in-place** writer, such as
another SQLite client, a sync tool or a raw copy over the file. It is still a
P1, because the tool must never call a mixed snapshot complete.

**Residual.** A writer that converts the index to WAL mode in the instant
between the header check and the open could make SQLite create an empty
`-wal` before it refuses the read (`readonly_shm=1` rules out a `-shm`). The
read then fails as unavailable.

`searcher::open_index` (used by `search`, `top` and `inspect`) still uses a plain
read-only open. It is outside #93's scope and left for a follow-up.

### Input health

| State | When |
|---|---|
| `unavailable` | missing, unreadable or not a regular file; not a BackupSage index; pending journal; WAL mode; more than one hard link; busy; changed during the read; a malformed row (for example a hash that is not 32 bytes) |
| `incompatible` | `schema_version` is not 3, `hash_algo` is not `blake3`, or there is no `index_uuid`. Rows are read only from a v3 layout. |
| `incomplete` | `completed` is not `1` |
| `complete` | otherwise |

Each input lists `notes` (`code` plus `detail`) saying why, including every
applicable caveat. A `metadata-only` index is noted, and its rows stay
inconclusive under rule 2. Source currency is a `stat` of the recorded absolute
path:
- A tar source whose size and mtime match `archive_size`/`archive_mtime_unix` is
  `stat_matches`. It was not re-hashed, so it is not called verified. Otherwise
  it is `stale`.
- A missing source is `offline`, and a permission error is `denied`.
- A directory is `directory_unverified`, because its own stat says nothing about
  the files inside it.
- A relative, lossy or unrecorded path is `not_checked`.

Currency never changes the comparison state or the exit code.

### Output and exit codes

`--json` emits the engine's version-1 report with two additive fields: `inputs`
(per-side health) and `move_inference`. Fixtures in `tests/fixtures/diff_cli/`
pin JSON and terminal output byte for byte, with temp paths and index UUIDs
normalized. The terminal rendering prints plain, sanitized lines. Byte-identical
rows are counted, not listed, and a non-UTF-8 path also shows its raw bytes.

The exit code is `0` when `comparison_state` is `complete`, and `2` otherwise
(unavailable, incompatible, incomplete, or any inconclusive row). It is `1` when
the engine refuses contradictory rows, such as a non-positive file ID.

### Carry-forward decisions from the #92 review

1. **Say why moves vanished.** `move_inference.blockers` lists, per side, each
   whole-snapshot cause (`snapshot_unavailable`, `snapshot_incompatible`,
   `snapshot_incomplete`) and each row cause with its count. Row causes are
   assigned in the engine's own order: `hardlink`, `symlink`,
   `unsupported_entry_type`, `read_error`, `pax_unparsed`, `unsupported_sparse`,
   `not_hashed`. The terminal explains `pax_unparsed` explicitly: legal pax
   values containing newlines (xattrs, names) produce it too, so a snapshot with
   one such row shows why it has no moves.
2. **One symlink disables all moves: kept, deliberately.** Narrowing rule 4 is
   sound for symlinks. A symlink row holds no bytes, and a target inside the
   snapshot is indexed as its own row. It is not sound for hardlinks, which alias
   bytes. The change still belongs in the engine and its move-safety mutation
   tests, not in this CLI change, and #93 may touch the engine only for item 3.
   The cost is now visible: the output names symlinks as the blocker. Revisit it
   as its own engine change if symlink-bearing snapshots make move inference
   rare in practice.
3. **`inconsistent_content_evidence`: added** (see rule 2), before the CLI makes
   the reason names a published contract.
4. **The SPARSE invariant is preserved.** The loader takes every field from its
   own row, with no joins and no backfill. A NULL `content_hash` stays `None`,
   so an unsupported PAX-sparse row can never borrow a hash, including the
   hash of an earlier regular file that it shadows (the #65 shape). Old-GNU
   sparse rows keep their logical-stream hash.

### Validation

`tests/diff_cli.rs` runs the real binary over tar and directory corpora that
include duplicate paths, hardlinks, symlinks, PAX-sparse and old-GNU sparse
entries, unparsed pax metadata and non-UTF-8 names. It covers unavailable,
incomplete, pre-v3, stale, offline, relative-source, pending-journal, WAL-mode
and multiply-linked inputs, plus every exit code. Each invariant test was shown
to fail when its mechanism is removed, and again when it is weakened; the table
is in this change's pull request. Among them:
- **Writable open:** `readonly_open_is_not_writable_and_creates_no_sidecar`.
- **Any mutation:** `diff_never_modifies_or_creates_anything_beside_its_inputs`
  checks bytes, size, inode, mtime, ctime and directory entries.
- **Consistency:** `sqlite_writer_cannot_commit_in_the_middle_of_a_read` has a
  writer commit between two rows and between the metadata and the rows, through
  a test-only hook. `in_place_write_that_ignores_locking_makes_the_index_unavailable`
  overwrites the file in place mid-read.
- **Hard links:** `hardlinked_index_is_unavailable_because_its_journal_may_hide_elsewhere`.

**Signed:** max-cloud (Claude) · 2026-09-26T13:52:04-04:00

## Addendum (#102): one locked loader for every command

Date: 2026-09-26 · Status: accepted · Issue: #102

The #93 loader now lives in `index_read`, as `LockedIndex`, and every index
read uses it: `search`, `top`, `inspect`, federated `search --all`, master
registration (`read_identity`) and the re-index replace check
(`assert_replaceable_index`). `diff_input` re-exports `open_index_readonly`,
`InputNote`, `NoteCode`, `ReadPoint` and `set_mid_read_hook` under their #93
names, and `load_index` is unchanged. `searcher::open_index` now returns the
`LockedIndex` handle, which dereferences to the connection.

Each caller must follow the same rules:
- **Read under one transaction.** The handle holds a read transaction from
  `LockedIndex::open` until `finish` or drop.
- **Show results only after a clean `finish`.** `finish` (or
  `searcher::finish_index`) must succeed first: no new sidecar, and an
  unchanged file stamp.
  - `search`, `top` and `inspect` finish before printing. `inspect` renders
    into a string first.
  - Federated search drops an archive whose read was not coherent.
- **Refused layouts fail closed with their reason.**
  - `search`, `top` and `inspect` exit 1 with `cannot read index '…' safely —
    <code>: <detail>`. The missing-index and not-an-index messages keep their
    v1.0 wording, which the contract fixtures pin.
  - Federated search lists each refused archive in `skipped` as
    `refused: <code>: <detail>`. A missing file stays `unreachable: …`.
- **Master registration replicates the snapshot it identified.**
  `read_identity` returns the lock-holding handle. `master add` keeps it
  alive while `ATTACH … mode=ro` copies the rows, so a writer cannot commit
  between the identity read and the replication. `add` itself is outside
  #102's paths, so its stamp is not compared there; the lock is the
  guarantee.
- **The replace check stays fail-safe.** An existing index the loader
  refuses, or one that changed while it was checked, is not replaceable.
  With `--index` the command fails. Without it, the indexer's existing
  fallback (`./<name>.db` in the working directory, pinned by
  `cwd_fallback_refuses_foreign_db`) applies, and its warning names the
  reason. The refused file is never touched.
- **Discovery never writes.** The last-resort probe of `*.db` files in the
  working directory uses the locked loader, so a refused candidate is not
  adopted.

Gaps measured but outside #102's paths:
- **`resolve_db_arg` (`master.rs:360`):** a plain read-only open that runs
  before `read_identity`. On a WAL-mode index it creates `-shm`/`-wal`, and the
  leftover `-wal` then makes every command refuse that index as
  `pending_journal`. The fix is a one-line switch to `LockedIndex`.
- **`master sync`:** it records any `read_identity` refusal as `db-missing`,
  with the reason in its action text.

`tests/locked_reads.rs` pins these rules. Each mechanism was shown to fail
when removed, and again when weakened; the table is in #102's pull request.

last_edited_by: max-cloud

**Signed:** max-cloud (Claude) · 2026-09-26T14:37:26-04:00
