//! Master catalog end-to-end: registration, replication, both staleness
//! layers, v2-limited registration, move re-registration.

mod common;

use std::fs;
use std::path::Path;

use backupsage::indexer::{self, IndexOptions};
use backupsage::master::{self, AddOutcome, Master};
use common::*;

fn index_archive(dir: &Path, name: &str, files: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
    let archive = write_archive(dir, name, &build_tar(files));
    indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path
}

fn open_master(dir: &Path) -> Master {
    master::open_at(&dir.join("master.db")).unwrap()
}

fn files_in_master(m: &Master, label: &str) -> i64 {
    m.conn
        .query_row(
            "SELECT COUNT(*) FROM files f JOIN archives a ON a.archive_id=f.archive_id
             WHERE a.label=?1",
            [label],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn add_replicates_metadata_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = index_archive(
        dir.path(),
        "photos.tar",
        &[
            ("a.txt", b"alpha".to_vec()),
            ("b.bin", b"\x00\x01\x02".to_vec()),
        ],
    );
    let mut m = open_master(dir.path());

    match m.add(&db).unwrap() {
        AddOutcome::Replicated { label, files } => {
            assert_eq!(label, "photos.tar");
            assert_eq!(files, 2);
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    assert_eq!(files_in_master(&m, "photos.tar"), 2);
    let rows = m.list().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "ok");
    assert_eq!(rows[0].schema_version, 3);

    // `add` accepts the archive path too, resolving the sibling .db, and
    // re-adding is idempotent (update, not duplicate).
    m.add(&dir.path().join("photos.tar")).unwrap();
    assert_eq!(m.list().unwrap().len(), 1);
}

#[test]
fn sync_detects_rebuild_and_missing_db() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(
        dir.path(),
        "sys.tar",
        &build_tar(&[("one.txt", b"first".to_vec())]),
    );
    let db = indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path;
    let mut m = open_master(dir.path());
    m.add(&db).unwrap();
    let uuid_before = m.list().unwrap()[0].index_uuid.clone();

    // Rebuild the index (new uuid, new content) → sync re-replicates.
    let bigger = build_tar(&[
        ("one.txt", b"first".to_vec()),
        ("two.txt", b"second".to_vec()),
    ]);
    fs::write(&archive, &bigger).unwrap();
    indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    let actions = m.sync(None).unwrap();
    assert!(
        actions.iter().any(|(_, a)| a == "re-replicated"),
        "{actions:?}"
    );
    let row = &m.list().unwrap()[0];
    assert_ne!(row.index_uuid, uuid_before);
    assert_eq!(row.files_count, 2);
    assert_eq!(files_in_master(&m, "sys.tar"), 2);

    // Delete the .db → db-missing, but replica rows survive for dedup.
    fs::remove_file(&db).unwrap();
    m.sync(None).unwrap();
    assert_eq!(m.list().unwrap()[0].status, "db-missing");
    assert_eq!(files_in_master(&m, "sys.tar"), 2);
}

#[test]
fn verify_flags_stale_index_and_deep_catches_content_change() {
    let dir = tempfile::tempdir().unwrap();
    let tar1 = build_tar_mtime(&[("x.txt", b"original".to_vec())], 1_600_000_000);
    let archive = write_archive(dir.path(), "b.tar", &tar1);
    let db = indexer::run_index(&archive, None, &IndexOptions::default())
        .unwrap()
        .db_path;
    let mut m = open_master(dir.path());
    m.add(&db).unwrap();
    assert_eq!(m.verify(false).unwrap()[0].status, "ok");

    // Same-length content change + restored mtime: cheap stat check can't
    // see it; --deep re-hashes and must.
    let orig_meta = fs::metadata(&archive).unwrap();
    let orig_mtime = orig_meta.modified().unwrap();
    let mut tar2 = tar1.clone();
    // Flip one content byte inside the data block (offset 512..) without
    // changing the length.
    tar2[512] ^= 0xFF;
    fs::write(&archive, &tar2).unwrap();
    let f = fs::File::options().write(true).open(&archive).unwrap();
    f.set_modified(orig_mtime).unwrap();
    drop(f);

    assert_eq!(
        m.verify(false).unwrap()[0].status,
        "ok",
        "stat-only verify cannot detect a same-size in-place edit"
    );
    assert_eq!(m.verify(true).unwrap()[0].status, "stale-index");

    // Archive removed entirely → archive-missing.
    fs::remove_file(&archive).unwrap();
    assert_eq!(m.verify(false).unwrap()[0].status, "archive-missing");
}

#[test]
fn v2_databases_register_limited_and_moves_reregister() {
    let dir = tempfile::tempdir().unwrap();

    // Fabricate a v2 index: v2 schema shape, meta without index_uuid.
    let v2 = dir.path().join("old.tar.db");
    {
        let conn = rusqlite::Connection::open(&v2).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE files_fts USING fts5(path, content, tokenize='unicode61');
             CREATE TABLE word_freq(word TEXT PRIMARY KEY,
                                    total_count INTEGER NOT NULL DEFAULT 0,
                                    doc_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version','2'),
                                     ('archive','/backups/old.tar'),
                                     ('created_unix','1600000000'),
                                     ('completed','1');
             INSERT INTO files_fts(path, content) VALUES ('etc/conf', 'legacy text');",
        )
        .unwrap();
    }
    let mut m = open_master(dir.path());
    match m.add(&v2).unwrap() {
        AddOutcome::V2Limited { label } => assert_eq!(label, "old.tar"),
        other => panic!("unexpected: {other:?}"),
    }
    let row = m.list().unwrap()[0].clone();
    assert_eq!(row.status, "v2-limited");
    assert_eq!(row.files_count, 0);
    assert!(row.index_uuid.starts_with("v2:"));

    // Move the .db: synthesized uid keys on meta.archive, so re-adding the
    // moved file updates the row instead of creating a ghost duplicate.
    let moved = dir.path().join("renamed.db");
    fs::rename(&v2, &moved).unwrap();
    m.add(&moved).unwrap();
    let rows = m.list().unwrap();
    assert_eq!(rows.len(), 1, "move must not duplicate the registration");
    assert!(rows[0].db_path.ends_with("renamed.db"));

    // v3 move: same story via the real index_uuid.
    let db3 = index_archive(dir.path(), "new.tar", &[("f.txt", b"data".to_vec())]);
    m.add(&db3).unwrap();
    let moved3 = dir.path().join("new-moved.db");
    fs::rename(&db3, &moved3).unwrap();
    m.add(&moved3).unwrap();
    let rows = m.list().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[1].db_path.ends_with("new-moved.db"));
}

