//! `backupsage diff` end to end (#93): read-only index loading, input
//! health, move-inference explanations, JSON and terminal fixtures, and the
//! 0/1/2 exit codes. The pure engine is covered by tests/diff.rs.
//!
//! Fixtures under tests/fixtures/diff_cli/ are compared byte for byte after
//! replacing the only run-varying values: the temp directory (as text and as
//! hex) and the random index UUIDs. Regenerate deliberately with:
//!   BACKUPSAGE_BLESS=1 cargo test --test diff_cli

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use backupsage::diff::{Side, SnapshotState};
use backupsage::diff_input::{self, MoveBlocker, MoveBlockerCause, NoteCode, ReadPoint};
use backupsage::indexer::{self, IndexOptions};
use backupsage::report::to_hex;
use backupsage::store::flags;
use common::digest_of;
use serde_json::Value;

const MTIME: u64 = 1_700_000_001;

// ── Corpus construction ─────────────────────────────────────────────────────

/// Raw tar bytes, member by member, so a corpus can repeat paths, carry
/// non-UTF-8 names, links and hand-made pax headers.
#[derive(Default)]
struct Tar(Vec<u8>);

impl Tar {
    fn member(mut self, path: &[u8], kind: tar::EntryType, data: &[u8], mode: u32) -> Self {
        self.member_header(path, kind, data.len() as u64, mode, None);
        self.0.extend_from_slice(data);
        self.pad()
    }

    fn member_header(
        &mut self,
        path: &[u8],
        kind: tar::EntryType,
        size: u64,
        mode: u32,
        link: Option<&[u8]>,
    ) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_path(OsStr::from_bytes(path)).unwrap();
        if let Some(target) = link {
            h.set_link_name(OsStr::from_bytes(target)).unwrap();
        }
        h.set_size(size);
        h.set_mode(mode);
        h.set_mtime(MTIME);
        h.set_cksum();
        self.0.extend_from_slice(h.as_bytes());
    }

    fn pad(mut self) -> Self {
        while !self.0.len().is_multiple_of(512) {
            self.0.push(0);
        }
        self
    }

    fn file(self, path: &[u8], data: &[u8]) -> Self {
        self.member(path, tar::EntryType::Regular, data, 0o644)
    }

    fn file_mode(self, path: &[u8], data: &[u8], mode: u32) -> Self {
        self.member(path, tar::EntryType::Regular, data, mode)
    }

    fn link(mut self, kind: tar::EntryType, path: &[u8], target: &[u8]) -> Self {
        self.member_header(path, kind, 0, 0o644, Some(target));
        self
    }

    /// A pax extended header applying to the next member.
    fn pax(self, body: &[u8]) -> Self {
        self.member(b"paxheader/next", tar::EntryType::XHeader, body, 0o644)
    }

    fn write(mut self, dir: &Path, name: &str) -> PathBuf {
        self.0.extend_from_slice(&[0u8; 1024]);
        let path = dir.join(name);
        fs::write(&path, &self.0).unwrap();
        path
    }
}

fn index(source: &Path) -> PathBuf {
    indexer::run_index(source, None, &IndexOptions::default())
        .unwrap()
        .db_path
}

/// Every class of change, duplicate paths and display-colliding non-UTF-8
/// names; no links and every row hashed, so moves can be inferred.
fn clean_corpus(dir: &Path) -> (PathBuf, PathBuf) {
    let before = Tar::default()
        .file(b"unchanged.txt", b"same bytes")
        .file(b"metadata.txt", b"metadata bytes")
        .file(b"content.txt", b"old content")
        .file(b"old-name.txt", b"unique moved bytes")
        .file(b"removed.txt", b"gone bytes")
        .file(b"dup.txt", b"first copy")
        .file(b"dup.txt", b"second copy")
        .file(b"raw-\xff", b"ff bytes")
        .file(b"raw-\xfe", b"fe bytes")
        .write(dir, "before.tar");
    let after = Tar::default()
        .file(b"unchanged.txt", b"same bytes")
        .file_mode(b"metadata.txt", b"metadata bytes", 0o600)
        .file(b"content.txt", b"new content")
        .file(b"new-name.txt", b"unique moved bytes")
        .file(b"added.txt", b"fresh bytes")
        .file(b"dup.txt", b"second copy")
        .file(b"raw-\xff", b"ff bytes")
        .file(b"raw-\xfe", b"fe changed")
        .write(dir, "after.tar");
    (index(&before), index(&after))
}

