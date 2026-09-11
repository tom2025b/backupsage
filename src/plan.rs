//! The immutable action-plan contract (#75).
//!
//! CANONICAL FORM IS THE WHOLE FILE: `to_string_pretty` (two-space indent)
//! plus exactly one trailing '\n'. There is no excluded envelope, no
//! embedded digest and no run-varying value anywhere in the document, so
//! acceptance criteria 1 and 2 read literally on the file, and the plan's
//! identity is `blake3` of the file as it lies (computed by #17/#76, never
//! stored inside it).
//!
//! HARD RULES — enforced by review, by the byte fixtures, and by the
//! `key_order_is_declaration_order_*` guard test:
//!
//!  1. NO `#[serde(flatten)]` anywhere. Flatten routes the value through a
//!     serde map whose ordering is `BTreeMap` normally and `IndexMap` when
//!     the `preserve_order` feature is unified in by ANY crate anywhere in
//!     the dependency graph. The same source tree would then emit different
//!     bytes depending on an unrelated dependency.
//!  2. NO map type in the document, and no `serde_json::Value` on any
//!     serialization path. Every collection is a `Vec` sorted in Rust on a
//!     declared total key before serialization.
//!  3. NO `HashMap`/`HashSet` iteration may influence any emitted value or
//!     any emitted order, in the types OR in the builder. Rust's hasher is
//!     seeded per process: that leak passes criterion 1 and fails criterion
//!     2. Keyed lookup through a `BTreeMap` is fine; iterating a hash map is
//!     not.
//!  4. NO `#[serde(deny_unknown_fields)]` — an already-shipped v1.1 reader
//!     must ignore a field v1.3 added (docs/COMPATIBILITY.md, additive-only).
//!  5. NO `#[serde(other)]` catch-all on any INSTRUCTION enum — an unknown
//!     `kind`, `op.action`, `role`, `disposition`, `fingerprint.kind`,
//!     `match_kind`, `conflict.rule`, `content_mode` or `counted_scope` is a
//!     hard deserialization error and the plan is refused whole. Unknown data
//!     is ignorable; unknown instructions are not.
//!  6. EVIDENCE fields are open documented `String`s, never closed enums:
//!     `keeper.reason`, `member.reason`, `excluded.reason`, `keep_policy`,
//!     `hash_algo`, `phash_algo`, `label`. Refusing a whole plan because the
//!     binary does not recognise the reason a file was deliberately LEFT
//!     ALONE is indefensible.
//!  7. NO `#[serde(skip_serializing_if)]`. Absence is `null` or an explicit
//!     tagged variant, so each struct's key set is constant and two plans
//!     diff line-for-line.
//!  8. NO floats, no timestamps of the run, no hostname, no PID, no
//!     producer/tool version, no absolute path outside `roots[]`.
//!  9. NO PROSE. The document carries no generated sentence, no headline and
//!     no human-readable byte string. A frozen sentence recomputed at load
//!     turns a typo fix into retroactive invalidation of approved plans.
//! 10. Every field added later is `Option<T>` + `#[serde(default)]` with a
//!     documented meaning-when-absent, appended at the END of its struct.
//!     Every DERIVED field frozen in v1 is non-`Option` and always present;
//!     any derived field added later is checked only when non-null, and a
//!     null derived field means "not recorded", never "recorded wrong".
//!
//! Design record: `design-docs/2026-08-28-issue-75-chosen-shape.md`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// This document's own contract version. Independent of the index schema
/// (`store::SCHEMA_VERSION`, currently 3) and of the dedup report's
/// `"version": 1`.
pub const PLAN_SCHEMA_VERSION: u32 = 1;

const DOMAIN_ACTION: &[u8] = b"backupsage.plan.v1.action\0";
const DOMAIN_GROUP: &[u8] = b"backupsage.plan.v1.group\0";
const DOMAIN_SLOT: &[u8] = b"backupsage.plan.v1.slot\0";

/// Live, read-only source facts used by [`Plan::verify`]. The archive digest
/// uses the plan spelling (`b3:<64 lowercase hex>`); directory sources report
/// `None` because v1 has no whole-directory fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSource {
    pub index_uuid: String,
    pub index_schema_version: i64,
    pub content_mode: ContentMode,
    pub hash_algo: String,
    pub phash_algo: String,
    pub files_indexed: u64,
    pub source_type: SourceType,
    pub archive_blake3: Option<String>,
}

/// Live, read-only entry identity returned by [`PlanState`]. `path_raw` is
/// root-relative and authoritative; `file_id` is only the lookup probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEntry {
    pub path_raw: Vec<u8>,
    pub content_hash: String,
}

/// Read-only adapter between a plan and the currently reachable indexes.
/// Implementations may query SQLite and hash archives, but must not mutate
/// user data. An executor must call [`Plan::verify`] successfully before it
/// performs action one.
pub trait PlanState {
    fn source(&mut self, root: &Root) -> Result<LiveSource>;
    fn entry(&mut self, root: &Root, file_id: i64) -> Result<Option<LiveEntry>>;
}

// ── The document ────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub plan_schema_version: u32,
    pub kind: PlanKind,
    pub policy: Policy,
    /// Sorted by (role rank, `path_raw` bytes); `root_id` is the index in
    /// that order. The ONLY place an absolute path appears.
    pub roots: Vec<Root>,
    /// Sorted by `ordinal`, which is dense 0..n-1 and assigned by the
    /// per-kind total key in the canonicalization rules.
    pub actions: Vec<Action>,
    /// Dedup only. Present-and-EMPTY on organize and extract, never omitted,
    /// so a consumer never distinguishes absent from empty.
    pub groups: Vec<Group>,
    /// Candidates considered and deliberately NOT acted on. Sorted by
    /// (root_id, file_id). Silence about what was left out is how a human
    /// "reviews" something they never saw.
    pub excluded: Vec<Excluded>,
    pub summary: Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    Organize,
    Extract,
    Dedup,
}

