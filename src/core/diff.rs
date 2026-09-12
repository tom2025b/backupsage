//! Pure, historical snapshot comparison (#92, child of #13).
//!
//! No source I/O, index discovery, or actions occur here. Callers supply index
//! evidence; availability/currency is reported separately from snapshot facts.
//! See ADR 0010 for comparison, shadow and conservative move rules.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Result};
use serde::Serialize;

use crate::report::to_hex;
use crate::store::{flags, SCHEMA_VERSION};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    Complete,
    Incomplete,
    Unavailable,
    Incompatible,
}

/// Evidence about the live source, never inferred from index completion.
/// A matching stat is deliberately not called "verified".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCurrency {
    NotChecked,
    StatMatches,
    Stale,
    Offline,
    Denied,
    DirectoryUnverified,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotInfo {
    pub index_uuid: Option<String>,
    pub schema_version: Option<i64>,
    pub hash_algo: Option<String>,
    pub state: SnapshotState,
    pub source_currency: SourceCurrency,
}

/// Raw entry evidence from one index. `file_id` is only an identity WITHIN
/// that index; it never establishes correspondence across snapshots.
#[derive(Clone, Debug)]
pub struct Entry {
    pub file_id: i64,
    pub path: Vec<u8>,
    pub entry_type: EntryType,
    pub link_target: Option<Vec<u8>>,
    pub size: u64,
    pub mtime_unix: Option<i64>,
    pub mode: Option<u32>,
    pub content_hash: Option<[u8; 32]>,
    pub flags: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    File,
    Hardlink,
    Symlink,
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub info: SnapshotInfo,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Removed,
    Moved,
    ByteIdentical,
    MetadataOnlyChanged,
    ContentChanged,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    PathAbsent,
    UniqueContentCorrespondence,
    EqualContentAndMetadata,
    EqualContentDifferentMetadata,
    DifferentContent,
    MissingContentEvidence,
    MissingMetadataEvidence,
    UnsupportedEntryType,
    OtherSnapshotIncomplete,
    IncompatibleSnapshots,
    ShadowedPath,
}

/// A report always includes raw bytes, even for valid UTF-8. The display path
/// is derived here and must be terminal-sanitized by a frontend.
#[derive(Debug, Serialize)]
pub struct ReportEntry {
    pub file_id: i64,
    pub path: String,
    pub path_bytes: String,
    pub entry_type: EntryType,
    pub link_target_bytes: Option<String>,
    pub size: u64,
    pub mtime_unix: Option<i64>,
    pub mode: Option<u32>,
    pub content_hash: Option<String>,
    pub flags: i64,
}