/// The same rename as in the clean corpus, but the after side holds a
/// hardlink, a symlink, a row with unparsed pax metadata and an unsupported
/// PAX-sparse row that shadows a hashed regular file (the #65 shape).
fn links_corpus(dir: &Path) -> (PathBuf, PathBuf) {
    let before = Tar::default()
        .file(b"renamed-src.bin", b"would-be-moved bytes")
        .file(b"data/file.bin", b"original content bytes")
        .file(b"linked.txt", b"link target bytes")
        .write(dir, "links-before.tar");
    let after = Tar::default()
        .file(b"renamed-dst.bin", b"would-be-moved bytes")
        .file(b"data/file.bin", b"original content bytes")
        .pax(b"25 GNU.sparse.size=40960\n")
        .file(b"data/file.bin", b"condensed")
        .file(b"linked.txt", b"link target bytes")
        .link(tar::EntryType::Link, b"alias", b"linked.txt")
        .link(tar::EntryType::Symlink, b"sym", b"linked.txt")
        .pax(b"this is not a pax record at all")
        .file(b"xattred.txt", b"xattr content")
        .write(dir, "links-after.tar");
    (index(&before), index(&after))
}

fn write_file(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME))
        .unwrap();
}

fn directory_corpus(dir: &Path) -> (PathBuf, PathBuf) {
    let before = dir.join("dir-before");
    let after = dir.join("dir-after");
    let raw = OsStr::from_bytes(b"name-\xff");
    write_file(&before.join("a.txt"), b"same in both");
    write_file(&before.join("sub/b.txt"), b"before bytes");
    write_file(&before.join(raw), b"raw name bytes");
    write_file(&before.join("moved-from.txt"), b"moved dir bytes");
    write_file(&after.join("a.txt"), b"same in both");
    write_file(&after.join("sub/b.txt"), b"after bytes");
    write_file(&after.join(raw), b"raw name bytes");
    write_file(&after.join("moved-to.txt"), b"moved dir bytes");
    (index(&before), index(&after))
}

/// A copy of `db` changed through an ordinary writable connection, as a
/// test fixture only.
fn altered_copy(db: &Path, name: &str, sql: &str) -> PathBuf {
    let copy = db.with_file_name(name);
    fs::copy(db, &copy).unwrap();
    rusqlite::Connection::open(&copy)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
    copy
}

/// A pre-v3 index: FTS table and meta, no `files` table.
fn v2_index(dir: &Path) -> PathBuf {
    let db = dir.join("v2.tar.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE files_fts USING fts5(path, content);
         CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         INSERT INTO meta VALUES ('schema_version', '2'), ('completed', '1'),
                                 ('source', '{}');",
        dir.join("v2.tar").display()
    ))
    .unwrap();
    db
}

/// An index whose `-journal` holds an uncommitted transaction: the main file
/// alone is torn, and only a writing open could roll it back.
fn hot_journal_copy(db: &Path) -> PathBuf {
    let work = db.with_file_name("hot-work.db");
    fs::copy(db, &work).unwrap();
    let conn = rusqlite::Connection::open(&work).unwrap();
    conn.execute_batch(
        "PRAGMA cache_size=1;
         BEGIN;
         INSERT INTO meta SELECT 'filler' || i, hex(randomblob(4000)) FROM
           (WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<100)
            SELECT i FROM r);
         UPDATE meta SET value = 'torn' WHERE key = 'source';",
    )
    .unwrap();
    let hot = db.with_file_name("hot.db");
    fs::copy(&work, &hot).unwrap();
    fs::copy(
        db.with_file_name("hot-work.db-journal"),
        db.with_file_name("hot.db-journal"),
    )
    .unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
    drop(conn);
    fs::remove_file(&work).unwrap();
    hot
}

/// An index whose committed changes still sit in its `-wal`.
fn pending_wal_copy(db: &Path) -> PathBuf {
    let work = db.with_file_name("wal-work.db");
    fs::copy(db, &work).unwrap();
    let conn = rusqlite::Connection::open(&work).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
         UPDATE meta SET value = 'moved-on' WHERE key = 'source';",
    )
    .unwrap();
    let pending = db.with_file_name("wal-pending.db");
    fs::copy(&work, &pending).unwrap();
    fs::copy(
        db.with_file_name("wal-work.db-wal"),
        db.with_file_name("wal-pending.db-wal"),
    )
    .unwrap();
    drop(conn);
    for leftover in ["wal-work.db", "wal-work.db-wal", "wal-work.db-shm"] {
        let _ = fs::remove_file(db.with_file_name(leftover));
    }
    pending
}

