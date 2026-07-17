//! Streams a tar archive (plain, gzip or zstd) entry-by-entry and indexes
//! text content into SQLite FTS5, without extracting anything to disk.
//!
//! Reader chain: File → progress tracker → BufReader → decompressor → tar.
//! Each entry is read once into a single capped buffer and converted to UTF-8
//! in one pass — v0.1 converted fixed-size chunks separately, which corrupted
//! multi-byte characters straddling chunk boundaries and made those words
//! unsearchable.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{params, Connection};

use crate::format::{self, Format};

/// Bytes sampled from the start of each file for the null-byte binary check
/// (the same heuristic git and file(1) use).
const BINARY_PROBE: usize = 8 * 1024;

/// Archive entries per SQLite transaction. One commit per entry would fsync
/// per row and be orders of magnitude slower.
const BATCH_SIZE: usize = 500;

/// Flush the in-memory word-frequency map to SQLite once it holds this many
/// distinct words, bounding memory on archives with huge vocabularies.
const WORD_FLUSH_THRESHOLD: usize = 100_000;

/// Tokens outside this length range (in characters) are excluded from word
/// statistics. FTS5 indexes them regardless, so search is unaffected — this
/// only keeps `top` output and the word_freq table sane.
const MIN_WORD_CHARS: usize = 3;
const MAX_WORD_CHARS: usize = 32;

pub const DEFAULT_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
const SCHEMA_VERSION: &str = "2";

pub struct IndexOptions {
    /// Per-file content cap in bytes; content beyond it is not indexed.
    pub max_file_size: u64,
    /// Whether to maintain the word_freq table used by `top`.
    pub word_stats: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            word_stats: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct IndexSummary {
    pub db_path: PathBuf,
    pub format: String,
    pub files_indexed: u64,
    pub files_skipped_binary: u64,
    pub files_truncated: u64,
    /// Hard links and symlinks, indexed by name only.
    pub files_link_names: u64,
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Index the archive at `archive_path` into a fresh SQLite database.
///
/// Any existing database at the target path is replaced. Prints a progress
/// bar and per-entry warnings; returns the totals.
pub fn run_index(
    archive_path: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
) -> Result<IndexSummary> {
    let fmt = format::detect_file(archive_path)?;
    let db_path = resolve_db_path(archive_path, explicit_db);

    let (conn, db_path) = match create_database(&db_path, archive_path, opts) {
        Ok(conn) => (conn, db_path),
        // Default location not writable (e.g. archive on a read-only mount):
        // fall back to the current directory, keeping the per-archive name.
        Err(e) if explicit_db.is_none() => {
            let fallback = PathBuf::from(db_file_name(archive_path));
            eprintln!(
                "warning: cannot create index at '{}' ({e:#}) — using './{}'",
                db_path.display(),
                fallback.display()
            );
            let conn = create_database(&fallback, archive_path, opts)?;
            (conn, fallback)
        }
        Err(e) => return Err(e),
    };

    println!("Archive : {} ({fmt})", archive_path.display());
    println!("Index   : {}", db_path.display());
    println!();

    let archive_file = File::open(archive_path)
        .with_context(|| format!("cannot open archive: {}", archive_path.display()))?;
    let total_bytes = archive_file.metadata().map(|m| m.len()).unwrap_or(0);

    // The progress bar tracks compressed bytes consumed from the file.
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta}) — {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );

    let tracked = BufReader::with_capacity(256 * 1024, pb.wrap_read(archive_file));
    let reader: Box<dyn Read> = match fmt {
        // `with_buffer` reuses our BufReader instead of stacking another one.
        Format::Zstd => Box::new(
            zstd::Decoder::with_buffer(tracked).context("failed to initialise zstd decoder")?,
        ),
        // MultiGzDecoder also handles concatenated gzip members (pigz etc.).
        Format::Gzip => Box::new(flate2::read::MultiGzDecoder::new(tracked)),
        Format::PlainTar => Box::new(tracked),
    };
    let mut tar_archive = tar::Archive::new(reader);

    let mut summary = IndexSummary {
        db_path: db_path.clone(),
        format: fmt.to_string(),
        ..IndexSummary::default()
    };
    // word → (total occurrences, number of docs), flushed periodically.
    let mut word_buf: HashMap<String, (u64, u64)> = HashMap::new();
    let mut in_batch = 0usize;
    let mut entry_no = 0u64;

    conn.execute_batch("BEGIN")?;

