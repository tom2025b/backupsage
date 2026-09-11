# Borg repository source design investigation

Date: 2026-09-11
Worktree: `/home/tom/projects/bs-borg-source`
Branch: `feat/borg-source-indexing`
Base: `bb8abe8079cf4917517667eb408e02b9070255a9`
Status: design complete; ADR 0007 accepted by Tom on 2026-09-11; no implementation or commits

## Outcome and accepted decision

Implement structural refusal of Borg repositories in ordinary indexing
first. Proposed Borg indexing is local, one explicitly selected archive,
from an operator-provided immutable snapshot with enforced read-only child
access. It uses a sealed list/extract adapter and the existing per-entry
pipeline. BackupSage must never be pointed at a live Borg repository that a
backup timer writes to, including through aliases or a read-only view.

Tom approved snapshot-only v1 and private BORG_BASE_DIR / BORG_CACHE_DIR /
BORG_SECURITY_DIR files outside SQLite indexes on 2026-09-11. Read-only
enforcement must be at the filesystem level: a read-only mount of the
snapshot or filesystem-enforced immutable snapshot must refuse writes.
Borg has no true read-only mode, and safe-verb conventions cannot guarantee
this. If the filesystem cannot refuse a write, indexing must refuse.
Technical validation is implementation work in #80, not a remaining task
for this design round. No production repositories,
credentials or backup timers were accessed during this investigation.

## Repository evidence and integration seams

Read README and ADRs 0001–0005; ADR 0004 supplies the model for a reasoned
decision, alternatives and a non-vacuous acceptance oracle.

| Existing seam | Observed behavior | Required extension |
|---|---|---|
| `indexer::run_index` | `is_dir()` immediately dispatches to directory walker | Structural refusal before either source dispatch |
| `source_dir::index_dir` | Pre-count and walk descend independently; output setup precedes traversal | Guard both traversals and abort safely on nested repositories |
| `indexer::process_reader` | Reads to EOF; hashes all bytes; returns an outcome before any child exit check | Counting/error adapter plus child status gate before accepting outcome |
| `IndexRun::record/finish` | Shared rows, FTS, modes and post-passes | Feed validated Borg entries; preserve transaction/promotion behavior |
| `create_db_with_fallback` | Protects a tree only for source type “dir”; replacement checks path/type | Borg-aware semantic ownership and explicit protected source tree |
| `store::SourceMeta` | Path/type and modes, no Borg version fields | Add repo ID, archive ID/name, Borg capability/version and index options |
| `master::add` | Registry match by UUID or DB path | Replicate Borg identity/capabilities; avoid treating reindexes as independent source copies |
| `master verify` | Archive path/stat/hash assumptions | Dispatch by source capability; no directory mtime or ID as whole-archive BLAKE3 |
| ADR 0002 | Raw-byte sibling fields retain lossless paths | Reject paths unsupported by installed Borg, never synthesize raw bytes |
| ADR 0005 | Content mode is replicated, not inferred by each consumer | Preserve full/search-only/metadata-only behavior and skip reporting |

`borg-backup-mcp/src/borg.rs` defines a closed `BorgOp` match; its
`command.rs` hides command fields and exposes no argument append operation.
However its operation set includes Create, and its runner inherits the
environment and buffers complete stdout/stderr. Reuse construction
discipline only. The systemd unit and `docs/titan-own-backup.md` use
`LoadCredential` and `BORG_PASSCOMMAND=/usr/bin/cat %d/borg-passphrase`;
no live credential file was opened.

## Source selection and refusal algorithm

Proposed syntax, not current CLI:

```text
backupsage index-borg SNAPSHOT_PATH --archive LITERAL_NAME --index PRIVATE_DB
```

Default this new command to metadata-only; full and search-only require
explicit mode selection. Existing tar/directory defaults remain unchanged.
Accept exactly one local filesystem repository locator and literal archive
name. Canonicalize the path and use separate argv elements; no shell quoting
round trip. Reject ambiguous Borg-location syntax in the canonical locator
(`::`, braces/placeholder expansion), braces in archive names, NUL,
slash in archive names, empty names, invalid Unicode, and any other
unsupported name encoding. Do not pass a user URI, SSH target, pattern,
additional Borg option, or inherited `BORG_REPO`. Ordinary paths may have
spaces. Explicit DB naming avoids sibling collisions between archive names.

