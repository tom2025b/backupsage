# ADR 0002 — Master signature via application_id; lossless paths as hex sibling fields

Date: 2026-07-23 · Status: accepted · Milestone: v1.0.1 (issues #37, #5)

## Context

Two contract decisions the spec left to the implementer:

1. Issue #37 requires "the BackupSage master signature" checked before any
   read-write open, but no mechanism was prescribed. Neither schema set
   `application_id` or `user_version`; the only distinguishing signatures
   were structural (per-source = `files_fts` + `meta`; master = `archives`
   + generated `pb0..pb3` columns).
2. Issue #5 requires JSON that round-trips non-UTF-8 path bytes alongside
   a safe display value, extending the add-fields-only v1 report contract.
   Paths were converted with `to_string_lossy` at ingestion and stored as
   TEXT, so the original bytes were unrecoverable.

## Decision

**Master signature** = SQLite `PRAGMA application_id = 0x42534147`
("BSAG"), stamped at creation. `master::probe()` classifies read-only
before any write: sidecar-name spellings and symlinks fail; empty files
and non-SQLite data are foreign; `files_fts` marks a per-source index;
`application_id == 0` with an `archives` table and pb0-shaped `files`
(checked via `pragma_table_xinfo` — `table_info` hides generated columns)
is a legacy v1.0.0 master, adopted and stamped once. Signed masters are
never rewritten just to open them.

**Lossless paths**: raw bytes are captured at ingestion (tar
`path_bytes()` / `link_name_bytes()`, directory-walk `OsStr` bytes) and
stored in new nullable `path_raw` / `link_target_raw` BLOB columns — only
when the display string was lossy. JSON gains optional sibling fields
(`path_bytes`, lowercase hex) emitted only when raw bytes exist; the
existing `path` display fields are unchanged. `schema_version` stays 3
with a `meta.path_raw = "1"` capability marker; readers probe column
presence per connection, so old indexes keep working.

## Alternatives considered

- **Signature in a meta table** — the master schema has no meta table;
  adding one is more DDL surface than a 4-byte header field, and
  `application_id` is readable without trusting any table content.
- **Magic filename or sidecar marker** — breaks silently on rename/copy;
  rejected.
- **Refusing legacy unsigned masters** — would strand every v1.0.0
  catalog; structural adoption is a one-time, precisely-shaped check.
- **base64 or JSON byte arrays for raw paths** — base64 needs a new
  dependency; byte arrays bloat reports. Hex is dependency-free,
  grep-able, and unambiguous.
- **Schema version bump to 4** — master v2/v3 handling already exists and
  a bump would cascade through it for what is an additive, probeable
  column; deferred to a real schema break.

## Consequences

- Any tool can identify a BackupSage master with one pragma; foreign
  SQLite files can never be contaminated by master DDL again.
- The first open of a legacy master writes exactly one header field
  (stamp); after that, opens are read-clean.
- JSON consumers get exact bytes for non-UTF-8 paths without any change
  to existing fields; fixtures pinning v1 field names keep passing.
- The `path_raw` capability marker means indexes built before v1.0.1
  simply omit `path_bytes` in JSON — honest absence, not fabricated data.
- Future schema metadata cleanup (v1.0.2 "explicit index modes") should
  fold the capability marker into a proper schema registry.
