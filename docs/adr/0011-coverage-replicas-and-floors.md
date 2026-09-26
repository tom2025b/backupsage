# ADR 0011 — Coverage replicas and minimum-copy floors

Date: 2026-09-26 · Status: proposed · Issues: #95 and #96 (children of #40)

## Context

#40 asks which content has only one trustworthy copy, without treating unknown
data as absence. #95 added the pure grouping engine (`coverage::group`); #96 adds
the floor classification (`floors::evaluate`) on top of it. Loading real sources
(#97) and rendering (#98) come later. Both modules take in-memory input and
perform no filesystem, index or master I/O.

## Decision

**Grouping (#95).** Content groups key on the full BLAKE3 hash of effective
regular-file rows; replicas are distinct sources. Raw path bytes decide the
effective row (greatest file ID wins); the stored display-text `SHADOWED` flag
is not used. A hash under `READ_ERROR` or `PAX_UNPARSED` proves nothing, and
such rows are unknown content. Hardlinks are aliases of a same-source copy or
unmatched exclusions, never copies. Per source, presence is `present`,
`absent` or `unknown`. `absent` needs a complete, hashed source whose unhashed
rows are all ruled out by a trustworthy, differing size. Any `unknown` makes
the replica count a lower bound (`AtLeast`).

**Floors (#96).**

1. The floor is at least 1 (default 2); a floor of 0 is refused. A floor above
   the number of sources is accepted and is simply never met.
2. Only copies in `ok` and `incomplete` sources count toward the floor. Both
   hashed the copy from bytes nothing has since flagged. A stale index, a
   missing index database or a missing source leaves the copy unverified, so
   copies in `stale-index`, `db-missing` and `archive-missing` sources are
   listed with their label and status but not counted.
3. A group meets the floor when its trusted replicas reach it, even under a
   lower-bound count. Below the floor, any `unknown` presence makes the group
   `inconclusive`, whatever that source's status: an unknown is never quietly
   discounted into a below-floor alarm. Only a fully known count is
   `below_floor`. Unknown never reads as below the floor or as met.
4. `only_copy` marks exactly one trusted replica with no unknown presence. It
   is independent of the verdict, so it also appears when a floor of 1 is met.
5. Content of a known length that is empty, or smaller than `min_size`, is out
   of scope. It is listed with a reason and its copies, never silently
   dropped; those copies carry the same label, status, counts and protected
   marks as in-scope copies.
   An unknown length is never excluded by size. Hardlink aliases are listed
   with their group, in scope or not. Unknown-content rows keep their path,
   size and reason, and engine row exclusions (shadowed, symlink, unmatched
   hardlink) keep theirs. The summary counts all of them from those lists.
6. Group order follows the engine (content hash); copy order follows the
   engine (source id, raw path, file id). Totals derive from the emitted rows.
7. **Protected/reference copies (Tom's decision, 2026-09-26).** Protected or
   reference designation is caller-supplied source metadata, a role that is
   independent of the source's status. A protected copy counts toward the
   floor exactly like any other trusted copy: 1 ordinary + 1 protected copy
   meets a floor of 2. The report shows protected copies separately: each
   copy is marked `protected`, each group has `protected_replicas` (trusted
   replicas on protected sources, also included in `trusted_replicas`), and
   the summary totals them. Rule 2 is unchanged: a protected source whose
   status is `stale-index`, `db-missing` or `archive-missing` is shown and
   marked but not counted. The designation survives every path: copies of
   out-of-scope content are marked too, and the report lists every source
   with its label, status and protected designation, even when it holds no
   copies. Out-of-scope content never enters the protected totals.

**Loading (#97).** `coverage_input` builds coverage input from real sources
and writes nothing beside any of them.

8. The master is read only while idle. Any `-wal`, `-shm` or `-journal` beside
   it (another command using it, or changes not yet folded into the file)
   refuses the load, as does a symlink, a second hard link, or a file that is
   not a signed master. An idle master is opened `mode=ro&immutable=1`, which
   reads the file alone and creates nothing. The same guards run again after
   the read, and any change to the file (inode, links, size, mtime, ctime) or
   any new sidecar fails the load. Only `Master::list` is called on it.
9. Each source's rows come from its own index, read by the #101 loader
   (`diff_input::load_index`). The master's replica rows are never used as
   evidence: only the index can confirm they are current.
10. Evidence follows what reading the index showed. A complete index is
    `Complete` (`NoContentHashes` when metadata-only). An incomplete index is
    `Incomplete`. An index the loader refuses (missing, pending journal, WAL
    mode, multiply linked, busy, changed during the read, unreadable,
    including a pre-v1.0.1 layout without `path_raw`) or finds incompatible
    (another schema, hash algorithm or identity) is `Unavailable`, with the
    loader's reason and no rows. An unknown entry type, or a metadata-only
    index whose row carries a hash, also makes it `Unavailable`: refused
    rather than guessed.
11. Trust starts from the same reading: an unusable index is `db-missing`, an
    offline or unreadable source is `archive-missing`, a source whose stat
    differs from the index is `stale-index`, an incomplete index is
    `incomplete`, and anything else is `ok`. The registry status, which
    records what `master sync`/`verify` last found (a deep verify sees what a
    stat cannot), can only lower that trust, never raise it:

    | Registry status | Effect |
    |---|---|
    | `ok` | none |
    | `incomplete` | at most `incomplete` (still counts) |
    | `stale-index` | `stale-index` (not counted) |
    | `db-missing` | `db-missing` (not counted), even if the index reads now |
    | `archive-missing` | `archive-missing` (not counted) |
    | `stale-replica`, `v2-limited` | none: they describe the replica, which is not used; a v2 index is itself unavailable |
    | anything else | the load is refused |

12. Ad-hoc `--db` indexes load the same way, with source ids 1, 2, … in
    argument order and no registry status. The same index given twice is
    refused, since it would count as two replicas.
13. Sources are handled in source-id order whatever order the registry
    lists them in. The master stores no protected/reference role yet, so
    the caller passes protected source ids to the floors step.

## Open decisions

- **Kind and path scope filters.** #96 lists kind and path exclusions. Coverage
  rows carry no kind, and #98 defines the command's filters "consistent with
  `dedup`". These filters are left to #98; only size and empty-content scope
  exists here.

## Consequences

Rules 8–13 map every registry state; a new registry status must be added to
rule 11 before the loader will accept it. A stored protected/reference role
needs a master change and belongs with #98's command options. Consumers
must render `inconclusive` distinctly from `below_floor` and must show
untrusted copies.

— max-cloud
