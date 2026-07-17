//! Query side: open an index, search it, list top words.
//!
//! These functions return data; rendering lives in main.rs. Errors from
//! SQLite propagate — v0.1 swallowed them with `filter_map(Result::ok)`,
//! which turned FTS5 syntax errors into a bogus "No results".

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};

pub struct SearchHit {
    pub path: String,
    /// Matches within the file content (matches in the path count as 0).
    pub matches: i64,
    pub snippet: Option<String>,
}

pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    /// True if more matches exist beyond `limit`.
    pub truncated: bool,
    /// True if the query was not valid FTS5 syntax and was retried as a
    /// literal phrase.
    pub literal_fallback: bool,
}

pub struct WordRow {
    pub word: String,
    pub total: i64,
    pub docs: i64,
}

// ── Opening an index ─────────────────────────────────────────────────────────

/// Open an index read-only, verifying it is actually a BackupSage database.
pub fn open_index(db_path: &Path) -> Result<Connection> {
    if !db_path.exists() {
        bail!(
            "index not found at '{}' — run `backupsage index <archive>` first",
            db_path.display()
        );
    }
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open index at '{}'", db_path.display()))?;
    if !is_backupsage_db(&conn) {
        bail!(
            "'{}' is not a BackupSage index (no files_fts table)",
            db_path.display()
        );
    }
    Ok(conn)
}

fn is_backupsage_db(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE name = 'files_fts'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// Read a key from the meta table. Returns None for keys that don't exist —
/// including on v0.1 databases, which have no meta table at all.
pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

/// Some(false) means the index was interrupted mid-build and is partial.
/// None means the database predates completion tracking (v0.1).
pub fn index_completed(conn: &Connection) -> Option<bool> {
    get_meta(conn, "completed").map(|v| v == "1")
}

// ── Search ───────────────────────────────────────────────────────────────────

/// Search the index. Invalid FTS5 syntax is retried as a quoted literal
/// phrase rather than surfacing a parser error for queries like `don't`.
pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    snippets: bool,
) -> Result<SearchOutcome> {
    match run_match(conn, query, limit, snippets) {
        Ok((hits, truncated)) => Ok(SearchOutcome {
            hits,
            truncated,
            literal_fallback: false,
        }),
        Err(e) if is_fts_syntax_error(&e) => {
            let quoted = format!("\"{}\"", query.replace('"', "\"\""));
            let (hits, truncated) = run_match(conn, &quoted, limit, snippets)
                .with_context(|| format!("literal search for '{query}' failed"))?;
            Ok(SearchOutcome {
                hits,
                truncated,
                literal_fallback: true,
            })
        }
        Err(e) => Err(e).context("search query failed"),
    }
}

fn run_match(
    conn: &Connection,
    query: &str,
    limit: usize,
    snippets: bool,
) -> rusqlite::Result<(Vec<SearchHit>, bool)> {
    // Match count per file: highlight() wraps every content match in a
    // char(1) marker pair, so (marker count) / 2 = number of matches.
    let snippet_col = if snippets {
        ", snippet(files_fts, 1, '[', ']', ' … ', 12)"
    } else {
        ""
    };
    let sql = format!(
        "SELECT
            path,
            (length(highlight(files_fts, 1, char(1), char(1))) -
             length(replace(highlight(files_fts, 1, char(1), char(1)), char(1), ''))) / 2
            {snippet_col}
         FROM files_fts
         WHERE files_fts MATCH ?1
         ORDER BY rank
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    // Fetch one extra row to know whether the limit cut anything off.
    let mut rows = stmt.query(params![query, (limit + 1) as i64])?;
    let mut hits = Vec::new();
    while let Some(row) = rows.next()? {
        hits.push(SearchHit {
            path: row.get(0)?,
            matches: row.get(1)?,
            snippet: if snippets { row.get(2)? } else { None },
        });
    }
    let truncated = hits.len() > limit;
    hits.truncate(limit);
    Ok((hits, truncated))
}

/// FTS5 reports query-syntax problems as plain SQLITE_ERROR; the message is
/// the only way to tell them apart from real failures.
fn is_fts_syntax_error(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(_, Some(msg)) => {
            let m = msg.to_lowercase();
            m.contains("fts5") || m.contains("syntax error") || m.contains("unterminated string")
        }
        _ => false,
    }
}

// ── Top words ────────────────────────────────────────────────────────────────

pub fn top_words(conn: &Connection, limit: usize) -> Result<Vec<WordRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT word, total_count, doc_count
             FROM word_freq
             ORDER BY total_count DESC
             LIMIT ?1",
        )
        .context("failed to prepare top-words query")?;
    let mut rows = stmt.query(params![limit as i64])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(WordRow {
            word: row.get(0)?,
            total: row.get(1)?,
            docs: row.get(2)?,
        });
    }
    Ok(out)
}

// ── Index discovery ──────────────────────────────────────────────────────────

/// Locate the index database for `search`/`top`:
///   1. explicit `--index` path
///   2. `<archive>.db` next to the archive, then `./<archive-name>.db`
///   3. `./backupsage.db` (v0.1's fallback name)
///   4. any BackupSage database in the current directory, with a hint
pub fn discover_db_path(explicit: Option<&Path>, archive: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }

    if let Some(archive_path) = archive {
        let beside = crate::indexer::resolve_db_path(archive_path, None);
        if beside.exists() {
            return Ok(beside);
        }
        if let Some(name) = beside.file_name() {
            let in_cwd = PathBuf::from(name);
            if in_cwd.exists() {
                return Ok(in_cwd);
            }
        }
    }

    let legacy = PathBuf::from("backupsage.db");
    if legacy.exists() {
        return Ok(legacy);
    }

    // Last resort: any *.db in the current directory that actually is a
    // BackupSage index (v0.1 grabbed the first *.db file of any kind and
    // failed later with "file is not a database").
    if let Ok(entries) = fs::read_dir(".") {
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "db"))
            .collect();
        candidates.sort();
        for p in candidates {
            let ok = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map(|c| is_backupsage_db(&c))
                .unwrap_or(false);
            if ok {
                eprintln!(
                    "hint: using index '{}' — pass --index to be explicit",
                    p.display()
                );
                return Ok(p);
            }
        }
    }

    bail!("no index found — run `backupsage index <archive>` first, or pass --index <path>")
}
