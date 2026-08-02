//! Sparse-tar handling. Issue #63: PAX sparse dialects (0.0/0.1/1.0) are
//! not implemented by tar-rs and must be indexed name-only — never with a
//! hash/FTS of the misread condensed bytes. The full differential corpus
//! against GNU tar (old-GNU dialect, compressed forms, malformed maps) is
//! issue #64 and extends this file.

mod common;

use std::path::Path;

use backupsage::indexer::{self, IndexOptions, IndexSummary};
use backupsage::store::flags;
use common::*;

fn index_tar_bytes(
    dir: &Path,
    name: &str,
    tar_bytes: &[u8],
) -> (IndexSummary, rusqlite::Connection) {
    let archive = write_archive(dir, name, tar_bytes);
    let summary = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    let conn = rusqlite::Connection::open(&summary.db_path).unwrap();
    (summary, conn)
}

fn fts_hits(conn: &rusqlite::Connection, word: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM fts WHERE fts MATCH ?1",
        [word],
        |r| r.get(0),
    )
    .unwrap()
}

fn plain_member(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    builder.append_data(&mut h, path, data).unwrap();
}

/// PAX 1.0 shape: member stored under the synthetic per-process path with
/// the sparse map as leading member bytes; real name/size only in pax keys.
#[test]
fn pax_1_0_sparse_indexes_name_only_under_real_name() {
    let dir = tempfile::tempdir().unwrap();
    let condensed = b"3\n0\n17\n65519\n17\n131055\n17\nfragment-not-file".to_vec();

    let mut builder = tar::Builder::new(Vec::new());
    let exts: Vec<(&str, &[u8])> = vec![
        ("GNU.sparse.major", b"1".as_slice()),
        ("GNU.sparse.minor", b"0"),
        ("GNU.sparse.name", b"data/real.bin"),
        ("GNU.sparse.realsize", b"65536"),
    ];
    builder
        .append_pax_extensions(exts.iter().map(|(k, v)| (*k, *v)))
        .unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_size(condensed.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    builder
        .append_data(&mut h, "GNUSparseFile.1234/real.bin", condensed.as_slice())
        .unwrap();
    // Sentinel after the sparse member proves the parser stayed in sync.
    plain_member(
        &mut builder,
        "after/sentinel.txt",
        b"sentinel text with sparsesentinelword",
    );
    let bytes = builder.into_inner().unwrap();

    let (summary, conn) = index_tar_bytes(dir.path(), "pax10.tar", &bytes);

    // Real-name row, name-only: no hash of misread condensed bytes, logical
    // size from GNU.sparse.realsize, SPARSE flagged.
    let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) =
        files_row(&conn, "data/real.bin");
    assert_eq!(etype, "file");
    assert!(
        hash.is_none(),
        "unsupported PAX sparse content must never be hashed"
    );
    assert_eq!(size, 65536, "logical size comes from GNU.sparse.realsize");
    assert!(flg & flags::SPARSE != 0);

    // The synthetic per-process path must not leak into the index.
    let synthetic: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path LIKE 'GNUSparseFile%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(synthetic, 0);

    // The condensed garbage must not be FTS-searchable.
    assert_eq!(fts_hits(&conn, "fragment"), 0);

    // Sentinel indexed intact after the sparse member.
    let (_, _, _, _, shash, _, _, sflags) = files_row(&conn, "after/sentinel.txt");
    assert!(shash.is_some());
    assert_eq!(sflags & flags::SPARSE, 0);
    assert_eq!(fts_hits(&conn, "sparsesentinelword"), 1);

    // Counted and reported.
    assert_eq!(summary.files_sparse_unsupported, 1);
}

