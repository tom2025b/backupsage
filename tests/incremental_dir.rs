// last_edited_by: codex
// **Signed:** codex · 2026-09-26T22:42:31-04:00
//! Directory reconciliation regression evidence. last_edited_by: codex
mod common;

use std::cell::RefCell;
use std::fs::{self, File, FileTimes};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use backupsage::indexer::{self, ContentMode, IndexOptions, IndexSummary};
use backupsage::progress::{CancellationToken, OperationControl, ProgressEvent, ProgressObserver};
use backupsage::searcher;
use rusqlite::types::Value;

fn index(source: &Path, opts: &IndexOptions) -> IndexSummary {
    indexer::run_index(source, None, opts).unwrap()
}

fn rows(db: &Path, sql: &str) -> Vec<Vec<Value>> {
    let conn = searcher::open_index(db).unwrap();
    let result = {
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        stmt.query_map([], |row| (0..n).map(|i| row.get(i)).collect())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    searcher::finish_index(conn, db).unwrap();
    result
}

fn assert_converges(source: &Path, incremental: &Path, opts: &IndexOptions) {
    let full = source.with_extension("forced.db");
    let force_opts = IndexOptions {
        force_full: true,
        ..*opts
    };
    let result = indexer::run_index(source, Some(&full), &force_opts).unwrap();
    assert_eq!(result.files_reused, 0);
    for sql in [
        "SELECT path,path_raw,entry_type,link_target,link_target_raw,size,mtime_unix,mode,kind,content_hash,img_w,img_h,phash,exif_unix,exif_src,flags FROM files ORDER BY path,path_raw",
        "SELECT path,content FROM files_fts ORDER BY path,content",
        "SELECT word,total_count,doc_count FROM word_freq ORDER BY word",
        "SELECT key,value FROM meta WHERE key IN ('files_indexed','files_skipped','files_truncated','completed','content_mode','word_stats') ORDER BY key",
    ] {
        assert_eq!(rows(incremental, sql), rows(&full, sql), "{sql}");
    }
}

#[derive(Default)]
struct Warnings(RefCell<Vec<String>>);
impl ProgressObserver for Warnings {
    fn on_event(&self, event: ProgressEvent<'_>) {
        if let ProgressEvent::Warning { message } = event {
            self.0.borrow_mut().push(message.to_owned());
        }
    }
}

#[test]
fn unchanged_content_reuses_media_text_and_rebuilds_statistics() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("words.txt"), "repeat repeat word").unwrap();
    fs::write(src.join("empty"), "").unwrap();
    fs::write(src.join("binary"), b"\0\x01\x02").unwrap();
    fs::write(src.join("image.png"), common::png_bytes(2, 24, 24)).unwrap();
    let opts = IndexOptions::default();
    assert_eq!(index(&src, &opts).files_reused, 0);
    let warnings = Warnings::default();
    let summary = indexer::run_index_with_control(
        &src,
        None,
        &opts,
        &OperationControl::new(&warnings, CancellationToken::default()),
    )
    .unwrap();
    assert_eq!(summary.files_reused, 4);
    assert_eq!(summary.images_phashed, 1);
    assert_eq!(summary.files_hashed, 4);
    assert!(warnings
        .0
        .borrow()
        .iter()
        .any(|m| m.contains("full subtree content rescan")));
    assert_converges(&src, &summary.db_path, &opts);
}