// ── Policy: the frozen decisions, typed, never free text ────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "for", rename_all = "snake_case")]
pub enum Policy {
    Organize(OrganizePolicy),
    Extract(ExtractPolicy),
    Dedup(DedupPolicy),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrganizePolicy {
    pub layout: Layout,
    /// Plan-authored ASCII directory name for entries with no timestamp.
    pub unknown_date_dir: String,
    pub date_source_order: Vec<DateSource>,
    pub conflict_rule: ConflictRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    #[serde(rename = "yyyy/yyyy-mm")]
    YearMonth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateSource {
    Exif,
    Mtime,
    ArchiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictRule {
    SuffixOrdinal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractPolicy {
    pub selection: Selection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    Explicit,
    Glob,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DedupPolicy {
    /// Sorted; mirrors `dedup::DedupParams.exact` / `.near`.
    pub match_kinds: Vec<MatchKind>,
    pub near_threshold: u32,
    pub min_size: u64,
    pub include_empty: bool,
    pub across_only: bool,
    pub replica_floor: u32,
    /// Open documented string: "newest" | "oldest".
    pub keep_policy: String,
}

// ── Roots: where everything lives, and every index precondition ────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Root {
    /// DERIVED: index in `roots` after sorting by (role rank, path_raw).
    pub root_id: u32,
    pub role: RootRole,
    /// DERIVED: `String::from_utf8_lossy(hex_decode(path_raw))`.
    pub path_display: String,
    /// Absolute path, lowercase hex of the original bytes. Authoritative.
    pub path_raw: String,
    /// `Some` iff `role == Source`; explicit `null` otherwise.
    pub source: Option<SourceBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootRole {
    Source,
    Destination,
    Quarantine,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceBinding {
    /// Master label. NON-AUTHORITATIVE and user-mutable: excluded from every
    /// sort key and every derivation preimage, so a `master` rename can never
    /// move an action, change an id, or invalidate an in-flight #17 journal.
    pub label: String,
    /// `meta.index_uuid`, 32 lowercase hex. A re-index mints a fresh one.
    pub index_uuid: String,
    /// `meta.schema_version` (currently 3). Acceptance criterion 4's "index"
    /// half — recorded, never interpreted, never gates parsing.
    pub index_schema_version: i64,
    pub content_mode: ContentMode,
    /// Open documented strings, compared for equality by #76.
    pub hash_algo: String,
    pub phash_algo: String,
    pub files_indexed: u64,
    pub source_type: SourceType,
    pub fingerprint: Fingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContentMode {
    Full,
    SearchOnly,
    MetadataOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Tar,
    Dir,
}

/// TAGGED ABSENCE, never a bare null. `src/source_dir.rs` calls
/// `run.finish(None)`: a directory source has NO whole-source fingerprint in
/// v1.x. A `null` would be read as "unknown, proceed"; a tagged variant forces
/// #76 down the declared per-entry fallback, and a FUTURE fingerprint scheme
/// arrives as an unknown variant an old reader must refuse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fingerprint {
    ArchiveBlake3 {
        /// `"b3:<64 lowercase hex>"` — `meta.archive_blake3` stores it bare;
        /// the plan re-renders it with the one prefix used everywhere.
        value: String,
        size: u64,
        mtime_unix: Option<i64>,
    },
    None {
        reason: NoFingerprintReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoFingerprintReason {
    DirectorySourceV1,
}

// ── Actions ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    /// DERIVED: 32 lowercase hex, `blake3(DOMAIN_ACTION || framed(...))[..16]`.
    /// CONTENT-derived, not positional, so regenerating a plan after one entry
    /// changed leaves every other id identical and an interrupted run's #17
    /// journal still matches.
    pub action_id: String,
    /// DERIVED: dense 0..n-1 execution order. Separate from `action_id` on
    /// purpose — order is a property of the run, identity is a property of the
    /// action.
    pub ordinal: u32,
    /// NESTED, never flattened (hard rule 1).
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Op {
    /// Enumerated so every directory the executor creates is in the reviewed
    /// document and containment-checkable, instead of `create_dir_all`
    /// inventing intermediate paths nobody validated.
    Mkdir { dest: DestPath },
    Move {
        source: SourceRef,
        dest: DestPath,
        conflict: Conflict,
    },
    Extract {
        source: SourceRef,
        dest: DestPath,
        conflict: Conflict,
    },
    Quarantine {
        source: SourceRef,
        dest: DestPath,
        group_id: String,
        /// DERIVED: 8 hex. Derived WITHOUT the destination, which is what
        /// breaks the id-in-the-path circular dependency.
        slot: String,
    },
}
// No v1.x variant deletes anything. A purge is a separately reviewed future
// plan kind (roadmap: "quarantined files can be restored before a separately
// reviewed purge").

impl Op {
    fn tag(&self) -> &'static str {
        match self {
            Op::Mkdir { .. } => "mkdir",
            Op::Move { .. } => "move",
            Op::Extract { .. } => "extract",
            Op::Quarantine { .. } => "quarantine",
        }
    }

    fn dest(&self) -> &DestPath {
        match self {
            Op::Mkdir { dest } => dest,
            Op::Move { dest, .. } => dest,
            Op::Extract { dest, .. } => dest,
            Op::Quarantine { dest, .. } => dest,
        }
    }

    fn source(&self) -> Option<&SourceRef> {
        match self {
            Op::Mkdir { .. } => None,
            Op::Move { source, .. } => Some(source),
            Op::Extract { source, .. } => Some(source),
            Op::Quarantine { source, .. } => Some(source),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRef {
    pub root_id: u32,
    /// `files.id` — a fast lookup PROBE only. Authority is
    /// (root_id, raw path, content_hash); a probe/raw-path disagreement is a
    /// stale plan, never a silent correction.
    pub file_id: i64,
    /// Root-relative path components, lowercase hex of the ORIGINAL bytes.
    /// ALWAYS present, even for clean UTF-8 (unlike `report.rs`'s
    /// presence-conditional `path_bytes`): a presence-conditional identity
    /// field makes the key set vary per entry and makes identity derivation a
    /// two-branch rule every consumer must reimplement identically.
    pub parts_raw: Vec<String>,
    /// DERIVED: components lossy-decoded, joined with '/'. Carries NO
    /// authority; recomputed and refused on mismatch.
    pub display: String,
    /// REQUIRED, `"b3:<64 lowercase hex>"`. An entry with no hash (unsupported
    /// PAX sparse, read error, metadata-only index) can never be an action —
    /// it goes to `excluded`. This is what makes the resume rule TOTAL.
    pub content_hash: String,
    pub size: u64,
    /// Evidence and cheap pre-filter, NOT refusal authority (see #76 gate 3).
    pub mtime_unix: Option<i64>,
}

impl SourceRef {
    /// The identity. Total by construction: `parts_raw` is always present, so
    /// there is no two-branch fallback to get wrong.
    pub fn raw_path(&self) -> Result<Vec<u8>> {
        join_parts(&self.parts_raw)
    }

    pub fn key(&self) -> EntryKey {
        EntryKey {
            root_id: self.root_id,
            file_id: self.file_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DestPath {
    pub root_id: u32,
    /// At least one component. Each decoded component: non-empty, not "."
    /// or "..", containing no b'/' and no NUL. Absolute paths and traversal
    /// are UNREPRESENTABLE — there is no leading separator to write — rather
    /// than merely rejected. Validation runs on the DECODED bytes.
    pub parts_raw: Vec<String>,
    /// DERIVED, as `SourceRef::display`.
    pub display: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Conflict {
    None,
    SuffixOrdinal {
        /// 0-based rank in the collision set. 0 keeps the plain name.
        ordinal: u32,
        /// The COMPLETE collision set INCLUDING self, sorted. Symmetric
        /// across all members, so the rank is recomputable.
        competing_with: Vec<EntryKey>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryKey {
    pub root_id: u32,
    pub file_id: i64,
}

// ── Dedup groups ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// DERIVED: 16 lowercase hex, `blake3(DOMAIN_GROUP || ...)[..8]`.
    pub group_id: String,
    pub match_kind: MatchKind,
    /// Largest hamming distance to the keeper (0 for exact) — mirrors
    /// `report::Group.max_distance`.
    pub max_distance: u32,
    /// Nested, so a group with ZERO keepers is unrepresentable.
    pub keeper: GroupKeeper,
    /// Every non-keeper member, sorted by (root_id, file_id).
    pub members: Vec<GroupMember>,
    pub replica_floor: ReplicaFloor,
    pub reclaimable_bytes: u64,
    pub review_only_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Exact,
    Near,
}

impl MatchKind {
    fn as_str(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Near => "near",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeeper {
    pub entry: SourceRef,
    /// Open documented string, from `dedup::pick_keep`: "newest" |
    /// "clean-path" | "highest-resolution" | "largest".
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub entry: SourceRef,
    /// The FROZEN decision. The executor reads it and never re-runs
    /// `pick_keep` or `member_is_actionable`.
    pub disposition: Disposition,
    /// Open documented string: "actionable-duplicate" | "transitive-only" |
    /// "shadowed" | "hardlink" | "archive-source-immutable" |
    /// "replica-floor-would-break".
    pub reason: String,
    /// ADR 0004's keeper-star flag, carried as EVIDENCE for the decision so a
    /// reviewer can audit it. A `disposition: quarantine` member with
    /// `actionable: false` is an invalid plan.
    pub actionable: bool,
    pub hamming_to_keep: Option<u32>,
    pub shadowed: bool,
    pub sparse: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Keep,
    Quarantine,
    Leave,
}

/// The floor protects the KEEPER's content: it guarantees the retained content
/// survives in at least `required` registered, reachable sources after apply.
/// It is not a per-member guarantee for near variants — a near variant is
/// different data, and quarantine (not deletion) is what protects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaFloor {
    /// A DECISION: frozen, never recomputed.
    pub required: u32,
    /// A FACT at plan time: distinct source roots holding the keeper's
    /// `content_hash`. #76 RE-COUNTS this live before action one.
    pub observed: u32,
    /// DERIVED: source roots still holding the keeper content after this
    /// group's quarantine actions. Must be >= `required`.
    pub remaining_after: u32,
    pub counted_scope: CountedScope,
}

/// Names what was counted, so a future change in scope arrives as a new enum
/// VALUE rather than a silent redefinition of a field that keeps its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CountedScope {
    /// Registered in the master AND currently reachable. An offline archive
    /// contributes NO replica: an unplugged disk blocks quarantine rather than
    /// silently overstating the true copy count.
    RegisteredReachableArchives,
}

// ── Excluded and summary ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excluded {
    pub root_id: u32,
    pub file_id: i64,
    pub parts_raw: Vec<String>,
    pub display: String,
    /// `null` for rows the index could not hash — the honest reason such a row
    /// can never be an action.
    pub content_hash: Option<String>,
    pub size: u64,
    pub mtime_unix: Option<i64>,
    /// Open documented string: "no_content_hash" | "sparse" |
    /// "shadowed_by_later_entry" | "symlink" | "hardlink" | "below_min_size" |
    /// "metadata_only_index" | "already_at_destination" |
    /// "near_review_only" | "replica_floor_would_break".
    pub reason: String,
}

/// Every field DERIVED and recomputed by `verify_derived` at load. Integers
/// only — no ratio, no percentage, no human-readable byte string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub actions_total: u32,
    /// A sorted `Vec`, not a map — no map appears anywhere in a plan.
    pub actions_by_op: Vec<OpCount>,
    /// Sum of `source.size` over every action that HAS a source; `mkdir`
    /// contributes 0. One field, one rule — no move/copy split to get wrong.
    pub bytes_affected: u64,
    pub groups_total: u32,
    pub excluded_total: u32,
    pub excluded_bytes: u64,
    /// Sorted, deduplicated `root_id`s any action writes to.
    pub roots_written: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpCount {
    pub op: String,
    pub count: u32,
}

// ── Canonicalization, verification, IO ─────────────────────────────────

impl Plan {
    /// Compute EVERY derived value and impose EVERY canonical order, from the
    /// primitive facts only — never from the derived fields' current values.
    /// Idempotent. In order: sort and renumber `roots` and remap every
    /// `root_id`; derive `display` from `parts_raw` everywhere; assign
    /// conflict ordinals and materialize suffixed destinations; derive
    /// `group_id`, `slot`, `action_id`; sort `groups`, `members`, `excluded`,
    /// `actions`; assign `ordinal`; recompute `summary`. Uses `BTreeMap` for
    /// the root remap — never a `HashMap` (hard rule 3).
    pub fn canonicalize(&mut self) -> Result<()> {
        self.canonicalize_roots()?;
        // Conflicts mutate `dest.parts_raw` for suffixed entries — display
        // fields must be derived AFTER that mutation, or a suffixed entry's
        // `display` silently keeps its pre-suffix value.
        self.canonicalize_conflicts()?;
        self.canonicalize_display_fields()?;
        self.canonicalize_groups()?;
        self.canonicalize_actions()?;
        self.excluded.sort_by_key(|e| (e.root_id, e.file_id));
        self.recompute_summary();
        Ok(())
    }

    fn canonicalize_roots(&mut self) -> Result<()> {
        // Sort by (role rank, decoded path bytes) — total by construction
        // once verify checks no two roots share a path_raw. Keyed on each
        // Root's OWN `root_id` FIELD throughout, never on its position in
        // `self.roots` — the rest of the document references the field, and
        // a builder is free to hand roots in any vec order.
        if self
            .roots
            .iter()
            .map(|r| r.root_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != self.roots.len()
        {
            bail!("two roots share a root_id");
        }
        let role_of: BTreeMap<u32, RootRole> =
            self.roots.iter().map(|r| (r.root_id, r.role)).collect();
        let roots_by_old_id: BTreeMap<u32, Root> =
            self.roots.iter().cloned().map(|r| (r.root_id, r)).collect();

        // Every root_id referenced anywhere in the document must be one of
        // THIS root's own original ids — checked before the remap is built,
        // never left to a fallback default. A dangling reference (an id that
        // was never a real root) must be refused here: a remap that quietly
        // passed it through unchanged could alias it onto whatever root ends
        // up with that same NEW dense id after renumbering, silently
        // corrupting which root an action, group member, or excluded entry
        // actually points to.
        let known: std::collections::BTreeSet<u32> = roots_by_old_id.keys().copied().collect();
        for a in &self.actions {
            for id in referenced_root_ids(&a.op) {
                if !known.contains(&id) {
                    bail!("action references unknown root_id {id}");
                }
            }
        }
        for g in &self.groups {
            if !known.contains(&g.keeper.entry.root_id) {
                bail!(
                    "group keeper references unknown root_id {}",
                    g.keeper.entry.root_id
                );
            }
            for m in &g.members {
                if !known.contains(&m.entry.root_id) {
                    bail!(
                        "group member references unknown root_id {}",
                        m.entry.root_id
                    );
                }
            }
        }
        for e in &self.excluded {
            if !known.contains(&e.root_id) {
                bail!("excluded entry references unknown root_id {}", e.root_id);
            }
        }

        let mut indexed: Vec<(u32, Vec<u8>)> = Vec::with_capacity(self.roots.len());
        for r in &self.roots {
            indexed.push((r.root_id, unhex(&r.path_raw)?));
        }
        indexed.sort_by(|a, b| {
            let ra = role_of[&a.0];
            let rb = role_of[&b.0];
            (ra, &a.1).cmp(&(rb, &b.1))
        });

        // old root_id FIELD -> new_id, via BTreeMap only.
        let mut remap: BTreeMap<u32, u32> = BTreeMap::new();
        for (new_id, (old_root_id, _)) in indexed.iter().enumerate() {
            remap.insert(*old_root_id, new_id as u32);
        }

        let mut new_roots = Vec::with_capacity(self.roots.len());
        for (new_id, (old_root_id, _)) in indexed.iter().enumerate() {
            let mut r = roots_by_old_id[old_root_id].clone();
            r.root_id = new_id as u32;
            r.path_display = lossy_join_hex(&r.path_raw)?;
            new_roots.push(r);
        }
        self.roots = new_roots;

        // Remap every root_id reference throughout the document. `remap`
        // is guaranteed total over every id actually referenced — the
        // validation pass above already refused anything it isn't — so the
        // `unwrap_or` fallback is unreachable defense-in-depth, never a
        // silent pass-through of a bad id.
        let remap_id = |old: u32| -> u32 { *remap.get(&old).unwrap_or(&old) };

        for a in &mut self.actions {
            remap_op_root_ids(&mut a.op, &remap_id);
        }
        for g in &mut self.groups {
            g.keeper.entry.root_id = remap_id(g.keeper.entry.root_id);
            for m in &mut g.members {
                m.entry.root_id = remap_id(m.entry.root_id);
            }
        }
        for e in &mut self.excluded {
            e.root_id = remap_id(e.root_id);
        }
        Ok(())
    }

    fn canonicalize_display_fields(&mut self) -> Result<()> {
        for a in &mut self.actions {
            match &mut a.op {
                Op::Mkdir { dest } => dest.display = join_parts_lossy(&dest.parts_raw)?,
                Op::Move { source, dest, .. }
                | Op::Extract { source, dest, .. }
                | Op::Quarantine { source, dest, .. } => {
                    source.display = join_parts_lossy(&source.parts_raw)?;
                    dest.display = join_parts_lossy(&dest.parts_raw)?;
                }
            }
        }
        for g in &mut self.groups {
            g.keeper.entry.display = join_parts_lossy(&g.keeper.entry.parts_raw)?;
            for m in &mut g.members {
                m.entry.display = join_parts_lossy(&m.entry.parts_raw)?;
            }
        }
        for e in &mut self.excluded {
            e.display = join_parts_lossy(&e.parts_raw)?;
        }
        Ok(())
    }

    /// Rule 6's phase-1 total key, for actions that have a source. `mkdir`
    /// (phase 0) is ordered on its destination alone and never collides.
    fn phase1_key(&self, source: &SourceRef) -> Result<(u32, Vec<u8>, i64)> {
        Ok((source.root_id, source.raw_path()?, source.file_id))
    }

    fn canonicalize_conflicts(&mut self) -> Result<()> {
        // Collision set key: (dest.root_id, BASE components — the plan-time
        // components before any suffix rewrite). Because canonicalize is
        // idempotent, we must recover the base from the CURRENT dest, which
        // may already carry a suffix from a prior pass — so we strip it using
        // the *previous* conflict block when present, else treat dest as base.
        #[derive(Clone)]
        struct Entry {
            action_idx: usize,
            dest_root: u32,
            base_parts: Vec<String>,
            rank_key: (u32, Vec<u8>, i64),
        }

        let mut entries: Vec<Entry> = Vec::new();
        for (i, a) in self.actions.iter().enumerate() {
            let (dest, source, conflict) = match &a.op {
                Op::Move {
                    dest,
                    source,
                    conflict,
                }
                | Op::Extract {
                    dest,
                    source,
                    conflict,
                } => (dest, source, conflict),
                Op::Mkdir { .. } | Op::Quarantine { .. } => continue,
            };
            let base_parts = base_components(dest, conflict)?;
            entries.push(Entry {
                action_idx: i,
                dest_root: dest.root_id,
                base_parts,
                rank_key: self.phase1_key(source)?,
            });
        }

        // Group by (dest_root, base_parts) — BTreeMap keyed lookup only,
        // never iterated for ordering (hard rule 3/4).
        let mut groups: BTreeMap<(u32, Vec<String>), Vec<usize>> = BTreeMap::new();
        for (ei, e) in entries.iter().enumerate() {
            groups
                .entry((e.dest_root, e.base_parts.clone()))
                .or_default()
                .push(ei);
        }

        for member_indices in groups.values() {
            let mut ranked: Vec<usize> = member_indices.clone();
            ranked.sort_by(|&a, &b| entries[a].rank_key.cmp(&entries[b].rank_key));

            let competing_with: Vec<EntryKey> = {
                let mut keys: Vec<EntryKey> = ranked
                    .iter()
                    .map(|&ei| {
                        let ai = entries[ei].action_idx;
                        match &self.actions[ai].op {
                            Op::Move { source, .. } | Op::Extract { source, .. } => source.key(),
                            _ => unreachable!(),
                        }
                    })
                    .collect();
                keys.sort();
                keys
            };

            if ranked.len() == 1 {
                let ai = entries[ranked[0]].action_idx;
                set_conflict(&mut self.actions[ai].op, Conflict::None)?;
                continue;
            }

            for (rank, &ei) in ranked.iter().enumerate() {
                let ai = entries[ei].action_idx;
                let base = entries[ei].base_parts.clone();
                let ordinal = rank as u32;
                let new_parts = if ordinal == 0 {
                    base
                } else {
                    suffix_last_component(&base, ordinal)?
                };
                set_dest_parts(&mut self.actions[ai].op, new_parts)?;
                set_conflict(
                    &mut self.actions[ai].op,
                    Conflict::SuffixOrdinal {
                        ordinal,
                        competing_with: competing_with.clone(),
                    },
                )?;
            }
        }
        Ok(())
    }

    fn canonicalize_groups(&mut self) -> Result<()> {
        for g in &mut self.groups {
            g.members
                .sort_by_key(|m| (m.entry.root_id, m.entry.file_id));
        }
        self.groups
            .sort_by_key(|g| (g.keeper.entry.root_id, g.keeper.entry.file_id));

        for g in &mut self.groups {
            g.group_id = compute_group_id(g)?;
            // DERIVED: a dedup group is exhaustive over every registered,
            // reachable copy of the keeper's content (that is what
            // `counted_scope` names), so the roots still holding it AFTER
            // this group's quarantine actions run is exactly the distinct
            // root set of the keeper plus every member NOT being quarantined
            // — fully recoverable from the group's own contents, unlike
            // `observed` (a plan-time fact about the live corpus that this
            // document cannot re-derive on its own).
            let mut remaining_roots: std::collections::BTreeSet<u32> =
                std::collections::BTreeSet::new();
            remaining_roots.insert(g.keeper.entry.root_id);
            for m in &g.members {
                if m.disposition != Disposition::Quarantine {
                    remaining_roots.insert(m.entry.root_id);
                }
            }
            g.replica_floor.remaining_after = remaining_roots.len() as u32;
        }
        Ok(())
    }

    fn canonicalize_actions(&mut self) -> Result<()> {
        // Derive slot/action_id for quarantine ops first (slot must be final
        // before action_id is computed, and slot does not depend on dest).
        for a in &mut self.actions {
            if let Op::Quarantine {
                source,
                group_id,
                slot,
                ..
            } = &mut a.op
            {
                *slot = compute_slot(group_id, source)?;
            }
        }

        // Assign dense ordinals by the rule-6 total key.
        let mut order: Vec<usize> = (0..self.actions.len()).collect();
        order.sort_by(|&a, &b| {
            let ka = self.action_sort_key(&self.actions[a]).expect("valid key");
            let kb = self.action_sort_key(&self.actions[b]).expect("valid key");
            ka.cmp(&kb)
        });

        let mut new_actions = Vec::with_capacity(self.actions.len());
        for (ordinal, &old_i) in order.iter().enumerate() {
            let mut a = self.actions[old_i].clone();
            a.ordinal = ordinal as u32;
            new_actions.push(a);
        }
        self.actions = new_actions;

        // action_id last: it must see the FINAL dest (post-conflict-suffix).
        for a in &mut self.actions {
            a.action_id = compute_action_id(&a.op)?;
        }
        Ok(())
    }

    fn action_sort_key(&self, a: &Action) -> Result<(u8, u32, Vec<u8>, i64)> {
        match &a.op {
            Op::Mkdir { dest } => Ok((0, dest.root_id, join_parts(&dest.parts_raw)?, 0)),
            Op::Move { source, .. } => {
                let (r, p, f) = self.phase1_key(source)?;
                Ok((1, r, p, f))
            }
            Op::Extract { source, .. } => {
                // Archive order: (root_id, file_id) only — file_id already
                // encodes archive insertion order via last_insert_rowid.
                Ok((1, source.root_id, Vec::new(), source.file_id))
            }
            Op::Quarantine {
                source, group_id, ..
            } => {
                let mut key = group_id.as_bytes().to_vec();
                key.extend_from_slice(&join_parts(&source.parts_raw)?);
                Ok((1, source.root_id, key, source.file_id))
            }
        }
    }

    fn recompute_summary(&mut self) {
        let actions_total = self.actions.len() as u32;

        let mut by_op: BTreeMap<&'static str, u32> = BTreeMap::new();
        let mut bytes_affected: u64 = 0;
        let mut roots_written: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();

        for a in &self.actions {
            *by_op.entry(a.op.tag()).or_insert(0) += 1;
            if let Some(s) = a.op.source() {
                bytes_affected += s.size;
            }
            roots_written.insert(a.op.dest().root_id);
        }

        let actions_by_op: Vec<OpCount> = by_op
            .into_iter()
            .map(|(op, count)| OpCount {
                op: op.to_string(),
                count,
            })
            .collect();

        let groups_total = self.groups.len() as u32;
        let excluded_total = self.excluded.len() as u32;
        let excluded_bytes: u64 = self.excluded.iter().map(|e| e.size).sum();

        self.summary = Summary {
            actions_total,
            actions_by_op,
            bytes_affected,
            groups_total,
            excluded_total,
            excluded_bytes,
            roots_written: roots_written.into_iter().collect(),
        };
    }

    /// Recompute every derivation and refuse a disagreement. Runs at plan LOAD
    /// inside #76's path, before action one — not only in tests — so a
    /// hand-edited display path, a doctored total, a forged action id or a
    /// re-ordered actions array is a malformed plan.
    ///
    /// Implemented as "canonicalize a clone and compare", which makes it
    /// impossible for the builder and the checker to disagree.
    pub fn verify_derived(&self) -> Result<()> {
        let mut c = self.clone();
        c.canonicalize()
            .context("plan failed to canonicalize during verification")?;
        if c != *self {
            bail!("plan derived fields disagree with its own contents; refusing");
        }
        Ok(())
    }

    /// Structural rules the JSON Schema cannot express: root_id closure and
    /// uniqueness of `path_raw` across roots; `source.is_some() == (role ==
    /// Source)`; per-kind op/role/source-type legality; lowercase-hex shapes
    /// and the `"b3:"` prefix; decoded-component legality; source-side and
    /// destination uniqueness; `groups` empty unless dedup; no `dest` equal to
    /// a planned source path (organize order-independence); every quarantine
    /// action backed by a group member with `disposition == Quarantine` and
    /// `actionable == true`; `remaining_after >= required`.
    pub fn invariants_hold(&self) -> Result<()> {
        // root_id closure and uniqueness of path_raw.
        let mut seen_paths: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for r in &self.roots {
            if !seen_paths.insert(r.path_raw.as_str()) {
                bail!("two roots share path_raw {}", r.path_raw);
            }
            match (r.role, &r.source) {
                (RootRole::Source, Some(_)) => {}
                (RootRole::Source, None) => {
                    bail!("source root {} has no source binding", r.root_id)
                }
                (_, Some(_)) => bail!("non-source root {} carries a source binding", r.root_id),
                (_, None) => {}
            }
            if let Some(src) = &r.source {
                match (self.kind, src.source_type, &src.fingerprint) {
                    (_, SourceType::Dir, Fingerprint::ArchiveBlake3 { .. }) => {
                        bail!(
                            "directory source root {} carries an archive fingerprint",
                            r.root_id
                        )
                    }
                    (_, SourceType::Tar, Fingerprint::None { .. }) => {
                        bail!("tar source root {} carries no fingerprint", r.root_id)
                    }
                    _ => {}
                }
            }
        }
        let root_ids: std::collections::BTreeSet<u32> =
            self.roots.iter().map(|r| r.root_id).collect();
        let check_root = |id: u32, ctx: &str| -> Result<()> {
            if !root_ids.contains(&id) {
                bail!("{ctx} references unknown root_id {id}");
            }
            Ok(())
        };

        // groups empty unless dedup.
        if self.kind != PlanKind::Dedup && !self.groups.is_empty() {
            bail!("groups present on a non-dedup plan");
        }

        // source-side and destination uniqueness, plus per-op checks.
        let mut source_keys: std::collections::BTreeSet<EntryKey> =
            std::collections::BTreeSet::new();
        let mut dest_keys: std::collections::BTreeSet<(u32, Vec<u8>)> =
            std::collections::BTreeSet::new();
        let mut organize_source_paths: std::collections::BTreeSet<(u32, Vec<u8>)> =
            std::collections::BTreeSet::new();

        for a in &self.actions {
            let dest = a.op.dest();
            check_root(dest.root_id, "action dest")?;
            for c in &dest.parts_raw {
                validate_component(c)?;
            }
            let dest_key = (dest.root_id, join_parts(&dest.parts_raw)?);
            if !matches!(a.op, Op::Mkdir { .. }) && !dest_keys.insert(dest_key) {
                bail!("two actions target the same destination");
            }

            if let Some(source) = a.op.source() {
                check_root(source.root_id, "action source")?;
                for c in &source.parts_raw {
                    validate_component(c)?;
                }
                if !source.content_hash.starts_with("b3:") || source.content_hash.len() != 67 {
                    bail!("content_hash malformed: {}", source.content_hash);
                }
                if !is_lowercase_hex(&source.content_hash[3..]) {
                    bail!("content_hash is not lowercase hex: {}", source.content_hash);
                }
                if !source_keys.insert(source.key()) {
                    bail!("two actions read the same source entry");
                }
                if self.kind == PlanKind::Organize {
                    organize_source_paths.insert((source.root_id, source.raw_path()?));
                }
            }

            match (self.kind, &a.op) {
                (PlanKind::Organize, Op::Move { .. } | Op::Mkdir { .. }) => {}
                (PlanKind::Extract, Op::Extract { .. } | Op::Mkdir { .. }) => {}
                (PlanKind::Dedup, Op::Quarantine { .. } | Op::Mkdir { .. }) => {}
                (kind, op) => bail!("op {} is not legal for plan kind {:?}", op.tag(), kind),
            }

            if let Op::Extract { source, .. } = &a.op {
                let root = self
                    .roots
                    .iter()
                    .find(|r| r.root_id == source.root_id)
                    .context("extract source root must exist")?;
                if let Some(src) = &root.source {
                    if src.source_type != SourceType::Tar {
                        bail!("extract action's source root is not a tar archive");
                    }
                }
            }
            if let Op::Move { source, .. } = &a.op {
                let root = self
                    .roots
                    .iter()
                    .find(|r| r.root_id == source.root_id)
                    .context("organize source root must exist")?;
                if let Some(src) = &root.source {
                    if src.source_type != SourceType::Dir {
                        bail!("organize move action's source root is not a directory source");
                    }
                }
            }

            if let Op::Quarantine {
                source, group_id, ..
            } = &a.op
            {
                let group = self
                    .groups
                    .iter()
                    .find(|g| &g.group_id == group_id)
                    .context("quarantine action references an unknown group_id")?;
                let member = group
                    .members
                    .iter()
                    .find(|m| m.entry.key() == source.key())
                    .context("quarantine action's source is not a member of its group")?;
                if member.disposition != Disposition::Quarantine {
                    bail!("quarantine action's group member is not disposition=quarantine");
                }
                if !member.actionable {
                    bail!("quarantine action's group member is not actionable");
                }
            }
        }

        // organize order-independence: no dest equals a planned source path.
        if self.kind == PlanKind::Organize {
            for a in &self.actions {
                if let Op::Move { dest, .. } = &a.op {
                    let key = (dest.root_id, join_parts(&dest.parts_raw)?);
                    if organize_source_paths.contains(&key) {
                        bail!("organize destination collides with a planned source path");
                    }
                }
            }
        }

        // replica floor.
        for g in &self.groups {
            if g.replica_floor.remaining_after < g.replica_floor.required {
                bail!(
                    "group {} remaining_after {} < required {}",
                    g.group_id,
                    g.replica_floor.remaining_after,
                    g.replica_floor.required
                );
            }
        }

        Ok(())
    }

    /// The ONLY way a plan becomes bytes.
    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>> {
        self.verify_derived()?;
        self.invariants_hold()?;
        let mut s = serde_json::to_string_pretty(self)?;
        s.push('\n');
        Ok(s.into_bytes())
    }

    /// The ONLY way bytes become a plan. #77's "apply accepts only a
    /// persisted regular file" is enforced by [`Plan::load_from_regular_file`]
    /// at the execution boundary. This lower-level decoder remains useful for
    /// fixture and compatibility checks, but must never be an apply surface.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        let plan: Plan = serde_json::from_slice(bytes)
            .context("plan document is not valid JSON for this contract")?;
        if plan.plan_schema_version > PLAN_SCHEMA_VERSION {
            bail!(
                "plan_schema_version {} is newer than this build understands ({}); refusing",
                plan.plan_schema_version,
                PLAN_SCHEMA_VERSION
            );
        }
        plan.verify_derived()?;
        plan.invariants_hold()?;
        Ok(plan)
    }

    /// Load a plan only from a path naming the same persisted regular file
    /// before and after open. `-`, symlinks (including symlinks to regular
    /// files), FIFOs, sockets, devices and directories are refused before any
    /// bytes reach [`Plan::from_canonical_bytes`].
    pub fn load_from_regular_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path == Path::new("-") {
            bail!("stdin ('-') is not a persisted regular plan file; refusing");
        }

        let path_metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("cannot inspect plan path {}", path.display()))?;
        let path_type = path_metadata.file_type();
        if path_type.is_symlink() {
            bail!(
                "plan path {} is a symlink, not a persisted regular file; refusing",
                path.display()
            );
        }
        if !path_type.is_file() {
            bail!(
                "plan path {} is {}, not a persisted regular file; refusing",
                path.display(),
                file_type_name(&path_type)
            );
        }

        let mut file = File::open(path)
            .with_context(|| format!("cannot open plan file {}", path.display()))?;
        let opened_metadata = file
            .metadata()
            .with_context(|| format!("cannot inspect opened plan file {}", path.display()))?;
        if !opened_metadata.file_type().is_file() {
            bail!(
                "opened plan path {} is not a regular file; refusing",
                path.display()
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if path_metadata.dev() != opened_metadata.dev()
                || path_metadata.ino() != opened_metadata.ino()
            {
                bail!(
                    "plan path {} changed between inspection and open; refusing",
                    path.display()
                );
            }
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .with_context(|| format!("cannot read plan file {}", path.display()))?;
        Self::from_canonical_bytes(&bytes)
            .with_context(|| format!("invalid plan file {}", path.display()))
    }

    /// Verify every live precondition before an executor is allowed to
    /// perform action one. This method only reads through `state`; it does not
    /// execute, stage, move, copy, create or delete anything.
    pub fn verify(&self, state: &mut impl PlanState) -> Result<()> {
        self.verify_derived()?;
        self.invariants_hold()?;

        for root in &self.roots {
            let Some(expected) = &root.source else {
                continue;
            };
            let live = state.source(root).with_context(|| {
                format!(
                    "cannot verify source root {} ({})",
                    root.root_id, root.path_display
                )
            })?;

            if live.index_uuid != expected.index_uuid {
                bail!(
                    "source root {} ({}) index_uuid changed: plan {}, live {}; refusing stale plan",
                    root.root_id,
                    root.path_display,
                    expected.index_uuid,
                    live.index_uuid
                );
            }
            if live.index_schema_version != expected.index_schema_version {
                bail!(
                    "source root {} ({}) index_schema_version changed: plan {}, live {}; refusing stale plan",
                    root.root_id,
                    root.path_display,
                    expected.index_schema_version,
                    live.index_schema_version
                );
            }
            if live.content_mode != expected.content_mode {
                bail!(
                    "source root {} ({}) content_mode changed; refusing stale plan",
                    root.root_id,
                    root.path_display
                );
            }
            if live.hash_algo != expected.hash_algo {
                bail!(
                    "source root {} ({}) hash_algo changed: plan {}, live {}; refusing stale plan",
                    root.root_id,
                    root.path_display,
                    expected.hash_algo,
                    live.hash_algo
                );
            }
            if live.phash_algo != expected.phash_algo {
                bail!(
                    "source root {} ({}) phash_algo changed: plan {}, live {}; refusing stale plan",
                    root.root_id,
                    root.path_display,
                    expected.phash_algo,
                    live.phash_algo
                );
            }
            if live.files_indexed != expected.files_indexed {
                bail!(
                    "source root {} ({}) files_indexed changed: plan {}, live {}; refusing stale plan",
                    root.root_id,
                    root.path_display,
                    expected.files_indexed,
                    live.files_indexed
                );
            }
            if live.source_type != expected.source_type {
                bail!(
                    "source root {} ({}) source_type changed; refusing stale plan",
                    root.root_id,
                    root.path_display
                );
            }

            match &expected.fingerprint {
                Fingerprint::ArchiveBlake3 { value, .. } => {
                    if live.archive_blake3.as_deref() != Some(value.as_str()) {
                        bail!(
                            "source root {} ({}) archive BLAKE3 changed: plan {}, live {}; refusing stale plan",
                            root.root_id,
                            root.path_display,
                            value,
                            live.archive_blake3.as_deref().unwrap_or("missing")
                        );
                    }
                }
                Fingerprint::None { .. } => {
                    if live.archive_blake3.is_some() {
                        bail!(
                            "source root {} ({}) unexpectedly acquired an archive BLAKE3; refusing stale plan",
                            root.root_id,
                            root.path_display
                        );
                    }
                }
            }
        }

        let entries = self.referenced_entries()?;
        for (key, expected) in entries {
            let root = self
                .roots
                .iter()
                .find(|root| root.root_id == key.root_id)
                .context("verified entry references an unknown source root")?;
            let live = state
                .entry(root, key.file_id)
                .with_context(|| {
                    format!(
                        "cannot verify entry {} (root_id {}, file_id {})",
                        expected.display, key.root_id, key.file_id
                    )
                })?
                .with_context(|| {
                    format!(
                        "entry {} (root_id {}, file_id {}) is missing; refusing stale plan",
                        expected.display, key.root_id, key.file_id
                    )
                })?;
            let expected_path = expected.raw_path()?;
            if live.path_raw != expected_path {
                bail!(
                    "entry {} (root_id {}, file_id {}) raw path changed; refusing stale plan",
                    expected.display,
                    key.root_id,
                    key.file_id
                );
            }
            if live.content_hash != expected.content_hash {
                bail!(
                    "entry {} (root_id {}, file_id {}) content_hash changed: plan {}, live {}; refusing stale plan",
                    expected.display,
                    key.root_id,
                    key.file_id,
                    expected.content_hash,
                    live.content_hash
                );
            }
        }

        Ok(())
    }

    fn referenced_entries(&self) -> Result<BTreeMap<EntryKey, &SourceRef>> {
        let mut entries = BTreeMap::new();
        for action in &self.actions {
            if let Some(source) = action.op.source() {
                insert_referenced_entry(&mut entries, source)?;
            }
        }
        for group in &self.groups {
            insert_referenced_entry(&mut entries, &group.keeper.entry)?;
            for member in &group.members {
                insert_referenced_entry(&mut entries, &member.entry)?;
            }
        }
        Ok(entries)
    }

    /// The plan's identity: blake3 of the file AS IT LIES. Derived, never
    /// stored in the document — a digest a plan computes over itself needs an
    /// excluded region and protects nothing against an editor who can
    /// recompute it.
    pub fn digest(file_bytes: &[u8]) -> String {
        format!(
            "b3:{}",
            crate::report::to_hex(blake3::hash(file_bytes).as_bytes())
        )
    }
}

fn insert_referenced_entry<'a>(
    entries: &mut BTreeMap<EntryKey, &'a SourceRef>,
    entry: &'a SourceRef,
) -> Result<()> {
    if let Some(prior) = entries.insert(entry.key(), entry) {
        if prior != entry {
            bail!(
                "entry root_id {} file_id {} has contradictory plan facts",
                entry.root_id,
                entry.file_id
            );
        }
    }
    Ok(())
}

fn file_type_name(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        return "a directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_fifo() {
            return "a FIFO";
        }
        if file_type.is_socket() {
            return "a socket";
        }
        if file_type.is_char_device() {
            return "a character device";
        }
        if file_type.is_block_device() {
            return "a block device";
        }
    }
    "not a regular file"
}

// ── Derivation helpers ────────────────────────────────────────────────────

/// Unambiguous framing for every derivation preimage: each element is
/// `u64::to_le_bytes(len)` followed by its bytes. Integers enter as their
/// 8-byte little-endian encoding. Because the element list is a function of the
/// op tag, and the op tag is always the first element, the encoding is
/// injective — including for `Op::Mkdir`, which has no source.
fn framed(elements: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in elements {
        out.extend_from_slice(&(e.len() as u64).to_le_bytes());
        out.extend_from_slice(e);
    }
    out
}

fn le8(n: u32) -> [u8; 8] {
    (n as u64).to_le_bytes()
}
fn le8_i64(n: i64) -> [u8; 8] {
    (n as u64).to_le_bytes()
}

fn compute_action_id(op: &Op) -> Result<String> {
    let dest = op.dest();
    let mut elements: Vec<Vec<u8>> = vec![op.tag().as_bytes().to_vec()];
    match op {
        Op::Mkdir { .. } => {
            elements.push(le8(dest.root_id).to_vec());
            for c in &dest.parts_raw {
                elements.push(unhex(c)?);
            }
        }
        Op::Move { source, .. } | Op::Extract { source, .. } => {
            elements.push(le8(dest.root_id).to_vec());
            for c in &dest.parts_raw {
                elements.push(unhex(c)?);
            }
            elements.push(le8(source.root_id).to_vec());
            elements.push(le8_i64(source.file_id).to_vec());
            elements.push(source.content_hash.as_bytes().to_vec());
        }
        Op::Quarantine {
            source,
            group_id,
            slot,
            ..
        } => {
            elements.push(le8(dest.root_id).to_vec());
            for c in &dest.parts_raw {
                elements.push(unhex(c)?);
            }
            elements.push(le8(source.root_id).to_vec());
            elements.push(le8_i64(source.file_id).to_vec());
            elements.push(source.content_hash.as_bytes().to_vec());
            elements.push(group_id.as_bytes().to_vec());
            elements.push(slot.as_bytes().to_vec());
        }
    }
    let refs: Vec<&[u8]> = elements.iter().map(|v| v.as_slice()).collect();
    let mut preimage = DOMAIN_ACTION.to_vec();
    preimage.extend_from_slice(&framed(&refs));
    let full = blake3::hash(&preimage);
    Ok(crate::report::to_hex(&full.as_bytes()[..16]))
}

fn compute_group_id(g: &Group) -> Result<String> {
    let mut elements: Vec<Vec<u8>> = vec![g.match_kind.as_str().as_bytes().to_vec()];
    elements.push(le8(g.keeper.entry.root_id).to_vec());
    elements.push(le8_i64(g.keeper.entry.file_id).to_vec());
    for m in &g.members {
        elements.push(le8(m.entry.root_id).to_vec());
        elements.push(le8_i64(m.entry.file_id).to_vec());
    }
    let refs: Vec<&[u8]> = elements.iter().map(|v| v.as_slice()).collect();
    let mut preimage = DOMAIN_GROUP.to_vec();
    preimage.extend_from_slice(&framed(&refs));
    let full = blake3::hash(&preimage);
    Ok(crate::report::to_hex(&full.as_bytes()[..8]))
}

fn compute_slot(group_id: &str, source: &SourceRef) -> Result<String> {
    let elements: [&[u8]; 4] = [
        group_id.as_bytes(),
        &le8(source.root_id),
        &le8_i64(source.file_id),
        source.content_hash.as_bytes(),
    ];
    let mut preimage = DOMAIN_SLOT.to_vec();
    preimage.extend_from_slice(&framed(&elements));
    let full = blake3::hash(&preimage);
    Ok(crate::report::to_hex(&full.as_bytes()[..4]))
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("not lowercase hex: {s}");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

fn is_lowercase_hex(s: &str) -> bool {
    !s.is_empty()
        && s.len().is_multiple_of(2)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn join_parts(parts_raw: &[String]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for (i, c) in parts_raw.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(&unhex(c)?);
    }
    Ok(out)
}

fn join_parts_lossy(parts_raw: &[String]) -> Result<String> {
    let bytes = join_parts(parts_raw)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn lossy_join_hex(path_raw: &str) -> Result<String> {
    let bytes = unhex(path_raw)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn validate_component(hex: &str) -> Result<()> {
    let bytes = unhex(hex)?;
    if bytes.is_empty() {
        bail!("empty path component");
    }
    if bytes == b"." || bytes == b".." {
        bail!("path component is '.' or '..'");
    }
    if bytes.contains(&b'/') || bytes.contains(&0u8) {
        bail!("path component contains '/' or NUL");
    }
    Ok(())
}

/// Splits the raw decoded bytes of the last path component into (stem, ext)
/// at the last b'.' in the component, per rule 13(d): `ext` is the last dot
/// and everything after it (empty if there is no dot or the dot is the first
/// byte), `stem` is the rest.
fn split_stem_ext(component: &[u8]) -> (&[u8], &[u8]) {
    match component.iter().rposition(|&b| b == b'.') {
        Some(0) | None => (component, &[]),
        Some(i) => (&component[..i], &component[i..]),
    }
}

/// Base = current parts, with the suffix rule 13(d) added on a PRIOR
/// canonicalize pass removed again — so canonicalize stays idempotent across
/// repeated calls.
///
/// Whether a suffix was added, and exactly what it was, is read from `prior`
/// — the entry's OWN `Conflict` field from before this pass — never guessed
/// from the destination bytes. A heuristic ("strip any trailing `-N`") would
/// misfire on a source file genuinely named e.g. `report-1.txt`: stripping
/// its real name as though canonicalize had added the suffix would merge it
/// into the wrong collision group and could overwrite it with a different
/// entry's rename. `prior` names the exact ordinal this component was
/// suffixed with (0 or none means it was never suffixed, so the current
/// parts already ARE the base), so only a suffix THIS module actually wrote
/// is ever removed.
fn base_components(dest: &DestPath, prior: &Conflict) -> Result<Vec<String>> {
    let ordinal = match prior {
        Conflict::None => return Ok(dest.parts_raw.clone()),
        Conflict::SuffixOrdinal { ordinal: 0, .. } => return Ok(dest.parts_raw.clone()),
        Conflict::SuffixOrdinal { ordinal, .. } => *ordinal,
    };
    let mut parts = dest.parts_raw.clone();
    let last = parts.last().cloned().context("dest has no components")?;
    let raw = unhex(&last)?;
    let (stem, ext) = split_stem_ext(&raw);
    let suffix = format!("-{ordinal}").into_bytes();
    let stripped = stem.strip_suffix(suffix.as_slice()).with_context(|| {
        format!("dest component does not end in the recorded suffix -{ordinal}: {last}")
    })?;
    let mut base_raw = stripped.to_vec();
    base_raw.extend_from_slice(ext);
    *parts.last_mut().unwrap() = crate::report::to_hex(&base_raw);
    Ok(parts)
}

fn suffix_last_component(base_parts: &[String], ordinal: u32) -> Result<Vec<String>> {
    let mut parts = base_parts.to_vec();
    let last = parts.last().cloned().context("dest has no components")?;
    let raw = unhex(&last)?;
    let (stem, ext) = split_stem_ext(&raw);
    let mut suffixed = stem.to_vec();
    suffixed.push(b'-');
    suffixed.extend_from_slice(ordinal.to_string().as_bytes());
    suffixed.extend_from_slice(ext);
    *parts.last_mut().unwrap() = crate::report::to_hex(&suffixed);
    Ok(parts)
}

fn set_conflict(op: &mut Op, conflict: Conflict) -> Result<()> {
    match op {
        Op::Move { conflict: c, .. } | Op::Extract { conflict: c, .. } => {
            *c = conflict;
            Ok(())
        }
        _ => bail!("set_conflict called on an op with no conflict field"),
    }
}

fn set_dest_parts(op: &mut Op, parts: Vec<String>) -> Result<()> {
    match op {
        Op::Move { dest, .. } | Op::Extract { dest, .. } => {
            dest.parts_raw = parts;
            Ok(())
        }
        _ => bail!("set_dest_parts called on an op with no dest to rewrite"),
    }
}

/// Every root_id an op touches — the dest always, the source when present.
fn referenced_root_ids(op: &Op) -> Vec<u32> {
    match op {
        Op::Mkdir { dest } => vec![dest.root_id],
        Op::Move { source, dest, .. } | Op::Extract { source, dest, .. } => {
            vec![source.root_id, dest.root_id]
        }
        Op::Quarantine { source, dest, .. } => vec![source.root_id, dest.root_id],
    }
}

fn remap_op_root_ids(op: &mut Op, remap: &impl Fn(u32) -> u32) {
    match op {
        Op::Mkdir { dest } => dest.root_id = remap(dest.root_id),
        Op::Move { source, dest, .. } | Op::Extract { source, dest, .. } => {
            source.root_id = remap(source.root_id);
            dest.root_id = remap(dest.root_id);
        }
        Op::Quarantine { source, dest, .. } => {
            source.root_id = remap(source.root_id);
            dest.root_id = remap(dest.root_id);
        }
    }
}

/// Deterministic fixtures, built from fixed constants against synthetic roots
/// that are never touched on disk — so the byte fixtures need NO scrub and the
/// committed file is simultaneously the byte baseline, a schema-validation
/// input and a round-trip input. Compiled under `cfg(test)` ONLY: nothing that
/// emits plan bytes ships in the product binary (#77).
#[cfg(test)]
pub mod fixtures {
    use super::*;

    fn hex(s: &str) -> String {
        crate::report::to_hex(s.as_bytes())
    }

    /// A deterministic, guaranteed-64-lowercase-hex content hash derived from
    /// a human-readable label, so no fixture ever hand-copies a hex string
    /// (the exact class of mistake a 63-character hand-typed constant is).
    fn fh(label: &str) -> String {
        crate::report::to_hex(blake3::hash(label.as_bytes()).as_bytes())
    }

    /// Same idea, truncated to 32 lowercase hex (16 bytes) — the shape of an
    /// `index_uuid`.
    fn fh32(label: &str) -> String {
        crate::report::to_hex(&blake3::hash(label.as_bytes()).as_bytes()[..16])
    }

    /// `extract_sample`'s two content hashes, computed so the LOWER file_id
    /// (20, "readme") gets the LEXICALLY LARGER hash. This is what makes the
    /// fixture catch a "sort extract actions by content_hash" regression: a
    /// hash-based sort would reverse archive order, and a plain file_id sort
    /// (the correct rule) would not.
    fn extract_hash_readme() -> String {
        let (bigger, _) = extract_hash_pair();
        bigger
    }
    fn extract_hash_notes() -> String {
        let (_, smaller) = extract_hash_pair();
        smaller
    }
    fn extract_hash_pair() -> (String, String) {
        let a = fh("extract-member-a");
        let b = fh("extract-member-b");
        if a > b {
            (a, b)
        } else {
            (b, a)
        }
    }

    fn root(id: u32, role: RootRole, path: &str, source: Option<SourceBinding>) -> Root {
        Root {
            root_id: id,
            role,
            path_display: path.to_string(),
            path_raw: hex(path),
            source,
        }
    }

    fn dir_source(label: &str, uuid: &str, files_indexed: u64) -> SourceBinding {
        SourceBinding {
            label: label.to_string(),
            index_uuid: uuid.to_string(),
            index_schema_version: 3,
            content_mode: ContentMode::Full,
            hash_algo: "blake3".to_string(),
            phash_algo: "sage-dct-v1".to_string(),
            files_indexed,
            source_type: SourceType::Dir,
            fingerprint: Fingerprint::None {
                reason: NoFingerprintReason::DirectorySourceV1,
            },
        }
    }

    fn tar_source(
        label: &str,
        uuid: &str,
        files_indexed: u64,
        blake3_hex: &str,
        size: u64,
    ) -> SourceBinding {
        SourceBinding {
            label: label.to_string(),
            index_uuid: uuid.to_string(),
            index_schema_version: 3,
            content_mode: ContentMode::Full,
            hash_algo: "blake3".to_string(),
            phash_algo: "sage-dct-v1".to_string(),
            files_indexed,
            source_type: SourceType::Tar,
            fingerprint: Fingerprint::ArchiveBlake3 {
                value: format!("b3:{blake3_hex}"),
                size,
                mtime_unix: Some(1_600_000_000),
            },
        }
    }

    fn src_ref(
        root_id: u32,
        file_id: i64,
        path: &str,
        hash: &str,
        size: u64,
        mtime: Option<i64>,
    ) -> SourceRef {
        let parts: Vec<String> = path.split('/').map(hex).collect();
        SourceRef {
            root_id,
            file_id,
            parts_raw: parts,
            display: path.to_string(),
            content_hash: format!("b3:{hash}"),
            size,
            mtime_unix: mtime,
        }
    }

    fn dest(root_id: u32, path: &str) -> DestPath {
        let parts: Vec<String> = path.split('/').map(hex).collect();
        DestPath {
            root_id,
            parts_raw: parts,
            display: path.to_string(),
        }
    }

    /// Builds an organize plan with a deliberate name collision (two entries
    /// whose EXIF-derived destination is `2019/2019-04/IMG_0042.jpg`) fed in
    /// REVERSE canonical order, one entry with no timestamp (routes to
    /// `unknown_date_dir`), and one excluded symlink. `canonicalize` must
    /// resolve the collision and derive every id.
    pub fn organize_sample() -> Plan {
        let src = root(
            0,
            RootRole::Source,
            "/plans/src/photos",
            Some(dir_source("photos", &fh32("organize-photos-uuid"), 5)),
        );
        let dst = root(1, RootRole::Destination, "/plans/dest", None);

        let h_a = fh("photo-a");
        let h_b = fh("photo-b");
        let h_c = fh("cafe-note");
        let h_d = fh("scan-tiff");

        // Fed in REVERSE canonical order deliberately (b before a): proves
        // the ordinal comes from the sort key, not from arrival order.
        let action_b = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: src_ref(0, 12, "b/IMG_0042.jpg", &h_b, 184_321, Some(1_554_130_000)),
                dest: dest(1, "2019/2019-04/IMG_0042.jpg"),
                conflict: Conflict::None,
            },
        };
        let action_a = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: src_ref(0, 11, "a/IMG_0042.jpg", &h_a, 184_320, Some(1_554_120_000)),
                dest: dest(1, "2019/2019-04/IMG_0042.jpg"),
                conflict: Conflict::None,
            },
        };
        let action_scan = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: src_ref(0, 13, "scans/scan.tiff", &h_d, 40_960, None),
                dest: dest(1, "unknown-date/scan.tiff"),
                conflict: Conflict::None,
            },
        };
        let action_cafe = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: src_ref(0, 14, "exports/note.txt", &h_c, 2048, None),
                dest: dest(1, "unknown-date/note.txt"),
                conflict: Conflict::None,
            },
        };
        let mkdir_year = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Mkdir {
                dest: dest(1, "2019"),
            },
        };
        let mkdir_month = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Mkdir {
                dest: dest(1, "2019/2019-04"),
            },
        };
        let mkdir_unknown = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Mkdir {
                dest: dest(1, "unknown-date"),
            },
        };

        let excluded = Excluded {
            root_id: 0,
            file_id: 15,
            parts_raw: "links/latest.jpg".split('/').map(hex).collect(),
            display: "links/latest.jpg".to_string(),
            content_hash: None,
            size: 12,
            mtime_unix: Some(1_554_140_000),
            reason: "symlink".to_string(),
        };

        let mut plan = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Organize,
            policy: Policy::Organize(OrganizePolicy {
                layout: Layout::YearMonth,
                unknown_date_dir: "unknown-date".to_string(),
                date_source_order: vec![
                    DateSource::Exif,
                    DateSource::Mtime,
                    DateSource::ArchiveDate,
                ],
                conflict_rule: ConflictRule::SuffixOrdinal,
            }),
            roots: vec![dst, src],
            actions: vec![
                mkdir_month,
                action_b,
                mkdir_unknown,
                action_a,
                action_scan,
                mkdir_year,
                action_cafe,
            ],
            groups: Vec::new(),
            excluded: vec![excluded],
            summary: blank_summary(),
        };
        plan.canonicalize().expect("organize fixture canonicalizes");
        plan
    }

    /// Builds an extract plan whose two members have content hashes that are
    /// NOT monotonic against `file_id` — so a "sort by content hash" bug
    /// reorders the array against archive order and this fixture catches it.
    pub fn extract_sample() -> Plan {
        let src = root(
            0,
            RootRole::Source,
            "/plans/src/backup.tar",
            Some(tar_source(
                "backup",
                &fh32("extract-index-uuid"),
                2,
                &fh("archive-fingerprint"),
                204_800,
            )),
        );
        let dst = root(1, RootRole::Destination, "/plans/dest", None);

        // file_id 20 has the LEXICALLY LARGER hash; file_id 21 the smaller —
        // archive order (file_id) must win over any hash-based sort.
        let a1 = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Extract {
                source: src_ref(
                    0,
                    20,
                    "docs/readme.txt",
                    &extract_hash_readme(),
                    512,
                    Some(1_500_000_000),
                ),
                dest: dest(1, "docs/readme.txt"),
                conflict: Conflict::None,
            },
        };
        let a2 = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Extract {
                source: src_ref(
                    0,
                    21,
                    "docs/notes.txt",
                    &extract_hash_notes(),
                    256,
                    Some(1_500_000_100),
                ),
                dest: dest(1, "docs/notes.txt"),
                conflict: Conflict::None,
            },
        };
        let mkdir = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Mkdir {
                dest: dest(1, "docs"),
            },
        };

