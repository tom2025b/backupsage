//! Entry point: parse the CLI, dispatch to the library, render results.
//!
//! Exit codes: 0 ok · 1 error · 2 completed but with skipped archives
//! (offline / v2-limited / incomplete) — scripts can rely on this.

use anyhow::{bail, Context, Result};
use clap::Parser;
use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table};

use backupsage::cli::{Cli, Commands, MasterCommands};
use backupsage::dedup::{self, DedupParams, SortKey};
use backupsage::indexer::{self, IndexOptions, IndexSummary};
use backupsage::master::{self, Master};
use backupsage::report::DedupReport;
use backupsage::searcher;

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    let master_path = cli
        .master
        .clone()
        .or_else(|| std::env::var_os("BACKUPSAGE_MASTER").map(Into::into))
        .unwrap_or_else(master::default_master_path);

    match cli.command {
        Commands::Index(args) => {
            if args.sources.len() > 1 && args.index.is_some() {
                bail!("--index only makes sense with a single source");
            }
            let opts = IndexOptions {
                max_file_size: args.max_file_size,
                media_cap: args.media_cap,
                word_stats: !args.no_word_stats,
            };
            for source in &args.sources {
                let summary = indexer::run_index(source, args.index.as_deref(), &opts)?;
                print_index_summary(&summary);
                if !master_path.exists() {
                    println!(
                        "hint: register it with `backupsage master add {}`",
                        summary.db_path.display()
                    );
                }
                println!();
            }
            if master_path.exists() {
                println!(
                    "hint: run `backupsage master add <db>` / `master sync` to refresh the catalog"
                );
            }
            Ok(0)
        }

        Commands::Search(args) if args.all => {
            let m = open_master_checked(&master_path)?;
            let outcome = searcher::search_all(&m, &args.keyword, args.limit, args.snippets)?;
            if args.json {
                println!("{}", search_all_json(&outcome));
            } else {
                if outcome.per_archive.is_empty() {
                    println!(
                        "No results for '{}' in any registered archive.",
                        args.keyword
                    );
                }
                for fed in &outcome.per_archive {
                    let n = fed.hits.len();
                    println!(
                        "── {} — {n} hit(s){} ──",
                        fed.archive_label,
                        if fed.truncated { ", more exist" } else { "" }
                    );
                    print_search_table(&fed.hits, args.snippets);
                    println!();
                }
                for (label, reason) in &outcome.skipped {
                    eprintln!("note: {label}: {reason}");
                }
            }
            Ok(if outcome.skipped.is_empty() { 0 } else { 2 })
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
            if args.json {
                let hits: Vec<serde_json::Value> = outcome
                    .hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "path": h.path, "matches": h.matches, "snippet": h.snippet,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "query": args.keyword, "hits": hits, "truncated": outcome.truncated,
                    }))?
                );
                return Ok(0);
            }
            if outcome.hits.is_empty() {
                println!("No results for '{}'.", args.keyword);
                return Ok(0);
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
            Ok(0)
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
                return Ok(0);
            }
            print_top_table(&rows);
            Ok(0)
        }

        Commands::Master(sub) => run_master(sub, &master_path),

        Commands::Dedup(args) => {
            let sort = match args.sort.as_str() {
                "wasted" => SortKey::Wasted,
                "count" => SortKey::Count,
                "newest" => SortKey::Newest,
                other => bail!("unknown --sort '{other}' (use wasted, count or newest)"),
            };
            let params = DedupParams {
                exact: !args.near_only,
                near: !args.exact_only,
                threshold: args.threshold,
                kind: args.kind.clone(),
                exts: args.ext.clone(),
                min_size: args.min_size,
                path_glob: args.path_glob.clone(),
                archives: args.archives.clone(),
                across_only: args.across_only,
                include_empty: args.include_empty,
                sort,
                limit_groups: args.limit_groups,
                bucket_cap: dedup::DEFAULT_BUCKET_CAP,
            };

            let (m, adhoc_skips) = if args.dbs.is_empty() {
                (open_master_checked(&master_path)?, Vec::new())
            } else {
                master::open_in_memory(&args.dbs)?
            };
            let mut report = dedup::run_dedup(&m, &params)?;
            report
                .summary
                .skipped_archives
                .extend(adhoc_skips.into_iter().map(|(label, reason)| {
                    (label, format!("{reason} — no hashes; re-index to include"))
                }));

            let rendered = if args.json {
                report.to_json()
            } else {
                render_dedup_report(&report)
            };
            match &args.output {
                Some(path) => {
                    // Everything dedup read is protected from the report
                    // write: master + ad-hoc indexes (with sidecars), and
                    // every registered index and source archive.
                    let mut protected = backupsage::outpath::ProtectedSet::new();
                    protected.add_db(&master_path);
                    for db in &args.dbs {
                        protected.add_db(db);
                    }
                    for a in m.list()? {
                        protected.add_db(std::path::Path::new(&a.db_path));
                        protected.add_file(std::path::Path::new(&a.source_path));
                    }
                    backupsage::outpath::write_new_file(path, rendered.as_bytes(), &protected)
                        .context("dedup report not written")?;
                    eprintln!("report written to {}", path.display());
                }
                None => println!("{rendered}"),
            }
            Ok(if report.has_skips() { 2 } else { 0 })
        }

        Commands::Inspect(args) => {
            let db_path =
                searcher::discover_db_path(args.index.as_deref(), args.archive.as_deref())?;
            let conn = searcher::open_index(&db_path)?;
            inspect_path(&conn, &db_path, &args.path)
        }
    }
}

