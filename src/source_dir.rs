//! Directory sources: walk a folder and feed the same per-entry pipeline the
//! tar front-end uses. A directory index is a point-in-time snapshot exactly
//! like an archive index; paths are stored relative to the source root
//! (tar-style). The index lands at the sibling `<dir>.db` — never inside the
//! source, which would contaminate future backups of it.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use walkdir::WalkDir;

use crate::indexer::{
    create_db_with_fallback, process_reader, truncate_path, IndexOptions, IndexRun, IndexSummary,
};
use crate::store::{flags, EntryRecord};

pub(crate) fn index_dir(
    dir: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
) -> Result<IndexSummary> {
    let (paths, conn) = create_db_with_fallback(dir, explicit_db, "dir", opts)?;
    let db_path = paths.final_path.clone();
    // Names to skip during the walk: the final output and the staged
    // build (plus their live WAL/SHM siblings), in the output directory.
    let skip_names: Vec<String> = [&paths.final_path, &paths.staged]
        .iter()
        .filter_map(|p| p.file_name())
        .flat_map(|n| {
            let n = n.to_string_lossy();
            [n.to_string(), format!("{n}-wal"), format!("{n}-shm")]
        })
        .collect();
    let out_dir_canon = paths
        .final_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
        .canonicalize()
        .ok();

    println!("Source  : {} (directory)", dir.display());
    println!("Index   : {}", db_path.display());
    println!();

    // Cheap metadata-only pre-count so the bar has a total.
    let total = WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter()
        .flatten()
        .filter(|e| !e.file_type().is_dir())
        .count() as u64;
    let pb = ProgressBar::new(total);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {pos}/{len} files ({per_sec}, eta {eta}) — {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );

    let mut summary = IndexSummary {
        db_path: db_path.clone(),
        format: "directory".to_string(),
        ..IndexSummary::default()
    };
    let mut run = IndexRun::new(&conn, opts, &mut summary)?;
    let mut entry_no = 0u64;

    for walk_entry in WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
    {
        let walk_entry = match walk_entry {
            Ok(e) => e,
            Err(e) => {
                pb.suspend(|| {
                    eprintln!(
                        "{}",
                        crate::textsafe::sanitize(&format!("warning: cannot walk: {e}"))
                    )
                });
                continue;
            }
        };
        if walk_entry.file_type().is_dir() {
            continue;
        }
        let abs = walk_entry.path();
        // Never index our own output — the staged build and the final
        // index, including their live WAL/SHM siblings.
        let own_output = abs
            .file_name()
            .is_some_and(|n| skip_names.iter().any(|s| *s == n.to_string_lossy()))
            && abs
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .is_some_and(|p| Some(p) == out_dir_canon);
        if own_output {
            continue;
        }
        let rel_path = abs
            .strip_prefix(dir)
            .unwrap_or(abs)
            .to_string_lossy()
            .into_owned();

        entry_no += 1;
        pb.inc(1);
        if entry_no % 64 == 1 {
            pb.set_message(crate::textsafe::sanitize(&truncate_path(&rel_path, 50)).into_owned());
        }

        let md = walk_entry.metadata().ok();
        let mtime = md.as_ref().and_then(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        });
        let mode = md.as_ref().map(unix_mode);

        if walk_entry.file_type().is_symlink() {
            let target = std::fs::read_link(abs)
                .ok()
                .map(|t| t.to_string_lossy().into_owned());
            let rec = EntryRecord {
                path: &rel_path,
                entry_type: "symlink",
                link_target: target.as_deref(),
                size: 0,
                mtime_unix: mtime,
                mode,
                kind: "link",
                content_hash: None,
                img_w: None,
                img_h: None,
                phash: None,
                exif_unix: None,
                exif_src: None,
                flags: 0,
                fts_content: "",
            };
            run.record(&rec, None)?;
            run.summary.files_link_names += 1;
            continue;
        }

        let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
        let outcome = match File::open(abs) {
            Ok(mut f) => process_reader(&mut f, size, &rel_path, opts, &mut |msg| {
                pb.suspend(|| eprintln!("{}", crate::textsafe::sanitize(&msg)))
            }),
            Err(e) => {
                // Unreadable file: warn-and-continue with a name-only row,
                // mirroring the tar front-end's read-error handling.
                pb.suspend(|| {
                    eprintln!(
                        "{}",
                        crate::textsafe::sanitize(&format!(
                            "warning: cannot open '{rel_path}': {e}"
                        ))
                    )
                });
                crate::indexer::EntryOutcome {
                    content_hash: None,
                    kind: "binary",
                    img_w: None,
                    img_h: None,
                    phash: None,
                    exif_unix: None,
                    exif_src: None,
                    flags: flags::READ_ERROR,
                    fts_text: None,
                    truncated_text: false,
                }
            }
        };

        let rec = EntryRecord {
            path: &rel_path,
            entry_type: "file",
            link_target: None,
            size,
            mtime_unix: mtime,
            mode,
            kind: outcome.kind,
            content_hash: outcome.content_hash,
            img_w: outcome.img_w,
            img_h: outcome.img_h,
            phash: outcome.phash,
            exif_unix: outcome.exif_unix,
            exif_src: outcome.exif_src,
            flags: outcome.flags,
            fts_content: outcome.fts_text.as_deref().unwrap_or(""),
        };
        run.record(&rec, Some(&outcome))?;
    }

    // Directories have no meaningful whole-source fingerprint in v1.0.
    run.finish(None)
        .context("failed to finalise directory index")?;
    pb.finish_with_message("done");
    drop(conn); // close the staged database before promoting it
    paths.promote()?;
    Ok(summary)
}

#[cfg(unix)]
fn unix_mode(md: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode()
}

#[cfg(not(unix))]
fn unix_mode(_md: &std::fs::Metadata) -> u32 {
    0
}
