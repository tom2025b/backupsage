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
            .query_row(
                "SELECT value FROM meta WHERE key='content_mode'",
                [],
                |r| r.get(0),
            )
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
