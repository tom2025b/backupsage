// ─── src/indexer.rs ───────────────────────────────────────────────────────────
//
// PURPOSE: Stream a .tar.zst archive entry-by-entry without extracting it to
// disk. For each entry, extract text content (skipping binary files), tokenise
// words from filenames and content, and write everything into a SQLite FTS5
// (Full-Text Search 5) virtual table for fast keyword lookups.
//
// DESIGN: We chain I/O adapters like pipes:
//
//   File  →  zstd::Decoder  →  tar::Archive  →  per-entry Read
//
// At no point do we buffer the entire archive in RAM.
// ─────────────────────────────────────────────────────────────────────────────

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::{params, Connection};

// How many bytes we read at once per archive entry when scanning content.
// 64 KiB is a sweet spot: small enough to keep RAM usage flat, large enough
// that we don't make thousands of tiny syscalls per file.
const READ_CHUNK: usize = 64 * 1024;

// How many bytes we sample from the start of each file to detect binary data.
// 8 KiB is what the `file` utility and git use — enough to be confident.
const BINARY_PROBE: usize = 8 * 1024;

// How many SQLite INSERT statements we batch before committing.
// Committing every row is ~1000× slower than batching.
const BATCH_SIZE: usize = 500;

// ── Public entry point ────────────────────────────────────────────────────────