/// The master must actually exist for read paths — a silently created empty
/// catalog would answer "no duplicates anywhere" to every question.
fn open_master_checked(path: &std::path::Path) -> Result<Master> {
    if !path.exists() {
        bail!(
            "no master catalog at '{}' — index something and run \
             `backupsage master add <db>`, or pass --db for ad-hoc dedup",
            path.display()
        );
    }
    master::open_at(path)
}

fn run_master(sub: MasterCommands, master_path: &std::path::Path) -> Result<i32> {
    match sub {
        MasterCommands::Add { targets } => {
            let mut m = master::open_at(master_path)?;
            for t in &targets {
                match m.add(t)? {
                    master::AddOutcome::Replicated { label, files } => {
                        println!("registered '{label}' — {files} file records replicated");
                    }
                    master::AddOutcome::V2Limited { label } => {
                        println!(
                            "registered '{label}' as v2-limited — searchable, but it has no \
                             hashes; re-index the archive to include it in dedup"
                        );
                    }
                }
            }
            println!("master: {}", master_path.display());
            Ok(0)
        }
        MasterCommands::List { json } => {
            let m = open_master_checked(master_path)?;
            let rows = m.list()?;
            if json {
                let v: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "archive_id": r.archive_id, "label": r.label,
                            "source": r.source_path, "source_type": r.source_type,
                            "db_path": r.db_path, "schema_version": r.schema_version,
                            "files": r.files_count, "completed": r.completed,
                            "status": r.status, "indexed_unix": r.indexed_unix,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if rows.is_empty() {
                println!("No archives registered. Add one with `backupsage master add <db>`.");
            } else {
                print_master_table(&rows);
            }
            Ok(0)
        }
        MasterCommands::Sync { prune } => {
            let mut m = open_master_checked(master_path)?;
            let actions = m.sync(prune)?;
            if actions.is_empty() {
                println!("All replicas up to date.");
            } else {
                for (label, action) in &actions {
                    println!("{label}: {action}");
                }
            }
            Ok(0)
        }
        MasterCommands::Verify { deep, json } => {
            let mut m = open_master_checked(master_path)?;
            let rows = m.verify(deep)?;
            let mut worst = 0;
            if json {
                let v: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "archive_id": r.archive_id, "label": r.label,
                            "status": r.status, "source": r.source_path,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_master_table(&rows);
            }
            for r in &rows {
                if r.status != master::STATUS_OK {
                    worst = 2;
                }
                if r.status == master::STATUS_STALE_INDEX {
                    eprintln!(
                        "note: '{}' changed since it was indexed — re-run \
                         `backupsage index {}`",
                        r.label, r.source_path
                    );
                }
            }
            Ok(worst)
        }
        MasterCommands::Rm { key } => {
            let mut m = open_master_checked(master_path)?;
            m.rm(&key)?;
            println!("removed '{key}' (the index file itself is untouched)");
            Ok(0)
        }
    }
}

fn inspect_path(conn: &rusqlite::Connection, db_path: &std::path::Path, path: &str) -> Result<i32> {
    let mut stmt = conn.prepare(
        "SELECT id, entry_type, kind, size, mtime_unix, mode, content_hash, img_w, img_h,
                phash, exif_unix, exif_src, flags, link_target
         FROM files WHERE path = ?1 ORDER BY id",
    )?;
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        i64,
        String,
        String,
        i64,
        Option<i64>,
        Option<i64>,
        Option<Vec<u8>>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        i64,
        Option<String>,
    )> = stmt
        .query_map([path], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if rows.is_empty() {
        // v2 index (no files table) or unknown path.
        if backupsage::store::schema_version(conn).unwrap_or(1) < 3 {
            bail!("this is a pre-v3 index without metadata — re-index the archive first");
        }
        bail!("no entry '{path}' in {}", db_path.display());
    }

    println!("Index    : {}", db_path.display());
    println!(
        "Source   : {}",
        searcher::get_meta(conn, "source").unwrap_or_default()
    );
    println!("Path     : {path}");
    for (i, row) in rows.iter().enumerate() {
        let (id, etype, kind, size, mtime, mode, hash, w, h, phash, exif, exif_src, fl, target) =
            row;
        if rows.len() > 1 {
            println!("── entry {} of {} (id {id}) ──", i + 1, rows.len());
        }
        println!("Type     : {etype} ({kind})");
        println!("Size     : {} ({size} bytes)", human_bytes(*size as u64));
        if let Some(m) = mtime {
            println!("Mtime    : {} (tar header / fs)", fmt_unix(*m));
        }
        if let Some(m) = mode {
            println!("Mode     : {m:o}");
        }
        if let Some(t) = target {
            println!("Target   : {t}");
        }
        if let Some(hh) = hash {
            println!(
                "BLAKE3   : {}",
                hh.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
        }
        if let (Some(w), Some(h)) = (w, h) {
            println!("Pixels   : {w}x{h}");
        }
        if let Some(p) = phash {
            println!(
                "pHash    : {:016x} ({})",
                *p as u64,
                searcher::get_meta(conn, "phash_algo").unwrap_or_default()
            );
        }
        if let Some(e) = exif {
            println!(
                "EXIF     : {} ({})",
                fmt_unix(*e),
                exif_src.as_deref().unwrap_or("unknown field")
            );
        }
        let mut markers = Vec::new();
        for (bit, name) in [
            (backupsage::store::flags::FTS_TRUNCATED, "fts-truncated"),
            (backupsage::store::flags::IMAGE_OVER_CAP, "image-over-cap"),
            (backupsage::store::flags::READ_ERROR, "read-error"),
            (backupsage::store::flags::SPARSE, "sparse"),
            (backupsage::store::flags::SHADOWED, "shadowed"),
            (backupsage::store::flags::DECODE_FAILED, "decode-failed"),
        ] {
            if fl & bit != 0 {
                markers.push(name);
            }
        }
        if !markers.is_empty() {
            println!("Flags    : {}", markers.join(", "));
        }
    }
    Ok(0)
}

fn warn_if_incomplete(conn: &rusqlite::Connection) {
    if searcher::index_completed(conn) == Some(false) {
        eprintln!("warning: this index was interrupted before finishing — results are partial");
    }
}

// ── Rendering ────────────────────────────────────────────────────────────────

fn print_index_summary(s: &IndexSummary) {
    println!();
    println!("Indexed  : {} text files", s.files_indexed);
    println!("Hashed   : {} files (full content)", s.files_hashed);
    if s.images_phashed + s.images_no_phash > 0 {
        println!(
            "Images   : {} with perceptual hash, {} without (HEIC/RAW/oversized/undecodable)",
            s.images_phashed, s.images_no_phash
        );
    }
    println!(
        "Skipped  : {} binary files (names only)",
        s.files_skipped_binary
    );
    if s.files_link_names > 0 {
        println!("Links    : {} hard/symlinks", s.files_link_names);
    }
    if s.files_truncated > 0 {
        println!(
            "Truncated: {} files larger than the text cap (raise with --max-file-size)",
            s.files_truncated
        );
    }
    println!("Database : {}", s.db_path.display());
}

fn render_dedup_report(r: &DedupReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    if r.groups.is_empty() {
        let _ = writeln!(out, "No duplicate groups found.");
    }
    for g in &r.groups {
        let archives: std::collections::BTreeSet<&str> =
            g.members.iter().map(|m| m.archive_label.as_str()).collect();
        let match_desc = if g.match_kind == "near" {
            format!("near (max dist {})", g.max_distance)
        } else {
            "exact".to_string()
        };
        let _ = writeln!(
            out,
            "Group {}  {} · {} copies · {} archive(s) · {} reclaimable",
            g.group_id,
            match_desc,
            g.members.len(),
            archives.len(),
            human_bytes(g.reclaimable_bytes),
        );
        let label_w = g
            .members
            .iter()
            .map(|m| m.archive_label.len())
            .max()
            .unwrap_or(10)
            .min(24);
        let path_w = g
            .members
            .iter()
            .map(|m| m.path.len())
            .max()
            .unwrap_or(20)
            .min(48);
        for m in &g.members {
            let ts = m
                .best_ts_unix
                .map(fmt_unix)
                .unwrap_or_else(|| "unknown".into());
            let mut extras = Vec::new();
            if let (Some(w), Some(h)) = (m.width, m.height) {
                extras.push(format!("{w}x{h}"));
            }
            if let Some(d) = m.hamming_to_keep {
                if !m.keep && d > 0 {
                    extras.push(format!("[dist {d}]"));
                }
            }
            if m.shadowed {
                extras.push("[shadowed]".into());
            }
            if m.sparse {
                extras.push("[sparse]".into());
            }
            if m.hardlink_of.is_some() {
                extras.push("[hardlink]".into());
            }
            if let Some(reason) = &m.keep_reason {
                extras.push(format!("({reason})"));
            }
            let _ = writeln!(
                out,
                "  {:4}  {:<label_w$}  {:<path_w$}  {:>9}  {} {:<22} {}",
                if m.keep { "KEEP" } else { "dup" },
                truncate_middle(&m.archive_label, label_w),
                truncate_middle(&m.path, path_w),
                human_bytes(m.size),
                ts,
                m.best_ts_source,
                extras.join(" "),
            );
        }
        let _ = writeln!(out);
    }

    let s = &r.summary;
    let _ = writeln!(
        out,
        "{} group(s) · {} duplicate file(s) · {} reclaimable",
        s.groups,
        s.duplicate_files,
        human_bytes(s.reclaimable_bytes)
    );
    if s.intra_archive_shadowed_bytes > 0 {
        let _ = writeln!(
            out,
            "shadowed intra-archive waste: {} (same path stored twice inside one tar)",
            human_bytes(s.intra_archive_shadowed_bytes)
        );
    }
    if s.images_without_phash > 0 {
        let _ = writeln!(
            out,
            "note: {} image(s) have no perceptual hash (HEIC/RAW/oversized) — \
             near-dup detection cannot see them; exact matching still applies",
            s.images_without_phash
        );
    }
    if s.near_buckets_skipped > 0 {
        let _ = writeln!(
            out,
            "warning: {} near-dup bucket(s) exceeded the cap and were skipped",
            s.near_buckets_skipped
        );
    }
    for label in &s.archives_offline {
        let _ = writeln!(out, "note: '{label}' is offline — dedup used its replica");
    }
    for label in &s.archives_incomplete {
        let _ = writeln!(
            out,
            "warning: '{label}' has an incomplete index — data is partial"
        );
    }
    for (label, reason) in &s.skipped_archives {
        let _ = writeln!(out, "skipped: {label}: {reason}");
    }
    out
}

fn print_master_table(rows: &[master::ArchiveRow]) {
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        header_cell("ID"),
        header_cell("Label"),
        header_cell("Type"),
        header_cell("Files"),
        header_cell("Schema"),
        header_cell("Status"),
        header_cell("Indexed"),
        header_cell("Source"),
    ]);
    for r in rows {
        let status_color = match r.status.as_str() {
            "ok" => Color::Green,
            "v2-limited" | "incomplete" => Color::Yellow,
            _ => Color::Red,
        };
        table.add_row(vec![
            Cell::new(r.archive_id).set_alignment(CellAlignment::Right),
            Cell::new(&r.label).add_attribute(Attribute::Bold),
            Cell::new(&r.source_type),
            Cell::new(format_number(r.files_count)).set_alignment(CellAlignment::Right),
            Cell::new(format!("v{}", r.schema_version)),
            Cell::new(&r.status).fg(status_color),
            Cell::new(r.indexed_unix.map(fmt_unix).unwrap_or_else(|| "-".into())),
            Cell::new(&r.source_path).fg(Color::DarkGrey),
        ]);
    }
    println!("{table}");
}

