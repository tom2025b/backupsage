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
        "SELECT COUNT(*) FROM files_fts WHERE files_fts MATCH ?1",
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
    let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) = files_row(&conn, "data/real.bin");
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

    let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) = files_row(&conn, "data/holey.bin");
    assert_eq!(etype, "file");
    assert!(hash.is_none());
    assert_eq!(size, 40960, "logical size comes from GNU.sparse.size");
    assert!(flg & flags::SPARSE != 0);
    assert_eq!(fts_hits(&conn, "fragments"), 0);

    let (_, _, _, _, shash, _, _, _) = files_row(&conn, "after/ok.txt");
    assert!(shash.is_some());
    assert_eq!(summary.files_sparse_unsupported, 1);
}

/// Unparsable pax SEGMENTS (real GNU tar emits them for legal values with
/// raw newlines — xattrs, newline filenames) must NOT strip the entry of
/// its content: the file stays hashed and searchable, and the unreadable
/// metadata is counted so it is loud, never silent.
#[test]
fn unparsable_pax_segments_warn_but_keep_content() {
    let dir = tempfile::tempdir().unwrap();

    // Hand-built 'x' (XHeader) member whose body is not "len key=value"
    // records, followed by the member it applies to.
    let mut bytes = Vec::new();
    xheader_raw(&mut bytes, b"this is not a pax record at all");
    plain_member_raw(
        &mut bytes,
        "data/xattred.txt",
        b"real content stays searchword",
    );
    plain_member_raw(&mut bytes, "after/clean.txt", b"cleansentinel word");
    bytes.extend_from_slice(&[0u8; 1024]); // end-of-archive blocks

    let (summary, conn) = index_tar_bytes(dir.path(), "badpax.tar", &bytes);

    let (etype, _kind, _size, _mtime, hash, _phash, _exif, flg) =
        files_row(&conn, "data/xattred.txt");
    assert_eq!(etype, "file");
    assert!(
        hash.is_some(),
        "unreadable pax metadata must not cost the file its hash"
    );
    assert_eq!(flg & flags::SPARSE, 0);
    assert!(
        flg & flags::PAX_UNPARSED != 0,
        "row-level audit flag for unreadable pax metadata"
    );
    assert_eq!(fts_hits(&conn, "searchword"), 1, "content stays searchable");
    assert_eq!(
        summary.entries_pax_unparsed, 1,
        "unreadable metadata is counted"
    );
    assert_eq!(summary.files_sparse_unsupported, 0);

    let (_, _, _, _, shash, _, _, _) = files_row(&conn, "after/clean.txt");
    assert!(shash.is_some());
}

/// A parseable GNU.sparse record still routes the entry name-only even when
/// it sits next to garbage segments in the same pax block — fail-open on
/// broken segments never fails open on a *detectable* sparse map.
#[test]
fn sparse_record_beside_garbage_segments_still_detected() {
    let dir = tempfile::tempdir().unwrap();

    let mut bytes = Vec::new();
    // "25 GNU.sparse.size=40960\n" is a valid length-prefixed record (25
    // bytes total including its own length field and newline).
    xheader_raw(&mut bytes, b"garbage line\n25 GNU.sparse.size=40960\n");
    plain_member_raw(&mut bytes, "data/holey.bin", b"condensed-fragments");
    plain_member_raw(&mut bytes, "after/ok.txt", b"mixedsentinel word");
    bytes.extend_from_slice(&[0u8; 1024]);

    let (summary, conn) = index_tar_bytes(dir.path(), "mixedpax.tar", &bytes);

    let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) = files_row(&conn, "data/holey.bin");
    assert_eq!(etype, "file");
    assert!(hash.is_none(), "detectable sparse map must still win");
    assert_eq!(size, 40960);
    assert!(flg & flags::SPARSE != 0);
    assert!(flg & flags::PAX_UNPARSED != 0);
    assert_eq!(summary.files_sparse_unsupported, 1);
    assert_eq!(summary.entries_pax_unparsed, 1);

    let (_, _, _, _, shash, _, _, _) = files_row(&conn, "after/ok.txt");
    assert!(shash.is_some());
}

// ── #64: differential corpus against GNU tar ────────────────────────────────
//
// Fixtures under tests/fixtures/sparse/ are built ONCE by GNU tar (1.35 on
// this box) via the bless test below and committed frozen-as-observed —
// PAX 1.0 embeds a per-process GNUSparseFile.<pid> path, so regeneration is
// deliberate, never compared byte-for-byte across runs. Verification runs
// need no system tar: expected logical hashes live in expected.json and the
// logical layout is reconstructed in-process.

const SPARSE_LOGICAL_SIZE: u64 = 1_048_576;
const SENTINEL_TEXT: &[u8] = b"sparse corpus sentinel gnuword\n";