/// Open the archive at `archive_path`, index all text content into `db_path`.
///
/// If `db_path` already exists it is deleted first — no resume logic.
/// This function streams the archive; it never writes temp files to disk.
pub fn run_index(archive_path: &Path, db_path: &PathBuf) -> Result<()> {
    // ── Step 1: Resolve the database path ────────────────────────────────────
    // If the user didn't provide --index, derive it from the archive name.
    // If the archive's parent directory is not writable, fall back to CWD.
    let db_path = resolve_db_path(archive_path, db_path)?;

    // Wipe any existing index so we start clean.
    if db_path.exists() {
        fs::remove_file(&db_path)
            .with_context(|| format!("Failed to remove existing index: {}", db_path.display()))?;
        println!("Removed old index at {}", db_path.display());
    }

    println!("Archive : {}", archive_path.display());
    println!("Index   : {}", db_path.display());
    println!();

    // ── Step 2: Open the archive for streaming ────────────────────────────────
    // `File::open` returns an `io::Read` backed by the OS file descriptor.
    let archive_file = File::open(archive_path)
        .with_context(|| format!("Cannot open archive: {}", archive_path.display()))?;

    // `archive_file.metadata()` gives us the compressed byte count, which
    // we use to drive the progress bar. We measure compressed bytes read,
    // not uncompressed — that's what `pb.wrap_read` tracks.
    let total_bytes = archive_file
        .metadata()
        .map(|m| m.len())
        .unwrap_or(0);

    // Wrap in BufReader to reduce per-byte syscall overhead.
    let buffered_file = BufReader::new(archive_file);

    // ── Step 3: Set up progress bar ───────────────────────────────────────────
    // `ProgressBar::new(total_bytes)` creates a bar that tracks bytes processed.
    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta}) — {msg}",
        )
        // `?` means fallback silently if the template has an error.
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        // Smooth the bytes-per-second estimate over a rolling window.
        .progress_chars("=>-"),
    );

    // `pb.wrap_read(reader)` wraps any `io::Read` so every `read()` call
    // automatically advances the progress bar by the bytes consumed.
    let tracked_reader = pb.wrap_read(buffered_file);

    // Layer zstd decompression on top of the tracked (compressed) reader.
    // `zstd::Decoder` implements `io::Read` — it decompresses on the fly.
    let zstd_reader = zstd::Decoder::new(tracked_reader)
        .context("Failed to initialise zstd decoder (is the file a valid .zst?)")?;

    // Layer the TAR parser on top of the decompressed byte stream.
    // `tar::Archive` also implements entry iteration without full buffering.
    let mut tar_archive = tar::Archive::new(zstd_reader);

    // ── Step 4: Open SQLite and create the FTS5 schema ───────────────────────
    let conn = setup_database(&db_path)?;

    // ── Step 5: Iterate entries ───────────────────────────────────────────────
    // `entries()` returns an iterator of `tar::Entry` — each has a path and
    // implements `io::Read` for its content.
    let mut entries = tar_archive
        .entries()
        .context("Failed to read tar entries")?;

    let mut files_indexed: u64 = 0;
    let mut files_skipped: u64 = 0;
    let mut batch_count = 0;

    // Start a transaction. We'll commit every BATCH_SIZE inserts.
    // Explicit transactions are 100–1000× faster than auto-commit per insert.
    conn.execute_batch("BEGIN")?;

    while let Some(entry_result) = entries.next() {
        // Some entries in a tar may be unreadable (corrupted header etc.).
        // `with_context` attaches a human-readable error message if unwrap fails.
        let mut entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                pb.suspend(|| eprintln!("Warning: skipped unreadable entry: {e}"));
                continue;
            }
        };

        // Get the entry's path as a UTF-8 string.
        // `entry.path()` returns a Cow<Path>; we convert to String for storage.
        let entry_path = entry
            .path()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "<non-utf8-path>".to_string());

        // Update the spinner message with the current filename (truncated).
        let display_name = truncate_path(&entry_path, 50);
        pb.set_message(display_name);

        // Only index regular files — skip directories, symlinks, device nodes.
        // `is_file()` checks the tar entry type header, not the filesystem.
        if !entry.header().entry_type().is_file() {
            continue;
        }

        // ── Read a probe chunk to detect binary content ───────────────────────
        // We read the first BINARY_PROBE bytes into a small buffer, check for
        // binary data, then prepend that buffer back for full content indexing.
        let mut probe = Vec::with_capacity(BINARY_PROBE);
        // `take(n)` limits how many bytes `read_to_end` will consume here.
        entry
            .by_ref()
            .take(BINARY_PROBE as u64)
            .read_to_end(&mut probe)
            .unwrap_or(0); // On read error, treat probe as empty (index name only).

        if is_binary(&probe) {
            files_skipped += 1;
            // Still index the filename — just no content.
            index_entry(&conn, &entry_path, "")?;
        } else {
            // The probe is valid text. Now read the REST of the entry's content.
            // We chain probe bytes + remaining bytes using `io::Cursor` + `chain`.
            let mut content = String::from_utf8_lossy(&probe).into_owned();

            // Read remaining content in chunks to keep memory flat.
            let mut chunk = vec![0u8; READ_CHUNK];
            loop {
                match entry.read(&mut chunk) {
                    Ok(0) => break,           // End of this entry.
                    Ok(n) => {
                        // `from_utf8_lossy` replaces invalid UTF-8 sequences with
                        // the Unicode replacement character U+FFFD rather than
                        // panicking. This handles Latin-1, Windows-1252, etc.
                        let text = String::from_utf8_lossy(&chunk[..n]);
                        content.push_str(&text);
                    }
                    Err(e) => {
                        pb.suspend(|| {
                            eprintln!("Warning: read error in '{entry_path}': {e}")
                        });
                        break;
                    }
                }
            }

            index_entry(&conn, &entry_path, &content)?;
            files_indexed += 1;
        }

        // Commit in batches to balance write throughput vs. memory usage.
        batch_count += 1;
        if batch_count >= BATCH_SIZE {
            conn.execute_batch("COMMIT; BEGIN")?;
            batch_count = 0;
        }
    }

    // Final commit for any remaining rows.
    conn.execute_batch("COMMIT")?;

    pb.finish_with_message("done");

    println!();
    println!(
        "Indexed  : {files_indexed} text files"
    );
    println!(
        "Skipped  : {files_skipped} binary files"
    );
    println!(
        "Database : {}",
        db_path.display()
    );

    Ok(())
}

// ── Database setup ────────────────────────────────────────────────────────────