#[test]
fn same_size_restored_mtime_edit_and_replacement_never_reuse_stale_content() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir_all(src.join("nested")).unwrap();
    let edit = src.join("nested/edit.txt");
    let replace = src.join("replace.txt");
    fs::write(&edit, "oldtoken").unwrap();
    fs::write(&replace, "oldtoken").unwrap();
    let opts = IndexOptions::default();
    index(&src, &opts);
    let time = fs::metadata(&edit).unwrap().modified().unwrap();
    fs::write(&edit, "newtoken").unwrap();
    File::options()
        .write(true)
        .open(&edit)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
    let tmp = src.join("replacement");
    fs::write(&tmp, "newtoken").unwrap();
    File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
    fs::rename(tmp, &replace).unwrap();
    let summary = index(&src, &opts);
    assert_eq!(summary.files_reused, 0);
    let conn = searcher::open_index(&summary.db_path).unwrap();
    assert_eq!(
        searcher::search(&conn, "newtoken", 10, false)
            .unwrap()
            .hits
            .len(),
        2
    );
    assert_eq!(
        searcher::search(&conn, "oldtoken", 10, false)
            .unwrap()
            .hits
            .len(),
        0
    );
    searcher::finish_index(conn, &summary.db_path).unwrap();
    assert_converges(&src, &summary.db_path, &opts);
}

#[test]
fn changed_bytes_beyond_retention_cap_are_reconciled() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    let path = src.join("large.txt");
    let mut bytes = vec![b'a'; 600_000];
    fs::write(&path, &bytes).unwrap();
    let opts = IndexOptions {
        max_file_size: 32,
        ..IndexOptions::default()
    };
    index(&src, &opts);
    let time = fs::metadata(&path).unwrap().modified().unwrap();
    bytes[500_000] = b'b';
    fs::write(&path, &bytes).unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(time))
        .unwrap();
    let summary = index(&src, &opts);
    assert_eq!(summary.files_reused, 0);
    assert_eq!(
        rows(&summary.db_path, "SELECT content_hash FROM files"),
        vec![vec![Value::Blob(blake3::hash(&bytes).as_bytes().to_vec())]]
    );
    assert_eq!(summary.files_truncated, 1);
    assert_converges(&src, &summary.db_path, &opts);
}

#[test]
fn namespace_links_permissions_and_clock_rollback_converge() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir_all(src.join("nested")).unwrap();
    fs::write(src.join("removed"), "removedtoken").unwrap();
    fs::write(src.join("nested/original"), "originaltoken").unwrap();
    fs::write(src.join("permissions"), "permissiontoken").unwrap();
    symlink("removed", src.join("link")).unwrap();
    let opts = IndexOptions::default();
    index(&src, &opts);
    fs::remove_file(src.join("removed")).unwrap();
    fs::rename(src.join("nested/original"), src.join("renamed")).unwrap();
    fs::remove_dir(src.join("nested")).unwrap();
    fs::write(src.join("added"), "addedtoken").unwrap();
    fs::hard_link(src.join("renamed"), src.join("hardlink")).unwrap();
    fs::set_permissions(src.join("permissions"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(src.join("link")).unwrap();
    symlink("added", src.join("link")).unwrap();
    File::options()
        .write(true)
        .open(src.join("permissions"))
        .unwrap()
        .set_times(FileTimes::new().set_modified(std::time::UNIX_EPOCH))
        .unwrap();
    let summary = index(&src, &opts);
    assert_eq!(summary.files_reused, 1);
    assert_converges(&src, &summary.db_path, &opts);
}

#[test]
fn exact_raw_names_never_alias_cached_rows() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    let a = src.join(std::ffi::OsString::from_vec(b"bad\xff.txt".to_vec()));
    let b = src.join(std::ffi::OsString::from_vec(b"bad\xfe.txt".to_vec()));
    fs::write(&a, "alphatoken").unwrap();
    fs::write(&b, "betatoken").unwrap();
    let opts = IndexOptions::default();
    index(&src, &opts);
    fs::write(&a, "gammatoken").unwrap();
    let summary = index(&src, &opts);
    assert_converges(&src, &summary.db_path, &opts);
}

