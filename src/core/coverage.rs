//! Coverage grouping (#95, child of #40): which byte-identical content lives
//! in which sources, and how sure we are about it.
//!
//! [`group`] is a pure function over caller-supplied, in-memory sources. It
//! performs no filesystem, index or master I/O; loading, source status,
//! minimum-copy floors and rendering belong to later children.
//!
//! **Unknown data never reads as zero copies.** Per group, every source is
//! `Present`, `Absent` or `Unknown`, and `Absent` is claimed only when the
//! source is complete, carries content hashes, and holds no unhashed row that
//! could be this content. Anything else is `Unknown`, and the group's replica
//! count becomes a lower bound.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{bail, Result};

use crate::store::flags;

/// How much of a source's content the caller could observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEvidence {
    /// Every entry was indexed with content hashes.
    Complete,
    /// Indexing stopped early: observed rows are real, but more may exist.
    Incomplete,
    /// No usable rows at all (index offline, unreadable, incompatible).
    Unavailable,
    /// Rows exist but carry no content hashes (metadata-only, v2-limited).
    NoContentHashes,
    /// The rows are real history, but the source cannot be reached now:
    /// its copies are listed, never counted as present, and its presence
    /// is unknown for every group.
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Hardlink,
    Symlink,
}

/// One indexed entry of a source.
#[derive(Debug, Clone)]
pub struct CoverageRow {
    /// Per-source row id; later entries have greater ids.
    pub file_id: i64,
    /// Exact path bytes; the effective namespace is keyed on these.
    pub path_raw: Vec<u8>,
    pub entry: EntryKind,
    pub size: Option<u64>,
    /// Full-content BLAKE3.
    pub content_hash: Option<[u8; 32]>,
    /// [`crate::store::flags`] bits.
    pub flags: i64,
}

#[derive(Debug, Clone)]
pub struct CoverageSource {
    pub source_id: i64,
    pub label: String,
    pub evidence: SourceEvidence,
    pub rows: Vec<CoverageRow>,
}

/// A row as it appears in the result.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowRef {
    pub source_id: i64,
    pub path_raw: Vec<u8>,
    pub file_id: i64,
}