/// The logical (hole-filled) content of the corpus sparse file: two 4 KiB
/// data fragments at 0 and 512 KiB, trailing hole to 1 MiB.
fn sparse_logical_buffer() -> Vec<u8> {
    let mut buf = vec![0u8; SPARSE_LOGICAL_SIZE as usize];
    for (i, b) in buf[..4096].iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    for (i, b) in buf[524_288..528_384].iter_mut().enumerate() {
        *b = ((i * 7) % 249) as u8 | 1;
    }
    buf
}

fn sparse_fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sparse")
}

#[derive(serde::Deserialize, serde::Serialize)]
struct SparseExpected {
    logical_size: u64,
    logical_blake3: String,
    sentinel_blake3: String,
}

fn load_expected() -> SparseExpected {
    let path = sparse_fixture_dir().join("expected.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing {} ({e}) — generate the corpus with \
             BACKUPSAGE_SPARSE_BLESS=1 cargo test --test sparse bless_sparse_fixtures",
            path.display()
        )
    });
    serde_json::from_str(&text).unwrap()
}

/// Regenerates the committed corpus with system GNU tar. Deliberate act:
///   BACKUPSAGE_SPARSE_BLESS=1 cargo test --test sparse bless_sparse_fixtures
/// No-op (pass) otherwise so the suite stays hermetic.
#[test]
fn bless_sparse_fixtures() {
    if std::env::var_os("BACKUPSAGE_SPARSE_BLESS").is_none() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    // Real sparse file on disk: seek-write the fragments, set_len the tail.
    let logical = sparse_logical_buffer();
    let holey = work.path().join("holey.bin");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(&holey).unwrap();
        f.write_all(&logical[..4096]).unwrap();
        f.seek(SeekFrom::Start(524_288)).unwrap();
        f.write_all(&logical[524_288..528_384]).unwrap();
        f.set_len(SPARSE_LOGICAL_SIZE).unwrap();
    }
    std::fs::write(work.path().join("sentinel.txt"), SENTINEL_TEXT).unwrap();

    let out_dir = sparse_fixture_dir();
    std::fs::create_dir_all(&out_dir).unwrap();
    let variants: [(&str, &[&str]); 4] = [
        ("sparse-oldgnu.tar", &["--format=gnu"]),
        ("sparse-pax00.tar", &["--format=posix", "--sparse-version=0.0"]),
        ("sparse-pax01.tar", &["--format=posix", "--sparse-version=0.1"]),
        ("sparse-pax10.tar", &["--format=posix", "--sparse-version=1.0"]),
    ];
    for (name, extra) in variants {
        let out = out_dir.join(name);
        let status = std::process::Command::new("tar")
            .args(extra)
            .args([
                "--sparse",
                "--mtime=@1700000001",
                "--owner=root",
                "--group=root",
                "--numeric-owner",
                "-C",
            ])
            .arg(work.path())
            .arg("-cf")
            .arg(&out)
            .args(["holey.bin", "sentinel.txt"])
            .status()
            .expect("GNU tar runs");
        assert!(status.success(), "tar failed for {name}");
        // Cross-check: GNU tar itself round-trips the fixture back to the
        // exact logical bytes we wrote.
        let extracted = std::process::Command::new("tar")
            .arg("-xOf")
            .arg(&out)
            .arg("holey.bin")
            .output()
            .expect("GNU tar extracts");
        assert!(extracted.status.success(), "extract failed for {name}");
        assert_eq!(
            blake3::hash(&extracted.stdout),
            blake3::hash(&logical),
            "GNU tar round-trip diverged for {name}"
        );
    }
    let expected = SparseExpected {
        logical_size: SPARSE_LOGICAL_SIZE,
        logical_blake3: blake3::hash(&logical).to_hex().to_string(),
        sentinel_blake3: blake3::hash(SENTINEL_TEXT).to_hex().to_string(),
    };
    std::fs::write(
        out_dir.join("expected.json"),
        serde_json::to_string_pretty(&expected).unwrap() + "\n",
    )
    .unwrap();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// AC: old-GNU logical size, byte stream, and BLAKE3 match GNU tar — across
