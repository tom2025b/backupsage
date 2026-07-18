//! Dedup end-to-end over real generated archives: exact cross-archive
//! groups, perceptual near groups, keep policies, exclusion semantics and
//! the JSON contract.

mod common;

use std::path::Path;

use backupsage::dedup::{run_dedup, DedupParams};
use backupsage::indexer::{self, IndexOptions};
use backupsage::master::{self, Master};
use common::*;

fn index_archive(dir: &Path, name: &str, files: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
    index_archive_mtime(dir, name, files, 1_700_000_001)
}

fn index_archive_mtime(
    dir: &Path,
    name: &str,
    files: &[(&str, Vec<u8>)],
    mtime: u64,
) -> std::path::PathBuf {
    let archive = write_archive(dir, name, &build_tar_mtime(files, mtime));
    indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path
}

fn master_of(dir: &Path, dbs: &[&std::path::PathBuf]) -> Master {
    let mut m = master::open_at(&dir.join("master.db")).unwrap();
    for db in dbs {
        m.add(db).unwrap();
    }
    m
}

#[test]
fn exact_duplicates_across_archives_with_different_names() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"identical bytes in every copy".to_vec();
    // Newer copy (higher mtime) lives in archive b under a different name.
    let db_a = index_archive_mtime(
        dir.path(),
        "old-laptop.tar",
        &[
            ("backup/report.pdf", payload.clone()),
            ("unrelated.txt", b"only here".to_vec()),
        ],
        1_600_000_000,
    );
    let db_b = index_archive_mtime(
        dir.path(),
        "new-backup.tar",
        &[("docs/final-report.pdf", payload.clone())],
        1_700_000_000,
    );

    let m = master_of(dir.path(), &[&db_a, &db_b]);
    let report = run_dedup(&m, &DedupParams::default()).unwrap();

    assert_eq!(report.summary.groups, 1);
    assert_eq!(report.summary.duplicate_files, 1);
    assert_eq!(report.summary.reclaimable_bytes, payload.len() as u64);

    let g = &report.groups[0];
    assert_eq!(g.match_kind, "exact");
    assert_eq!(g.members.len(), 2);
    let keep = g.members.iter().find(|m| m.keep).unwrap();
    // Same content, different names — the newer mtime wins.
    assert_eq!(keep.path, "docs/final-report.pdf");
    assert_eq!(keep.archive_label, "new-backup.tar");
    assert_eq!(keep.keep_reason.as_deref(), Some("newest"));
    assert_eq!(keep.best_ts_source, "tar-mtime");
    assert!(keep.content_hash.as_deref().unwrap().starts_with("b3:"));
}

#[test]
fn near_duplicate_images_group_perceptually() {
    let dir = tempfile::tempdir().unwrap();
    let original = png_bytes(11, 320, 240);
    let brightened = png_bytes_brightened(11, 320, 240, 8);
    // Sanity: different bytes.
    assert_ne!(original, brightened);

    let db_a = index_archive(dir.path(), "photos-a.tar", &[("2023/shot.png", original)]);
    let db_b = index_archive(
        dir.path(),
        "photos-b.tar",
        &[("export/shot-edit.png", brightened)],
    );
    let m = master_of(dir.path(), &[&db_a, &db_b]);
    let report = run_dedup(&m, &DedupParams::default()).unwrap();

    assert_eq!(
        report.summary.groups, 1,
        "expected one near group, got: {}",
        report.to_json()
    );
    let g = &report.groups[0];
    assert_eq!(g.match_kind, "near");
    assert!(g.max_distance <= 3);
    let keep = g.members.iter().find(|m| m.keep).unwrap();
    assert_eq!(keep.keep_reason.as_deref(), Some("highest-resolution"));
    let dup = g.members.iter().find(|m| !m.keep).unwrap();
    assert!(dup.hamming_to_keep.unwrap() <= 3);
    assert!(dup.width.is_some() && dup.height.is_some());

    // With --exact-only the two different-byte images do not group at all.
    let exact_only = DedupParams {
        near: false,
        ..DedupParams::default()
    };
    let r2 = run_dedup(&m, &exact_only).unwrap();
    assert_eq!(r2.summary.groups, 0);
}

#[test]
fn identical_images_stay_one_group_when_near_enabled() {
    // Distance-0 images must appear once (in the near group), never doubled
    // as an extra exact group.
    let dir = tempfile::tempdir().unwrap();
    let img = png_bytes(5, 200, 150);
    let db_a = index_archive(dir.path(), "a.tar", &[("x/p.png", img.clone())]);
    let db_b = index_archive(dir.path(), "b.tar", &[("y/q.png", img.clone())]);
    let m = master_of(dir.path(), &[&db_a, &db_b]);

    let report = run_dedup(&m, &DedupParams::default()).unwrap();
    assert_eq!(report.summary.groups, 1);
    assert_eq!(report.groups[0].match_kind, "near");
    assert_eq!(report.groups[0].max_distance, 0);
    assert_eq!(report.summary.duplicate_files, 1);
}

