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

### Read-only loading

Diffing two indexes must never modify either one. `diff_input::open_index_readonly`
opens with `SQLITE_OPEN_READ_ONLY` and the URI parameters `mode=ro&immutable=1`.
Measured against the bundled SQLite 3.51.3:

| Open | WAL-mode index, no sidecar | Pending `-wal` | Hot `-journal` |
|---|---|---|---|
| read-only flag, with or without `mode=ro` | reads correctly, but **creates** `-shm` and `-wal` beside the index | reads, creates `-shm` | refuses |
| `immutable=1` | reads correctly, creates nothing | creates nothing, but **ignores** the committed frames | creates nothing, but reads **torn** pages |

So an index with a `-wal` or `-journal` sidecar is `unavailable`
(`pending_journal`): the main file alone is not the database, and only a
recovering (writing) open could make it one. The check runs before opening and
again after reading. Every other index is opened immutable, which never creates,
locks, recovers or writes anything. A missing path is never opened, so it is
never created. The only other file-system access is a `stat` of the recorded
source, for currency. Residual: an outside writer changing an index in place
during the read is not detected. BackupSage itself only ever replaces an index by
renaming a finished file over it.

`searcher::open_index` (used by `search`, `top` and `inspect`) still uses the
plain read-only open, so it would create sidecars beside a WAL-mode index.
Indexes BackupSage writes are folded back to rollback-journal mode before they
are promoted, so only converted or externally modified indexes are affected. It is
outside #93's scope and left for a follow-up.

### Input health

| State | When |
|---|---|
| `unavailable` | missing, unreadable or not a regular file; not a BackupSage index; pending journal; a malformed row (for example a hash that is not 32 bytes) |
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
incomplete, pre-v3, stale, offline, relative-source and pending-journal inputs,
plus every exit code. Each invariant test was shown to fail when its mechanism is
removed, and again when it is weakened; the table is in this change's pull
request. Two tests guard read-only behaviour:
- **Writable open:** `readonly_open_is_not_writable_and_creates_no_sidecar`
  fails if the connection reports writable, if a write succeeds, or if opening
  changes the directory.
- **Any mutation:** `diff_never_modifies_or_creates_anything_beside_its_inputs`
  runs every layout both ways and in both output modes. It fails if any file's
  bytes, size, inode, mtime or ctime change, or if the directory gains or loses
  an entry.

last_edited_by: max-cloud

**Signed:** max-cloud (Claude) · 2026-09-26T13:20:53-04:00