/// PAX 0.0/0.1 shape: real path in the header, map in GNU.sparse.* keys
/// (size carries the logical size), member bytes are the condensed stream.
#[test]
fn pax_0_x_sparse_indexes_name_only_with_logical_size() {
    let dir = tempfile::tempdir().unwrap();
    let condensed = b"only-the-data-fragments".to_vec();

    let mut builder = tar::Builder::new(Vec::new());
    let exts: Vec<(&str, &[u8])> = vec![
        ("GNU.sparse.size", b"40960".as_slice()),
        ("GNU.sparse.numblocks", b"2"),
        ("GNU.sparse.offset", b"0"),
        ("GNU.sparse.numbytes", b"12"),
    ];
    builder
        .append_pax_extensions(exts.iter().map(|(k, v)| (*k, *v)))
        .unwrap();
    let mut h = tar::Header::new_ustar();
    h.set_size(condensed.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    builder
        .append_data(&mut h, "data/holey.bin", condensed.as_slice())
        .unwrap();
    plain_member(&mut builder, "after/ok.txt", b"pax0sentinel word");
    let bytes = builder.into_inner().unwrap();

    let (summary, conn) = index_tar_bytes(dir.path(), "pax00.tar", &bytes);

    let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) =
        files_row(&conn, "data/holey.bin");
    assert_eq!(etype, "file");
    assert!(hash.is_none());
    assert_eq!(size, 40960, "logical size comes from GNU.sparse.size");
    assert!(flg & flags::SPARSE != 0);
    assert_eq!(fts_hits(&conn, "fragments"), 0);

    let (_, _, _, _, shash, _, _, _) = files_row(&conn, "after/ok.txt");
    assert!(shash.is_some());
    assert_eq!(summary.files_sparse_unsupported, 1);
}

/// A malformed PAX extended header must fail the affected entry closed
/// (name-only + READ_ERROR), never index it as an ordinary hashed file —
/// a broken pax block can hide a sparse map.
#[test]
fn malformed_pax_extensions_fail_the_entry_closed() {
    let dir = tempfile::tempdir().unwrap();

    // Hand-built: an 'x' (XHeader) member whose body is not "len key=value"
    // records, followed by a normal member it would have applied to.
    let mut bytes = Vec::new();
    let garbage = b"this is not a pax record at all";
    let mut xh = tar::Header::new_ustar();
    xh.set_entry_type(tar::EntryType::XHeader);
    xh.set_path("paxheader/next").unwrap();
    xh.set_size(garbage.len() as u64);
    xh.set_mode(0o644);
    xh.set_mtime(1_700_000_001);
    xh.set_cksum();
    bytes.extend_from_slice(xh.as_bytes());
    bytes.extend_from_slice(garbage);
    bytes.resize(bytes.len().div_ceil(512) * 512, 0);

    let payload = b"payload that must not be trusted";
    let mut fh = tar::Header::new_ustar();
    fh.set_path("data/suspect.bin").unwrap();
    fh.set_size(payload.len() as u64);
    fh.set_mode(0o644);
    fh.set_mtime(1_700_000_001);
    fh.set_cksum();
    bytes.extend_from_slice(fh.as_bytes());
    bytes.extend_from_slice(payload);
    bytes.resize(bytes.len().div_ceil(512) * 512, 0);

    plain_member_raw(&mut bytes, "after/clean.txt", b"cleansentinel word");
    bytes.extend_from_slice(&[0u8; 1024]); // end-of-archive blocks

    let (_summary, conn) = index_tar_bytes(dir.path(), "badpax.tar", &bytes);

    let (etype, _kind, _size, _mtime, hash, _phash, _exif, flg) =
        files_row(&conn, "data/suspect.bin");
    assert_eq!(etype, "file");
    assert!(
        hash.is_none(),
        "entry under an unreadable pax header must not be hashed"
    );
    assert!(flg & flags::READ_ERROR != 0);

    let (_, _, _, _, shash, _, _, _) = files_row(&conn, "after/clean.txt");
    assert!(shash.is_some(), "indexing continues after the failed entry");
}

/// Raw-bytes twin of `plain_member` for hand-assembled archives.
fn plain_member_raw(bytes: &mut Vec<u8>, path: &str, data: &[u8]) {
    let mut h = tar::Header::new_ustar();
    h.set_path(path).unwrap();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    bytes.extend_from_slice(h.as_bytes());
    bytes.extend_from_slice(data);
    bytes.resize(bytes.len().div_ceil(512) * 512, 0);
}
