# ADR 0006 — The immutable action-plan contract: byte-stable canonicalization and identity

Date: 2026-08-28 · Status: accepted · Milestone: v1.1 (issue #75, child of #12)

## Context

v1.2 introduces the first commands that mutate user data (organize, extract, dedup
apply). The permanent safety invariant — BackupSage never rewrites or deletes from an
archive — is enforced entirely by the plan contract: every mutating action is
authorized by a plan document a human reviewed, and #76/#77 refuse to apply anything
that isn't exactly what was reviewed. That guarantee only holds if a plan can be
compared byte-for-byte, because "this plan is unchanged since I read it" is
unprovable any other way.

`docs/COMPATIBILITY.md` binds every v1.x machine-readable surface to additive-only
change. A plan file saved under v1.1 must still mean the same thing to a v1.4 binary.
This ADR freezes the plan document's shape, its canonical byte form, and the
derivation rules that make two independently-computed plans over identical inputs
produce identical bytes — issue #75's four acceptance criteria.

A design workflow (three independent proposals, three judging lenses, one synthesis;
full record in `design-docs/2026-08-28-issue-75-chosen-shape.md`) chose the shape
before implementation began. Two real defects surfaced during that design pass, and a
third was found only once the design was implemented and tested — all three are
recorded here because each shaped a decision this ADR freezes.

## Decision

### The document

One JSON document per plan, of one `kind` (`organize` | `extract` | `dedup`). The
**entire file is the canonical body** — no excluded envelope, no embedded digest, no
timestamp, no hostname, no producer version. This is deliberate: it means acceptance
criteria 1 and 2 read literally on the file as written, `b3sum plan.json` is the
plan's identity with no special tooling, and a future executor's journal hashes
exactly what a human reviewed rather than a subset of it. Provenance that genuinely
varies by run (when a plan was generated, by which binary) lives in the file's own
mtime and in the future executor's own journal — never inside the plan.

Top-level shape: `plan_schema_version`, `kind`, `policy` (a per-kind tagged union),
`roots[]`, `actions[]`, `groups[]` (empty unless `kind == dedup`), `excluded[]`, and a
fully-derived `summary`. Full type definitions live in `src/plan.rs`; the JSON Schema
at `docs/schema/plan-v1.schema.json` is hand-authored and reviewed alongside the
Rust types, not generated — a generated schema would churn with its generator's
version, and the schema is itself part of the frozen contract.

### Three independent version counters

`plan_schema_version` (this document's own contract, starting at `1`) is independent
of `index_schema_version` (recorded per source root, currently `3`) and of the dedup
report's `"version": 1` (`src/report.rs`). Satisfies acceptance criterion 4.
`from_canonical_bytes` refuses a plan whose `plan_schema_version` is newer than the
build understands, naming both numbers.

### Identity: content hash plus raw path bytes, never the display path

Every entry's identity is `content_hash` (mandatory, `"b3:<64 lowercase hex>"`) plus
raw path bytes. The lossy `display` string is a pure derived function of those bytes
and carries no authority — a plan can never be misled by a rendering, only by its
identity fields.

**Raw path bytes are lowercase hex, not base64**, matching `src/report.rs`'s existing
`path_bytes` convention (ADR 0002). Unlike that presence-conditional field, a plan's
`parts_raw` is **always** hex-encoded, even for clean UTF-8 — a presence-conditional
identity field would make the key set vary per entry and force every consumer to
reimplement a two-branch fallback identically. A second path-encoding convention
inside one product would be worse than a suboptimal one.

### Directory sources: a tagged absence, never a bare null

`src/source_dir.rs:239` records that a directory source has no whole-source
`archive_blake3` fingerprint in v1.x. The plan's `Fingerprint` type is a two-variant
tagged enum — `archive_blake3 { value, size, mtime_unix }` or
`none { reason: "directory_source_v1" }` — never a bare JSON `null`. A `null` would
read as "unknown, proceed"; the tagged variant forces #76 down a declared per-entry
fallback and makes a future fingerprint scheme arrive as an unknown variant an old
reader must refuse, rather than silently degrading.

### Canonicalization: the whole file, one printer, declaration-order keys

`serde_json::to_string_pretty` (two-space indent), exactly one trailing `\n` inside
the compared bytes. **Key order is struct declaration order** — no
`#[serde(flatten)]`, no map type anywhere in the document, no `serde_json::Value` on
any serialization path, no `#[serde(skip_serializing_if)]`. This is the load-bearing
choice: serde writes struct fields straight to the writer with no intermediate map,
so declaration order needs no external guarantee to hold. The alternative —
alphabetical key order via a `serde_json::Value` round-trip — was seriously
considered (it evolves better: a new field lands where it sorts) and rejected,
because it can only be achieved by routing through `serde_json`'s `Map`, whose
ordering is a `BTreeMap` only until any crate anywhere in the dependency graph
unifies the `preserve_order` feature, at which point it silently becomes an
`IndexMap` and every plan ever written re-serializes with different bytes.
`Cargo.lock` carries no `indexmap` today — the hazard is latent, which is exactly
when it is cheapest to design out rather than guard with a test someone can delete.

Every collection is a `Vec` sorted in Rust on a declared **total** key before
serialization — never on SQLite row order (`src/dedup.rs:515`'s `fetch_scope` has no
`ORDER BY`) and never on `HashMap`/`HashSet` iteration, which is reseeded per
process and would pass a same-process test while failing a cross-process one.
`BTreeMap` is permitted for keyed *lookup* inside canonicalization; iterating one for
*ordering* is not.

### The sort key must be total — a design-time defect this ADR corrects

The design's first draft froze entry ordering on `(content_hash, raw_path_bytes)`.
That key is not total: `src/dedup.rs` (~line 251) deliberately groups rows sharing
both a path and a content hash — the shadowed-entry case — so two such rows are
indistinguishable under that key and the tie falls through to SQLite's unspecified
row order, the exact class of bug ADR 0003 already fixed once for search ranking.
Every sort key in this contract instead carries a final tiebreak of
`(root_id, file_id)`, which is injective over every row a plan can reference because
it mirrors the master catalog's own `(archive_id, file_id)` primary key
(`src/master.rs:283`).

### Conflict-ordinal assignment is part of canonicalization — the defect the design workflow found

When two `move`/`extract` actions collide on the same destination name, the loser
gets a `-N` suffix. Every one of three independent design proposals assigned that
suffix while walking the *unsorted* input and sorted the action array only
afterward — sorting a list cannot repair a name already baked into one of its
elements, so two runs could hand the same two colliding files different final names
while both plans still validated. The fix, frozen here: the collision set is the
complete set of actions sharing `(dest.root_id, base destination components)`; rank
is assigned by the *same* total key the action array itself sorts by; rank 0 keeps
the plain name; rank `N ≥ 1` rewrites the final path component to `{stem}-{N}{ext}`,
splitting on the last `.` in the component. Every member of the collision set —
including rank 0 — records the complete, symmetric `competing_with` set, which is
what makes the rank independently checkable by `verify_derived`.

### Content-derived identifiers, and the circular dependency the synthesis pass found

`action_id`, `group_id` and `slot` are derived by BLAKE3 over a length-framed
preimage (`u64` little-endian length prefix per element, so the encoding is
injective including for `mkdir`, which has no source) under a distinct domain
separator per identifier kind. They are content-derived, not positional: regenerating
a plan after one entry changed leaves every other identifier unchanged, so a resumed
run's future execution journal still matches.

The design's first cut derived a quarantine action's destination path from its
`slot`, and derived `slot` from a preimage that included the destination —
circular, and unimplementable. No individual judging lens caught it (each was
reading through one dimension); the final synthesis pass did, by writing the
derivation out end to end. The fix: `slot` derives from
`(group_id, source.root_id, source.file_id, source.content_hash)` only — never the
destination — which breaks the cycle. The quarantine destination is then built from
`slot`, not the reverse.

### `verify_derived`: canonicalize a clone, compare, refuse on disagreement

Every derived field — `root_id`, `display`, every content-derived identifier,
`Summary`, and `ReplicaFloor.remaining_after` — is recomputed by cloning the plan,
canonicalizing the clone, and comparing it to the original. Any disagreement refuses
the whole plan. This runs inside `Plan::to_canonical_bytes` and
`Plan::from_canonical_bytes` — at both write time and load time, not only in tests —
so a hand-edited display path, a doctored summary total, a forged identifier, or a
reordered array is a malformed plan, never a plan a reviewer could be fooled by.

`ReplicaFloor.remaining_after` deserves its own note: it is fully derivable from a
group's own contents (the distinct root set of the keeper plus every member *not*
being quarantined — a dedup group is exhaustive over every registered, reachable
copy by construction) and so `canonicalize` recomputes it. `ReplicaFloor.observed`
is not derivable the same way — it is a fact about the live corpus at plan-build
time that only #76's live re-count can verify, exactly like a source root's
`archive_blake3`.

### The symlink question #77 flags as genuinely ambiguous

Applying a plan from a symlink to a regular file: **refused**, not accepted.
`#77`'s hard rule is "apply accepts only a persisted regular file"; a symlink adds a
TOCTOU window between review and apply (the link target can change without the plan
file itself changing) that a plain regular file does not have. The safer default is
adopted here so #77 does not have to relitigate it under a different set of
pressures; #77 may revisit with an explicit `--follow-symlink` opt-in if a real
workflow needs it, but the default stays closed.

### Compatibility asymmetry

Unknown top-level **fields** are ignored (no `#[serde(deny_unknown_fields)]`) — a
shipped v1.1 reader must survive a field v1.3 additively appends. Unknown
**instruction variants** (`op.action`, `fingerprint.kind`, `disposition`, and every
other closed enum that names an action or a structural role) are a hard
deserialization error that refuses the whole plan — an executor that silently skips
an operation it does not recognize would apply a partial plan while reporting
success, which is worse than refusing outright. Evidence fields (`reason` strings,
`hash_algo`, `phash_algo`, `keep_policy`, `label`) stay open `String`s deliberately:
refusing a whole plan because the binary does not recognize the reason a file was
left alone is indefensible.

## Alternatives considered

- **Alphabetical key order via a `Value` round-trip** — evolves more gracefully but
  is reachable by the `preserve_order` hazard described above; rejected in favor of
  declaration order, which the serializer cannot get wrong regardless of what any
  dependency does.
- **Base64 for raw path bytes** — the more common general-purpose choice, rejected
  in favor of matching `src/report.rs`'s existing lowercase-hex convention. A second
  encoding inside one product costs more than base64's marginal density advantage
  buys back.
- **A version field or digest embedded inside the plan body** — rejected: it would
  either have to be excluded from the byte comparison (defeating "the whole file is
  the canonical body") or would make the plan's own bytes depend on when it was
  computed, breaking acceptance criterion 1 outright.
- **JSON Schema validation only, no byte-fixture comparison** — the existing
  `tests/contract.rs` machinery compares via `serde_json::Value` semantic equality,
  which cannot see a key-order change, a whitespace change, or an integer silently
  becoming a float — precisely the properties this contract exists to freeze. Plan
  fixtures therefore get their own byte-exact comparison in a separate suite and
  directory (`tests/plan_contract` logic lives inside `src/plan.rs`'s own
  `#[cfg(test)]` module — see Consequences), reusing ADR 0003's `BACKUPSAGE_BLESS=1`
  regeneration semantics but not its comparator.
- **`schemars`-generated JSON Schema** — rejected: the schema is the frozen,
  reviewed contract, and a generated schema churns with its generator's own version
  rather than with a deliberate review of what changed.

## Consequences

- **Fixture tests live inside `src/plan.rs`'s own `#[cfg(test)]` module, not in a
  separate `tests/plan_contract.rs` integration test.** This was not a stylistic
  choice — it is forced by a Rust compilation boundary the design's fixture plan did
  not anticipate. `plan::fixtures` is `#[cfg(test)]`-gated so that no plan-emitting
  code compiles into a normal build (the exact guarantee #77 needs: nothing that
  emits plan bytes ships in the product binary). `#[cfg(test)]` is active only when
  the library compiles its *own* unit-test binary; a separate integration-test crate
  links the library as an ordinary dependency, where `#[cfg(test)]` is never set, so
  `plan::fixtures` is invisible to it. Confirmed directly with a throwaway probe
  file before committing to this structure. The practical effect: `cargo test --lib
  plan::contract::` runs the plan suite; there is no `tests/plan_contract.rs`.
- **`docs/CONTRACT.md` gains a new surface**: the plan document, typed structs in
  `src/plan.rs`, `plan_schema_version: 1`, described as byte-exact — stricter than
  every existing JSON surface, because formatting and key order are part of the
  frozen contract, not incidental to it. `docs/COMPATIBILITY.md` gains the matching
  v1.x promise: a plan file written by an earlier v1.x release remains loadable by a
  later one.
- Two implementation bugs surfaced during test-driven development, after this
  design was already frozen, and both are now guarded by the test suite: the root-id
  remap was originally keyed by a root's position in `self.roots` rather than by its
  `root_id` field (caught by a fixture whose vec order and field values
  deliberately differed), and a suffixed destination's `display` was derived before
  the suffix was applied rather than after (caught by asserting the resolved
  filename, not just the ordinal).
- A fresh-reader review (codex, read-only, before this branch's PR) found two further
  real defects the design's own tests had not covered: the root-id remap's fallback
  silently passed through a *dangling* reference (a `root_id` that was never a real
  root) rather than refusing it, and `base_components` recovered a destination's
  pre-suffix name by heuristically stripping any trailing `-N`, indistinguishable
  from a source file genuinely named e.g. `report-1.txt`. Both are now fixed —
  the remap is preceded by an explicit closure check over every reference, and
  `base_components` reads the entry's own prior `Conflict` field (ground truth for
  what this module actually suffixed) instead of guessing from bytes.
- **Residual risk, accepted deliberately**: full N-way collision resolution against
  RENAME TARGETS — a collision loser's suffix landing on a destination a third,
  unrelated entry already occupies by its genuine name — is not solved here.
  `canonicalize`'s collision grouping is single-pass, keyed on pre-suffix bases,
  not on final post-rename names. What this issue owes, and delivers, is that the
  case never corrupts silently: `invariants_hold`'s existing destination-uniqueness
  check refuses such a plan rather than applying it with a lost or overwritten file.
  Iterative/fixed-point resolution so more such plans succeed instead of refusing is
  future work, not #75's scope.
- `#16` (plan generation from live dedup/organize data) and `#76`/`#77` (precondition
  verification and the apply path) inherit every field frozen here without needing
  to renegotiate the shape. #76 in particular can rely on `Fingerprint`'s tagged
  absence to know exactly what it can and cannot prove for a directory source.
- **Residual risk, accepted deliberately**: `index_schema_version` is checked for
  strict equality by #76 (not implemented in this issue, but the field exists for
  it). A future in-place index migration that changed the schema version while
  preserving `index_uuid` would cause every plan computed against the old schema to
  be refused. This is fail-closed by design — a plan refusing to apply is always
  recoverable; a plan applying against a schema it was not computed for is not.

## Verification

25 tests in `src/plan.rs`'s `plan::contract` module (21 from the original design
plus 4 written in response to the fresh-reader review, two per finding) cover all
four acceptance criteria, four byte-exact golden fixtures (one per plan kind plus
the empty-dedup case), schema validation with six negative cases, round-trip
coverage of every tagged enum variant, hand-edited-derivation rejection,
structural invariant rejection, the conflict-ordinal cross-cutting defect,
idempotence, and both review findings (with a paired test proving the harder
unsolved rename-cascade case fails closed rather than corrupting). The
cross-process determinism criterion (acceptance criterion 2) re-execs the test
binary itself via `std::env::current_exe()` rather than shipping a plan-emitting
dev command, for the same #77-driven reason fixtures stay `#[cfg(test)]`-gated.

Every determinism guard is mutation-proven through `failure-atlas`, two ways per
guard, per the standing rule that one `caught` verdict proves a test noticed *that*
specific break, not that it pins the invariant generally. Results recorded in
`design-docs/2026-08-28-issue-75-decision-log.md`.

## Safety invariant

BackupSage never rewrites or deletes from an archive.