The ordinary-index probe is independent of the Borg binary and passphrase:

1. Before opening a source or allocating output, inspect its canonical
   ancestors, including the source itself if a directory.
2. At each candidate directory, examine only bounded structural metadata:
   `config`, `data/`, and relevant markers such as `README`,
   `index.*`, `hints.*` or `integrity.*`. Never dump config contents:
   encrypted repokey material can be stored there.
3. A no-follow, bounded regular config read with interpolation disabled,
   `[repository]`, version 1, 64 hex ID characters, and `data/` is a
   strong candidate. This is refusal detection, not proof of validity.
4. Repository section with missing/bad fields, unsupported version,
   repository markers with missing config, suspicious marker symlinks,
   permission failure, oversized config, or conflicting evidence gives
   an inconclusive/error result and refuses. “Not Borg” requires a
   successfully examined directory without repository evidence.
5. Refuse the root or descendant source, without redirecting automatically
   to an encrypted-repo operation. Explain the explicit command and required
   snapshot context. For nested directories, perform this check before
   descent in both walks; abort the source, never silently skip the subtree
   and call the whole directory complete.
6. Preserve original source/output identity checks and staging cleanup.
   Normal directories, including one named borg, still index.

This is not a complete hostile-filesystem race solution: ordinary directory
sources are already mutable snapshots. Traversal must fail if the checked
identity changes, and the guard must share any stronger fd-based traversal
work supplied by the concurrent lane. It must not claim that filename
heuristics identify all malformed repositories.

## Borg 1.4.4 commands verified locally

Read `borg --help`, `borg list --help`, `borg extract --help`,
`borg info --help`, `borg help patterns`, `borg init --help`, and
`borg create --help` from `/usr/bin/borg`. The following local forms
were actually exercised against the temporary fixture:

```sh
borg init --encryption=none /tmp/backupsage-borg-design.TXiBvJ/repo
borg create /tmp/backupsage-borg-design.TXiBvJ/repo::fixture input
borg list --json --lock-wait 1 -- /tmp/backupsage-borg-design.TXiBvJ/repo
borg list --json-lines \
  --format '{type}{mode}{uid}{gid}{size}{isomtime}{archiveid}{archivename}' \
  -- /tmp/backupsage-borg-design.TXiBvJ/repo::fixture
borg extract --stdout -- \
  /tmp/backupsage-borg-design.TXiBvJ/repo::fixture pf:input/one.txt
borg extract --stdout -- \
  /tmp/backupsage-borg-design.TXiBvJ/repo::fixture 'pf:input/sh:literal[1].txt'
borg list --json --bypass-lock -- /tmp/backupsage-borg-design.TXiBvJ/repo
```

Fixture setup ran from the temporary root with `env -i`, fixed PATH and
locale, and `BORG_BASE_DIR`, `BORG_CACHE_DIR`, `BORG_SECURITY_DIR` all
inside that root. Init/create are fixture-author operations, never adapter
operations. Examples are evidence from this throwaway repo, not instructions
to touch an existing backup. Set child `LC_ALL=C.UTF-8` and `TZ=UTC` in
the adapter; the experiment exposed a four-hour local/UTC difference in
offset-free `isomtime` before fixing TZ.

`list --json` enumerates repository archives; `--json-lines` enumerates
archive items. Explicit fixed `--format` requests size/time/archive ID
without content-hash or compressed-size keys that could read data/load
cache. Default JSONL includes `healthy`, type/mode and link information.
An empty archive legitimately emits no items, so validate its mapping from
repository JSON, not an assumed first row.

`pf:` is full-path matching. The default PATH style is a directory-aware
prefix, and extracting the fixture's `input` prefix returned four file
payloads concatenated with no framing. The literal-pattern filename was
retrieved correctly with `pf:`. Build exactly one `pf:` selector from
one validated regular-file path, after `--`; reject directories, duplicates,
path normalization ambiguities, absolute paths and dot/dotdot components.
Do not expose Borg's pattern grammar to callers.

