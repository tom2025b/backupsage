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

use backupsage::coverage::{Presence, ReplicaCount, SourceEvidence, UnknownReason};
use backupsage::coverage_input::{
    build, currency_can_show_presence, load_from_indexes, load_from_master, load_registry,
    set_master_read_hook, LoadCode, LoadedCoverage, LoadedSource, RegistrySource,
};
use backupsage::diff::SourceCurrency;
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
    ]
}

/// The refused source must be unavailable with the loader's reason, carry
/// no rows, count no copy, and leave the shared content inconclusive.
fn assert_refused(loaded: &LoadedCoverage, id: i64, code: NoteCode, what: &str) {
    let s = src(loaded, id);
    assert_eq!(s.evidence, SourceEvidence::Unavailable, "{what}");
    assert!(s.rows.is_empty(), "{what}: refused index carried rows");
    assert!(
        s.index_notes
            .iter()
            .any(|n| n.code == code && !n.detail.is_empty()),
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

/// A pre-v1.0.1 index has no raw-path columns. #103 pinned it as refused
/// (`IndexUnreadable`); since #105 it loads, and its UTF-8 names are the
/// exact bytes, so its copies count like any other source's.
#[test]
fn pre_v1_0_1_layout_is_mapped_not_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = pair(tmp.path());
    for (what, sql) in [
        ("path_raw only", "ALTER TABLE files DROP COLUMN path_raw;"),
        (
            "both raw columns",
            "ALTER TABLE files DROP COLUMN path_raw;
             ALTER TABLE files DROP COLUMN link_target_raw;
             DELETE FROM meta WHERE key = 'path_raw';",
        ),
    ] {
        let old = altered_copy(&b, "old.db", sql);
        let loaded = build(&registry(&[(1, "a", &a), (2, "old", &old)])).unwrap();
        let s = src(&loaded, 2);
        assert_eq!(
            s.evidence,
            SourceEvidence::Complete,
            "{what}: {:?}",
            s.index_notes
        );
        assert_eq!(s.status, SourceStatus::Ok, "{what}");
        assert_eq!(s.rows.len(), 1, "{what}");
        assert_eq!(s.rows[0].path_raw, b"copy-of-shared", "{what}");
        let cov = loaded.coverage().unwrap();
        let g = cov
            .groups
            .iter()
            .find(|g| g.content_hash == shared_hash())
            .unwrap();
        assert_eq!(g.replicas, ReplicaCount::Exact(2), "{what}");
        fs::remove_file(&old).unwrap();
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
    assert!(s.notes[0].detail.contains("files row"), "{:?}", s.notes);
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
    assert!(s.notes[0].detail.contains("metadata-only"), "{:?}", s.notes);
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
    // A distinct identity, so the master registers it as a third source.
    // The master refuses to register a WAL-mode index (#104), so it turns
    // WAL only after registration; loading it must still write nothing.
    let wal_header = altered_copy(
        &b,
        "wal.db",
        "UPDATE meta SET value = 'wal-uuid' WHERE key = 'index_uuid';",
    );
    let master = master_with(tmp.path(), &[&a, &b, &wal_header]);
    rusqlite::Connection::open(&wal_header)
        .unwrap()
        .execute_batch("PRAGMA journal_mode=WAL;")
        .unwrap();
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
            ("archive-missing", E::Unreachable, S::ArchiveMissing),
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
    assert_eq!(g.unknown_sources, 4);
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
    assert!(s.notes[0].detail.contains("'stale-index'"), "{:?}", s.notes);

    // The live view lowers trust too: an ok registry entry whose archive
    // has since vanished is archive-missing.
    let archive2 = tar(d, "b.tar", &[(b"s", SHARED)]);
    let db2 = index(&archive2);
    fs::remove_file(&archive2).unwrap();
    let loaded = build(&registry(&[(1, "b", &db2)])).unwrap();
    assert_eq!(src(&loaded, 1).status, SourceStatus::ArchiveMissing);
    assert_eq!(src(&loaded, 1).evidence, SourceEvidence::Unreachable);
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

// ── Review round 1 (codex, PR #103) ─────────────────────────────────────────

/// Two archives holding SHARED, both indexed; returns (a index, b archive, b index).
fn online_and_unplugged(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let a = tar(dir, "a.tar", &[(b"shared.txt", SHARED)]);
    let b = tar(dir, "b.tar", &[(b"copy-of-shared", SHARED)]);
    let (a_db, b_db) = (index(&a), index(&b));
    (a_db, b, b_db)
}

#[test]
fn unplugged_archive_beside_an_online_copy_is_inconclusive_not_only_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let (a_db, b, b_db) = online_and_unplugged(tmp.path());
    fs::remove_file(&b).unwrap();
    // No other source, so nothing unrelated can make the result unknown.
    let loaded = build(&registry(&[(1, "a", &a_db), (2, "b", &b_db)])).unwrap();
    let s = src(&loaded, 2);
    assert_eq!(s.evidence, SourceEvidence::Unreachable);
    assert_eq!(s.status, SourceStatus::ArchiveMissing);

    let report = loaded.floors(&[], &floor(2)).unwrap();
    let g = &report.groups[0];
    assert_eq!(g.verdict, Verdict::Inconclusive);
    assert!(
        !g.only_copy,
        "an unplugged copy made the online one only-copy"
    );
    assert_eq!(
        (g.trusted_replicas, g.untrusted_replicas, g.unknown_sources),
        (1, 0, 1)
    );
    assert_eq!(report.summary.below_floor, 0);
    // The historical copy is still listed, and not counted.
    let listed: Vec<_> = g
        .copies
        .iter()
        .map(|c| (c.row.source_id, c.status, c.counts_toward_floor))
        .collect();
    assert_eq!(
        listed,
        vec![
            (1, SourceStatus::Ok, true),
            (2, SourceStatus::ArchiveMissing, false)
        ]
    );
}

#[test]
fn unplugged_archive_alone_is_inconclusive_never_zero_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, b, b_db) = online_and_unplugged(tmp.path());
    fs::remove_file(&b).unwrap();
    let loaded = build(&registry(&[(1, "b", &b_db)])).unwrap();
    for n in [1, 2] {
        let report = loaded.floors(&[], &floor(n)).unwrap();
        assert_eq!(report.groups.len(), 1, "content vanished from the report");
        let g = &report.groups[0];
        assert_eq!(g.verdict, Verdict::Inconclusive, "floor {n}");
        assert_eq!((g.trusted_replicas, g.unknown_sources), (0, 1), "floor {n}");
        assert!(!g.only_copy);
        assert_eq!(report.summary.below_floor, 0);
        assert_eq!(g.copies[0].row.source_id, 1);
    }
}

