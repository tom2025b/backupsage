// last_edited_by: codex
// **Signed:** codex · 2026-09-26T22:42:31-04:00
//! Conservative directory reuse. last_edited_by: codex
//! A fresh full hash is the only content-equality gate; timestamps never are.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use walkdir::WalkDir;

use crate::index_read::LockedIndex;
use crate::indexer::{ContentMode, EntryOutcome, IndexOptions};
use crate::progress::{OperationControl, ProgressEvent};
use crate::searcher::{get_meta, open_index};
use crate::store::flags;

// Bump when content derivation changes so old processing results are not reused.
pub(super) const VERSION: &str = "1";

pub(super) fn open_previous(
    path: &Path,
    opts: &IndexOptions,
    control: &OperationControl<'_>,
) -> Option<LockedIndex> {
    if !path.exists() {
        return None;
    }
    let candidate = (|| -> Result<LockedIndex> {
        anyhow::ensure!(!opts.force_full, "--force-full requested");
        anyhow::ensure!(
            opts.mode == ContentMode::Full,
            "content mode requires fresh processing"
        );
        let conn = open_index(path)?;
        for (key, expected) in [
            ("schema_version", "3".to_string()),
            ("completed", "1".to_string()),
            ("source_type", "dir".to_string()),
            ("hash_algo", "blake3".to_string()),
            ("phash_algo", crate::phash::PHASH_ALGO.to_string()),
            ("directory_reuse_version", VERSION.to_string()),
            ("content_mode", "full".to_string()),
            ("text_cap", opts.max_file_size.to_string()),
            ("media_cap", opts.media_cap.to_string()),
        ] {
            anyhow::ensure!(
                get_meta(&conn, key).as_deref() == Some(&expected),
                "prior index has incompatible {key}"
            );
        }
        Ok(conn)
    })();
    match candidate {
        Ok(conn) => {
            control.emit(ProgressEvent::Warning {
                message: "directory re-index: full subtree content rescan; timestamps and watcher hints cannot prove unchanged bytes. Reusing derived results only after full BLAKE3 verification.",
            });
            Some(conn)
        }
        Err(error) => {
            control.emit(ProgressEvent::Warning {
                message: &format!(
                    "directory re-index: full subtree rescan with fresh processing ({error:#})"
                ),
            });
            None
        }
    }
}

pub(super) fn previous_outcome(
    conn: &LockedIndex,
    path: &str,
    raw: Option<&[u8]>,
) -> Result<Option<EntryOutcome>> {
    // Exact raw identity prevents two lossy display names sharing cached results.
    let mut stmt = conn.prepare_cached(
        "SELECT f.kind, f.content_hash, f.img_w, f.img_h, f.phash,
                f.exif_unix, f.exif_src, f.flags, t.content
         FROM files f JOIN files_fts t ON t.rowid = f.id
         WHERE f.path = ?1 AND f.path_raw IS ?2 AND f.entry_type = 'file'
         ORDER BY f.id DESC LIMIT 1",
    )?;
    let result = stmt
        .query_row(params![path, raw], |row| {
            let kind: String = row.get(0)?;
            let kind = match kind.as_str() {
                "text" => "text",
                "empty" => "empty",
                "binary" => "binary",
                "image" => "image",
                "raw" => "raw",
                "video" => "video",
                _ => return Ok(None),
            };
            let hash: Option<Vec<u8>> = row.get(1)?;
            let Some(hash) = hash.and_then(|h| <[u8; 32]>::try_from(h).ok()) else {
                return Ok(None);
            };
            let exif_src: Option<String> = row.get(6)?;
            let exif_src = match exif_src.as_deref() {
                None => None,
                Some("DateTimeOriginal") => Some("DateTimeOriginal"),
                Some("DateTimeDigitized") => Some("DateTimeDigitized"),
                Some("DateTime") => Some("DateTime"),
                Some(_) => return Ok(None),
            };
            let flags: i64 = row.get(7)?;
            if flags & !(flags::FTS_TRUNCATED | flags::IMAGE_OVER_CAP) != 0 {
                return Ok(None); // Retry read/decode errors; never propagate unknown flags.
            }
            let text: String = row.get(8)?;
            Ok(Some(EntryOutcome {
                content_hash: Some(hash),
                kind,
                img_w: row.get(2)?,
                img_h: row.get(3)?,
                phash: row.get::<_, Option<i64>>(4)?.map(|p| p as u64),
                exif_unix: row.get(5)?,
                exif_src,
                flags,
                fts_text: matches!(kind, "text" | "empty").then_some(text),
                truncated_text: flags & flags::FTS_TRUNCATED != 0,
            }))
        })
        .optional()?;
    Ok(result.flatten())
}

