//! Coverage input from the master and real indexes (#97): refused indexes
//! stay unavailable, nothing is written beside any input, registry states
//! and protected designations reach the floors engine as ADR 0011 says, and
//! the result does not depend on registry order.

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use backupsage::coverage::{Presence, SourceEvidence, UnknownReason};
use backupsage::coverage_input::{
    build, load_from_indexes, load_from_master, load_registry, LoadCode, LoadedCoverage,
    LoadedSource, RegistrySource,
};
use backupsage::diff_input::{self, NoteCode, ReadPoint};
use backupsage::floors::{FloorParams, SourceStatus, Verdict};
use backupsage::indexer::{self, ContentMode, IndexOptions};
use backupsage::master;
use common::digest_of;

const MTIME: u64 = 1_700_000_001;
const SHARED: &[u8] = b"shared bytes held by more than one source";

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A tar written header by header, so names may be non-UTF-8.
fn tar(dir: &Path, name: &str, members: &[(&[u8], &[u8])]) -> PathBuf {
    let mut bytes = Vec::new();
    for (path, data) in members {
        let mut h = tar::Header::new_gnu();
        h.set_path(OsStr::from_bytes(path)).unwrap();
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
    index_with(source, ContentMode::Full)
}

fn index_with(source: &Path, mode: ContentMode) -> PathBuf {
    let opts = IndexOptions {
        mode,
        ..IndexOptions::default()
    };
    indexer::run_index(source, None, &opts).unwrap().db_path
}

/// Two tar sources that share one content; returns their indexes.
fn pair(dir: &Path) -> (PathBuf, PathBuf) {
    let a = tar(
        dir,
        "a.tar",
        &[(b"shared.txt", SHARED), (b"only-a.txt", b"a")],
    );
    let b = tar(dir, "b.tar", &[(b"copy-of-shared", SHARED)]);
    (index(&a), index(&b))
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

fn src(loaded: &LoadedCoverage, id: i64) -> &LoadedSource {
    loaded.sources.iter().find(|s| s.source_id == id).unwrap()
}

fn shared_hash() -> [u8; 32] {
    *blake3::hash(SHARED).as_bytes()
}

fn floor(n: usize) -> FloorParams {
    FloorParams {
        min_copies: n,
        ..FloorParams::default()
    }
}

// ── Invariant 1: a refused index is unavailable, never absence ─────────────

/// Every way the #101 loader refuses an index, built from `db`.
fn refusals(db: &Path) -> Vec<(&'static str, PathBuf, NoteCode)> {
    let dir = db.parent().unwrap();
    // Pending journal: a hot -journal beside a copy.
    let hot = altered_copy(db, "hot.db", "SELECT 1;");
    fs::write(dir.join("hot.db-journal"), b"not empty").unwrap();
    // Multiply linked.
    let linked = altered_copy(db, "linked.db", "SELECT 1;");
    fs::hard_link(&linked, dir.join("linked-alias.db")).unwrap();
    vec![
        ("missing", dir.join("missing.db"), NoteCode::IndexMissing),
        ("pending journal", hot, NoteCode::PendingJournal),
        (
            "wal mode",
            altered_copy(db, "wal.db", "PRAGMA journal_mode=WAL;"),
            NoteCode::WalModeIndex,
        ),
        ("multiply linked", linked, NoteCode::IndexMultiplyLinked),
        (
            "incompatible hash",
            altered_copy(
                db,
                "sha.db",
                "UPDATE meta SET value = 'sha256' WHERE key = 'hash_algo';",
            ),
            NoteCode::UnsupportedHashAlgo,
        ),
        (
            "pre-v1.0.1 layout",
            altered_copy(db, "old.db", "ALTER TABLE files DROP COLUMN path_raw;"),
            NoteCode::IndexUnreadable,
        ),
    ]
}

/// The refused source must be unavailable with the loader's reason, carry
/// no rows, count no copy, and leave the shared content inconclusive.
fn assert_refused(loaded: &LoadedCoverage, id: i64, code: NoteCode, what: &str) {
    let s = src(loaded, id);
    assert_eq!(s.evidence, SourceEvidence::Unavailable, "{what}");
    assert!(s.rows.is_empty(), "{what}: refused index carried rows");
    assert!(
        s.index_notes.iter().any(|n| n.code == code),
        "{what}: {:?}",
        s.index_notes
    );
    assert_eq!(s.status, SourceStatus::DbMissing, "{what}");

    let cov = loaded.coverage().unwrap();
    let group = cov
        .groups
        .iter()
        .find(|g| g.content_hash == shared_hash())
        .unwrap();
    let p = group
        .presence
        .iter()
        .find(|p| p.source_id == id)
        .unwrap()
        .presence;
    assert_eq!(
        p,
        Presence::Unknown(UnknownReason::SourceUnavailable),
        "{what}: refused index read as {p:?}"
    );

    let report = loaded.floors(&[], &floor(2)).unwrap();
    let g = report
        .groups
        .iter()
        .find(|g| g.content_hash == shared_hash())
        .unwrap();
    assert_eq!(g.verdict, Verdict::Inconclusive, "{what}");
    assert!(!g.only_copy, "{what}: refused index produced only-copy");
    assert_eq!(report.summary.below_floor, 0, "{what}");
}

#[test]
fn every_refused_index_is_an_unavailable_source_never_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    for (what, variant, code) in refusals(&b) {
        let loaded = build(&registry(&[(1, "a", &a), (2, "b", &variant)])).unwrap();
        assert_refused(&loaded, 2, code, what);
    }
}

#[test]
fn busy_index_is_an_unavailable_source_never_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let busy = altered_copy(&b, "busy.db", "SELECT 1;");
    // An in-memory journal holds the exclusive lock with no file beside it.
    let holder = rusqlite::Connection::open(&busy).unwrap();
    holder
        .execute_batch(
            "PRAGMA journal_mode=MEMORY; PRAGMA locking_mode=EXCLUSIVE;
             BEGIN EXCLUSIVE; COMMIT;",
        )
        .unwrap();
    let loaded = build(&registry(&[(1, "a", &a), (2, "b", &busy)])).unwrap();
    drop(holder);
    assert_refused(&loaded, 2, NoteCode::IndexBusy, "busy");
}