    for entry_result in tar_archive
        .entries()
        .context("failed to read tar entries")?
    {
        // A corrupt entry header is fatal: tar's iterator cannot resync past
        // it and permanently returns None afterwards, so "skip and continue"
        // would silently drop the rest of the archive and still report
        // success. Bail instead — completed stays 0, so searches on the
        // partial database warn.
        let mut entry = entry_result.with_context(|| {
            format!(
                "corrupt tar entry after {entry_no} readable entries — \
                 the index is incomplete"
            )
        })?;

        let entry_type = entry.header().entry_type();
        // Content lives in regular files (plus GNU sparse/contiguous
        // variants). Hard links and symlinks are alternate names for content
        // stored elsewhere — index the name so it is at least findable.
        let content_entry = entry_type.is_file()
            || matches!(
                entry_type,
                tar::EntryType::GNUSparse | tar::EntryType::Continuous
            );
        let name_only_entry = matches!(entry_type, tar::EntryType::Link | tar::EntryType::Symlink);
        if !content_entry && !name_only_entry {
            continue; // directories, device nodes, fifos, pax metadata
        }

        let entry_path = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from("<unreadable-path>"));

        entry_no += 1;
        if entry_no % 64 == 1 {
            pb.set_message(truncate_path(&entry_path, 50));
        }

        if name_only_entry {
            insert_file(&conn, &entry_path, "")?;
            summary.files_link_names += 1;
        } else {
            // Read once, up to the cap. The tar header knows the entry size,
            // but don't trust it for preallocation — a corrupt header could
            // claim petabytes.
            let size = entry.size();
            let to_read = size.min(opts.max_file_size);
            let mut buf = Vec::with_capacity(to_read.min(1024 * 1024) as usize);
            if let Err(e) = entry.by_ref().take(to_read).read_to_end(&mut buf) {
                pb.suspend(|| eprintln!("warning: read error in '{entry_path}': {e}"));
                buf.clear(); // fall through and index the file name at least
            }
            if size > opts.max_file_size {
                summary.files_truncated += 1;
            }

            if is_binary(&buf[..buf.len().min(BINARY_PROBE)]) {
                insert_file(&conn, &entry_path, "")?; // name only
                summary.files_skipped_binary += 1;
            } else {
                // One lossy conversion over the whole buffer: multi-byte
                // chars can no longer be split across read boundaries.
                let mut content = String::from_utf8_lossy(&buf);
                // 0x01 is the search-time highlight marker; strip it so
                // per-file match counts cannot be inflated by files that
                // happen to contain it.
                if content.contains('\u{1}') {
                    content = content.replace('\u{1}', "").into();
                }
                insert_file(&conn, &entry_path, &content)?;
                if opts.word_stats {
                    accumulate_words(&content, &mut word_buf);
                    if word_buf.len() >= WORD_FLUSH_THRESHOLD {
                        flush_word_stats(&conn, &mut word_buf)?;
                    }
                }
                summary.files_indexed += 1;
            }
        }

        in_batch += 1;
        if in_batch >= BATCH_SIZE {
            conn.execute_batch("COMMIT; BEGIN")?;
            in_batch = 0;
        }
    }

    flush_word_stats(&conn, &mut word_buf)?;
    // Created only now: building it during the load would make every
    // word_freq upsert also rebalance this b-tree.
    conn.execute_batch("CREATE INDEX idx_word_freq_total ON word_freq(total_count DESC)")?;
    mark_complete(&conn, &summary)?;
    conn.execute_batch("COMMIT")?;
    // Fold the WAL back into the main file so the finished index is a single
    // file that can live on (and be opened read-only from) any medium.
    conn.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA optimize;")?;

    pb.finish_with_message("done");
    Ok(summary)
}

// ── Database ─────────────────────────────────────────────────────────────────

/// Delete any stale database (plus WAL/SHM siblings) and create a fresh one
/// with the FTS5 schema.
fn create_database(db_path: &Path, archive_path: &Path, opts: &IndexOptions) -> Result<Connection> {
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

    // WAL + NORMAL sync: fast bulk loading, still crash-safe. The index is
    // rebuildable, so we don't pay for FULL durability.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-65536;",
    )?;

    // NOTE: this FTS5 table stores the full text content (that is what makes
    // highlight()/snippet() and match counts work at search time). Expect the
    // database to be roughly the size of the text it indexes.
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
        );",
    )?;

    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    set_meta(&conn, "schema_version", SCHEMA_VERSION)?;
    set_meta(&conn, "archive", &archive_path.display().to_string())?;
    set_meta(&conn, "created_unix", &created.to_string())?;
    set_meta(&conn, "word_stats", if opts.word_stats { "1" } else { "0" })?;
    set_meta(&conn, "completed", "0")?;

    Ok(conn)
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Record final totals and flip the completed flag. A database without
/// completed=1 was interrupted mid-index and searches on it are partial.
fn mark_complete(conn: &Connection, summary: &IndexSummary) -> Result<()> {
    set_meta(conn, "files_indexed", &summary.files_indexed.to_string())?;
    set_meta(
        conn,
        "files_skipped",
        &summary.files_skipped_binary.to_string(),
    )?;
    set_meta(
        conn,
        "files_truncated",
        &summary.files_truncated.to_string(),
    )?;
    set_meta(conn, "completed", "1")?;
    Ok(())
}

