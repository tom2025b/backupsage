//! Minimum-copy floors (#96, child of #40): classify every content group
//! from [`crate::coverage::group`] against a configurable replica target.
//!
//! [`evaluate`] is pure logic over the engine's output plus caller-supplied
//! source statuses. It performs no filesystem, index or master I/O.
//!
//! **Unknown never reads as below the floor or as meeting it.** A group
//! meets the floor only when its trusted replicas reach it. Below that, any
//! `Unknown` presence makes it `Inconclusive`; only a fully known count can
//! be `BelowFloor`.

use anyhow::{bail, Result};

use crate::coverage::{Coverage, Exclusion, RowRef};

/// Replicas wanted per content when the caller does not choose.
pub const DEFAULT_MIN_COPIES: usize = 2;

/// A source's registry state, as the caller last observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceStatus {
    Ok,
    Incomplete,
    StaleIndex,
    DbMissing,
    ArchiveMissing,
}

impl SourceStatus {
    /// Whether a copy observed in a source with this status is trusted to
    /// count toward the floor.
    pub fn counts_toward_floor(self) -> bool {
        let _ = self;
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloorParams {
    /// Distinct trusted sources wanted per content; at least 1.
    pub min_copies: usize,
    /// Content of a known length below this is out of scope.
    pub min_size: u64,
    /// Keep zero-length content in scope.
    pub include_empty: bool,
}

impl Default for FloorParams {
    fn default() -> Self {
        FloorParams {
            min_copies: DEFAULT_MIN_COPIES,
            min_size: 0,
            include_empty: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    MeetsFloor,
    BelowFloor,
    Inconclusive,
}

/// One observed copy, trusted or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorCopy {
    pub row: RowRef,
    pub source_label: String,
    pub status: SourceStatus,
    pub counts_toward_floor: bool,
}

#[derive(Debug, Clone)]
pub struct GroupFloor {
    pub content_hash: [u8; 32],
    pub size: Option<u64>,
    pub verdict: Verdict,
    /// Exactly one trusted replica and no unknown presence anywhere.
    pub only_copy: bool,
    /// Sources holding a copy whose status counts toward the floor.
    pub trusted_replicas: usize,
    /// Sources holding a copy whose status does not count.
    pub untrusted_replicas: usize,
    /// Sources where presence could not be determined.
    pub unknown_sources: usize,
    /// Every copy, trusted or not, sorted by (source id, raw path, file id).
    pub copies: Vec<FloorCopy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupExclusionReason {
    EmptyContent,
    BelowMinSize,
}

/// A whole content group left out of scope; its copies are still listed.
#[derive(Debug, Clone)]
pub struct GroupExclusion {
    pub content_hash: [u8; 32],
    pub size: Option<u64>,
    pub reason: GroupExclusionReason,
    pub copies: Vec<RowRef>,
}

/// Totals derived from the emitted rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorSummary {
    pub min_copies: usize,
    pub groups: usize,
    pub meets_floor: usize,
    pub below_floor: usize,
    pub inconclusive: usize,
    pub only_copy: usize,
    pub excluded_groups: usize,
    pub shadowed_rows: usize,
    pub symlink_rows: usize,
    pub unmatched_hardlink_rows: usize,
    pub hardlink_aliases: usize,
    pub unknown_content_rows: usize,
}

#[derive(Debug, Clone)]
pub struct FloorReport {
    /// In-scope groups, in content-hash order.
    pub groups: Vec<GroupFloor>,
    /// Out-of-scope groups, in content-hash order.
    pub excluded_groups: Vec<GroupExclusion>,
    /// The engine's row exclusions, unchanged.
    pub excluded_rows: Vec<Exclusion>,
    pub summary: FloorSummary,
}

/// Classify every group in `coverage` against `params.min_copies`.
/// `statuses` must name every coverage source exactly once.
pub fn evaluate(
    coverage: &Coverage,
    statuses: &[(i64, SourceStatus)],
    params: &FloorParams,
) -> Result<FloorReport> {
    let _ = (coverage, statuses, params);
    bail!("floor evaluation is not implemented yet")
}
