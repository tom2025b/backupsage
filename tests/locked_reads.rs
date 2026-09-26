//! Every read of an index goes through the shared locked loader (#102):
//! `search`, `top`, `inspect`, federated search, master registration and
//! the re-index replace check. Nothing is written beside an index, each
//! command's reads form one snapshot, and every refused layout gets a named
//! reason. `tests/diff_cli.rs` covers the same loader for `diff`.

mod common;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::rc::Rc;

use backupsage::index_read::{self, LockedIndex, NoteCode, ReadPoint};
use backupsage::indexer::{self, IndexOptions};
use backupsage::master;
use backupsage::searcher;
use common::{build_tar, digest_of};

const WORD: &str = "lockword";

// ── Corpus and index layouts ────────────────────────────────────────────────

/// A tar source with enough rows for multi-page reads, indexed beside it.
fn indexed_source(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    fs::create_dir_all(dir).unwrap();
    let mut files: Vec<(String, Vec<u8>)> = (0..400)
        .map(|i| {
            (
                format!("bulk/file-{i:03}.txt"),
                format!("bulk row {i}").into_bytes(),
            )
        })
        .collect();
    files.push((
        "docs/note.txt".into(),
        format!("{WORD} alpha beta").into_bytes(),
    ));
    let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(p, d)| (p.as_str(), d.clone())).collect();
    let source = dir.join(name);
    fs::write(&source, build_tar(&refs)).unwrap();
    let db = indexer::run_index(&source, None, &IndexOptions::default())
        .unwrap()
        .db_path;
    (source, db)
}

fn sql(db: &Path, statements: &str) {
    rusqlite::Connection::open(db)
        .unwrap()
        .execute_batch(statements)
        .unwrap();
}

/// A complete index whose header says WAL mode, with no sidecar present.
fn make_wal_header(db: &Path) {
    sql(db, "PRAGMA journal_mode=WAL;");
}

/// Leave committed changes in `db-wal`, as an interrupted writer would.
fn make_pending_wal(db: &Path) {
    let dir = db.parent().unwrap();
    let work = dir.join("wal-work.db");
    fs::copy(db, &work).unwrap();
    let conn = rusqlite::Connection::open(&work).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         UPDATE meta SET value = 'moved-on' WHERE key = 'source';",
    )
    .unwrap();
    fs::copy(&work, db).unwrap();
    fs::copy(dir.join("wal-work.db-wal"), sidecar(db, "-wal")).unwrap();
    drop(conn);
    for leftover in ["wal-work.db", "wal-work.db-wal", "wal-work.db-shm"] {
        let _ = fs::remove_file(dir.join(leftover));
    }
}

/// Leave a hot `db-journal` beside a torn main file.
fn make_hot_journal(db: &Path) {
    let dir = db.parent().unwrap();
    let work = dir.join("hot-work.db");
    fs::copy(db, &work).unwrap();
    let conn = rusqlite::Connection::open(&work).unwrap();
    conn.execute_batch(
        "PRAGMA cache_size=1;
         BEGIN;
         INSERT INTO meta SELECT 'filler' || i, hex(randomblob(4000)) FROM
           (WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<100)
            SELECT i FROM r);",
    )
    .unwrap();
    fs::copy(&work, db).unwrap();
    fs::copy(dir.join("hot-work.db-journal"), sidecar(db, "-journal")).unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
    drop(conn);
    fs::remove_file(&work).unwrap();
}

/// Give the index a second hard link, in another directory.
fn make_multiply_linked(db: &Path, elsewhere: &Path) {
    fs::create_dir_all(elsewhere).unwrap();
    fs::hard_link(db, elsewhere.join("second-name.db")).unwrap();
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Clean,
    WalHeader,
    PendingWal,
    HotJournal,
    MultiplyLinked,
    Missing,
}

const REFUSED: [(Layout, &str); 5] = [
    (Layout::WalHeader, "wal_mode_index"),
    (Layout::PendingWal, "pending_journal"),
    (Layout::HotJournal, "pending_journal"),
    (Layout::MultiplyLinked, "index_multiply_linked"),
    (Layout::Missing, "index not found"),
];

