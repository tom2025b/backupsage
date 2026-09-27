# ADR 0011 — Coverage replicas and minimum-copy floors

Date: 2026-09-26 · Status: proposed · Issues: #95, #96, #97 and #98 (children of #40)

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
    `Incomplete`. Rows stay proof of a copy only when the source was
    positively shown present and readable now. That takes both of these:
    - a currency that observed the source (`stat_matches`, `stale` or
      `directory_unverified`; the match is exhaustive, so a new variant
      cannot default to trusted);
    - an actual open for reading of the recorded path (a directory is
      opened and listed). A stat alone is not enough, since an unreadable
      archive can keep the size and mtime its index recorded. Only the
      recorded kind is ever opened: the path is stat'ed first, and
      anything but a regular file (a directory, for a directory source)
      is refused before any open, since a FIFO with no writer would block
      the load forever. The open itself is nonblocking and the opened
      handle is checked again, so a swap between the stat and the open can
      neither hang the load nor pass as the archive.

    Any other source is `Unreachable`: offline, denied, never checked,
    recorded under a lossy or relative path or none at all, replaced by
    another kind of file, or unreadable.
    Its rows are listed as history, but its presence is unknown for every
    group. Its copies are never counted as present, and content it alone
    holds reads as an unknown lower bound (`AtLeast(0)`), never as zero
    copies. An index the loader refuses (missing, pending journal, WAL
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
    argument order and no registry status. The same index given twice (the
    same canonical path) is refused, and so is any index whose `index_uuid`
    another input already has: a copied index is the same evidence under
    another name, never a second replica.
13. Sources are handled in source-id order whatever order the registry
    lists them in. The master stores no protected/reference role yet, so
    the caller passes protected source ids to the floors step.

**Command and report (#98).** `backupsage coverage` loads as in rules 8–13
and renders the floors result.

14. The JSON report is `version` 1, additive-only. Every list is sorted by
    the report itself on an explicit key, whatever order the engines or the
    registry handed it: sources and per-group presence by source id; groups
    and out-of-scope groups by content hash; copies, aliases, unknown-content
    rows and excluded rows by (source id, raw path bytes, file id). A
    master assigns source ids in registration order, so registering the
    same sources in another order changes the ids and nothing else. The
    terminal text lists the same rows in the same order: the groups that
    do not meet the floor are one list in content-hash order, each line
    naming its verdict (below floor or inconclusive), never regrouped by
    verdict. It lists every unknown-content row and counts the groups that
    meet the floor.
15. Every summary total is counted from the rows the report emits, after
    filtering. `coverage_state` is `complete` only when every source is a
    complete, `ok` source, no emitted group is inconclusive and no emitted
    row's content is unknown; anything else is `inconclusive` and exits 2.
    Content below the floor is a finding and exits 0 when all else is
    known.
16. Scope filters follow `dedup`. `--archive` chooses the sources the
    question is about. `--min-size` and `--include-empty` put content out
    of scope by size (rule 5). `--kind` (the indexer's recorded kind of
    each row, `f.kind = ?`), `--ext` (ASCII case folded, `lower(path)
    LIKE '%.ext'`) and `--path-glob` (SQLite `GLOB` over the display path,
    evaluated by SQLite) choose which content is reported, never which
    copies count. They combine on one row, as dedup's `WHERE` clause does.
    Content is reported, with every copy, when any of its copies or
    aliases matches; unknown-content and excluded rows are reported when
    they match themselves. `dedup` accepts any `--kind` and finds nothing
    for a misspelt one; `coverage` refuses a kind outside dedup's
    documented set (`image`, `raw`, `video`, `text`, `binary`), since
    "nothing below the floor" would read as complete coverage. Filtering rows before grouping
    would make a renamed copy vanish and could turn a safe group into a
    false only-copy.
17. `--protected` names protected/reference sources (rule 7) by id, label
    or index path, among the chosen sources. The master stores no such role
    yet.
18. Terminal text passes every untrusted string (labels, paths, source
    paths, note details, filter values) through the central sanitizer;
    a lossy display path is followed by its raw bytes in hex. `-o` writes
    through the output-safety boundary (ADR 0001): a new file only, never
    over an existing one, and never onto or inside anything the run read:
    the master, every index and its sidecars, every source archive, and
    every directory source's whole tree. Inputs are protected by name as
    well as by identity, so a name is refused even when nothing exists
    there: a missing index, an unplugged archive's recorded path, and the
    `-wal`, `-shm` and `-journal` names beside the master and every index,
    under both the given spelling and the file it resolves to (SQLite
    names sidecars after the resolved file, and the reader checks both).
    A report written there would be taken for that input, or would make
    the index look in use.

## Consequences

Rules 8–13 map every registry state; a new registry status must be added to
rule 11 before the loader will accept it. A stored protected/reference role
still needs a master change; until then `--protected` supplies it per run. Consumers
must render `inconclusive` distinctly from `below_floor` and must show
untrusted copies.

— max-cloud
