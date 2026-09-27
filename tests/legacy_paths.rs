//! Legacy indexes without raw-path columns (#105): a pre-v1.0.1 index loads
//! through the shared locked loader instead of being refused; its UTF-8
//! names map byte-exact; a name stored only as a lossy rendering is flagged
//! and never treated as exact, by `diff` or by coverage; and the loader's
//! guards hold for the legacy layout as they do for the current one.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use backupsage::coverage::{Presence, ReplicaCount, UnknownContentReason, UnknownReason};
use backupsage::coverage_input::{build, LoadCode, LoadedCoverage, RegistrySource};
use backupsage::diff::{self, ChangeKind, Entry, Reason, SnapshotState};
use backupsage::diff_input::{self, LoadedIndex, NoteCode, ReadPoint};
use backupsage::indexer::{self, IndexOptions};
use backupsage::store::flags;

const MTIME: u64 = 1_700_000_001;
const PLAIN: &[u8] = b"plain";
const CAFE: &[u8] = b"cafe bytes, 16B";
const FIRST: &[u8] = b"first lossy name holds this, 32B";
const SECOND: &[u8] = b"second lossy name holds these bytes, 48 bytes..";
const MOVED: &[u8] = b"content that a rename would carry along, 50 bytes.";

/// Removes what v1.0.1 added, leaving the layout older indexes have.
const LEGACY: &str = "ALTER TABLE files DROP COLUMN path_raw;
                      ALTER TABLE files DROP COLUMN link_target_raw;
                      DELETE FROM meta WHERE key = 'path_raw';";

const CAFE_NAME: &[u8] = "café.txt".as_bytes();
const LOSSY_NAME: &[u8] = "d\u{fffd}".as_bytes();

enum Member<'a> {
    File(&'a [u8], &'a [u8]),
    Symlink(&'a [u8], &'a [u8]),
}

/// A tar written header by header, so names and targets may be non-UTF-8.
fn tar(dir: &Path, name: &str, members: &[Member]) -> PathBuf {
    let mut bytes = Vec::new();
    for member in members {
        let mut h = tar::Header::new_gnu();
        let data: &[u8] = match *member {
            Member::File(path, data) => {
                h.set_path(OsStr::from_bytes(path)).unwrap();
                data
            }
            Member::Symlink(path, target) => {
                h.set_path(OsStr::from_bytes(path)).unwrap();
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_link_name(OsStr::from_bytes(target)).unwrap();
                b""
            }
        };
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(MTIME);
        h.set_cksum();
        bytes.extend_from_slice(h.as_bytes());
        bytes.extend_from_slice(data);
        while !bytes.len().is_multiple_of(512) {
            bytes.push(0);
        }
    }
    bytes.extend_from_slice(&[0u8; 1024]);
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

fn index(source: &Path) -> PathBuf {
    indexer::run_index(source, None, &IndexOptions::default())
        .unwrap()
        .db_path
}

fn altered_copy(db: &Path, name: &str, sql: &str) -> PathBuf {
    let copy = db.with_file_name(name);
    fs::copy(db, &copy).unwrap();
    rusqlite::Connection::open(&copy)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    copy
}

/// The mixed source: two UTF-8 names, two non-UTF-8 names that render the
/// same, and a symlink whose target is non-UTF-8. Returns the current index
/// and a legacy copy of it.
fn mixed(dir: &Path) -> (PathBuf, PathBuf) {
    let source = tar(
        dir,
        "mixed.tar",
        &[
            Member::File(b"plain.txt", PLAIN),
            Member::File(CAFE_NAME, CAFE),
            Member::File(b"d\xff", FIRST),
            Member::File(b"d\xfe", SECOND),
            Member::Symlink(b"link", b"t\xff"),
        ],
    );
    let current = index(&source);
    let legacy = altered_copy(&current, "legacy.db", LEGACY);
    (current, legacy)
}

fn entry<'a>(loaded: &'a LoadedIndex, path: &[u8]) -> &'a Entry {
    loaded
        .snapshot
        .entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("no row {:?}", String::from_utf8_lossy(path)))
}

fn codes(loaded: &LoadedIndex) -> Vec<NoteCode> {
    loaded.health.notes.iter().map(|n| n.code).collect()
}

