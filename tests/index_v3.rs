//! Schema v3 end-to-end: metadata capture, full-content hashing, archive
//! fingerprints, media handling, links, shadowing and directory sources.

mod common;

use std::fs;

use backupsage::indexer::{self, IndexOptions};
use backupsage::searcher;
use backupsage::store::flags;
use common::*;

#[test]
fn metadata_hash_and_archive_fingerprint_all_formats() {
    let dir = tempfile::tempdir().unwrap();
    let content = b"cross archive dedup test content\n".to_vec();
    let tar_bytes = build_tar(&[("docs/a.txt", content.clone())]);

    let mut gz = Vec::new();
    {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(&mut gz, flate2::Compression::default());
        enc.write_all(&tar_bytes).unwrap();
        enc.finish().unwrap();
    }
    let archives = [
        write_archive(dir.path(), "f.tar", &tar_bytes),
        write_archive(dir.path(), "f.tar.gz", &gz),
        write_archive(
            dir.path(),
            "f.tar.zst",
            &zstd::encode_all(&tar_bytes[..], 3).unwrap(),
        ),
    ];

    for archive in &archives {
        let summary = indexer::run_index(archive, None, &IndexOptions::default()).unwrap();
        assert_eq!(summary.files_hashed, 1, "for {}", archive.display());
        let conn = searcher::open_index(&summary.db_path).unwrap();

        let (etype, kind, size, mtime, hash, _, _, fl) = files_row(&conn, "docs/a.txt");
        assert_eq!((etype.as_str(), kind.as_str()), ("file", "text"));
        assert_eq!(size as usize, content.len());
        assert_eq!(mtime, Some(1_700_000_001));
        assert_eq!(
            hash.as_deref(),
            Some(blake3::hash(&content).as_bytes().as_slice())
        );
        assert_eq!(fl, 0);

        // The fingerprint must equal b3sum of the archive file itself —
        // this is the drained-to-EOF contract behind `master verify --deep`.
        let bytes = fs::read(archive).unwrap();
        assert_eq!(
            searcher::get_meta(&conn, "archive_blake3").as_deref(),
            Some(blake3::hash(&bytes).to_hex().as_str()),
            "fingerprint mismatch for {}",
            archive.display()
        );
        assert_eq!(
            searcher::get_meta(&conn, "schema_version").as_deref(),
            Some("3")
        );
        assert!(searcher::get_meta(&conn, "index_uuid").unwrap().len() == 32);
    }
}

#[test]
fn full_hash_past_the_text_cap() {
    let dir = tempfile::tempdir().unwrap();
    let big = vec![b'y'; 50_000];
    let archive = write_archive(
        dir.path(),
        "big.tar",
        &build_tar(&[("logs/big.log", big.clone())]),
    );

    let opts = IndexOptions {
        max_file_size: 4096,
        ..IndexOptions::default()
    };
    let summary = indexer::run_index(&archive, None, &opts).unwrap();
    assert_eq!(summary.files_truncated, 1);

    let conn = searcher::open_index(&summary.db_path).unwrap();
    let (_, _, size, _, hash, _, _, fl) = files_row(&conn, "logs/big.log");
    assert_eq!(size, 50_000);
    // Hash covers ALL bytes even though FTS only got the first 4096.
    assert_eq!(
        hash.as_deref(),
        Some(blake3::hash(&big).as_bytes().as_slice())
    );
    assert_ne!(fl & flags::FTS_TRUNCATED, 0);
}

#[test]
fn images_get_phash_dims_and_exif_degrades_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let png = png_bytes(1, 64, 48);
    let tif = tiff_with_exif_date("2023:07:04 19:22:33");
    let archive = write_archive(
        dir.path(),
        "media.tar",
        &build_tar(&[
            ("photos/shot.png", png.clone()),
            ("photos/meta.tif", tif.clone()),
            ("photos/fake.jpg", b"not a real jpeg".to_vec()),
        ]),
    );

    let summary = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    assert_eq!(summary.images_phashed, 1);
    assert_eq!(summary.images_no_phash, 2);

    let conn = searcher::open_index(&summary.db_path).unwrap();

    let (_, kind, _, _, hash, phash, _, _) = files_row(&conn, "photos/shot.png");
    assert_eq!(kind, "image");
    assert!(phash.is_some());
    assert_eq!(
        hash.as_deref(),
        Some(blake3::hash(&png).as_bytes().as_slice())
    );
    let (w, h): (i64, i64) = conn
        .query_row(
            "SELECT img_w, img_h FROM files WHERE path='photos/shot.png'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((w, h), (64, 48));

    // TIFF: EXIF date extracted, pixel decode fails → flagged, no phash.
    let (_, kind, _, _, _, phash, exif, fl) = files_row(&conn, "photos/meta.tif");
    assert_eq!(kind, "image");
    assert!(phash.is_none());
    assert_eq!(exif, Some(1_688_498_553)); // 2023-07-04T19:22:33Z
    assert_ne!(fl & flags::DECODE_FAILED, 0);

    // Garbage with a .jpg name: flagged, never aborts the run.
    let (_, _, _, _, _, phash, _, fl) = files_row(&conn, "photos/fake.jpg");
    assert!(phash.is_none());
    assert_ne!(fl & flags::DECODE_FAILED, 0);
}