#[test]
fn copied_index_is_refused_rather_than_counted_as_a_second_replica() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, _) = pair(tmp.path());
    let copy = tmp.path().join("a-copy.db");
    fs::copy(&a, &copy).unwrap();
    assert_eq!(digest_of(&a), digest_of(&copy));
    let err = load_from_indexes(&[a.clone(), copy.clone()]).unwrap_err();
    assert!(format!("{err:#}").contains("index_uuid"), "{err:#}");
    let err = build(&registry(&[(1, "a", &a), (2, "copy", &copy)])).unwrap_err();
    assert!(format!("{err:#}").contains("index_uuid"), "{err:#}");
}

#[test]
fn alias_of_an_unreadable_index_is_refused_as_a_repeat() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, b) = pair(tmp.path());
    // Refused before its identity is read, so only the path can match.
    let wal = altered_copy(&b, "wal.db", "PRAGMA journal_mode=WAL;");
    let alias = tmp.path().join("alias.db");
    std::os::unix::fs::symlink(&wal, &alias).unwrap();
    let err = load_from_indexes(&[wal, alias]).unwrap_err();
    assert!(format!("{err:#}").contains("more than once"), "{err:#}");
}

#[test]
fn master_changed_during_the_read_is_refused() {
    for (what, sidecar) in [("appended", false), ("sidecar", true)] {
        let tmp = tempfile::tempdir().unwrap();
        let (a, _) = pair(tmp.path());
        let master = master_with(tmp.path(), &[&a]);
        let target = master.clone();
        set_master_read_hook(move || {
            if sidecar {
                let mut wal = target.as_os_str().to_owned();
                wal.push("-wal");
                fs::write(PathBuf::from(wal), b"").unwrap();
            } else {
                let mut f = fs::OpenOptions::new().append(true).open(&target).unwrap();
                std::io::Write::write_all(&mut f, b"x").unwrap();
            }
        });
        let err = load_registry(&master).unwrap_err();
        assert!(
            format!("{err:#}").contains("changed while it was read"),
            "{what}: {err:#}"
        );
    }
}