#[test]
fn index_changed_during_read_is_an_unavailable_source_never_absence() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let changing = altered_copy(&b, "changing.db", "SELECT 1;");
    let target = changing.clone();
    // Source 1 is read first, so the hook fires during its read.
    diff_input::set_mid_read_hook(ReadPoint::BetweenRows, move || {
        let mut f = fs::OpenOptions::new().append(true).open(&target).unwrap();
        std::io::Write::write_all(&mut f, b"x").unwrap();
    });
    let loaded = build(&registry(&[(1, "changing", &changing), (2, "a", &a)])).unwrap();
    assert_refused(&loaded, 1, NoteCode::IndexChangedDuringRead, "changed");
}

#[test]
fn unknown_entry_type_refuses_the_index_rather_than_guessing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let odd = altered_copy(
        &b,
        "odd.db",
        "INSERT INTO files (path, entry_type, kind, size, flags)
         VALUES ('dev/null', 'chardev', 'binary', 0, 0);",
    );
    let loaded = build(&registry(&[(1, "a", &a), (2, "odd", &odd)])).unwrap();
    let s = src(&loaded, 2);
    assert_eq!(s.evidence, SourceEvidence::Unavailable);
    assert!(s.rows.is_empty());
    assert_eq!(s.notes[0].code, LoadCode::UnknownEntryType);
    let report = loaded.floors(&[], &floor(2)).unwrap();
    assert_eq!(report.groups[0].verdict, Verdict::Inconclusive);
}

#[test]
fn metadata_only_index_with_a_hash_is_refused_as_contradictory() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tar(tmp.path(), "a.tar", &[(b"shared.txt", SHARED)]);
    let m = tar(tmp.path(), "m.tar", &[(b"shared.txt", SHARED)]);
    let meta = index_with(&m, ContentMode::MetadataOnly);
    let forged = altered_copy(
        &meta,
        "forged.db",
        "UPDATE files SET content_hash = randomblob(32);",
    );
    let loaded = build(&registry(&[(1, "a", &index(&a)), (2, "forged", &forged)])).unwrap();
    let s = src(&loaded, 2);
    assert_eq!(s.evidence, SourceEvidence::Unavailable);
    assert_eq!(s.notes[0].code, LoadCode::ContradictoryContentMode);
}

// ── Invariant 2: nothing is written beside the master or any index ─────────

type TreeState = BTreeMap<String, (u64, String, u64, i64, i64, i64, i64)>;