/// A fresh index in `dir`, then shaped into `layout`.
fn layout_index(dir: &Path, layout: Layout) -> (PathBuf, PathBuf) {
    let (source, db) = indexed_source(dir, "src.tar");
    match layout {
        Layout::Clean => {}
        Layout::WalHeader => make_wal_header(&db),
        Layout::PendingWal => make_pending_wal(&db),
        Layout::HotJournal => make_hot_journal(&db),
        Layout::MultiplyLinked => make_multiply_linked(&db, &dir.join("other")),
        Layout::Missing => fs::remove_file(&db).unwrap(),
    }
    (source, db)
}

// ── Observing a directory ───────────────────────────────────────────────────

type TreeState = BTreeMap<String, (u64, String, u64, u64, i64, i64, i64, i64)>;

/// Size, bytes, inode, links, mtime and ctime of every file in `dir`, and of
/// `dir` itself (".").
fn tree_state(dir: &Path) -> TreeState {
    let observe = |p: &Path, digest: String| {
        let md = fs::symlink_metadata(p).unwrap();
        (
            if md.is_file() { md.len() } else { 0 },
            digest,
            md.ino(),
            md.nlink(),
            md.mtime(),
            md.mtime_nsec(),
            md.ctime(),
            md.ctime_nsec(),
        )
    };
    let mut state: TreeState = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .map(|p| {
            let name = String::from_utf8_lossy(p.file_name().unwrap().as_bytes()).into_owned();
            (name, observe(&p, digest_of(&p)))
        })
        .collect();
    state.insert(".".into(), observe(dir, String::new()));
    state
}

fn assert_same_tree(expected: &TreeState, actual: &TreeState, context: &str) {
    let changed: Vec<_> = expected
        .keys()
        .chain(actual.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|name| expected.get(*name) != actual.get(*name))
        .map(|name| match (expected.get(name), actual.get(name)) {
            (Some(_), Some(_)) => format!("changed {name}"),
            (None, _) => format!("created {name}"),
            (_, None) => format!("removed {name}"),
        })
        .collect();
    assert!(changed.is_empty(), "{context}: {changed:?}");
}