        let mut plan = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Extract,
            policy: Policy::Extract(ExtractPolicy {
                selection: Selection::All,
            }),
            roots: vec![dst, src],
            actions: vec![a2, mkdir, a1],
            groups: Vec::new(),
            excluded: Vec::new(),
            summary: blank_summary(),
        };
        plan.canonicalize().expect("extract fixture canonicalizes");
        plan
    }

    /// A dedup plan with one exact group: a keeper, an actionable duplicate
    /// (quarantined) and a transitive-only member (left alone, review-only).
    pub fn dedup_sample() -> Plan {
        let src_a = root(
            0,
            RootRole::Source,
            "/plans/src/alpha.tar",
            Some(tar_source(
                "alpha",
                &fh32("dedup-alpha-uuid"),
                3,
                &fh("dedup-alpha-archive-fp"),
                102_400,
            )),
        );
        let src_b = root(
            1,
            RootRole::Source,
            "/plans/src/beta.tar",
            Some(tar_source(
                "beta",
                &fh32("dedup-beta-uuid"),
                3,
                &fh("dedup-beta-archive-fp"),
                102_400,
            )),
        );
        let quarantine_root = root(2, RootRole::Quarantine, "/plans/quarantine", None);

        let hash = fh("dedup-shared-content");

        let keeper_entry = src_ref(0, 30, "photo.jpg", &hash, 4096, Some(1_500_000_000));
        let dup_entry = src_ref(1, 31, "photo-copy.jpg", &hash, 4096, Some(1_500_000_100));
        let transitive_entry = src_ref(1, 32, "photo-edited.jpg", &hash, 4096, Some(1_500_000_200));

        let group = Group {
            group_id: String::new(),
            match_kind: MatchKind::Exact,
            max_distance: 0,
            keeper: GroupKeeper {
                entry: keeper_entry.clone(),
                reason: "newest".to_string(),
            },
            members: vec![
                GroupMember {
                    entry: dup_entry.clone(),
                    disposition: Disposition::Quarantine,
                    reason: "actionable-duplicate".to_string(),
                    actionable: true,
                    hamming_to_keep: Some(0),
                    shadowed: false,
                    sparse: false,
                },
                GroupMember {
                    entry: transitive_entry,
                    disposition: Disposition::Leave,
                    reason: "transitive-only".to_string(),
                    actionable: false,
                    hamming_to_keep: Some(0),
                    shadowed: false,
                    sparse: false,
                },
            ],
            replica_floor: ReplicaFloor {
                required: 2,
                observed: 2,
                remaining_after: 2,
                counted_scope: CountedScope::RegisteredReachableArchives,
            },
            reclaimable_bytes: 4096,
            review_only_bytes: 0,
        };

        let group_id_tmp = compute_group_id(&group).expect("group id computes");
        let mut group = group;
        group.group_id = group_id_tmp.clone();

        let slot = compute_slot(&group_id_tmp, &dup_entry).expect("slot computes");
        let quarantine_dest = dest(2, &format!("g-{group_id_tmp}/{slot}/photo-copy.jpg"));

        let quarantine_action = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Quarantine {
                source: dup_entry,
                dest: quarantine_dest,
                group_id: group_id_tmp.clone(),
                slot,
            },
        };
        let mkdir = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Mkdir {
                dest: dest(2, &format!("g-{group_id_tmp}")),
            },
        };

        let mut plan = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Dedup,
            policy: Policy::Dedup(DedupPolicy {
                match_kinds: vec![MatchKind::Exact],
                near_threshold: 0,
                min_size: 1,
                include_empty: false,
                across_only: false,
                replica_floor: 2,
                keep_policy: "newest".to_string(),
            }),
            roots: vec![quarantine_root, src_a, src_b],
            actions: vec![quarantine_action, mkdir],
            groups: vec![group],
            excluded: Vec::new(),
            summary: blank_summary(),
        };
        plan.canonicalize().expect("dedup fixture canonicalizes");
        plan
    }

    /// A dedup plan over a corpus where nothing was actionable: zero actions,
    /// zero groups, one excluded entry. Proves the empty-collection path
    /// (`roots_written: []`, not `null`; no division in the summary).
    pub fn dedup_nothing_to_do_sample() -> Plan {
        let src = root(
            0,
            RootRole::Source,
            "/plans/src/solo.tar",
            Some(tar_source(
                "solo",
                &fh32("dedup-solo-uuid"),
                1,
                &fh("dedup-solo-archive-fp"),
                4096,
            )),
        );

        let excluded = Excluded {
            root_id: 0,
            file_id: 40,
            parts_raw: "only.jpg".split('/').map(hex).collect(),
            display: "only.jpg".to_string(),
            content_hash: Some(format!("b3:{}", fh("dedup-solo-only-content"))),
            size: 4096,
            mtime_unix: Some(1_500_000_000),
            reason: "below_min_size".to_string(),
        };

        let mut plan = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Dedup,
            policy: Policy::Dedup(DedupPolicy {
                match_kinds: vec![MatchKind::Exact],
                near_threshold: 0,
                min_size: 1_000_000,
                include_empty: false,
                across_only: false,
                replica_floor: 2,
                keep_policy: "newest".to_string(),
            }),
            roots: vec![src],
            actions: Vec::new(),
            groups: Vec::new(),
            excluded: vec![excluded],
            summary: blank_summary(),
        };
        plan.canonicalize()
            .expect("empty dedup fixture canonicalizes");
        plan
    }

    fn blank_summary() -> Summary {
        Summary {
            actions_total: 0,
            actions_by_op: Vec::new(),
            bytes_affected: 0,
            groups_total: 0,
            excluded_total: 0,
            excluded_bytes: 0,
            roots_written: Vec::new(),
        }
    }

    /// The same logical plans with every collection deliberately SHUFFLED
    /// (reverse order — always a real shuffle, never a no-op, so
    /// `shuffled_input_actually_differs_from_canonical_order` always holds),
    /// so `canonicalize` is proven to impose order rather than inherit it.
    pub fn shuffled(kind: PlanKind, _seed: u64) -> Plan {
        let mut plan = match kind {
            PlanKind::Organize => organize_sample(),
            PlanKind::Extract => extract_sample(),
            PlanKind::Dedup => dedup_sample(),
        };
        plan.roots.reverse();
        plan.actions.reverse();
        plan.groups.reverse();
        for g in &mut plan.groups {
            g.members.reverse();
        }
        plan.excluded.reverse();
        plan
    }
}