/// Size, digest, inode, mtime and ctime of every file, and of the directory.
fn tree_state(dir: &Path) -> TreeState {
    let observe = |p: &Path, digest: String| {
        let md = fs::symlink_metadata(p).unwrap();
        let len = if md.is_file() { md.len() } else { 0 };
        (
            len,
            digest,
            md.ino(),
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

fn master_with(dir: &Path, dbs: &[&Path]) -> PathBuf {
    let path = dir.join("master.db");
    let mut m = master::open_at(&path).unwrap();
    for db in dbs {
        m.add(db).unwrap();
    }
    drop(m);
    path
}

#[test]
fn master_and_indexes_are_read_with_nothing_written_beside_them() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let wal_header = altered_copy(&b, "wal.db", "PRAGMA journal_mode=WAL;");
    let master = master_with(tmp.path(), &[&a, &b, &wal_header]);
    // The master itself is a WAL database; idle, it has no sidecar.
    for sidecar in ["master.db-wal", "master.db-shm", "master.db-journal"] {
        assert!(!tmp.path().join(sidecar).exists(), "{sidecar}");
    }
    let before = tree_state(tmp.path());

    let loaded = load_from_master(&master).unwrap();
    assert_eq!(loaded.sources.len(), 3);
    loaded.floors(&[1], &floor(2)).unwrap();
    load_registry(&master).unwrap();

    assert_eq!(
        before,
        tree_state(tmp.path()),
        "loading wrote or changed something beside the master or an index"
    );
}

#[test]
fn master_in_use_or_missing_is_refused_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, _) = pair(tmp.path());
    let master = master_with(tmp.path(), &[&a]);

    // Another command holding the master open: its -wal/-shm are live.
    let holder = master::open_at(&master).unwrap();
    holder.list().unwrap();
    let before = tree_state(tmp.path());
    let err = load_from_master(&master).unwrap_err();
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert_eq!(before, tree_state(tmp.path()));
    drop(holder);

    // A missing master is an error, and is never created.
    let missing = tmp.path().join("no-master.db");
    let before = tree_state(tmp.path());
    assert!(load_from_master(&missing).is_err());
    assert_eq!(before, tree_state(tmp.path()));
    assert!(!missing.exists());

    // A per-source index is not a master.
    assert!(load_registry(&a).is_err());
}

// ── Invariant 3: registry states and designations reach the floors engine ──