fn run(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .args(args)
        .output()
        .expect("binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ── Read-only commands never touch an index's directory ─────────────────────

/// `search`, `top` and `inspect` against every layout: a clean index works;
/// every other layout fails closed with its named reason; the directory is
/// byte-for-byte and inode-for-inode unchanged either way.
#[test]
fn read_commands_write_nothing_and_name_every_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cases = vec![(Layout::Clean, "")];
    cases.extend(REFUSED);
    for (layout, reason) in cases {
        let dir = tmp.path().join(format!("{layout:?}"));
        let (_, db) = layout_index(&dir, layout);
        let state = tree_state(&dir);
        for args in [
            vec!["search", WORD, "--index"],
            vec!["search", WORD, "--json", "--index"],
            vec!["top", "--index"],
            vec!["inspect", "docs/note.txt", "--index"],
        ] {
            let mut argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            argv.push(db.as_os_str());
            let out = run(&argv);
            let what = format!("{layout:?} {args:?}");
            if matches!(layout, Layout::Clean) {
                assert_eq!(out.status.code(), Some(0), "{what}: {}", stderr(&out));
            } else {
                assert_eq!(out.status.code(), Some(1), "{what}");
                assert!(out.stdout.is_empty(), "{what}: printed results");
                assert!(stderr(&out).contains(reason), "{what}: {}", stderr(&out));
            }
            assert_same_tree(&state, &tree_state(&dir), &what);
        }
        if matches!(layout, Layout::Missing) {
            assert!(!db.exists(), "a read created the missing index");
        }
    }
}

/// Federated search lists each refused archive as skipped with its reason
/// and still returns the others; no index directory changes.
#[test]
fn federated_search_skips_refused_archives_with_their_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let idx = tmp.path().join("idx");
    let master_path = tmp.path().join("m").join("master.db");
    fs::create_dir_all(master_path.parent().unwrap()).unwrap();
    let names = ["a-clean", "b-wal", "c-pending", "d-hot", "e-linked"];
    let mut m = master::open_at(&master_path).unwrap();
    let mut dbs = Vec::new();
    for name in names {
        let (_, db) = indexed_source(&idx.join(name), &format!("{name}.tar"));
        m.add(&db).unwrap();
        dbs.push(db);
    }
    drop(m);
    // Registered healthy; now reshaped on disk.
    make_wal_header(&dbs[1]);
    make_pending_wal(&dbs[2]);
    make_hot_journal(&dbs[3]);
    make_multiply_linked(&dbs[4], &tmp.path().join("elsewhere"));
    let states: Vec<_> = names.iter().map(|n| tree_state(&idx.join(n))).collect();

    let out = run(&[
        OsStr::new("--master"),
        master_path.as_os_str(),
        OsStr::new("search"),
        OsStr::new(WORD),
        OsStr::new("--all"),
        OsStr::new("--json"),
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let searched: Vec<_> = doc["archives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["archive"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(searched, ["a-clean.tar"]);
    let skipped: BTreeMap<String, String> = doc["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["archive"].as_str().unwrap().to_owned(),
                s["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for (archive, code) in [
        ("b-wal.tar", "wal_mode_index"),
        ("c-pending.tar", "pending_journal"),
        ("d-hot.tar", "pending_journal"),
        ("e-linked.tar", "index_multiply_linked"),
    ] {
        let reason = skipped
            .get(archive)
            .unwrap_or_else(|| panic!("{archive} not skipped"));
        assert!(
            reason.starts_with(&format!("refused: {code}")),
            "{archive}: {reason}"
        );
    }
    for (name, state) in names.iter().zip(&states) {
        assert_same_tree(state, &tree_state(&idx.join(name)), name);
    }
}

/// Re-indexing over an existing index it cannot read safely never replaces
/// it. With `--index` the command fails; by default the indexer's existing
/// fallback builds `./<name>.db` in the working directory instead, and its
/// warning names the reason.
#[test]
fn reindex_never_replaces_an_index_it_cannot_read_safely() {
    let tmp = tempfile::tempdir().unwrap();
    for (layout, code) in REFUSED
        .iter()
        .filter(|(l, _)| !matches!(l, Layout::Missing))
    {
        let dir = tmp.path().join(format!("{layout:?}"));
        let cwd = tmp.path().join(format!("{layout:?}-cwd"));
        fs::create_dir_all(&cwd).unwrap();
        let (source, db) = layout_index(&dir, *layout);
        let state = tree_state(&dir);

        let explicit = Command::new(env!("CARGO_BIN_EXE_backupsage"))
            .current_dir(&cwd)
            .args([
                OsStr::new("index"),
                OsStr::new("--index"),
                db.as_os_str(),
                source.as_os_str(),
            ])
            .output()
            .unwrap();
        assert_eq!(explicit.status.code(), Some(1), "{layout:?}");
        let err = stderr(&explicit);
        assert!(
            err.contains("refusing to replace") && err.contains(code),
            "{layout:?}: {err}"
        );
        assert_same_tree(&state, &tree_state(&dir), &format!("{layout:?} --index"));
        assert_eq!(
            fs::read_dir(&cwd).unwrap().count(),
            0,
            "{layout:?}: wrote to cwd"
        );

        let default = Command::new(env!("CARGO_BIN_EXE_backupsage"))
            .current_dir(&cwd)
            .args([OsStr::new("index"), source.as_os_str()])
            .output()
            .unwrap();
        assert_eq!(
            default.status.code(),
            Some(0),
            "{layout:?}: {}",
            stderr(&default)
        );
        assert!(
            stderr(&default).contains(code),
            "{layout:?}: {}",
            stderr(&default)
        );
        assert_same_tree(&state, &tree_state(&dir), &format!("{layout:?} default"));
        assert!(
            cwd.join("src.tar.db").exists(),
            "{layout:?}: no fallback index"
        );
    }
    // A clean index of the same source is still replaced in place.
    let dir = tmp.path().join("clean");
    let (source, db) = layout_index(&dir, Layout::Clean);
    let cwd = tmp.path().join("clean-cwd");
    fs::create_dir_all(&cwd).unwrap();
    let before = digest_of(&db);
    let out = Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .current_dir(&cwd)
        .args([OsStr::new("index"), source.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_ne!(digest_of(&db), before, "the clean index was not rebuilt");
    assert_eq!(fs::read_dir(&cwd).unwrap().count(), 0, "fell back to cwd");
}

// ── Consistency: a writer never lands mid-read ──────────────────────────────

const BUMP: &str = "UPDATE files SET mtime_unix = mtime_unix + 1;
                    UPDATE meta SET value = 'rewritten' WHERE key = 'index_uuid';";

/// Arm a hook that tries to commit `BUMP` through SQLite and records
/// whether the commit went through.
fn sqlite_writer_at(point: ReadPoint, db: &Path) -> Rc<RefCell<Option<Result<(), String>>>> {
    let outcome = Rc::new(RefCell::new(None));
    let (db, seen) = (db.to_path_buf(), outcome.clone());
    index_read::set_mid_read_hook(point, move || {
        let writer = rusqlite::Connection::open(&db).unwrap();
        writer.busy_timeout(std::time::Duration::ZERO).unwrap();
        let result = writer.execute_batch(&format!("BEGIN IMMEDIATE; {BUMP} COMMIT;"));
        let _ = writer.execute_batch("ROLLBACK");
        *seen.borrow_mut() = Some(result.map_err(|e| e.to_string()));
    });
    outcome
}

/// Arm a hook that rewrites the file in place, bypassing SQLite locking.
fn raw_writer_at(point: ReadPoint, db: &Path) {
    let bytes = fs::read(db).unwrap();
    let db = db.to_path_buf();
    index_read::set_mid_read_hook(point, move || {
        let mut file = fs::OpenOptions::new().write(true).open(&db).unwrap();
        std::io::Write::write_all(&mut file, &bytes).unwrap();
    });
}

fn mtimes(conn: &rusqlite::Connection, sql: &str) -> BTreeSet<i64> {
    let mut stmt = conn.prepare(sql).unwrap();
    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
    rows.map(Result::unwrap).collect()
}

#[test]
fn search_and_top_read_one_snapshot_under_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, db) = indexed_source(tmp.path(), "a.tar");
    let writer = sqlite_writer_at(ReadPoint::AfterOpen, &db);
    let conn = searcher::open_index(&db).unwrap();
    let hits = searcher::search(&conn, WORD, 10, false).unwrap().hits;
    let words = searcher::top_words(&conn, 5).unwrap();
    let seen = mtimes(&conn, "SELECT DISTINCT mtime_unix FROM files");
    searcher::finish_index(conn, &db).unwrap();
    let writer = writer.borrow_mut().take().expect("the hook ran");
    assert!(
        writer.is_err(),
        "a writer committed while search held the index"
    );
    assert_eq!(hits.len(), 1);
    assert!(!words.is_empty());
    assert_eq!(seen.len(), 1, "rows from two states: {seen:?}");
}

#[test]
fn a_file_changed_mid_read_is_never_presented() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, db) = indexed_source(tmp.path(), "a.tar");
    raw_writer_at(ReadPoint::AfterOpen, &db);
    let conn = searcher::open_index(&db).unwrap();
    let _hits = searcher::search(&conn, WORD, 10, false).unwrap();
    let err = searcher::finish_index(conn, &db).unwrap_err();
    assert!(
        format!("{err:#}").contains("index_changed_during_read"),
        "{err:#}"
    );
}

#[test]
fn federated_search_drops_an_archive_changed_mid_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut m = master::open_at(&tmp.path().join("master.db")).unwrap();
    for name in ["a", "b"] {
        let (_, db) = indexed_source(&tmp.path().join(name), &format!("{name}.tar"));
        m.add(&db).unwrap();
    }
    // The first archive searched is rewritten in place while it is read.
    let first = m.list().unwrap()[0].clone();
    raw_writer_at(ReadPoint::AfterOpen, Path::new(&first.db_path));
    let outcome = searcher::search_all(&m, WORD, 10, false).unwrap();
    let searched: Vec<_> = outcome
        .per_archive
        .iter()
        .map(|a| a.archive_label.clone())
        .collect();
    assert!(
        !searched.contains(&first.label),
        "a torn read was presented"
    );
    assert_eq!(searched.len(), 1);
    let (label, reason) = &outcome.skipped[0];
    assert_eq!(label, &first.label);
    assert!(
        reason.starts_with("refused: index_changed_during_read"),
        "{reason}"
    );
}

/// `master add` reads the identity, then replicates rows through a second
/// connection. The identity read's lock must still be held then, so both
/// come from one snapshot.
#[test]
fn master_registration_replicates_the_snapshot_it_identified() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, db) = indexed_source(tmp.path(), "a.tar");
    let master_path = tmp.path().join("master.db");
    let writer = sqlite_writer_at(ReadPoint::HeldByCaller, &db);
    let mut m = master::open_at(&master_path).unwrap();
    m.add(&db).unwrap();
    let uuid = m.list().unwrap()[0].clone();
    drop(m);
    let writer = writer.borrow_mut().take().expect("the hook ran");
    assert!(
        writer.is_err(),
        "a writer committed between identity and replication"
    );

    let conn = rusqlite::Connection::open(&master_path).unwrap();
    let replicated = mtimes(&conn, "SELECT DISTINCT mtime_unix FROM files");
    assert_eq!(
        replicated,
        BTreeSet::from([1_700_000_001]),
        "replicated rows moved on"
    );
    let registered: String = conn
        .query_row("SELECT index_uuid FROM archives", [], |r| r.get(0))
        .unwrap();
    assert_ne!(registered, "rewritten");
    assert_eq!(registered, uuid.index_uuid);
}

