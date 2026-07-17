//! Command-line interface, defined with clap's derive API.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "backupsage",
    version,
    about = "Index and search words inside .tar, .tar.gz and .tar.zst backups — without extracting them",
    long_about = None,
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Scan a tar archive (plain, gzip or zstd) and build a full-text index.
    ///
    /// Example:
    ///   backupsage index /backups/system.tar.zst
    ///   backupsage index /backups/old.tar.gz --index /tmp/old.db
    Index(IndexArgs),

    /// Search the index for files containing a keyword.
    ///
    /// Example:
    ///   backupsage search "password"
    ///   backupsage search "TODO" --index /tmp/system.db --snippets
    Search(SearchArgs),

    /// Show the most frequent words across the entire backup.
    ///
    /// Example:
    ///   backupsage top
    ///   backupsage top --index /tmp/system.db --limit 100
    Top(TopArgs),
}

#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Path to the archive to index (.tar, .tar.gz or .tar.zst — detected by
    /// content, not extension). The archive is streamed, never extracted.
    pub archive: PathBuf,

    /// Where to store the SQLite index database.
    ///
    /// Defaults to <archive-path>.db next to the archive; falls back to the
    /// current directory if that location is not writable.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// Maximum content indexed per file. Larger files are indexed up to this
    /// limit (their tail is not searchable). Accepts K/M/G suffixes.
    /// 0 indexes file names only.
    #[arg(long, value_name = "SIZE", default_value = "16M", value_parser = parse_size)]
    pub max_file_size: u64,

    /// Skip building word-frequency statistics (faster; `top` will be empty).
    #[arg(long)]
    pub no_word_stats: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// The keyword or phrase to search for.
    ///
    /// FTS5 syntax is supported: single words, "exact phrases", prefix*
    /// queries, AND / OR / NOT. Anything that is not valid FTS5 syntax is
    /// automatically retried as a literal phrase.
    pub keyword: String,

    /// Path to the archive (used to auto-discover its index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit path to the SQLite index database (overrides auto-discovery).
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// Maximum number of matching files to show.
    #[arg(long, short = 'n', default_value_t = 100)]
    pub limit: usize,

    /// Show a short excerpt of the matched text for each file.
    #[arg(long, short = 's')]
    pub snippets: bool,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// Path to the archive (used to auto-discover its index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit path to the SQLite index database (overrides auto-discovery).
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// How many top words to show.
    #[arg(long, short = 'n', default_value_t = 50)]
    pub limit: usize,
}

/// Parse a byte size with an optional K/M/G suffix (base 1024), e.g. "16M".
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let (num, mult): (&str, u64) = match t.chars().last() {
        Some('k') | Some('K') => (&t[..t.len() - 1], 1024),
        Some('m') | Some('M') => (&t[..t.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        _ => (t, 1),
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("invalid size '{s}' — use bytes or a K/M/G suffix, e.g. 16M"))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("16M").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("2k").unwrap(), 2048);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert!(parse_size("banana").is_err());
        assert!(parse_size("999999999999G").is_err());
    }
}
