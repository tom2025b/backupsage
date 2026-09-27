// last_edited_by: codex
// **Signed:** codex · 2026-09-26T22:42:31-04:00
//! Directory sources: walk a folder and feed the same per-entry pipeline the
//! tar front-end uses. A directory index is a point-in-time snapshot exactly
//! like an archive index; paths are stored relative to the source root
//! (tar-style). The index lands at the sibling `<dir>.db` — never inside the
//! source, which would contaminate future backups of it.

mod reuse;

use std::collections::BTreeSet;
use std::fs::{File, Metadata};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result};
use walkdir::WalkDir;

use crate::indexer::{
    create_db_with_fallback, process_reader_with_reuse, IndexOptions, IndexRun, IndexSummary,
};
use crate::progress::{OperationControl, ProgressEvent, ProgressUnit, SourceKind};
use crate::store::{flags, EntryRecord};

pub(crate) fn index_dir(
    dir: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
    control: &OperationControl<'_>,
) -> Result<IndexSummary> {
    index_dir_controlled(dir, explicit_db, opts, || Ok(()), control)
}

#[cfg(test)]
fn index_dir_after_count<F>(
    dir: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
    after_count: F,
) -> Result<IndexSummary>
where
    F: FnOnce() -> Result<()>,
{
    index_dir_controlled(
        dir,
        explicit_db,
        opts,
        after_count,
        &OperationControl::default(),
    )
}