/// A complete index whose header says WAL mode, with no sidecar present.
/// A plain read-only SQLite open creates `-shm`/`-wal` beside such a file.
fn wal_header_copy(db: &Path) -> PathBuf {
    altered_copy(db, "wal-header.db", "PRAGMA journal_mode=WAL;")
}

// ── Running the binary and fixtures ─────────────────────────────────────────

fn run(args: &[&OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .args(args)
        .output()
        .expect("binary runs")
}

fn diff(before: &Path, after: &Path, json: bool) -> Output {
    let mut args = vec![OsStr::new("diff"), before.as_os_str(), after.as_os_str()];
    if json {
        args.push(OsStr::new("--json"));
    }
    run(&args)
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("no signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("diff output is UTF-8")
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

/// Replace the temp directory (text and hex spellings) and index UUIDs.
fn normalize(raw: &str, tmp: &Path, dbs: &[(&Path, &str)]) -> String {
    let mut s = raw.to_owned();
    let canonical = tmp.canonicalize().unwrap();
    for spelling in [canonical.as_path(), tmp] {
        s = s.replace(&to_hex(spelling.as_os_str().as_bytes()), "<TMP-HEX>");
        s = s.replace(spelling.to_str().unwrap(), "<TMP>");
    }
    for (db, label) in dbs {
        if let Some(uuid) = diff_input::load_index(db).snapshot.info.index_uuid {
            s = s.replace(&uuid, label);
        }
    }
    s
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_cli")
}

const FIXTURES: &[&str] = &[
    "clean.json",
    "clean.txt",
    "currency.json",
    "directory.json",
    "incompatible_v2.json",
    "incomplete.json",
    "links_sparse_pax.json",
    "links_sparse_pax.txt",
    "unavailable.json",
    "unavailable.txt",
];

fn golden(name: &str, actual: &str) {
    assert!(FIXTURES.contains(&name), "unlisted fixture {name}");
    let path = fixture_dir().join(name);
    if std::env::var_os("BACKUPSAGE_BLESS").is_some() {
        fs::create_dir_all(fixture_dir()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing fixture {name}"));
    assert!(
        expected == actual,
        "diff CLI contract drifted: {name}\n--- expected\n{expected}\n--- actual\n{actual}"
    );
}

/// Both renderings of one comparison: fixtures, exit code, determinism,
/// and agreement between the move blockers and the engine's result.
fn check(
    tmp: &Path,
    before: &Path,
    after: &Path,
    fixture: &str,
    text_fixture: bool,
    exit: i32,
) -> Value {
    let dbs = [(before, "<UUID-BEFORE>"), (after, "<UUID-AFTER>")];
    let out = diff(before, after, true);
    assert_eq!(
        code(&out),
        exit,
        "{fixture}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        diff(before, after, true).stdout,
        out.stdout,
        "{fixture}: output differs between identical runs"
    );
    golden(fixture, &normalize(&stdout(&out), tmp, &dbs));
    let doc = json(&out);
    if doc["move_inference"]["enabled"] == Value::Bool(false) {
        assert_eq!(doc["summary"]["moved"], 0, "{fixture}: blocked yet moved");
    }

    let text = diff(before, after, false);
    assert_eq!(code(&text), exit);
    if text_fixture {
        golden(
            &fixture.replace(".json", ".txt"),
            &normalize(&stdout(&text), tmp, &dbs),
        );
    }
    doc
}

fn summary(doc: &Value) -> [u64; 8] {
    let s = &doc["summary"];
    [
        "added",
        "removed",
        "moved",
        "byte_identical",
        "metadata_only_changed",
        "content_changed",
        "inconclusive",
        "excluded",
    ]
    .map(|k| s[k].as_u64().unwrap())
}

fn change_for<'a>(doc: &'a Value, path: &str) -> &'a Value {
    doc["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| {
            [&c["before"], &c["after"]]
                .iter()
                .any(|e| e["path"].as_str() == Some(path))
        })
        .unwrap_or_else(|| panic!("no change for {path}"))
}

// ── Fixtures and exit codes ─────────────────────────────────────────────────

#[test]
fn clean_corpus_classifies_every_kind_and_exits_0() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    let doc = check(tmp.path(), &before, &after, "clean.json", true, 0);
    assert_eq!(doc["comparison_state"], "complete");
    assert_eq!(summary(&doc), [1, 1, 1, 3, 1, 2, 0, 1]);
    assert_eq!(doc["move_inference"]["enabled"], true);
    let moved = change_for(&doc, "old-name.txt");
    assert_eq!(moved["kind"], "moved");
    assert_eq!(moved["after"]["path"], "new-name.txt");
    // The two raw names render identically but stay distinct rows.
    let raw: Vec<_> = doc["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["before"]["path"] == "raw-\u{fffd}")
        .map(|c| (c["kind"].clone(), c["before"]["path_bytes"].clone()))
        .collect();
    assert_eq!(raw.len(), 2);
    assert!(raw.contains(&(Value::from("byte_identical"), Value::from("7261772dff"))));
    assert!(raw.contains(&(Value::from("content_changed"), Value::from("7261772dfe"))));
    // The earlier duplicate row is excluded, never compared.
    assert_eq!(doc["excluded"][0]["entry"]["path"], "dup.txt");
    assert_eq!(doc["excluded"][0]["reason"], "shadowed_path");
}

#[test]
fn links_sparse_and_unparsed_pax_are_inconclusive_and_explained() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = links_corpus(tmp.path());
    let doc = check(
        tmp.path(),
        &before,
        &after,
        "links_sparse_pax.json",
        true,
        2,
    );
    assert_eq!(doc["comparison_state"], "incomplete");
    // The rename cannot be inferred, so it stays a removal and an addition.
    assert_eq!(change_for(&doc, "renamed-src.bin")["kind"], "removed");
    assert_eq!(change_for(&doc, "renamed-dst.bin")["kind"], "added");
    let blockers: Vec<_> = doc["move_inference"]["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["side"].clone(), b["cause"].clone(), b["rows"].clone()))
        .collect();
    assert_eq!(
        blockers,
        [
            ("after", "hardlink"),
            ("after", "symlink"),
            ("after", "pax_unparsed"),
            ("after", "unsupported_sparse"),
        ]
        .map(|(side, cause)| (Value::from(side), Value::from(cause), Value::from(1)))
    );
}

#[test]
fn directory_sources_say_their_currency_is_unverified() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = directory_corpus(tmp.path());
    let doc = check(tmp.path(), &before, &after, "directory.json", false, 0);
    for side in ["before", "after"] {
        assert_eq!(doc[side]["source_currency"], "directory_unverified");
        assert!(doc["inputs"][side]["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["code"] == "source_directory_unverified"));
    }
    assert_eq!(summary(&doc), [0, 0, 1, 2, 0, 1, 0, 0]);
    assert_eq!(change_for(&doc, "name-\u{fffd}")["kind"], "byte_identical");
}

#[test]
fn unavailable_input_is_inconclusive_never_absent_and_never_created() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, after) = clean_corpus(tmp.path());
    let missing = tmp.path().join("missing.db");
    let doc = check(tmp.path(), &missing, &after, "unavailable.json", true, 2);
    assert!(!missing.exists(), "diff created the missing index");
    assert_eq!(doc["comparison_state"], "unavailable");
    assert_eq!(doc["inputs"]["before"]["notes"][0]["code"], "index_missing");
    assert_eq!(summary(&doc)[..2], [0, 0], "absence was claimed");
    assert!(doc["changes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["kind"] == "inconclusive" && c["reason"] == "other_snapshot_incomplete"));
}

#[test]
fn incomplete_index_is_explicit_and_cannot_prove_additions() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    let partial = altered_copy(
        &before,
        "partial.db",
        "UPDATE meta SET value = '0' WHERE key = 'completed';",
    );
    let doc = check(tmp.path(), &partial, &after, "incomplete.json", false, 2);
    assert_eq!(doc["before"]["state"], "incomplete");
    assert_eq!(
        doc["inputs"]["before"]["notes"][0]["code"],
        "index_incomplete"
    );
    // A row the partial index lacks may exist unseen: never "added".
    assert_eq!(summary(&doc)[0], 0);
    assert_eq!(change_for(&doc, "added.txt")["kind"], "inconclusive");
    // A row it has, absent from the complete peer, is really removed.
    assert_eq!(change_for(&doc, "removed.txt")["kind"], "removed");
}

#[test]
fn pre_v3_index_is_incompatible_not_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, after) = clean_corpus(tmp.path());
    let v2 = v2_index(tmp.path());
    let doc = check(tmp.path(), &v2, &after, "incompatible_v2.json", false, 2);
    assert_eq!(doc["comparison_state"], "incompatible");
    assert_eq!(doc["before"]["state"], "incompatible");
    let codes: Vec<_> = doc["inputs"]["before"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["code"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        codes,
        [
            "unsupported_schema",
            "unsupported_hash_algo",
            "missing_index_uuid",
            "source_offline"
        ]
    );
    assert_eq!(summary(&doc)[0], 0);
}

#[test]
fn source_currency_is_reported_separately_from_the_comparison() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Tar::default()
        .file(b"f.txt", b"one")
        .write(tmp.path(), "a.tar");
    let b = Tar::default()
        .file(b"f.txt", b"one")
        .write(tmp.path(), "b.tar");
    let (a_db, b_db) = (index(&a), index(&b));
    // a.tar changes after indexing; b.tar goes away.
    std::io::Write::write_all(
        &mut fs::OpenOptions::new().append(true).open(&a).unwrap(),
        &[0u8; 512],
    )
    .unwrap();
    fs::remove_file(&b).unwrap();
    let doc = check(tmp.path(), &a_db, &b_db, "currency.json", false, 0);
    assert_eq!(doc["comparison_state"], "complete");
    assert_eq!(doc["before"]["source_currency"], "stale");
    assert_eq!(doc["after"]["source_currency"], "offline");

    // A relative source path cannot be probed from another directory.
    let rel = Tar::default()
        .file(b"f.txt", b"one")
        .write(tmp.path(), "rel.tar");
    let indexed = Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .current_dir(tmp.path())
        .args(["index", "rel.tar"])
        .output()
        .unwrap();
    assert!(indexed.status.success());
    let loaded = diff_input::load_index(&rel.with_extension("tar.db"));
    assert_eq!(
        loaded.snapshot.info.source_currency,
        backupsage::diff::SourceCurrency::NotChecked
    );
    assert_eq!(
        loaded.health.notes.last().unwrap().code,
        NoteCode::SourcePathRelative
    );
}

