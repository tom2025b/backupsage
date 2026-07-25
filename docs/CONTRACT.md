# Public contract: JSON surfaces and exit codes

This document declares the stability status of every machine-readable
surface BackupSage exposes, and how the golden fixtures that freeze them
work. The compatibility *window* (how long these promises hold, across
which versions) is tracked separately in issue #57.

## Golden fixtures

Every surface below is frozen as a committed fixture under
`tests/fixtures/contract/` and compared on every test run by
`tests/contract.rs` — seven fixtures: one per surface plus
completed-with-skips variants of `search --all --json` and `dedup --json`
(populated `skipped[]` / `archives_offline`):

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
| `dedup --json` | Typed structs in `src/report.rs`, top-level `"version": 1` | **Stable.** Versioned, additive-only, consumed by scripts and the future web UI. |
| `search --json` | `{query, hits[], truncated}`; hits carry `path`, `matches`, `snippet`, and `path_bytes` (hex) only for non-UTF-8 paths | **Frozen-as-observed.** No version field yet; fixture-protected; changes must be additive. |
| `search --all --json` | `{archives[{archive, truncated, hits[]}], skipped[{archive, reason}]}` | **Frozen-as-observed.** Same rules as `search --json`. |
| `master list --json` | Array of `{archive_id, label, source, source_type, db_path, schema_version, files, completed, status, indexed_unix}` | **Frozen-as-observed.** Same rules. |
| `master verify --json` | Array of `{archive_id, label, status, source}` | **Frozen-as-observed.** Same rules. |

"Frozen-as-observed" means: the shape carries no version field today, but
the golden fixtures pin it exactly, so any non-additive change is caught
in CI. Giving these surfaces a version field is itself an additive change
and may happen in a later release.

Known residual gaps (fields the fixtures pin only at a degenerate value,
so a *type* change to them would not be caught): `hardlink_of` and
`sparse` are null/false on every corpus member (no hardlink or sparse tar
entries yet — the sparse corpus is issue #8), and `archives_incomplete`
appears only as an empty array. The element shape of
`summary.skipped_archives` is additionally pinned by an inline assertion
in `tests/cli.rs` (v2-limited flow). Field *renames and removals* of all
of these are still caught — the keys themselves are in the fixtures.

All other commands (`index`, `top`, `inspect`, `master add/sync/rm`)
produce human-oriented text only — **unstable**, no machine-readable
promise.

## Exit codes

Documented in `src/main.rs` and executed as a per-subcommand matrix in
`tests/contract.rs::exit_code_matrix`:

| Code | Meaning |
|---|---|
| `0` | Completed cleanly. Includes "no results" and "no duplicates". |
| `1` | Error: bad input, missing file/index/master, unknown key or path. |
| `2` | Completed **with skips**: `dedup` skipped offline/incomplete/v2-limited archives; `search --all` skipped offline or unreadable archives (v2-limited archives are fully searchable and do not cause exit 2); or `master verify` found any non-`ok` archive. |

Caveat frozen as observed behavior: **clap usage errors also exit 2**
(unknown flag, missing subcommand). They are distinguishable from
completed-with-skips — usage errors print help/usage text to stderr and
perform no work. Scripts distinguishing the two should treat exit 2 with
a usage message on stderr as an invocation bug, not a skip report.
