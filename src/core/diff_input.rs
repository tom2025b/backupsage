//! Read-only index loading and input health for `backupsage diff` (#93).
//!
//! This module turns two index files into [`crate::diff::Snapshot`]s for the
//! pure engine, and says plainly what it could and could not establish about
//! each input. See ADR 0010 for the evidence rules.
//!
//! **Nothing here writes, and nothing mixed is called complete.** Every
//! index is read through [`crate::index_read::LockedIndex`], the loader all
//! commands share: one read transaction, a fail-closed refusal of every
//! layout SQLite could only read by writing beside it (pending journal, WAL
//! mode, several hard links), and a before/after file stamp. Its public
//! names are re-exported here unchanged. The only other file-system access
//! is a `stat` of the recorded source, for [`SourceCurrency`].

use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use serde::Serialize;

use crate::index_read::{note, read_note, run_mid_read_hook, LockedIndex};
pub use crate::index_read::{
    open_index_readonly, set_mid_read_hook, InputNote, NoteCode, ReadPoint,
};

use crate::diff::{
    DiffReport, Entry, EntryType, Side, Snapshot, SnapshotInfo, SnapshotState, SourceCurrency,
};
use crate::report::to_hex;
use crate::searcher::get_meta;
use crate::store::{flags, SCHEMA_VERSION};

/// What the loader established about one input, besides its rows.
#[derive(Clone, Debug, Serialize)]
pub struct InputHealth {
    /// Display text of the index path; `db_path_bytes` is authoritative.
    pub db_path: String,
    pub db_path_bytes: String,
    pub source: Option<String>,
    pub source_type: Option<String>,
    pub content_mode: Option<String>,
    /// Rows handed to the engine, shadowed rows included.
    pub rows: usize,
    pub notes: Vec<InputNote>,
}

/// The source stat the index recorded at indexing time.
#[derive(Default)]
struct RecordedStat {
    size: Option<u64>,
    mtime_unix: Option<i64>,
}

pub struct LoadedIndex {
    pub snapshot: Snapshot,
    pub health: InputHealth,
}

/// Load one index. Never fails: anything that stops the rows from being
/// used becomes an unavailable snapshot with a note saying why, so the
/// engine reports it as inconclusive, never as an absence.
pub fn load_index(db_path: &Path) -> LoadedIndex {
    let mut health = InputHealth {
        db_path: db_path.display().to_string(),
        db_path_bytes: to_hex(db_path.as_os_str().as_bytes()),
        source: None,
        source_type: None,
        content_mode: None,
        rows: 0,
        notes: Vec::new(),
    };
    let mut info = SnapshotInfo {
        index_uuid: None,
        schema_version: None,
        hash_algo: None,
        state: SnapshotState::Unavailable,
        source_currency: SourceCurrency::NotChecked,
    };
    let mut recorded = RecordedStat::default();
    let entries = match read_index(db_path, &mut info, &mut health, &mut recorded) {
        Ok(entries) => entries,
        Err(unavailable) => {
            health.notes.push(unavailable);
            info.state = SnapshotState::Unavailable;
            Vec::new()
        }
    };
    if info.state != SnapshotState::Unavailable {
        let (currency, currency_note) = probe_source(&health, &recorded);
        info.source_currency = currency;
        health.notes.push(currency_note);
    }
    health.rows = entries.len();
    LoadedIndex {
        snapshot: Snapshot { info, entries },
        health,
    }
}

/// Fills `info`/`health` and returns the rows, or the note that makes the
/// index unavailable.
fn read_index(
    db_path: &Path,
    info: &mut SnapshotInfo,
    health: &mut InputHealth,
    recorded: &mut RecordedStat,
) -> std::result::Result<Vec<Entry>, InputNote> {
    let conn = LockedIndex::open(db_path)?;

    info.schema_version = get_meta(&conn, "schema_version").and_then(|v| v.parse().ok());
    info.hash_algo = get_meta(&conn, "hash_algo");
    info.index_uuid = get_meta(&conn, "index_uuid").filter(|id| !id.is_empty());
    health.source = get_meta(&conn, "source");
    health.source_type = get_meta(&conn, "source_type");
    health.content_mode = get_meta(&conn, "content_mode");
    recorded.size = get_meta(&conn, "archive_size").and_then(|v| v.parse().ok());
    recorded.mtime_unix = get_meta(&conn, "archive_mtime_unix").and_then(|v| v.parse().ok());

    let mut notes = Vec::new();
    let mut incompatible = false;
    if info.schema_version != Some(SCHEMA_VERSION) {
        incompatible = true;
        notes.push(note(
            NoteCode::UnsupportedSchema,
            match info.schema_version {
                Some(v) => format!("schema_version {v}; diff reads only v{SCHEMA_VERSION}"),
                None => format!("no schema_version; diff reads only v{SCHEMA_VERSION}"),
            },
        ));
    }
    if info.hash_algo.as_deref() != Some("blake3") {
        incompatible = true;
        notes.push(note(
            NoteCode::UnsupportedHashAlgo,
            format!(
                "hash_algo {}; diff compares only blake3",
                info.hash_algo.as_deref().unwrap_or("not recorded")
            ),
        ));
    }
    if info.index_uuid.is_none() {
        incompatible = true;
        notes.push(note(NoteCode::MissingIndexUuid, "no index_uuid recorded"));
    }
    let completed = get_meta(&conn, "completed").as_deref() == Some("1");
    if !completed {
        notes.push(note(
            NoteCode::IndexIncomplete,
            "indexing stopped before it finished; rows seen are real, more may exist",
        ));
    }
    if health.content_mode.as_deref() == Some("metadata-only") {
        notes.push(note(
            NoteCode::MetadataOnlyIndex,
            "content was never read: no row has a content hash",
        ));
    }
    info.state = if incompatible {
        SnapshotState::Incompatible
    } else if !completed {
        SnapshotState::Incomplete
    } else {
        SnapshotState::Complete
    };

    // Rows are read only from a v3 layout; another schema is reported as
    // incompatible with no rows rather than guessed at.
    run_mid_read_hook(ReadPoint::BetweenStatements);
    let entries = if info.schema_version == Some(SCHEMA_VERSION) {
        read_rows(&conn).map_err(|e| read_note(&e))?
    } else {
        Vec::new()
    };
    // Coherent only if the whole read happened under the one lock and the
    // file is exactly as it was.
    conn.finish()?;
    health.notes.extend(notes);
    Ok(entries)
}

