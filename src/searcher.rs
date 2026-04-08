// ─── src/searcher.rs ──────────────────────────────────────────────────────────
//
// PURPOSE: Query the SQLite FTS5 index built by indexer.rs.
// Provides two operations:
//   1. `run_search` — find every file containing a keyword and show match counts.
//   2. `run_top`    — show the N most frequent words with both raw frequency
//                     and document frequency side-by-side in a table.
//
// ─────────────────────────────────────────────────────────────────────────────

use std::path::PathBuf;

use anyhow::{Context, Result};
use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table};
use rusqlite::{params, Connection};

// ── Search ────────────────────────────────────────────────────────────────────

/// Search the FTS5 index for files containing `keyword`.
///
/// Prints a table of matching file paths and per-file match counts.
/// The FTS5 `bm25()` ranking function orders results from most to least relevant.
pub fn run_search(db_path: &PathBuf, keyword: &str) -> Result<()> {
    let conn = open_readonly(db_path)?;

    // ── FTS5 query ────────────────────────────────────────────────────────────
    //
    // `files_fts` is our FTS5 virtual table. Querying it with a WHERE clause
    // on `files_fts` (the table name itself, as a function) triggers FTS5 search.
    //
    // `highlight(files_fts, 1, '[', ']')` — FTS5 built-in function that wraps
    // matched tokens in marker characters. Column 1 = content.
    //
    // `rank` is FTS5's implicit BM25 relevance score (lower = more relevant).
    //
    // `snippet(...)` returns a short excerpt around each match — useful for
    // context, but we omit it here to keep output concise.
    //
    // We count matches with `length(highlight(...)) - length(path)` trick
    // or simply report rows — FTS5 does not expose per-row match count directly.
    // Instead we use a trick: fts5's `highlight` inserts markers, so we count
    // marker occurrences to get per-file match count.
    let sql = "
        SELECT
            path,
            (length(highlight(files_fts, 1, '\x01', '\x01')) -
             length(replace(highlight(files_fts, 1, '\x01', '\x01'), '\x01', '')))
             / 2 AS match_count
        FROM files_fts
        WHERE files_fts MATCH ?1
        ORDER BY rank
        LIMIT 10000
    ";

    let mut stmt = conn
        .prepare(sql)
        .context("Failed to prepare search query")?;

    // `query_map` runs the query and maps each row to a Rust tuple.
    // The closure receives a `&Row` — we extract columns by index (0-based).
    let rows: Vec<(String, i64)> = stmt
        .query_map(params![keyword], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .context("FTS5 search query failed")?
        .filter_map(|r| r.ok()) // Skip any rows that fail to deserialise.
        .collect();

    if rows.is_empty() {
        println!("No results for '{keyword}'.");
        return Ok(());
    }

    println!(
        "Found {} file(s) matching '{keyword}':\n",
        rows.len()
    );

    // ── Build result table ────────────────────────────────────────────────────
    // `comfy_table` creates a Unicode box-drawing table that auto-sizes columns
    // to fit the terminal width.
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    // Header row — styled bold + cyan.
    table.set_header(vec![
        Cell::new("#")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("Matches")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("File Path")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
    ]);

    for (i, (path, count)) in rows.iter().enumerate() {
        table.add_row(vec![
            Cell::new(i + 1)
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(count)
                .set_alignment(CellAlignment::Right)
                .fg(Color::Yellow),
            Cell::new(path),
        ]);
    }

    println!("{table}");
    Ok(())
}

// ── Top words ─────────────────────────────────────────────────────────────────

/// Show the `limit` most common words across the entire backup.
///
/// Displays two frequency columns side-by-side:
///   - Total count: how many times the word appears across ALL files.
///   - Doc count:   how many distinct files the word appears in.
pub fn run_top(db_path: &PathBuf, limit: usize) -> Result<()> {
    let conn = open_readonly(db_path)?;

    // Query the word_freq table we populated during indexing.
    // `ORDER BY total_count DESC` puts the most-used words first.
    // We cap at `limit` rows (default 50).
    let sql = "
        SELECT word, total_count, doc_count
        FROM word_freq
        ORDER BY total_count DESC
        LIMIT ?1
    ";

    let mut stmt = conn
        .prepare(sql)
        .context("Failed to prepare top-words query")?;

    let rows: Vec<(String, i64, i64)> = stmt
        .query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,  // word
                row.get::<_, i64>(1)?,     // total_count
                row.get::<_, i64>(2)?,     // doc_count
            ))
        })
        .context("Top-words query failed")?
        .filter_map(|r| r.ok())
        .collect();

    if rows.is_empty() {
        println!("No words found in the index. Has indexing completed?");
        return Ok(());
    }

    // Get total word count for percentage column.
    let grand_total: i64 = rows.iter().map(|(_, c, _)| c).sum();

    println!("Top {} words in backup (grand total shown = top-{} only):\n", rows.len(), rows.len());

    // ── Build table ───────────────────────────────────────────────────────────
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    // Six columns: rank, word, total occurrences, % of shown total, files containing it, bar.
    table.set_header(vec![
        Cell::new("Rank")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("Word")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("Occurrences")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("% of Top")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("In Files")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
        Cell::new("Bar")
            .add_attribute(Attribute::Bold)
            .fg(Color::Cyan),
    ]);

    // Find the max total_count to scale the mini bar chart (max 20 chars wide).
    let max_count = rows.first().map(|(_, c, _)| *c).unwrap_or(1);

    for (rank, (word, total, docs)) in rows.iter().enumerate() {
        // Calculate percentage of the top-N words total.
        let pct = if grand_total > 0 {
            (*total as f64 / grand_total as f64) * 100.0
        } else {
            0.0
        };

        // Mini bar: scale `total` against `max_count` into 20 filled blocks.
        let bar_len = ((*total as f64 / max_count as f64) * 20.0).round() as usize;
        let bar = format!("{}{}", "█".repeat(bar_len), "░".repeat(20 - bar_len));

        table.add_row(vec![
            Cell::new(rank + 1)
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(word)
                .add_attribute(Attribute::Bold),
            Cell::new(format_number(*total))
                .set_alignment(CellAlignment::Right)
                .fg(Color::Yellow),
            Cell::new(format!("{:.1}%", pct))
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(format_number(*docs))
                .set_alignment(CellAlignment::Right)
                .fg(Color::Blue),
            Cell::new(bar)
                .fg(Color::Green),
        ]);
    }

    println!("{table}");
    println!(
        "\n  Occurrences = total word hits across all files  |  In Files = distinct file count"
    );

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Open the database in read-only mode.
///
/// `SQLITE_OPEN_READONLY` prevents accidental writes.
/// We pass `?mode=ro` as a URI parameter — rusqlite supports SQLite URI syntax.
fn open_readonly(db_path: &PathBuf) -> Result<Connection> {
    if !db_path.exists() {
        anyhow::bail!(
            "Index not found at '{}'. Run `backupsage index <archive>` first.",
            db_path.display()
        );
    }

    // Open with the URI `file:/path?mode=ro` for a true read-only connection.
    // This also allows multiple processes to read the database simultaneously.
    let uri = format!("file:{}?mode=ro", db_path.display());
    Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("Cannot open index at '{uri}'"))
}