#[test]
fn replace_check_refuses_an_index_changed_while_it_was_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let (source, db) = indexed_source(tmp.path(), "a.tar");
    let before = digest_of(&db);
    raw_writer_at(ReadPoint::AfterOpen, &db);
    // Explicit destination: no working-directory fallback in-process.
    let err = indexer::run_index(&source, Some(&db), &IndexOptions::default()).unwrap_err();
    assert!(
        format!("{err:#}").contains("refusing to replace"),
        "{err:#}"
    );
    assert!(
        format!("{err:#}").contains("index_changed_during_read"),
        "{err:#}"
    );
    // The raw writer put identical bytes back; the index was not rebuilt.
    assert_eq!(digest_of(&db), before);
}

#[test]
fn locked_open_refuses_each_layout_with_its_code() {
    let tmp = tempfile::tempdir().unwrap();
    for (layout, code) in [
        (Layout::WalHeader, NoteCode::WalModeIndex),
        (Layout::PendingWal, NoteCode::PendingJournal),
        (Layout::HotJournal, NoteCode::PendingJournal),
        (Layout::MultiplyLinked, NoteCode::IndexMultiplyLinked),
        (Layout::Missing, NoteCode::IndexMissing),
    ] {
        let dir = tmp.path().join(format!("{layout:?}"));
        let (_, db) = layout_index(&dir, layout);
        let state = tree_state(&dir);
        let refused = LockedIndex::open(&db).unwrap_err();
        assert_eq!(refused.code, code, "{layout:?}");
        assert_same_tree(&state, &tree_state(&dir), &format!("{layout:?}"));
    }
    // Every code prints under its JSON name.
    for code in [
        NoteCode::WalModeIndex,
        NoteCode::PendingJournal,
        NoteCode::IndexMultiplyLinked,
        NoteCode::IndexBusy,
        NoteCode::IndexChangedDuringRead,
        NoteCode::SourceDirectoryUnverified,
    ] {
        assert_eq!(
            serde_json::to_value(code).unwrap().as_str(),
            Some(code.as_str())
        );
    }
}