The production immutable-view command profile would add fixed
`--bypass-lock` to the same permitted verbs only after containment succeeds.
Its healthy CLI semantics were checked; OS containment was not proved here.
The final fixed profiles, including `--consider-part-files` on archive list
and extract together with `--bypass-lock`, were subsequently run on the
healthy fixture: list succeeded and exact extraction returned 22 bytes.

## Read-only boundary: observed failures of a verbs-only design

Installed source evidence:

- `repository.py::open` acquires a lock before parsing config.
  `locking.py` writes lock directories and roster state, and can clean stale
  lock state. BackupSage must not implement or invoke lock breaking.
- `repository.py::check_transaction` can replay segments and write an
  index; `open_index` catches index read/integrity errors and executes
  `os.unlink(index_path)` before recovery. This is reachable through reads.
- `cache.py::SecurityManager` saves location, key type and manifest time;
  helper directory functions create security/cache directories and tags.
  Manifest authentication can change TAM-required state.
- `info` uses `cache=True`. Simple fixed-format list/extract avoid its
  unnecessary chunk-cache work, but still have security-state side effects.

Therefore a valid read-only operation enum is necessary but insufficient.
Require an immutable snapshot and OS-enforced denial of repository writes
for Borg and credential-helper descendants. Immutable storage prevents
other writers changing bytes; access confinement prevents Borg changing
or repairing them. A pathname called snapshot, `chmod`, a cooperative
“do not write” flag or a read-only bind of a changing live path is not proof.
Pin the selected target; do not let a symlink/path swap or writable alias
escape protection. No writable repository alias may be exposed to children.

A Linux namespace/container profile is a possible implementation mechanism,
not an accepted dependency yet. A `bwrap` namespace smoke check failed in
this execution environment with “No permissions to create a new namespace.”
This does not establish whether deployment outside the sandbox supports it.
Do not silently fall back. The implementer must prove confinement, including
failed unlink/replay and helper writes, on supported deployment platforms.
Backend implementation and validation belong to #80: sealed builders require a
`VerifiedImmutableSnapshot` that only a successfully validated platform
backend can produce. The backend must attest storage-level snapshot identity,
pin that object and confine descendants; there is no unchecked path
constructor. Config checksums and read-only mount flags alone are inadequate.

Only the approved private state directory is writable to the Borg/helper
tree; index writing belongs to the parent and passes the common output
boundary. State cannot be located inside a source, aliased to protected
inputs, or shared with backup jobs. It preserves authentication and rollback
history across runs; do not discard it each run to suppress security errors.
Repository config and Borg keys are read-only inputs. Private state may
contain key/security metadata and is not harmless public cache.

## Authentication, child lifecycle, and disclosure

Use a user-configured credential helper inherited via `BORG_PASSCOMMAND`.
For systemd, `LoadCredential` supplies a private file and a fixed cat helper
reads it. The helper string must contain no secret literal; neither an
echo-secret command nor a shell wrapper embedding the secret is supported.
An arbitrary helper is trusted user code, so execute it inside the same
restricted environment/filesystem boundary. Do not derive it from source
metadata or persist it in the index.

Reject `BORG_PASSPHRASE`, `BORG_NEW_PASSPHRASE`,
`BORG_PASSPHRASE_FD` and secret-display settings if supplied; do not print
their values. Borg 1.4.4 itself gives passphrase environment precedence over
passcommand, so simply adding a helper without rejecting conflicts is unsafe.
A future FD option needs its own lifecycle/reopen design, not blind inheritance.

Use exactly the finite environment profile recorded in ADR 0007: PATH,
LC_ALL, TZ, fixed BORG_BASE_DIR, BORG_CACHE_DIR, BORG_SECURITY_DIR and
BORG_KEYS_DIR, and optionally vetted inherited BORG_PASSCOMMAND. Key
material is exposed read-only. Base/cache/security metadata is private
state covered by Tom's approved exception; that approval is not
limited to the security directory alone. Borg performs passcommand
`shlex.split` and executes it without a shell; BackupSage does not parse
and execute it a second time. Only helpers compatible with the fixed
environment are supported.
Do not forward `BORG_LOGGING_CONF`, `BORG_RSH`, `BORG_REMOTE_PATH`,
`BORG_REPO`, `BORG_WORKAROUNDS`, Python/loader injection variables,
security-answer overrides, or arbitrary inherited state locations.
Use legacy exit codes explicitly or clear the modern-code override.
No SSH/network access in this first slice.

