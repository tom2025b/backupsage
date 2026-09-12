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
   produce `inconclusive` with a reason, rather than equality. `PAX_UNPARSED`
   makes metadata unknown. FTS truncation and image-decoding flags do not weaken
   a full file hash. A supported sparse entry's logical-stream hash can establish
   equality; unsupported sparse entries remain unknown.
3. Hardlink and symlink byte comparisons are inconclusive. In particular, copied
   v3 hardlink hashes used display-name lookup and link header sizes need not be
   target sizes. This engine does not independently resolve those links. Their
   raw link targets remain reviewable in the report.
4. An unmatched path is `moved` only when both snapshots are compatible and
   complete, the full hash appears exactly once in **all** rows on each side,
   sizes agree, and both paths are unmatched effective entries. Matched paths
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