/// plain tar, gzip, and zstd — and the sentinel after the sparse member is
/// indexed intact (parser-sync proof).
#[test]
fn old_gnu_sparse_matches_gnu_tar_logically_in_all_formats() {
    let expected = load_expected();
    let tar_bytes = std::fs::read(sparse_fixture_dir().join("sparse-oldgnu.tar")).unwrap();

    let gz = {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap()
    };
    let zst = zstd::encode_all(&tar_bytes[..], 3).unwrap();

    for (name, bytes) in [
        ("oldgnu.tar", tar_bytes.clone()),
        ("oldgnu.tar.gz", gz),
        ("oldgnu.tar.zst", zst),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (summary, conn) = index_tar_bytes(dir.path(), name, &bytes);

        let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) =
            files_row(&conn, "holey.bin");
        assert_eq!(etype, "file");
        assert_eq!(
            size as u64, expected.logical_size,
            "{name}: stored size must be the logical size"
        );
        assert_eq!(
            hex(&hash.expect("old-GNU sparse content is hashed")),
            expected.logical_blake3,
            "{name}: hash must equal GNU tar's logical byte stream"
        );
        assert!(flg & flags::SPARSE != 0, "{name}: sparse flag");
        assert_eq!(flg & flags::PAX_UNPARSED, 0, "{name}");
        assert_eq!(summary.files_sparse_unsupported, 0, "{name}");

        let (_, _, _, _, shash, _, _, sflags) = files_row(&conn, "sentinel.txt");
        assert_eq!(
            hex(&shash.expect("sentinel hashed")),
            expected.sentinel_blake3,
            "{name}: sentinel content intact after the sparse member"
        );
        assert_eq!(sflags, 0, "{name}");
        assert_eq!(fts_hits(&conn, "gnuword"), 1, "{name}");
    }
}

/// AC: every real GNU tar PAX sparse dialect yields the hardened
/// inconclusive shape (#63): name-only under the real name, logical size,
/// SPARSE flag, counted — with the sentinel intact behind it.
#[test]
fn real_pax_sparse_dialects_index_name_only() {
    let expected = load_expected();
    for name in ["sparse-pax00.tar", "sparse-pax01.tar", "sparse-pax10.tar"] {
        let bytes = std::fs::read(sparse_fixture_dir().join(name)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (summary, conn) = index_tar_bytes(dir.path(), name, &bytes);

        let (etype, _kind, size, _mtime, hash, _phash, _exif, flg) =
            files_row(&conn, "holey.bin");
        assert_eq!(etype, "file", "{name}");
        assert!(hash.is_none(), "{name}: PAX sparse must not be hashed");
        assert_eq!(size as u64, expected.logical_size, "{name}: logical size");
        assert!(flg & flags::SPARSE != 0, "{name}");
        assert_eq!(summary.files_sparse_unsupported, 1, "{name}");

        let synthetic: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%GNUSparseFile%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(synthetic, 0, "{name}: synthetic path must not leak");

        let (_, _, _, _, shash, _, _, _) = files_row(&conn, "sentinel.txt");
        assert_eq!(
            hex(&shash.expect("sentinel hashed")),
            expected.sentinel_blake3,
            "{name}"
        );
        assert_eq!(fts_hits(&conn, "gnuword"), 1, "{name}");
    }
}

// ── Malformed old-GNU sparse maps: every case fails LOUDLY ──────────────────

const TYPEFLAG_OFF: usize = 156;
const CHKSUM_OFF: usize = 148;
const GNU_SPARSE_OFF: usize = 386; // 4 entries × (12-byte offset + 12-byte numbytes)
const GNU_REALSIZE_OFF: usize = 483;

/// Recompute the tar header checksum after byte-patching.
fn fix_chksum(header: &mut [u8]) {
    header[CHKSUM_OFF..CHKSUM_OFF + 8].fill(b' ');
    let sum: u64 = header[..512].iter().map(|&b| b as u64).sum();
    let rendered = format!("{sum:06o}\0 ");
    header[CHKSUM_OFF..CHKSUM_OFF + 8].copy_from_slice(rendered.as_bytes());
}

fn patched_oldgnu<F: FnOnce(&mut [u8])>(patch: F) -> Vec<u8> {
    let mut bytes = std::fs::read(sparse_fixture_dir().join("sparse-oldgnu.tar")).unwrap();
    assert_eq!(bytes[TYPEFLAG_OFF], b'S', "fixture starts with the sparse member");
    patch(&mut bytes);
    fix_chksum(&mut bytes[..512]);
    bytes
}

fn assert_aborts_loudly(name: &str, bytes: &[u8]) {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), name, bytes);
    let err = indexer::run_index(&archive, None, &IndexOptions::default())
        .expect_err("malformed sparse map must abort, never index silently");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("corrupt tar entry"),
        "{name}: abort must be the loud corrupt-entry path, got: {msg}"
    );
    assert!(
        !archive.with_extension("tar.db").exists(),
        "{name}: no index may be promoted from an aborted run"
    );
}

