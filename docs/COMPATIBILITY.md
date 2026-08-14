# Compatibility policy

This policy defines how long BackupSage's public contracts remain compatible
and how a future breaking change is announced. It applies to released v1.x
versions; unreleased main-branch behavior is not a compatibility promise.

## Window and change rules

Within v1.x, every surface listed below is stable. Additions are permitted
where stated; removals, renames, semantic changes, and incompatible format
changes are not. A v1.x release must continue to read the on-disk data formats
created by earlier v1.x releases.

A breaking change requires a new major version. Before that major release,
BackupSage will document the affected surface, migration or re-indexing steps,
and the first release containing the change in both this policy and its release
notes. A deprecation notice may be published during v1.x, but it does not make
the v1.x contract breakable.

## Public surfaces

| Surface | Current form | v1.x promise |
|---|---|---|
| JSON reports | `dedup --json` report `"version": 1`; `search --json`, `search --all --json`, `master list --json`, and `master verify --json` | Stable and additive-only. Existing keys retain their names, types, and meanings; `dedup` remains report version 1. v1.0.2 added the keeper-star fields (#9): `actionable`, `review_only_bytes`, `transitive_only_files`, `actionable_rule` — additive. In the same release `reclaimable_bytes`/`duplicate_files` stopped counting transitive-only members (a correctness fix to match their documented meaning: only members measured safe against the keeper are reclaimable). v1.0.2 also added `search --json`'s top-level `mode` (#70) — additive; `matches` is `null` only on indexes built with the new `--mode search-only`, and output for `--mode full` (the default, and every index built by an earlier v1.x) is unchanged. v1.0.2 also added `content_mode` to `master list --json` and `dedup --json`'s `archives[]`, and `mode` to `search --all --json`'s `archives[]` (#71) — additive; a metadata-only archive now also appears in `dedup`'s `summary.skipped_archives` (previously silently produced zero groups with no explanation). |
| Exit codes | `0` clean, `1` error, `2` completed with skips; clap usage errors currently also use `2` | Stable meanings, including the documented usage-error caveat. |
| Per-source index | SQLite schema version `3` | Stable within v1.x. v1.x continues to read schema-v3 indexes and the supported schema-v2 indexes, which remain `v2-limited` where metadata required by a command is absent. |
| Master catalog | SQLite master catalog containing registered index metadata and replicas | Stable within v1.x. A catalog created by an earlier v1.x release remains readable by later v1.x releases; registration and query behavior preserve existing entries. |
| Perceptual hash | `sage-dct-v1` 64-bit DCT pHash | Frozen. The algorithm identifier and its hash semantics do not change; any future algorithm receives a new identifier, and mixed algorithms are not compared. |
| CLI | Documented commands, subcommands, flags, defaults, and accepted values | Stable within v1.x. New commands or flags may be added, but existing invocation syntax and behavior are not removed or repurposed. `dedup --threshold` remains capped at `0..3`, because its four 16-bit bands guarantee recall only through Hamming distance 3. |

`docs/CONTRACT.md` records the exact current JSON and exit-code shapes and the
golden tests that enforce them. This policy adds the version window and
deprecation procedure; it does not widen a surface that is documented there as
human-oriented and unstable.

## On-disk data

BackupSage never rewrites an existing index or master catalog merely to read
it. A later v1.x release may create a newer index only through the normal
explicit indexing flow. If a future major version cannot read an old format, it
will provide the migration or re-indexing instructions required by the
deprecation procedure above before that breaking release ships.
