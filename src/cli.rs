// last_edited_by: codex
// **Signed:** codex · 2026-09-26T22:22:09-04:00
//! Command-line interface, defined with clap's derive API.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "backupsage",
    version,
    about = "Index, search and deduplicate files across tar backups and directories — without extracting them",
    long_about = None,
)]
pub struct Cli {
    /// Master catalog path (env BACKUPSAGE_MASTER;
    /// default ~/.local/share/backupsage/master.db).
    #[arg(long, global = true, value_name = "FILE")]
    pub master: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Index tar archives (plain/gzip/zstd) or directories into v3 indexes.
    ///
    /// Example:
    ///   backupsage index /backups/system.tar.zst /backups/old.tar.gz
    ///   backupsage index ~/Pictures
    Index(IndexArgs),

    /// Search one index, or every archive registered in the master (--all).
    ///
    /// Example:
    ///   backupsage search "password" -a /backups/system.tar.zst
    ///   backupsage search "invoice 2023" --all
    Search(SearchArgs),

    /// Show the most frequent words in one index.
    Top(TopArgs),

    /// Manage the master catalog (register indexes, sync, verify, remove).
    #[command(subcommand)]
    Master(MasterCommands),

    /// Find duplicate files across all registered archives (or ad-hoc DBs).
    ///
    /// Example:
    ///   backupsage dedup --min-size 100K --json -o dupes.json
    ///   backupsage dedup --db a.tar.zst.db --db b.tar.gz.db
    Dedup(DedupArgs),

    /// Show every indexed detail for one file path.
    ///
    /// Example:
    ///   backupsage inspect DCIM/IMG_0142.JPG -a /backups/photos.tar.zst
    Inspect(InspectArgs),

    /// Compare two indexed snapshots, read-only: added, removed, moved,
    /// byte-identical, metadata-only and content changes.
    ///
    /// Neither index is modified and nothing is re-indexed. Exit 0 when the
    /// comparison is complete; 2 when an input is unavailable, incomplete or
    /// incompatible, or any row is inconclusive.
    ///
    /// Example:
    ///   backupsage diff old.tar.zst.db new.tar.zst.db
    ///   backupsage diff ~/Pictures.db /mnt/backup/Pictures.db --json
    Diff(DiffArgs),

    /// Report which content has fewer trusted copies than a floor, read-only.
    ///
    /// Reads the master's registry and each source's own index; writes
    /// nothing beside any of them. Unknown data is never counted as a
    /// missing copy: content whose count cannot be settled is
    /// `inconclusive`, never `below floor`. Exit 0 when every source is a
    /// complete, ok source and nothing is inconclusive; 2 otherwise.
    ///
    /// Example:
    ///   backupsage coverage --min-copies 2
    ///   backupsage coverage --db a.tar.zst.db --db b.tar.zst.db --json -o coverage.json
    Coverage(CoverageArgs),
}

#[derive(Args, Debug)]
pub struct CoverageArgs {
    /// Trusted copies wanted per content (at least 1).
    #[arg(long, value_name = "N", default_value_t = 2)]
    pub min_copies: usize,

    /// Content smaller than this is out of scope (listed, not judged).
    /// Accepts K/M/G.
    #[arg(long, value_name = "SIZE", default_value = "0", value_parser = parse_size)]
    pub min_size: u64,

    /// Judge empty (0-byte) content too.
    #[arg(long)]
    pub include_empty: bool,

    /// Only content with a path ending in one of these extensions,
    /// comma-separated: jpg,heic,mp4. Every copy still counts.
    #[arg(long, value_name = "EXTS", value_delimiter = ',')]
    pub ext: Vec<String>,

    /// Only content with a path matching this GLOB (e.g. "*/DCIM/*").
    /// Every copy still counts.
    #[arg(long, value_name = "GLOB")]
    pub path_glob: Option<String>,

    /// Only content of one kind: image, raw, video, text, binary.
    /// Every copy still counts.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Restrict to these sources (id, label or index path); repeatable.
    #[arg(long = "archive", value_name = "ID_OR_LABEL")]
    pub archives: Vec<String>,

    /// Mark these sources protected/reference copies (id, label or index
    /// path); repeatable. They count like any trusted copy and are shown
    /// separately.
    #[arg(long = "protected", value_name = "ID_OR_LABEL")]
    pub protected: Vec<String>,

    /// Ad-hoc mode: read these index files without the master. Repeatable.
    #[arg(long = "db", value_name = "FILE")]
    pub dbs: Vec<PathBuf>,

    /// Machine-readable JSON report (version 1).
    #[arg(long)]
    pub json: bool,

    /// Write the report to a new file instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct DiffArgs {
    /// The earlier index file.
    pub before: PathBuf,

    /// The later index file.
    pub after: PathBuf,

    /// Machine-readable JSON report (version 1).
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Fully reprocess directory files instead of reusing content-verified results.
    #[arg(long)]
    pub force_full: bool,
    /// Archives (.tar/.tar.gz/.tar.zst, detected by content) or directories.
    #[arg(required = true)]
    pub sources: Vec<PathBuf>,

    /// Where to store the index (single source only).
    /// Defaults to <source>.db next to the source.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// Maximum content indexed for text search per file. Accepts K/M/G.
    /// 0 indexes names only. The full-content hash always covers every byte.
    #[arg(long, value_name = "SIZE", default_value = "16M", value_parser = parse_size)]
    pub max_file_size: u64,

    /// Maximum bytes retained for image decoding (larger images get no
    /// perceptual hash). Accepts K/M/G.
    #[arg(long, value_name = "SIZE", default_value = "64M", value_parser = parse_size)]
    pub media_cap: u64,