fn search_all_json(outcome: &searcher::FederatedOutcome) -> String {
    let archives: Vec<serde_json::Value> = outcome
        .per_archive
        .iter()
        .map(|f| {
            serde_json::json!({
                "archive": f.archive_label,
                "truncated": f.truncated,
                "hits": f.hits.iter().map(|h| serde_json::json!({
                    "path": h.path, "matches": h.matches, "snippet": h.snippet,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let skipped: Vec<serde_json::Value> = outcome
        .skipped
        .iter()
        .map(|(l, r)| serde_json::json!({"archive": l, "reason": r}))
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "archives": archives, "skipped": skipped,
    }))
    .expect("json render cannot fail")
}

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

// ── Formatting helpers ───────────────────────────────────────────────────────

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Unix timestamp → "YYYY-MM-DD HH:MM" (UTC).
fn fmt_unix(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60
    )
}

/// Inverse of days_from_civil (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn truncate_middle(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count + 1 - max).collect();
    format!("…{tail}")
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
    use super::{civil_from_days, fmt_unix, format_number, human_bytes};

    #[test]
    fn formats_numbers() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1000), "1,000");
        assert_eq!(format_number(1234567), "1,234,567");
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(4 * 1024 * 1024 + 100 * 1024), "4.1 MiB");
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(fmt_unix(0), "1970-01-01 00:00");
        assert_eq!(fmt_unix(1_688_498_553), "2023-07-04 19:22");
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