const LOSSY: i64 = flags::LOSSY_PATH | flags::LOSSY_LINK_TARGET;

// ── The legacy layout loads, UTF-8 names exactly ────────────────────────────

#[test]
fn legacy_index_loads_through_the_shared_loader() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, legacy) = mixed(tmp.path());
    let loaded = diff_input::load_index(&legacy);
    assert_eq!(
        loaded.snapshot.info.state,
        SnapshotState::Complete,
        "{:?}",
        loaded.health.notes
    );
    assert_eq!(loaded.snapshot.entries.len(), 5);
    let lossy = loaded
        .health
        .notes
        .iter()
        .find(|n| n.code == NoteCode::LegacyLossyPaths)
        .expect("the lossy rows are named");
    assert!(lossy.detail.starts_with("3 row(s)"), "{}", lossy.detail);
}

#[test]
fn utf8_legacy_rows_map_byte_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, legacy) = mixed(tmp.path());
    let now = diff_input::load_index(&current);
    let old = diff_input::load_index(&legacy);
    for name in [&b"plain.txt"[..], CAFE_NAME] {
        let (a, b) = (entry(&now, name), entry(&old, name));
        assert_eq!(b.path, name, "exact bytes");
        assert_eq!(b.flags & LOSSY, 0, "an exact UTF-8 name is flagged lossy");
        assert_eq!(
            format!("{a:?}"),
            format!("{b:?}"),
            "legacy row differs from the current one"
        );
    }
    // The current layout never flags anything.
    assert!(now.snapshot.entries.iter().all(|e| e.flags & LOSSY == 0));
    assert!(!codes(&now).contains(&NoteCode::LegacyLossyPaths));
}

#[test]
fn lossy_legacy_rows_are_flagged_never_exact() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, legacy) = mixed(tmp.path());
    let old = diff_input::load_index(&legacy);
    let lossy: Vec<&Entry> = old
        .snapshot
        .entries
        .iter()
        .filter(|e| e.path == LOSSY_NAME)
        .collect();
    assert_eq!(lossy.len(), 2, "both non-UTF-8 names render the same");
    for e in &lossy {
        assert_ne!(e.flags & flags::LOSSY_PATH, 0, "lossy name not flagged");
    }
    let link = entry(&old, b"link");
    assert_eq!(
        link.flags & flags::LOSSY_PATH,
        0,
        "the link's own name is exact"
    );
    assert_ne!(
        link.flags & flags::LOSSY_LINK_TARGET,
        0,
        "lossy link target not flagged"
    );
    // The current index kept the exact bytes, and flags nothing.
    let now = diff_input::load_index(&current);
    assert_eq!(entry(&now, b"d\xff").flags & LOSSY, 0);
    assert_eq!(
        entry(&now, b"link").link_target.as_deref(),
        Some(&b"t\xff"[..])
    );
}

// ── Coverage: a lossy row is inconclusive, never a trusted copy ─────────────

fn registry(entries: &[(i64, &str, &Path)]) -> Vec<RegistrySource> {
    entries
        .iter()
        .map(|&(id, label, db)| RegistrySource {
            source_id: id,
            label: label.into(),
            db_path: db.to_path_buf(),
            registry_status: Some("ok".into()),
        })
        .collect()
}

fn presence(loaded: &LoadedCoverage, content: &[u8], id: i64) -> (Presence, ReplicaCount) {
    let cov = loaded.coverage().unwrap();
    let hash = *blake3::hash(content).as_bytes();
    let group = cov
        .groups
        .iter()
        .find(|g| g.content_hash == hash)
        .expect("group exists");
    let p = group
        .presence
        .iter()
        .find(|p| p.source_id == id)
        .unwrap()
        .presence;
    (p, group.replicas)
}