#[test]
fn every_registry_state_maps_to_evidence_and_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let ok = tar(d, "ok.tar", &[(b"s", SHARED)]);
    let stale = tar(d, "stale.tar", &[(b"s", SHARED)]);
    let gone = tar(d, "gone.tar", &[(b"s", SHARED)]);
    let offline = tar(d, "offline.tar", &[(b"s", SHARED)]);
    let meta = tar(d, "meta.tar", &[(b"s", SHARED)]);
    let folder = d.join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join("s"), SHARED).unwrap();

    let ok_db = index(&ok);
    let stale_db = index(&stale);
    let gone_db = index(&gone);
    let offline_db = index(&offline);
    let meta_db = index_with(&meta, ContentMode::MetadataOnly);
    let folder_db = index(&folder);
    let partial_db = altered_copy(
        &ok_db,
        "partial.db",
        "UPDATE meta SET value = '0' WHERE key = 'completed';
         UPDATE meta SET value = 'partial-uuid' WHERE key = 'index_uuid';",
    );
    let v2_db = altered_copy(
        &ok_db,
        "v2.db",
        "UPDATE meta SET value = '2' WHERE key = 'schema_version';
         UPDATE meta SET value = 'v2-uuid' WHERE key = 'index_uuid';",
    );
    let path = master_with(
        d,
        &[
            &ok_db,
            &stale_db,
            &gone_db,
            &offline_db,
            &meta_db,
            &folder_db,
            &partial_db,
            &v2_db,
        ],
    );
    // Drive the registry into each state with real master operations.
    let mut f = fs::OpenOptions::new().append(true).open(&stale).unwrap();
    std::io::Write::write_all(&mut f, &[0u8; 512]).unwrap();
    drop(f);
    fs::remove_file(&gone).unwrap();
    fs::remove_file(&offline_db).unwrap();
    let mut m = master::open_at(&path).unwrap();
    m.sync(None).unwrap();
    m.verify(false).unwrap();
    let statuses: BTreeMap<i64, String> = m
        .list()
        .unwrap()
        .into_iter()
        .map(|r| (r.archive_id, r.status))
        .collect();
    drop(m);
    assert_eq!(
        statuses.values().cloned().collect::<Vec<_>>(),
        [
            "ok",
            "stale-index",
            "archive-missing",
            "db-missing",
            "ok",
            "ok",
            "incomplete",
            "v2-limited"
        ]
    );

    let loaded = load_from_master(&path).unwrap();
    let got: Vec<_> = loaded
        .sources
        .iter()
        .map(|s| (s.registry_status.as_deref().unwrap(), s.evidence, s.status))
        .collect();
    use SourceEvidence as E;
    use SourceStatus as S;
    assert_eq!(
        got,
        vec![
            ("ok", E::Complete, S::Ok),
            ("stale-index", E::Complete, S::StaleIndex),
            ("archive-missing", E::Complete, S::ArchiveMissing),
            ("db-missing", E::Unavailable, S::DbMissing),
            ("ok", E::NoContentHashes, S::Ok),
            ("ok", E::Complete, S::Ok),
            ("incomplete", E::Incomplete, S::Incomplete),
            ("v2-limited", E::Unavailable, S::DbMissing),
        ]
    );
    // The same statuses reach the floors engine, copy by copy.
    assert_eq!(
        loaded.statuses(),
        loaded
            .sources
            .iter()
            .map(|s| (s.source_id, s.status))
            .collect::<Vec<_>>()
    );
    let report = loaded.floors(&[6], &floor(2)).unwrap();
    let g = &report.groups[0];
    let copies: Vec<_> = g
        .copies
        .iter()
        .map(|c| {
            (
                c.row.source_id,
                c.status,
                c.counts_toward_floor,
                c.protected,
            )
        })
        .collect();
    assert_eq!(
        copies,
        vec![
            (1, S::Ok, true, false),
            (2, S::StaleIndex, false, false),
            (3, S::ArchiveMissing, false, false),
            (6, S::Ok, true, true),
            (7, S::Incomplete, true, false),
        ]
    );
    // db-missing, metadata-only and v2 sources are unknown, never absent.
    assert_eq!(g.trusted_replicas, 3);
    assert_eq!(g.protected_replicas, 1);
    assert_eq!(g.unknown_sources, 3);
    assert_eq!(g.verdict, Verdict::MeetsFloor);
    let report = loaded.floors(&[6], &floor(4)).unwrap();
    assert_eq!(report.groups[0].verdict, Verdict::Inconclusive);
}

#[test]
fn registry_status_only_ever_lowers_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let archive = tar(d, "a.tar", &[(b"s", SHARED)]);
    let db = index(&archive);
    let original = fs::read(&archive).unwrap();
    let original_mtime = fs::metadata(&archive).unwrap().modified().unwrap();
    let path = master_with(d, &[&db]);

    // `master verify` recorded stale-index; the archive is then restored
    // byte for byte, so a live stat matches again. The registry still wins.
    let mut f = fs::OpenOptions::new().append(true).open(&archive).unwrap();
    std::io::Write::write_all(&mut f, &[0u8; 512]).unwrap();
    drop(f);
    let mut m = master::open_at(&path).unwrap();
    m.verify(false).unwrap();
    drop(m);
    fs::write(&archive, &original).unwrap();
    fs::File::options()
        .write(true)
        .open(&archive)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let loaded = load_from_master(&path).unwrap();
    let s = src(&loaded, 1);
    assert_eq!(s.registry_status.as_deref(), Some("stale-index"));
    assert_eq!(s.status, SourceStatus::StaleIndex);
    assert_eq!(s.notes[0].code, LoadCode::RegistryStatus);

    // The live view lowers trust too: an ok registry entry whose archive
    // has since vanished is archive-missing.
    let archive2 = tar(d, "b.tar", &[(b"s", SHARED)]);
    let db2 = index(&archive2);
    fs::remove_file(&archive2).unwrap();
    let loaded = build(&registry(&[(1, "b", &db2)])).unwrap();
    assert_eq!(src(&loaded, 1).status, SourceStatus::ArchiveMissing);
}

