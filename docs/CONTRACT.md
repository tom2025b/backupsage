# Public contract: JSON surfaces and exit codes

This document declares the stability status of every machine-readable
surface BackupSage exposes, and how the golden fixtures that freeze them
work. The compatibility *window* (how long these promises hold, across
which versions) is tracked separately in issue #57.

## Golden fixtures

Every surface below is frozen as a committed fixture under
`tests/fixtures/contract/` and compared on every test run by
`tests/contract.rs` — ten fixtures: one per surface plus
completed-with-skips variants of `search --all --json` and `dedup --json`
(populated `skipped[]` / `archives_offline`), plus `dedup_chain.json`,
which pins the keeper-star fields (#9) at non-degenerate values — a real
transitive-only member with `actionable: false` and `hamming_to_keep > 3` —
plus two more non-degenerate content-mode corpora (#71): `search_only.json`
(a `search-only` archive's `mode` field on `search --all --json`) and
`metadata_only_skips.json` (a `metadata-only` archive's worded skip on
`dedup --json`, alongside a normal duplicate pair so `summary.groups`
stays non-zero too):

```bash
cargo test --test contract          # verify against the frozen contract
BACKUPSAGE_BLESS=1 cargo test --test contract   # regenerate deliberately
```

Comparison is semantic JSON equality after normalizing the only
run-varying values (temp-directory prefixes become `<TMP>`, the
wall-clock `indexed_unix` becomes `"<TS>"`). Any field rename, removal,
addition, or type change fails the suite. **The contract is
additive-only**: a re-bless whose diff adds fields may ship; a re-bless
whose diff renames or removes fields is a breaking change and must not.
Fixture generation only reads archives and writes to fresh temp paths —
BackupSage never rewrites or deletes from an archive.

## JSON surfaces

| Surface | Shape | Status |
|---|---|---|
| `dedup --json` | Typed structs in `src/report.rs`, top-level `"version": 1` | **Stable.** Versioned, additive-only, consumed by scripts and the future web UI. v1.0.2 (#9) added `Member.actionable`, `Group.review_only_bytes`, `Summary.transitive_only_files`/`review_only_bytes`, `params.actionable_rule` — additive, still version 1. `reclaimable_bytes`/`duplicate_files` now count only keeper-star-safe members (correctness fix: previously inflated by transitive-only members whose distance to the keeper exceeds the threshold). v1.0.2 (#71) also added `ReportArchive.content_mode` and a `metadata-only` case in `summary.skipped_archives` (alongside the existing `v2-limited` case) — additive, still version 1. A metadata-only-only master now exits 2 where it previously exited 0 with an empty, unexplained report. |
| `search --json` | `{query, hits[], truncated, mode}`; hits carry `path`, `matches`, `snippet`, and `path_bytes` (hex) only for non-UTF-8 paths | **Frozen-as-observed.** No version field yet; fixture-protected; changes must be additive. v1.0.2 (#70) added top-level `mode` (`full`/`search-only`/`metadata-only`) — additive. On a `search-only` index `matches` is `null`: contentless FTS cannot run `highlight()`, and `snippet` is absent for the same reason. Full-mode output is byte-identical to earlier v1.x. |
| `search --all --json` | `{archives[{archive, truncated, mode, hits[]}], skipped[{archive, reason}]}` | **Frozen-as-observed.** Same rules as `search --json`, including `matches: null` from a `search-only` child. A `metadata-only` child is never searched: it appears in `skipped` with a worded reason, which is exit 2. v1.0.2 (#71) added the per-archive `mode` field — additive, present on every successfully-searched archive. `--snippets` against a search-only archive now also prints a stderr note, matching the single-archive path's existing note. |
| `master list --json` | Array of `{archive_id, label, source, source_type, db_path, schema_version, files, completed, status, indexed_unix, content_mode}` | **Frozen-as-observed.** Same rules. v1.0.2 (#71) added `content_mode` — additive. |
| `master verify --json` | Array of `{archive_id, label, status, source}` | **Frozen-as-observed.** Same rules. |
| `diff --json` | The engine's typed report in `src/core/diff.rs`, top-level `"version": 1`, flattened with `inputs` (per-side health from `src/core/diff_input.rs`) and `move_inference` | **Stable, versioned, additive-only (#93).** Pinned byte for byte, after replacing temp paths and index UUIDs, by `tests/diff_cli.rs` against `tests/fixtures/diff_cli/`, alongside the terminal rendering. It is not part of `tests/contract.rs`'s set. Enum values (`kind`, `reason`, `state`, `source_currency`, note `code`, blocker `cause`) are snake_case strings, and a new value is an additive change. Every path carries hex raw bytes (`path_bytes`, `db_path_bytes`). See ADR 0010. |
| `coverage --json` | Typed structs in `src/core/coverage_report.rs`, top-level `"version": 1`: `coverage_state`, `params`, `sources[]`, `groups[]`, `excluded_groups[]`, `unknown_content[]`, `excluded_rows[]`, `summary` | **Stable, versioned, additive-only (#98).** Pinned byte for byte, after replacing temp paths, by `tests/coverage_cli.rs` against `tests/fixtures/coverage_cli/`, alongside the terminal rendering; not part of `tests/contract.rs`'s set. Every list is sorted by the report on explicit keys: sources and presence by source id; groups and excluded groups by content hash; copies, aliases, unknown-content and excluded rows by (source id, raw path bytes, file id). Registration order changes only the source ids a master assigns. The terminal rendering (also fixture-pinned) lists groups in the same content-hash order, marking each line's verdict; it never regroups by verdict. `params.kind` and the `--kind` filter were added in the same change (#98 review round 1), before any release. Every `summary` total is counted from the emitted lists. Enum values (`verdict`, `evidence`, presence `state`/`reason`, row `reason`, note `code`) are snake_case strings, and source and copy `status` use the master's strings (`ok`, `stale-index`, …); a new value is an additive change. Every path carries hex raw bytes (`path_bytes`, `db_path_bytes`). `coverage_state` is `complete` only when every source is a complete, `ok` source and no emitted group or row is unknown. See ADR 0011. |
| Plan document | Typed structs in `src/plan.rs`, top-level `"plan_schema_version": 1`, schema at `docs/schema/plan-v1.schema.json` | **Stable, versioned, BYTE-EXACT** — stricter than every surface above: key order (struct declaration order), whitespace (`to_string_pretty`, two-space indent, one trailing newline) and number formatting are all part of the contract, not incidental to it. `plan_schema_version` is independent of `index_schema_version` and of the dedup report's `version: 1`. Compared by raw byte equality in `src/plan.rs`'s own `#[cfg(test)] mod contract`, not by `tests/contract.rs`'s semantic `serde_json::Value` comparator, which cannot see a key-order or whitespace change. An execution boundary must load through `Plan::load_from_regular_file` (stdin, symlinks and non-regular files refused), then pass `Plan::verify` against current index UUIDs, schema versions, archive BLAKE3 values and per-entry raw paths/content hashes before action one. This contract provides no production executor. See ADR 0006 (#75/#76/#77). |

"Frozen-as-observed" means: the shape carries no version field today, but
the golden fixtures pin it exactly, so any non-additive change is caught
in CI. Giving these surfaces a version field is itself an additive change
and may happen in a later release.

Known residual gaps (fields the fixtures pin only at a degenerate value,
so a *type* change to them would not be caught): `hardlink_of` is null on
every corpus member (no hardlink entries yet), and `archives_incomplete`
appears only as an empty array. `sparse` stays pinned false in dedup
output *by design* — sparse rows are excluded from dedup candidates at
the SQL level — so its evidence lives elsewhere: the #64 differential
corpus (`tests/fixtures/sparse/`, four GNU-tar-built dialect archives,
frozen-as-observed) proves old-GNU logical size/bytes/BLAKE3 against GNU
tar across tar/gz/zstd, pins the PAX name-only shape, and drives every
malformed-map case to a loud abort in `tests/sparse.rs`. The element
shape of `summary.skipped_archives` is additionally pinned by an inline
assertion in `tests/cli.rs` (v2-limited flow). Field *renames and
removals* of all of these are still caught — the keys themselves are in
the fixtures.

All other commands (`index`, `top`, `inspect`, `master add/sync/rm`)
produce human-oriented text only — **unstable**, no machine-readable
promise.

## Exit codes

Documented in `src/main.rs` and executed as a per-subcommand matrix in
`tests/contract.rs::exit_code_matrix`:

| Code | Meaning |
|---|---|
| `0` | Completed cleanly. Includes "no results" and "no duplicates". |
| `1` | Error: bad input, missing file/index/master, unknown key or path. For `diff`, a missing index is not an error: it is an unavailable input (exit 2). `diff` exits 1 when the engine refuses contradictory rows. `coverage` exits 1 on a missing or in-use master, a floor of 0, or an `--archive`/`--protected` value that names no source or is ambiguous. |
| `2` | Completed **with skips**: `dedup` skipped offline/incomplete/v2-limited archives; `search --all` skipped offline or unreadable archives (v2-limited archives are fully searchable and do not cause exit 2); `master verify` found any non-`ok` archive; or `diff`'s `comparison_state` is not `complete` (an input unavailable, incompatible or incomplete, or any inconclusive row). A stale or offline source alone does not cause exit 2 for `diff`. `coverage` exits 2 when its `coverage_state` is not `complete`: any source is not a complete, `ok` source (unreachable, unavailable, stale, incomplete or without content hashes), or any reported group is inconclusive, or any reported row's content is unknown. Content below the floor alone is a finding and exits 0. |

Caveat frozen as observed behavior: **clap usage errors also exit 2**
(unknown flag, missing subcommand). They are distinguishable from
completed-with-skips — usage errors print help/usage text to stderr and
perform no work. Scripts distinguishing the two should treat exit 2 with
a usage message on stderr as an invocation bug, not a skip report.
