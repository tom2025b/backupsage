//! Cross-archive duplicate detection over a master catalog (real or the
//! throwaway in-memory one).
//!
//! Exact groups: `GROUP BY content_hash` over the filtered scope. Near
//! groups (images): multi-index hashing — the 64-bit pHash is cut into four
//! 16-bit bands; at hamming distance ≤ 3 at least one band matches exactly
//! (pigeonhole), so bucketing by band value finds every candidate pair with
//! zero recall loss. Buckets are iterated and verified with popcounts —
//! never a SQL band self-join, which goes quadratic on skewed corpora.

use std::collections::HashMap;

use anyhow::{bail, Result};
use rusqlite::params;

use crate::master::{Master, STATUS_DB_MISSING, STATUS_INCOMPLETE, STATUS_V2_LIMITED};
use crate::phash;
use crate::report::*;
use crate::store::flags;

/// Hamming distances above 3 would need 8 bands to keep the recall
/// guarantee; hard-capped in v1.0 rather than silently losing matches.
pub const MAX_THRESHOLD: u32 = 3;
pub const DEFAULT_BUCKET_CAP: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Wasted,
    Count,
    Newest,
}

#[derive(Debug, Clone)]
pub struct DedupParams {
    pub exact: bool,
    pub near: bool,
    /// Hamming threshold for near groups (0..=3).
    pub threshold: u32,
    /// Restrict to one kind (`image|raw|video|text|binary`); None = any.
    pub kind: Option<String>,
    /// Restrict to these extensions (lowercase, no dot).
    pub exts: Vec<String>,
    pub min_size: u64,
    pub path_glob: Option<String>,
    /// Restrict scope to these archive ids/labels; empty = all registered.
    pub archives: Vec<String>,
    /// Hide groups confined to a single archive.
    pub across_only: bool,
    pub include_empty: bool,
    pub sort: SortKey,
    pub limit_groups: Option<usize>,
    pub bucket_cap: usize,
}

impl Default for DedupParams {
    fn default() -> Self {
        DedupParams {
            exact: true,
            near: true,
            threshold: MAX_THRESHOLD,
            kind: None,
            exts: Vec::new(),
            min_size: 1,
            path_glob: None,
            archives: Vec::new(),
            across_only: false,
            include_empty: false,
            sort: SortKey::Wasted,
            limit_groups: None,
            bucket_cap: DEFAULT_BUCKET_CAP,
        }
    }
}

/// One master `files` row joined with its archive, plus derived fields.
#[derive(Debug, Clone)]
struct Row {
    archive_id: i64,
    archive_label: String,
    archive_indexed_unix: Option<i64>,
    file_id: i64,
    path: String,
    entry_type: String,
    kind: String,
    size: u64,
    mtime_unix: Option<i64>,
    exif_unix: Option<i64>,
    exif_src: Option<String>,
    content_hash: Option<Vec<u8>>,
    phash: Option<i64>,
    img_w: Option<u32>,
    img_h: Option<u32>,
    flags: i64,
}

impl Row {
    fn shadowed(&self) -> bool {
        self.flags & flags::SHADOWED != 0
    }
    fn sparse(&self) -> bool {
        self.flags & flags::SPARSE != 0
    }
    fn is_hardlink(&self) -> bool {
        self.entry_type == "hardlink"
    }
    /// (timestamp, source-tag) under the fixed precedence
    /// EXIF > tar/fs mtime > archive index date.
    fn best_ts(&self) -> (Option<i64>, String) {
        if let Some(t) = self.exif_unix {
            let src = self
                .exif_src
                .as_deref()
                .map(|s| format!("exif:{s}"))
                .unwrap_or_else(|| "exif".into());
            (Some(t), src)
        } else if let Some(t) = self.mtime_unix {
            (Some(t), "tar-mtime".into())
        } else if let Some(t) = self.archive_indexed_unix {
            (Some(t), "archive-date".into())
        } else {
            (None, "none".into())
        }
    }
}

