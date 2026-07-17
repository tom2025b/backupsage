//! End-to-end tests: build real archives in a temp dir, index them, search.
//!
//! The straddle fixtures place multi-byte UTF-8 characters exactly on the
//! read boundaries of the v0.1 implementation (8 KiB probe, next 64 KiB
//! chunk). v0.1 corrupted those words into U+FFFD halves, making them
//! unsearchable — these tests pin the fix.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use backupsage::indexer::{self, IndexOptions};
use backupsage::searcher;

const PROBE_BOUNDARY: usize = 8 * 1024;
const CHUNK_BOUNDARY: usize = 8 * 1024 + 64 * 1024;

fn fixture_files() -> Vec<(&'static str, Vec<u8>)> {
    // 'ö' (0xC3 0xB6) straddling the old probe boundary.
    let word1 = "wörterbuchspezial".as_bytes();
    let mut probe_straddle = vec![b'a'; PROBE_BOUNDARY - 3];
    probe_straddle.push(b' ');
    probe_straddle.extend_from_slice(word1);
    probe_straddle.extend_from_slice(b"\nprobefileend\n");
    assert_eq!(probe_straddle[PROBE_BOUNDARY - 1], 0xC3);
    assert_eq!(probe_straddle[PROBE_BOUNDARY], 0xB6);

    // 'ü' (0xC3 0xBC) straddling the old first-chunk boundary.
    let word2 = "münzsammlungtest".as_bytes();
    let mut chunk_straddle = vec![b'b'; CHUNK_BOUNDARY - 3];
    chunk_straddle.push(b' ');
    chunk_straddle.extend_from_slice(word2);
    chunk_straddle.extend_from_slice(b"\nchunkfileend\n");
    assert_eq!(chunk_straddle[CHUNK_BOUNDARY - 1], 0xC3);
    assert_eq!(chunk_straddle[CHUNK_BOUNDARY], 0xBC);

    vec![
        ("src/probe_straddle.txt", probe_straddle),
        ("src/chunk_straddle.txt", chunk_straddle),
        (
            "src/binary_blob.bin",
            b"\x00\x01\x02 hiddenbinaryword \x00\xff".repeat(50),
        ),
        (
            "src/notes.txt",
            b"the database_password is hunter2\ncall valid( here\nMixedCaseWord appears\n".to_vec(),
        ),
    ]
}

fn build_tar(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, data.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Index `archive`, return an open read-only connection to the result.
fn index_and_open(archive: &Path, opts: &IndexOptions) -> rusqlite::Connection {
    let summary = indexer::run_index(archive, None, opts).unwrap();
    searcher::open_index(&summary.db_path).unwrap()
}

fn hit_paths(conn: &rusqlite::Connection, query: &str) -> Vec<String> {
    searcher::search(conn, query, 100, false)
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.path)
        .collect()
}

#[test]
fn indexes_all_three_formats_identically() {
    let dir = tempfile::tempdir().unwrap();
    let tar_bytes = build_tar(&fixture_files());

    let archives = [
        write_archive(dir.path(), "fix.tar", &tar_bytes),
        write_archive(dir.path(), "fix.tar.gz", &gzip_bytes(&tar_bytes)),
        write_archive(
            dir.path(),
            "fix.tar.zst",
            &zstd::encode_all(&tar_bytes[..], 3).unwrap(),
        ),
    ];

    for archive in &archives {
        let summary = indexer::run_index(archive, None, &IndexOptions::default()).unwrap();
        assert_eq!(summary.files_indexed, 3, "for {}", archive.display());
        assert_eq!(summary.files_skipped_binary, 1, "for {}", archive.display());
        assert_eq!(summary.files_truncated, 0);

        let conn = searcher::open_index(&summary.db_path).unwrap();
        assert_eq!(searcher::index_completed(&conn), Some(true));
        assert_eq!(
            hit_paths(&conn, "database_password"),
            vec!["src/notes.txt".to_string()],
            "for {}",
            archive.display()
        );
    }
}

#[test]
fn finds_words_straddling_old_read_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let tar_bytes = build_tar(&fixture_files());
    let archive = write_archive(
        dir.path(),
        "fix.tar.zst",
        &zstd::encode_all(&tar_bytes[..], 3).unwrap(),
    );
    let conn = index_and_open(&archive, &IndexOptions::default());

    // v0.1 regression: these two returned no results.
    assert_eq!(
        hit_paths(&conn, "wörterbuchspezial"),
        vec!["src/probe_straddle.txt"]
    );
    assert_eq!(
        hit_paths(&conn, "münzsammlungtest"),
        vec!["src/chunk_straddle.txt"]
    );
}