Use absolute executable resolution fixed at startup; version acceptance
is tied to the tested 1.4.4 capability profile. Fail unknown profiles rather
than interpreting Borg 2 JSON or location syntax optimistically. No helper
configuration means encrypted access fails quickly; detach the child session
from a controlling terminal as well as using null stdin, because getpass
may otherwise open the terminal.

Stream stdout, concurrently drain and discard bounded stderr, and report
operation/status categories without raw exceptions, argv/env dumps, helper
output, plaintext or paths copied out of child diagnostics. Apply byte/line/
entry limits to listing parsers and a configured execution timeout; on
timeout, parser failure, pipe failure or cancellation terminate and reap
the whole child group, including helpers. Do not deadlock on a full stderr
pipe or promote output while a child is still running.

Full mode stores decrypted text; search-only stores revealing tokens,
hashes and media metadata; metadata-only stores names and filesystem facts
but no content reads, hashes or tokens. Set private permissions for DB,
WAL/SHM and staging files before writing, and protect indexes like the
backup. Metadata filenames can also be sensitive.

## Identity, paths, entries and publication

Pin repository ID plus selected archive ID, not repo location/name or
repository directory mtime. Names can change/recur; copies can retain repo
IDs. Store IDs, selected name, display locator, index UUID, capability
profile, content mode and relevant algorithm/options separately.
One archive is one source DB. Default implicit sibling naming and fallback
are disabled for this command. Apply existing ProtectedSet checks to the
whole snapshot tree, credential/key inputs, state and output sidecars;
hardlink aliases need identity checks too. The generic helper currently
treats only “dir” as a tree and cannot simply be called with “borg”.

Catalog registration stays explicit via the DB path. Existing UUID/DB-path
tracking remains index-artifact identity; add Borg snapshot identity so
registering two builds of the same snapshot does not imply two backup
copies. Do not make destructive DB replacement decisions using an archive
name or ambiguous joined string. Rebuilding different content modes can
replace an owned index of the same snapshot only under the existing
explicit replacement contract.

At preflight, parse repository JSON and find exactly one selected mapping.
At enumeration request `archiveid` in each JSONL row and check it. Require
a complete valid listing and successful exit; stage rows internally and do
not publish after truncated JSONL. Reject duplicate paths before extracting,
even if Borg accepted the archive, since one full-match extraction could
otherwise concatenate multiple entries. Postflight must find the same repo
ID and selected archive ID. Fail missing/renamed/reused archives. This is
defense in depth, not an ABA-proof alternative to immutable storage.

The installed JSONL reports undecodable byte 0xff as “?” in both path and
linktarget. A second `list --format '{bpath}{NUL}'` also produced “?”;
`PYTHONIOENCODING=utf-8:surrogateescape` did not fix it because Borg resets
stdout to replacement mode. Thus no raw-path side channel is proposed.
Reject “?” anywhere in paths/link targets and selected name, even literal
question marks, plus invalid-surrogate/replacement-character cases. For
accepted UTF-8 paths, use existing capture semantics with raw fields NULL.
A future genuinely lossless CLI profile can lift the restriction.

Regular entries require healthy=true, validated integer size, recognized
type/mode and timestamp; special permission bits must survive mode parsing.
Skip directory rows consistently with existing sources. Never follow
symlinks or read devices/FIFOs/sockets; reject unsupported special types.
Hardlink slave type can be “-” while mode starts “h” and size is zero:
do not infer file content from that size. Validate its exact in-archive
master, resolve a cycle-free link chain to a healthy regular entry, and
use the master's logical size/hash semantics. Test the existing store's
hardlink post-pass before reusing it; no content hash in metadata-only.
Dangling/ambiguous/cyclic hardlinks fail, not hash as empty.

Default Borg listing/extraction hide part files; always use the fixed
`--consider-part-files` option consistently on both commands in this
profile, and validate included rows before enabling the adapter.
Do not advertise a complete snapshot while a known item class was silently
excluded. Checkpoint archives remain excluded from initial selection.
The fixed flag combination was exercised on the healthy fixture; a genuine
partial-item corpus remains an implementation acceptance gate.