// ── #104 review, repair cycle 1 ─────────────────────────────────────────────

/// `dedup --db` and `master add` resolve and register an index through the
/// locked loader: nothing is written beside a refused layout, and each
/// refusal is named.
#[test]
fn dedup_db_and_master_add_write_nothing_and_name_every_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cases = vec![(Layout::Clean, "")];
    cases.extend(
        REFUSED
            .iter()
            .filter(|(l, _)| !matches!(l, Layout::Missing)),
    );
    for (layout, reason) in cases {
        let dir = tmp.path().join(format!("{layout:?}"));
        let (_, db) = layout_index(&dir, layout);
        let master_path = tmp
            .path()
            .join(format!("{layout:?}-master"))
            .join("master.db");
        fs::create_dir_all(master_path.parent().unwrap()).unwrap();
        let state = tree_state(&dir);
        for argv in [
            vec![OsStr::new("dedup"), OsStr::new("--db"), db.as_os_str()],
            vec![
                OsStr::new("--master"),
                master_path.as_os_str(),
                OsStr::new("master"),
                OsStr::new("add"),
                db.as_os_str(),
            ],
        ] {
            let out = run(&argv);
            let what = format!("{layout:?} {argv:?}");
            if matches!(layout, Layout::Clean) {
                assert_eq!(out.status.code(), Some(0), "{what}: {}", stderr(&out));
            } else {
                assert_eq!(out.status.code(), Some(1), "{what}");
                assert!(stderr(&out).contains(reason), "{what}: {}", stderr(&out));
            }
            assert_same_tree(&state, &tree_state(&dir), &what);
        }
    }
}