#[test]
fn master_symlink_or_hard_link_is_refused_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, _) = pair(tmp.path());
    let master = master_with(tmp.path(), &[&a]);
    let link = tmp.path().join("master-link.db");
    std::os::unix::fs::symlink(&master, &link).unwrap();
    let before = tree_state(tmp.path());
    let err = load_registry(&link).unwrap_err();
    assert!(format!("{err:#}").contains("symlink"), "{err:#}");
    assert_eq!(before, tree_state(tmp.path()));

    let hard = tmp.path().join("master-hard.db");
    fs::hard_link(&master, &hard).unwrap();
    let before = tree_state(tmp.path());
    for path in [&master, &hard] {
        let err = load_registry(path).unwrap_err();
        assert!(format!("{err:#}").contains("hard links"), "{err:#}");
    }
    assert_eq!(before, tree_state(tmp.path()));
}

// ── Review round 2 (codex, PR #103): presence must be positively shown ─────

#[test]
fn removed_archive_with_a_lossy_name_is_inconclusive() {
    let tmp = tempfile::tempdir().unwrap();
    let plain = tar(tmp.path(), "plain.tar", &[(b"s", SHARED)]);
    let lossy = tmp.path().join(OsStr::from_bytes(b"b-\xff.tar"));
    fs::rename(&plain, &lossy).unwrap();
    let db = index(&lossy);

    // Present or removed, a lossy recorded path can never be checked, so
    // nothing shows the copy is still there.
    for removed in [false, true] {
        if removed {
            fs::remove_file(&lossy).unwrap();
        }
        let loaded = build(&registry(&[(1, "b", &db)])).unwrap();
        let s = src(&loaded, 1);
        assert_eq!(s.evidence, SourceEvidence::Unreachable, "removed {removed}");
        let note = s
            .notes
            .iter()
            .find(|n| n.code == LoadCode::SourceUnverified)
            .expect("the reason is recorded");
        assert!(!note.detail.is_empty());
        let report = loaded.floors(&[], &floor(2)).unwrap();
        let g = &report.groups[0];
        assert_eq!(g.verdict, Verdict::Inconclusive, "removed {removed}");
        assert!(!g.only_copy, "removed {removed}");
        assert_eq!(report.summary.below_floor, 0);
    }
}

#[test]
fn unreadable_archive_is_inconclusive_alone_and_beside_an_online_copy() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let (a_db, b, b_db) = online_and_unplugged(tmp.path());
    // Size and mtime stay as indexed, so a stat alone would match.
    fs::set_permissions(&b, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&b).is_ok() {
        // Running as root: permission bits do not stop reads, so this
        // case cannot be produced here. CI runs as an ordinary user.
        eprintln!("skipped: running as root, a 000 file is still readable");
        return;
    }
    let alone = build(&registry(&[(1, "b", &b_db)])).unwrap();
    assert_eq!(src(&alone, 1).evidence, SourceEvidence::Unreachable);
    let report = alone.floors(&[], &floor(2)).unwrap();
    assert_eq!(report.groups[0].verdict, Verdict::Inconclusive);
    assert!(!report.groups[0].only_copy);

    let both = build(&registry(&[(1, "a", &a_db), (2, "b", &b_db)])).unwrap();
    let report = both.floors(&[], &floor(2)).unwrap();
    let g = &report.groups[0];
    assert_eq!(g.verdict, Verdict::Inconclusive);
    assert!(
        !g.only_copy,
        "an unreadable copy made the online one only-copy"
    );
    assert_eq!((g.trusted_replicas, g.unknown_sources), (1, 1));
}

