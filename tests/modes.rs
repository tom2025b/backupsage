//! Content indexing modes (#70, child of #39): per-mode storage and
//! read-time gates. Master/dedup replication is #71.

mod common;

use std::path::Path;

use backupsage::indexer::{self, ContentMode, IndexOptions};
use common::*;

fn index_with_mode(
    dir: &Path,
    name: &str,
    files: &[(&str, Vec<u8>)],
    mode: ContentMode,
) -> (indexer::IndexSummary, rusqlite::Connection) {
    let archive = write_archive(dir, name, &build_tar(files));
    let opts = IndexOptions {
        mode,
        ..IndexOptions::default()
    };
    let summary = indexer::run_index(&archive, None, &opts).unwrap();
    let conn = rusqlite::Connection::open(&summary.db_path).unwrap();
    (summary, conn)
}

#[test]
fn mode_is_recorded_in_meta_and_defaults_to_full() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("a.txt", b"hello modeword".to_vec())];
    for (mode, expect) in [
        (ContentMode::Full, "full"),
        (ContentMode::SearchOnly, "search-only"),
        (ContentMode::MetadataOnly, "metadata-only"),
    ] {
        let (_s, conn) = index_with_mode(dir.path(), &format!("{expect}.tar"), &files, mode);
        let got: String = conn
            .query_row("SELECT value FROM meta WHERE key='content_mode'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(got, expect);
    }
    assert_eq!(IndexOptions::default().mode, ContentMode::Full);
}

#[test]
fn search_only_stores_no_plaintext_but_still_matches() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, conn) = index_with_mode(
        dir.path(),
        "so.tar",
        &[("doc.txt", b"secret plaintext with leakword inside".to_vec())],
        ContentMode::SearchOnly,
    );
    // Tokens match…
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH 'leakword'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
    // …but no shadow content table exists (contentless FTS5 stores none),
    // so the plaintext is not recoverable from the index file.
    let content_tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='files_fts_content'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        content_tables, 0,
        "contentless FTS must have no content shadow table"
    );
    // Hashes kept (dedup still works on search-only archives).
    let (_, _, _, _, hash, _, _, _) = files_row(&conn, "doc.txt");
    assert!(hash.is_some());
    // Word stats dropped — the frequency surface — and the meta key is
    // honest about it regardless of the CLI flag.
    let words: i64 = conn
        .query_row("SELECT COUNT(*) FROM word_freq", [], |r| r.get(0))
        .unwrap();
    assert_eq!(words, 0);
    let ws: String = conn
        .query_row("SELECT value FROM meta WHERE key='word_stats'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(ws, "0");
}

#[test]
fn metadata_only_never_reads_content() {
    let dir = tempfile::tempdir().unwrap();
    let (summary, conn) = index_with_mode(
        dir.path(),
        "mo.tar",
        &[
            ("doc.txt", b"text that must never be read".to_vec()),
            ("pic.png", png_bytes(3, 64, 48)),
        ],
        ContentMode::MetadataOnly,
    );
    for path in ["doc.txt", "pic.png"] {
        let (etype, _kind, size, mtime, hash, phash, exif, _flg) = files_row(&conn, path);
        assert_eq!(etype, "file");
        assert!(size > 0, "header metadata kept");
        assert!(mtime.is_some());
        assert!(hash.is_none(), "{path}: content must not be hashed");
        assert!(phash.is_none());
        assert!(exif.is_none());
    }
    // Media kinds still classified from the name.
    let (_, kind, _, _, _, _, _, _) = files_row(&conn, "pic.png");
    assert_eq!(kind, "image");
    let fts: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH 'read'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts, 0, "no content tokens in a metadata-only index");
    assert_eq!(summary.files_hashed, 0);
}

#[test]
fn old_index_without_mode_key_reads_as_full() {
    let dir = tempfile::tempdir().unwrap();
    let (_s, conn) = index_with_mode(
        dir.path(),
        "old.tar",
        &[("a.txt", b"grandfather word".to_vec())],
        ContentMode::Full,
    );
    drop(conn);
    let db = dir.path().join("old.tar.db");
    // Simulate a pre-#70 index: the key simply doesn't exist. The file is
    // a test fixture this test just built — not a real user index.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM meta WHERE key='content_mode'", [])
        .unwrap();
    assert_eq!(
        backupsage::store::content_mode(&conn),
        ContentMode::Full,
        "absent content_mode key must grandfather to full"
    );
    drop(conn);
    // And search still works end-to-end through the CLI.
    let out = run(&["search", "grandfather", "-i", db.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("a.txt"), "{}", stdout(&out));
}

// ── CLI-level read-time gates ───────────────────────────────────────────────

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_backupsage"))
}

fn run(args: &[&str]) -> std::process::Output {
    bin().args(args).output().expect("binary runs")
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn read_time_gates_reject_and_degrade_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let files = [("notes/doc.txt", b"findable gateword here".to_vec())];

    // One archive per mode, indexed through the real CLI.
    for mode in ["full", "search-only", "metadata-only"] {
        let archive = write_archive(dir.path(), &format!("{mode}.tar"), &build_tar(&files));
        let out = run(&["index", archive.to_str().unwrap(), "--mode", mode]);
        assert!(out.status.success(), "index --mode {mode} failed");
    }
    let db = |mode: &str| {
        dir.path()
            .join(format!("{mode}.tar.db"))
            .to_str()
            .unwrap()
            .to_string()
    };

    // metadata-only: search rejects clearly, exit 1, never "No results".
    let out = run(&["search", "gateword", "-i", &db("metadata-only")]);
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(err.contains("metadata-only"), "{err}");
    assert!(err.contains("--mode"), "{err}");

    // search-only: search works…
    let out = run(&["search", "gateword", "-i", &db("search-only")]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("doc.txt"), "{}", stdout(&out));
    // …snippets degrade with an explicit note…
    let out = run(&["search", "gateword", "-i", &db("search-only"), "--snippets"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stderr(&out).contains("search-only"), "{}", stderr(&out));
    // …and JSON carries the mode with null match counts.
    let out = run(&["search", "gateword", "-i", &db("search-only"), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["mode"], "search-only");
    assert!(v["hits"][0]["matches"].is_null(), "{v}");

    // full: JSON mode present, matches numeric — unchanged semantics.
    let out = run(&["search", "gateword", "-i", &db("full"), "--json"]);
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["mode"], "full");
    assert!(v["hits"][0]["matches"].is_number(), "{v}");

    // top explains itself on non-full modes.
    let out = run(&["top", "-i", &db("search-only")]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("search-only"), "{}", stdout(&out));
    let out = run(&["top", "-i", &db("metadata-only")]);
    assert!(stdout(&out).contains("metadata-only"), "{}", stdout(&out));
}