#[test]
fn coverage_counts_exact_legacy_rows_and_never_a_lossy_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, legacy) = mixed(tmp.path());
    let other = tar(
        tmp.path(),
        "other.tar",
        &[
            Member::File(b"copy-of-plain", PLAIN),
            Member::File(b"copy-of-cafe", CAFE),
            Member::File(b"copy-of-first", FIRST),
            Member::File(b"copy-of-second", SECOND),
        ],
    );
    let other = index(&other);
    let loaded = build(&registry(&[(1, "legacy", &legacy), (2, "other", &other)])).unwrap();
    let s = &loaded.sources[0];
    assert!(s
        .index_notes
        .iter()
        .any(|n| n.code == NoteCode::LegacyLossyPaths));

    // Exact UTF-8 legacy rows are trusted copies.
    for content in [PLAIN, CAFE] {
        assert_eq!(
            presence(&loaded, content, 1),
            (Presence::Present { copies: 1 }, ReplicaCount::Exact(2))
        );
    }
    // The legacy source holds FIRST and SECOND under lossy names that render
    // the same: neither is a copy, and neither is ruled out either.
    for content in [FIRST, SECOND] {
        let (p, replicas) = presence(&loaded, content, 1);
        assert!(
            matches!(p, Presence::Unknown(_)),
            "a lossy row was read as {p:?}"
        );
        assert_eq!(replicas, ReplicaCount::AtLeast(1));
    }

    let cov = loaded.coverage().unwrap();
    let lossy: Vec<_> = cov
        .unknown_content
        .iter()
        .filter(|u| u.row.source_id == 1)
        .collect();
    assert_eq!(lossy.len(), 2, "{:?}", cov.unknown_content);
    assert!(lossy
        .iter()
        .all(|u| u.reason == UnknownContentReason::LegacyNameUncertain));
    assert!(
        cov.exclusions.iter().all(|x| x.row.path_raw != LOSSY_NAME),
        "a lossy row was shadowed: {:?}",
        cov.exclusions
    );
}

#[test]
fn unknown_entry_type_in_a_legacy_index_is_still_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, legacy) = mixed(tmp.path());
    let odd = altered_copy(
        &legacy,
        "odd.db",
        "INSERT INTO files (path, entry_type, kind, size, flags)
         VALUES ('dev/null', 'chardev', 'binary', 0, 0);",
    );
    let loaded = build(&registry(&[(1, "odd", &odd)])).unwrap();
    let s = &loaded.sources[0];
    assert!(s.rows.is_empty());
    assert_eq!(s.notes[0].code, LoadCode::UnknownEntryType);
}

// ── diff: a lossy row is inconclusive, and so is what it could be ───────────

fn changes(report: &diff::DiffReport, kind: ChangeKind) -> Vec<&diff::Change> {
    report.changes.iter().filter(|c| c.kind == kind).collect()
}

fn lossy_reasons(report: &diff::DiffReport) -> usize {
    report
        .changes
        .iter()
        .filter(|c| c.kind == ChangeKind::Inconclusive && c.reason == Reason::LegacyNameUncertain)
        .count()
}

#[test]
fn diff_never_claims_a_change_or_absence_through_a_lossy_name() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, legacy) = mixed(tmp.path());
    let old = diff_input::load_index(&legacy);
    let now = diff_input::load_index(&current);

    // Legacy before, current after: the exact non-UTF-8 names could be the
    // lossy ones, so nothing is added, removed or moved.
    let report = diff::compare(&old.snapshot, &now.snapshot).unwrap();
    assert_eq!(report.summary.added, 0, "{:#?}", report.changes);
    assert_eq!(report.summary.removed, 0, "{:#?}", report.changes);
    assert_eq!(report.summary.moved, 0);
    assert_eq!(report.summary.byte_identical, 2, "plain.txt and café.txt");
    assert_eq!(lossy_reasons(&report), 4, "{:#?}", report.changes);
    assert!(report.excluded.is_empty(), "{:#?}", report.excluded);
    assert_eq!(report.comparison_state, SnapshotState::Incomplete);

    // And the other way round.
    let report = diff::compare(&now.snapshot, &old.snapshot).unwrap();
    assert_eq!(report.summary.added + report.summary.removed, 0);
    assert_eq!(lossy_reasons(&report), 4);

    // Legacy against itself: two lossy rows are never paired as identical.
    let report = diff::compare(&old.snapshot, &old.snapshot).unwrap();
    assert_eq!(report.summary.byte_identical, 2);
    assert_eq!(lossy_reasons(&report), 4);
    assert!(report.excluded.is_empty());
}