#[test]
fn contradictory_index_rows_exit_1() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    let bad = altered_copy(
        &after,
        "row-zero.db",
        "INSERT INTO files (id, path, entry_type, kind, flags) VALUES (0, 'zero', 'file', 'empty', 0);",
    );
    let out = diff(&before, &bad, true);
    assert_eq!(code(&out), 1);
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot compare these indexes"));
}

#[test]
fn malformed_hash_makes_the_index_unavailable_not_trusted() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, _) = clean_corpus(tmp.path());
    let bad = altered_copy(
        &before,
        "short-hash.db",
        "UPDATE files SET content_hash = x'00' WHERE id = 1;",
    );
    let loaded = diff_input::load_index(&bad);
    assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
    assert!(loaded.snapshot.entries.is_empty());
    assert_eq!(loaded.health.notes[0].code, NoteCode::IndexUnreadable);
    assert!(loaded.health.notes[0]
        .detail
        .contains("malformed content_hash"));
}

#[test]
fn fixture_directory_holds_exactly_the_listed_fixtures() {
    let mut present: Vec<String> = fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    present.sort();
    assert_eq!(present, FIXTURES);
}

// ── Carry-forward 1: say why moves vanished ─────────────────────────────────

#[test]
fn unparsed_pax_row_that_suppresses_moves_is_named_as_the_cause() {
    let tmp = tempfile::tempdir().unwrap();
    let before = Tar::default()
        .file(b"old-name", b"unique renamed bytes")
        .file(b"keep.txt", b"kept")
        .write(tmp.path(), "p-before.tar");
    let control = Tar::default()
        .file(b"new-name", b"unique renamed bytes")
        .file(b"keep.txt", b"kept")
        .write(tmp.path(), "p-control.tar");
    // Legal pax values containing newlines (xattrs, names) produce the same
    // unparsed segments as this garbage body.
    let after = Tar::default()
        .file(b"new-name", b"unique renamed bytes")
        .file(b"keep.txt", b"kept")
        .pax(b"this is not a pax record at all")
        .file(b"xattred.txt", b"xattr content")
        .write(tmp.path(), "p-after.tar");
    let (before, control, after) = (index(&before), index(&control), index(&after));

    let doc = json(&diff(&before, &control, true));
    assert_eq!(doc["summary"]["moved"], 1);
    assert_eq!(doc["move_inference"]["enabled"], true);
    assert_eq!(doc["move_inference"]["blockers"], serde_json::json!([]));

    let doc = json(&diff(&before, &after, true));
    assert_eq!(doc["summary"]["moved"], 0);
    assert_eq!(doc["move_inference"]["enabled"], false);
    assert_eq!(
        doc["move_inference"]["blockers"],
        serde_json::json!([{"side": "after", "cause": "pax_unparsed", "rows": 1}])
    );
    let text = stdout(&diff(&before, &after, false));
    assert!(text.contains("moves: not inferred"), "{text}");
    assert!(
        text.contains("after: 1 row with unparsed pax metadata")
            && text.contains("legal xattrs or names containing newlines"),
        "{text}"
    );

    // The library view agrees with the rendered one.
    let (b, a, _) = diff_input::diff_indexes(&before, &after).unwrap();
    assert_eq!(
        diff_input::MoveInference::of(&b.snapshot, &a.snapshot).blockers,
        [MoveBlocker {
            side: Side::After,
            cause: MoveBlockerCause::PaxUnparsed,
            rows: Some(1),
        }]
    );
}