#[test]
fn binary_content_skipped_but_name_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "fix.tar", &build_tar(&fixture_files()));
    let conn = index_and_open(&archive, &IndexOptions::default());

    assert!(hit_paths(&conn, "hiddenbinaryword").is_empty());
    // The file name itself is tokenised via the path column.
    assert_eq!(hit_paths(&conn, "binary_blob"), vec!["src/binary_blob.bin"]);
}

#[test]
fn search_is_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "fix.tar", &build_tar(&fixture_files()));
    let conn = index_and_open(&archive, &IndexOptions::default());

    assert_eq!(hit_paths(&conn, "MIXEDCASEWORD"), vec!["src/notes.txt"]);
}

#[test]
fn invalid_fts_syntax_falls_back_to_literal_phrase() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "fix.tar", &build_tar(&fixture_files()));
    let conn = index_and_open(&archive, &IndexOptions::default());

    // v0.1 swallowed the FTS5 syntax error and printed "No results".
    let outcome = searcher::search(&conn, "valid(", 100, false).unwrap();
    assert!(outcome.literal_fallback);
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].path, "src/notes.txt");
}

#[test]
fn oversized_files_are_truncated_at_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut big = b"headmarker ".to_vec();
    big.extend(std::iter::repeat_n(b'x', 8192));
    big.extend_from_slice(b" tailmarker\n");
    let files = vec![("src/big.log", big)];
    let archive = write_archive(dir.path(), "big.tar", &build_tar(&files));

    let opts = IndexOptions {
        max_file_size: 4096,
        ..IndexOptions::default()
    };
    let summary = indexer::run_index(&archive, None, &opts).unwrap();
    assert_eq!(summary.files_truncated, 1);

    let conn = searcher::open_index(&summary.db_path).unwrap();
    assert_eq!(hit_paths(&conn, "headmarker"), vec!["src/big.log"]);
    assert!(hit_paths(&conn, "tailmarker").is_empty()); // beyond the cap
}

#[test]
fn snippets_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "fix.tar", &build_tar(&fixture_files()));
    let conn = index_and_open(&archive, &IndexOptions::default());

    let outcome = searcher::search(&conn, "hunter2", 100, true).unwrap();
    let snippet = outcome.hits[0].snippet.as_deref().unwrap();
    assert!(snippet.contains("[hunter2]"), "snippet was: {snippet}");

    // limit smaller than the result count reports truncation
    let outcome = searcher::search(&conn, "a OR b OR the OR call", 1, false).unwrap();
    assert_eq!(outcome.hits.len(), 1);
}

#[test]
fn top_words_counts_totals_and_docs() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        ("a.txt", b"apple apple banana".to_vec()),
        ("b.txt", b"apple cherry".to_vec()),
    ];
    let archive = write_archive(dir.path(), "words.tar", &build_tar(&files));
    let conn = index_and_open(&archive, &IndexOptions::default());

    let rows = searcher::top_words(&conn, 10).unwrap();
    let apple = rows.iter().find(|r| r.word == "apple").unwrap();
    assert_eq!((apple.total, apple.docs), (3, 2));
    assert!(rows.iter().any(|r| r.word == "banana" && r.total == 1));
}

#[test]
fn no_word_stats_skips_word_freq() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "fix.tar", &build_tar(&fixture_files()));
    let opts = IndexOptions {
        word_stats: false,
        ..IndexOptions::default()
    };
    let summary = indexer::run_index(&archive, None, &opts).unwrap();
    let conn = searcher::open_index(&summary.db_path).unwrap();

    assert!(searcher::top_words(&conn, 10).unwrap().is_empty());
    assert_eq!(
        searcher::get_meta(&conn, "word_stats").as_deref(),
        Some("0")
    );
    // search still works
    assert_eq!(hit_paths(&conn, "database_password"), vec!["src/notes.txt"]);
}

#[test]
fn v1_databases_without_meta_table_still_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("old.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE files_fts USING fts5(path, content, tokenize='unicode61');
             CREATE TABLE word_freq(word TEXT PRIMARY KEY,
                                    total_count INTEGER NOT NULL DEFAULT 0,
                                    doc_count INTEGER NOT NULL DEFAULT 0);
             INSERT INTO files_fts(path, content) VALUES ('etc/app.conf', 'secret token here');",
        )
        .unwrap();
    }

    let conn = searcher::open_index(&db_path).unwrap();
    assert_eq!(searcher::index_completed(&conn), None); // no meta table: pre-v2
    assert_eq!(hit_paths(&conn, "secret"), vec!["etc/app.conf"]);
}