A counting reader must stop/reject any excess bytes and reject short reads.
Even zero-size regular entries require a verified extraction result in
content modes. Wait for successful child exit after EOF; `process_reader`
alone cannot establish successful extraction. Fail the entire staged build
on any Borg nonzero exit (warnings included), unhealthy content, IO error,
listing inconsistency or identity change. Partial hashes never become rows
in a completed index; previous completed DB survives. Successful single
source returns 0, runtime failure 1, usage remains clap's existing 2;
federated skip-2 semantics remain unchanged.

Borg IDs are not BLAKE3; `archive_blake3` stays absent. Offline search and
dedup can use completed DBs normally. Identity verification is distinct
from full integrity verification; any unsupported `master verify --deep`
operation must return an explicit unsupported/inconclusive result, never
success based on repo ID/mtime. Source capabilities must also prevent
filesystem actions on virtual entries. Borg logical duplicate bytes cannot
be counted as physically reclaimable storage or independent-copy coverage
without a separate chunk/retention model.

## Subprocess cost and implementation sequence

Measured warm repeated extraction of one 22-byte file, 10 calls:
median 0.485661 seconds; min 0.346601; max 0.640538. Initial archive had
four files and two directories. It was unencrypted: credential-helper and
key-decryption costs are not included. No representative large-archive or
remote benchmark was run. Per-call scanning of all archive metadata can
make the content-mode path quadratic in entry count. The 81-minute/13.5-hour
10k/100k extrapolations are illustrative floors only if that measured rate
holds, not latency guarantees.

Mitigations within scope: explicit one-archive selection, metadata-only
mode (one enumeration plus pre/post repo metadata), bounded serial content
reads with progress and cancellation, and exact-version/options cache reuse.
Reusing a prior full index requires matching content mode, caps, pHash
algorithm, parser/profile version and completeness as well as source ID.
Do not reuse a content-derived cache for metadata-only or equate Borg chunk
IDs with the existing BLAKE3 hash.

Follow-up benchmark gate: synthetic 1k, 10k and 100k entries, small text,
mixed media and encrypted fixtures, explicit cold/warm distinction,
wall time, process count, metadata bytes, bytes decrypted and peak memory.
Evaluate framed one-pass transport separately if the baseline is unusable.
An export stream might eventually provide framing, but export-tar is not
in this allowlist and no such invocation is proposed or verified here.
Native format parsing, mounting and uncontrolled parallel extract are out.

Implementation order:
1. Independently land the directory/ancestor/nested-repository refusal.
2. Implement and validate filesystem-enforced read-only snapshot access under #80; Tom's scope/state decisions are approved.
3. Build sealed operation/environment/lifecycle and credential tests.
4. Add source metadata, protected output ownership and catalog capabilities.
5. Implement metadata-only, then validated content streaming and links.
6. Run compatibility/correctness gates and scale benchmarks before release.

## Acceptance strategy (proposed, not tests already implemented)

- Construct real throwaway repos under fresh temp roots. Never run tests
  against configured production paths. All Borg state belongs to the test.
- Guard tests: root, alias, ancestor segment, nested repo, misleading name,
  ordinary config/data directory, unsupported/malformed repository markers,
  unreadable/oversized/symlink config; assert no chunk bytes are opened and
  no DB is published on refusal, with a prior DB checksum preserved.
- Exact-stream oracle: compare expected bytes and BLAKE3 against equivalent
  tar/dir input; include prefix siblings, wildcard-looking names, empty,
  large, binary, sparse logical content, media and non-ASCII paths.
- Ambiguity tests: distinct literal “?” and invalid-byte names, raw-newline
  names, normalization collisions, duplicate paths, hardlinks/symlinks and
  non-UTF-8 link targets. Assert explicit failure rather than a green empty
  index or a hash of concatenated/empty output.
- Fault tests: nonzero after complete stdout, short/excess payload,
  damaged-item status, malformed/truncated/oversized JSONL, helper failure,
  stderr saturation, timeout and cancellation; no promoted partial result
  and no surviving child.