/// Every row, verbatim. Each field comes from the row itself: in particular
/// a NULL `content_hash` stays `None`. The indexer stores no hash for an
/// unsupported PAX-sparse row, and nothing here may supply one (ADR 0010).
fn read_rows(conn: &Connection) -> Result<Vec<Entry>> {
    let mut stmt = conn.prepare(
        "SELECT id, path, path_raw, entry_type, link_target, link_target_raw,
                size, mtime_unix, mode, content_hash, flags, kind
         FROM files ORDER BY id",
    )?;
    let mut rows = stmt.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let file_id: i64 = row.get(0)?;
        let path: String = row.get(1)?;
        let path_raw: Option<Vec<u8>> = row.get(2)?;
        let entry_type: String = row.get(3)?;
        let link_target: Option<String> = row.get(4)?;
        let link_target_raw: Option<Vec<u8>> = row.get(5)?;
        let size: i64 = row.get(6)?;
        let mode: Option<i64> = row.get(8)?;
        let content_hash: Option<Vec<u8>> = row.get(9)?;
        let malformed = |what: &str| anyhow!("malformed {what} on files row {file_id}");
        entries.push(Entry {
            file_id,
            path: path_raw.unwrap_or_else(|| path.into_bytes()),
            entry_type: match entry_type.as_str() {
                "file" => EntryType::File,
                "hardlink" => EntryType::Hardlink,
                "symlink" => EntryType::Symlink,
                _ => EntryType::Unsupported,
            },
            link_target: link_target_raw.or_else(|| link_target.map(String::into_bytes)),
            size: u64::try_from(size).map_err(|_| malformed("size"))?,
            mtime_unix: row.get(7)?,
            mode: mode
                .map(|m| u32::try_from(m).map_err(|_| malformed("mode")))
                .transpose()?,
            content_hash: content_hash
                .map(|h| <[u8; 32]>::try_from(h.as_slice()).map_err(|_| malformed("content_hash")))
                .transpose()?,
            flags: row.get(10)?,
            kind: row.get(11)?,
        });
        if entries.len() == 1 {
            run_mid_read_hook(ReadPoint::BetweenRows);
        }
    }
    Ok(entries)
}

/// Where the live source stands against what the index recorded. A `stat`
/// only: the source is never opened or read.
fn probe_source(health: &InputHealth, recorded: &RecordedStat) -> (SourceCurrency, InputNote) {
    let Some(source) = health.source.as_deref() else {
        return (
            SourceCurrency::NotChecked,
            note(NoteCode::SourceNotRecorded, "the index records no source"),
        );
    };
    if source.contains('\u{fffd}') {
        return (
            SourceCurrency::NotChecked,
            note(
                NoteCode::SourcePathLossy,
                "the recorded source path is a lossy rendering; its exact bytes are unknown",
            ),
        );
    }
    let path = Path::new(source);
    if !path.is_absolute() {
        return (
            SourceCurrency::NotChecked,
            note(
                NoteCode::SourcePathRelative,
                "the recorded source path is relative to the directory indexing ran in",
            ),
        );
    }
    let md = match fs::metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            return (
                SourceCurrency::Denied,
                note(NoteCode::SourceDenied, "permission denied"),
            );
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return (
                SourceCurrency::Offline,
                note(NoteCode::SourceOffline, "the source is not reachable"),
            );
        }
        Err(e) => {
            return (
                SourceCurrency::Offline,
                note(NoteCode::SourceOffline, e.to_string()),
            );
        }
    };
    if health.source_type.as_deref() == Some("dir") {
        return (
            SourceCurrency::DirectoryUnverified,
            note(
                NoteCode::SourceDirectoryUnverified,
                "a directory's own stat does not reflect changes to the files inside it",
            ),
        );
    }
    let (Some(size), Some(mtime)) = (recorded.size, recorded.mtime_unix) else {
        return (
            SourceCurrency::NotChecked,
            note(
                NoteCode::SourceStatNotRecorded,
                "the index records no source size and mtime",
            ),
        );
    };
    let current_mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    if md.len() == size && current_mtime == Some(mtime) {
        (
            SourceCurrency::StatMatches,
            note(
                NoteCode::SourceStatMatches,
                "size and mtime match the index; the source was not re-hashed",
            ),
        )
    } else {
        (
            SourceCurrency::Stale,
            note(
                NoteCode::SourceStale,
                "size or mtime differ from the index; the source changed since indexing",
            ),
        )
    }
}

