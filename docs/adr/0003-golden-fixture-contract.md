# ADR 0003 — Golden-fixture freeze of the public JSON and exit-code contract

Date: 2026-07-24 · Status: accepted · Milestone: v1.0.2 (issue #55, child of #11)

## Context

The public machine-readable contract existed only as code and scattered
assertions: `src/report.rs` declares the dedup report a stable `version: 1`
shape, but the other four JSON surfaces (`search --json`,
`search --all --json`, `master list --json`, `master verify --json`) are
inline `serde_json::json!` blobs with no version field, and the 0/1/2
exit-code contract was asserted piecemeal across integration tests. A
silent field rename, removal, or type change — or a behavioral drift in an
exit code — could ship without any test noticing. Issues #13 and #15 will
add new JSON-emitting commands and need a foundation that makes contract
drift impossible to miss.

## Decision

- **Committed golden fixtures** under `tests/fixtures/contract/` freeze
  all five JSON surfaces, plus completed-with-skips variants of the two
  surfaces that have one (`search --all`, `dedup`), so skip-reporting
  shapes are pinned populated rather than as empty arrays. Seven fixtures
  total, compared by **semantic JSON equality** in `tests/contract.rs` on
  every test run; the test also asserts the fixture directory contains
  exactly the frozen set, so a renamed surface cannot leave an orphaned
  fixture silently exempt.
- **Normalization is minimal and explicit**: temp-directory prefixes (both
  the raw and canonicalized spellings) become `<TMP>`, and the wall-clock
  `indexed_unix` becomes `"<TS>"`. Everything else must be deterministic,
  which the corpus guarantees by construction (fixed mtimes, seeded PNGs,
  fixed payloads, a byte-fixed EXIF TIFF).
- **Regeneration is a deliberate act**: `BACKUPSAGE_BLESS=1 cargo test
  --test contract` rewrites the fixtures. The contract is
  **additive-only** — a re-bless whose diff adds fields may ship; one that
  renames or removes fields must not (`docs/CONTRACT.md`).
- The four unversioned surfaces are declared **frozen-as-observed** rather
  than versioned now: adding a version field later is itself an additive
  change, so nothing is lost by deferring it.
- **Exit codes are executed, not just documented**: a table-driven matrix
  runs every subcommand's 0/1/2 paths, including completed-with-skips
  (offline dedup, federated-search skip, archive-missing verify isolated
  on its own single-archive master) and clap's usage-error exit 2, frozen
  as observed behavior distinct from skip-2.
- One product change rides along: search results order by
  `rank, rowid` instead of bare `rank`. Equal-rank hits previously fell
  back to SQLite's unspecified tie order — fixtures would have pinned an
  accident of the bundled SQLite build, and any SQLite bump could legally
  reorder ties and fail CI with no behavioral change. Additive; no shape
  change.

## Alternatives considered

- **`insta` snapshot crate** — mature review tooling, but adds a dev
  dependency and its own redaction DSL for what is here five small JSON
  documents; a ~60-line harness keeps the whole mechanism readable inside
  the repo and its bless semantics explicit.
- **JSON Schema validation instead of golden values** — catches type
  changes but not value regressions (a keeper flipping to the wrong
  member validates fine); golden values catch both, and the schema can be
  derived later from the fixtures if wanted.
- **Version every surface now** — rejected as a contract change smuggled
  into a testing issue; frozen-as-observed gives the same regression
  protection, and adding `version` later is additive.
- **Fixing clap's usage-error exit to 1** — would make exit 2
  unambiguous, but it is a user-visible behavior change out of scope for
  a freeze; instead the collision is documented (usage errors print
  usage text to stderr and perform no work) and pinned as observed.

## Consequences

- Any field rename, removal, addition, or type change in a frozen surface
  fails `cargo test --test contract` — additions are unblocked by an
  explicit re-bless commit whose fixture diff shows exactly what was
  added. CI runs the suite automatically (`cargo test --locked`).
- #13 (snapshot diff) and #15 (integrity checks) inherit the pattern:
  new JSON surfaces should ship with a fixture and a `version` field from
  day one, following the `report.rs` typed-struct style.
- The corpus freezes non-degenerate values (hamming distance 2, non-null
  `exif_unix`, `path_bytes` on three surfaces), so distance computation
  and raw-path emission cannot silently collapse to defaults.
- Known residual gaps are documented in `docs/CONTRACT.md`: `hardlink_of`
  and `sparse` are pinned only at null/false until the #8 sparse corpus
  exists; `archives_incomplete` only as an empty array. Renames/removals
  of those keys are still caught.
- Fixture generation only reads archives and writes to fresh temp paths —
  the archive-immutability invariant is untouched.
- Future SQLite/rusqlite bumps can no longer flake the fixtures through
  tie ordering; if a bump changes BM25 itself, the fixture diff will show
  it as a ranking change, which is the honest signal.