    /// Skip word-frequency statistics (faster; `top` will be empty).
    #[arg(long)]
    pub no_word_stats: bool,

    /// What to store about file content: full, search-only (contentless
    /// FTS — searchable, no stored plaintext, no snippets), or
    /// metadata-only (content never read; names/sizes/times only).
    #[arg(long, default_value = "full",
          value_parser = ["full", "search-only", "metadata-only"])]
    pub mode: String,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// FTS5 query (words, "exact phrases", prefix*, AND/OR/NOT). Invalid
    /// syntax is retried as a literal phrase.
    pub keyword: String,

    /// Search every archive registered in the master.
    #[arg(long, conflicts_with_all = ["archive", "index"])]
    pub all: bool,

    /// Archive path (used to auto-discover its index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit index path (overrides auto-discovery).
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// Maximum matching files to show (per archive with --all).
    #[arg(long, short = 'n', default_value_t = 100)]
    pub limit: usize,

    /// Show a short excerpt of the matched text for each file.
    #[arg(long, short = 's')]
    pub snippets: bool,

    /// Machine-readable JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct TopArgs {
    /// Archive path (used to auto-discover its index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit index path (overrides auto-discovery).
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// How many top words to show.
    #[arg(long, short = 'n', default_value_t = 50)]
    pub limit: usize,
}

#[derive(Subcommand, Debug)]
pub enum MasterCommands {
    /// Register index files (or their sources) and replicate their metadata.
    Add {
        /// .db index files, or the archives/directories they sit beside.
        #[arg(required = true)]
        targets: Vec<PathBuf>,
    },
    /// List registered archives with status.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Refresh replicas of rebuilt indexes; flag missing DBs.
    Sync {
        /// Delete registrations whose .db has been missing this many days.
        #[arg(long, value_name = "DAYS")]
        prune: Option<u32>,
    },
    /// Check whether sources changed since indexing (stat compare).
    Verify {
        /// Re-hash tar archives and compare fingerprints (slow, certain).
        #[arg(long)]
        deep: bool,
        #[arg(long)]
        json: bool,
    },
    /// Remove a registration (never touches the index file itself).
    Rm {
        /// Archive id, label, or path.
        key: String,
    },
}

#[derive(Args, Debug)]
pub struct DedupArgs {
    /// Only exact (identical content) groups.
    #[arg(long, conflicts_with = "near_only")]
    pub exact_only: bool,

    /// Only perceptual (image) groups.
    #[arg(long)]
    pub near_only: bool,

    /// Hamming distance for near matches (0-3).
    #[arg(long, default_value_t = 3)]
    pub threshold: u32,

    /// Restrict to one kind: image, raw, video, text, binary.
    #[arg(long, value_name = "KIND")]
    pub kind: Option<String>,

    /// Restrict to extensions, comma-separated: jpg,heic,mp4
    #[arg(long, value_name = "EXTS", value_delimiter = ',')]
    pub ext: Vec<String>,

    /// Ignore files smaller than this. Accepts K/M/G.
    #[arg(long, value_name = "SIZE", default_value = "1", value_parser = parse_size)]
    pub min_size: u64,

    /// Only paths matching this GLOB (e.g. "*/DCIM/*").
    #[arg(long, value_name = "GLOB")]
    pub path_glob: Option<String>,

    /// Restrict to these archives (id or label); repeatable.
    #[arg(long = "archive", value_name = "ID_OR_LABEL")]
    pub archives: Vec<String>,

    /// Hide groups confined to a single archive.
    #[arg(long)]
    pub across_only: bool,

    /// Include empty (0-byte) files.
    #[arg(long)]
    pub include_empty: bool,

    /// Ad-hoc mode: dedup these index files without touching the master.
    /// Repeatable; one DB finds intra-archive duplicates.
    #[arg(long = "db", value_name = "FILE")]
    pub dbs: Vec<PathBuf>,

    /// Sort groups: wasted, count, newest.
    #[arg(long, default_value = "wasted")]
    pub sort: String,

    /// Show at most this many groups.
    #[arg(long, value_name = "N")]
    pub limit_groups: Option<usize>,

    /// Machine-readable JSON report.
    #[arg(long)]
    pub json: bool,

    /// Write the report to a file instead of stdout.
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    /// The file path inside the index (exact match).
    pub path: String,

    /// Archive path (used to auto-discover its index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit index path (overrides auto-discovery).
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,
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
    use super::*;
    use clap::CommandFactory;

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

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_new_subcommands() {
        let cli = Cli::parse_from([
            "backupsage",
            "dedup",
            "--db",
            "a.db",
            "--db",
            "b.db",
            "--json",
        ]);
        match cli.command {
            Commands::Dedup(d) => {
                assert_eq!(d.dbs.len(), 2);
                assert!(d.json);
                assert_eq!(d.threshold, 3);
                assert_eq!(d.min_size, 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let cli = Cli::parse_from(["backupsage", "diff", "a.db", "b.db", "--json"]);
        match cli.command {
            Commands::Diff(d) => {
                assert_eq!(d.before, PathBuf::from("a.db"));
                assert_eq!(d.after, PathBuf::from("b.db"));
                assert!(d.json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        let cli = Cli::parse_from(["backupsage", "master", "add", "x.tar.zst.db"]);
        assert!(matches!(
            cli.command,
            Commands::Master(MasterCommands::Add { .. })
        ));
        let cli = Cli::parse_from([
            "backupsage",
            "index",
            "a.tar",
            "b.tar",
            "--media-cap",
            "128M",
        ]);
        match cli.command {
            Commands::Index(i) => {
                assert_eq!(i.sources.len(), 2);
                assert_eq!(i.media_cap, 128 * 1024 * 1024);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
