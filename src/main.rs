// ─── src/main.rs ──────────────────────────────────────────────────────────────
//
// PURPOSE: Entry point. Parses the CLI (via cli.rs), resolves paths,
// and dispatches to the indexer or searcher.
//
// This file is intentionally thin — all real logic lives in indexer.rs and
// searcher.rs. main.rs is just the "switchboard".
// ─────────────────────────────────────────────────────────────────────────────

// Bring our modules into scope. Each `mod` declaration tells the Rust compiler
// to compile the corresponding `src/<name>.rs` file.
mod cli;
mod indexer;
mod searcher;

// `clap::Parser` is the trait that gives our Cli struct its `.parse()` method.
use clap::Parser;

// `anyhow::Result` is a type alias for `Result<T, anyhow::Error>`.
// It lets us use `?` on any error type without wrapping manually.
use anyhow::Result;

// The CLI struct and subcommand variants we defined in cli.rs.
use cli::{Cli, Commands};

// `std::path::PathBuf` — owned heap-allocated file path.
use std::path::PathBuf;

fn main() -> Result<()> {
    // `Cli::parse()` reads `std::env::args()`, validates them against our
    // struct definitions, and returns a populated `Cli` — or prints a helpful
    // error and exits if the args are invalid.
    let cli = Cli::parse();

    // Dispatch to the correct handler based on which subcommand was typed.
    match cli.command {

        // ── backupsage index <archive> [--index <db>] ─────────────────────────
        Commands::Index(args) => {
            // Resolve the database path:
            //   - If --index was given, use it directly.
            //   - Otherwise pass an empty PathBuf so indexer uses its default logic.
            let db_path = args.index.unwrap_or_else(PathBuf::new);

            // `?` propagates any error up to main, which prints it and exits 1.
            indexer::run_index(&args.archive, &db_path)?;
        }

        // ── backupsage search <keyword> [--archive <a>] [--index <db>] ────────
        Commands::Search(args) => {
            // Auto-discover the database: explicit → archive-derived → CWD fallback.
            let db_path = indexer::discover_db_path(&args.index, &args.archive)?;
            searcher::run_search(&db_path, &args.keyword)?;
        }

        // ── backupsage top [--archive <a>] [--index <db>] [--limit N] ─────────
        Commands::Top(args) => {
            let db_path = indexer::discover_db_path(&args.index, &args.archive)?;
            searcher::run_top(&db_path, args.limit)?;
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// LEARNING NOTES
// ─────────────────────────────────────────────────────────────────────────────
// • `fn main() -> Result<()>` is a Rust idiom that allows `?` at the top level.
//   If main returns Err, Rust prints the error chain and exits with code 1.
//
// • `Cli::parse()` from clap does all the heavy lifting: --help, --version,
//   subcommand routing, type coercion, and error messages are automatic.
//
// • `match` on an enum is exhaustive — the compiler forces us to handle every
//   variant. If we add a new subcommand later, main.rs won't compile until we
//   handle it here. This prevents silent omissions.
//
// • `PathBuf::new()` creates an empty path (length 0). We use this as a
//   sentinel value meaning "not provided by the user". The indexer checks
//   `as_os_str().len() > 0` to distinguish "user gave a path" vs "default".
// ─────────────────────────────────────────────────────────────────────────────