#[test]
fn incompatible_options_modes_and_legacy_indexes_fall_back() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("words"), "first second third").unwrap();
    let mut opts = IndexOptions::default();
    let initial = index(&src, &opts);
    {
        let conn = rusqlite::Connection::open(&initial.db_path).unwrap();
        conn.execute("DELETE FROM meta WHERE key='directory_reuse_version'", [])
            .unwrap();
    }
    assert_eq!(index(&src, &opts).files_reused, 0);
    opts.max_file_size = 5;
    assert_eq!(index(&src, &opts).files_reused, 0);
    opts.media_cap = 8;
    assert_eq!(index(&src, &opts).files_reused, 0);
    opts.word_stats = false;
    let summary = index(&src, &opts);
    assert_eq!(summary.files_reused, 1);
    assert_converges(&src, &summary.db_path, &opts);
    for mode in [
        ContentMode::SearchOnly,
        ContentMode::MetadataOnly,
        ContentMode::Full,
    ] {
        opts.mode = mode;
        assert_eq!(index(&src, &opts).files_reused, 0);
        let summary = index(&src, &opts);
        assert_converges(&src, &summary.db_path, &opts);
    }
}

#[test]
fn cancellation_and_observed_instability_preserve_last_good_bytes() {
    struct Interrupt {
        token: CancellationToken,
        mutate: Option<PathBuf>,
    }
    impl ProgressObserver for Interrupt {
        fn on_event(&self, event: ProgressEvent<'_>) {
            if let ProgressEvent::ReadyToPromote = event {
                if let Some(path) = &self.mutate {
                    fs::write(path, "latechange").unwrap();
                } else {
                    self.token.cancel();
                }
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("words"), "originaltoken").unwrap();
    let opts = IndexOptions::default();
    let db = index(&src, &opts).db_path;
    let bytes = fs::read(&db).unwrap();
    for mutate in [None, Some(src.join("words")), Some(src.join("newpath"))] {
        let observer = Interrupt {
            token: CancellationToken::default(),
            mutate,
        };
        let error = indexer::run_index_with_control(
            &src,
            None,
            &opts,
            &OperationControl::new(&observer, observer.token.clone()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unstable") || error.to_string().contains("cancelled"));
        assert_eq!(fs::read(&db).unwrap(), bytes);
        assert!(fs::read_dir(temp.path()).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp.")));
    }
}

#[test]
fn force_full_cli_bypasses_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("words"), "words").unwrap();
    let opts = IndexOptions::default();
    let db = index(&src, &opts).db_path;
    assert_eq!(index(&src, &opts).files_reused, 1);
    let output = Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .env("HOME", temp.path())
        .args(["index", "--force-full"])
        .arg(&src)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        rows(
            &db,
            "SELECT value FROM meta WHERE key='directory_files_reused'"
        ),
        vec![vec![Value::Text("0".to_owned())]]
    );
}

fn assert_degraded(db: &Path, path: &str, reason: &str) {
    let conn = searcher::open_index(db).unwrap();
    let (flags, hash, text): (i64, Option<Vec<u8>>, String) = conn.query_row(
        "SELECT f.flags, f.content_hash, t.content FROM files f JOIN files_fts t ON t.rowid=f.id WHERE f.path=?1",
        [path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_ne!(flags & 4, 0);
    assert!(hash.is_none());
    assert!(text.is_empty());
    let mut stmt = conn
        .prepare("SELECT value FROM meta WHERE key LIKE 'directory_entry_issue:%'")
        .unwrap();
    let issues: Vec<serde_json::Value> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
        .collect();
    assert!(
        issues
            .iter()
            .any(|v| v["path"] == path && v["reason"].as_str().unwrap().contains(reason)),
        "{issues:?}"
    );
    drop(stmt);
    searcher::finish_index(conn, db).unwrap();
}

#[test]
fn permission_denied_file_is_recorded_and_never_reused() {
    if Command::new("id").arg("-u").output().unwrap().stdout == b"0\n" {
        eprintln!("skipped permission-denied fixture: running as root");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    let path = src.join("denied");
    fs::write(&path, "previously readable token").unwrap();
    fs::write(src.join("z-healthy"), "still indexed").unwrap();
    let opts = IndexOptions::default();
    index(&src, &opts);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
    for _ in 0..2 {
        let summary = index(&src, &opts);
        assert_eq!(summary.files_reused, 1); // only z-healthy
        assert_eq!(summary.files_skipped_binary, 1);
        assert_degraded(&summary.db_path, "denied", "Permission denied");
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(index(&src, &opts).files_reused, 1); // denied must be retried
    assert_eq!(index(&src, &opts).files_reused, 2);
}

#[test]
fn fifo_without_writer_finishes_with_skipped_type_and_no_reuse() {
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    assert!(Command::new("mkfifo")
        .arg(src.join("pipe"))
        .status()
        .unwrap()
        .success());
    fs::write(src.join("z-healthy"), "still indexed").unwrap();
    for expected in ["0", "1"] {
        // Supervise the entire indexing process, including open(), not just read().
        let output = Command::new("timeout")
            .args(["--kill-after=1s", "5s"])
            .arg(env!("CARGO_BIN_EXE_backupsage"))
            .arg("index")
            .arg(&src)
            .env("HOME", temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "status {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        let db = src.with_extension("db");
        assert_degraded(&db, "pipe", "skipped non-regular entry: fifo");
        assert_eq!(
            rows(
                &db,
                "SELECT value FROM meta WHERE key='directory_files_reused'"
            ),
            vec![vec![Value::Text(expected.into())]]
        );
        assert_eq!(
            rows(&db, "SELECT value FROM meta WHERE key='files_skipped'"),
            vec![vec![Value::Text("1".into())]]
        );
    }
}

#[test]
fn removed_between_walk_and_stat_is_recorded_and_never_reused() {
    struct RemoveOnEntry(PathBuf);
    impl ProgressObserver for RemoveOnEntry {
        fn on_event(&self, event: ProgressEvent<'_>) {
            if let ProgressEvent::Entry { path: "victim", .. } = event {
                fs::remove_file(&self.0).unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    let victim = src.join("victim");
    fs::write(&victim, "old token").unwrap();
    fs::write(src.join("z-healthy"), "still indexed").unwrap();
    let opts = IndexOptions::default();
    index(&src, &opts);
    for _ in 0..2 {
        fs::write(&victim, "old token").unwrap();
        let observer = RemoveOnEntry(victim.clone());
        let summary = indexer::run_index_with_control(
            &src,
            None,
            &opts,
            &OperationControl::new(&observer, CancellationToken::default()),
        )
        .unwrap();
        assert_eq!(summary.files_reused, 1); // only z-healthy
        assert_eq!(summary.files_skipped_binary, 1);
        assert_degraded(&summary.db_path, "victim", "cannot stat: No such file");
    }
    fs::write(&victim, "old token").unwrap();
    assert_eq!(index(&src, &opts).files_reused, 1);
    assert_eq!(index(&src, &opts).files_reused, 2);
}

#[test]
fn degraded_entry_does_not_mask_other_namespace_or_content_changes() {
    struct MutateAtPromotion(PathBuf);
    impl ProgressObserver for MutateAtPromotion {
        fn on_event(&self, event: ProgressEvent<'_>) {
            if let ProgressEvent::ReadyToPromote = event {
                fs::write(&self.0, "unobserved new content").unwrap();
            }
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let src = temp.path().join("source");
    fs::create_dir(&src).unwrap();
    assert!(Command::new("mkfifo")
        .arg(src.join("pipe"))
        .status()
        .unwrap()
        .success());
    fs::write(src.join("healthy"), "original content").unwrap();
    let opts = IndexOptions::default();
    let db = index(&src, &opts).db_path;
    let original = fs::read(&db).unwrap();
    for name in ["healthy", "added"] {
        let observer = MutateAtPromotion(src.join(name));
        let error = indexer::run_index_with_control(
            &src,
            None,
            &opts,
            &OperationControl::new(&observer, CancellationToken::default()),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unstable directory subtree"),
            "{error}"
        );
        assert_eq!(fs::read(&db).unwrap(), original);
    }
}