fn index_dir_controlled<F>(
    dir: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
    after_count: F,
    control: &OperationControl<'_>,
) -> Result<IndexSummary>
where
    F: FnOnce() -> Result<()>,
{
    let (paths, conn) = create_db_with_fallback(dir, explicit_db, "dir", opts, control)?;
    let db_path = paths.final_path.clone();
    let previous = reuse::open_previous(&db_path, opts, control);
    crate::store::set_meta(&conn, "directory_reuse_version", reuse::VERSION)?;
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

    control.emit(ProgressEvent::IndexStarted {
        source: dir,
        destination: &db_path,
        kind: SourceKind::Directory,
    });
    control.check_cancelled()?;

    // Cheap metadata-only pre-count so the bar has a total.  This is a
    // safety walk too: errors and nested Borg candidates abort instead of
    // being flattened away, before any content file is opened.
    let before = reuse::snapshot_tree(dir, control)?;
    let total = count_entries_controlled(dir, control)?;
    after_count()?;
    control.emit(ProgressEvent::Total {
        amount: total,
        unit: ProgressUnit::Files,
    });

    let mut summary = IndexSummary {
        db_path: db_path.clone(),
        format: "directory".to_string(),
        mode: opts.mode.as_str().to_string(),
        ..IndexSummary::default()
    };
    let mut run = IndexRun::new(&conn, opts, &mut summary)?;
    let mut entry_no = 0u64;
    let mut degraded = BTreeSet::new();

    let mut walker = WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();
    while let Some(walk_entry) = walker.next() {
        control.check_cancelled()?;
        let walk_entry = walk_entry.context("cannot walk directory source")?;
        if walk_entry.file_type().is_dir() {
            if let Err(e) = crate::borg_guard::reject_borg_directory(walk_entry.path()) {
                walker.skip_current_dir();
                return Err(e);
            }
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
        // Lossless capture (same rule as the tar front-end): valid UTF-8
        // stores text only; anything else stores lossy text + raw bytes.
        let (rel_path, path_raw) = {
            use std::os::unix::ffi::OsStrExt;
            crate::store::capture_text(abs.strip_prefix(dir).unwrap_or(abs).as_os_str().as_bytes())
        };

        entry_no += 1;
        control.emit(ProgressEvent::Advanced { amount: 1 });
        control.emit(ProgressEvent::Entry {
            path: &rel_path,
            number: entry_no,
        });
        control.check_cancelled()?;

        let md = match std::fs::symlink_metadata(abs) {
            Ok(md) => Some(md),
            Err(error) => {
                record_degraded(
                    &mut run,
                    abs,
                    dir,
                    None,
                    file_type_name(walk_entry.file_type()),
                    &format!("cannot stat: {error}"),
                    control,
                )?;
                degraded.insert(abs.to_path_buf());
                continue;
            }
        };
        let mtime = md.as_ref().and_then(|m| {
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
        });
        let mode = md.as_ref().map(unix_mode);

        if md.as_ref().unwrap().file_type().is_symlink() {
            let (target, target_raw) = match std::fs::read_link(abs) {
                Ok(t) => {
                    use std::os::unix::ffi::OsStrExt;
                    let (text, raw) = crate::store::capture_text(t.as_os_str().as_bytes());
                    (Some(text), raw)
                }
                Err(_) => (None, None),
            };
            let rec = EntryRecord {
                path: &rel_path,
                path_raw: path_raw.as_deref(),
                entry_type: "symlink",
                link_target: target.as_deref(),
                link_target_raw: target_raw.as_deref(),
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

        let file_type = md.as_ref().unwrap().file_type();
        if !file_type.is_file() {
            let kind = file_type_name(file_type);
            record_degraded(
                &mut run,
                abs,
                dir,
                md.as_ref(),
                kind,
                &format!("skipped non-regular entry: {kind}"),
                control,
            )?;
            degraded.insert(abs.to_path_buf());
            continue;
        }
        if before.get(abs).is_some_and(Option::is_none) {
            record_degraded(
                &mut run,
                abs,
                dir,
                md.as_ref(),
                "file",
                "cannot stat during initial reconciliation; retry on next scan",
                control,
            )?;
            degraded.insert(abs.to_path_buf());
            continue;
        }
        let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
        if opts.mode == crate::indexer::ContentMode::MetadataOnly {
            // Metadata-only (#39): never open the file — stat facts only.
            let rec = EntryRecord {
                path: &rel_path,
                path_raw: path_raw.as_deref(),
                entry_type: "file",
                link_target: None,
                link_target_raw: None,
                size,
                mtime_unix: mtime,
                mode,
                kind: crate::indexer::metadata_kind(&rel_path, size),
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
            continue;
        }
        let cached = if let Some(previous) = &previous {
            match reuse::previous_outcome(previous, &rel_path, path_raw.as_deref()) {
                Ok(cached) => cached,
                Err(error) => {
                    control.emit(ProgressEvent::Warning {
                        message: &format!("directory re-index: fresh processing for '{rel_path}' (unusable cached row: {error})"),
                    });
                    None
                }
            }
        } else {
            None
        };
        let mut file = match open_regular(abs) {
            Ok(file) => file,
            Err((kind, reason)) => {
                record_degraded(&mut run, abs, dir, md.as_ref(), kind, &reason, control)?;
                degraded.insert(abs.to_path_buf());
                continue;
            }
        };
        let (outcome, reused) = reuse::with_stable_file(
            &mut file,
            abs,
            before.get(abs).and_then(Option::as_ref),
            |f| {
                process_reader_with_reuse(
                    f,
                    size,
                    &rel_path,
                    opts,
                    &mut |msg| control.emit(ProgressEvent::Warning { message: &msg }),
                    control,
                    cached,
                )
            },
        )?;
        run.summary.files_reused += u64::from(reused);

        let rec = EntryRecord {
            path: &rel_path,
            path_raw: path_raw.as_deref(),
            entry_type: "file",
            link_target: None,
            link_target_raw: None,
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
    control.check_cancelled()?;
    crate::store::set_meta(
        &conn,
        "directory_files_reused",
        &run.summary.files_reused.to_string(),
    )?;
    run.finish(None)
        .context("failed to finalise directory index")?;
    control.emit(ProgressEvent::ReadyToPromote);
    control.check_cancelled()?;
    anyhow::ensure!(
        reuse::snapshots_match(&before, &reuse::snapshot_tree(dir, control)?, &degraded),
        "unstable directory subtree: changed during reconciliation; previous index preserved"
    );
    if let Some(previous) = previous {
        crate::searcher::finish_index(previous, &db_path)?;
    }
    drop(conn); // close the staged database before promoting it
    paths.promote()?;
    control.emit(ProgressEvent::Finished);
    Ok(summary)
}

// Check before opening, then use nonblocking/no-follow open and check the handle:
// a raced-in FIFO cannot hang and a symlink cannot redirect the content read.
fn open_regular(path: &Path) -> std::result::Result<File, (&'static str, String)> {
    open_regular_after_stat(path, || {})
}

fn open_regular_after_stat(
    path: &Path,
    after_stat: impl FnOnce(),
) -> std::result::Result<File, (&'static str, String)> {
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| ("unknown", format!("cannot stat before open: {e}")))?;
    if !md.is_file() {
        let kind = file_type_name(md.file_type());
        return Err((kind, format!("skipped non-regular entry: {kind}")));
    }
    after_stat();
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| ("file", format!("cannot open: {e}")))?;
    let opened = file
        .metadata()
        .map_err(|e| ("unknown", format!("cannot stat opened file: {e}")))?;
    if !opened.is_file() {
        let kind = file_type_name(opened.file_type());
        return Err((kind, format!("skipped non-regular opened entry: {kind}")));
    }
    Ok(file)
}

fn file_type_name(kind: std::fs::FileType) -> &'static str {
    if kind.is_file() {
        "file"
    } else if kind.is_dir() {
        "directory"
    } else if kind.is_symlink() {
        "symlink"
    } else if kind.is_fifo() {
        "fifo"
    } else if kind.is_socket() {
        "socket"
    } else if kind.is_block_device() {
        "block device"
    } else if kind.is_char_device() {
        "character device"
    } else {
        "unknown"
    }
}

fn record_degraded(
    run: &mut IndexRun<'_>,
    abs: &Path,
    root: &Path,
    md: Option<&Metadata>,
    actual_type: &str,
    reason: &str,
    control: &OperationControl<'_>,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let (path, raw) =
        crate::store::capture_text(abs.strip_prefix(root).unwrap_or(abs).as_os_str().as_bytes());
    control.emit(ProgressEvent::Warning {
        message: &format!("warning: '{path}': {reason}"),
    });
    // Preserve the v3 entry-type contract; older readers classify this as unknown
    // content via READ_ERROR. Exact type and OS error survive in additive metadata.
    let rec = EntryRecord {
        path: &path,
        path_raw: raw.as_deref(),
        entry_type: "file",
        link_target: None,
        link_target_raw: None,
        size: md.map_or(0, Metadata::len),
        mtime_unix: md
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
        mode: md.map(unix_mode),
        kind: "binary",
        content_hash: None,
        img_w: None,
        img_h: None,
        phash: None,
        exif_unix: None,
        exif_src: None,
        flags: flags::READ_ERROR,
        fts_content: "",
    };
    run.record(&rec, None)?;
    run.summary.files_skipped_binary += 1;
    let id = run.conn.last_insert_rowid();
    crate::store::set_meta(
        run.conn,
        &format!("directory_entry_issue:{id}"),
        &serde_json::json!({"path": path, "path_raw": raw, "type": actual_type, "reason": reason})
            .to_string(),
    )?;
    Ok(())
}

#[cfg(test)]
fn count_entries(dir: &Path) -> Result<u64> {
    count_entries_controlled(dir, &OperationControl::default())
}

fn count_entries_controlled(dir: &Path, control: &OperationControl<'_>) -> Result<u64> {
    let mut total = 0u64;
    let mut walker = WalkDir::new(dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();
    while let Some(entry) = walker.next() {
        control.check_cancelled()?;
        let entry = entry.context("cannot pre-count directory source")?;
        if entry.file_type().is_dir() {
            if let Err(e) = crate::borg_guard::reject_borg_directory(entry.path()) {
                walker.skip_current_dir();
                return Err(e);
            }
        } else {
            total += 1;
        }
    }
    Ok(total)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_swap_after_stat_is_nonblocking_and_rejected_on_handle() {
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("entry");
            std::fs::write(&path, "regular before stat").unwrap();
            let result = open_regular_after_stat(&path, || {
                std::fs::remove_file(&path).unwrap();
                assert!(std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success());
            });
            send.send(result.map(|_| ())).unwrap();
        });
        let result = receive
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("opening a raced-in FIFO must not block");
        assert!(result
            .unwrap_err()
            .1
            .contains("skipped non-regular opened entry: fifo"));
    }

    fn add_candidate(path: &Path) {
        std::fs::create_dir_all(path.join("data/0")).unwrap();
        std::fs::write(path.join("data/0/0"), b"segment sentinel").unwrap();
        std::fs::write(
            path.join("config"),
            b"[repository]\nversion = 1\nid = 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        )
        .unwrap();
    }

    #[test]
    fn pre_count_refuses_nested_repository_instead_of_flattening_it() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join("ordinary")).unwrap();
        std::fs::write(source.join("ordinary/file.txt"), b"ordinary").unwrap();
        add_candidate(&source.join("nested"));

        let error = format!("{:#}", count_entries(&source).unwrap_err());
        assert!(error.contains("Borg repository candidate"), "{error}");
    }

    #[test]
    fn sorted_walk_rechecks_directory_that_changes_after_pre_count() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let nested = source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(source.join("ordinary.txt"), b"ordinary").unwrap();

        let result = index_dir_after_count(&source, None, &IndexOptions::default(), || {
            add_candidate(&nested);
            Ok(())
        });

        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("Borg repository candidate"), "{error}");
        assert!(!temp.path().join("source.db").exists());
        let names: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|name| !name.contains(".tmp.")),
            "{names:?}"
        );
    }
}