/// Format a large integer with thousands separators for readability.
/// e.g. 1234567 → "1,234,567"
fn format_number(n: i64) -> String {
    // Convert to string, then insert commas every 3 digits from the right.
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    // Reverse back to get the correct order.
    result.chars().rev().collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// LEARNING NOTES
// ─────────────────────────────────────────────────────────────────────────────
// • FTS5 MATCH queries use a specialised syntax: single words, "phrase queries",
//   word* prefix searches, and boolean operators (AND, OR, NOT).
//
// • `highlight(table, column_index, open, close)` is an FTS5 auxiliary function
//   that inserts marker strings around matched tokens. We use ASCII 0x01 (SOH)
//   as a marker because it never appears in normal text, making counting safe.
//
// • `bm25()` is FTS5's built-in ranking function (Best Match 25). Lower scores
//   (more negative) mean higher relevance. `ORDER BY rank` automatically uses it.
//
// • `comfy_table` adapts column widths to the terminal and handles Unicode box
//   drawing characters correctly on any platform.
//
// • Opening SQLite with `SQLITE_OPEN_READ_ONLY` is a safety measure: it's
//   impossible for query bugs to accidentally corrupt the index.
//
// • `format_number` with comma insertion works by reversing the digit string,
//   inserting commas at every third position, then reversing back — a classic
//   string manipulation pattern.
// ─────────────────────────────────────────────────────────────────────────────