#[test]
fn diff_never_moves_onto_a_path_a_lossy_name_could_be() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tar(
        tmp.path(),
        "before.tar",
        &[
            Member::File(b"d\xff", FIRST),
            Member::File(b"old.txt", MOVED),
        ],
    );
    let before = altered_copy(&index(&before), "before-legacy.db", LEGACY);
    let after = tar(tmp.path(), "after.tar", &[Member::File(b"d\xff", MOVED)]);
    let after = index(&after);
    let (old, now) = (
        diff_input::load_index(&before),
        diff_input::load_index(&after),
    );
    let report = diff::compare(&old.snapshot, &now.snapshot).unwrap();
    // `d\xff` may have existed before under its lossy name, so `old.txt`
    // cannot be shown to have moved there.
    assert!(
        changes(&report, ChangeKind::Moved).is_empty(),
        "{:#?}",
        report.changes
    );
    let added = changes(&report, ChangeKind::Added);
    assert!(added.is_empty(), "{added:#?}");
}

#[test]
fn diff_json_names_the_lossy_rows_and_their_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, legacy) = mixed(tmp.path());
    let out = Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .args([OsStr::new("diff"), legacy.as_os_str(), current.as_os_str()])
        .arg("--json")
        .output()
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    let notes = doc["inputs"]["before"]["notes"].as_array().unwrap();
    assert!(
        notes.iter().any(|n| n["code"] == "legacy_lossy_paths"),
        "{notes:?}"
    );
    let reasons: Vec<&str> = doc["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["reason"].as_str())
        .collect();
    assert_eq!(
        reasons
            .iter()
            .filter(|r| **r == "legacy_name_uncertain")
            .count(),
        4,
        "{reasons:?}"
    );
}

// ── The loader's guards hold for the legacy layout ──────────────────────────

#[test]
fn legacy_index_keeps_every_loader_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, legacy) = mixed(tmp.path());
    let dir = tmp.path();

    let hot = altered_copy(&legacy, "hot.db", "SELECT 1;");
    fs::write(dir.join("hot.db-journal"), b"not empty").unwrap();
    let wal = altered_copy(&legacy, "wal.db", "PRAGMA journal_mode=WAL;");
    let linked = altered_copy(&legacy, "linked.db", "SELECT 1;");
    fs::hard_link(&linked, dir.join("linked-alias.db")).unwrap();
    for (what, db, code) in [
        ("pending journal", hot, NoteCode::PendingJournal),
        ("wal mode", wal, NoteCode::WalModeIndex),
        ("multiply linked", linked, NoteCode::IndexMultiplyLinked),
    ] {
        let loaded = diff_input::load_index(&db);
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Unavailable,
            "{what}"
        );
        assert!(loaded.snapshot.entries.is_empty(), "{what}");
        assert_eq!(codes(&loaded), vec![code], "{what}");
    }

    // A file changed mid-read is never presented.
    let changing = altered_copy(&legacy, "changing.db", "SELECT 1;");
    let target = changing.clone();
    diff_input::set_mid_read_hook(ReadPoint::BetweenRows, move || {
        let mut f = fs::OpenOptions::new().append(true).open(&target).unwrap();
        std::io::Write::write_all(&mut f, b"x").unwrap();
    });
    let loaded = diff_input::load_index(&changing);
    assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
    assert!(loaded.snapshot.entries.is_empty());
    assert_eq!(codes(&loaded), vec![NoteCode::IndexChangedDuringRead]);

    // So is one rewritten in place with its own bytes: same inode, same
    // size, only the times move.
    let rewritten = altered_copy(&legacy, "rewritten.db", "SELECT 1;");
    let (target, bytes) = (rewritten.clone(), fs::read(&rewritten).unwrap());
    diff_input::set_mid_read_hook(ReadPoint::BetweenRows, move || {
        let mut f = fs::OpenOptions::new().write(true).open(&target).unwrap();
        std::io::Write::write_all(&mut f, &bytes).unwrap();
    });
    let loaded = diff_input::load_index(&rewritten);
    assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
    assert_eq!(codes(&loaded), vec![NoteCode::IndexChangedDuringRead]);

    // One read transaction: a writer can commit neither between the
    // metadata and the rows nor while the rows stream.
    for point in [ReadPoint::BetweenStatements, ReadPoint::BetweenRows] {
        let locked = altered_copy(&legacy, &format!("locked-{point:?}.db"), "SELECT 1;");
        let target = locked.clone();
        let outcome = std::rc::Rc::new(std::cell::RefCell::new(None));
        let seen = outcome.clone();
        diff_input::set_mid_read_hook(point, move || {
            let writer = rusqlite::Connection::open(&target).unwrap();
            writer.busy_timeout(std::time::Duration::ZERO).unwrap();
            let result = writer
                .execute_batch("BEGIN IMMEDIATE; UPDATE files SET path = 'rewritten'; COMMIT;");
            let _ = writer.execute_batch("ROLLBACK");
            *seen.borrow_mut() = Some(result.is_ok());
        });
        let loaded = diff_input::load_index(&locked);
        assert_eq!(
            *outcome.borrow(),
            Some(false),
            "{point:?}: a writer committed mid-read"
        );
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Complete,
            "{point:?}"
        );
        assert!(loaded
            .snapshot
            .entries
            .iter()
            .all(|e| e.path != b"rewritten"));
    }
}