pub fn run_dedup(master: &Master, p: &DedupParams) -> Result<DedupReport> {
    if p.threshold > MAX_THRESHOLD {
        bail!(
            "--threshold is capped at {MAX_THRESHOLD} in v1.0: four 16-bit bands only \
             guarantee full recall up to distance {MAX_THRESHOLD}"
        );
    }

    let registry = master.list()?;
    let scope_ids = resolve_scope(&registry, &p.archives)?;

    // Refuse to near-match across different pHash algorithms.
    if p.near {
        let mut algos: Vec<String> = registry
            .iter()
            .filter(|a| scope_ids.contains(&a.archive_id))
            .filter_map(|a| {
                master
                    .conn
                    .query_row(
                        "SELECT phash_algo FROM archives WHERE archive_id=?1",
                        [a.archive_id],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
            })
            .collect();
        algos.sort();
        algos.dedup();
        if algos.len() > 1 {
            bail!(
                "near-dup refused: archives were indexed with different pHash \
                 algorithms ({algos:?}) — re-index the older ones"
            );
        }
    }

    let rows = fetch_scope(master, p, &scope_ids)?;

    // Index rows by (archive_id, file_id) for group assembly.
    let mut by_key: HashMap<(i64, i64), usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        by_key.insert((r.archive_id, r.file_id), i);
    }

    let mut groups_raw: Vec<(String, Vec<usize>, u32)> = Vec::new(); // (kind, member idxs, max_dist)
    let mut near_buckets_skipped = 0u64;

    // ── Near groups (images with a pHash) ────────────────────────────────
    let mut in_near_group: Vec<bool> = vec![false; rows.len()];
    if p.near {
        let image_idxs: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.kind == "image"
                    && r.phash.is_some()
                    && !r.shadowed()
                    && !r.is_hardlink()
                    && !phash::is_trivial(r.phash.unwrap() as u64)
            })
            .map(|(i, _)| i)
            .collect();

        let pairs = mih_pairs(
            &image_idxs
                .iter()
                .map(|&i| rows[i].phash.unwrap() as u64)
                .collect::<Vec<_>>(),
            p.threshold,
            p.bucket_cap,
            &mut near_buckets_skipped,
        );

        let mut uf = UnionFind::new(image_idxs.len());
        for &(a, b) in &pairs {
            uf.union(a, b);
        }
        let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
        for (local, &global) in image_idxs.iter().enumerate() {
            clusters.entry(uf.find(local)).or_default().push(global);
        }
        for (_, members) in clusters {
            if members.len() > 1 {
                for &m in &members {
                    in_near_group[m] = true;
                }
                groups_raw.push(("near".into(), members, 0)); // max_dist filled later
            }
        }
    }

    // ── Exact groups ─────────────────────────────────────────────────────
    if p.exact {
        let mut by_hash: HashMap<&[u8], Vec<usize>> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            // Images already in a near group are grouped perceptually;
            // distance-0 members cover the exact-duplicate case there.
            if p.near && in_near_group[i] {
                continue;
            }
            if let Some(h) = &r.content_hash {
                by_hash.entry(h.as_slice()).or_default().push(i);
            }
        }
        for (_, members) in by_hash {
            // A group needs ≥2 real (non-hardlink, non-shadowed) files — a
            // file plus its own hardlink shares storage and is not a dup —
            // OR one real file plus shadowed twin(s), so intra-archive
            // shadow waste is surfaced. Hardlinks alone never form a group.
            let real = members
                .iter()
                .filter(|&&i| !rows[i].is_hardlink() && !rows[i].shadowed())
                .count();
            let shadowed = members.iter().filter(|&&i| rows[i].shadowed()).count();
            if real >= 2 || (real >= 1 && shadowed >= 1) {
                groups_raw.push(("exact".into(), members, 0));
            }
        }
    }

    // ── Assemble the report ──────────────────────────────────────────────
    let mut groups: Vec<Group> = Vec::new();
    let mut duplicate_files = 0usize;
    let mut reclaimable_total = 0u64;
    let mut shadowed_bytes = 0u64;

    for (kind, mut member_idxs, _) in groups_raw {
        member_idxs.sort_by_key(|&i| (rows[i].archive_id, rows[i].file_id));
        if p.across_only {
            let mut archs: Vec<i64> = member_idxs.iter().map(|&i| rows[i].archive_id).collect();
            archs.sort();
            archs.dedup();
            if archs.len() < 2 {
                continue;
            }
        }

        let keep_idx = pick_keep(&rows, &member_idxs, &kind);
        let (keep_i, keep_reason) = match keep_idx {
            Some(v) => v,
            None => continue, // no eligible keeper (all shadowed/hardlinks)
        };
        let keep_phash = rows[keep_i].phash;

        let mut members = Vec::new();
        let mut max_dist = 0u32;
        let mut reclaimable = 0u64;
        for &i in &member_idxs {
            let r = &rows[i];
            let hamming = match (keep_phash, r.phash) {
                (Some(a), Some(b)) if kind == "near" => Some(phash::hamming(a as u64, b as u64)),
                _ if kind == "exact" => Some(0),
                _ => None,
            };
            if let Some(d) = hamming {
                max_dist = max_dist.max(d);
            }
            let is_keep = i == keep_i;
            if !is_keep {
                if r.shadowed() {
                    shadowed_bytes += r.size;
                } else if !r.is_hardlink() {
                    reclaimable += r.size;
                    duplicate_files += 1;
                }
            }
            let (best_ts, ts_src) = r.best_ts();
            members.push(Member {
                archive_id: r.archive_id,
                archive_label: r.archive_label.clone(),
                file_id: r.file_id,
                path: r.path.clone(),
                kind: r.kind.clone(),
                size: r.size,
                content_hash: r.content_hash.as_ref().map(|h| format!("b3:{}", hex(h))),
                phash: r.phash.map(|p| format!("{:016x}", p as u64)),
                mtime_unix: r.mtime_unix,
                exif_unix: r.exif_unix,
                best_ts_unix: best_ts,
                best_ts_source: ts_src,
                width: r.img_w,
                height: r.img_h,
                hamming_to_keep: hamming,
                keep: is_keep,
                keep_reason: if is_keep {
                    Some(keep_reason.clone())
                } else {
                    None
                },
                shadowed: r.shadowed(),
                sparse: r.sparse(),
                hardlink_of: if r.is_hardlink() {
                    rows[i].link_target_hint()
                } else {
                    None
                },
            });
        }
        // Keep first, then by archive/path for stable output.
        members.sort_by_key(|m| (!m.keep, m.archive_id, m.file_id));
        reclaimable_total += reclaimable;
        groups.push(Group {
            group_id: 0, // assigned after sorting
            match_kind: kind,
            max_distance: max_dist,
            reclaimable_bytes: reclaimable,
            members,
        });
    }

    match p.sort {
        SortKey::Wasted => groups.sort_by_key(|g| std::cmp::Reverse(g.reclaimable_bytes)),
        SortKey::Count => groups.sort_by_key(|g| std::cmp::Reverse(g.members.len())),
        SortKey::Newest => groups.sort_by_key(|g| {
            std::cmp::Reverse(
                g.members
                    .iter()
                    .find(|m| m.keep)
                    .and_then(|m| m.best_ts_unix)
                    .unwrap_or(i64::MIN),
            )
        }),
    }
    if let Some(limit) = p.limit_groups {
        groups.truncate(limit);
    }
    for (i, g) in groups.iter_mut().enumerate() {
        g.group_id = i + 1;
    }

    // Scope-wide caveat counts.
    let images_without_phash: u64 = {
        let clause = scope_clause(&scope_ids);
        master.conn.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE kind='image' AND phash IS NULL {clause}"),
            [],
            |r| r.get::<_, i64>(0),
        )? as u64
    };

    let in_scope = |id: i64| scope_ids.contains(&id);
    let summary = Summary {
        groups: groups.len(),
        duplicate_files,
        reclaimable_bytes: reclaimable_total,
        archives_offline: registry
            .iter()
            .filter(|a| in_scope(a.archive_id) && a.status == STATUS_DB_MISSING)
            .map(|a| a.label.clone())
            .collect(),
        archives_incomplete: registry
            .iter()
            .filter(|a| in_scope(a.archive_id) && a.status == STATUS_INCOMPLETE)
            .map(|a| a.label.clone())
            .collect(),
        skipped_archives: registry
            .iter()
            .filter(|a| in_scope(a.archive_id) && a.status == STATUS_V2_LIMITED)
            .map(|a| {
                (
                    a.label.clone(),
                    format!(
                        "v2-limited — no hashes; run `backupsage index {}` to upgrade",
                        a.source_path
                    ),
                )
            })
            .collect(),
        images_without_phash,
        intra_archive_shadowed_bytes: shadowed_bytes,
        near_buckets_skipped,
    };

    Ok(DedupReport {
        version: 1,
        params: ReportParams {
            exact: p.exact,
            near: p.near,
            threshold: p.threshold,
            min_size: p.min_size,
            include_empty: p.include_empty,
            across_only: p.across_only,
            keep_policy: "exact:newest-then-clean-path; near:resolution-then-size-then-newest"
                .into(),
            images_grouped_perceptually: p.near,
        },
        archives: registry
            .iter()
            .filter(|a| in_scope(a.archive_id))
            .map(|a| ReportArchive {
                archive_id: a.archive_id,
                label: a.label.clone(),
                source: a.source_path.clone(),
                source_type: a.source_type.clone(),
                status: a.status.clone(),
            })
            .collect(),
        groups,
        summary,
    })
}