- Security tests: synthetic sentinel secret never appears in argv, logs,
  DB, WAL/SHM or error reports; missing helper cannot prompt. Reject hostile
  inherited environment, malicious location placeholders and helper attempts
  to write source paths or an alias.
- Read-only oracle: fingerprint the entire fixture repo and observe attempted
  writes under enforced confinement, including corrupt-index and replay
  conditions; prove storage is unchanged on both success and failure.
  Also prove the source cannot change during the job and the state directory
  retains security history without touching ordinary Borg state.
- Identity/catalog tests: renamed archive, reused name/new ID, moved snapshot,
  cloned repo ID, duplicate index registration, changed options, empty archive,
  different archive same path, offline source and unsupported deep verify.
- Preserve existing contract fixtures and mode behavior. Add populated Borg
  fixtures/capabilities; no blind blessing of tar/directory regressions.

## One-line decision log

- D01: Keep this lane design-only and reserve ADR 0007; leave the index and shared contracts to the coordinator.
- D02: Use index-borg with one literal archive and explicit DB; omission refuses before work.
- D03: Guard ordinary source roots, ancestors and nested traversal structurally; a separate command alone is insufficient.
- D04: Reuse sealed construction, excluding Create, arbitrary flags and unbounded output capture.
- D05: Require immutable snapshots plus enforced read-only child access because Borg reads can attempt recovery writes.
- D06: Gate bypass-lock on that context only; never bypass a live writable repo's locks.
- D07: Propose private persistent Borg security state; obtain Tom's approval for the expanded write-surface promise.
- D08: Use a trusted inherited passcommand with no secret-bearing argv/env fallback and no interactive prompt.
- D09: Identify entries by repo ID, archive ID and exact path; preserve index UUID as separate artifact identity.
- D10: Reject ambiguous question-mark/non-UTF-8 paths on 1.4.4; bpath text output is not a lossless escape hatch.
- D11: Extract exactly one validated regular path with pf:; accept only after length and child-exit validation.
- D12: Treat hardlink slaves as metadata references, never successful empty stdout content.
- D13: Preserve content modes, protect plaintext indexes and distinguish logical duplicates from physical Borg savings.
- D14: Use metadata-only and identity/options reuse for cost control; benchmark before full-mode scale claims.
- D15: Containment is unverified in this sandbox; refuse unavailable enforcement instead of falling back.
- D16: User required exclusive worktree use and no commits; copied the pre-existing .gitignore patch as requested.
- D17: Claude/Grok reviews changed only the new command's default to metadata-only and made the closed argv/env profile explicit.
- D18: No immutable-storage backend is approved; source capabilities cannot be constructed until a platform proof passes.
- D19: Tom approved snapshot-only v1 and private BORG_BASE_DIR / BORG_CACHE_DIR / BORG_SECURITY_DIR outside SQLite indexes.
- D20: Filesystem-level write refusal is mandatory; safe Borg verbs or convention are not enforcement.
- D21: Never point BackupSage at a live repository written by a backup timer; aliases/read-only views do not waive this.
- D22: ADR 0007 is accepted; technical filesystem-enforcement validation moves to implementation issue #80, not this design round.

## Reviews and delivery

Daybreak independently reviewed installed source and the proposed boundary,
without accessing a repository or editing files. Findings incorporated:
recovery writes beyond locks, persistent security state, exact-selector
normalization, lossy bpath, hardlink-empty stdout, part-file omission,
placeholder expansion, output-before-validation and child isolation.

Claude refutation and Grok ADR review both completed with tool execution
disabled for review. Neither approved adapter enablement. Incorporated their
requests for metadata-only default, exact argv/environment profiles, explicit
immutable-backend gate, expanded state-directory accounting, helper parsing,
type handling and moved-locator ownership.

Review disagreements were checked against installed source: a normal
fixed-format list uses `cache=None`, and extract's repository decorator
does not request `cache=True`; therefore Grok's claim that these always
load the files/chunks cache is not supported. Both still touch security/base
metadata, which is explicitly covered. Sparse stdout emits logical bytes
including zeros in `archive.py::extract_item`, so logical-size validation
is required rather than disabled. Proposed config-hash checks cannot prove
immutability or rule out ABA and were not adopted as a replacement boundary.
An inconclusive structural probe remains fail-closed; a non-fatal fallback
would reintroduce the original chunk-indexing failure.

