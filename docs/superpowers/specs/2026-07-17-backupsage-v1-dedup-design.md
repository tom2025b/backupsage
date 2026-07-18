# BackupSage v1.0 — Cross-Archive Dedup Design

**Date:** 2026-07-17
**Status:** Approved (Tom, 2026-07-17: build in this repo, local + GitHub)
**Supersedes:** BackupSage v0.2 (text search only) — absorbs the core ideas of
[photo-organizer](https://github.com/tom2025b/photo-organizer) (perceptual dedup, EXIF dates).

---

## 1. Goal

One Rust tool that indexes tar backups **and** directories — one index file per
source, exactly as BackupSage does today — and then answers, across *all* of
them at once:

- Which files are exact duplicates (identical content, any filename)?
- Which photos are near-duplicates (perceptual match)?
- For each duplicate group: where does each copy live, and **which is newest**
  (with honest timestamp provenance so the user decides)?
- Plus everything v0.2 already answers (full-text search, top words), now
  across every indexed archive via a **master index**.

**v1.0 is strictly read-only over user data.** The only files it ever creates
or modifies are `.db` index files and the master catalog. No code path writes
into an archive or a source directory. Acting on duplicates (organize,
extract, delete scripts) is deferred to v1.1+.

## 2. Non-goals for v1.0

- Mutating, rewriting, or extracting archives (never, at any version, for
  mutation; extract is v1.1).
- HEIC/RAW pixel decode (EXIF dates and exact hashing still work — see §8).
- Video perceptual hashing (exact hash only, same as photo-organizer).
- Web UI (v2.0 — but every command grows `--json` now, and that JSON is the
  future web API contract).
- Incremental/watch re-indexing; a re-index is always a full rebuild.

## 3. Architecture

Single binary `backupsage` (name kept: `sage` collides with SageMath's
binary), library-first as today. Three planes with one-way data flow:

```
1. INDEX  (per source, one streaming pass — write path)
   tar:  File → blake3 archive-hash wrapper → progress → BufReader(256K)
              → zstd/MultiGzDecoder/none → tar entries
   dir:  walkdir → File per entry (same downstream pipeline)
   per entry: chunked read to EOF
              ├─ every chunk → per-file BLAKE3 (full content, past any cap)
              └─ first N bytes retained → head buffer
                   ├─ null-byte probe (8K)          → text | binary
                   ├─ text → FTS5 + word stats       (v0.2 logic unchanged)
                   ├─ image ext → decode (capped) → w/h + pHash; EXIF date
                   └─ raw/video ext → EXIF date attempt only
   output: <source>.db  (schema v3, single file, WAL folded back)

2. MASTER (replication — metadata only, never text)
   master add/sync:  ATTACH one per-source DB at a time (read-only URI)
                     → INSERT INTO files SELECT …  → DETACH
   staleness: two layers (index_uuid replica freshness; archive stat/hash)

3. QUERY
   dedup:        runs 100% inside master.db (works with all archives offline)
   search --all: federated fan-out — open each registered per-source DB
                 read-only, run FTS5 MATCH, merge grouped per archive
                 (BM25 is not comparable across indexes; grouped display)
   search/top on one DB: exactly as v0.2
```

Module map (single crate, lib/bin split as today):
`src/{format, source_tar, source_dir, indexer, phash, exif_date, store,
master, dedup, searcher, report, cli}.rs` + `main.rs` (rendering only).
A future axum server (v2.0) imports the lib and serves the same JSON shapes;
promote to a cargo workspace only when that lands.

## 4. Per-source index — schema v3

`files_fts` keeps its exact v2 shape; the new `files` table is the metadata
spine, paired by explicit rowid:

```sql
-- unchanged from v2:
CREATE VIRTUAL TABLE files_fts USING fts5(path, content, tokenize='unicode61');
CREATE TABLE word_freq (word TEXT PRIMARY KEY,
                        total_count INTEGER NOT NULL DEFAULT 0,
                        doc_count   INTEGER NOT NULL DEFAULT 0);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- new in v3:
CREATE TABLE files (
    id            INTEGER PRIMARY KEY,   -- == files_fts rowid for the same entry
    path          TEXT NOT NULL,
    entry_type    TEXT NOT NULL,         -- 'file' | 'symlink' | 'hardlink'
    link_target   TEXT,                  -- links only
    size          INTEGER NOT NULL DEFAULT 0,  -- tar header size / stat size
    mtime_unix    INTEGER,               -- tar header mtime / stat mtime  (v2 discarded this)
    mode          INTEGER,               -- unix mode bits, informational
    kind          TEXT NOT NULL,         -- 'text'|'image'|'raw'|'video'|'binary'|'link'|'empty'
    content_hash  BLOB,                  -- 32-byte BLAKE3 of FULL content; hardlinks resolved
                                         -- post-pass; NULL for symlinks/read-errors
    img_w         INTEGER,               -- images only, free from decode
    img_h         INTEGER,
    phash         INTEGER,               -- 64-bit DCT pHash bit-cast to i64; NULL if absent
    exif_unix     INTEGER,               -- best EXIF timestamp
    exif_src      TEXT,                  -- 'DateTimeOriginal'|'DateTimeDigitized'|'DateTime'
    flags         INTEGER NOT NULL DEFAULT 0
    -- bit0 fts-truncated (content beyond text cap not searchable; hash still full)
    -- bit1 image-over-cap (no decode → no phash/dims)
    -- bit2 read-error (name-only row)
    -- bit3 sparse (GNU/PAX sparse entry; hash covers the CONDENSED stream)
    -- bit4 shadowed (same path appears later in this tar; that later entry wins on extract)
    -- bit5 decode-failed (image decode error/limit → no phash/dims)
);
CREATE INDEX idx_files_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX idx_files_path ON files(path);   -- hardlink resolution, shadow detection, inspect
```

Deliberately **not** in per-source DBs: pHash band columns/indexes — dedup
only ever runs on a master (real or throwaway), so per-source files stay lean.

**meta keys, v3** (v2 keys kept): `schema_version=3`,
`source_type` = `tar`|`dir`, `source` (path; `archive` still written for old
readers), `index_uuid` (16 random bytes hex, minted per build — the master's
stable identity for this index across file moves),
`archive_size`, `archive_mtime_unix` (stat of the tar at index time),
`archive_blake3` (hex; tar sources only — see below),
`hash_algo='blake3'`, `phash_algo='sage-dct-v1'`,
`text_cap`, `media_cap`, plus existing `created_unix`, `word_stats`,
`completed`, `files_indexed`, `files_skipped`, `files_truncated`.

### Indexer changes (the one real delta, `src/indexer.rs:190-199`)

Replace `take(cap).read_to_end` with a chunked loop: read the **whole** entry
in 256 KiB chunks; every chunk feeds `blake3::Hasher`; chunks append to the
head buffer only while it is under the cap (text cap 16 MiB default; separate
media cap 64 MiB default for image-extension entries, since a truncated JPEG
cannot be decoded). Cost note: compressed formats already decompress every
byte to advance the stream, and the v0.2 reader chain is `Box<dyn Read>` with
no Seek — plain tar already reads through over-cap entries too. The delta is
BLAKE3 CPU only (multi-GB/s, SIMD).

- **Tar header capture:** `entry.header().mtime()/size()/mode()` — recorded
  now instead of discarded.
- **Archive fingerprint (tar sources):** a hashing reader wraps the `File`
  at the bottom of the chain. tar stops reading at the end-of-archive zero
  blocks, so after entry iteration the indexer **must drain the underlying
  compressed reader to EOF** (recover it via `into_inner()` /
  `zstd::Decoder::finish()` from concrete reader types, then `io::copy` to
  sink) so that `archive_blake3 == b3sum(file)`. Without the drain the
  stored hash is wrong for essentially every archive and `master verify
  --deep` would report false staleness — this is a correctness contract,
  covered by a test.
- **Hardlinks:** indexed as `entry_type='hardlink'` with `link_target`; after
  the streaming pass, resolve `content_hash` from the target via SQL UPDATE
  supported by `idx_files_path` (O(links·log n) — never the unindexed
  quadratic scan), iterated up to 5 rounds to resolve link-to-link chains.
- **Shadowed paths:** tar permits the same path twice (later entry wins on
  extract). Post-pass marks all but the highest `id` per path with flags
  bit4. Shadowed rows are excluded from KEEP candidacy in dedup and their
  bytes are reported separately as intra-archive waste.
- **Sparse entries:** GNU sparse (and PAX-sparse when detectable via pax
  extensions) get flags bit3. tar-rs reads the *condensed* stream (upstream
  issues #286/#295), so their hash does not equal the logical file's hash:
  excluded from exact dedup by default, counted and surfaced in reports.
- **Empty files:** `size == 0` → `kind='empty'`. BLAKE3 of empty input is a
  constant; without this, every 0-byte file across all archives forms one
  giant garbage group. Excluded from dedup unless `--include-empty`.
- **Directory sources:** walkdir (`follow_links=false`), same per-entry
  pipeline; stat provides size/mtime/mode; unreadable files warn-and-continue
  as name-only rows (flags bit2). Index lands at sibling `<dir>.db`
  (`/photos/2020` → `/photos/2020.db`) — never inside the source directory
  (would self-contaminate future backups of it) — with v0.2's fallback-to-cwd
  when the parent is read-only.
- **Image decode hardening:** `image::Limits` caps (dimensions 16384×16384,
  512 MiB decode memory) from day one; any decode error or panic
  (`catch_unwind`) degrades to `phash NULL` + flags bit5 — an attacker-shaped
  JPEG inside an archive must never abort an index run.
- Corrupt-tar-is-fatal, `completed=0` partial marker, 500/txn batching,
  WAL-then-fold, the `\u{1}` strip, word stats — all unchanged from v0.2.

### Kind classification

By extension for media (photo-organizer's lists + `.tif/.tiff` images and
`.cr3` RAW): images `.jpg .jpeg .png .heic .webp .tif .tiff`, RAW
`.nef .cr2 .cr3 .arw .dng`, video `.mp4 .mov .avi .mkv`; everything else
text/binary by the existing null-byte probe. pHash computed for decodable
image formats (JPEG/PNG/WebP/TIFF); HEIC/RAW get EXIF + hash only in v1.0.

## 5. pHash — `sage-dct-v1` (frozen)

~100 LOC in-house, mirroring Python `imagehash.phash` semantics: decode →
`to_luma8` → 32×32 Lanczos3 resize → 2D DCT-II (separable, precomputed 32×32
cosine table) → top-left 8×8 block → median over those 64 coefficients
(**including** DC, as imagehash does) → 64 bits (`coef > median`).

- **Why in-house:** `img_hash` is unmaintained (last release 2021, pinned to
  image 0.23); its fork `image_hasher` has an uncertain maintainer. Persisted
  hashes must be stable for years; the algorithm belongs under our test
  suite, frozen behind golden vectors.
- **Parity note:** bit-for-bit parity with Python imagehash is impossible
  (PIL uses ITU-R 601 grayscale + its own resample; the image crate differs).
  Distances remain comparable in practice; photo-organizer never persisted
  hashes, so nothing is imported. This is documented, not papered over.
- `phash_algo` is versioned in meta; dedup **refuses** to near-match across
  mixed algorithm versions.
- Golden tests: fixture images with expected 64-bit hashes asserted in CI;
  plus distance cross-checks against Python imagehash on the fixtures
  (assert small hamming distance, not equality).

## 6. Master index (`master.db`)

A replicated-**metadata** catalog — never text content, never FTS. Rationale
over query-time ATTACH federation: ATTACH caps at 10 by default (125 hard
max, raising it needs fragile build flags), dies when archives are offline,
and cross-DB FTS is manual anyway. Metadata is ~150 B/row (1M files ≈ 150 MB)
and buys: dedup as plain indexed SQL that works with every archive unplugged.

Default location `$XDG_DATA_HOME/backupsage/master.db`
(`~/.local/share/backupsage/master.db`), overridable with `--master PATH` or
`BACKUPSAGE_MASTER`. **Kept permanently in WAL with `busy_timeout=5000`** —
two Claude accounts share this machine's state and concurrent sync/dedup runs
are realistic; `SQLITE_BUSY` is a handled code path, not a crash.

```sql
CREATE TABLE archives (
    archive_id     INTEGER PRIMARY KEY,
    index_uuid     TEXT UNIQUE NOT NULL,    -- from per-source meta; survives moves
    db_path        TEXT NOT NULL,
    source_path    TEXT NOT NULL,
    source_type    TEXT NOT NULL,           -- 'tar' | 'dir'
    label          TEXT,                    -- defaults to source file stem
    schema_version INTEGER NOT NULL,
    files_count    INTEGER,
    completed      INTEGER,
    indexed_unix   INTEGER,
    archive_size   INTEGER,                 -- of the tar, at index time
    archive_mtime_unix INTEGER,
    archive_blake3 TEXT,
    db_size        INTEGER,                 -- replica-freshness fingerprint
    db_mtime_unix  INTEGER,
    status         TEXT NOT NULL DEFAULT 'ok',
    -- ok | stale-replica | stale-index | db-missing | archive-missing
    --    | incomplete | v2-limited
    added_unix     INTEGER NOT NULL,
    synced_unix    INTEGER
);
CREATE TABLE files (
    archive_id   INTEGER NOT NULL REFERENCES archives(archive_id) ON DELETE CASCADE,
    file_id      INTEGER NOT NULL,          -- files.id in the source DB (back-pointer)
    path         TEXT NOT NULL,
    entry_type   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    size         INTEGER,
    mtime_unix   INTEGER,
    exif_unix    INTEGER,
    exif_src     TEXT,
    content_hash BLOB,
    phash        INTEGER,
    img_w        INTEGER,
    img_h        INTEGER,
    flags        INTEGER NOT NULL DEFAULT 0,
    -- 16-bit bands for multi-index hamming lookup (SQLite >> is arithmetic;
    -- the & 0xFFFF mask makes sign irrelevant):
    pb0 INTEGER GENERATED ALWAYS AS ((phash >> 48) & 0xFFFF) VIRTUAL,
    pb1 INTEGER GENERATED ALWAYS AS ((phash >> 32) & 0xFFFF) VIRTUAL,
    pb2 INTEGER GENERATED ALWAYS AS ((phash >> 16) & 0xFFFF) VIRTUAL,
    pb3 INTEGER GENERATED ALWAYS AS ( phash        & 0xFFFF) VIRTUAL,
    PRIMARY KEY (archive_id, file_id)
);
CREATE INDEX m_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
CREATE INDEX m_pb0 ON files(pb0) WHERE phash IS NOT NULL;  -- likewise pb1..pb3
CREATE INDEX m_size ON files(size);
CREATE INDEX m_path ON files(path);
```

**Replication** (one source at a time — the ATTACH limit is never in play):
`ATTACH 'file:<db>?mode=ro' AS src; DELETE FROM files WHERE archive_id=:id;
INSERT INTO files SELECT … FROM src.files; DETACH src;`

**Two-layer staleness, surfaced separately:**

1. *Replica vs index* (`master sync`): re-read each registered db's meta; if
   `index_uuid` differs (index rebuilt) → re-replicate + refresh fingerprint.
   Unreachable db → `db-missing` (rows retained; dedup still works, reports
   say so). `sync --prune DAYS` is the only auto-removal.
2. *Index vs archive* (`master verify`): stat the source; size/mtime changed
   → `stale-index` (remedy: re-index; the master only reports).
   `verify --deep` re-hashes the tar and compares `archive_blake3`.

Re-registration after a move: `master add` on a known `index_uuid` updates
`db_path`/`source_path` in place — no duplicate rows possible (UNIQUE).
`completed=0` indexes register as `incomplete`; every dedup/search touching
them carries a partial-data warning **in the JSON summary**, not just stderr.

**v2 compatibility:** `master add` accepts a v2 db → `status='v2-limited'`,
uid synthesized as `v2:` + blake3(meta.archive + created_unix) (stable across
db moves since it keys on the *archive* path stored in meta). It participates
in federated `search --all`, contributes zero dedup rows, and every dedup
report names it under `skipped_archives` with the exact fix
(`backupsage index <archive>` — re-indexing IS the migration; hashes cannot
be synthesized without re-reading the archive). v0.1 dbs (no meta): refused
with a clear message. `search`/`top` on a single v2 db work unchanged.

## 7. Dedup engine

Runs entirely on a master — the registered one, or a **throwaway in-memory
master** built by the same replication code when invoked ad-hoc:
`backupsage dedup --db a.tar.zst.db --db b.tar.gz.db` (2+ DBs, or 1 for
intra-archive dupes). One code path, zero registration ceremony.

**Exact groups:** `GROUP BY content_hash HAVING COUNT(*) > 1` over the
filtered scope. Excluded by default: `kind='empty'`, sparse-flagged rows,
symlinks. Hardlinks resolve to their target's hash but are **excluded from
copy counts and reclaimable bytes** (they share storage; a file plus its own
hardlink is not a duplicate) — displayed as aliases on their target's line.

**Near groups (images):** multi-index hashing over the four 16-bit bands.
Pigeonhole guarantee: at hamming distance ≤ 3, at least one band matches
exactly → indexed equality probes with zero recall loss. Implementation is
**bucket iteration, never a naive band self-join**: iterate distinct band
values having COUNT(*) > 1, cap pathological buckets (default 10 000, warn
when skipped), popcount-verify `(a ^ b).count_ones() <= d` in Rust, group via
union-find (~30 lines). Trivial hashes (all-0 / all-1 pHash from flat or
near-monochrome images) are excluded with a warning — they would weld
thousands of unrelated images into one group. `--threshold` capped at 3 in
v1.0 (that is what 4 bands guarantee; >3 would silently lose recall — an
8-band scheme is v1.1). Exact duplicates inside a near group are sub-labeled.
Near-dup refuses to run across mixed `phash_algo` versions.

**KEEP recommendation** (advisory label only; machine-readable
`keep_reason`; v1.0 never acts on it):

- *Exact groups* (identical bytes ⇒ identical EXIF): newest best-timestamp;
  tie → path without conflict-suffix markers (`(1)`, `_1`, `copy`); tie →
  shortest path; tie → lowest archive_id. Reasons: `newest`, `clean-path`.
- *Near groups* (different bytes): highest pixel count (`img_w*img_h`); tie →
  largest size; tie → newest EXIF. Reasons: `highest-resolution`, `largest`.
  Original-vs-edited is explicitly the user's call; the report orders the
  evidence. Shadowed rows are never KEEP.

**Timestamp precedence, always shown with its source:**
EXIF (`DateTimeOriginal` > `DateTimeDigitized` > `DateTime`) > tar-header
mtime / fs mtime > archive index date (marked). A 1970 tar mtime next to a
real EXIF date is exactly the evidence the user needs.

## 8. Command surface

Global: `--master PATH` (env `BACKUPSAGE_MASTER`), `--json` on all reporting
commands. Exit codes: `0` ok, `1` error, `2` completed but with skipped
archives (offline / v2-limited / incomplete) — scriptable honesty.

```
backupsage index <SOURCE>... [--db PATH] [--max-file-size 16M]
                 [--media-cap 64M] [--no-word-stats]
    SOURCE = tar/tar.gz/tar.zst (magic-byte detect) OR a directory.
    Writes <source>.db next to the source; read-only fallback to ./ as v0.2.
    Replaces any existing .db. Prints a hint to `master add` if unregistered.

backupsage search <QUERY> [--db PATH | -a ARCHIVE | --all] [--limit N]
                 [--snippets] [--json]
    --all = every registered non-missing db, sequential fan-out, results
    grouped per archive, skipped archives listed. Single-db behavior = v0.2.

backupsage top   [--db PATH | -a ARCHIVE] [--limit N]        # v0.2 parity

backupsage master add <DB_OR_SOURCE>...   # accepts x.tar.zst.db or the tar/dir
backupsage master list [--json]
backupsage master sync [--prune DAYS]
backupsage master verify [--deep] [--json]
backupsage master rm <ID|LABEL|PATH>      # registration + replica rows only

backupsage dedup [--exact-only | --near-only] [--threshold 0..3]
                 [--kind image|raw|video|text|any] [--ext jpg,heic,...]
                 [--min-size BYTES] [--path-glob GLOB] [--archive ID|LABEL]...
                 [--across-only] [--include-empty] [--db PATH]...
                 [--sort wasted|count|newest] [--limit-groups N]
                 [--json] [-o FILE]
    Default: exact + near, threshold 3, min-size 1, whole master.
    --across-only hides groups confined to one archive.

backupsage inspect <PATH-IN-ARCHIVE> (--db PATH | -a ARCHIVE)
    One file, every column: all timestamps with sources, hash, phash, dims, flags.
```

## 9. Report design

Terminal (comfy-table style, groups sorted by reclaimable bytes desc):

```
Group 12  exact · 3 copies · 2 archives · 4.2 MiB each · 8.4 MiB reclaimable
  KEEP  photos-2024   DCIM/IMG_0142.JPG        2024-03-14 18:02  exif      4032x3024
  dup   photos-2024   export/img_0142 (1).jpg  2024-03-20 09:11  tar-mtime
  dup   old-laptop    backup/IMG_0142.JPG      2019-01-01 00:00  tar-mtime

Group 17  near (max dist 2) · 2 copies · 2 archives · ~3.8 MiB reclaimable
  KEEP  photos-2023   2023/07/IMG_2041.jpg     2023-07-04 19:22  exif      4032x3024
  dup   misc-dl       dl/IMG_2041 (1).jpg      2024-01-11 08:01  tar-mtime 1600x1200  [dist 2]

Footer: N groups · M duplicate files · X reclaimable · skipped: old-v2 (v2-limited)
        · 312 images without phash (HEIC/RAW) · near-group reclaimable is an estimate
```

JSON (stable contract, the future web API; `version: 1`): `params`
(threshold, filters, keep policy), `archives` (id, label, source, status),
`groups` — each with `match` exact|near, `reclaimable_bytes`, `members`
carrying archive_id, file_id, path, size, kind, `content_hash` ("b3:…"),
`phash` (hex), `mtime_unix`, `exif_unix`, `best_ts_unix`,
`best_ts_source` ("exif:DateTimeOriginal" | "tar-mtime" | "archive-date"),
`width`/`height`, `hamming_to_keep`, `keep` bool, `keep_reason`, `flags`
decoded (`shadowed`, `sparse`, `hardlink_of`) — plus `summary` (totals,
`archives_offline`, `archives_incomplete`, `skipped_archives` with reasons,
`images_without_phash`). Staleness is part of the result, not a log line.

## 10. Tech stack

Kept (proven in this pipeline): clap 4 derive, rusqlite 0.39 `bundled`
(guarantees FTS5 + generated columns everywhere), zstd, flate2
(MultiGzDecoder), tar, indicatif, anyhow, comfy-table.

Added: **blake3** (content + archive hashing; ~10× sha2 with SIMD, no FIPS
need, `hash_algo` versioned in meta), **image** ~0.25 (JPEG/PNG/WebP/TIFF
decode with `Limits`), **kamadak-exif** 0.6 (pure-Rust EXIF from JPEG,
TIFF-based RAW, PNG, WebP, and HEIF *containers* — HEIC capture dates without
pixel decode), **walkdir**, **serde + serde_json**, **getrandom** (16-byte
index_uuid).

Rejected: `img_hash`/`image_hasher` (unmaintained / uncertain — pHash is
in-house and frozen, §5), `libheif-rs` (C dependency; v1.1 opt-in feature),
`rawloader` (dormant — **rawler** is the maintained v1.1 candidate),
rayon (decompress-bound; measure first), tantivy, ORMs.

## 11. Project structure & repo plan

Evolve `/home/tom/projects/backupsage` in place (history, CI, tests kept) on
branch `v1.0-dedup`; backup of pre-v1 state at `/home/tom/backup-backupsage`.
Version → 1.0.0; index schema → 3. Single crate, lib/bin split; workspace
split (core/cli/web) deferred to v2.0 when axum lands.

photo-organizer: unchanged for now. Its durable assets are absorbed as (a)
pHash threshold semantics + extension lists, (b) EXIF date precedence, (c)
the organize layout spec (v1.1) and Flask+Vite API patterns (v2.0 seed).
When v1.1 ships `organize`, archive that repo with a pointer README.

## 12. v1.0 scope (definition of done)

Ships: schema v3 indexing of tar/tar.gz/tar.zst **and** directories in one
streaming pass (full-content BLAKE3, tar mtime/size/mode, EXIF dates, pHash +
dimensions for decodable images, drained archive fingerprint, hardlink
resolution, shadow/sparse/empty flagging); master catalog
(add/list/sync/verify/rm, two-layer staleness, move re-registration,
v2-limited); federated `search --all`; `dedup` exact + near with filters,
keep recommendations, terminal + stable JSON reports; `inspect`; v0.2 command
parity; exit-code contract.

Quality gates: all existing 16 tests keep passing; new tests cover the
indexer delta (hash-past-cap, header capture, drain, hardlinks, shadowing,
sparse, empty), pHash golden vectors, MIH candidate generation =
brute-force on random corpora (recall proof at d≤3), bucket-cap and
trivial-hash paths, master staleness transitions, v2-limited registration,
dedup e2e over generated archives, JSON schema stability. Performance
targets (measured, not promised): master dedup at 1M rows < 10 s with
sources offline; indexing throughput within ~15 % of v0.2 on text archives.

## 13. Deferred

**v1.1** (first mutating release — directory targets only; archives immutable
forever): `organize` (YYYY/YYYY-MM layout, unknown-date/, dry-run + manifest
default, `--execute`, conflict suffixes — photo-organizer parity), `extract`
(copy chosen winners out of archives), `dedup --apply-plan` (reviewed
delete/hardlink script for dirs), HEIC pHash (`heif` feature, libheif-rs),
RAW decode via rawler, mp4 creation dates, threshold >3 via 8×8-bit bands,
`--keep-policy newest|oldest`, incremental dir re-index, parallel fan-out
search. **v2.0**: axum web server + frontend on the v1 JSON contract;
workspace split; remote sources (S3/ssh — the chain is already `Read`-based);
video pHash. **Never**: rewriting or deleting from archives.

## 14. Risks (accepted, with mitigations)

- **HEIC gets no pHash in v1.0** — near-dup is blind to iPhone photos (exact
  dedup + EXIF dates still work). Surfaced as a count in every report; docs
  set expectations.
- **In-house pHash is forever** — frozen behind golden vectors before
  release; versioned in meta; near-dup refuses mixed versions. Threshold 3
  inherited from photo-organizer needs corpus validation on real photos
  before release (planned during verification).
- **Sparse handling is mitigation, not resolution** — tar-rs upstream is
  incomplete; sparse entries are flagged + excluded, and a PAX-sparse archive
  can still hit the (inherited) corrupt-entry-is-fatal abort.
- **Incompleteness must propagate** — `completed=0`/stale/offline statuses
  flow into every JSON summary; a dropped warning turns missing data into
  "no duplicates found". Covered by tests.
- **Scale numbers are estimates** until the perf gates run; master
  replication is per-archive granular so sync cost is bounded by changed
  archives.
- **Concurrent master access** (two accounts, one machine) — WAL +
  busy_timeout permanent; SQLITE_BUSY handled; sync/dedup overlap tested.
- **BM25 is incomparable across archives** — grouped-per-archive display,
  documented; global ranking is an honest v2 non-goal.