// ── What the old writers really stored (review rounds 1 and 2, #107) ───────

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Every member of a tar as tar-rs reads it: for a PAX-sparse member that
/// is the condensed stream the pre-#63 indexer hashed.
fn members(archive: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut ar = tar::Archive::new(fs::File::open(archive).unwrap());
    ar.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let path = e.path_bytes().into_owned();
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut e, &mut data).unwrap();
            (path, data)
        })
        .collect()
}

/// A copy of an index an old indexer wrote (tests/fixtures/legacy), with its
/// recorded source pointed at a copy of the archive it indexed, so the
/// source is reachable. Returns (index, archive).
fn old_index(dir: &Path, db: &str, archive: &str) -> (PathBuf, PathBuf) {
    let source = dir.join(Path::new(archive).file_name().unwrap());
    fs::copy(fixture(archive), &source).unwrap();
    let copy = dir.join(db);
    fs::copy(fixture(&format!("legacy/{db}")), &copy).unwrap();
    let md = fs::metadata(&source).unwrap();
    let mtime = md
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let conn = rusqlite::Connection::open(&copy).unwrap();
    for (key, value) in [
        ("source", source.to_str().unwrap().to_owned()),
        ("archive", source.to_str().unwrap().to_owned()),
        ("archive_size", md.len().to_string()),
        ("archive_mtime_unix", mtime.to_string()),
    ] {
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            [key, value.as_str()],
        )
        .unwrap();
    }
    (copy, source)
}

/// The old indexes holding sparse rows, each with the archive it was
/// written from.
const OLD_SPARSE: [(&str, &str); 5] = [
    ("pre101-pax00.db", "sparse/sparse-pax00.tar"),
    ("pre101-pax10.db", "sparse/sparse-pax10.tar"),
    ("pre101-oldgnu.db", "sparse/sparse-oldgnu.tar"),
    ("v101-pax10.db", "sparse/sparse-pax10.tar"),
    ("pre101-mismatch-pax10.db", "legacy/mismatch-pax10.tar"),
];

fn stored_hash(db: &Path, id: i64) -> [u8; 32] {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let h: Vec<u8> = conn
        .query_row("SELECT content_hash FROM files WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .unwrap();
    h.try_into().unwrap()
}

#[test]
fn a_failed_column_probe_makes_the_index_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, legacy) = mixed(tmp.path());
    for (what, db) in [("current", current), ("legacy", legacy)] {
        // Shadows the pragma_table_xinfo table-valued function, so the probe
        // fails while the row query would still succeed.
        let odd = altered_copy(
            &db,
            &format!("probe-{what}.db"),
            "CREATE TABLE pragma_table_xinfo(x);",
        );
        let loaded = diff_input::load_index(&odd);
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Unavailable,
            "{what}"
        );
        assert!(loaded.snapshot.entries.is_empty(), "{what}");
        assert_eq!(codes(&loaded), vec![NoteCode::IndexUnreadable], "{what}");
        assert!(
            loaded.health.notes[0].detail.contains("pragma_table_xinfo"),
            "{what}: the diagnostic was lost: {}",
            loaded.health.notes[0].detail
        );
    }
}

