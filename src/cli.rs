// ─── src/cli.rs ───────────────────────────────────────────────────────────────
//
// PURPOSE: Defines the command-line interface for BackupSage using clap's
// "derive" API. Each subcommand is a variant of the `Commands` enum.
// Clap automatically handles --help, --version, argument parsing, and
// validation. We never write argument-parsing logic by hand.
//
// ─────────────────────────────────────────────────────────────────────────────

// `Parser` lets clap turn command-line args into our struct automatically.
// `Subcommand` is used for variants that represent subcommands (index/search/top).
// `Args` is used for groups of flags that can be shared between subcommands.
use clap::{Args, Parser, Subcommand};

// `PathBuf` is the owned, heap-allocated form of a filesystem path.
// It's safer than `String` for paths because it handles OS differences.
use std::path::PathBuf;

// ── Top-level CLI struct ──────────────────────────────────────────────────────
//
// `#[derive(Parser)]` tells clap to generate all parsing boilerplate.
// The `#[command(...)]` attribute sets the help text and version string.
#[derive(Parser, Debug)]
#[command(
    name = "backupsage",
    version,
    about = "Index and search words inside a .tar.zst backup — without extracting it",
    long_about = None,
)]
pub struct Cli {
    // `#[command(subcommand)]` means the next positional arg selects a subcommand.
    #[command(subcommand)]
    pub command: Commands,
}

// ── Subcommands ───────────────────────────────────────────────────────────────
//
// Each variant corresponds to one subcommand the user can type.
// Clap reads the variant name (lowercased) as the subcommand name by default.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Scan a .tar.zst archive and build a full-text search index (SQLite FTS5).
    ///
    /// Example:
    ///   backupsage index /backups/system.tar.zst
    ///   backupsage index /backups/system.tar.zst --index /tmp/system.db
    Index(IndexArgs),

    /// Search the index for files containing a keyword.
    ///
    /// Example:
    ///   backupsage search "password"
    ///   backupsage search "TODO" --index /tmp/system.db
    Search(SearchArgs),

    /// Show the 50 most frequent words across the entire backup.
    ///
    /// Example:
    ///   backupsage top
    ///   backupsage top --index /tmp/system.db --limit 100
    Top(TopArgs),
}

// ── index arguments ──────────────────────────────────────────────────────────
//
// `#[derive(Args)]` lets this struct be embedded inside a `Subcommand` variant.
#[derive(Args, Debug)]
pub struct IndexArgs {
    /// Path to the .tar.zst archive to index.
    ///
    /// Must be a readable file. The archive is streamed — it is never
    /// fully extracted to disk.
    pub archive: PathBuf,

    /// Where to store the SQLite index database.
    ///
    /// Defaults to <archive-path>.db (e.g. system.tar.zst → system.tar.zst.db).
    /// Falls back to ./backupsage.db in the current directory if the archive's
    /// directory is not writable.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,
}

// ── search arguments ─────────────────────────────────────────────────────────
#[derive(Args, Debug)]
pub struct SearchArgs {
    /// The keyword or phrase to search for.
    ///
    /// Matched using SQLite FTS5 full-text search. Supports:
    ///   - Single words:     password
    ///   - Phrase queries:   "error 404"
    ///   - Prefix queries:   config*
    pub keyword: String,

    /// Path to the archive (used to auto-discover the index file).
    ///
    /// If omitted, BackupSage looks for an index in the current directory.
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit path to the SQLite index database.
    ///
    /// Overrides auto-discovery. Use this when the index was saved to a
    /// non-default location during `backupsage index`.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,
}

// ── top arguments ─────────────────────────────────────────────────────────────
#[derive(Args, Debug)]
pub struct TopArgs {
    /// Path to the archive (used to auto-discover the index file).
    #[arg(long, short = 'a', value_name = "ARCHIVE")]
    pub archive: Option<PathBuf>,

    /// Explicit path to the SQLite index database.
    #[arg(long, short = 'i', value_name = "FILE")]
    pub index: Option<PathBuf>,

    /// How many top words to show. Defaults to 50.
    #[arg(long, short = 'n', default_value_t = 50)]
    pub limit: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// LEARNING NOTES
// ─────────────────────────────────────────────────────────────────────────────
// • clap's `derive` API lets you write plain Rust structs/enums and get
//   full CLI parsing for free — no manual match blocks needed.
//
// • `#[command(...)]` on the top-level struct controls the global help text
//   and binary name shown in `--help`.
//
// • `#[arg(...)]` on a field controls that argument's flag name, short alias,
//   value placeholder in help text, and default value.
//
// • `PathBuf` (not `String`) is always preferred for file system paths.
//   It implements `From<OsStr>` so clap can set it directly from argv.
//
// • `Option<PathBuf>` means the argument is optional — None if not supplied.
// ─────────────────────────────────────────────────────────────────────────────