/// AC: malformed maps — out-of-order/overlapping offsets abort loudly.
#[test]
fn overlapping_sparse_blocks_abort() {
    let bytes = patched_oldgnu(|b| {
        // Copy entry 0's offset over entry 1's: second block now starts
        // before the first ends — tar-rs's add_block rejects it.
        let (e0, e1) = (GNU_SPARSE_OFF, GNU_SPARSE_OFF + 24);
        let first: Vec<u8> = b[e0..e0 + 12].to_vec();
        b[e1..e1 + 12].copy_from_slice(&first);
    });
    assert_aborts_loudly("overlap.tar", &bytes);
}

/// AC: a sparse map whose blocks disagree with the header's realsize aborts.
#[test]
fn realsize_mismatch_aborts() {
    let bytes = patched_oldgnu(|b| {
        let wrong = format!("{:011o}\0", SPARSE_LOGICAL_SIZE + 512);
        b[GNU_REALSIZE_OFF..GNU_REALSIZE_OFF + 12].copy_from_slice(wrong.as_bytes());
    });
    assert_aborts_loudly("realsize.tar", &bytes);
}

/// AC: an archive truncated inside the sparse member's data aborts.
#[test]
fn truncated_sparse_data_aborts() {
    let full = std::fs::read(sparse_fixture_dir().join("sparse-oldgnu.tar")).unwrap();
    let truncated = &full[..1024]; // header + a sliver of data, then EOF
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(dir.path(), "truncated.tar", truncated);
    let err = indexer::run_index(&archive, None, &IndexOptions::default())
        .expect_err("truncated sparse data must not produce a complete index");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("corrupt tar entry") || msg.to_lowercase().contains("eof"),
        "truncated.tar: got: {msg}"
    );
    assert!(!archive.with_extension("tar.db").exists());
}

/// #63 residual, pinned honestly (crafted-only; GNU tar parses this block
/// fine and extracts sparse, tar-rs stops at the empty split segment): a
/// valid record whose value ends in '\n' hides later sparse records. The
/// row is hashed-with-PAX_UNPARSED — auditable, documented, not silent.
#[test]
fn crafted_trailing_newline_value_pins_fail_open_with_audit_flag() {
    let mut bytes = Vec::new();
    xheader_raw(
        &mut bytes,
        b"39 SCHILY.xattr.user.note=config blob\n\n25 GNU.sparse.size=40960\n37 GNU.sparse.name=data/g2c-real.bin\n",
    );
    plain_member_raw(&mut bytes, "data/g2c.bin", b"condensed-not-logical");
    bytes.extend_from_slice(&[0u8; 1024]);

    let dir = tempfile::tempdir().unwrap();
    let (summary, conn) = index_tar_bytes(dir.path(), "g2c.tar", &bytes);
    let (_, _, _, _, hash, _, _, flg) = files_row(&conn, "data/g2c.bin");
    assert!(hash.is_some(), "pinned: tar-rs cannot see the hidden map");
    assert!(
        flg & flags::PAX_UNPARSED != 0,
        "the audit flag is what keeps this shape non-silent"
    );
    assert_eq!(flg & flags::SPARSE, 0);
    assert_eq!(summary.entries_pax_unparsed, 1);
}

/// #63 residual, pinned honestly: a bare leading-'\n' pax block terminates
/// tar-rs's record walk before any record — crafted-only, and GNU tar
/// REJECTS such an archive outright, so no extractor sees it as sparse.
#[test]
fn crafted_leading_newline_block_pins_fail_open() {
    let mut bytes = Vec::new();
    xheader_raw(&mut bytes, b"\n25 GNU.sparse.size=40960\n");
    plain_member_raw(&mut bytes, "data/e3.bin", b"condensed-not-logical");
    bytes.extend_from_slice(&[0u8; 1024]);

    let dir = tempfile::tempdir().unwrap();
    let (summary, conn) = index_tar_bytes(dir.path(), "e3.tar", &bytes);
    let (_, _, _, _, hash, _, _, flg) = files_row(&conn, "data/e3.bin");
    assert!(hash.is_some());
    assert_eq!(flg, 0, "pinned: the empty-segment stop is invisible to tar-rs");
    assert_eq!(summary.entries_pax_unparsed, 0);
    assert_eq!(summary.files_sparse_unsupported, 0);
}

/// Raw-bytes 'x' XHeader member with an arbitrary body.
fn xheader_raw(bytes: &mut Vec<u8>, body: &[u8]) {
    let mut xh = tar::Header::new_ustar();
    xh.set_entry_type(tar::EntryType::XHeader);
    xh.set_path("paxheader/next").unwrap();
    xh.set_size(body.len() as u64);
    xh.set_mode(0o644);
    xh.set_mtime(1_700_000_001);
    xh.set_cksum();
    bytes.extend_from_slice(xh.as_bytes());
    bytes.extend_from_slice(body);
    bytes.resize(bytes.len().div_ceil(512) * 512, 0);
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