// ── Review round 4 (PR #103): never open anything but the recorded kind ────

/// Run `build` on a worker thread and fail, rather than hang, if it does
/// not return within a few seconds. A blocked worker is left behind; the
/// test process ends regardless.
fn build_promptly(sources: Vec<RegistrySource>) -> LoadedCoverage {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(build(&sources));
    });
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the load hung opening the source")
        .unwrap()
}

/// Replace the archive at `path` with a FIFO nobody writes to.
fn replace_with_fifo(path: &Path) {
    use std::os::unix::fs::FileTypeExt;
    fs::remove_file(path).unwrap();
    let ok = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo runs")
        .success();
    assert!(ok, "mkfifo failed");
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo());
}

#[test]
fn archive_replaced_by_a_fifo_or_directory_is_inconclusive_promptly() {
    for what in ["fifo", "directory"] {
        let tmp = tempfile::tempdir().unwrap();
        let (a_db, b, b_db) = online_and_unplugged(tmp.path());
        if what == "fifo" {
            replace_with_fifo(&b);
        } else {
            fs::remove_file(&b).unwrap();
            fs::create_dir(&b).unwrap();
        }

        let alone = build_promptly(registry(&[(1, "b", &b_db)]));
        let s = src(&alone, 1);
        assert_eq!(s.evidence, SourceEvidence::Unreachable, "{what}");
        assert_eq!(s.status, SourceStatus::ArchiveMissing, "{what}");
        let note = s
            .notes
            .iter()
            .find(|n| n.code == LoadCode::SourceUnverified)
            .expect("the reason is recorded");
        assert!(
            note.detail.contains("not a regular file"),
            "{what}: {}",
            note.detail
        );
        let report = alone.floors(&[], &floor(2)).unwrap();
        assert_eq!(report.groups[0].verdict, Verdict::Inconclusive, "{what}");
        assert!(!report.groups[0].only_copy, "{what}");
        assert_eq!(report.summary.below_floor, 0, "{what}");

        let both = build_promptly(registry(&[(1, "a", &a_db), (2, "b", &b_db)]));
        let report = both.floors(&[], &floor(2)).unwrap();
        let g = &report.groups[0];
        assert_eq!(g.verdict, Verdict::Inconclusive, "{what}");
        assert!(!g.only_copy, "{what}: made the online copy only-copy");
        assert_eq!((g.trusted_replicas, g.unknown_sources), (1, 1), "{what}");
    }
}

/// Every source currency. `exhaustive` below has no `_` arm, so adding a
/// variant to `SourceCurrency` stops this test compiling until the new
/// variant is listed here and its classification asserted.
const ALL_CURRENCIES: [SourceCurrency; 6] = [
    SourceCurrency::NotChecked,
    SourceCurrency::StatMatches,
    SourceCurrency::Stale,
    SourceCurrency::Offline,
    SourceCurrency::Denied,
    SourceCurrency::DirectoryUnverified,
];

#[allow(dead_code)]
fn exhaustive(c: SourceCurrency) {
    match c {
        SourceCurrency::NotChecked
        | SourceCurrency::StatMatches
        | SourceCurrency::Stale
        | SourceCurrency::Offline
        | SourceCurrency::Denied
        | SourceCurrency::DirectoryUnverified => {}
    }
}

#[test]
fn only_currencies_that_observed_the_source_can_show_presence() {
    let allowed: Vec<SourceCurrency> = ALL_CURRENCIES
        .into_iter()
        .filter(|c| currency_can_show_presence(*c))
        .collect();
    // NotChecked never looked at the source (a lossy or unrecorded path).
    assert_eq!(
        allowed,
        vec![
            SourceCurrency::StatMatches,
            SourceCurrency::Stale,
            SourceCurrency::DirectoryUnverified
        ]
    );
}
