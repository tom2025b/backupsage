//! Entry point: parse the CLI, dispatch to the library, render results.

use anyhow::Result;
use clap::Parser;
use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table};

use backupsage::cli::{Cli, Commands};
use backupsage::indexer::{self, IndexOptions};
use backupsage::searcher;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index(args) => {
            let opts = IndexOptions {
                max_file_size: args.max_file_size,
                media_cap: backupsage::indexer::DEFAULT_MEDIA_CAP,
                word_stats: !args.no_word_stats,
            };
            let summary = indexer::run_index(&args.archive, args.index.as_deref(), &opts)?;

            println!();
            println!("Indexed  : {} text files", summary.files_indexed);
            println!(
                "Skipped  : {} binary files (names only)",
                summary.files_skipped_binary
            );
            if summary.files_link_names > 0 {
                println!(
                    "Links    : {} hard/symlinks (names only)",
                    summary.files_link_names
                );
            }
            if summary.files_truncated > 0 {
                println!(
                    "Truncated: {} files larger than the per-file cap (raise with --max-file-size)",
                    summary.files_truncated
                );
            }
            println!("Database : {}", summary.db_path.display());
        }

        Commands::Search(args) => {
            let db_path =
                searcher::discover_db_path(args.index.as_deref(), args.archive.as_deref())?;
            let conn = searcher::open_index(&db_path)?;
            warn_if_incomplete(&conn);

            let outcome = searcher::search(&conn, &args.keyword, args.limit, args.snippets)?;
            if outcome.literal_fallback {
                eprintln!(
                    "note: '{}' is not valid FTS5 syntax — searched for it as a literal phrase",
                    args.keyword
                );
            }
            if outcome.hits.is_empty() {
                println!("No results for '{}'.", args.keyword);
                return Ok(());
            }

            let shown = outcome.hits.len();
            if outcome.truncated {
                println!(
                    "Showing the first {shown} files matching '{}' (more exist — raise with --limit):\n",
                    args.keyword
                );
            } else {
                println!("Found {shown} file(s) matching '{}':\n", args.keyword);
            }
            print_search_table(&outcome.hits, args.snippets);
        }

        Commands::Top(args) => {
            let db_path =
                searcher::discover_db_path(args.index.as_deref(), args.archive.as_deref())?;
            let conn = searcher::open_index(&db_path)?;
            warn_if_incomplete(&conn);

            let rows = searcher::top_words(&conn, args.limit)?;
            if rows.is_empty() {
                if searcher::get_meta(&conn, "word_stats").as_deref() == Some("0") {
                    println!("This index was built with --no-word-stats; re-index to use `top`.");
                } else {
                    println!("No words found in the index. Has indexing completed?");
                }
                return Ok(());
            }
            print_top_table(&rows);
        }
    }

    Ok(())
}

fn warn_if_incomplete(conn: &rusqlite::Connection) {
    if searcher::index_completed(conn) == Some(false) {
        eprintln!("warning: this index was interrupted before finishing — results are partial");
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn header_cell(text: &str) -> Cell {
    Cell::new(text)
        .add_attribute(Attribute::Bold)
        .fg(Color::Cyan)
}

fn print_search_table(hits: &[searcher::SearchHit], snippets: bool) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);

    let mut headers = vec![
        header_cell("#"),
        header_cell("Matches"),
        header_cell("File Path"),
    ];
    if snippets {
        headers.push(header_cell("Context"));
    }
    table.set_header(headers);

    for (i, hit) in hits.iter().enumerate() {
        let mut row = vec![
            Cell::new(i + 1)
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(hit.matches)
                .set_alignment(CellAlignment::Right)
                .fg(Color::Yellow),
            Cell::new(&hit.path),
        ];
        if snippets {
            row.push(Cell::new(hit.snippet.as_deref().unwrap_or("")).fg(Color::DarkGrey));
        }
        table.add_row(row);
    }

    println!("{table}");
}

fn print_top_table(rows: &[searcher::WordRow]) {
    let grand_total: i64 = rows.iter().map(|r| r.total).sum();
    println!(
        "Top {} words in backup (percentages are of the top-{} total):\n",
        rows.len(),
        rows.len()
    );

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        header_cell("Rank"),
        header_cell("Word"),
        header_cell("Occurrences"),
        header_cell("% of Top"),
        header_cell("In Files"),
        header_cell("Bar"),
    ]);

    let max_count = rows.first().map(|r| r.total).unwrap_or(1).max(1);
    for (rank, row) in rows.iter().enumerate() {
        let pct = if grand_total > 0 {
            (row.total as f64 / grand_total as f64) * 100.0
        } else {
            0.0
        };
        let bar_len = ((row.total as f64 / max_count as f64) * 20.0).round() as usize;
        let bar = format!("{}{}", "█".repeat(bar_len), "░".repeat(20 - bar_len));

        table.add_row(vec![
            Cell::new(rank + 1)
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(&row.word).add_attribute(Attribute::Bold),
            Cell::new(format_number(row.total))
                .set_alignment(CellAlignment::Right)
                .fg(Color::Yellow),
            Cell::new(format!("{pct:.1}%"))
                .set_alignment(CellAlignment::Right)
                .fg(Color::DarkGrey),
            Cell::new(format_number(row.docs))
                .set_alignment(CellAlignment::Right)
                .fg(Color::Blue),
            Cell::new(bar).fg(Color::Green),
        ]);
    }

    println!("{table}");
    println!(
        "\n  Occurrences = total word hits across all files  |  In Files = distinct file count"
    );
}

/// 1234567 → "1,234,567"
fn format_number(n: i64) -> String {
    let s = n.to_string();
    let mut result = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::format_number;

    #[test]
    fn formats_numbers() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
    }
}
