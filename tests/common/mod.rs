//! Shared fixtures for v3 integration tests.
//!
//! Each test binary compiles this module separately and none uses every
//! helper, so per-binary dead-code analysis is noise here.
#![allow(dead_code)]

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// Build an uncompressed tar with fixed mtime/mode so metadata is assertable.
pub fn build_tar(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    build_tar_mtime(files, 1_700_000_001)
}

pub fn build_tar_mtime(files: &[(&str, Vec<u8>)], mtime: u64) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        builder
            .append_data(&mut header, path, data.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

pub fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

/// BLAKE3 hex digest of a file's bytes.
pub fn digest_of(path: &Path) -> String {
    blake3::hash(&fs::read(path).unwrap()).to_hex().to_string()
}

/// Inode number (identity) of the entry at `path` itself (no follow).
pub fn inode_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().ino()
}

/// Deterministic small PNG whose pixels vary with `seed`.
pub fn png_bytes(seed: u32, w: u32, h: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(w, h, |x, y| {
        image::Rgb([
            ((x * 7 + y * 3 + seed * 11) % 256) as u8,
            ((x + y * 2 + seed * 5) % 256) as u8,
            if (x / 8 + y / 8 + seed).is_multiple_of(2) {
                200
            } else {
                40
            },
        ])
    }));
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

/// A slightly brightened variant of `png_bytes(seed, ..)` — a near-duplicate
/// with different bytes (different content hash, close perceptual hash).
pub fn png_bytes_brightened(seed: u32, w: u32, h: u32, delta: u8) -> Vec<u8> {
    let base = png_bytes(seed, w, h);
    let img = image::load_from_memory(&base).unwrap().to_rgb8();
    let bright = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(w, h, |x, y| {
        let p = img.get_pixel(x, y).0;
        image::Rgb([
            p[0].saturating_add(delta),
            p[1].saturating_add(delta),
            p[2].saturating_add(delta),
        ])
    }));
    let mut png = Vec::new();
    bright
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

/// Minimal little-endian TIFF carrying only an EXIF DateTimeOriginal —
/// kamadak-exif reads it; the image crate cannot decode it (no pixels), so
/// it also exercises the decode-failure degrade path.
pub fn tiff_with_exif_date(date: &str) -> Vec<u8> {
    fn u16le(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn u32le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend(u16le(tag));
        e.extend(u16le(typ));
        e.extend(u32le(count));
        e.extend(u32le(value));
        e
    }
    let mut t = Vec::new();
    t.extend(b"II");
    t.extend(u16le(42));
    t.extend(u32le(8));
    t.extend(u16le(1)); // IFD0: 1 entry → Exif pointer
    t.extend(entry(0x8769, 4, 1, 26)); // Exif IFD at 26
    t.extend(u32le(0));
    t.extend(u16le(1)); // Exif IFD: 1 entry
    t.extend(entry(0x9003, 2, 20, 44)); // DateTimeOriginal at 44
    t.extend(u32le(0));
    let mut s = date.as_bytes().to_vec();
    s.resize(20, 0);
    t.extend(s);
    t
}

/// Fetch one row of the files table by path (latest entry wins).
#[allow(clippy::type_complexity)]
pub fn files_row(
    conn: &rusqlite::Connection,
    path: &str,
) -> (
    String,          // entry_type
    String,          // kind
    i64,             // size
    Option<i64>,     // mtime_unix
    Option<Vec<u8>>, // content_hash
    Option<i64>,     // phash
    Option<i64>,     // exif_unix
    i64,             // flags
) {
    conn.query_row(
        "SELECT entry_type, kind, size, mtime_unix, content_hash, phash, exif_unix, flags
         FROM files WHERE path = ?1 ORDER BY id DESC LIMIT 1",
        [path],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        },
    )
    .unwrap_or_else(|e| panic!("no files row for '{path}': {e}"))
}