#[test]
fn a_surviving_raw_link_target_is_used_without_path_raw() {
    let tmp = tempfile::tempdir().unwrap();
    let (current, _) = mixed(tmp.path());
    let half = altered_copy(
        &current,
        "half.db",
        "ALTER TABLE files DROP COLUMN path_raw;",
    );
    let loaded = diff_input::load_index(&half);
    let link = entry(&loaded, b"link");
    assert_eq!(link.link_target.as_deref(), Some(&b"t\xff"[..]));
    assert_eq!(link.flags & LOSSY, 0, "an exact link target was flagged");
    // The names themselves are lossy now.
    assert_eq!(
        loaded
            .snapshot
            .entries
            .iter()
            .filter(|e| e.flags & flags::LOSSY_PATH != 0)
            .count(),
        2
    );
}

#[test]
fn an_exact_move_beside_a_lossy_row_is_still_found() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tar(
        tmp.path(),
        "before.tar",
        &[
            Member::File(b"d\xff", FIRST),
            Member::File(b"old.txt", MOVED),
        ],
    );
    let before = altered_copy(&index(&before), "before-legacy.db", LEGACY);
    let after = tar(
        tmp.path(),
        "after.tar",
        &[
            Member::File(b"d\xff", FIRST),
            Member::File(b"new.txt", MOVED),
        ],
    );
    let after = index(&after);
    let report = diff::compare(
        &diff_input::load_index(&before).snapshot,
        &diff_input::load_index(&after).snapshot,
    )
    .unwrap();
    let moved = changes(&report, ChangeKind::Moved);
    assert_eq!(moved.len(), 1, "{:#?}", report.changes);
    assert_eq!(moved[0].before.as_ref().unwrap().path, "old.txt");
    assert_eq!(moved[0].after.as_ref().unwrap().path, "new.txt");
    assert_eq!(
        lossy_reasons(&report),
        2,
        "the d\\xff rows stay inconclusive"
    );
}

#[test]
fn old_sparse_indexes_are_refused_with_a_reindex_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    for (db, archive) in OLD_SPARSE {
        let dir = tmp.path().join(db);
        fs::create_dir_all(&dir).unwrap();
        let (old, source) = old_index(&dir, db, archive);
        let loaded = diff_input::load_index(&old);
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Unavailable,
            "{db}"
        );
        assert!(loaded.snapshot.entries.is_empty(), "{db}");
        assert_eq!(codes(&loaded), vec![NoteCode::LegacySparseIndex], "{db}");
        let detail = &loaded.health.notes[0].detail;
        assert!(detail.contains("1 sparse row(s)"), "{db}: {detail}");
        assert!(detail.contains("re-index this archive"), "{db}: {detail}");

        // Today's indexer, on the same archive, loads.
        let now = diff_input::load_index(&index(&source));
        assert_eq!(now.snapshot.info.state, SnapshotState::Complete, "{db}");
        assert!(!codes(&now).contains(&NoteCode::LegacySparseIndex), "{db}");
    }
}

#[test]
fn a_condensed_sparse_hash_never_proves_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    for (db, archive) in OLD_SPARSE.iter().filter(|(db, _)| db.contains("-pax")) {
        let dir = tmp.path().join(db);
        fs::create_dir_all(&dir).unwrap();
        let (old, source) = old_index(&dir, db, archive);
        let read = members(&source);
        let (id, condensed) = read
            .iter()
            .enumerate()
            .find(|(_, (path, _))| {
                path.windows(14).any(|w| w == b"GNUSparseFile.") || path == b"holey.bin"
            })
            .map(|(i, (_, data))| (i as i64 + 1, data))
            .unwrap();
        // The fixture is what the old indexer really stored: the hash of the
        // condensed stream.
        assert_eq!(
            stored_hash(&old, id),
            *blake3::hash(condensed).as_bytes(),
            "{db}: fixture does not hold the condensed hash"
        );

        // An ordinary file holding exactly those fragments, in another source.
        let other = tar(
            &dir,
            "fragments.tar",
            &[Member::File(b"holey.bin", condensed)],
        );
        let other = index(&other);
        let loaded = build(&registry(&[(1, "old", &old), (2, "fragments", &other)])).unwrap();
        assert_eq!(
            presence(&loaded, condensed, 1),
            (
                Presence::Unknown(UnknownReason::SourceUnavailable),
                ReplicaCount::AtLeast(1)
            ),
            "{db}"
        );
        // diff calls nothing identical, different, added or removed.
        let report = diff::compare(
            &diff_input::load_index(&old).snapshot,
            &diff_input::load_index(&other).snapshot,
        )
        .unwrap();
        let s = &report.summary;
        assert_eq!(
            s.byte_identical + s.content_changed + s.added + s.removed + s.moved,
            0,
            "{db}: {:#?}",
            report.changes
        );
    }
}

