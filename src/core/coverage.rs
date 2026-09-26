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

pub fn group(sources: &[CoverageSource]) -> Result<Coverage> {
    let _ = (sources, flags::SPARSE);
    bail!("coverage grouping is not implemented yet")
}