Roadmap examples #33, #34, #40, #41 and the existing label set were read
through GitHub. No pre-existing Borg issue was found in that search.
Filed and read back as OPEN with the intended labels:
[guard #79](https://github.com/tom2025b/backupsage/issues/79) and
[adapter parent #80](https://github.com/tom2025b/backupsage/issues/80).

Requested ADR index row (coordinator adds it; this lane does not edit index):

```text
| [0007](0007-borg-source.md) | Borg sources require immutable snapshots and sealed reads | accepted | 2026-09-11 |
```

Only the ADR and this investigation are authored deliverables. The existing
shared-checkout .gitignore change was saved to `/tmp/astra-borg-wip.patch`
and applied to this worktree at the user's explicit request. It is not part
of the Borg design. No commit or PR is authorized by the latest instruction.

## Issue draft: Prevent ordinary indexing of Borg repository storage

Labels: roadmap, area:indexing, safety-critical, correctness

Filed as [#79](https://github.com/tom2025b/backupsage/issues/79).

<!-- guard-issue-start -->
## Why

Ordinary indexing currently sends every directory into the filesystem
walker, including Borg repository storage. That can silently index encrypted
chunks instead of archived files. The problem also occurs through nested
repositories or selecting a repository descendant.

## Scope

Add a bounded structural Borg-candidate probe before source dispatch and
before nested directory descent in both counting and indexing walks.
Refuse confirmed and suspicious/inconclusive repositories without spawning
Borg or creating a published index. Keep this independently useful safety
fix separate from the proposed Borg source adapter (ADR 0007).

## Acceptance criteria

- [ ] Root, alias, ancestor/descendant and nested-repository cases refuse before chunk reads.
- [ ] Malformed, unsupported, unreadable and symlinked repository markers fail closed.
- [ ] Ordinary directories, including a directory named borg, retain expected behavior.
- [ ] Previous completed index survives refusal; no partial new index is published.
- [ ] Tests use only fresh temporary repositories and prove positive and negative cases.

## Dependencies

Existing shared output-safety boundary; coordinate with any concurrent
directory-traversal work. No credential or snapshot policy decision is
required to implement refusal.

## Tracking

This is a bounded roadmap implementation issue. Acceptance requires the
non-vacuous refusal and ordinary-directory regression tests above.
Design: docs/adr/0007-borg-source.md and
docs/investigations/2026-09-11-borg-source-design.md (local proposal).

## Safety invariant

BackupSage never rewrites or deletes from an archive. The guard must not
invoke Borg, read encrypted chunks, or automatically redirect into indexing.
<!-- guard-issue-end -->

## Issue draft: Add snapshot-backed Borg sources behind read-only and credential gates

Labels: roadmap, area:indexing, area:remote, safety-critical, privacy

Filed as [#80](https://github.com/tom2025b/backupsage/issues/80).

<!-- adapter-issue-start -->
## Why

Borg archives should participate in BackupSage search and dedup through the
same per-entry pipeline as tar and directory sources. Borg 1.4.4 read verbs
can perform repository recovery writes, its JSON path output is lossy, and
per-file extraction has substantial repeated-process/metadata-scan cost.
A verbs-only read-only claim is insufficient.

## Scope

Design-led, local-only index-borg command: one explicit archive and DB,
immutable snapshot with filesystem-enforced read-only child access, sealed list and
single-path extract --stdout operations, inherited trusted BORG_PASSCOMMAND,
versioned source identity, metadata-only default and explicitly selected
validated full/search-only content modes.
Never point BackupSage at a live repository that a backup timer writes to,
including through aliases or a read-only view. No automatic snapshot creation,
generic Borg arguments, mount, native format parser, PR or implementation
is part of the current design deliverable.

## Acceptance criteria

- [x] Tom approved snapshot-only v1 and private BORG_BASE_DIR / BORG_CACHE_DIR / BORG_SECURITY_DIR files outside SQLite indexes on 2026-09-11; ADR 0007 is accepted.
- [ ] Filesystem-level enforcement refuses repository writes: use a read-only mount of an immutable point-in-time snapshot or a filesystem-enforced immutable snapshot; unsupported enforcement refuses indexing.
- [ ] No-archive invocation refuses; ordinary Borg directories are protected by the companion guard.
- [ ] Credentials never reach argv, logs, SQLite or inherited secret-valued fallback; missing helper never prompts.
- [ ] Repository ID plus archive ID plus exact path define identity; pre/post mapping and per-item archive ID checks pass.
- [ ] Unsupported/ambiguous paths fail closed on the verified Borg profile; raw-byte fields are never fabricated.
- [ ] Exactly one full-path regular entry per extraction; length, health and exit status are validated before results are accepted.
- [ ] Hardlinks, symlinks, partial items, malformed streams and cancellation have tested conservative behavior.
- [ ] Metadata-only is the new command's default and does no extraction; modes, output ownership, catalog identity and frozen contracts remain correct.
- [ ] Borg IDs are not BLAKE3; logical duplicate sizes are not physical reclaimable space; unsupported deep verification is explicit.
- [ ] Synthetic 1k/10k/100k benchmarks quantify startup/scan cost; no unframed multi-file stdout optimization.
- [ ] Every integration test uses throwaway repositories and private synthetic credentials/state.

## Dependencies

Structural-refusal issue #79; accepted ADR 0007; filesystem-enforcement
validation in this implementation issue; existing output-safety,
content-mode and contract boundaries.
Align source capabilities with #33 and any later remote adapter work #34;
this local slice does not implement or change their remote rollout.

## Tracking

This is a parent roadmap issue. Before implementation, split it into linked
children for containment/credentials, adapter/identity/catalog, and performance
acceptance. Close only when every child and the release gate are satisfied.
Design: docs/adr/0007-borg-source.md and
docs/investigations/2026-09-11-borg-source-design.md (accepted decision).

## Filesystem enforcement validation — implementation scope

This validation belongs to #80 and its implementation children, not the
completed design round. Borg has no true read-only mode; even its read paths
can attempt index-recovery writes. An allowlist of safe verbs or a convention
is not enforcement. If the filesystem cannot refuse a write, the guarantee
does not exist.

- [ ] Establish an immutable point-in-time snapshot source; a read-only bind of a changing live repository is not a snapshot.
- [ ] Demonstrate actual filesystem refusal of create, write, truncate, unlink and rename operations against a throwaway repository under the same access conditions as Borg and its helper descendants.
- [ ] Exercise Borg read-path index-recovery/unlink/replay attempts on damaged throwaway fixtures; the filesystem must refuse source mutation and the build must fail without a repair fallback.
- [ ] Pin the validated snapshot target and verify that path swaps or writable aliases cannot bypass the filesystem boundary.
- [ ] Confirm snapshot access never targets a live repository written by a backup timer; test rejection with synthetic fixtures only, never real backup jobs.
- [ ] Verify approved private base/cache/security state remains writable outside the source and SQLite indexes while repository writes fail.
- [ ] Preserve repository bytes and metadata on success and failure; unavailable or unverifiable enforcement must refuse indexing before Borg access.

## Safety invariant

BackupSage never rewrites or deletes from an archive. Source mutation and
lock breaking remain impossible; approved private Borg state writes do not
authorize repository repair. BackupSage must never be pointed at a live
repository that a backup timer writes to. No production repository may be
used in tests.
<!-- adapter-issue-end -->

## External references

Local binary behavior and installed source are the primary evidence.
Version-pinned documentation cross-checks:
[Borg 1.4.4 general usage](https://borgbackup.readthedocs.io/en/1.4.4/usage/general.html)
describes passcommand precedence, noninteractive environment and locking;
[Borg 1.4.4 frontend JSON](https://borgbackup.readthedocs.io/en/1.4.4/internals/frontends.html)
describes frontend data interfaces. Do not substitute Borg 2 documentation.

Roadmap style:
[#33](https://github.com/tom2025b/backupsage/issues/33),
[#34](https://github.com/tom2025b/backupsage/issues/34),
[#40](https://github.com/tom2025b/backupsage/issues/40),
[#41](https://github.com/tom2025b/backupsage/issues/41).