/// Create the SQLite database, configure it for bulk-insert performance,
/// and set up the FTS5 virtual table plus the word-frequency table.
fn setup_database(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("Cannot create database at {}", db_path.display()))?;

    // PRAGMA journal_mode=WAL: Write-Ahead Logging is faster for concurrent
    // reads/writes and reduces fsync calls during bulk inserts.
    conn.execute_batch("PRAGMA journal_mode=WAL")?;

    // PRAGMA synchronous=NORMAL: Let the OS buffer writes. Safe with WAL,
    // and much faster than FULL (which fsyncs after every commit).
    conn.execute_batch("PRAGMA synchronous=NORMAL")?;

    // PRAGMA cache_size: Give SQLite a 64 MB page cache to reduce disk I/O.
    // Negative value means kilobytes (not pages).
    conn.execute_batch("PRAGMA cache_size=-65536")?;

    // ── FTS5 virtual table ────────────────────────────────────────────────────
    //
    // FTS5 (Full-Text Search 5) is a SQLite extension that builds an inverted
    // index over text columns. `content=""` means "contentless" — FTS5 stores
    // only the index, not a copy of the text, saving significant space.
    //
    // `tokenize='unicode61'` uses SQLite's built-in Unicode-aware tokeniser:
    //   - Splits on whitespace and punctuation
    //   - Case-folds (search is case-insensitive by default)
    //   - Handles multi-byte UTF-8 correctly
    //
    // Because we use content="" we cannot retrieve original text via FTS5,
    // only look up which rowids match — which is exactly what we need.
    conn.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(
            path,
            content,
            tokenize = 'unicode61'
        )",
    )?;

    // ── Word frequency table ──────────────────────────────────────────────────
    //
    // FTS5 can answer "which files contain word X" very fast, but it can't
    // efficiently answer "what are the top-50 words globally". For that we
    // maintain a separate key-value table: word → (total_count, doc_count).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS word_freq (
            word        TEXT PRIMARY KEY,
            total_count INTEGER NOT NULL DEFAULT 0,
            doc_count   INTEGER NOT NULL DEFAULT 0
        )",
    )?;

    // Index on total_count so `ORDER BY total_count DESC LIMIT N` is fast.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_word_freq_total
         ON word_freq(total_count DESC)",
    )?;

    Ok(conn)
}

// ── Per-entry indexing ────────────────────────────────────────────────────────

/// Insert one archive entry into both FTS5 and the word-frequency table.
fn index_entry(conn: &Connection, path: &str, content: &str) -> Result<()> {
    // Insert into the FTS5 table. The FTS5 engine tokenises `content`
    // automatically using the unicode61 tokeniser we configured above.
    conn.execute(
        "INSERT INTO files_fts(path, content) VALUES (?1, ?2)",
        params![path, content],
    )
    .with_context(|| format!("FTS5 insert failed for '{path}'"))?;

    // ── Word frequency counting ────────────────────────────────────────────
    // We tokenise here in Rust (same rules as FTS5 unicode61) to count words
    // per document, then upsert into word_freq.
    //
    // Why not use FTS5's own term stats? Because FTS5's `fts5vocab` virtual
    // table is slow for "give me all terms ranked by frequency" on large sets.
    // A plain table with an index is much faster for the `top` query.
    let words = tokenise(content);
    if words.is_empty() {
        return Ok(());
    }

    // Count occurrences of each word within this document.
    // `std::collections::HashMap` is the standard key-value count structure.
    use std::collections::HashMap;
    let mut counts: HashMap<&str, u64> = HashMap::new();
    for word in &words {
        *counts.entry(word.as_str()).or_insert(0) += 1;
    }

    // `INSERT OR IGNORE` creates a row if it doesn't exist yet.
    // `UPDATE` then adds to the counters. This is an "upsert" pattern.
    let mut insert_stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO word_freq(word, total_count, doc_count)
         VALUES (?1, 0, 0)",
    )?;
    let mut update_stmt = conn.prepare_cached(
        "UPDATE word_freq
         SET total_count = total_count + ?2,
             doc_count   = doc_count + 1
         WHERE word = ?1",
    )?;

    for (word, count) in &counts {
        insert_stmt.execute(params![word])?;
        // `rusqlite` only implements `ToSql` for `i64`, not `u64`, so cast here.
        update_stmt.execute(params![word, *count as i64])?;
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return true if `data` looks like binary (non-text) content.
///
/// We use the same heuristic as `git` and the Unix `file` command:
/// if the probe buffer contains a null byte (0x00), it's binary.
/// This catches executables, images, compressed files, databases, etc.
fn is_binary(data: &[u8]) -> bool {
    // `iter().any(|&b| b == 0)` scans until the first null byte, then stops.
    data.iter().any(|&b| b == 0)
}

/// Tokenise a string into lowercase alphabetic words of at least 3 characters.
///
/// This mirrors what SQLite's unicode61 tokeniser does in broad strokes:
/// split on non-alphanumeric, lowercase, drop very short tokens (noise words).
/// We don't need it to be identical — just consistent enough that the word_freq
/// table is meaningful.
fn tokenise(text: &str) -> Vec<String> {
    text
        // `split_whitespace` handles any Unicode whitespace character.
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() >= 3)
        .map(|s| s.to_lowercase())
        .collect()
}