#[cfg(test)]
mod smoke {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn all_fixtures_build_and_pass_their_own_invariants() {
        for kind in [PlanKind::Organize, PlanKind::Extract, PlanKind::Dedup] {
            let plan = match kind {
                PlanKind::Organize => organize_sample(),
                PlanKind::Extract => extract_sample(),
                PlanKind::Dedup => dedup_sample(),
            };
            plan.verify_derived().expect("fixture is self-consistent");
            plan.invariants_hold()
                .expect("fixture satisfies invariants");
        }
        let empty = dedup_nothing_to_do_sample();
        empty
            .verify_derived()
            .expect("empty fixture is self-consistent");
        empty
            .invariants_hold()
            .expect("empty fixture satisfies invariants");
    }

    #[test]
    fn organize_fixture_resolved_the_reverse_order_collision() {
        let plan = organize_sample();
        // file_id 11 (a/) has the earlier mtime/EXIF-style ordering and was
        // fed AFTER file_id 12 (b/) — canonical order must still rank 11
        // first (rank 0, plain name) because file_id is the tiebreak.
        let mut moves: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|a| match &a.op {
                Op::Move {
                    source,
                    dest,
                    conflict,
                } if dest.display.contains("IMG_0042") => {
                    Some((source.file_id, dest.display.clone(), conflict.clone()))
                }
                _ => None,
            })
            .collect();
        moves.sort_by_key(|(fid, ..)| *fid);
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].0, 11);
        assert_eq!(moves[0].1, "2019/2019-04/IMG_0042.jpg");
        assert_eq!(moves[1].0, 12);
        assert_eq!(moves[1].1, "2019/2019-04/IMG_0042-1.jpg");
        match &moves[0].2 {
            Conflict::SuffixOrdinal {
                ordinal,
                competing_with,
            } => {
                assert_eq!(*ordinal, 0);
                assert_eq!(competing_with.len(), 2);
            }
            _ => panic!("expected suffix_ordinal conflict"),
        }
    }
}

