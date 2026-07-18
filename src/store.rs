//! Schema v3 per-source database: creation, entry inserts, finalisation.
//!
//! v3 keeps v2's `files_fts` / `word_freq` / `meta` exactly and adds the
//! `files` metadata table, paired to FTS rows by explicit rowid
//! (`files.id == files_fts.rowid`). Every entry gets a `files_fts` row —
//! empty content for binaries and links — so filename search keeps working
//! exactly as in v0.2.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

pub const SCHEMA_VERSION: i64 = 3;

/// Bit flags on `files.flags` (spec §4).
pub mod flags {
    /// Content beyond the text cap is not FTS-searchable (hash is still full).
    pub const FTS_TRUNCATED: i64 = 1;
    /// Image entry larger than the media cap — not decoded, no phash/dims.
    pub const IMAGE_OVER_CAP: i64 = 2;
    /// Read error mid-entry — name-only row.
    pub const READ_ERROR: i64 = 4;
    /// GNU/PAX sparse entry — hash covers the condensed stream, not the file.
    pub const SPARSE: i64 = 8;
    /// A later entry in the same source has the same path and wins on extract.
    pub const SHADOWED: i64 = 16;
    /// Image decode failed or hit limits — no phash/dims.
    pub const DECODE_FAILED: i64 = 32;
}

/// Static description of the source being indexed, written into `meta`.
pub struct SourceMeta<'a> {
    pub source: &'a Path,
    /// `"tar"` or `"dir"`.
    pub source_type: &'a str,
    pub text_cap: u64,
    pub media_cap: u64,
    pub word_stats: bool,
}

/// One indexed entry. `fts_content` is empty for binaries/links — the row is
/// still inserted so the path stays searchable.
pub struct EntryRecord<'a> {
    pub path: &'a str,
    /// `"file"` | `"symlink"` | `"hardlink"`.
    pub entry_type: &'a str,
    pub link_target: Option<&'a str>,
    pub size: u64,
    pub mtime_unix: Option<i64>,
    pub mode: Option<u32>,
    /// `"text"|"image"|"raw"|"video"|"binary"|"link"|"empty"`.
    pub kind: &'a str,
    pub content_hash: Option<[u8; 32]>,
    pub img_w: Option<u32>,
    pub img_h: Option<u32>,
    pub phash: Option<u64>,
    pub exif_unix: Option<i64>,
    pub exif_src: Option<&'a str>,
    pub flags: i64,
    pub fts_content: &'a str,
}

/// Totals recorded into meta by [`finalize_v3`].
#[derive(Debug, Default, Clone)]
pub struct FinalizeCounts {
    pub files_indexed: u64,
    pub files_skipped_binary: u64,
    pub files_truncated: u64,
}

/// Delete any stale database (plus WAL/SHM) and create a fresh v3 schema.
pub fn create_v3(db_path: &Path, meta: &SourceMeta) -> Result<Connection> {
    for stale in [
        db_path.to_path_buf(),
        sibling(db_path, "-wal"),
        sibling(db_path, "-shm"),
    ] {
        if stale.exists() {
            fs::remove_file(&stale)
                .with_context(|| format!("failed to remove old index file: {}", stale.display()))?;
        }
    }

    let conn = Connection::open(db_path)
        .with_context(|| format!("cannot create database at {}", db_path.display()))?;

    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-65536;",
    )?;

    conn.execute_batch(
        "CREATE VIRTUAL TABLE files_fts USING fts5(
            path,
            content,
            tokenize = 'unicode61'
        );
        CREATE TABLE word_freq (
            word        TEXT PRIMARY KEY,
            total_count INTEGER NOT NULL DEFAULT 0,
            doc_count   INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE files (
            id            INTEGER PRIMARY KEY,
            path          TEXT NOT NULL,
            entry_type    TEXT NOT NULL,
            link_target   TEXT,
            size          INTEGER NOT NULL DEFAULT 0,
            mtime_unix    INTEGER,
            mode          INTEGER,
            kind          TEXT NOT NULL,
            content_hash  BLOB,
            img_w         INTEGER,
            img_h         INTEGER,
            phash         INTEGER,
            exif_unix     INTEGER,
            exif_src      TEXT,
            flags         INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_files_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
        CREATE INDEX idx_files_path ON files(path);",
    )?;

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut uuid_bytes = [0u8; 16];
    getrandom::fill(&mut uuid_bytes)
        .map_err(|e| anyhow::anyhow!("failed to generate index_uuid: {e}"))?;
    let index_uuid: String = uuid_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let source_str = meta.source.display().to_string();
    set_meta(&conn, "schema_version", &SCHEMA_VERSION.to_string())?;
    set_meta(&conn, "index_uuid", &index_uuid)?;
    set_meta(&conn, "source", &source_str)?;
    // v2 readers look for "archive"; keep writing it.
    set_meta(&conn, "archive", &source_str)?;
    set_meta(&conn, "source_type", meta.source_type)?;
    set_meta(&conn, "hash_algo", "blake3")?;
    set_meta(&conn, "phash_algo", crate::phash::PHASH_ALGO)?;
    set_meta(&conn, "text_cap", &meta.text_cap.to_string())?;
    set_meta(&conn, "media_cap", &meta.media_cap.to_string())?;
    set_meta(&conn, "created_unix", &created.to_string())?;
    set_meta(&conn, "word_stats", if meta.word_stats { "1" } else { "0" })?;
    set_meta(&conn, "completed", "0")?;

    // Stat the source itself (archive-vs-index staleness, spec §6).
    if let Ok(md) = fs::metadata(meta.source) {
        set_meta(&conn, "archive_size", &md.len().to_string())?;
        if let Ok(mtime) = md.modified() {
            if let Ok(d) = mtime.duration_since(UNIX_EPOCH) {
                set_meta(&conn, "archive_mtime_unix", &d.as_secs().to_string())?;
            }
        }
    }

    Ok(conn)
}

