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

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::coverage::{Coverage, Exclusion, ExclusionReason, Presence, RowRef, UnknownContent};

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
    /// count toward the floor. `Ok` and `Incomplete` sources hashed the copy
    /// from bytes nothing has since flagged; a stale index, a missing index
    /// or a missing source leaves the copy unverified. Untrusted copies are
    /// still listed.
    pub fn counts_toward_floor(self) -> bool {
        matches!(self, SourceStatus::Ok | SourceStatus::Incomplete)
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
    /// Hardlinks sharing a same-source copy's bytes; listed, never counted.
    pub aliases: Vec<RowRef>,
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
    pub aliases: Vec<RowRef>,
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
    /// The engine's unknown-content rows with their reasons, unchanged.
    pub unknown_content: Vec<UnknownContent>,
    pub summary: FloorSummary,
}

/// Classify every group in `coverage` against `params.min_copies`.
/// `statuses` must name every coverage source exactly once.
pub fn evaluate(
    coverage: &Coverage,
    statuses: &[(i64, SourceStatus)],
    params: &FloorParams,
) -> Result<FloorReport> {
    if params.min_copies == 0 {
        bail!("the minimum-copy floor must be at least 1");
    }
    let mut status_of: BTreeMap<i64, SourceStatus> = BTreeMap::new();
    for &(id, status) in statuses {
        if status_of.insert(id, status).is_some() {
            bail!("source {id} has more than one status");
        }
    }
    let labels: BTreeMap<i64, &str> = coverage
        .sources
        .iter()
        .map(|s| (s.source_id, s.label.as_str()))
        .collect();
    if let Some(id) = labels.keys().find(|id| !status_of.contains_key(id)) {
        bail!("source {id} has no status");
    }
    if let Some(id) = status_of.keys().find(|id| !labels.contains_key(id)) {
        bail!("status given for unknown source {id}");
    }

    let mut groups = Vec::new();
    let mut excluded_groups = Vec::new();
    for group in &coverage.groups {
        let out_of_scope = match group.size {
            Some(0) if !params.include_empty => Some(GroupExclusionReason::EmptyContent),
            Some(n) if n < params.min_size => Some(GroupExclusionReason::BelowMinSize),
            // An unknown length is never excluded by size.
            _ => None,
        };
        if let Some(reason) = out_of_scope {
            excluded_groups.push(GroupExclusion {
                content_hash: group.content_hash,
                size: group.size,
                reason,
                copies: group.copies.clone(),
                aliases: group.aliases.clone(),
            });
            continue;
        }

        let (mut trusted, mut untrusted, mut unknown) = (0, 0, 0);
        for p in &group.presence {
            match p.presence {
                Presence::Present { .. } if status_of[&p.source_id].counts_toward_floor() => {
                    trusted += 1
                }
                Presence::Present { .. } => untrusted += 1,
                Presence::Unknown(_) => unknown += 1,
                Presence::Absent => {}
            }
        }
        // Trusted replicas alone decide a met floor; below it, any unknown
        // presence could hide a trusted copy, so only a fully known count
        // is judged below the floor.
        let verdict = if trusted >= params.min_copies {
            Verdict::MeetsFloor
        } else if unknown > 0 {
            Verdict::Inconclusive
        } else {
            Verdict::BelowFloor
        };
        // The engine emits copies sorted; mapping them in place keeps that.
        let copies: Vec<FloorCopy> = group
            .copies
            .iter()
            .map(|row| {
                let status = status_of[&row.source_id];
                FloorCopy {
                    row: row.clone(),
                    source_label: labels[&row.source_id].to_owned(),
                    status,
                    counts_toward_floor: status.counts_toward_floor(),
                }
            })
            .collect();
        groups.push(GroupFloor {
            content_hash: group.content_hash,
            size: group.size,
            verdict,
            only_copy: trusted == 1 && unknown == 0,
            trusted_replicas: trusted,
            untrusted_replicas: untrusted,
            unknown_sources: unknown,
            copies,
            aliases: group.aliases.clone(),
        });
    }

    let count_rows = |reason| {
        coverage
            .exclusions
            .iter()
            .filter(|e| e.reason == reason)
            .count()
    };
    let count = |verdict| groups.iter().filter(|g| g.verdict == verdict).count();
    let unknown_content = coverage.unknown_content.clone();
    let summary = FloorSummary {
        min_copies: params.min_copies,
        groups: groups.len(),
        meets_floor: count(Verdict::MeetsFloor),
        below_floor: count(Verdict::BelowFloor),
        inconclusive: count(Verdict::Inconclusive),
        only_copy: groups.iter().filter(|g| g.only_copy).count(),
        excluded_groups: excluded_groups.len(),
        shadowed_rows: count_rows(ExclusionReason::Shadowed),
        symlink_rows: count_rows(ExclusionReason::Symlink),
        unmatched_hardlink_rows: count_rows(ExclusionReason::UnmatchedHardlink),
        hardlink_aliases: groups.iter().map(|g| g.aliases.len()).sum::<usize>()
            + excluded_groups
                .iter()
                .map(|g| g.aliases.len())
                .sum::<usize>(),
        unknown_content_rows: unknown_content.len(),
    };
    Ok(FloorReport {
        groups,
        excluded_groups,
        excluded_rows: coverage.exclusions.clone(),
        unknown_content,
        summary,
    })
}