/// Why move inference could not run on one side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveBlockerCause {
    SnapshotUnavailable,
    SnapshotIncompatible,
    SnapshotIncomplete,
    Hardlink,
    Symlink,
    UnsupportedEntryType,
    ReadError,
    /// Unparsed pax records: tar-rs could not read some pax metadata (legal
    /// values containing newlines, such as xattrs or names, do this too), so
    /// the stored hash may not cover the logical file.
    PaxUnparsed,
    UnsupportedSparse,
    NotHashed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MoveBlocker {
    pub side: Side,
    pub cause: MoveBlockerCause,
    /// Rows with this cause; `None` for a whole-snapshot cause.
    pub rows: Option<usize>,
}

/// Whether the engine could look for moves at all, and if not, why.
/// Mirrors ADR 0010 rule 4: moves need both snapshots compatible and
/// complete, and every row on both sides to carry a trusted content hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MoveInference {
    pub enabled: bool,
    pub blockers: Vec<MoveBlocker>,
}

/// The first reason a row lacks a trusted hash, in the engine's own order.
fn row_blocker(entry: &Entry) -> Option<MoveBlockerCause> {
    Some(match entry.entry_type {
        EntryType::Hardlink => MoveBlockerCause::Hardlink,
        EntryType::Symlink => MoveBlockerCause::Symlink,
        EntryType::Unsupported => MoveBlockerCause::UnsupportedEntryType,
        EntryType::File if entry.flags & flags::READ_ERROR != 0 => MoveBlockerCause::ReadError,
        EntryType::File if entry.flags & flags::PAX_UNPARSED != 0 => MoveBlockerCause::PaxUnparsed,
        EntryType::File if entry.content_hash.is_some() => return None,
        EntryType::File if entry.flags & flags::SPARSE != 0 => MoveBlockerCause::UnsupportedSparse,
        EntryType::File => MoveBlockerCause::NotHashed,
    })
}

impl MoveInference {
    pub fn of(before: &Snapshot, after: &Snapshot) -> Self {
        let mut blockers = Vec::new();
        for (side, snapshot) in [(Side::Before, before), (Side::After, after)] {
            let whole = match snapshot.info.state {
                SnapshotState::Complete => None,
                SnapshotState::Incomplete => Some(MoveBlockerCause::SnapshotIncomplete),
                SnapshotState::Unavailable => Some(MoveBlockerCause::SnapshotUnavailable),
                SnapshotState::Incompatible => Some(MoveBlockerCause::SnapshotIncompatible),
            };
            if let Some(cause) = whole {
                blockers.push(MoveBlocker {
                    side,
                    cause,
                    rows: None,
                });
            }
            let mut counts = std::collections::BTreeMap::new();
            for cause in snapshot.entries.iter().filter_map(row_blocker) {
                *counts.entry(cause).or_insert(0) += 1;
            }
            blockers.extend(counts.into_iter().map(|(cause, rows)| MoveBlocker {
                side,
                cause,
                rows: Some(rows),
            }));
        }
        Self {
            enabled: blockers.is_empty(),
            blockers,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Inputs<'a> {
    pub before: &'a InputHealth,
    pub after: &'a InputHealth,
}

/// The `diff --json` document: the engine's version-1 report, plus what the
/// loader established about each input and about move inference.
#[derive(Debug, Serialize)]
pub struct DiffDocument<'a> {
    #[serde(flatten)]
    pub report: &'a DiffReport,
    pub inputs: Inputs<'a>,
    pub move_inference: MoveInference,
}

impl<'a> DiffDocument<'a> {
    pub fn new(report: &'a DiffReport, before: &'a LoadedIndex, after: &'a LoadedIndex) -> Self {
        Self {
            report,
            inputs: Inputs {
                before: &before.health,
                after: &after.health,
            },
            move_inference: MoveInference::of(&before.snapshot, &after.snapshot),
        }
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }
}

/// Load both indexes and compare them. Errors only for input the engine
/// refuses as contradictory (for example, repeated row ids).
pub fn diff_indexes(before: &Path, after: &Path) -> Result<(LoadedIndex, LoadedIndex, DiffReport)> {
    let before = load_index(before);
    let after = load_index(after);
    let report = match crate::diff::compare(&before.snapshot, &after.snapshot) {
        Ok(report) => report,
        Err(e) => bail!("cannot compare these indexes: {e:#}"),
    };
    Ok((before, after, report))
}