/// Insert one entry; returns `files.id` (== the FTS rowid).
pub fn insert_entry(conn: &Connection, rec: &EntryRecord) -> Result<i64> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO files (path, entry_type, link_target, size, mtime_unix, mode,
                            kind, content_hash, img_w, img_h, phash, exif_unix,
                            exif_src, flags)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
    )?;
    stmt.execute(params![
        rec.path,
        rec.entry_type,
        rec.link_target,
        rec.size as i64,
        rec.mtime_unix,
        rec.mode,
        rec.kind,
        rec.content_hash.as_ref().map(|h| h.as_slice()),
        rec.img_w,
        rec.img_h,
        rec.phash.map(|p| p as i64),
        rec.exif_unix,
        rec.exif_src,
        rec.flags,
    ])
    .with_context(|| format!("files insert failed for '{}'", rec.path))?;
    let id = conn.last_insert_rowid();

    let mut fts =
        conn.prepare_cached("INSERT INTO files_fts(rowid, path, content) VALUES (?1, ?2, ?3)")?;
    fts.execute(params![id, rec.path, rec.fts_content])
        .with_context(|| format!("FTS5 insert failed for '{}'", rec.path))?;
    Ok(id)
}

/// Post-passes + completion. Runs inside the caller's open transaction:
/// hardlink hash resolution, shadowed-path marking, the word_freq index,
/// final counters, `completed=1`. The caller commits and then calls
/// [`fold_wal`].
pub fn finalize_v3(
    conn: &Connection,
    counts: &FinalizeCounts,
    archive_blake3: Option<&str>,
) -> Result<()> {
    // Hardlink hashes: copy from the entry the link points at (latest entry
    // wins when the path is shadowed — that is what extraction produces).
    // Indexed by idx_files_path; iterate to resolve link→link chains.
    for _ in 0..5 {
        let changed = conn.execute(
            "UPDATE files SET content_hash = (
                 SELECT t.content_hash FROM files t
                 WHERE t.path = files.link_target AND t.content_hash IS NOT NULL
                 ORDER BY t.id DESC LIMIT 1)
             WHERE entry_type = 'hardlink'
               AND content_hash IS NULL
               AND link_target IS NOT NULL
               AND EXISTS (SELECT 1 FROM files t
                           WHERE t.path = files.link_target
                             AND t.content_hash IS NOT NULL)",
            [],
        )?;
        if changed == 0 {
            break;
        }
    }

    // Shadowed paths: tar allows the same path twice; the last entry wins on
    // extract. Mark every non-final row (unique paths are their own MAX).
    conn.execute(
        &format!(
            "UPDATE files SET flags = flags | {} WHERE id NOT IN
             (SELECT MAX(id) FROM files GROUP BY path)",
            flags::SHADOWED
        ),
        [],
    )?;

    conn.execute_batch("CREATE INDEX idx_word_freq_total ON word_freq(total_count DESC)")?;

    set_meta(conn, "files_indexed", &counts.files_indexed.to_string())?;
    set_meta(
        conn,
        "files_skipped",
        &counts.files_skipped_binary.to_string(),
    )?;
    set_meta(conn, "files_truncated", &counts.files_truncated.to_string())?;
    if let Some(hash) = archive_blake3 {
        set_meta(conn, "archive_blake3", hash)?;
    }
    set_meta(conn, "completed", "1")?;
    Ok(())
}