#[test]
fn unknown_registry_status_is_refused_rather_than_guessed() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, _) = pair(tmp.path());
    let mut reg = registry(&[(1, "a", &a)]);
    reg[0].registry_status = Some("quarantined".into());
    let err = build(&reg).unwrap_err();
    assert!(format!("{err:#}").contains("quarantined"), "{err:#}");
}

#[test]
fn protected_designation_reaches_the_floors_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let loaded = build(&registry(&[(1, "a", &a), (2, "b", &b)])).unwrap();
    let report = loaded.floors(&[2], &floor(2)).unwrap();
    let g = report
        .groups
        .iter()
        .find(|g| g.content_hash == shared_hash())
        .unwrap();
    assert_eq!(g.verdict, Verdict::MeetsFloor);
    assert_eq!(g.protected_replicas, 1);
    let marks: Vec<_> = g
        .copies
        .iter()
        .map(|c| (c.row.source_id, c.protected))
        .collect();
    assert_eq!(marks, vec![(1, false), (2, true)]);
    let sources: Vec<_> = report
        .sources
        .iter()
        .map(|s| (s.source_id, s.protected))
        .collect();
    assert_eq!(sources, vec![(1, false), (2, true)]);
}

// ── Invariant 4: deterministic, whatever the registry order ────────────────

#[test]
fn result_is_identical_for_any_registry_order() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let c = index(&tar(tmp.path(), "c.tar", &[(b"s", SHARED)]));
    // Labels sort opposite to ids, so only an id order passes.
    let base = registry(&[(1, "zz", &a), (2, "mm", &b), (3, "aa", &c)]);
    let expected = build(&base).unwrap();
    let ids: Vec<i64> = expected.sources.iter().map(|s| s.source_id).collect();
    assert_eq!(ids, vec![1, 2, 3]);
    let expected_report = format!("{:?}", expected.floors(&[3], &floor(2)).unwrap());
    let expected = format!("{:?}", expected.sources);

    for rotation in 0..3 {
        for reverse in [false, true] {
            let mut reg = base.clone();
            reg.rotate_left(rotation);
            if reverse {
                reg.reverse();
            }
            let loaded = build(&reg).unwrap();
            assert_eq!(format!("{:?}", loaded.sources), expected);
            assert_eq!(
                format!("{:?}", loaded.floors(&[3], &floor(2)).unwrap()),
                expected_report
            );
        }
    }

    let mut dup = base.clone();
    dup[2].source_id = 1;
    assert!(build(&dup).is_err(), "duplicate source id");
}

// ── Paths, ad-hoc indexes ───────────────────────────────────────────────────

#[test]
fn non_utf8_raw_paths_map_losslessly_from_tar_and_directory_indexes() {
    let tmp = tempfile::tempdir().unwrap();
    let t = tar(
        tmp.path(),
        "raw.tar",
        &[(b"raw-\xff", SHARED), (b"plain.txt", b"plain")],
    );
    let folder = tmp.path().join("folder");
    fs::create_dir(&folder).unwrap();
    fs::write(folder.join(OsStr::from_bytes(b"dir-\xfe")), SHARED).unwrap();
    let loaded = build(&registry(&[
        (1, "t", &index(&t)),
        (2, "d", &index(&folder)),
    ]))
    .unwrap();

    let cov = loaded.coverage().unwrap();
    let g = cov
        .groups
        .iter()
        .find(|g| g.content_hash == shared_hash())
        .unwrap();
    let paths: Vec<_> = g
        .copies
        .iter()
        .map(|c| (c.source_id, c.path_raw.clone()))
        .collect();
    assert_eq!(
        paths,
        vec![(1, b"raw-\xff".to_vec()), (2, b"dir-\xfe".to_vec())]
    );
    // A UTF-8 name stores no path_raw; its text bytes are the raw bytes.
    assert!(src(&loaded, 1)
        .rows
        .iter()
        .any(|r| r.path_raw == b"plain.txt".to_vec()));
}

#[test]
fn ad_hoc_indexes_load_in_argument_order_and_refuse_repeats() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    let loaded = load_from_indexes(&[b.clone(), a.clone()]).unwrap();
    let got: Vec<_> = loaded
        .sources
        .iter()
        .map(|s| (s.source_id, s.label.as_str(), s.registry_status.clone()))
        .collect();
    assert_eq!(got, vec![(1, "b.tar.db", None), (2, "a.tar.db", None)]);
    assert!(load_from_indexes(&[a.clone(), a.clone()]).is_err());
}