#[test]
fn hardlinks_resolve_and_shadowed_paths_are_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let data = b"the linked content".to_vec();

    let mut builder = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(1_700_000_001);
    h.set_cksum();
    builder
        .append_data(&mut h, "bin/original", data.as_slice())
        .unwrap();

    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Link);
    link.set_size(0);
    link.set_cksum();
    builder
        .append_link(&mut link, "bin/alias", "bin/original")
        .unwrap();

    // Same path twice: the later entry wins on extraction.
    for content in [&b"old version"[..], &b"new version"[..]] {
        let mut h = tar::Header::new_gnu();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        builder.append_data(&mut h, "etc/config", content).unwrap();
    }

    let archive = write_archive(dir.path(), "links.tar", &builder.into_inner().unwrap());
    let summary = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
    assert_eq!(summary.files_link_names, 1);

    let conn = searcher::open_index(&summary.db_path).unwrap();
    let (etype, _, _, _, hash, _, _, _) = files_row(&conn, "bin/alias");
    assert_eq!(etype, "hardlink");
    assert_eq!(
        hash.as_deref(),
        Some(blake3::hash(&data).as_bytes().as_slice())
    );

    let shadow_flags: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT flags FROM files WHERE path='etc/config' ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows
    };
    assert_eq!(shadow_flags.len(), 2);
    assert_ne!(shadow_flags[0] & flags::SHADOWED, 0, "older entry shadowed");
    assert_eq!(shadow_flags[1] & flags::SHADOWED, 0, "final entry wins");
}

#[test]
fn empty_files_get_empty_kind() {
    let dir = tempfile::tempdir().unwrap();
    let archive = write_archive(
        dir.path(),
        "e.tar",
        &build_tar(&[
            ("a/zero.dat", Vec::new()),
            ("a/real.txt", b"words".to_vec()),
        ]),
    );
    let conn = {
        let s = indexer::run_index(&archive, None, &IndexOptions::default()).unwrap();
        searcher::open_index(&s.db_path).unwrap()
    };
    let (_, kind, size, _, hash, _, _, _) = files_row(&conn, "a/zero.dat");
    assert_eq!(kind, "empty");
    assert_eq!(size, 0);
    assert_eq!(
        hash.as_deref(),
        Some(blake3::hash(b"").as_bytes().as_slice())
    );
}

#[test]
fn directory_source_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("photos");
    fs::create_dir_all(src.join("2023")).unwrap();
    fs::write(src.join("2023/note.txt"), b"holiday plans dedupword").unwrap();
    fs::write(src.join("2023/pic.png"), png_bytes(4, 40, 30)).unwrap();
    fs::write(src.join("empty.bin"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("2023/note.txt", src.join("latest")).unwrap();

    let summary = indexer::run_index(&src, None, &IndexOptions::default()).unwrap();
    // Sibling placement: <dir>.db next to the directory, never inside it.
    assert_eq!(summary.db_path, tmp.path().join("photos.db"));
    assert_eq!(summary.format, "directory");
    assert_eq!(summary.files_indexed, 1);
    assert_eq!(summary.images_phashed, 1);
    #[cfg(unix)]
    assert_eq!(summary.files_link_names, 1);

    let conn = searcher::open_index(&summary.db_path).unwrap();
    let (etype, kind, _, mtime, hash, _, _, _) = files_row(&conn, "2023/note.txt");
    assert_eq!((etype.as_str(), kind.as_str()), ("file", "text"));
    assert!(mtime.is_some());
    assert_eq!(
        hash.as_deref(),
        Some(
            blake3::hash(b"holiday plans dedupword")
                .as_bytes()
                .as_slice()
        )
    );

    // FTS search works identically on directory indexes.
    let hits = searcher::search(&conn, "dedupword", 10, false)
        .unwrap()
        .hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "2023/note.txt");

    // Self-exclusion: an explicit db inside the source is not indexed.
    let inner_db = src.join("self.db");
    let s2 = indexer::run_index(&src, Some(&inner_db), &IndexOptions::default()).unwrap();
    let conn2 = searcher::open_index(&s2.db_path).unwrap();
    let n: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path LIKE '%self.db%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}