#[test]
fn empty_files_hardlinks_and_single_archive_filters() {
    let dir = tempfile::tempdir().unwrap();

    // Two empty files in different archives: excluded by default.
    let db_a = index_archive(dir.path(), "ea.tar", &[("a/zero1", Vec::new())]);
    let db_b = index_archive(dir.path(), "eb.tar", &[("b/zero2", Vec::new())]);
    let m = master_of(dir.path(), &[&db_a, &db_b]);
    let report = run_dedup(&m, &DedupParams::default()).unwrap();
    assert_eq!(report.summary.groups, 0, "empty files must not group");

    let with_empty = DedupParams {
        include_empty: true,
        min_size: 0,
        ..DedupParams::default()
    };
    let r2 = run_dedup(&m, &with_empty).unwrap();
    assert_eq!(r2.summary.groups, 1, "--include-empty opts back in");

    // A file plus its own hardlink is NOT a duplicate group.
    let mut builder = tar::Builder::new(Vec::new());
    let data = b"hardlinked payload".to_vec();
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    builder.append_data(&mut h, "f/original", data.as_slice()).unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    link.set_cksum();
    builder.append_link(&mut link, "f/alias", "f/original").unwrap();
    let archive = write_archive(dir.path(), "hl.tar", &builder.into_inner().unwrap());
    let db_hl = indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path;
    let (m2, _) = master::open_in_memory(std::slice::from_ref(&db_hl)).unwrap();
    let r3 = run_dedup(&m2, &DedupParams::default()).unwrap();
    assert_eq!(
        r3.summary.groups, 0,
        "file + own hardlink shares storage: {}",
        r3.to_json()
    );
}

#[test]
fn across_only_and_archive_scope_filters() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"duplicated within one archive".to_vec();
    let db_a = index_archive(
        dir.path(),
        "solo.tar",
        &[
            ("x/copy1.dat", payload.clone()),
            ("y/copy2.dat", payload.clone()),
        ],
    );
    let db_b = index_archive(dir.path(), "other.tar", &[("z/unique.dat", b"one".to_vec())]);
    let m = master_of(dir.path(), &[&db_a, &db_b]);

    // Intra-archive dupes show by default…
    let all = run_dedup(&m, &DedupParams::default()).unwrap();
    assert_eq!(all.summary.groups, 1);
    // …and disappear under --across-only.
    let across = DedupParams {
        across_only: true,
        ..DedupParams::default()
    };
    assert_eq!(run_dedup(&m, &across).unwrap().summary.groups, 0);

    // Archive scope by label.
    let scoped = DedupParams {
        archives: vec!["solo.tar".into()],
        ..DedupParams::default()
    };
    let r = run_dedup(&m, &scoped).unwrap();
    assert_eq!(r.archives.len(), 1);
    assert_eq!(r.summary.groups, 1);

    // Unknown archive → error.
    let bad = DedupParams {
        archives: vec!["nope.tar".into()],
        ..DedupParams::default()
    };
    assert!(run_dedup(&m, &bad).is_err());
}

#[test]
fn shadowed_rows_never_keep_and_report_their_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"the same content again".to_vec();

    // Archive with a shadowed path whose content equals a file in another
    // archive: the shadowed row joins the group but must not be KEEP, and
    // its bytes land in intra_archive_shadowed_bytes.
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data, mtime) in [
        ("etc/config", payload.clone(), 1_710_000_000u64), // will be shadowed
        ("etc/config", b"different final version".to_vec(), 1_700_000_000),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(mtime);
        h.set_cksum();
        builder.append_data(&mut h, path, data.as_slice()).unwrap();
    }
    let archive = write_archive(dir.path(), "shadow.tar", &builder.into_inner().unwrap());
    let db_a = indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path;
    let db_b = index_archive_mtime(
        dir.path(),
        "plain.tar",
        &[("data/copy.bin", payload.clone())],
        1_600_000_000,
    );

    let m = master_of(dir.path(), &[&db_a, &db_b]);
    let report = run_dedup(&m, &DedupParams::default()).unwrap();

    assert_eq!(report.summary.groups, 1, "{}", report.to_json());
    let g = &report.groups[0];
    let keep = g.members.iter().find(|m| m.keep).unwrap();
    // The shadowed copy has the newest mtime but is ineligible.
    assert!(!keep.shadowed);
    assert_eq!(keep.path, "data/copy.bin");
    assert!(g.members.iter().any(|m| m.shadowed));
    assert_eq!(
        report.summary.intra_archive_shadowed_bytes,
        payload.len() as u64
    );
    // Shadowed bytes are not double-counted as reclaimable.
    assert_eq!(report.summary.reclaimable_bytes, 0);
    assert_eq!(report.summary.duplicate_files, 0);
}

#[test]
fn json_contract_field_names_are_stable() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"contract test payload".to_vec();
    let db_a = index_archive(dir.path(), "c1.tar", &[("f1.dat", payload.clone())]);
    let db_b = index_archive(dir.path(), "c2.tar", &[("f2.dat", payload)]);
    let (m, _) = master::open_in_memory(&[db_a, db_b]).unwrap();
    let report = run_dedup(&m, &DedupParams::default()).unwrap();

    let v: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(v["version"], 1);
    for key in ["params", "archives", "groups", "summary"] {
        assert!(v.get(key).is_some(), "missing top-level key {key}");
    }
    let group = &v["groups"][0];
    for key in ["group_id", "match_kind", "max_distance", "reclaimable_bytes", "members"] {
        assert!(group.get(key).is_some(), "missing group key {key}");
    }
    let member = &group["members"][0];
    for key in [
        "archive_id", "archive_label", "file_id", "path", "kind", "size",
        "content_hash", "phash", "mtime_unix", "exif_unix", "best_ts_unix",
        "best_ts_source", "width", "height", "hamming_to_keep", "keep",
        "keep_reason", "shadowed", "sparse", "hardlink_of",
    ] {
        assert!(member.get(key).is_some(), "missing member key {key}");
    }
    for key in [
        "groups", "duplicate_files", "reclaimable_bytes", "archives_offline",
        "archives_incomplete", "skipped_archives", "images_without_phash",
        "intra_archive_shadowed_bytes", "near_buckets_skipped",
    ] {
        assert!(v["summary"].get(key).is_some(), "missing summary key {key}");
    }
}