// ── Carry-forward 4: the loader never supplies a PAX-sparse hash ────────────

#[test]
fn unsupported_pax_sparse_row_never_gets_a_hash_from_the_loader() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = links_corpus(tmp.path());
    let loaded = diff_input::load_index(&after);
    let rows: Vec<_> = loaded
        .snapshot
        .entries
        .iter()
        .filter(|e| e.path == b"data/file.bin")
        .collect();
    assert_eq!(rows.len(), 2);
    // The earlier regular file keeps its own hash; the effective PAX-sparse
    // row that shadows it has none, and must not borrow one.
    assert!(rows[0].content_hash.is_some() && rows[0].flags & flags::SPARSE == 0);
    assert!(rows[1].file_id > rows[0].file_id);
    assert_ne!(rows[1].flags & flags::SPARSE, 0);
    assert_eq!(rows[1].content_hash, None);

    // Borrowing the shadowed hash would read as byte-identical here.
    let doc = json(&diff(&before, &after, true));
    let change = change_for(&doc, "data/file.bin");
    assert_eq!(change["kind"], "inconclusive");
    assert_eq!(change["reason"], "missing_content_evidence");
}

#[test]
fn real_pax_sparse_rows_load_without_hashes_and_old_gnu_keeps_its_logical_hash() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sparse");
    let expected: Value =
        serde_json::from_slice(&fs::read(fixtures.join("expected.json")).unwrap()).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    for (name, pax) in [
        ("sparse-pax00.tar", true),
        ("sparse-pax01.tar", true),
        ("sparse-pax10.tar", true),
        ("sparse-oldgnu.tar", false),
    ] {
        let archive = tmp.path().join(name);
        fs::copy(fixtures.join(name), &archive).unwrap();
        let db = index(&archive);
        let loaded = diff_input::load_index(&db);
        let sparse: Vec<_> = loaded
            .snapshot
            .entries
            .iter()
            .filter(|e| e.flags & flags::SPARSE != 0)
            .collect();
        assert_eq!(sparse.len(), 1, "{name}");
        if pax {
            assert_eq!(sparse[0].content_hash, None, "{name}");
        } else {
            assert_eq!(
                to_hex(&sparse[0].content_hash.expect("old-GNU logical hash")),
                expected["logical_blake3"].as_str().unwrap()
            );
            // A supported sparse file compares like any regular file.
            let doc = json(&diff(&db, &db, true));
            assert_eq!(doc["comparison_state"], "complete");
        }
    }
}