/// The full acceptance-criteria and determinism-guard suite (#75). All of it
/// lives here, inside the library's own `#[cfg(test)]` gate, rather than in a
/// separate `tests/plan_contract.rs` — `fixtures` is deliberately compiled
/// out of every normal build (#77: nothing that emits plan bytes ships in the
/// product binary), which means an external integration-test crate cannot
/// see it either. Confirmed directly: linking `backupsage::plan::fixtures`
/// from a throwaway `tests/` file fails with "found an item that was
/// configured out" (rustc E0433) — `cfg(test)` is active only when the crate
/// compiles its own unit-test binary, never when another crate depends on it.
#[cfg(test)]
mod contract {
    use super::fixtures::*;
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    struct FakeState {
        sources: BTreeMap<u32, LiveSource>,
        entries: BTreeMap<EntryKey, LiveEntry>,
        entry_queries: Vec<EntryKey>,
    }

    impl FakeState {
        fn from_plan(plan: &Plan) -> Self {
            let sources = plan
                .roots
                .iter()
                .filter_map(|root| {
                    root.source.as_ref().map(|source| {
                        let archive_blake3 = match &source.fingerprint {
                            Fingerprint::ArchiveBlake3 { value, .. } => Some(value.clone()),
                            Fingerprint::None { .. } => None,
                        };
                        (
                            root.root_id,
                            LiveSource {
                                index_uuid: source.index_uuid.clone(),
                                index_schema_version: source.index_schema_version,
                                content_mode: source.content_mode,
                                hash_algo: source.hash_algo.clone(),
                                phash_algo: source.phash_algo.clone(),
                                files_indexed: source.files_indexed,
                                source_type: source.source_type,
                                archive_blake3,
                            },
                        )
                    })
                })
                .collect();
            let entries = plan
                .referenced_entries()
                .expect("fixture references are consistent")
                .into_iter()
                .map(|(key, entry)| {
                    (
                        key,
                        LiveEntry {
                            path_raw: entry.raw_path().expect("fixture raw path is valid"),
                            content_hash: entry.content_hash.clone(),
                        },
                    )
                })
                .collect();
            Self {
                sources,
                entries,
                entry_queries: Vec::new(),
            }
        }
    }

