# ADR 0005 — content_mode as a replicated fact, not a per-surface re-derivation

Date: 2026-08-13 · Status: accepted · Milestone: v1.0.2 (issue #71, child of #39)

## Context

#70 introduced `ContentMode` (`full`/`search-only`/`metadata-only`) and stored
it as a `content_mode` meta key on each per-source index, with three
read-time gates (`search`, `top`, snippet availability) keyed off
`store::content_mode`. ADR 0002's consequences section anticipated exactly
this follow-up: "Future schema metadata cleanup (v1.0.2 'explicit index
modes') should fold the capability marker into a proper schema registry."
#70 explicitly scoped master/dedup replication out ("Master/dedup
replication is #71", `tests/modes.rs:2`) because the master catalog has no
notion of content mode at all — `dedup`'s `fetch_scope` filters on
`content_hash IS NOT NULL`, so a metadata-only archive (which never hashes
content by design) simply produced zero rows with no explanation, and
`master list`/`search --all --json` had no way to say which mode an archive
was built in without opening its per-source `.db` directly.

## Decision

- **Replicate the fact once, at `Master::add()` time**, into a new
  `archives.content_mode` column — same probe-then-ALTER migration shape as
  `path_raw` (ADR 0002): new masters get the column in `CREATE TABLE`
  directly, masters opened from an on-disk file predating this change get it
  via `ALTER TABLE ... ADD COLUMN content_mode TEXT NOT NULL DEFAULT 'full'`.
  It is derived via the existing `store::content_mode` grandfather logic
  (absent meta key ⇒ `full`), not re-implemented.
- **Every consuming surface gets one additive field, named to match its
  neighbors.** `master list` (table + `--json`) and `dedup --json`'s
  `ReportArchive` use `content_mode` (matching `ArchiveRow`'s own field
  name and the existing `schema_version`/`status` style). `search --json`
  and `search --all --json` use `mode` (matching the top-level `mode` field
  #70 already put on single-archive `search --json`, so the two search
  surfaces read consistently).
- **Metadata-only archives become a counted dedup skip**, not a silent
  empty report. `Summary.skipped_archives` — until now populated only by
  `v2-limited` archives — gains a second, additive case: any archive whose
  `content_mode` is `metadata-only`. This closes the exact gap named in the
  issue: `fetch_scope`'s `content_hash IS NOT NULL` filter (`src/dedup.rs`)
  already excluded these rows correctly; what was missing was telling the
  caller why the count is zero. `has_skips()` (and therefore exit code 2)
  now fires for a metadata-only-only master, matching the existing
  v2-limited exit-code contract.
- **`search --all` gets the same search-only snippets note the single-archive
  path already had.** `search()` (single-index) has printed
  `"note: search-only index — snippets are unavailable"` since #70; the
  federated path silently dropped snippets with no note. `FederatedHit` now
  carries `content_mode`, and the CLI prints the same note per search-only
  archive when `--snippets` was requested.

## Alternatives considered

- **Re-derive `content_mode` per surface by opening each source `.db` at
  render time** — rejected: the master's whole design point is answering
  registry-shaped questions (`list`, `dedup`, federated `search`) without
  every source database being reachable (`archives_offline` exists
  precisely because sources go offline); re-deriving would silently lose
  the mode fact for exactly the archives most likely to need it explained.
- **One shared field name (`mode`) everywhere** — rejected: `master list`
  and `dedup`'s `ReportArchive` already name their other replicated facts
  after the `ArchiveRow` column (`schema_version`, `status`, `source_type`);
  breaking that pattern for one field to match `search --json`'s unrelated
  precedent would make the two surfaces internally inconsistent instead of
  externally consistent. Naming to match each surface's *existing* sibling
  fields costs nothing and reads better in both places.
- **A full mode *registry* table (id → capabilities bitmap), per ADR 0002's
  literal wording** — rejected as premature: there are exactly three modes,
  they do not vary independently (each already has one canonical
  capability set enforced in `store.rs`/`searcher.rs`), and a registry
  table buys nothing until a fourth mode or per-mode-capability query
  actually exists. The `content_mode: TEXT` column is that registry's
  degenerate/correct-for-now form; revisit only if that changes.

## Consequences

- `master list`, `dedup --json`, and `search --all --json` all gained one
  additive field each; `docs/CONTRACT.md` and `docs/COMPATIBILITY.md`
  record the exact keys. No existing field renamed, removed, or changed
  type — the fixture re-bless diffs are additions only.
- A metadata-only-only master now exits 2 from `dedup` where it previously
  exited 0 with an empty report. This is a **behavior change** within the
  documented exit-code contract's own terms (`2` = "completed with skips"),
  not a new exit code or a new meaning for an existing one — scripts already
  treating exit 2 as "check `skipped_archives`" see one more legitimate
  reason to.
- `tests/fixtures/contract/search_only.json` and `metadata_only_skips.json`
  join the frozen set (ten fixtures total) so both non-`full` modes are
  pinned at real values on at least one surface each, per ADR 0003's
  residual-gaps note.
- The two-line README security note (search-only reduces stored content but
  is not encryption; metadata-only stores no content-derived data) is now
  the single place that states the privacy posture of all three modes
  together, closing the gap where #70 documented the read-time behavior but
  not the honest security framing.