#[test]
fn non_backupsage_db_rejected_with_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let bogus = dir.path().join("random.db");
    fs::write(&bogus, b"definitely not sqlite").unwrap();

    let err = searcher::open_index(&bogus).unwrap_err();
    assert!(
        err.to_string().contains("not a BackupSage index"),
        "got: {err:#}"
    );
}

#[test]
fn mislabelled_extension_still_detected_by_content() {
    let dir = tempfile::tempdir().unwrap();
    // gzip data hiding behind a .tar.zst name — v0.1 trusted the extension
    // and produced an empty index with exit 0.
    let tar_bytes = build_tar(&fixture_files());
    let archive = write_archive(dir.path(), "wrong.tar.zst", &gzip_bytes(&tar_bytes));

    let summary = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    assert_eq!(summary.files_indexed, 3);
    assert_eq!(summary.format, "tar + gzip");
}

#[test]
fn corrupt_entry_header_aborts_and_marks_index_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![
        ("a.txt", b"first file alphaword".to_vec()),
        ("b.txt", b"second file betaword".to_vec()),
        ("c.txt", b"third file gammaword".to_vec()),
    ];
    let mut tar_bytes = build_tar(&files);

    // Corrupt the checksum field (offset 148) of the second entry's header.
    // Entry layout: 512-byte header + data padded to 512-byte blocks.
    let second_header = 512 + 512; // a.txt data (20 bytes) pads to one block
    for b in &mut tar_bytes[second_header + 148..second_header + 156] {
        *b = b'z';
    }
    let archive = write_archive(dir.path(), "corrupt.tar", &tar_bytes);

    // tar's entry iterator cannot resync past a bad header — v0.1 printed a
    // warning, silently dropped every later entry, and reported success.
    let err = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("corrupt tar entry"),
        "got: {err:#}"
    );

    // The partial database must not claim completeness.
    let db_path = indexer::resolve_db_path(&archive, None);
    let conn = searcher::open_index(&db_path).unwrap();
    assert_eq!(searcher::index_completed(&conn), Some(false));
}

#[test]
fn hardlink_and_symlink_names_are_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let mut builder = tar::Builder::new(Vec::new());

    let data = b"the real content".to_vec();
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, "bin/original", data.as_slice())
        .unwrap();

    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    link.set_cksum();
    builder
        .append_link(&mut link, "bin/hardlinkname", "bin/original")
        .unwrap();

    let mut sym = tar::Header::new_gnu();
    sym.set_entry_type(tar::EntryType::Symlink);
    sym.set_size(0);
    sym.set_cksum();
    builder
        .append_link(&mut sym, "bin/symlinkname", "bin/original")
        .unwrap();

    let archive = write_archive(dir.path(), "links.tar", &builder.into_inner().unwrap());
    let summary = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    assert_eq!(summary.files_indexed, 1);
    assert_eq!(summary.files_link_names, 2);

    let conn = searcher::open_index(&summary.db_path).unwrap();
    // v0.1 skipped link entries entirely — their names were unfindable.
    assert_eq!(hit_paths(&conn, "hardlinkname"), vec!["bin/hardlinkname"]);
    assert_eq!(hit_paths(&conn, "symlinkname"), vec!["bin/symlinkname"]);
}

#[test]
fn control_chars_in_content_do_not_inflate_match_counts() {
    let dir = tempfile::tempdir().unwrap();
    let files = vec![(
        "weird.txt",
        b"\x01\x01 markerword \x01 and more \x01\x01".to_vec(),
    )];
    let archive = write_archive(dir.path(), "weird.tar", &build_tar(&files));
    let conn = index_and_open(&archive, &IndexOptions::default());

    let outcome = searcher::search(&conn, "markerword", 10, false).unwrap();
    assert_eq!(outcome.hits.len(), 1);
    // 0x01 doubles as the highlight marker; without stripping it at index
    // time this count would be inflated by the file's own bytes.
    assert_eq!(outcome.hits[0].matches, 1);
}

#[test]
fn non_archive_input_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let not_archive = dir.path().join("notes.txt");
    fs::write(&not_archive, b"just some text, no tar magic anywhere").unwrap();

    let err = indexer::run_index(&not_archive, None, &IndexOptions::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("unrecognised archive format"),
        "got: {err:#}"
    );
}