fn insert_file(conn: &Connection, path: &str, content: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("INSERT INTO files_fts(path, content) VALUES (?1, ?2)")?;
    stmt.execute(params![path, content])
        .with_context(|| format!("FTS5 insert failed for '{path}'"))?;
    Ok(())
}

// ── Word statistics ──────────────────────────────────────────────────────────

/// Tokenise `text` and merge its per-document counts into `acc`.
///
/// Tokenisation approximates FTS5's unicode61 (split on non-alphanumeric,
/// case-fold); it only feeds the `top` statistics, not search.
fn accumulate_words(text: &str, acc: &mut HashMap<String, (u64, u64)>) {
    let mut doc_counts: HashMap<String, u64> = HashMap::new();
    for token in text.split(|c: char| !c.is_alphanumeric()) {
        // Cheap byte-length pre-filter before the exact char count.
        if token.len() < MIN_WORD_CHARS || token.len() > MAX_WORD_CHARS * 4 {
            continue;
        }
        let chars = token.chars().count();
        if !(MIN_WORD_CHARS..=MAX_WORD_CHARS).contains(&chars) {
            continue;
        }
        *doc_counts.entry(token.to_lowercase()).or_insert(0) += 1;
    }
    for (word, count) in doc_counts {
        let entry = acc.entry(word).or_insert((0, 0));
        entry.0 += count;
        entry.1 += 1;
    }
}

/// Write the accumulated word counts with one upsert per distinct word.
/// v0.1 ran two statements per word per document instead, which took hours
/// on large archives.
fn flush_word_stats(conn: &Connection, acc: &mut HashMap<String, (u64, u64)>) -> Result<()> {
    if acc.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO word_freq(word, total_count, doc_count) VALUES (?1, ?2, ?3)
         ON CONFLICT(word) DO UPDATE SET
            total_count = total_count + excluded.total_count,
            doc_count   = doc_count + excluded.doc_count",
    )?;
    for (word, (total, docs)) in acc.drain() {
        stmt.execute(params![word, total as i64, docs as i64])?;
    }
    Ok(())
}

// ── Paths and helpers ────────────────────────────────────────────────────────

fn db_file_name(archive_path: &Path) -> String {
    format!(
        "{}.db",
        archive_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    )
}

/// Default database location: `<archive>.db` next to the archive, unless an
/// explicit path was given.
pub fn resolve_db_path(archive_path: &Path, explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => archive_path.with_file_name(db_file_name(archive_path)),
    }
}

fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    db_path.with_file_name(name)
}

/// True if `data` looks like binary content (contains a null byte).
fn is_binary(data: &[u8]) -> bool {
    data.contains(&0)
}

/// Truncate a path to `max_chars` characters, keeping the tail (file name)
/// visible.
fn truncate_path(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count + 1 - max_chars).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\x00def"));
        assert!(!is_binary(b"plain text, nothing to see"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn tokeniser_filters_and_folds() {
        let mut acc = HashMap::new();
        accumulate_words("Foo foo BAR ab x 123 Grüße", &mut acc);
        assert_eq!(acc.get("foo"), Some(&(2, 1)));
        assert_eq!(acc.get("bar"), Some(&(1, 1)));
        assert_eq!(acc.get("123"), Some(&(1, 1)));
        assert_eq!(acc.get("grüße"), Some(&(1, 1)));
        assert!(!acc.contains_key("ab")); // below MIN_WORD_CHARS
        assert!(!acc.contains_key("x"));
    }

    #[test]
    fn tokeniser_drops_monster_tokens() {
        let mut acc = HashMap::new();
        accumulate_words(&"a".repeat(MAX_WORD_CHARS + 1), &mut acc);
        assert!(acc.is_empty());
    }

    #[test]
    fn doc_count_across_documents() {
        let mut acc = HashMap::new();
        accumulate_words("apple apple", &mut acc);
        accumulate_words("apple banana", &mut acc);
        assert_eq!(acc.get("apple"), Some(&(3, 2)));
        assert_eq!(acc.get("banana"), Some(&(1, 1)));
    }

    #[test]
    fn truncate_keeps_tail() {
        assert_eq!(truncate_path("short", 10), "short");
        let t = truncate_path("a/very/long/path/to/some/file.txt", 12);
        assert_eq!(t.chars().count(), 12);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("file.txt"));
    }

    #[test]
    fn default_db_path_next_to_archive() {
        let p = resolve_db_path(Path::new("/backups/sys.tar.zst"), None);
        assert_eq!(p, Path::new("/backups/sys.tar.zst.db"));
        let e = resolve_db_path(
            Path::new("/backups/sys.tar.zst"),
            Some(Path::new("/tmp/x.db")),
        );
        assert_eq!(e, Path::new("/tmp/x.db"));
    }
}