impl RowRef {
    /// Lossy display text; `path_raw` stays authoritative.
    pub fn display_path(&self) -> String {
        String::from_utf8_lossy(&self.path_raw).into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaCount {
    /// Every source was conclusively present or absent.
    Exact(usize),
    /// At least this many sources; some could not be ruled out.
    AtLeast(usize),
}

impl ReplicaCount {
    pub fn observed(self) -> usize {
        match self {
            ReplicaCount::Exact(n) | ReplicaCount::AtLeast(n) => n,
        }
    }

    pub fn is_exact(self) -> bool {
        matches!(self, ReplicaCount::Exact(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    SourceUnavailable,
    SourceIncomplete,
    SourceHasNoContentHashes,
    SourceUnreachable,
    /// A complete source holds unhashed rows that could be this content.
    UnhashedRowsMayMatch {
        rows: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Effective copies observed in this source.
    Present {
        copies: usize,
    },
    /// Conclusively not in this source.
    Absent,
    Unknown(UnknownReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePresence {
    pub source_id: i64,
    pub presence: Presence,
}

#[derive(Debug, Clone)]
pub struct ContentGroup {
    pub content_hash: [u8; 32],
    /// Content length when trustworthy member sizes agree.
    pub size: Option<u64>,
    pub replicas: ReplicaCount,
    /// One entry per source, in source-id order.
    pub presence: Vec<SourcePresence>,
    /// Effective regular-file copies.
    pub copies: Vec<RowRef>,
    /// Hardlinks sharing a same-source copy's bytes; never counted.
    pub aliases: Vec<RowRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownContentReason {
    ReadError,
    UnsupportedSparse,
    /// Unparsed pax records may hide sparse metadata: any hash covers what
    /// tar-rs read, which need not be the logical file.
    PaxUnparsed,
    NotHashed,
}

#[derive(Debug, Clone)]
pub struct UnknownContent {
    pub row: RowRef,
    pub size: Option<u64>,
    pub reason: UnknownContentReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    /// A later row at the same raw path wins on extraction.
    Shadowed,
    Symlink,
    /// Hardlink with no effective same-source copy of its content.
    UnmatchedHardlink,
}

#[derive(Debug, Clone)]
pub struct Exclusion {
    pub row: RowRef,
    pub reason: ExclusionReason,
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub source_id: i64,
    pub label: String,
    pub evidence: SourceEvidence,
}

/// Every input row appears exactly once: as a group copy or alias, as
/// unknown content, or as an exclusion.
#[derive(Debug, Clone)]
pub struct Coverage {
    /// Sorted by source id.
    pub sources: Vec<SourceInfo>,
    /// Sorted by content hash.
    pub groups: Vec<ContentGroup>,
    /// Sorted by (source id, raw path, file id).
    pub unknown_content: Vec<UnknownContent>,
    /// Sorted by (source id, raw path, file id).
    pub exclusions: Vec<Exclusion>,
}

/// Unhashed effective rows of one source, bucketed for size matching.
#[derive(Default)]
struct UnknownRows {
    /// Rows whose size cannot rule any content out.
    untrusted: usize,
    /// Rows with a trustworthy size, keyed by it.
    by_size: BTreeMap<u64, usize>,
    trusted: usize,
}

impl UnknownRows {
    fn add(&mut self, size: Option<u64>) {
        match size {
            Some(n) => {
                *self.by_size.entry(n).or_default() += 1;
                self.trusted += 1;
            }
            None => self.untrusted += 1,
        }
    }

    /// How many of these rows could hold content of length `len`.
    fn could_match(&self, len: Option<u64>) -> usize {
        self.untrusted
            + match len {
                Some(n) => self.by_size.get(&n).copied().unwrap_or(0),
                None => self.trusted,
            }
    }
}

/// Flags under which a stored hash cannot prove a row's content: a read
/// error cut the stream short, and unparsed pax records may hide sparse
/// metadata, so the hash can cover condensed fragments (tests/sparse.rs).
const UNTRUSTED_HASH: i64 = flags::READ_ERROR | flags::PAX_UNPARSED;

/// A row's size when it is the content length: sparse, unparsed-PAX and
/// read-error sizes are not.
fn trusted_size(row: &CoverageRow) -> Option<u64> {
    if row.flags & (flags::SPARSE | flags::PAX_UNPARSED | flags::READ_ERROR) != 0 {
        None
    } else {
        row.size
    }
}

#[derive(Default)]
struct Building {
    copies: BTreeMap<i64, Vec<RowRef>>,
    aliases: Vec<RowRef>,
    sizes: BTreeSet<u64>,
}

/// Group byte-identical content across `sources`.
///
/// Rules, per source:
/// 1. The effective namespace is keyed on raw path bytes; the greatest file
///    id wins. Earlier rows are `Shadowed` exclusions and never count. The
///    stored `SHADOWED` flag is ignored: v3 computed it on display text.
/// 2. Effective files with a hash and neither a read error nor unparsed pax
///    records are copies of that content; the rest are unknown content.
/// 3. Hardlinks carry no bytes: an alias when an effective same-source copy
///    of their hash exists, otherwise an `UnmatchedHardlink` exclusion.
///    Symlinks are excluded.
///
/// Presence of a group in a source without a copy is `Absent` only for a
/// complete source none of whose unknown rows could be that content;
/// otherwise it is `Unknown` and the replica count is a lower bound.
pub fn group(sources: &[CoverageSource]) -> Result<Coverage> {
    let mut ordered: Vec<&CoverageSource> = sources.iter().collect();
    ordered.sort_by_key(|s| s.source_id);
    for pair in ordered.windows(2) {
        if pair[0].source_id == pair[1].source_id {
            bail!("duplicate coverage source id {}", pair[0].source_id);
        }
    }

    let mut groups: BTreeMap<[u8; 32], Building> = BTreeMap::new();
    let mut unknown_rows: Vec<UnknownRows> = Vec::with_capacity(ordered.len());
    let mut unknown_content = Vec::new();
    let mut exclusions = Vec::new();

    for source in &ordered {
        let id = source.source_id;
        match source.evidence {
            SourceEvidence::Unavailable if !source.rows.is_empty() => {
                bail!("coverage source {id} is unavailable but carries rows")
            }
            SourceEvidence::NoContentHashes
                if source.rows.iter().any(|r| r.content_hash.is_some()) =>
            {
                bail!("coverage source {id} has no content hashes but a row carries one")
            }
            _ => {}
        }

        let mut file_ids = BTreeSet::new();
        let mut latest: HashMap<&[u8], i64> = HashMap::new();
        for row in &source.rows {
            if !file_ids.insert(row.file_id) {
                bail!("coverage source {id} repeats file id {}", row.file_id);
            }
            let winner = latest.entry(&row.path_raw).or_insert(row.file_id);
            *winner = (*winner).max(row.file_id);
        }

        let row_ref = |row: &CoverageRow| RowRef {
            source_id: id,
            path_raw: row.path_raw.clone(),
            file_id: row.file_id,
        };
        let mut unknown = UnknownRows::default();
        let mut hardlinks = Vec::new();
        for row in &source.rows {
            if latest[row.path_raw.as_slice()] != row.file_id {
                exclusions.push(Exclusion {
                    row: row_ref(row),
                    reason: ExclusionReason::Shadowed,
                });
                continue;
            }
            match (row.entry, row.content_hash) {
                (EntryKind::Symlink, _) => exclusions.push(Exclusion {
                    row: row_ref(row),
                    reason: ExclusionReason::Symlink,
                }),
                (EntryKind::Hardlink, _) => hardlinks.push(row),
                (EntryKind::File, Some(hash)) if row.flags & UNTRUSTED_HASH == 0 => {
                    let building = groups.entry(hash).or_default();
                    building.copies.entry(id).or_default().push(row_ref(row));
                    building.sizes.extend(trusted_size(row));
                }
                (EntryKind::File, _) => {
                    let reason = if row.flags & flags::READ_ERROR != 0 {
                        UnknownContentReason::ReadError
                    } else if row.flags & flags::PAX_UNPARSED != 0 {
                        UnknownContentReason::PaxUnparsed
                    } else if row.flags & flags::SPARSE != 0 {
                        UnknownContentReason::UnsupportedSparse
                    } else {
                        UnknownContentReason::NotHashed
                    };
                    unknown.add(trusted_size(row));
                    unknown_content.push(UnknownContent {
                        row: row_ref(row),
                        size: row.size,
                        reason,
                    });
                }
            }
        }

        // After this source's files, so every same-source copy is known.
        for row in hardlinks {
            match row
                .content_hash
                .and_then(|hash| groups.get_mut(&hash))
                .filter(|building| building.copies.contains_key(&id))
            {
                Some(building) => building.aliases.push(row_ref(row)),
                None => exclusions.push(Exclusion {
                    row: row_ref(row),
                    reason: ExclusionReason::UnmatchedHardlink,
                }),
            }
        }
        unknown_rows.push(unknown);
    }

    let groups = groups
        .into_iter()
        .map(|(content_hash, building)| {
            let size = match building.sizes.len() {
                1 => building.sizes.first().copied(),
                _ => None,
            };
            let presence: Vec<SourcePresence> = ordered
                .iter()
                .zip(&unknown_rows)
                .map(|(source, unknown)| SourcePresence {
                    source_id: source.source_id,
                    presence: presence_in(source, unknown, &building, size),
                })
                .collect();
            let present = presence
                .iter()
                .filter(|p| matches!(p.presence, Presence::Present { .. }))
                .count();
            let replicas = if presence
                .iter()
                .any(|p| matches!(p.presence, Presence::Unknown(_)))
            {
                ReplicaCount::AtLeast(present)
            } else {
                ReplicaCount::Exact(present)
            };
            let mut copies: Vec<RowRef> = building.copies.into_values().flatten().collect();
            copies.sort();
            let mut aliases = building.aliases;
            aliases.sort();
            ContentGroup {
                content_hash,
                size,
                replicas,
                presence,
                copies,
                aliases,
            }
        })
        .collect();

    unknown_content.sort_by(|a: &UnknownContent, b| a.row.cmp(&b.row));
    exclusions.sort_by(|a: &Exclusion, b| a.row.cmp(&b.row));

    Ok(Coverage {
        sources: ordered
            .iter()
            .map(|s| SourceInfo {
                source_id: s.source_id,
                label: s.label.clone(),
                evidence: s.evidence,
            })
            .collect(),
        groups,
        unknown_content,
        exclusions,
    })
}

/// Where a group stands in one source. Only a copy proves presence, and
/// only complete, fully explained evidence proves absence.
fn presence_in(
    source: &CoverageSource,
    unknown: &UnknownRows,
    building: &Building,
    size: Option<u64>,
) -> Presence {
    if let Some(copies) = building.copies.get(&source.source_id) {
        return Presence::Present {
            copies: copies.len(),
        };
    }
    match source.evidence {
        SourceEvidence::Unavailable => Presence::Unknown(UnknownReason::SourceUnavailable),
        SourceEvidence::Incomplete => Presence::Unknown(UnknownReason::SourceIncomplete),
        SourceEvidence::NoContentHashes => {
            Presence::Unknown(UnknownReason::SourceHasNoContentHashes)
        }
        SourceEvidence::Complete | SourceEvidence::Unreachable => match unknown.could_match(size) {
            0 => Presence::Absent,
            rows => Presence::Unknown(UnknownReason::UnhashedRowsMayMatch { rows }),
        },
    }
}