/// Fold the WAL back so the finished index is one portable file.
pub fn fold_wal(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA optimize;")?;
    Ok(())
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Schema version of an open index: `Some(2)`/`Some(3)`… or `None` for a
/// v0.1 database that predates the meta table.
pub fn schema_version(conn: &Connection) -> Option<i64> {
    crate::searcher::get_meta(conn, "schema_version").and_then(|v| v.parse().ok())
}

fn sibling(db_path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_meta(source: &Path) -> SourceMeta<'_> {
        SourceMeta {
            source,
            source_type: "tar",
            text_cap: 16 * 1024 * 1024,
            media_cap: 64 * 1024 * 1024,
            word_stats: true,
        }
    }

    fn entry<'a>(path: &'a str, content: &'a str, hash_byte: u8) -> EntryRecord<'a> {
        EntryRecord {
            path,
            entry_type: "file",
            link_target: None,
            size: content.len() as u64,
            mtime_unix: Some(1_700_000_000),
            mode: Some(0o644),
            kind: "text",
            content_hash: Some([hash_byte; 32]),
            img_w: None,
            img_h: None,
            phash: None,
            exif_unix: None,
            exif_src: None,
            flags: 0,
            fts_content: content,
        }
    }

    #[test]
    fn v3_creation_meta_and_fts_pairing() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let conn = create_v3(&db, &test_meta(Path::new("/backups/x.tar"))).unwrap();

        assert_eq!(schema_version(&conn), Some(3));
        let uuid = crate::searcher::get_meta(&conn, "index_uuid").unwrap();
        assert_eq!(uuid.len(), 32);
        assert_eq!(
            crate::searcher::get_meta(&conn, "phash_algo").as_deref(),
            Some(crate::phash::PHASH_ALGO)
        );
        assert_eq!(
            crate::searcher::get_meta(&conn, "archive").as_deref(),
            Some("/backups/x.tar")
        );

        let id = insert_entry(&conn, &entry("docs/readme.txt", "hello dedup world", 1)).unwrap();
        // FTS row must share the rowid so search can join metadata.
        let (fts_rowid, path): (i64, String) = conn
            .query_row(
                "SELECT rowid, path FROM files_fts WHERE files_fts MATCH 'dedup'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(fts_rowid, id);
        assert_eq!(path, "docs/readme.txt");
    }

    #[test]
    fn hardlink_chains_resolve_and_shadowing_marks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        let conn = create_v3(&db, &test_meta(Path::new("/b/y.tar"))).unwrap();

        insert_entry(&conn, &entry("data/original.bin", "", 7)).unwrap();
        let link = |path, target| EntryRecord {
            entry_type: "hardlink",
            link_target: Some(target),
            content_hash: None,
            kind: "link",
            fts_content: "",
            ..entry(path, "", 0)
        };
        insert_entry(&conn, &link("data/link1", "data/original.bin")).unwrap();
        insert_entry(&conn, &link("data/link2", "data/link1")).unwrap(); // chain

        // Shadowing: same path twice — the later wins, earlier is flagged.
        insert_entry(&conn, &entry("etc/config", "old contents", 2)).unwrap();
        insert_entry(&conn, &entry("etc/config", "new contents", 3)).unwrap();

        finalize_v3(&conn, &FinalizeCounts::default(), Some("deadbeef")).unwrap();

        let hash_of = |p: &str| -> Option<Vec<u8>> {
            conn.query_row(
                "SELECT content_hash FROM files WHERE path=?1 ORDER BY id DESC LIMIT 1",
                [p],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hash_of("data/link1"), Some(vec![7u8; 32]));
        assert_eq!(hash_of("data/link2"), Some(vec![7u8; 32])); // via chain

        let shadowed: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM files WHERE flags & {} != 0",
                    flags::SHADOWED
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(shadowed, 1);
        let (sh_flags,): (i64,) = conn
            .query_row(
                "SELECT flags FROM files WHERE path='etc/config' ORDER BY id ASC LIMIT 1",
                [],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_ne!(sh_flags & flags::SHADOWED, 0);

        assert_eq!(
            crate::searcher::get_meta(&conn, "completed").as_deref(),
            Some("1")
        );
        assert_eq!(
            crate::searcher::get_meta(&conn, "archive_blake3").as_deref(),
            Some("deadbeef")
        );
    }
}