/// Excludes atime, which our reads can update. Retains nanoseconds, inode and
/// ctime so same-size rewrites/replacements and chmod during hashing are detected.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    links: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl From<&Metadata> for Stamp {
    fn from(md: &Metadata) -> Self {
        Self {
            dev: md.dev(),
            ino: md.ino(),
            size: md.len(),
            mode: md.mode(),
            links: md.nlink(),
            mtime: (md.mtime(), md.mtime_nsec()),
            ctime: (md.ctime(), md.ctime_nsec()),
        }
    }
}

/// Keep the before/after checks around the complete read, including cache reuse.
/// A closure makes mutation during a read deterministic in the unit regression.
pub(super) fn with_stable_file<T>(
    file: &mut File,
    path: &Path,
    expected: Option<&Stamp>,
    read: impl FnOnce(&mut File) -> Result<T>,
) -> Result<T> {
    let opened = Stamp::from(&file.metadata()?);
    anyhow::ensure!(
        Some(&opened) == expected,
        "unstable directory entry '{}': changed before hashing; previous index preserved",
        path.display()
    );
    let result = read(file)?;
    anyhow::ensure!(
        opened == Stamp::from(&file.metadata()?)
            && opened == Stamp::from(&std::fs::symlink_metadata(path)?),
        "unstable directory entry '{}': changed during hashing; previous index preserved",
        path.display()
    );
    Ok(result)
}

pub(super) fn snapshot_tree(
    dir: &Path,
    control: &OperationControl<'_>,
) -> Result<BTreeMap<PathBuf, Option<Stamp>>> {
    let mut snapshot = BTreeMap::new();
    for entry in WalkDir::new(dir).follow_links(false) {
        control.check_cancelled()?;
        let entry = entry.context("cannot reconcile directory source")?;
        if entry.file_type().is_dir() {
            crate::borg_guard::reject_borg_directory(entry.path())?;
        }
        let stamp = match std::fs::symlink_metadata(entry.path()) {
            Ok(md) => Some(Stamp::from(&md)),
            Err(error) if !entry.file_type().is_dir() => {
                control.emit(ProgressEvent::Warning {
                    message: &format!(
                        "cannot stat '{}' during reconciliation: {error}",
                        entry.path().display()
                    ),
                });
                None
            }
            Err(error) => return Err(error).context("cannot stat source directory"),
        };
        snapshot.insert(entry.path().to_path_buf(), stamp);
    }
    Ok(snapshot)
}

/// A degraded row makes no content claim. Ignore only that exact path and the
/// membership timestamps of its existing parent directories, retaining their
/// identity/mode and all other paths' full stability checks.
pub(super) fn snapshots_match(
    before: &BTreeMap<PathBuf, Option<Stamp>>,
    after: &BTreeMap<PathBuf, Option<Stamp>>,
    degraded: &BTreeSet<PathBuf>,
) -> bool {
    let before: BTreeMap<_, _> = before
        .iter()
        .filter(|(p, _)| !degraded.contains(*p))
        .collect();
    let after: BTreeMap<_, _> = after
        .iter()
        .filter(|(p, _)| !degraded.contains(*p))
        .collect();
    before.len() == after.len()
        && before.iter().all(|(path, old)| {
            let Some(new) = after.get(path) else {
                return false;
            };
            if old == new {
                return true;
            }
            match (old.as_ref(), new.as_ref()) {
                (Some(a), Some(b))
                    if a.mode & libc::S_IFMT == libc::S_IFDIR
                        && degraded.iter().any(|p| p.starts_with(path)) =>
                {
                    (a.dev, a.ino, a.mode, a.links) == (b.dev, b.ino, b.mode, b.links)
                }
                _ => false,
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, FileTimes};
    use std::io::Read;

    #[test]
    fn changes_during_read_reject_both_in_place_writes_and_replacements() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        for replace in [false, true] {
            fs::write(&path, b"old bytes").unwrap();
            let before = Stamp::from(&fs::symlink_metadata(&path).unwrap());
            let mtime = fs::metadata(&path).unwrap().modified().unwrap();
            let mut file = File::open(&path).unwrap();
            let error = with_stable_file(&mut file, &path, Some(&before), |reader| {
                let mut first = [0];
                reader.read_exact(&mut first)?;
                if replace {
                    let replacement = temp.path().join("replacement");
                    fs::write(&replacement, b"new bytes")?;
                    fs::rename(replacement, &path)?;
                } else {
                    fs::write(&path, b"new bytes")?;
                }
                File::options()
                    .write(true)
                    .open(&path)?
                    .set_times(FileTimes::new().set_modified(mtime))?;
                let mut rest = Vec::new();
                reader.read_to_end(&mut rest)?;
                Ok(())
            })
            .unwrap_err();
            assert!(
                error.to_string().contains("changed during hashing"),
                "{error}"
            );
        }
    }
}