#[test]
fn incomplete_indexes_carry_status_and_rm_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let db = index_archive(dir.path(), "ok.tar", &[("a.txt", b"data".to_vec())]);

    // Force completed=0 (as an interrupted build would leave it).
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("UPDATE meta SET value='0' WHERE key='completed'", [])
            .unwrap();
    }
    let mut m = open_master(dir.path());
    m.add(&db).unwrap();
    assert_eq!(m.list().unwrap()[0].status, "incomplete");

    m.rm("ok.tar").unwrap();
    assert!(m.list().unwrap().is_empty());
    let orphans: i64 = m
        .conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(orphans, 0);

    assert!(m.rm("nonexistent").is_err());
}

#[test]
fn in_memory_master_for_adhoc_dedup() {
    let dir = tempfile::tempdir().unwrap();
    let db_a = index_archive(dir.path(), "a.tar", &[("x.txt", b"same bytes".to_vec())]);
    let db_b = index_archive(dir.path(), "b.tar", &[("y.txt", b"same bytes".to_vec())]);

    let (m, skipped) = master::open_in_memory(&[db_a, db_b]).unwrap();
    assert!(skipped.is_empty());
    assert_eq!(m.list().unwrap().len(), 2);
    let total: i64 = m
        .conn
        .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(total, 2);
}
