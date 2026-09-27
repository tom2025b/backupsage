//! The `backupsage coverage` report (#98, child of #40): the versioned JSON
//! document and the scope selection behind it.
//!
//! Every list is sorted here on an explicit key (content hash; source id;
//! raw path bytes; file id), never on the order the engine, the registry or
//! a map happened to produce. Totals are counted from the emitted rows.
//!
//! Scope filters mirror `dedup` where the evidence allows: `--archive`
//! chooses sources, `--min-size`/`--include-empty` put content out of scope
//! (listed, never dropped), and `--ext`/`--path-glob` choose which content
//! is reported. A path filter never removes a copy from a count: content is
//! reported, with every copy, when any of its paths matches.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::Serialize;

use crate::coverage::{
    ContentGroup, Coverage, ExclusionReason, Presence, ReplicaCount, RowRef, SourceEvidence,
    UnknownContentReason, UnknownReason,
};
use crate::coverage_input::{LoadCode, LoadedSource, RegistrySource};
use crate::floors::{
    FloorCopy, FloorParams, FloorReport, GroupExclusionReason, SourceStatus, Verdict,
};
use crate::master;
use crate::report::to_hex;

/// The report's `version`; additive changes keep it.
pub const REPORT_VERSION: u32 = 1;

/// What the caller asked for.
#[derive(Debug, Clone, Default)]
pub struct CoverageScope {
    pub floor: FloorParams,
    /// Extensions as given (`jpg`, `.JPG`); matched like `dedup --ext`.
    pub exts: Vec<String>,
    /// SQLite GLOB over the display path, like `dedup --path-glob`.
    pub path_glob: Option<String>,
    /// One content kind, like `dedup --kind`.
    pub kind: Option<String>,
}

/// The kinds `--kind` accepts: dedup's documented set.
pub const KINDS: [&str; 5] = ["image", "raw", "video", "text", "binary"];