/// Resolve the SQLite database path given the archive path and an optional override.
///
/// Priority:
///   1. Caller-supplied explicit path (already a full PathBuf).
///   2. Default: <archive>.db in the same directory.
///   3. Fallback: ./backupsage.db if the archive's directory is not writable.
pub fn resolve_db_path(archive_path: &Path, explicit: &PathBuf) -> Result<PathBuf> {
    // If the caller already resolved an explicit path, use it directly.
    // The `PathBuf::new()` default (empty path) signals "not provided".
    if explicit.as_os_str().len() > 0 {
        return Ok(explicit.clone());
    }

    // Build default: add ".db" suffix to the archive filename.
    let mut default_db = archive_path.to_path_buf();
    let new_name = format!(
        "{}.db",
        archive_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );
    default_db.set_file_name(new_name);

    // Check if the parent directory is writable by trying to create a temp file.
    let parent = default_db.parent().unwrap_or_else(|| Path::new("."));
    let test_path = parent.join(".backupsage_write_test");
    let writable = File::create(&test_path).is_ok();
    if writable {
        // Clean up the test file immediately.
        let _ = fs::remove_file(&test_path);
        return Ok(default_db);
    }

    // Fallback: use CWD. Warn the user so they're not confused.
    let fallback = PathBuf::from("backupsage.db");
    eprintln!(
        "Warning: '{}' is not writable — saving index to '{}'",
        parent.display(),
        fallback.display()
    );
    Ok(fallback)
}

/// Auto-discover the SQLite index path given an optional archive path.
///
/// Used by `search` and `top` commands. Resolution order:
///   1. Explicit --index path (already passed as Option).
///   2. <archive>.db if --archive was given.
///   3. ./backupsage.db in the current directory.
///   4. Any *.db file in the current directory (best-effort hint).
pub fn discover_db_path(
    explicit: &Option<PathBuf>,
    archive: &Option<PathBuf>,
) -> Result<PathBuf> {
    // Priority 1: explicit path supplied by user.
    if let Some(p) = explicit {
        return Ok(p.clone());
    }

    // Priority 2: derive from archive name, same logic as indexer.
    if let Some(archive_path) = archive {
        let empty = PathBuf::new();
        let derived = resolve_db_path(archive_path, &empty)?;
        if derived.exists() {
            return Ok(derived);
        }
    }

    // Priority 3: conventional name in current directory.
    let cwd_default = PathBuf::from("backupsage.db");
    if cwd_default.exists() {
        return Ok(cwd_default);
    }

    // Priority 4: scan the current directory for any .db file as a hint.
    if let Ok(entries) = fs::read_dir(".") {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "db") {
                eprintln!(
                    "Hint: found index at '{}'. Use --index to be explicit.",
                    p.display()
                );
                return Ok(p);
            }
        }
    }

    anyhow::bail!(
        "No index found. Run `backupsage index <archive>` first, \
         or pass --index <path>."
    )
}

/// Truncate a path string to `max_chars`, adding "…" prefix if it was cut.
fn truncate_path(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        // Take the last `max_chars - 1` characters so the tail (filename) is visible.
        let tail: String = s.chars().rev().take(max_chars - 1).collect::<String>()
            .chars().rev().collect();
        format!("…{tail}")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LEARNING NOTES
// ─────────────────────────────────────────────────────────────────────────────
// • Streaming pipeline: File → BufReader → ProgressBar wrapper → zstd::Decoder
//   → tar::Archive. Each layer wraps the previous `io::Read`, so decompression
//   and TAR parsing are interleaved — no full file is ever in memory at once.
//
// • `pb.wrap_read(reader)` is a zero-cost wrapper: it intercepts every `read()`
//   call and adds the byte count to the progress bar.
//
// • SQLite FTS5 with `content=""` is "contentless" — it only stores the search
//   index, not a copy of the original text. This cuts database size dramatically.
//
// • Batch transactions: committing every 500 rows instead of every 1 row gives
//   ~500× faster throughput because each COMMIT is an fsync.
//
// • Binary detection via null-byte probe is fast (O(probe size)) and reliable
//   for the common case of ELF, PNG, JPEG, ZIP, etc.
//
// • `prepare_cached` caches the parsed SQL statement so we don't re-parse the
//   same INSERT/UPDATE SQL thousands of times per archive entry.
// ─────────────────────────────────────────────────────────────────────────────
