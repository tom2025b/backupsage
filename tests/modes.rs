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