// ── Read-only: the index files never change ─────────────────────────────────

/// Everything observable about each entry of a directory, and about the
/// directory itself ("."): size, bytes, inode identity, mtime and ctime.
type TreeState = BTreeMap<String, (u64, String, u64, i64, i64, i64, i64)>;

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

/// Name exactly which entries changed, appeared or vanished.
fn assert_same_tree(expected: &TreeState, actual: &TreeState, context: &str) {
    let changed: Vec<_> = expected
        .keys()
        .chain(actual.keys())
        .collect::<std::collections::BTreeSet<_>>()
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

#[test]
fn readonly_open_is_not_writable_and_creates_no_sidecar() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, _) = clean_corpus(tmp.path());
    let state = tree_state(tmp.path());
    let conn = diff_input::open_index_readonly(&before).unwrap();
    assert!(
        conn.is_readonly(rusqlite::MAIN_DB).unwrap(),
        "opened writable"
    );
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();
    assert!(rows > 0);
    assert!(conn.execute_batch("CREATE TABLE zz (x)").is_err());
    assert!(conn
        .execute_batch("UPDATE meta SET value = 'x' WHERE key = 'source'")
        .is_err());
    drop(conn);
    assert_same_tree(
        &state,
        &tree_state(tmp.path()),
        "opening changed the directory",
    );

    // A WAL-mode index could only be read by creating -shm/-wal beside it,
    // so it is refused before SQLite ever opens it.
    let wal_header = wal_header_copy(&before);
    let state = tree_state(tmp.path());
    let refused = diff_input::open_index_readonly(&wal_header).map(|_| ());
    assert_eq!(refused.unwrap_err().code, NoteCode::WalModeIndex);
    assert_same_tree(
        &state,
        &tree_state(tmp.path()),
        "refusing a WAL index changed the directory",
    );
}