impl Row {
    /// For hardlink members the interesting name is the link target; the
    /// master does not replicate link_target, so surface the path itself —
    /// the CLI's inspect command has the full story in the source DB.
    fn link_target_hint(&self) -> Option<String> {
        Some(self.path.clone())
    }
}

// ── Scope resolution and fetching ────────────────────────────────────────────

fn resolve_scope(registry: &[crate::master::ArchiveRow], wanted: &[String]) -> Result<Vec<i64>> {
    if wanted.is_empty() {
        return Ok(registry.iter().map(|a| a.archive_id).collect());
    }
    let mut ids = Vec::new();
    for w in wanted {
        let found: Vec<i64> = registry
            .iter()
            .filter(|a| a.archive_id.to_string() == *w || a.label == *w || a.source_path == *w)
            .map(|a| a.archive_id)
            .collect();
        match found.as_slice() {
            [] => bail!("--archive '{w}' matches no registered archive"),
            [one] => ids.push(*one),
            many => bail!("--archive '{w}' is ambiguous ({many:?}); use the id"),
        }
    }
    Ok(ids)
}

fn scope_clause(scope_ids: &[i64]) -> String {
    let list = scope_ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("AND archive_id IN ({list})")
}

fn fetch_scope(master: &Master, p: &DedupParams, scope_ids: &[i64]) -> Result<Vec<Row>> {
    let mut sql = String::from(
        "SELECT f.archive_id, a.label, a.indexed_unix, f.file_id, f.path, f.entry_type,
                f.kind, f.size, f.mtime_unix, f.exif_unix, f.exif_src, f.content_hash,
                f.phash, f.img_w, f.img_h, f.flags
         FROM files f JOIN archives a ON a.archive_id = f.archive_id
         WHERE f.content_hash IS NOT NULL
           AND f.entry_type != 'symlink'
           AND (f.flags & ?1) = 0",
    );
    // Sparse entries: hash covers the condensed stream — never dedup them.
    let sparse_flag = flags::SPARSE;
    sql.push_str(&format!(
        " AND f.archive_id IN ({})",
        scope_ids
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    ));
    if !p.include_empty {
        sql.push_str(" AND f.kind != 'empty'");
    }
    sql.push_str(" AND f.size >= ?2");
    if p.kind.is_some() {
        sql.push_str(" AND f.kind = ?3");
    } else {
        sql.push_str(" AND (?3 IS NULL OR 1)");
    }
    if let Some(_glob) = &p.path_glob {
        sql.push_str(" AND f.path GLOB ?4");
    } else {
        sql.push_str(" AND (?4 IS NULL OR 1)");
    }
    if !p.exts.is_empty() {
        let ors: Vec<String> = p
            .exts
            .iter()
            .map(|e| {
                let clean: String = e
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_lowercase();
                format!("lower(f.path) LIKE '%.{clean}'")
            })
            .collect();
        sql.push_str(&format!(" AND ({})", ors.join(" OR ")));
    }

    let mut stmt = master.conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            params![
                sparse_flag,
                p.min_size as i64,
                p.kind.as_deref(),
                p.path_glob.as_deref()
            ],
            |r| {
                Ok(Row {
                    archive_id: r.get(0)?,
                    archive_label: r.get(1)?,
                    archive_indexed_unix: r.get(2)?,
                    file_id: r.get(3)?,
                    path: r.get(4)?,
                    entry_type: r.get(5)?,
                    kind: r.get(6)?,
                    size: r.get::<_, i64>(7)? as u64,
                    mtime_unix: r.get(8)?,
                    exif_unix: r.get(9)?,
                    exif_src: r.get(10)?,
                    content_hash: r.get(11)?,
                    phash: r.get(12)?,
                    img_w: r.get(13)?,
                    img_h: r.get(14)?,
                    flags: r.get(15)?,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ── Multi-index hashing ──────────────────────────────────────────────────────

/// All pairs (by local index) within `threshold` hamming distance.
/// Bucket iteration per 16-bit band; buckets over `cap` are skipped and
/// counted — a warning surfaces in the report summary.
fn mih_pairs(hashes: &[u64], threshold: u32, cap: usize, skipped: &mut u64) -> Vec<(usize, usize)> {
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    let mut seen: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    for band in 0..4u32 {
        let shift = 48 - band * 16;
        let mut buckets: HashMap<u16, Vec<usize>> = HashMap::new();
        for (i, &h) in hashes.iter().enumerate() {
            buckets
                .entry(((h >> shift) & 0xFFFF) as u16)
                .or_default()
                .push(i);
        }
        for (_, bucket) in buckets {
            if bucket.len() < 2 {
                continue;
            }
            if bucket.len() > cap {
                *skipped += 1;
                continue;
            }
            for x in 0..bucket.len() {
                for y in (x + 1)..bucket.len() {
                    let (a, b) = (bucket[x], bucket[y]);
                    if phash::hamming(hashes[a], hashes[b]) <= threshold && seen.insert((a, b)) {
                        pairs.push((a, b));
                    }
                }
            }
        }
    }
    pairs
}

// ── Keep policy ──────────────────────────────────────────────────────────────

/// Pick the member to keep. Shadowed rows and hardlinks are never eligible.
/// Exact: newest best-timestamp → clean path → shortest path → lowest id.
/// Near: highest pixel count → largest size → newest → lowest id.
fn pick_keep(rows: &[Row], members: &[usize], kind: &str) -> Option<(usize, String)> {
    let eligible: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&i| !rows[i].shadowed() && !rows[i].is_hardlink())
        .collect();
    if eligible.is_empty() {
        return None;
    }
    let best = if kind == "near" {
        *eligible
            .iter()
            .max_by_key(|&&i| {
                let r = &rows[i];
                (
                    r.img_w.unwrap_or(0) as u64 * r.img_h.unwrap_or(0) as u64,
                    r.size,
                    r.best_ts().0.unwrap_or(i64::MIN),
                    std::cmp::Reverse((r.archive_id, r.file_id)),
                )
            })
            .unwrap()
    } else {
        *eligible
            .iter()
            .max_by_key(|&&i| {
                let r = &rows[i];
                (
                    r.best_ts().0.unwrap_or(i64::MIN),
                    !has_conflict_marker(&r.path),
                    std::cmp::Reverse(r.path.len()),
                    std::cmp::Reverse((r.archive_id, r.file_id)),
                )
            })
            .unwrap()
    };
    let reason = if kind == "near" {
        let r = &rows[best];
        if r.img_w.is_some() {
            "highest-resolution"
        } else {
            "largest"
        }
    } else {
        let r = &rows[best];
        let newest = eligible
            .iter()
            .all(|&i| rows[i].best_ts().0.unwrap_or(i64::MIN) <= r.best_ts().0.unwrap_or(i64::MIN));
        let distinct_ts = eligible
            .iter()
            .any(|&i| rows[i].best_ts().0 != r.best_ts().0);
        if newest && distinct_ts {
            "newest"
        } else {
            "clean-path"
        }
    };
    Some((best, reason.to_string()))
}

/// Paths that look like copy artifacts: `img (1).jpg`, `img_1.jpg`,
/// `img copy.jpg`.
fn has_conflict_marker(path: &str) -> bool {
    let stem = std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if stem.contains("copy") {
        return true;
    }
    if stem.ends_with(')') {
        if let Some(open) = stem.rfind('(') {
            if stem[open + 1..stem.len() - 1]
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                return true;
            }
        }
    }
    if let Some(pos) = stem.rfind('_') {
        let tail = &stem[pos + 1..];
        if !tail.is_empty() && tail.len() <= 3 && tail.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── Union-find ───────────────────────────────────────────────────────────────

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]]; // path halving
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MIH must find exactly the pairs a brute-force scan finds — the
    /// recall proof behind the whole near-dup design.
    #[test]
    fn mih_equals_brute_force_on_random_corpus() {
        // Deterministic xorshift so the corpus is reproducible.
        let mut state = 0x243F_6A88_85A3_08D3u64;
        let mut rand = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Base hashes plus planted near-duplicates at distances 1..=3.
        let mut hashes: Vec<u64> = (0..2000).map(|_| rand()).collect();
        for i in 0..200 {
            let base = hashes[i * 7];
            let flips = (i % 3) + 1;
            let mut v = base;
            for f in 0..flips {
                v ^= 1u64 << ((i * 11 + f * 23) % 64);
            }
            hashes.push(v);
        }

        for threshold in 0..=3u32 {
            let mut skipped = 0;
            let mut mih: Vec<(usize, usize)> =
                mih_pairs(&hashes, threshold, DEFAULT_BUCKET_CAP, &mut skipped);
            mih.sort_unstable();

            let mut brute: Vec<(usize, usize)> = Vec::new();
            for a in 0..hashes.len() {
                for b in (a + 1)..hashes.len() {
                    if phash::hamming(hashes[a], hashes[b]) <= threshold {
                        brute.push((a, b));
                    }
                }
            }
            brute.sort_unstable();
            assert_eq!(mih, brute, "recall mismatch at threshold {threshold}");
            assert_eq!(skipped, 0);
        }
    }

    #[test]
    fn bucket_cap_skips_and_counts() {
        // 20 001 identical hashes: one bucket per band, all over a cap of 10.
        let hashes = vec![0xABCD_1234_5678_9ABCu64; 20_001];
        let mut skipped = 0;
        let pairs = mih_pairs(&hashes, 3, 10, &mut skipped);
        assert!(pairs.is_empty());
        assert_eq!(skipped, 4); // one oversized bucket in each of 4 bands
    }

    #[test]
    fn conflict_markers_detected() {
        assert!(has_conflict_marker("dl/IMG_2041 (1).jpg"));
        assert!(has_conflict_marker("x/photo_2.png"));
        assert!(has_conflict_marker("a/img copy.jpg"));
        assert!(!has_conflict_marker("DCIM/IMG_0142.JPG")); // camera numbering: 4 digits
        assert!(!has_conflict_marker("2023/07/shot.jpg"));
    }

    #[test]
    fn union_find_components() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(3, 4);
        uf.union(1, 3);
        assert_eq!(uf.find(0), uf.find(4));
        assert_ne!(uf.find(0), uf.find(2));
    }
}