/// The master catalogue as a test sees it: every archive's uuid, and every
/// replicated row's mtime.
fn catalogue(master_path: &Path) -> (Vec<String>, BTreeSet<i64>) {
    let conn = rusqlite::Connection::open(master_path).unwrap();
    let mut stmt = conn.prepare("SELECT index_uuid FROM archives").unwrap();
    let uuids = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    (
        uuids,
        mtimes(&conn, "SELECT DISTINCT mtime_unix FROM files"),
    )
}

/// A file renamed over the index path between the identity read and the
/// row copy must not lend its rows to the other file's identity. The whole
/// registration is abandoned and nothing is recorded.
#[test]
fn registration_is_abandoned_when_another_file_is_renamed_over_the_path() {
    let tmp = tempfile::tempdir().unwrap();
    for same_uuid in [false, true] {
        let dir = tmp.path().join(format!("same-uuid-{same_uuid}"));
        let (source, db) = indexed_source(&dir, "a.tar");
        // B: a rebuild of the same source (new uuid), or a copy of A with
        // changed rows and A's own uuid.
        let replacement = dir.join("b.db");
        if same_uuid {
            fs::copy(&db, &replacement).unwrap();
            sql(&replacement, BUMP_ROWS_ONLY);
        } else {
            indexer::run_index(&source, Some(&replacement), &IndexOptions::default()).unwrap();
            sql(&replacement, BUMP_ROWS_ONLY);
        }
        let master_path = dir.join("master.db");
        let (from, to) = (replacement.clone(), db.clone());
        index_read::set_mid_read_hook(ReadPoint::HeldByCaller, move || {
            fs::rename(&from, &to).unwrap();
        });
        let mut m = master::open_at(&master_path).unwrap();
        let err = m.add(&db).unwrap_err();
        drop(m);
        let err = format!("{err:#}");
        assert!(
            err.contains("index_changed_during_read") && err.contains("nothing was recorded"),
            "same uuid {same_uuid}: {err}"
        );
        assert_eq!(
            catalogue(&master_path),
            (Vec::new(), BTreeSet::new()),
            "same uuid {same_uuid}: something was recorded"
        );
    }
}

const BUMP_ROWS_ONLY: &str = "UPDATE files SET mtime_unix = mtime_unix + 1;";

/// The source's read lock is held from the moment `read_identity` returns
/// until the rows are copied, not only while the identity is read: a writer
/// is refused before replication's first statement on the handle and again
/// just before the rows are read.
#[test]
fn source_lock_is_held_while_rows_are_replicated() {
    for point in [ReadPoint::BeforeReplication, ReadPoint::DuringReplication] {
        let tmp = tempfile::tempdir().unwrap();
        let (_, db) = indexed_source(tmp.path(), "a.tar");
        let master_path = tmp.path().join("master.db");
        let writer = sqlite_writer_at(point, &db);
        let mut m = master::open_at(&master_path).unwrap();
        let added = m.add(&db);
        drop(m);
        let writer = writer.borrow_mut().take().expect("the hook ran");
        assert!(
            writer.is_err(),
            "{point:?}: a writer committed while the source was held for replication"
        );
        added.unwrap();
        let (uuids, rows) = catalogue(&master_path);
        assert_eq!(uuids.len(), 1, "{point:?}");
        assert_ne!(uuids[0], "rewritten", "{point:?}");
        assert_eq!(rows, BTreeSet::from([1_700_000_001]), "{point:?}");
    }
}