#[test]
fn diff_never_modifies_or_creates_anything_beside_its_inputs() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    let linked = altered_copy(&before, "linked.db", "SELECT 1;");
    let alias = tmp.path().join("alias.db");
    fs::hard_link(&linked, &alias).unwrap();
    let inputs = [
        before.clone(),
        alias,
        wal_header_copy(&before),
        altered_copy(
            &before,
            "partial.db",
            "UPDATE meta SET value = '0' WHERE key = 'completed';",
        ),
        v2_index(tmp.path()),
        hot_journal_copy(&before),
        pending_wal_copy(&before),
        tmp.path().join("missing.db"),
        tmp.path().join("before.tar"),
    ];
    let state = tree_state(tmp.path());
    for input in &inputs {
        for (x, y) in [(input, &after), (&after, input), (input, input)] {
            for json in [true, false] {
                let out = diff(x, y, json);
                assert!(
                    matches!(code(&out), 0 | 2),
                    "{}: {}",
                    input.display(),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
        assert_same_tree(
            &state,
            &tree_state(tmp.path()),
            &format!("diff changed something while reading {}", input.display()),
        );
    }
    assert!(!tmp.path().join("missing.db").exists());
}

#[test]
fn pending_journal_or_wal_is_unavailable_and_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    for (db, sidecar) in [
        (hot_journal_copy(&before), "-journal"),
        (pending_wal_copy(&before), "-wal"),
    ] {
        let loaded = diff_input::load_index(&db);
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Unavailable,
            "{}",
            db.display()
        );
        assert!(loaded.snapshot.entries.is_empty());
        assert_eq!(loaded.health.notes[0].code, NoteCode::PendingJournal);
        assert!(loaded.health.notes[0].detail.contains(sidecar));
        let doc = json(&diff(&db, &after, true));
        assert_eq!(doc["comparison_state"], "unavailable");
        assert_eq!(doc["summary"]["added"], 0);

        // Reached through a symlink, the sidecar sits beside the target.
        let link = tmp.path().join(format!("link-to{sidecar}.db"));
        std::os::unix::fs::symlink(&db, &link).unwrap();
        let loaded = diff_input::load_index(&link);
        assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
        assert_eq!(loaded.health.notes[0].code, NoteCode::PendingJournal);
    }

    let wal_header = wal_header_copy(&before);
    let loaded = diff_input::load_index(&wal_header);
    assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
    assert_eq!(loaded.health.notes[0].code, NoteCode::WalModeIndex);
}

// ── Consistency: a mixed snapshot is never called complete ──────────────────

/// The clean corpus's before index plus 4,000 more rows, so that its `files`
/// table spans many pages and a read can be interrupted between them.
fn bulk_index(dir: &Path) -> PathBuf {
    let (before, _) = clean_corpus(dir);
    altered_copy(
        &before,
        "bulk.db",
        "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM r WHERE i < 4000)
         INSERT INTO files (path, entry_type, kind, size, mtime_unix, mode, content_hash, flags)
         SELECT 'bulk/' || i, 'file', 'binary', 32, 1700000001, 420, randomblob(32), 0 FROM r;",
    )
}

/// How many rows carry the original mtime and how many the bumped one.
fn old_and_new(loaded: &diff_input::LoadedIndex) -> (usize, usize) {
    let old = |e: &&backupsage::diff::Entry| e.mtime_unix == Some(MTIME as i64);
    let new = |e: &&backupsage::diff::Entry| e.mtime_unix == Some(MTIME as i64 + 1);
    let rows = &loaded.snapshot.entries;
    (
        rows.iter().filter(old).count(),
        rows.iter().filter(new).count(),
    )
}

const BUMP_MTIMES: &str = "UPDATE files SET mtime_unix = mtime_unix + 1;";