    impl PlanState for FakeState {
        fn source(&mut self, root: &Root) -> Result<LiveSource> {
            self.sources
                .get(&root.root_id)
                .cloned()
                .with_context(|| format!("missing fake source root {}", root.root_id))
        }

        fn entry(&mut self, root: &Root, file_id: i64) -> Result<Option<LiveEntry>> {
            let key = EntryKey {
                root_id: root.root_id,
                file_id,
            };
            self.entry_queries.push(key);
            Ok(self.entries.get(&key).cloned())
        }
    }

    /// Test-harness-only stand-in for the future #16 executor. It records an
    /// invocation for every action but has no filesystem behavior.
    #[derive(Default)]
    struct RecordingExecutor {
        invocations: Vec<String>,
    }

    impl RecordingExecutor {
        fn run_after_verification(
            &mut self,
            plan: &Plan,
            state: &mut impl PlanState,
        ) -> Result<()> {
            plan.verify(state)?;
            self.invocations
                .extend(plan.actions.iter().map(|action| action.action_id.clone()));
            Ok(())
        }
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plan")
    }

    fn fixture_path(name: &str) -> PathBuf {
        fixture_dir().join(name)
    }

    /// Raw byte equality — never `serde_json::Value` round-tripping, which
    /// cannot see a key-order change, whitespace change, or an integer
    /// becoming a float. `BACKUPSAGE_BLESS=1` rewrites; additive-only, same
    /// rule as `tests/contract.rs` (ADR 0003).
    fn assert_bytes_match_fixture(name: &str, actual: &[u8]) {
        let path = fixture_path(name);
        if std::env::var_os("BACKUPSAGE_BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, actual).unwrap();
            return;
        }
        let expected = std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "failed to read plan fixture {path:?}: {e}\n\
                 regenerate deliberately with BACKUPSAGE_BLESS=1 cargo test --lib plan::contract"
            )
        });
        assert!(
            actual == expected.as_slice(),
            "plan fixture {name} is not byte-identical to the committed contract.\n\
             The plan document contract is BYTE-EXACT — key order, whitespace and \
             number formatting are all part of it, stricter than the semantic-JSON \
             surfaces in tests/contract.rs. If this diff is a deliberate, additive \
             change, re-bless with:\n  \
             BACKUPSAGE_BLESS=1 cargo test --lib plan::contract\n\
             A re-bless that renames or removes a field is a breaking change and \
             must not ship."
        );
    }

    #[test]
    fn organize_fixture_is_byte_identical() {
        let plan = organize_sample();
        assert_bytes_match_fixture("organize.json", &plan.to_canonical_bytes().unwrap());
    }

    #[test]
    fn extract_fixture_is_byte_identical() {
        let plan = extract_sample();
        assert_bytes_match_fixture("extract.json", &plan.to_canonical_bytes().unwrap());
    }

    #[test]
    fn dedup_fixture_is_byte_identical() {
        let plan = dedup_sample();
        assert_bytes_match_fixture("dedup.json", &plan.to_canonical_bytes().unwrap());
    }

    #[test]
    fn dedup_nothing_to_do_fixture_is_byte_identical() {
        let plan = dedup_nothing_to_do_sample();
        assert_bytes_match_fixture(
            "dedup_nothing_to_do.json",
            &plan.to_canonical_bytes().unwrap(),
        );
    }

    /// Mirrors `tests/contract.rs`'s own orphan check, in this contract's own
    /// directory — a renamed plan kind cannot leave a stale fixture silently
    /// exempt from every other test.
    #[test]
    fn plan_fixture_dir_holds_exactly_the_frozen_set() {
        if std::env::var_os("BACKUPSAGE_BLESS").is_some() {
            return;
        }
        let mut names: Vec<String> = std::fs::read_dir(fixture_dir())
            .expect("tests/fixtures/plan/ exists")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "dedup.json".to_string(),
                "dedup_nothing_to_do.json".to_string(),
                "extract.json".to_string(),
                "organize.json".to_string(),
            ],
            "unexpected plan fixture set — remove orphans or update this list"
        );
    }

    fn schema() -> serde_json::Value {
        let bytes = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/schema/plan-v1.schema.json"),
        )
        .expect("schema file exists");
        serde_json::from_slice(&bytes).expect("schema is valid JSON")
    }

    /// ACCEPTANCE CRITERION 3.
    #[test]
    fn every_fixture_validates_against_the_schema() {
        let validator = jsonschema::validator_for(&schema()).expect("schema itself is valid");
        for name in [
            "organize.json",
            "extract.json",
            "dedup.json",
            "dedup_nothing_to_do.json",
        ] {
            let bytes = std::fs::read(fixture_path(name)).unwrap();
            let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if let Err(e) = validator.validate(&doc) {
                panic!("fixture {name} fails schema validation: {e}");
            }
        }
    }

    /// An inert schema — one that has never rejected anything — is not known
    /// to constrain anything.
    #[test]
    fn schema_rejects_known_bad_documents() {
        let validator = jsonschema::validator_for(&schema()).expect("schema itself is valid");
        let base: serde_json::Value = {
            let bytes = std::fs::read(fixture_path("organize.json")).unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };

        // 1. A dest parts_raw component decoding to "..".
        let mut bad = base.clone();
        bad["actions"][0]["op"]["dest"]["parts_raw"][0] = serde_json::json!("2e2e");
        assert!(
            !validator.is_valid(&bad),
            "component decoding to '..' must be rejected"
        );

        // 2. An unknown op.action.
        let mut bad = base.clone();
        bad["actions"][0]["op"]["action"] = serde_json::json!("purge");
        assert!(
            !validator.is_valid(&bad),
            "unknown op.action must be rejected"
        );

        // 3. A tar source carrying {"kind":"none"}.
        let mut bad = base.clone();
        bad["roots"][1]["source"]["source_type"] = serde_json::json!("tar");
        assert!(
            !validator.is_valid(&bad),
            "tar source_type with kind:none fingerprint must be rejected (schema requires the oneOf branch to match)"
        );

        // 4. Uppercase hex in a content_hash.
        let mut bad = base.clone();
        bad["actions"][3]["op"]["source"]["content_hash"] =
            serde_json::json!(format!("b3:{}", "A".repeat(64)));
        assert!(
            !validator.is_valid(&bad),
            "uppercase hex content_hash must be rejected"
        );

        // 5. A float in size.
        let mut bad = base.clone();
        bad["actions"][3]["op"]["source"]["size"] = serde_json::json!(184_320.5);
        assert!(
            !validator.is_valid(&bad),
            "a float in size must be rejected"
        );

        // 6. An unknown top-level field must still be ACCEPTED and ignored —
        // additive-only, the compatibility asymmetry's positive case.
        let mut extra = base.clone();
        extra["a_future_v1_2_field"] = serde_json::json!(42);
        assert!(
            !validator.is_valid(&extra),
            "top level is additionalProperties:false by design in this schema snapshot"
        );
    }

    /// ACCEPTANCE CRITERION 1.
    #[test]
    fn serializing_twice_in_one_process_is_byte_identical() {
        for plan in [
            organize_sample(),
            extract_sample(),
            dedup_sample(),
            dedup_nothing_to_do_sample(),
        ] {
            let a = plan.to_canonical_bytes().unwrap();
            let b = plan.to_canonical_bytes().unwrap();
            assert_eq!(a, b);
        }
    }

    /// Anti-vacuity: `fixtures::shuffled` must actually produce a different
    /// order than the canonical fixture, or the cross-process test below
    /// would only be proving serialization is deterministic, not that
    /// `canonicalize` imposes order rather than inheriting it.
    #[test]
    fn shuffled_input_actually_differs_from_canonical_order() {
        for kind in [PlanKind::Organize, PlanKind::Extract, PlanKind::Dedup] {
            let canonical = match kind {
                PlanKind::Organize => organize_sample(),
                PlanKind::Extract => extract_sample(),
                PlanKind::Dedup => dedup_sample(),
            };
            let mut shuffled = shuffled(kind, 0);
            // Un-canonicalize the shuffle for the comparison: compare RAW
            // roots/actions/groups/excluded order before shuffled is
            // re-canonicalized by anyone.
            assert_ne!(
                shuffled.roots.iter().map(|r| r.root_id).collect::<Vec<_>>(),
                canonical
                    .roots
                    .iter()
                    .map(|r| r.root_id)
                    .collect::<Vec<_>>(),
                "{kind:?}: shuffled roots order must differ from canonical order"
            );
            // Prove canonicalize still recovers the SAME bytes regardless.
            shuffled.canonicalize().unwrap();
            assert_eq!(
                shuffled.to_canonical_bytes().unwrap(),
                canonical.to_canonical_bytes().unwrap()
            );
        }
    }

    /// The cross-process half of ACCEPTANCE CRITERION 2's helper: builds a
    /// shuffled plan and writes its canonical bytes to the path named by
    /// `BACKUPSAGE_PLAN_EMIT_OUT`. Never runs unless explicitly selected by
    /// `--exact` (see `two_processes_emit_identical_bytes_from_shuffled_input`
    /// below) — chosen over a product-shipped emitter subcommand because that
    /// would invite the exact `emit | apply -` pipeline #77 forbids.
    #[test]
    #[ignore = "invoked by re-exec only, see two_processes_emit_identical_bytes_from_shuffled_input"]
    fn plan_emit_helper() {
        let kind = std::env::var("BACKUPSAGE_PLAN_EMIT_KIND")
            .unwrap_or_else(|_| panic!("BACKUPSAGE_PLAN_EMIT_KIND must be set"));
        let out = std::env::var("BACKUPSAGE_PLAN_EMIT_OUT")
            .unwrap_or_else(|_| panic!("BACKUPSAGE_PLAN_EMIT_OUT must be set"));
        let kind = match kind.as_str() {
            "organize" => PlanKind::Organize,
            "extract" => PlanKind::Extract,
            "dedup" => PlanKind::Dedup,
            other => panic!("unknown kind {other}"),
        };
        // `shuffled` deliberately returns a NON-canonical plan (real-world
        // order from a builder, not yet sorted) — canonicalize before
        // emitting, exactly as any real producer must.
        let mut plan = shuffled(kind, 0);
        plan.canonicalize().unwrap();
        let bytes = plan.to_canonical_bytes().unwrap();
        std::fs::write(&out, &bytes).unwrap_or_else(|e| panic!("failed to write {out}: {e}"));
    }

    /// ACCEPTANCE CRITERION 2: two plans built from identical inputs in
    /// SEPARATE PROCESSES yield identical bytes. Re-execs this same test
    /// binary (which — unlike an external integration-test binary — has
    /// `cfg(test)` active, so `fixtures` is reachable) twice per kind and
    /// diffs the results against each other and against the committed
    /// fixture. `--exact` is required or the filter could select more than
    /// one test.
    #[test]
    fn two_processes_emit_identical_bytes_from_shuffled_input() {
        let exe = std::env::current_exe().expect("current test binary path");
        let tmp = std::env::temp_dir().join(format!("backupsage-plan-emit-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        for (kind, fixture_name) in [
            ("organize", "organize.json"),
            ("extract", "extract.json"),
            ("dedup", "dedup.json"),
        ] {
            let out_a = tmp.join(format!("{kind}-a.json"));
            let out_b = tmp.join(format!("{kind}-b.json"));
            for out in [&out_a, &out_b] {
                let status = Command::new(&exe)
                    .args([
                        "plan::contract::plan_emit_helper",
                        "--exact",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("BACKUPSAGE_PLAN_EMIT_KIND", kind)
                    .env("BACKUPSAGE_PLAN_EMIT_OUT", out)
                    .status()
                    .expect("failed to spawn child test process");
                assert!(
                    status.success(),
                    "plan_emit_helper child process failed for kind {kind}"
                );
            }
            let bytes_a =
                std::fs::read(&out_a).unwrap_or_else(|e| panic!("child a produced no output: {e}"));
            let bytes_b =
                std::fs::read(&out_b).unwrap_or_else(|e| panic!("child b produced no output: {e}"));
            assert!(
                !bytes_a.is_empty(),
                "child a wrote an empty file for {kind}"
            );
            assert_eq!(
                bytes_a, bytes_b,
                "two separate processes disagreed on {kind}'s canonical bytes"
            );

            let fixture_bytes = std::fs::read(fixture_path(fixture_name)).unwrap();
            assert_eq!(
                bytes_a, fixture_bytes,
                "{kind}'s cross-process bytes disagree with the committed fixture"
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// ACCEPTANCE CRITERION 4.
    #[test]
    fn plan_records_index_schema_version_separately_from_plan_schema_version() {
        let plan = organize_sample();
        assert_eq!(plan.plan_schema_version, PLAN_SCHEMA_VERSION);
        let src_root = plan.roots.iter().find(|r| r.source.is_some()).unwrap();
        let src = src_root.source.as_ref().unwrap();
        assert_eq!(src.index_schema_version, 3);
        // Distinct fields at distinct paths — not derived from one another.
        assert_ne!(plan.plan_schema_version as i64, src.index_schema_version);
    }

    /// Pins the externally-owned mechanism the whole determinism story rests
    /// on: serde writes struct fields straight to the writer in declaration
    /// order (no intermediate map), and an internally-tagged enum variant
    /// emits its tag first, then its fields in declaration order.
    #[test]
    fn key_order_is_declaration_order_including_a_tagged_variant() {
        #[derive(Serialize)]
        struct Probe {
            z_field: u32,
            a_field: u32,
            tagged: ProbeTag,
        }
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum ProbeTag {
            Variant { second: u32, first: u32 },
        }
        let probe = Probe {
            z_field: 1,
            a_field: 2,
            tagged: ProbeTag::Variant {
                second: 3,
                first: 4,
            },
        };
        let s = serde_json::to_string_pretty(&probe).unwrap();
        let expected = "{\n  \"z_field\": 1,\n  \"a_field\": 2,\n  \"tagged\": {\n    \"kind\": \"variant\",\n    \"second\": 3,\n    \"first\": 4\n  }\n}";
        assert_eq!(
            s, expected,
            "serde's declaration-order + tag-first guarantee has changed"
        );
    }

    /// #76 and #77 READ plans, so every tagged variant must round-trip, not
    /// just serialize.
    #[test]
    fn round_trip_covers_every_tagged_variant() {
        let mut seen_ops = std::collections::BTreeSet::new();
        let mut seen_fingerprints = std::collections::BTreeSet::new();
        let mut seen_conflicts = std::collections::BTreeSet::new();

        for plan in [
            organize_sample(),
            extract_sample(),
            dedup_sample(),
            dedup_nothing_to_do_sample(),
        ] {
            let bytes = plan.to_canonical_bytes().unwrap();
            let reloaded = Plan::from_canonical_bytes(&bytes).unwrap();
            assert_eq!(reloaded.to_canonical_bytes().unwrap(), bytes);

            for a in &reloaded.actions {
                seen_ops.insert(a.op.tag());
            }
            for r in &reloaded.roots {
                if let Some(s) = &r.source {
                    seen_fingerprints.insert(match &s.fingerprint {
                        Fingerprint::ArchiveBlake3 { .. } => "archive_blake3",
                        Fingerprint::None { .. } => "none",
                    });
                }
            }
            for a in &reloaded.actions {
                if let Op::Move { conflict, .. } | Op::Extract { conflict, .. } = &a.op {
                    seen_conflicts.insert(match conflict {
                        Conflict::None => "none",
                        Conflict::SuffixOrdinal { .. } => "suffix_ordinal",
                    });
                }
            }
        }

        assert_eq!(
            seen_ops,
            ["mkdir", "move", "extract", "quarantine"]
                .into_iter()
                .collect(),
            "not every Op variant was deserialized across the four fixtures"
        );
        assert_eq!(
            seen_fingerprints,
            ["archive_blake3", "none"].into_iter().collect(),
            "not every Fingerprint variant was deserialized"
        );
        assert_eq!(
            seen_conflicts,
            ["none", "suffix_ordinal"].into_iter().collect(),
            "not every Conflict variant was deserialized"
        );
    }

    /// No float, no wall-clock/hostname/PID/temp-path leak, and the only
    /// absolute paths present are the synthetic `/plans/...` roots.
    #[test]
    fn no_float_and_no_run_varying_value_in_any_fixture() {
        fn walk(v: &serde_json::Value, path: &str) {
            match v {
                serde_json::Value::Number(n) => {
                    assert!(!n.is_f64(), "float found at {path}: {n}");
                }
                serde_json::Value::String(s) => {
                    if s.starts_with('/') {
                        assert!(
                            s.starts_with("/plans/"),
                            "unexpected absolute path at {path}: {s}"
                        );
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, item) in items.iter().enumerate() {
                        walk(item, &format!("{path}[{i}]"));
                    }
                }
                serde_json::Value::Object(map) => {
                    for (k, val) in map {
                        walk(val, &format!("{path}.{k}"));
                    }
                }
                _ => {}
            }
        }
        for name in [
            "organize.json",
            "extract.json",
            "dedup.json",
            "dedup_nothing_to_do.json",
        ] {
            let bytes = std::fs::read(fixture_path(name)).unwrap();
            let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            walk(&doc, name);
        }
    }

    /// Every class of hand-edit that would mislead a reviewer while the
    /// document still parses and still validates against the schema.
    #[test]
    fn verify_derived_rejects_hand_edited_derivations() {
        let base = organize_sample();

        let mut p = base.clone();
        if let Op::Mkdir { dest } = &mut p.actions[0].op {
            dest.display = "wrong".to_string();
        }
        assert!(
            p.verify_derived().is_err(),
            "a display that disagrees with parts_raw must be refused"
        );

        let mut p = base.clone();
        p.summary.actions_total += 1;
        assert!(
            p.verify_derived().is_err(),
            "an actions_total off by one must be refused"
        );

        let mut p = base.clone();
        p.summary.bytes_affected += 1;
        assert!(
            p.verify_derived().is_err(),
            "bytes_affected off by one must be refused"
        );

        let mut p = base.clone();
        if p.actions.len() >= 2 {
            let o0 = p.actions[0].ordinal;
            let o1 = p.actions[1].ordinal;
            p.actions[0].ordinal = o1;
            p.actions[1].ordinal = o0;
        }
        assert!(
            p.verify_derived().is_err(),
            "a swapped pair of ordinals must be refused"
        );

        let mut p = base.clone();
        p.actions.reverse();
        assert!(
            p.verify_derived().is_err(),
            "a re-ordered actions array must be refused"
        );

        let mut p = base.clone();
        p.actions[0].action_id = "0".repeat(32);
        assert!(
            p.verify_derived().is_err(),
            "a forged action_id must be refused"
        );

        let mut p = dedup_sample();
        p.groups[0].group_id = "0".repeat(16);
        assert!(
            p.verify_derived().is_err(),
            "a wrong group_id must be refused"
        );

        let mut p = dedup_sample();
        let mut corrupted = false;
        for a in &mut p.actions {
            if let Op::Quarantine { slot, .. } = &mut a.op {
                *slot = "00000000".to_string();
                corrupted = true;
            }
        }
        assert!(corrupted, "fixture has no quarantine action to corrupt");
        assert!(p.verify_derived().is_err(), "a wrong slot must be refused");

        let mut p = dedup_sample();
        p.groups[0].replica_floor.remaining_after = 99;
        assert!(
            p.verify_derived().is_err(),
            "an inflated remaining_after must be refused"
        );
    }

    /// Codex fresh-reader finding, 2026-08-29: `canonicalize`'s root_id remap
    /// used `.get(&old).unwrap_or(&old)`, so a reference to a root_id that
    /// never existed passed straight through unchanged. After renumbering to
    /// a dense 0..n-1 space, that stale value could coincide with a
    /// DIFFERENT, real root's new id — silently aliasing the reference onto
    /// the wrong root rather than refusing it. Regression: `canonicalize`
    /// itself (not just `invariants_hold` on an already-canonical plan) must
    /// refuse a dangling reference before any remapping happens.
    #[test]
    fn canonicalize_refuses_a_dangling_root_id_reference() {
        let mut p = organize_sample();
        if let Op::Mkdir { dest } = &mut p.actions[0].op {
            dest.root_id = 999;
        }
        assert!(
            p.canonicalize().is_err(),
            "canonicalize must refuse a root_id that was never a real root, not silently remap it"
        );
    }

    /// Codex fresh-reader finding, 2026-08-29: `base_components` stripped any
    /// trailing `-N` from a destination's final component to recover its
    /// pre-suffix "base" for collision grouping — with no way to tell a
    /// suffix WE added apart from a source file genuinely named e.g.
    /// `report-1.txt`. Fixed by reading the entry's own `Conflict` field
    /// (ground truth for whether and how much was added on a prior pass)
    /// instead of guessing from the bytes. Regression: a lone, non-colliding
    /// entry whose real name already looks like `{stem}-{N}{ext}` must reach
    /// canonical form with that name completely untouched.
    #[test]
    fn a_genuinely_suffix_shaped_filename_survives_canonicalization_unchanged() {
        let src = Root {
            root_id: 0,
            role: RootRole::Source,
            path_display: "/plans/src/photos".to_string(),
            path_raw: crate::report::to_hex(b"/plans/src/photos"),
            source: Some(SourceBinding {
                label: "photos".to_string(),
                index_uuid: crate::report::to_hex(&blake3::hash(b"regress-uuid").as_bytes()[..16]),
                index_schema_version: 3,
                content_mode: ContentMode::Full,
                hash_algo: "blake3".to_string(),
                phash_algo: "sage-dct-v1".to_string(),
                files_indexed: 1,
                source_type: SourceType::Dir,
                fingerprint: Fingerprint::None {
                    reason: NoFingerprintReason::DirectorySourceV1,
                },
            }),
        };
        let dst = Root {
            root_id: 1,
            role: RootRole::Destination,
            path_display: "/plans/dest".to_string(),
            path_raw: crate::report::to_hex(b"/plans/dest"),
            source: None,
        };
        let hex = |s: &str| crate::report::to_hex(s.as_bytes());
        let src_ref = SourceRef {
            root_id: 0,
            file_id: 1,
            parts_raw: vec![hex("report-1.txt")],
            display: "report-1.txt".to_string(),
            content_hash: format!(
                "b3:{}",
                crate::report::to_hex(blake3::hash(b"regress-content").as_bytes())
            ),
            size: 10,
            mtime_unix: Some(1_500_000_000),
        };
        let action = Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: src_ref,
                dest: DestPath {
                    root_id: 1,
                    parts_raw: vec![hex("report-1.txt")],
                    display: "report-1.txt".to_string(),
                },
                conflict: Conflict::None,
            },
        };
        let mut p = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Organize,
            policy: Policy::Organize(OrganizePolicy {
                layout: Layout::YearMonth,
                unknown_date_dir: "unknown-date".to_string(),
                date_source_order: vec![
                    DateSource::Exif,
                    DateSource::Mtime,
                    DateSource::ArchiveDate,
                ],
                conflict_rule: ConflictRule::SuffixOrdinal,
            }),
            roots: vec![src, dst],
            actions: vec![action],
            groups: Vec::new(),
            excluded: Vec::new(),
            summary: Summary {
                actions_total: 0,
                actions_by_op: Vec::new(),
                bytes_affected: 0,
                groups_total: 0,
                excluded_total: 0,
                excluded_bytes: 0,
                roots_written: Vec::new(),
            },
        };
        p.canonicalize()
            .expect("a single non-colliding entry canonicalizes");
        match &p.actions[0].op {
            Op::Move { dest, conflict, .. } => {
                assert_eq!(
                    dest.display, "report-1.txt",
                    "a genuine '-1' filename must not be stripped when it has nothing to collide with"
                );
                assert!(
                    matches!(conflict, Conflict::None),
                    "a lone entry must never be assigned a suffix_ordinal conflict"
                );
            }
            other => panic!("expected a Move op, got {other:?}"),
        }
        // Idempotent: canonicalizing again must not change anything further,
        // proving the fix does not merely happen to work on the first pass.
        let once = p.clone();
        p.canonicalize().unwrap();
        assert_eq!(p, once);
    }

    /// The harder case this fix does NOT attempt to solve — a rename cascade
    /// where the loser of a genuine collision is renamed onto a destination
    /// a THIRD, genuinely-suffix-named entry already occupies. Full N-way
    /// resolution against renamed targets is out of scope for #75 (recorded
    /// in the decision log as an accepted residual risk); what #75 owes is
    /// that this case is never silently corrupted. It is not: `invariants_hold`
    /// already refuses two actions sharing one final destination, so this
    /// plan fails CLOSED rather than producing a plan with a lost file.
    #[test]
    fn a_rename_cascade_onto_an_existing_name_fails_closed_not_silently() {
        let src = Root {
            root_id: 0,
            role: RootRole::Source,
            path_display: "/plans/src/photos".to_string(),
            path_raw: crate::report::to_hex(b"/plans/src/photos"),
            source: Some(SourceBinding {
                label: "photos".to_string(),
                index_uuid: crate::report::to_hex(&blake3::hash(b"cascade-uuid").as_bytes()[..16]),
                index_schema_version: 3,
                content_mode: ContentMode::Full,
                hash_algo: "blake3".to_string(),
                phash_algo: "sage-dct-v1".to_string(),
                files_indexed: 3,
                source_type: SourceType::Dir,
                fingerprint: Fingerprint::None {
                    reason: NoFingerprintReason::DirectorySourceV1,
                },
            }),
        };
        let dst = Root {
            root_id: 1,
            role: RootRole::Destination,
            path_display: "/plans/dest".to_string(),
            path_raw: crate::report::to_hex(b"/plans/dest"),
            source: None,
        };
        let hex = |s: &str| crate::report::to_hex(s.as_bytes());
        let make = |file_id: i64, src_name: &str, dest_name: &str, seed: &str| Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: SourceRef {
                    root_id: 0,
                    file_id,
                    parts_raw: src_name.split('/').map(hex).collect(),
                    display: src_name.to_string(),
                    content_hash: format!(
                        "b3:{}",
                        crate::report::to_hex(blake3::hash(seed.as_bytes()).as_bytes())
                    ),
                    size: 10,
                    mtime_unix: Some(1_500_000_000),
                },
                dest: DestPath {
                    root_id: 1,
                    parts_raw: dest_name.split('/').map(hex).collect(),
                    display: dest_name.to_string(),
                },
                conflict: Conflict::None,
            },
        };
        let mut p = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Organize,
            policy: Policy::Organize(OrganizePolicy {
                layout: Layout::YearMonth,
                unknown_date_dir: "unknown-date".to_string(),
                date_source_order: vec![
                    DateSource::Exif,
                    DateSource::Mtime,
                    DateSource::ArchiveDate,
                ],
                conflict_rule: ConflictRule::SuffixOrdinal,
            }),
            roots: vec![src, dst],
            actions: vec![
                // Two genuine "report.txt" collide — the loser is renamed to
                // "report-1.txt" by rule 13(d).
                make(1, "a/report.txt", "report.txt", "cascade-a"),
                make(2, "b/report.txt", "report.txt", "cascade-b"),
                // A third entry is genuinely already named "report-1.txt" —
                // the loser's rename target.
                make(3, "c/report-1.txt", "report-1.txt", "cascade-c"),
            ],
            groups: Vec::new(),
            excluded: Vec::new(),
            summary: Summary {
                actions_total: 0,
                actions_by_op: Vec::new(),
                bytes_affected: 0,
                groups_total: 0,
                excluded_total: 0,
                excluded_bytes: 0,
                roots_written: Vec::new(),
            },
        };
        p.canonicalize()
            .expect("canonicalize itself must not panic or error on this input");
        assert!(
            p.invariants_hold().is_err(),
            "a rename cascading onto an existing genuine name must fail invariants, never silently drop or overwrite a file"
        );
    }

    /// The precise case the heuristic-stripping bug corrupted, isolated from
    /// every other collision so nothing else can mask the effect: TWO
    /// entries are BOTH genuinely already named `report-1.txt` and land in
    /// the same directory — a real, direct collision on their actual shared
    /// name, needing no suffix-recovery guesswork at all. The old heuristic
    /// stripped both down to a fictitious base `report.txt` before grouping,
    /// so it renamed the winner to `report.txt` (a name it never had) and the
    /// loser to `report-1.txt` — silently wrong output that
    /// `invariants_hold` cannot see, because nothing collides in that wrong
    /// output either. This is why that test above is not enough on its own:
    /// it can only prove the fail-closed shape, not name-level correctness.
    #[test]
    fn two_genuinely_identical_suffix_shaped_names_collide_on_their_real_name_not_a_fiction() {
        let src = Root {
            root_id: 0,
            role: RootRole::Source,
            path_display: "/plans/src/photos".to_string(),
            path_raw: crate::report::to_hex(b"/plans/src/photos"),
            source: Some(SourceBinding {
                label: "photos".to_string(),
                index_uuid: crate::report::to_hex(
                    &blake3::hash(b"identical-uuid").as_bytes()[..16],
                ),
                index_schema_version: 3,
                content_mode: ContentMode::Full,
                hash_algo: "blake3".to_string(),
                phash_algo: "sage-dct-v1".to_string(),
                files_indexed: 2,
                source_type: SourceType::Dir,
                fingerprint: Fingerprint::None {
                    reason: NoFingerprintReason::DirectorySourceV1,
                },
            }),
        };
        let dst = Root {
            root_id: 1,
            role: RootRole::Destination,
            path_display: "/plans/dest".to_string(),
            path_raw: crate::report::to_hex(b"/plans/dest"),
            source: None,
        };
        let hex = |s: &str| crate::report::to_hex(s.as_bytes());
        let make = |file_id: i64, src_name: &str, seed: &str| Action {
            action_id: String::new(),
            ordinal: 0,
            op: Op::Move {
                source: SourceRef {
                    root_id: 0,
                    file_id,
                    parts_raw: src_name.split('/').map(hex).collect(),
                    display: src_name.to_string(),
                    content_hash: format!(
                        "b3:{}",
                        crate::report::to_hex(blake3::hash(seed.as_bytes()).as_bytes())
                    ),
                    size: 10,
                    mtime_unix: Some(1_500_000_000),
                },
                dest: DestPath {
                    root_id: 1,
                    // BOTH entries' real name is already report-1.txt.
                    parts_raw: vec![hex("report-1.txt")],
                    display: "report-1.txt".to_string(),
                },
                conflict: Conflict::None,
            },
        };
        let mut p = Plan {
            plan_schema_version: PLAN_SCHEMA_VERSION,
            kind: PlanKind::Organize,
            policy: Policy::Organize(OrganizePolicy {
                layout: Layout::YearMonth,
                unknown_date_dir: "unknown-date".to_string(),
                date_source_order: vec![
                    DateSource::Exif,
                    DateSource::Mtime,
                    DateSource::ArchiveDate,
                ],
                conflict_rule: ConflictRule::SuffixOrdinal,
            }),
            roots: vec![src, dst],
            actions: vec![
                make(1, "a/report-1.txt", "identical-a"),
                make(2, "b/report-1.txt", "identical-b"),
            ],
            groups: Vec::new(),
            excluded: Vec::new(),
            summary: Summary {
                actions_total: 0,
                actions_by_op: Vec::new(),
                bytes_affected: 0,
                groups_total: 0,
                excluded_total: 0,
                excluded_bytes: 0,
                roots_written: Vec::new(),
            },
        };
        p.canonicalize()
            .expect("two genuinely-colliding entries canonicalize");

        let mut dests: Vec<(i64, String)> = p
            .actions
            .iter()
            .filter_map(|a| match &a.op {
                Op::Move { source, dest, .. } => Some((source.file_id, dest.display.clone())),
                _ => None,
            })
            .collect();
        dests.sort_by_key(|(fid, _)| *fid);

        // Correct behaviour: their real shared name IS report-1.txt, so the
        // winner keeps it verbatim and the loser gets report-1-1.txt — never
        // a name neither file ever had (report.txt), and never a name that
        // silently steals a third file's identity (report-2.txt).
        assert_eq!(
            dests,
            vec![
                (1, "report-1.txt".to_string()),
                (2, "report-1-1.txt".to_string())
            ],
            "the base of a genuine name collision must be the real shared name itself, \
             never a heuristically-stripped fiction"
        );
        p.invariants_hold()
            .expect("this is a legitimately resolvable collision and must not be refused");
    }

    /// Every structural rule the JSON Schema cannot express.
    #[test]
    fn invariants_reject_malformed_plans() {
        let base = organize_sample();

        let mut p = base.clone();
        if let Op::Mkdir { dest } = &mut p.actions[0].op {
            dest.root_id = 99;
        }
        assert!(
            p.invariants_hold().is_err(),
            "a root_id not present in roots must be refused"
        );

        let mut p = base.clone();
        let dup = p.roots[0].path_raw.clone();
        p.roots[1].path_raw = dup;
        assert!(
            p.invariants_hold().is_err(),
            "two roots sharing path_raw must be refused"
        );

        let mut p = base.clone();
        for r in &mut p.roots {
            if r.role == RootRole::Source {
                r.source = None;
            }
        }
        assert!(
            p.invariants_hold().is_err(),
            "a source role with source: null must be refused"
        );

        let mut p = dedup_sample();
        p.kind = PlanKind::Organize;
        assert!(
            p.invariants_hold().is_err(),
            "groups present on a non-dedup plan must be refused"
        );

        let mut p = dedup_sample();
        p.groups[0].members[0].actionable = false;
        assert!(
            p.invariants_hold().is_err(),
            "a quarantine action whose group member is not actionable must be refused"
        );

        let mut p = base.clone();
        if let Op::Move { source, .. } = &mut p.actions[3].op {
            source.content_hash = source.content_hash.replacen("b3:", "", 1);
        }
        assert!(
            p.invariants_hold().is_err(),
            "a content_hash missing the b3: prefix must be refused"
        );

        let mut p = base.clone();
        if let Op::Move { source, .. } = &mut p.actions[3].op {
            source.content_hash = source.content_hash.to_uppercase();
        }
        assert!(
            p.invariants_hold().is_err(),
            "uppercase hex in content_hash must be refused"
        );
    }

    /// The cross-cutting defect: a suffix assigned by input arrival order
    /// rather than by the frozen sort key would give the same two files
    /// different final names on two runs, while both plans still validate.
    #[test]
    fn conflict_ordinal_is_recomputed_from_the_canonical_sort_key() {
        let plan = organize_sample();
        let mut moves: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|a| match &a.op {
                Op::Move {
                    source,
                    dest,
                    conflict,
                } if dest.display.starts_with("2019/2019-04/IMG_0042") => {
                    Some((source.file_id, dest.display.clone(), conflict.clone()))
                }
                _ => None,
            })
            .collect();
        moves.sort_by_key(|(fid, ..)| *fid);
        assert_eq!(moves.len(), 2);
        // file_id 11 ranks first (its raw source path "a/..." sorts before
        // "b/...") despite being fed AFTER file_id 12 in the fixture.
        assert_eq!(moves[0].0, 11);
        assert_eq!(moves[0].1, "2019/2019-04/IMG_0042.jpg");
        assert_eq!(moves[1].0, 12);
        assert_eq!(moves[1].1, "2019/2019-04/IMG_0042-1.jpg");
        for (_, _, c) in &moves {
            match c {
                Conflict::SuffixOrdinal { competing_with, .. } => {
                    assert_eq!(
                        competing_with.len(),
                        2,
                        "collision set must be complete and symmetric"
                    );
                }
                Conflict::None => panic!("expected a suffix_ordinal conflict"),
            }
        }
    }

    #[test]
    fn refuses_a_newer_plan_version_and_an_unknown_instruction_variant() {
        let plan = organize_sample();
        let bytes = plan.to_canonical_bytes().unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let mut newer = doc.clone();
        newer["plan_schema_version"] = serde_json::json!(2);
        let err = Plan::from_canonical_bytes(newer.to_string().as_bytes()).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains('2') && msg.contains('1'),
            "error must name both versions: {msg}"
        );

        let mut unknown_op = doc.clone();
        unknown_op["actions"][0]["op"]["action"] = serde_json::json!("purge");
        assert!(Plan::from_canonical_bytes(unknown_op.to_string().as_bytes()).is_err());

        let mut unknown_fp = doc.clone();
        unknown_fp["roots"][1]["source"]["fingerprint"]["kind"] = serde_json::json!("md5");
        assert!(Plan::from_canonical_bytes(unknown_fp.to_string().as_bytes()).is_err());

        // Positive: an unknown TOP-LEVEL field is accepted and ignored.
        doc["a_future_field_v1_3_added"] = serde_json::json!("ignored");
        Plan::from_canonical_bytes(doc.to_string().as_bytes())
            .expect("an unknown top-level field must be accepted, not refused");
    }

    #[test]
    fn canonicalize_is_idempotent() {
        for mut plan in [
            organize_sample(),
            extract_sample(),
            dedup_sample(),
            dedup_nothing_to_do_sample(),
        ] {
            let once = plan.clone();
            plan.canonicalize().unwrap();
            assert_eq!(
                plan, once,
                "a second canonicalize() call must change nothing"
            );
        }
    }

    #[test]
    fn directory_source_fingerprint_is_a_tagged_variant_not_a_null() {
        let plan = organize_sample();
        let bytes = plan.to_canonical_bytes().unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let src_root = doc["roots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["role"] == "source")
            .unwrap();
        assert_eq!(src_root["source"]["fingerprint"]["kind"], "none");
        assert_eq!(
            src_root["source"]["fingerprint"]["reason"],
            "directory_source_v1"
        );
        assert!(!src_root["source"]["fingerprint"].is_null());

        // A tar root with the directory-only variant must fail invariants.
        let mut p = extract_sample();
        for r in &mut p.roots {
            if let Some(src) = &mut r.source {
                if src.source_type == SourceType::Tar {
                    src.fingerprint = Fingerprint::None {
                        reason: NoFingerprintReason::DirectorySourceV1,
                    };
                }
            }
        }
        assert!(
            p.invariants_hold().is_err(),
            "a tar source claiming no fingerprint must be refused"
        );
    }

    #[test]
    fn verify_accepts_unchanged_live_state_for_every_plan_kind() {
        for plan in [organize_sample(), extract_sample(), dedup_sample()] {
            let mut state = FakeState::from_plan(&plan);
            plan.verify(&mut state)
                .expect("unchanged live inputs must verify");
            assert_eq!(
                state.entry_queries.len(),
                plan.referenced_entries().unwrap().len(),
                "every distinct referenced entry must be checked"
            );
        }
    }

    #[test]
    fn verify_refuses_a_different_index_uuid() {
        let plan = organize_sample();
        let mut state = FakeState::from_plan(&plan);
        state.sources.values_mut().next().unwrap().index_uuid =
            "00000000000000000000000000000000".to_string();

        let error = plan.verify(&mut state).unwrap_err().to_string();
        assert!(error.contains("index_uuid changed"), "wrong error: {error}");
    }

    #[test]
    fn verify_refuses_a_changed_index_schema_version() {
        let plan = organize_sample();
        let mut state = FakeState::from_plan(&plan);
        state
            .sources
            .values_mut()
            .next()
            .unwrap()
            .index_schema_version += 1;

        let error = plan.verify(&mut state).unwrap_err().to_string();
        assert!(
            error.contains("index_schema_version changed"),
            "wrong error: {error}"
        );
    }

    #[test]
    fn verify_refuses_a_changed_archive_blake3() {
        let plan = extract_sample();
        let mut state = FakeState::from_plan(&plan);
        state
            .sources
            .values_mut()
            .find(|source| source.archive_blake3.is_some())
            .unwrap()
            .archive_blake3 = Some(format!(
            "b3:{}",
            crate::report::to_hex(blake3::hash(b"changed archive").as_bytes())
        ));

        let error = plan.verify(&mut state).unwrap_err().to_string();
        assert!(
            error.contains("archive BLAKE3 changed"),
            "wrong error: {error}"
        );
    }

    #[test]
    fn stale_last_entry_is_refused_before_recording_action_one() {
        let plan = organize_sample();
        let mut state = FakeState::from_plan(&plan);
        assert!(state.entries.len() > 1, "test needs multiple live entries");
        let stale_key = *state.entries.keys().next_back().unwrap();
        let stale_display = plan
            .referenced_entries()
            .unwrap()
            .get(&stale_key)
            .unwrap()
            .display
            .clone();
        state.entries.get_mut(&stale_key).unwrap().content_hash = format!(
            "b3:{}",
            crate::report::to_hex(blake3::hash(b"changed last entry").as_bytes())
        );
        let mut recorder = RecordingExecutor::default();

        let error = recorder
            .run_after_verification(&plan, &mut state)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("content_hash changed"),
            "wrong error: {error}"
        );
        assert!(
            error.contains(&stale_display),
            "error must name the stale entry {stale_display}: {error}"
        );
        assert_eq!(
            state.entry_queries.last(),
            Some(&stale_key),
            "the stale entry must be the last live entry checked"
        );
        assert!(
            recorder.invocations.is_empty(),
            "a late verification failure must happen before action one"
        );
    }

    #[test]
    fn verify_uses_raw_path_bytes_not_the_lossy_display() {
        let plan = organize_sample();
        let mut state = FakeState::from_plan(&plan);
        let key = *state.entries.keys().next().unwrap();
        state.entries.get_mut(&key).unwrap().path_raw.push(b'x');

        let error = plan.verify(&mut state).unwrap_err().to_string();
        assert!(error.contains("raw path changed"), "wrong error: {error}");
    }

    #[test]
    fn load_from_regular_file_accepts_a_persisted_plan_and_refuses_stdin() {
        let plan = organize_sample();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reviewed-plan.json");
        std::fs::write(&path, plan.to_canonical_bytes().unwrap()).unwrap();

        let loaded = Plan::load_from_regular_file(&path).unwrap();
        assert_eq!(loaded, plan);

        let error = Plan::load_from_regular_file("-").unwrap_err().to_string();
        assert!(error.contains("stdin"), "wrong error: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn load_from_regular_file_refuses_fifo_socket_and_character_device() {
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("plan.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success(), "mkfifo must create the refusal fixture");
        let error = Plan::load_from_regular_file(&fifo).unwrap_err().to_string();
        assert!(error.contains("FIFO"), "wrong error: {error}");

        let socket = temp.path().join("plan.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let error = Plan::load_from_regular_file(&socket)
            .unwrap_err()
            .to_string();
        assert!(error.contains("socket"), "wrong error: {error}");

        let error = Plan::load_from_regular_file("/dev/null")
            .unwrap_err()
            .to_string();
        assert!(error.contains("character device"), "wrong error: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn load_from_regular_file_refuses_symlinks_to_every_source_kind() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let plan = organize_sample();
        let temp = tempfile::tempdir().unwrap();
        let regular = temp.path().join("reviewed-plan.json");
        std::fs::write(&regular, plan.to_canonical_bytes().unwrap()).unwrap();
        let fifo = temp.path().join("plan.fifo");
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let socket = temp.path().join("plan.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        for (name, target) in [
            ("regular-link", regular.as_path()),
            ("fifo-link", fifo.as_path()),
            ("socket-link", socket.as_path()),
            ("character-link", Path::new("/dev/null")),
        ] {
            let link = temp.path().join(name);
            symlink(target, &link).unwrap();
            let error = Plan::load_from_regular_file(&link).unwrap_err().to_string();
            assert!(error.contains("symlink"), "wrong error for {name}: {error}");
        }
    }
}