/// Resolve `--archive`/`--protected` values (source id, label or index
/// path) to source ids. Each value must name exactly one source.
pub fn resolve(registry: &[RegistrySource], wanted: &[String], flag: &str) -> Result<Vec<i64>> {
    let mut ids = Vec::new();
    for w in wanted {
        let found: Vec<i64> = registry
            .iter()
            .filter(|r| {
                r.source_id.to_string() == *w
                    || r.label == *w
                    || r.db_path.as_os_str() == std::ffi::OsStr::new(w)
            })
            .map(|r| r.source_id)
            .collect();
        match found.as_slice() {
            [] => bail!("{flag} '{w}' matches no source"),
            [one] => {
                if ids.contains(one) {
                    bail!("{flag} names source {one} more than once");
                }
                ids.push(*one);
            }
            many => bail!("{flag} '{w}' is ambiguous (sources {many:?}); use the id"),
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

/// Keep only the chosen sources; an empty choice keeps every source.
pub fn select(registry: Vec<RegistrySource>, ids: &[i64]) -> Vec<RegistrySource> {
    if ids.is_empty() {
        return registry;
    }
    registry
        .into_iter()
        .filter(|r| ids.contains(&r.source_id))
        .collect()
}

/// Every row's recorded kind, by (source id, file id).
type RowKinds = BTreeMap<(i64, i64), String>;

/// `--kind`, `--ext` and `--path-glob`, evaluated with dedup's own
/// semantics.
struct PathFilter {
    /// Lowercased, alphanumeric-only suffixes, as dedup cleans them.
    exts: Vec<String>,
    glob: Option<(Connection, String)>,
    /// The wanted kind, and every row's kind by (source id, file id).
    kind: Option<(String, RowKinds)>,
}

impl PathFilter {
    fn new(scope: &CoverageScope, sources: &[LoadedSource]) -> Result<Self> {
        // dedup matches `f.kind = ?` and so silently finds nothing for a
        // misspelt kind; here that would read as complete coverage of
        // nothing, so an unknown kind is refused.
        let kind = match &scope.kind {
            Some(k) if !KINDS.contains(&k.as_str()) => {
                bail!("unknown --kind '{k}' (use {})", KINDS.join(", "))
            }
            Some(k) => {
                let kinds = sources
                    .iter()
                    .flat_map(|s| {
                        s.kinds
                            .iter()
                            .map(|(&file_id, kind)| ((s.source_id, file_id), kind.clone()))
                    })
                    .collect();
                Some((k.clone(), kinds))
            }
            None => None,
        };
        let exts = scope
            .exts
            .iter()
            .map(|e| {
                e.chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_lowercase()
            })
            .collect();
        // GLOB is evaluated by SQLite itself, so it matches exactly what
        // `dedup --path-glob` matches. An in-memory database touches no file.
        let glob = match &scope.path_glob {
            Some(g) => Some((Connection::open_in_memory()?, g.clone())),
            None => None,
        };
        Ok(PathFilter { exts, glob, kind })
    }

    fn is_active(&self) -> bool {
        !self.exts.is_empty() || self.glob.is_some() || self.kind.is_some()
    }

    fn matches(&self, row: &RowRef) -> Result<bool> {
        if let Some((want, kinds)) = &self.kind {
            // dedup: f.kind = ?. A row with no recorded kind never matches.
            if kinds.get(&(row.source_id, row.file_id)) != Some(want) {
                return Ok(false);
            }
        }
        if !self.exts.is_empty() {
            // dedup: lower(path) LIKE '%.ext'. SQLite's lower() and LIKE fold
            // ASCII only, and the suffix is ASCII.
            let path = &row.path_raw;
            let hit = self.exts.iter().any(|e| {
                let want = format!(".{e}");
                path.len() >= want.len()
                    && path[path.len() - want.len()..].eq_ignore_ascii_case(want.as_bytes())
            });
            if !hit {
                return Ok(false);
            }
        }
        if let Some((conn, pattern)) = &self.glob {
            let hit: bool = conn
                .query_row(
                    "SELECT ?1 GLOB ?2",
                    rusqlite::params![row.display_path(), pattern],
                    |r| r.get(0),
                )
                .context("evaluating --path-glob")?;
            if !hit {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Content is in scope when any of its rows matches.
    fn any(&self, rows: &[&RowRef]) -> Result<bool> {
        for row in rows {
            if self.matches(row)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Where the sources come from.
#[derive(Debug, Clone, Copy)]
pub enum CoverageInput<'a> {
    /// The master's registry, read without writing anything (ADR 0011).
    Master(&'a std::path::Path),
    /// Ad-hoc indexes, source ids 1, 2, … in argument order.
    Indexes(&'a [std::path::PathBuf]),
}

/// The loaded sources and the report built from them.
pub struct CoverageRun {
    pub loaded: crate::coverage_input::LoadedCoverage,
    pub report: CoverageReport,
}

/// Load the chosen sources, group, classify and build the report.
/// `archives` and `protected` are `--archive`/`--protected` values.
pub fn run(
    input: CoverageInput<'_>,
    archives: &[String],
    protected: &[String],
    scope: &CoverageScope,
) -> Result<CoverageRun> {
    let (kind, registry) = match input {
        CoverageInput::Master(path) => ("master", crate::coverage_input::load_registry(path)?),
        CoverageInput::Indexes(dbs) => ("db", crate::coverage_input::adhoc_registry(dbs)?),
    };
    let chosen = resolve(&registry, archives, "--archive")?;
    let registry = select(registry, &chosen);
    let protected = resolve(&registry, protected, "--protected")?;
    let loaded = crate::coverage_input::build(&registry)?;
    let coverage = loaded.coverage()?;
    let floors = crate::floors::evaluate(&coverage, &loaded.statuses(), &protected, &scope.floor)?;
    let report = build_report(
        kind,
        &loaded.sources,
        &coverage,
        &floors,
        scope,
        (!archives.is_empty()).then_some(chosen),
        &protected,
    )?;
    Ok(CoverageRun { loaded, report })
}

// ── The JSON document ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub version: u32,
    /// `complete`, or `inconclusive` when any source is not a complete,
    /// `ok` source, or any emitted group or row is unknown.
    pub coverage_state: &'static str,
    pub params: ReportParams,
    pub sources: Vec<ReportSource>,
    pub groups: Vec<ReportGroup>,
    pub excluded_groups: Vec<ReportExcludedGroup>,
    pub unknown_content: Vec<ReportUnknownRow>,
    pub excluded_rows: Vec<ReportExcludedRow>,
    pub summary: ReportSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportParams {
    /// `master` or `db` (ad-hoc indexes).
    pub input: &'static str,
    pub min_copies: usize,
    pub min_size: u64,
    pub include_empty: bool,
    pub exts: Vec<String>,
    pub path_glob: Option<String>,
    pub kind: Option<String>,
    /// The chosen source ids, or null for every source.
    pub archives: Option<Vec<i64>>,
    pub protected: Vec<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportNote {
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportSource {
    pub source_id: i64,
    pub label: String,
    /// Display text; `db_path_bytes` is authoritative.
    pub db_path: String,
    pub db_path_bytes: String,
    pub source: Option<String>,
    pub source_type: Option<String>,
    pub registry_status: Option<String>,
    pub evidence: &'static str,
    pub status: &'static str,
    pub counts_toward_floor: bool,
    pub protected: bool,
    /// Rows handed to the engine.
    pub rows: usize,
    pub index_notes: Vec<ReportNote>,
    pub notes: Vec<ReportNote>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportReplicas {
    pub observed: usize,
    /// False when some source could not be ruled out: `observed` is then a
    /// lower bound.
    pub exact: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportPresence {
    pub source_id: i64,
    /// `present`, `absent` or `unknown`.
    pub state: &'static str,
    pub copies: Option<usize>,
    pub reason: Option<&'static str>,
    pub unhashed_rows: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportCopy {
    pub source_id: i64,
    pub label: String,
    pub path: String,
    pub path_bytes: String,
    pub file_id: i64,
    pub status: &'static str,
    pub counts_toward_floor: bool,
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportRow {
    pub source_id: i64,
    pub path: String,
    pub path_bytes: String,
    pub file_id: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportGroup {
    pub content_hash: String,
    pub size: Option<u64>,
    /// `meets_floor`, `below_floor` or `inconclusive`.
    pub verdict: &'static str,
    pub only_copy: bool,
    pub replicas: ReportReplicas,
    pub trusted_replicas: usize,
    pub untrusted_replicas: usize,
    pub protected_replicas: usize,
    pub unknown_sources: usize,
    pub presence: Vec<ReportPresence>,
    pub copies: Vec<ReportCopy>,
    pub aliases: Vec<ReportRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportExcludedGroup {
    pub content_hash: String,
    pub size: Option<u64>,
    /// `empty_content` or `below_min_size`.
    pub reason: &'static str,
    pub copies: Vec<ReportCopy>,
    pub aliases: Vec<ReportRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportUnknownRow {
    #[serde(flatten)]
    pub row: ReportRow,
    pub size: Option<u64>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportExcludedRow {
    #[serde(flatten)]
    pub row: ReportRow,
    /// `shadowed`, `symlink` or `unmatched_hardlink`.
    pub reason: &'static str,
}

/// Every total is counted from the emitted lists above.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReportSummary {
    pub min_copies: usize,
    pub sources: usize,
    /// Sources that are not complete, `ok` sources.
    pub sources_degraded: usize,
    pub groups: usize,
    pub meets_floor: usize,
    pub below_floor: usize,
    pub inconclusive: usize,
    pub only_copy: usize,
    pub protected_replicas: usize,
    pub excluded_groups: usize,
    pub unknown_content_rows: usize,
    pub shadowed_rows: usize,
    pub symlink_rows: usize,
    pub unmatched_hardlink_rows: usize,
    pub hardlink_aliases: usize,
}

impl CoverageReport {
    pub fn is_complete(&self) -> bool {
        self.coverage_state == "complete"
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)? + "\n")
    }
}

// ── Building it ─────────────────────────────────────────────────────────────

/// Build the report from the loaded sources and the engines' output. The
/// inputs may be in any order: every list is sorted here.
pub fn build_report(
    input: &'static str,
    sources: &[LoadedSource],
    coverage: &Coverage,
    floors: &FloorReport,
    scope: &CoverageScope,
    archives: Option<Vec<i64>>,
    protected: &[i64],
) -> Result<CoverageReport> {
    let filter = PathFilter::new(scope, sources)?;
    let is_protected = |id: i64| protected.contains(&id);

    let mut report_sources: Vec<ReportSource> = sources
        .iter()
        .map(|s| ReportSource {
            source_id: s.source_id,
            label: s.label.clone(),
            db_path: s.db_path.display().to_string(),
            db_path_bytes: to_hex(std::os::unix::ffi::OsStrExt::as_bytes(
                s.db_path.as_os_str(),
            )),
            source: s.source.clone(),
            source_type: s.source_type.clone(),
            registry_status: s.registry_status.clone(),
            evidence: evidence_str(s.evidence),
            status: status_str(s.status),
            counts_toward_floor: s.status.counts_toward_floor(),
            protected: is_protected(s.source_id),
            rows: s.rows.len(),
            index_notes: s
                .index_notes
                .iter()
                .map(|n| ReportNote {
                    code: n.code.as_str(),
                    detail: n.detail.clone(),
                })
                .collect(),
            notes: s
                .notes
                .iter()
                .map(|n| ReportNote {
                    code: load_code_str(n.code),
                    detail: n.detail.clone(),
                })
                .collect(),
        })
        .collect();
    report_sources.sort_by_key(|s| s.source_id);

    let engine_groups: BTreeMap<[u8; 32], &ContentGroup> = coverage
        .groups
        .iter()
        .map(|g| (g.content_hash, g))
        .collect();

    let mut groups = Vec::new();
    for g in &floors.groups {
        let rows: Vec<&RowRef> = g.copies.iter().map(|c| &c.row).chain(&g.aliases).collect();
        if filter.is_active() && !filter.any(&rows)? {
            continue;
        }
        let Some(engine) = engine_groups.get(&g.content_hash) else {
            bail!("floors reported content the coverage engine did not group");
        };
        let mut presence: Vec<ReportPresence> = engine
            .presence
            .iter()
            .map(|p| presence(p.source_id, p.presence))
            .collect();
        presence.sort_by_key(|p| p.source_id);
        let (observed, exact) = match engine.replicas {
            ReplicaCount::Exact(n) => (n, true),
            ReplicaCount::AtLeast(n) => (n, false),
        };
        groups.push(ReportGroup {
            content_hash: hash_str(&g.content_hash),
            size: g.size,
            verdict: verdict_str(g.verdict),
            only_copy: g.only_copy,
            replicas: ReportReplicas { observed, exact },
            trusted_replicas: g.trusted_replicas,
            untrusted_replicas: g.untrusted_replicas,
            protected_replicas: g.protected_replicas,
            unknown_sources: g.unknown_sources,
            presence,
            copies: copies(&g.copies),
            aliases: rows_sorted(&g.aliases),
        });
    }
    groups.sort_by(|a, b| a.content_hash.cmp(&b.content_hash));

    let mut excluded_groups = Vec::new();
    for g in &floors.excluded_groups {
        let rows: Vec<&RowRef> = g.copies.iter().map(|c| &c.row).chain(&g.aliases).collect();
        if filter.is_active() && !filter.any(&rows)? {
            continue;
        }
        excluded_groups.push(ReportExcludedGroup {
            content_hash: hash_str(&g.content_hash),
            size: g.size,
            reason: match g.reason {
                GroupExclusionReason::EmptyContent => "empty_content",
                GroupExclusionReason::BelowMinSize => "below_min_size",
            },
            copies: copies(&g.copies),
            aliases: rows_sorted(&g.aliases),
        });
    }
    excluded_groups.sort_by(|a, b| a.content_hash.cmp(&b.content_hash));

    let mut unknown = Vec::new();
    for u in &floors.unknown_content {
        if filter.is_active() && !filter.matches(&u.row)? {
            continue;
        }
        unknown.push((
            u.row.clone(),
            ReportUnknownRow {
                row: row(&u.row),
                size: u.size,
                reason: match u.reason {
                    UnknownContentReason::ReadError => "read_error",
                    UnknownContentReason::UnsupportedSparse => "unsupported_sparse",
                    UnknownContentReason::PaxUnparsed => "pax_unparsed",
                    UnknownContentReason::NotHashed => "not_hashed",
                    UnknownContentReason::LegacyNameUncertain => "legacy_name_uncertain",
                },
            },
        ));
    }
    unknown.sort_by(|a, b| a.0.cmp(&b.0));

    let mut excluded_rows = Vec::new();
    for e in &floors.excluded_rows {
        if filter.is_active() && !filter.matches(&e.row)? {
            continue;
        }
        excluded_rows.push((
            e.row.clone(),
            ReportExcludedRow {
                row: row(&e.row),
                reason: match e.reason {
                    ExclusionReason::Shadowed => "shadowed",
                    ExclusionReason::Symlink => "symlink",
                    ExclusionReason::UnmatchedHardlink => "unmatched_hardlink",
                },
            },
        ));
    }
    excluded_rows.sort_by(|a, b| a.0.cmp(&b.0));

    let unknown_content: Vec<ReportUnknownRow> = unknown.into_iter().map(|(_, r)| r).collect();
    let excluded_rows: Vec<ReportExcludedRow> = excluded_rows.into_iter().map(|(_, r)| r).collect();
    let summary = summarize(
        scope.floor.min_copies,
        &report_sources,
        &groups,
        &excluded_groups,
        &unknown_content,
        &excluded_rows,
    );
    let complete = summary.sources_degraded == 0
        && summary.inconclusive == 0
        && summary.unknown_content_rows == 0;

    Ok(CoverageReport {
        version: REPORT_VERSION,
        coverage_state: if complete { "complete" } else { "inconclusive" },
        params: ReportParams {
            input,
            min_copies: scope.floor.min_copies,
            min_size: scope.floor.min_size,
            include_empty: scope.floor.include_empty,
            exts: scope.exts.clone(),
            path_glob: scope.path_glob.clone(),
            kind: scope.kind.clone(),
            archives,
            protected: protected.to_vec(),
        },
        sources: report_sources,
        groups,
        excluded_groups,
        unknown_content,
        excluded_rows,
        summary,
    })
}

/// Totals counted from the emitted rows only.
fn summarize(
    min_copies: usize,
    sources: &[ReportSource],
    groups: &[ReportGroup],
    excluded_groups: &[ReportExcludedGroup],
    unknown_content: &[ReportUnknownRow],
    excluded_rows: &[ReportExcludedRow],
) -> ReportSummary {
    let verdicts = |v: &str| groups.iter().filter(|g| g.verdict == v).count();
    let rows = |r: &str| excluded_rows.iter().filter(|e| e.reason == r).count();
    ReportSummary {
        min_copies,
        sources: sources.len(),
        sources_degraded: sources
            .iter()
            .filter(|s| s.evidence != "complete" || s.status != master::STATUS_OK)
            .count(),
        groups: groups.len(),
        meets_floor: verdicts("meets_floor"),
        below_floor: verdicts("below_floor"),
        inconclusive: verdicts("inconclusive"),
        only_copy: groups.iter().filter(|g| g.only_copy).count(),
        protected_replicas: groups.iter().map(|g| g.protected_replicas).sum(),
        excluded_groups: excluded_groups.len(),
        unknown_content_rows: unknown_content.len(),
        shadowed_rows: rows("shadowed"),
        symlink_rows: rows("symlink"),
        unmatched_hardlink_rows: rows("unmatched_hardlink"),
        hardlink_aliases: groups.iter().map(|g| g.aliases.len()).sum::<usize>()
            + excluded_groups
                .iter()
                .map(|g| g.aliases.len())
                .sum::<usize>(),
    }
}

fn row(r: &RowRef) -> ReportRow {
    ReportRow {
        source_id: r.source_id,
        path: r.display_path(),
        path_bytes: to_hex(&r.path_raw),
        file_id: r.file_id,
    }
}

fn rows_sorted(rows: &[RowRef]) -> Vec<ReportRow> {
    let mut sorted: Vec<&RowRef> = rows.iter().collect();
    sorted.sort();
    sorted.into_iter().map(row).collect()
}

fn copies(copies: &[FloorCopy]) -> Vec<ReportCopy> {
    let mut sorted: Vec<&FloorCopy> = copies.iter().collect();
    // (source id, raw path bytes, file id): RowRef's field order.
    sorted.sort_by(|a, b| a.row.cmp(&b.row));
    sorted
        .into_iter()
        .map(|c| ReportCopy {
            source_id: c.row.source_id,
            label: c.source_label.clone(),
            path: c.row.display_path(),
            path_bytes: to_hex(&c.row.path_raw),
            file_id: c.row.file_id,
            status: status_str(c.status),
            counts_toward_floor: c.counts_toward_floor,
            protected: c.protected,
        })
        .collect()
}

fn presence(source_id: i64, p: Presence) -> ReportPresence {
    let (state, copies, reason, unhashed_rows) = match p {
        Presence::Present { copies } => ("present", Some(copies), None, None),
        Presence::Absent => ("absent", None, None, None),
        Presence::Unknown(why) => {
            let (reason, rows) = match why {
                UnknownReason::SourceUnavailable => ("source_unavailable", None),
                UnknownReason::SourceIncomplete => ("source_incomplete", None),
                UnknownReason::SourceHasNoContentHashes => ("source_has_no_content_hashes", None),
                UnknownReason::SourceUnreachable => ("source_unreachable", None),
                UnknownReason::UnhashedRowsMayMatch { rows } => {
                    ("unhashed_rows_may_match", Some(rows))
                }
            };
            ("unknown", None, Some(reason), rows)
        }
    };
    ReportPresence {
        source_id,
        state,
        copies,
        reason,
        unhashed_rows,
    }
}

fn hash_str(h: &[u8; 32]) -> String {
    format!("b3:{}", to_hex(h))
}

pub fn evidence_str(e: SourceEvidence) -> &'static str {
    match e {
        SourceEvidence::Complete => "complete",
        SourceEvidence::Incomplete => "incomplete",
        SourceEvidence::Unavailable => "unavailable",
        SourceEvidence::NoContentHashes => "no_content_hashes",
        SourceEvidence::Unreachable => "unreachable",
    }
}

/// The master's own status strings.
pub fn status_str(s: SourceStatus) -> &'static str {
    match s {
        SourceStatus::Ok => master::STATUS_OK,
        SourceStatus::Incomplete => master::STATUS_INCOMPLETE,
        SourceStatus::StaleIndex => master::STATUS_STALE_INDEX,
        SourceStatus::DbMissing => master::STATUS_DB_MISSING,
        SourceStatus::ArchiveMissing => master::STATUS_ARCHIVE_MISSING,
    }
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::MeetsFloor => "meets_floor",
        Verdict::BelowFloor => "below_floor",
        Verdict::Inconclusive => "inconclusive",
    }
}

fn load_code_str(c: LoadCode) -> &'static str {
    match c {
        LoadCode::UnknownEntryType => "unknown_entry_type",
        LoadCode::ContradictoryContentMode => "contradictory_content_mode",
        LoadCode::RegistryStatus => "registry_status",
        LoadCode::SourceUnverified => "source_unverified",
    }
}