impl From<&Entry> for ReportEntry {
    fn from(entry: &Entry) -> Self {
        Self {
            file_id: entry.file_id,
            path: String::from_utf8_lossy(&entry.path).into_owned(),
            path_bytes: to_hex(&entry.path),
            entry_type: entry.entry_type,
            link_target_bytes: entry.link_target.as_deref().map(to_hex),
            size: entry.size,
            mtime_unix: entry.mtime_unix,
            mode: entry.mode,
            content_hash: entry
                .content_hash
                .as_ref()
                .map(|h| format!("b3:{}", to_hex(h))),
            flags: entry.flags,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub reason: Reason,
    pub before: Option<ReportEntry>,
    pub after: Option<ReportEntry>,
}

#[derive(Debug, Serialize)]
pub struct Excluded {
    pub side: Side,
    pub reason: Reason,
    pub entry: ReportEntry,
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub added: usize,
    pub removed: usize,
    pub moved: usize,
    pub byte_identical: usize,
    pub metadata_only_changed: usize,
    pub content_changed: usize,
    pub inconclusive: usize,
    pub excluded: usize,
}

#[derive(Debug, Serialize)]
pub struct DiffReport {
    pub version: u32,
    pub before: SnapshotInfo,
    pub after: SnapshotInfo,
    /// About historical snapshot evidence, not live source currency.
    pub comparison_state: SnapshotState,
    pub changes: Vec<Change>,
    pub excluded: Vec<Excluded>,
    pub summary: Summary,
}

impl DiffReport {
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }
}

fn compatible(info: &SnapshotInfo) -> bool {
    info.schema_version == Some(SCHEMA_VERSION)
        && info.hash_algo.as_deref() == Some("blake3")
        && info.index_uuid.as_ref().is_some_and(|id| !id.is_empty())
        && info.state != SnapshotState::Incompatible
}

fn trusted_hash(entry: &Entry) -> Option<[u8; 32]> {
    // v3 hardlink hashes were copied by display-name lookup, and link sizes
    // need not describe target content. Do not promote them to byte evidence.
    (entry.entry_type == EntryType::File && entry.flags & flags::READ_ERROR == 0)
        .then_some(entry.content_hash)
        .flatten()
}

fn metadata_known(entry: &Entry) -> bool {
    entry.mtime_unix.is_some() && entry.mode.is_some() && entry.flags & flags::PAX_UNPARSED == 0
}

fn same_path(before: &Entry, after: &Entry) -> (ChangeKind, Reason) {
    if before.entry_type != EntryType::File || after.entry_type != EntryType::File {
        return (ChangeKind::Inconclusive, Reason::UnsupportedEntryType);
    }
    match (trusted_hash(before), trusted_hash(after)) {
        (Some(a), Some(b)) if a != b => (ChangeKind::ContentChanged, Reason::DifferentContent),
        (Some(a), Some(b)) if a == b && before.size == after.size => {
            if !metadata_known(before) || !metadata_known(after) {
                (ChangeKind::Inconclusive, Reason::MissingMetadataEvidence)
            } else if before.mtime_unix == after.mtime_unix && before.mode == after.mode {
                (ChangeKind::ByteIdentical, Reason::EqualContentAndMetadata)
            } else {
                (
                    ChangeKind::MetadataOnlyChanged,
                    Reason::EqualContentDifferentMetadata,
                )
            }
        }
        _ => (ChangeKind::Inconclusive, Reason::MissingContentEvidence),
    }
}

fn visible(snapshot: &Snapshot) -> Result<BTreeMap<&[u8], &Entry>> {
    let mut ids = BTreeSet::new();
    let mut paths: BTreeMap<&[u8], &Entry> = BTreeMap::new();
    ensure!(
        snapshot.info.state != SnapshotState::Unavailable || snapshot.entries.is_empty(),
        "unavailable snapshot must not carry unqualified rows"
    );
    for entry in &snapshot.entries {
        ensure!(
            entry.file_id > 0 && ids.insert(entry.file_id),
            "invalid or duplicate file_id"
        );
        let winner = paths.entry(entry.path.as_slice()).or_insert(entry);
        if entry.file_id > winner.file_id {
            *winner = entry;
        }
    }
    // Recompute shadows using RAW paths. v3 SHADOWED flags were computed
    // from lossy display paths; two distinct non-UTF-8 names can collide.
    Ok(paths)
}

fn excluded(snapshot: &Snapshot, side: Side, paths: &BTreeMap<&[u8], &Entry>) -> Vec<Excluded> {
    snapshot
        .entries
        .iter()
        .filter(|entry| paths[entry.path.as_slice()].file_id != entry.file_id)
        .map(|entry| Excluded {
            side,
            reason: Reason::ShadowedPath,
            entry: entry.into(),
        })
        .collect()
}

/// Content multiplicity includes ALL rows, including matched paths and
/// shadowed rows. Unknown content anywhere prevents unique move evidence:
/// an unhashable entry could contain the candidate bytes too.
fn unique_hashes(snapshot: &Snapshot) -> BTreeMap<[u8; 32], &Entry> {
    if snapshot.info.state != SnapshotState::Complete
        || snapshot.entries.iter().any(|e| trusted_hash(e).is_none())
    {
        return BTreeMap::new();
    }
    let mut by_hash: BTreeMap<[u8; 32], Vec<&Entry>> = BTreeMap::new();
    for entry in &snapshot.entries {
        by_hash
            .entry(trusted_hash(entry).expect("checked above"))
            .or_default()
            .push(entry);
    }
    by_hash
        .into_iter()
        .filter_map(|(hash, entries)| (entries.len() == 1).then_some((hash, entries[0])))
        .collect()
}

fn change(
    kind: ChangeKind,
    reason: Reason,
    before: Option<&Entry>,
    after: Option<&Entry>,
) -> Change {
    Change {
        kind,
        reason,
        before: before.map(Into::into),
        after: after.map(Into::into),
    }
}

fn change_key(change: &Change) -> (&str, i64, Option<i64>) {
    let entry = change
        .before
        .as_ref()
        .or(change.after.as_ref())
        .expect("one side exists");
    (
        &entry.path_bytes,
        entry.file_id,
        change.after.as_ref().map(|e| e.file_id),
    )
}

/// Deterministic diff of the effective raw-path namespace. A move means a
/// unique hash+size correspondence, not proof of an actual filesystem rename.
/// An unavailable/incomplete peer cannot establish an addition or removal.
/// Malformed entry IDs fail as an error rather than discarding evidence.
pub fn compare(before: &Snapshot, after: &Snapshot) -> Result<DiffReport> {
    let left = visible(before)?;
    let right = visible(after)?;
    let is_compatible = compatible(&before.info) && compatible(&after.info);
    let mut changes = Vec::new();
    let mut excluded = excluded(before, Side::Before, &left);
    excluded.extend(self::excluded(after, Side::After, &right));
    let mut used_after = BTreeSet::new();
    let before_hashes = unique_hashes(before);
    let after_hashes = unique_hashes(after);

    for (path, old) in &left {
        if let Some(new) = right.get(path) {
            let (kind, reason) = if is_compatible {
                same_path(old, new)
            } else {
                (ChangeKind::Inconclusive, Reason::IncompatibleSnapshots)
            };
            changes.push(change(kind, reason, Some(old), Some(new)));
            used_after.insert(new.file_id);
            continue;
        }
        let moved_to = trusted_hash(old).and_then(|hash| {
            if !is_compatible || !before_hashes.contains_key(&hash) {
                return None;
            }
            after_hashes.get(&hash).copied().filter(|new| {
                !left.contains_key(new.path.as_slice())
                    && right
                        .get(new.path.as_slice())
                        .is_some_and(|e| e.file_id == new.file_id)
                    && old.size == new.size
            })
        });
        if let Some(new) = moved_to {
            changes.push(change(
                ChangeKind::Moved,
                Reason::UniqueContentCorrespondence,
                Some(old),
                Some(new),
            ));
            used_after.insert(new.file_id);
        } else {
            let (kind, reason) = absence(&after.info, is_compatible, ChangeKind::Removed);
            changes.push(change(kind, reason, Some(old), None));
        }
    }
    for new in right.values().filter(|e| !used_after.contains(&e.file_id)) {
        let (kind, reason) = absence(&before.info, is_compatible, ChangeKind::Added);
        changes.push(change(kind, reason, None, Some(new)));
    }
    // Hex preserves byte lexicographic order. Anchor to the old path when
    // present, otherwise the new one; IDs make the key explicitly total.
    changes.sort_by(|a, b| change_key(a).cmp(&change_key(b)));
    excluded.sort_by(|a, b| {
        (a.side, &a.entry.path_bytes, a.entry.file_id).cmp(&(
            b.side,
            &b.entry.path_bytes,
            b.entry.file_id,
        ))
    });
    let mut summary = Summary {
        excluded: excluded.len(),
        ..Summary::default()
    };
    for row in &changes {
        *match row.kind {
            ChangeKind::Added => &mut summary.added,
            ChangeKind::Removed => &mut summary.removed,
            ChangeKind::Moved => &mut summary.moved,
            ChangeKind::ByteIdentical => &mut summary.byte_identical,
            ChangeKind::MetadataOnlyChanged => &mut summary.metadata_only_changed,
            ChangeKind::ContentChanged => &mut summary.content_changed,
            ChangeKind::Inconclusive => &mut summary.inconclusive,
        } += 1;
    }
    let comparison_state = if before.info.state == SnapshotState::Unavailable
        || after.info.state == SnapshotState::Unavailable
    {
        SnapshotState::Unavailable
    } else if !is_compatible {
        SnapshotState::Incompatible
    } else if before.info.state != SnapshotState::Complete
        || after.info.state != SnapshotState::Complete
        || summary.inconclusive > 0
    {
        SnapshotState::Incomplete
    } else {
        SnapshotState::Complete
    };
    Ok(DiffReport {
        version: 1,
        before: before.info.clone(),
        after: after.info.clone(),
        comparison_state,
        changes,
        excluded,
        summary,
    })
}

fn absence(other: &SnapshotInfo, is_compatible: bool, kind: ChangeKind) -> (ChangeKind, Reason) {
    if other.state == SnapshotState::Unavailable || other.state == SnapshotState::Incomplete {
        (ChangeKind::Inconclusive, Reason::OtherSnapshotIncomplete)
    } else if !is_compatible {
        (ChangeKind::Inconclusive, Reason::IncompatibleSnapshots)
    } else {
        (kind, Reason::PathAbsent)
    }
}
