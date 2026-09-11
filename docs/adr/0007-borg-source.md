# ADR 0007 — Borg sources require immutable snapshots and sealed reads

Date: 2026-09-11 · Status: accepted (Tom approved 2026-09-11) · Milestone: unassigned · Issues: #79, #80

## Context

BackupSage currently dispatches every directory to `index_dir`
(`src/indexer.rs::run_index`). A Borg repository is a directory, so
ordinary indexing can silently index encrypted storage segments. A distinct
command alone does not prevent that mistake. This repeats the failure class
documented in `src/format.rs`: accepting an unsuitable source and producing
a plausible, incorrect index.

The existing tar/directory front ends feed `process_reader`, then
`IndexRun` and `store`. ADRs 0001–0005 establish output protection,
lossless identity, tested public contracts, conservative dedup classification,
and content-mode propagation. Borg must participate in those contracts.

The installed binary is Borg 1.4.4. Its CLI and Python implementation were
inspected, and only a newly created temporary repository was exercised.
Two constraints are stronger than the obvious list/extract adapter suggests:

- Read verbs are not a filesystem write boundary. Repository opening takes
  locks; transaction recovery can write indexes; corrupt-index recovery
  unlinks an index before trying recovery. Security/cache state can also be
  created or updated outside the repository.
- JSON Lines replaces non-UTF-8 path/link bytes with question marks. Even
  text `{bpath}{NUL}` loses bytes in this installed CLI's stdout wrapper.

Thus directly indexing live writable repositories cannot satisfy the current
README promise that only BackupSage indexes and its master are written.
A lock-only exception would not resolve the recovery-write problem.

## Decision

### 1. How users select a repository and archive

Use a distinct, local-only command:

```text
backupsage index-borg /operator-provided/immutable-snapshot \
  --archive daily-2026-09-11 --index /private/indexes/daily.db \
  --mode metadata-only
```

This is the decided BackupSage syntax, not an implemented command or a command
to run against a live repository. Require exactly one literal archive and
an explicit output path. No archive means a usage error before spawning
Borg or creating an index; never implicitly index every archive. No URI,
SSH, glob, latest-archive shortcut, implicit registration, or all-archives
mode in the first slice. This new command defaults to metadata-only;
`--mode full` or `--mode search-only` explicitly opts into content reads
and disclosure. Existing tar/directory defaults do not change.

Before ordinary directory dispatch, perform a bounded structural probe:
a regular `config` with a `[repository]` section, version and 32-byte
hex ID, plus `data/`, is a Borg candidate regardless of its name.
A positive or suspicious/inconclusive probe refuses ordinary indexing,
with the explicit-command guidance. Incomplete, inaccessible, symlinked,
or unsupported-version candidates must not fall back to directory walking.
Probe canonical ancestors too, so passing `repo/data` or a segment is
refused. Check nested directories before descending in both the pre-count
and actual walk; discovering one aborts and preserves any previous index.
A directory merely named “borg” remains ordinary. No override in this slice.

### 2. Passphrase acquisition and read-only authority

Use inherited `BORG_PASSCOMMAND` for encrypted repositories, following the
existing systemd `LoadCredential` pattern in borg-backup-mcp. The environment
holds a helper command and credential path, never the passphrase. The helper
must retrieve the secret through its output; embedding a secret in its
arguments is outside the supported contract. No BackupSage `--passphrase`
or `--passcommand` option, secret-valued environment fallback, prompt, or
SQLite credential storage. Reject conflicting/secret Borg passphrase
variables without printing their values. Missing or failed authentication
fails closed. An unencrypted repository may omit the helper.

The helper is trusted executable configuration, not data supplied by an
archive or index. Do not inherit an arbitrary child environment, raw stderr
logging, or interactive TTY behavior from the reference project's wrapper.
Use an audited environment, fixed Borg executable, no shell, closed
unneeded file descriptors, null stdin, no controlling TTY, bounded streams,
and process-group cancellation. Suppress raw helper/Borg errors: escaping
terminal control characters cannot redact a secret.

**Repository access requires an operator-provided, immutable point-in-time
snapshot exposed read-only to the entire Borg/helper process tree.**
**BackupSage must never be pointed at a live Borg repository that a backup
timer writes to.** This prohibition also applies through aliases or a
read-only view of that live repository.

**Read-only must be enforced at the filesystem level: a read-only mount
of the snapshot or filesystem-enforced immutable snapshot must refuse
repository writes.** Borg has no true read-only mode; its read paths can
attempt index-recovery writes. Convention, command allowlists and selecting
apparently safe verbs do not provide this guarantee. If the filesystem
cannot refuse the write, the guarantee does not exist and indexing must
refuse.

Do not create snapshots, copy a live repository, change mounts, or manipulate
backup timers inside BackupSage. A read-only view of a still-changing live
repository is insufficient. Pin the snapshot target so path replacement
cannot redirect child operations. Permit fixed `--bypass-lock` only inside
this validated context; it is not a user-supplied bypass switch. If the
platform cannot enforce the boundary, refuse.