#[test]
fn sqlite_writer_cannot_commit_in_the_middle_of_a_read() {
    // Between two rows of one statement, and between the metadata and the
    // rows: the read transaction must cover both gaps.
    for point in [ReadPoint::BetweenRows, ReadPoint::BetweenStatements] {
        let tmp = tempfile::tempdir().unwrap();
        let bulk = bulk_index(tmp.path());
        let outcome = std::rc::Rc::new(std::cell::RefCell::new(None));
        let (db, seen) = (bulk.clone(), outcome.clone());
        diff_input::set_mid_read_hook(point, move || {
            let writer = rusqlite::Connection::open(&db).unwrap();
            writer.busy_timeout(std::time::Duration::ZERO).unwrap();
            let result = writer.execute_batch(&format!(
                "BEGIN IMMEDIATE; {BUMP_MTIMES}
                 UPDATE meta SET value = 'rewritten' WHERE key = 'index_uuid'; COMMIT;"
            ));
            let _ = writer.execute_batch("ROLLBACK");
            *seen.borrow_mut() = Some(result.map_err(|e| e.to_string()));
        });
        let loaded = diff_input::load_index(&bulk);
        let writer = outcome.borrow_mut().take().expect("the hook ran mid-read");
        let (old, new) = old_and_new(&loaded);

        // Never a mixed snapshot reported as complete.
        assert!(
            !(old > 0 && new > 0) || loaded.snapshot.info.state != SnapshotState::Complete,
            "{point:?}: mixed snapshot ({old} old, {new} new) reported complete"
        );
        // Concretely: the read transaction's lock refused the commit, so the
        // snapshot is the one from before the write, whole.
        assert!(writer.is_err(), "{point:?}: the writer committed mid-read");
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Complete,
            "{point:?}"
        );
        assert_eq!((old, new), (loaded.snapshot.entries.len(), 0), "{point:?}");
        assert_ne!(
            loaded.snapshot.info.index_uuid.as_deref(),
            Some("rewritten"),
            "{point:?}"
        );
    }
}

#[test]
fn in_place_write_that_ignores_locking_makes_the_index_unavailable() {
    let tmp = tempfile::tempdir().unwrap();
    let bulk = bulk_index(tmp.path());
    let bumped = altered_copy(&bulk, "bumped.db", BUMP_MTIMES);
    let replacement = fs::read(&bumped).unwrap();
    assert_eq!(replacement.len() as u64, fs::metadata(&bulk).unwrap().len());
    let db = bulk.clone();
    diff_input::set_mid_read_hook(ReadPoint::BetweenRows, move || {
        // Same inode, same size, new bytes; no SQLite lock is consulted.
        let mut file = fs::OpenOptions::new().write(true).open(&db).unwrap();
        std::io::Write::write_all(&mut file, &replacement).unwrap();
    });
    let loaded = diff_input::load_index(&bulk);
    assert_eq!(loaded.snapshot.info.state, SnapshotState::Unavailable);
    assert!(loaded.snapshot.entries.is_empty());
    assert_eq!(
        loaded.health.notes[0].code,
        NoteCode::IndexChangedDuringRead
    );
}

#[test]
fn hardlinked_index_is_unavailable_because_its_journal_may_hide_elsewhere() {
    let tmp = tempfile::tempdir().unwrap();
    let (before, after) = clean_corpus(tmp.path());
    // hot.db has a hot -journal; alias.db is the same file under a name
    // with no journal beside it, so nothing at alias.db says it is torn.
    let hot = hot_journal_copy(&before);
    let alias = tmp.path().join("alias.db");
    fs::hard_link(&hot, &alias).unwrap();
    // The WAL case from review: committed frames beside the real name only.
    let pending = pending_wal_copy(&before);
    let wal_alias = tmp.path().join("wal-alias.db");
    fs::hard_link(&pending, &wal_alias).unwrap();
    // A clean index with a second name is refused too: the check cannot see
    // what the other name's directory holds.
    let clean = altered_copy(&before, "clean.db", "SELECT 1;");
    let clean_alias = tmp.path().join("clean-alias.db");
    fs::hard_link(&clean, &clean_alias).unwrap();

    for db in [&alias, &wal_alias, &clean_alias] {
        let loaded = diff_input::load_index(db);
        assert_eq!(
            loaded.snapshot.info.state,
            SnapshotState::Unavailable,
            "{}",
            db.display()
        );
        assert!(loaded.snapshot.entries.is_empty());
        assert_eq!(
            loaded.health.notes[0].code,
            NoteCode::IndexMultiplyLinked,
            "{}",
            db.display()
        );
        let out = diff(db, &after, true);
        assert_eq!(code(&out), 2);
        let doc = json(&out);
        assert_eq!(doc["comparison_state"], "unavailable");
        assert_eq!(summary(&doc)[..6], [0; 6], "classified through an alias");
    }
}