/// The reviewer's case: a PAX 1.0 wrapper `./GNUSparseFile.1370099/holey.bin`
/// whose `GNU.sparse.name` is `data/real.bin`, after an ordinary
/// `data/real.bin`. GNU tar extracts the sparse member there; the old index
/// recorded only the ordinary one under that name.
#[test]
fn a_real_name_the_old_index_never_recorded_is_never_absent_or_a_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, source) = old_index(
        tmp.path(),
        "pre101-mismatch-pax10.db",
        "legacy/mismatch-pax10.tar",
    );
    let read = members(&source);
    assert_eq!(read[0].0, b"data/real.bin");
    assert!(read[1].0.ends_with(b"GNUSparseFile.1370099/holey.bin"));
    let ordinary = &read[0].1;
    assert_eq!(stored_hash(&old, 1), *blake3::hash(ordinary).as_bytes());

    let other = tar(tmp.path(), "other.tar", &[Member::File(b"copy", ordinary)]);
    let other = index(&other);
    let loaded = build(&registry(&[(1, "old", &old), (2, "other", &other)])).unwrap();
    assert_eq!(
        presence(&loaded, ordinary, 1),
        (
            Presence::Unknown(UnknownReason::SourceUnavailable),
            ReplicaCount::AtLeast(1)
        ),
        "the overwritten row counted as a copy"
    );

    let (old, now) = (
        diff_input::load_index(&old),
        diff_input::load_index(&index(&source)),
    );
    entry(&now, b"data/real.bin");
    for (before, after) in [(&old, &now), (&now, &old)] {
        let report = diff::compare(&before.snapshot, &after.snapshot).unwrap();
        let s = &report.summary;
        assert_eq!(s.added + s.removed + s.moved, 0, "{:#?}", report.changes);
        assert_eq!(report.comparison_state, SnapshotState::Unavailable);
    }
}

/// An old indexer's index with no sparse rows keeps loading: its UTF-8
/// names are exact and its lossy ones are marked.
#[test]
fn an_old_index_without_sparse_rows_keeps_loading() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, _) = old_index(tmp.path(), "pre101-plain.db", "legacy/plain-legacy.tar");
    let loaded = diff_input::load_index(&old);
    assert_eq!(
        loaded.snapshot.info.state,
        SnapshotState::Complete,
        "{:?}",
        loaded.health.notes
    );
    assert!(!codes(&loaded).contains(&NoteCode::LegacySparseIndex));
    let cafe = entry(&loaded, "./café.txt".as_bytes());
    assert_eq!(cafe.flags & LOSSY, 0);
    let lossy = loaded
        .snapshot
        .entries
        .iter()
        .filter(|e| e.path == "./d\u{fffd}".as_bytes())
        .collect::<Vec<_>>();
    assert_eq!(lossy.len(), 2);
    assert!(lossy.iter().all(|e| e.flags & flags::LOSSY_PATH != 0));
    assert_ne!(
        entry(&loaded, b"./link").flags & flags::LOSSY_LINK_TARGET,
        0
    );

    let other = tar(
        tmp.path(),
        "other.tar",
        &[
            Member::File(b"copy-of-cafe", CAFE),
            Member::File(b"copy-of-first", FIRST),
        ],
    );
    let other = index(&other);
    let loaded = build(&registry(&[(1, "old", &old), (2, "other", &other)])).unwrap();
    assert_eq!(
        presence(&loaded, CAFE, 1),
        (Presence::Present { copies: 1 }, ReplicaCount::Exact(2))
    );
    let (p, replicas) = presence(&loaded, FIRST, 1);
    assert!(matches!(p, Presence::Unknown(_)), "{p:?}");
    assert_eq!(replicas, ReplicaCount::AtLeast(1));
}
