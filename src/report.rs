//! The dedup report: serde types whose JSON shape is a stable contract
//! (`version: 1`) — the future web UI consumes exactly this. Renaming or
//! removing a field is a breaking change; add fields instead.

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct DedupReport {
    pub version: u32,
    pub params: ReportParams,
    pub archives: Vec<ReportArchive>,
    pub groups: Vec<Group>,
    pub summary: Summary,
}

#[derive(Debug, Serialize)]
pub struct ReportParams {
    pub exact: bool,
    pub near: bool,
    pub threshold: u32,
    pub min_size: u64,
    pub include_empty: bool,
    pub across_only: bool,
    pub keep_policy: String,
    /// When near-dup runs, images with a pHash are grouped perceptually and
    /// excluded from exact groups (distance-0 pairs appear there instead).
    pub images_grouped_perceptually: bool,
}

#[derive(Debug, Serialize)]
pub struct ReportArchive {
    pub archive_id: i64,
    pub label: String,
    pub source: String,
    pub source_type: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct Group {
    pub group_id: usize,
    /// `"exact"` or `"near"`.
    pub match_kind: String,
    /// Largest hamming distance to the kept copy (0 for exact groups).
    pub max_distance: u32,
    pub reclaimable_bytes: u64,
    pub members: Vec<Member>,
}

#[derive(Debug, Serialize)]
pub struct Member {
    pub archive_id: i64,
    pub archive_label: String,
    pub file_id: i64,
    pub path: String,
    /// Lowercase hex of the raw path bytes — present only when `path`
    /// is a lossy rendering of a non-UTF-8 original (added v1.0.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_bytes: Option<String>,
    pub kind: String,
    pub size: u64,
    /// `"b3:<hex>"`.
    pub content_hash: Option<String>,
    /// pHash as 16 hex digits.
    pub phash: Option<String>,
    pub mtime_unix: Option<i64>,
    pub exif_unix: Option<i64>,
    pub best_ts_unix: Option<i64>,
    /// `"exif:DateTimeOriginal"`, `"tar-mtime"`, `"archive-date"`, `"none"`.
    pub best_ts_source: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub hamming_to_keep: Option<u32>,
    pub keep: bool,
    /// Why this member was chosen (only on the kept member).
    pub keep_reason: Option<String>,
    pub shadowed: bool,
    pub sparse: bool,
    /// Set on hardlink members: the path their content lives under.
    pub hardlink_of: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub groups: usize,
    pub duplicate_files: usize,
    pub reclaimable_bytes: u64,
    /// Registered DBs that are unreachable — dedup used their replicas.
    pub archives_offline: Vec<String>,
    pub archives_incomplete: Vec<String>,
    /// (label, reason) — archives contributing no dedup rows.
    pub skipped_archives: Vec<(String, String)>,
    pub images_without_phash: u64,
    /// Bytes in rows shadowed by a later same-path entry in the same source
    /// (intra-archive waste; reclaiming means rewriting the archive).
    pub intra_archive_shadowed_bytes: u64,
    /// Near-dup buckets skipped because they exceeded the bucket cap.
    pub near_buckets_skipped: u64,
}

/// Lowercase hex rendering used for every `*_bytes` JSON field.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl DedupReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("report serialization cannot fail")
    }

    /// True when the result carries caveats scripts should notice (exit 2).
    pub fn has_skips(&self) -> bool {
        !self.summary.archives_offline.is_empty()
            || !self.summary.archives_incomplete.is_empty()
            || !self.summary.skipped_archives.is_empty()
    }
}