Technical validation of the filesystem enforcement belongs to
[implementation issue #80](https://github.com/tom2025b/backupsage/issues/80),
not this design round. The implementation must establish the snapshot's
stable identity, pin the same object for child access and verify that the
filesystem refuses writes through every visible alias. A
`VerifiedImmutableSnapshot` capability must have no unchecked path/string
constructor. Reading mount flags or comparing config hashes alone is not
proof of denied writes or an immutable point-in-time source. Adapter
enablement requires the implementation tests in #80; this accepted design
does not claim those tests have passed. Recovery attempts on read-only
storage fail the build; no repair fallback is allowed.

A private sealed operation type represents only repository `list --json`,
archive `list --json-lines` with a fixed format, and single-file
`extract --stdout`. Omit `info` in the first slice because it loads the
Borg cache unnecessarily. No arbitrary verb, additional argument, format,
environment entry, shell command, or extraction without stdout is constructible.
Reuse the reference project's construction pattern, not its Create variant
or unbounded output capture. An allowlist and OS write prevention are both
necessary.

The closed 1.4.4 argv profiles are below. R is the pinned snapshot locator,
A its validated literal archive name, and P exactly one validated regular
file path. F is exactly
`{type}{mode}{uid}{gid}{size}{isomtime}{archiveid}{archivename}`.
These fixed flags are builder-owned, not an argument-extension interface.

| Operation | Arguments after the fixed `/usr/bin/borg` executable |
|---|---|
| Repository metadata | `list --json --bypass-lock -- R` |
| Archive entries | `list --json-lines --consider-part-files --bypass-lock --format F -- R::A` |
| One regular file | `extract --stdout --consider-part-files --bypass-lock -- R::A pf:P` |

The child environment is rebuilt from empty: PATH=/usr/bin:/bin,
LC_ALL=C.UTF-8, TZ=UTC, and fixed BORG_BASE_DIR, BORG_CACHE_DIR,
BORG_SECURITY_DIR, BORG_KEYS_DIR within the validated private profile.
Only a vetted BORG_PASSCOMMAND may be copied from the caller. Keys are
exposed read-only; cache/security/base metadata state is separately private
and writable under the approved state exception. Every other inherited
variable is absent, including unknown BORG_* keys. Borg itself performs
passcommand shlex splitting and executes without a shell; BackupSage must
not add another parser/executor. Support only helpers compatible with this
restricted profile. Never copy the reference project's operation variants.

Fix `--consider-part-files` on both item listing and extraction so the
adapter never silently omits part items. Checkpoint archives are outside
initial selection. Validate all included items by the same rules; test a
real partial-item fixture before claiming this profile is complete.

Borg security metadata must remain persistent enough to preserve rollback
and encryption checks. Use a private, mode-0700 BackupSage-owned Borg
state directory outside sources, with mode-0600 files; no content cache or
plaintext extraction files. Keep it isolated from daily-backup state, and
do not auto-accept relocation, unknown-unencrypted, or security warnings.
This state exception includes base/cache metadata, directory tags and
security records, not just one passphrase-related file. Unknown unencrypted
repositories or relocated snapshots need a separate operator-managed trust
initialization before noninteractive indexing; suppressing the prompt must
not silently accept it.

**Tom approved snapshot-only support for v1 and private BORG_BASE_DIR,
BORG_CACHE_DIR and BORG_SECURITY_DIR files outside the SQLite indexes on
2026-09-11.** This is a
narrow change to the README's literal “only .db/master” promise, not an
exception allowing repository repair or live-source writes. The directory
guard can be implemented independently. These policy decisions are resolved;
filesystem-enforcement validation is implementation work tracked in #80.

### 3. Entry identity and integration

Semantic identity is `(repository_id, archive_id, exact_archive_relative_path)`.
Names and physical locations are labels/locators. Archive names can be reused
or renamed; a new ID is a different snapshot. Repository clones sharing an
ID are locations of the same logical repository, not proof of independent
backup copies. Preserve the selected name and archive ID as separate facts.

Each Borg archive gets its own index and one catalog source. Preserve
`files.id`/FTS rowid as build-local identifiers and `index_uuid` as the
identity of the generated index. Existing tar indexes admit repeated paths
and mark shadowed occurrences; directory indexes use paths relative to one
snapshot root. Neither currently has Borg's version key. Add explicit
source identity metadata and catalog replication; do not overload a path,
directory mtime, or `archive_blake3` with an archive ID. Replacement ownership
must match source kind, repo ID and archive ID, not just the repository path.
A changed locator with the same IDs remains the same logical source:
rebuild into its explicitly selected owned DB or register a moved DB as the
same snapshot. A different archive ID cannot overwrite that DB merely
because the locator or archive name matches.

For Borg 1.4.4, accept only paths and link targets provably representable by
the verified JSONL interface. Reject the selected archive on any question
mark (including a literal one), invalid surrogate, replacement character,
duplicate path, or unsafe/noncanonical path. This intentionally restricted
subset avoids conflating `bad-?.txt` with an undecodable filename.
Never fabricate `path_raw` or `link_target_raw`. Broader lossless-path support
requires separately verified Borg capabilities.

Repository JSON preflight pins repo ID and name-to-ID mapping; every listed
item must carry the expected `archiveid`. Repeat the repository mapping
check before publication. These detect mistakes and changes but do not
replace the immutable-snapshot prerequisite or prevent an ABA race alone.

Regular-file streams feed the existing `process_reader`. Accept their
results only after exact size, healthy-item, complete-listing, and child-exit
checks. Metadata-only performs no extraction and stores no content-derived
values. Symlinks are name/target records, never followed. Borg hardlink
slaves are metadata records resolved only to a validated in-archive master;
never hash their potentially empty stdout as their content. Unsupported,
damaged or ambiguous entries fail the build; retain the previous index.
Regular sparse files use Borg's logical stdout bytes, including zeros, so
compare logical size and hash the whole stream; do not infer physical
allocation from listing size. Directories emit no row, hardlink slaves
invoke no extraction, and devices/FIFOs/sockets are unsupported failures.

### 4. Subprocess cost and mitigation

A serial `list` plus N `extract` calls is a real scaling problem.
Ten 22-byte extractions from a six-item local fixture measured median
0.486 seconds (range 0.347–0.641 seconds). At that measured rate, 10,000
calls take about 81 minutes and 100,000 about 13.5 hours, before allowing
for larger-archive metadata scans. These are extrapolations, not production
benchmarks. Each extraction scans archive items, so N files among M items
can require O(N × M) metadata work, beyond startup and credential lookup.

Use one archive per invocation, serial bounded extraction, explicit
metadata-only mode, progress/cancellation, and reuse of completed indexes
only after matching immutable identity and all indexing options/algorithm
versions. A known archive ID is not a content hash and does not establish
a “metadata-only is already fully indexed” shortcut.

Benchmark synthetic 1k/10k/100k-entry archives before promising full-mode
scale. A future framed one-pass transport could amortize startup, but
expanding the allowlist needs its own decision. Do not concatenate multiple
paths, extract directory prefixes, enable parallelism by default, mount
Borg, or use its Python internals as an undocumented API.

## Alternatives considered

- `borg://` auto-routing or only a separate subcommand: neither prevents
  ordinary indexing from walking a repository's storage; structural refusal
  is required regardless of the user-facing selector.
- Archive name as identity: changes/reuse break snapshot pinning. The ID
  describes the version; names describe how the operator selected it.
- Secret in environment/argv, arbitrary passcommand CLI, interactive v1:
  avoidable exposure and non-deterministic authentication; use the existing
  credential-helper deployment pattern with an explicit trust boundary.
- Verbs-only “read-only”, normal locks, or bypass-lock on a writable source:
  rejected by observed recovery behavior. Lock safety and byte immutability
  are separate requirements.
- Read-only bind of a live repo plus pre/post checks: prevents some writes
  but does not make a coherent snapshot, and endpoint checks miss ABA.
- JSONL or raw `bpath` assumed lossless: contradicted by the installed
  binary and its source. Reject ambiguous names now rather than index
  another file under the same display name.
- Batch `extract --stdout`: has no file framing; no valid per-file hashing
  interpretation. A path argument is prefix-matched unless made exact.

## Consequences

This ADR records the accepted design, not a claim that Borg support ships or
that filesystem-enforcement tests have passed. The design round is complete;
technical enforcement validation belongs to implementation issue #80.
Start implementation with the independently useful dispatch guard. Adapter
work implements and validates the filesystem boundary, then credentials/command
construction, identity/schema integration, entry processing, and scale gates.

The guard intentionally makes genuine or conservatively ambiguous Borg
storage unavailable through ordinary indexing even before an adapter is
enabled. This is a safety restriction, not a promise of an immediate Borg
replacement command. Snapshot-only scope and extra private state are approved.
The remaining enablement gate is implementation evidence in #80 that the
filesystem refuses source writes. A live repository written by a backup
timer is never an eligible source.

Borg entries remain read-only/report-only. Logical duplicate file sizes
must not be presented as physically reclaimable Borg storage: chunk sharing
and retention invalidate that inference. Catalog/query changes must preserve
existing tar/directory behavior and frozen JSON/exit contracts, with additive
source capability/provenance fields and explicit unsupported verification
reporting. Source files, contracts, schemas and the ADR index were not
changed in this design lane.

Evidence, test strategy, decision log, review notes, and issue bodies are in
[the investigation](../investigations/2026-09-11-borg-source-design.md).