/// Through the real commands (a debug-build seam rewrites each index in
/// place right after its lock is taken): no command presents a read whose
/// file changed; each says why.
#[cfg(debug_assertions)]
#[test]
fn commands_never_present_a_read_whose_file_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, db) = indexed_source(&tmp.path().join("idx"), "a.tar");
    let master_path = tmp.path().join("m").join("master.db");
    fs::create_dir_all(master_path.parent().unwrap()).unwrap();
    let touched = |argv: &[&OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_backupsage"))
            .env("BACKUPSAGE_TEST_TOUCH_INDEX_AFTER_OPEN", "1")
            .args(argv)
            .output()
            .unwrap()
    };
    for args in [
        vec!["search", WORD, "--index"],
        vec!["search", WORD, "--json", "--index"],
        vec!["top", "--index"],
        vec!["inspect", "docs/note.txt", "--index"],
        vec!["dedup", "--db"],
    ] {
        let mut argv: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        argv.push(db.as_os_str());
        let out = touched(&argv);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}: presented results");
        assert!(
            stderr(&out).contains("index_changed_during_read"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    let out = touched(&[
        OsStr::new("--master"),
        master_path.as_os_str(),
        OsStr::new("master"),
        OsStr::new("add"),
        db.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("index_changed_during_read"),
        "{}",
        stderr(&out)
    );
    assert_eq!(catalogue(&master_path), (Vec::new(), BTreeSet::new()));
}

/// Federated search decides nothing from an archive's read, its
/// metadata-only or incomplete notes included, before that read is proven
/// coherent.
#[cfg(debug_assertions)]
#[test]
fn federated_notes_come_only_from_coherent_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let master_path = tmp.path().join("m").join("master.db");
    fs::create_dir_all(master_path.parent().unwrap()).unwrap();
    let dir = tmp.path().join("meta");
    fs::create_dir_all(&dir).unwrap();
    let source = dir.join("meta.tar");
    fs::write(
        &source,
        build_tar(&[("docs/note.txt", WORD.as_bytes().to_vec())]),
    )
    .unwrap();
    let opts = IndexOptions {
        mode: indexer::ContentMode::MetadataOnly,
        ..IndexOptions::default()
    };
    let db = indexer::run_index(&source, None, &opts).unwrap().db_path;
    let mut m = master::open_at(&master_path).unwrap();
    m.add(&db).unwrap();
    drop(m);

    let search_all = |touch: bool| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_backupsage"));
        if touch {
            cmd.env("BACKUPSAGE_TEST_TOUCH_INDEX_AFTER_OPEN", "1");
        }
        let out = cmd
            .args([OsStr::new("--master"), master_path.as_os_str()])
            .args(["search", WORD, "--all", "--json"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
        let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        doc["skipped"][0]["reason"].as_str().unwrap().to_owned()
    };
    assert!(search_all(false).starts_with("metadata-only"));
    let reason = search_all(true);
    assert!(
        reason.starts_with("refused: index_changed_during_read"),
        "{reason}"
    );
}

/// Discovery by the working directory never writes beside a candidate, and
/// a lone refused candidate is named with its reason, not "no index found".
#[test]
fn discovery_names_refused_candidates_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    let (_, db) = indexed_source(&tmp.path().join("build"), "a.tar");
    fs::copy(&db, cwd.join("archive.db")).unwrap();
    fs::write(cwd.join("notes.db"), b"not sqlite at all").unwrap();
    let discover = || {
        Command::new(env!("CARGO_BIN_EXE_backupsage"))
            .current_dir(&cwd)
            .args(["search", WORD])
            .output()
            .unwrap()
    };
    let out = discover();
    assert_eq!(
        out.status.code(),
        Some(0),
        "clean discovery: {}",
        stderr(&out)
    );

    make_wal_header(&cwd.join("archive.db"));
    let state = tree_state(&cwd);
    let out = discover();
    assert_eq!(out.status.code(), Some(1));
    let err = stderr(&out);
    assert!(
        err.contains("archive.db") && err.contains("wal_mode_index"),
        "{err}"
    );
    assert!(!err.contains("notes.db"), "a non-index was listed: {err}");
    assert_same_tree(&state, &tree_state(&cwd), "discovery");
}

/// A writer holding the index exclusively makes the read fail closed as
/// busy, through a command and in the library, without writing anything.
///
/// POSIX record locks belong to a process, and closing any descriptor on a
/// file drops them all, so while the writer holds its lock this test process
/// must not open and close the index itself (no stamping, no library open)
/// before the command has run.
#[test]
fn busy_index_is_refused_with_its_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, db) = indexed_source(tmp.path(), "a.tar");
    let state = tree_state(tmp.path());

    let writer = rusqlite::Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let out = run(&[
        OsStr::new("search"),
        OsStr::new(WORD),
        OsStr::new("--index"),
        db.as_os_str(),
    ]);
    // In-process, SQLite's own lock table answers before any descriptor is
    // closed, so the library refusal is checked while the lock is still held.
    let in_process = LockedIndex::open(&db).map(|_| ()).unwrap_err().code;
    writer.execute_batch("ROLLBACK;").unwrap();
    drop(writer);

    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("index_busy"), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(in_process, NoteCode::IndexBusy);
    assert_same_tree(&state, &tree_state(tmp.path()), "busy");
}
