//! `backupsage coverage` end to end (#98): versioned JSON and terminal
//! fixtures, explicit ordering, totals from emitted rows, scope filters,
//! the sanitizer, the `-o` output-safety boundary and the 0/1/2 exit codes.
//! The engines are covered by tests/coverage.rs, tests/floors.rs and
//! tests/coverage_input.rs.
//!
//! Fixtures under tests/fixtures/coverage_cli/ are compared byte for byte
//! after replacing the only run-varying value, the temp directory (as text
//! and as hex). Regenerate deliberately with:
//!   BACKUPSAGE_BLESS=1 cargo test --test coverage_cli

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, UNIX_EPOCH};

use backupsage::coverage_input::{self, LoadedSource};
use backupsage::coverage_report::{self, CoverageScope};
use backupsage::floors::{self, FloorParams};
use backupsage::indexer::{self, IndexOptions};
use backupsage::report::to_hex;
use serde_json::Value;

const MTIME: u64 = 1_700_000_001;
const SHARED: &[u8] = b"shared bytes held by a, b and c";
const RAW_SHARED: &[u8] = b"raw-named bytes held by a and c";

// ── Corpus construction ─────────────────────────────────────────────────────

/// Raw tar bytes, member by member, so a corpus can repeat paths, carry
/// non-UTF-8 names and links.
#[derive(Default)]
struct Tar(Vec<u8>);

impl Tar {
    fn header(&mut self, path: &[u8], kind: tar::EntryType, size: u64, link: Option<&[u8]>) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(kind);
        h.set_path(OsStr::from_bytes(path)).unwrap();
        if let Some(target) = link {
            h.set_link_name(OsStr::from_bytes(target)).unwrap();
        }
        h.set_size(size);
        h.set_mode(0o644);
        h.set_mtime(MTIME);
        h.set_cksum();
        self.0.extend_from_slice(h.as_bytes());
    }

    fn file(mut self, path: &[u8], data: &[u8]) -> Self {
        self.header(path, tar::EntryType::Regular, data.len() as u64, None);
        self.0.extend_from_slice(data);
        while !self.0.len().is_multiple_of(512) {
            self.0.push(0);
        }
        self
    }

    /// A pax extended header applying to the next member.
    fn pax(mut self, body: &[u8]) -> Self {
        self.header(
            b"paxheader/next",
            tar::EntryType::XHeader,
            body.len() as u64,
            None,
        );
        self.0.extend_from_slice(body);
        while !self.0.len().is_multiple_of(512) {
            self.0.push(0);
        }
        self
    }

    fn link(mut self, kind: tar::EntryType, path: &[u8], target: &[u8]) -> Self {
        self.header(path, kind, 0, Some(target));
        self
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

struct Corpus {
    a_tar: PathBuf,
    b_tar: PathBuf,
    c_dir: PathBuf,
    a: PathBuf,
    b: PathBuf,
    c: PathBuf,
}

/// Two tars and a directory. Shared content in all three, content only in
/// one, a shadowed path, a hardlink alias, a symlink, empty content and
/// non-UTF-8 names (one of them shared across sources).
fn corpus(dir: &Path) -> Corpus {
    let a_tar = Tar::default()
        .file(b"shared.txt", SHARED)
        .file(b"a-only.txt", b"only in a")
        .file(b"dup.txt", b"first, shadowed")
        .file(b"dup.txt", b"second, effective")
        .link(tar::EntryType::Link, b"alias-of-shared", b"shared.txt")
        .link(tar::EntryType::Symlink, b"sym", b"shared.txt")
        .file(b"raw-\xff", RAW_SHARED)
        .file(b"empty.txt", b"")
        .link(tar::EntryType::Link, b"alias-of-empty", b"empty.txt")
        .write(dir, "a.tar");
    let b_tar = Tar::default()
        .file(b"copy/shared.txt", SHARED)
        .file(b"b-\xfe.bin", b"only in b, raw name")
        .link(tar::EntryType::Symlink, b"a-sym", b"copy/shared.txt")
        .write(dir, "b.tar");
    let c_dir = dir.join("c-dir");
    write_file(&c_dir.join("deep/shared.txt"), SHARED);
    write_file(&c_dir.join(OsStr::from_bytes(b"raw-\xff")), RAW_SHARED);
    let (a, b, c) = (index(&a_tar), index(&b_tar), index(&c_dir));
    Corpus {
        a_tar,
        b_tar,
        c_dir,
        a,
        b,
        c,
    }
}

// ── Running the binary ──────────────────────────────────────────────────────

fn run<S: AsRef<OsStr>>(args: &[S]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .args(args)
        .output()
        .expect("binary runs")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("no signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("coverage output is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A master holding `dbs`, registered one at a time in the given order.
fn master_with(dir: &Path, name: &str, dbs: &[&Path]) -> PathBuf {
    let master = dir.join(name);
    for db in dbs {
        let out = run(&[
            OsStr::new("--master"),
            master.as_os_str(),
            OsStr::new("master"),
            OsStr::new("add"),
            db.as_os_str(),
        ]);
        assert_eq!(code(&out), 0, "master add: {}", stderr(&out));
    }
    master
}

fn coverage_master(master: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        OsStr::new("--master"),
        master.as_os_str(),
        OsStr::new("coverage"),
    ];
    args.extend(extra.iter().map(OsStr::new));
    run(&args)
}

fn coverage_dbs(dbs: &[&Path], extra: &[&str]) -> Output {
    let mut args = vec![OsStr::new("coverage")];
    for db in dbs {
        args.push(OsStr::new("--db"));
        args.push(db.as_os_str());
    }
    args.extend(extra.iter().map(OsStr::new));
    run(&args)
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

// ── Fixtures ────────────────────────────────────────────────────────────────

/// Replace the temp directory (text and hex spellings).
fn normalize(raw: &str, tmp: &Path) -> String {
    let mut s = raw.to_owned();
    let canonical = tmp.canonicalize().unwrap();
    for spelling in [canonical.as_path(), tmp] {
        s = s.replace(&to_hex(spelling.as_os_str().as_bytes()), "<TMP-HEX>");
        s = s.replace(spelling.to_str().unwrap(), "<TMP>");
    }
    s
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/coverage_cli")
}

const FIXTURES: &[&str] = &[
    "clean.json",
    "clean.txt",
    "clean_permuted.json",
    "filtered.json",
    "inconclusive.json",
    "inconclusive.txt",
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
        "coverage CLI contract drifted: {name}\n--- expected\n{expected}\n--- actual\n{actual}"
    );
}

#[test]
fn fixture_directory_holds_exactly_the_listed_fixtures() {
    let mut on_disk: Vec<String> = fs::read_dir(fixture_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = FIXTURES.iter().map(|s| s.to_string()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

/// Every list sorted on its documented key.
fn assert_ordered(doc: &Value, what: &str) {
    let arr = |v: &Value| v.as_array().unwrap().clone();
    let row_key = |r: &Value| {
        (
            r["source_id"].as_i64().unwrap(),
            // Hex of bytes sorts exactly like the bytes.
            r["path_bytes"].as_str().unwrap().to_owned(),
            r["file_id"].as_i64().unwrap(),
        )
    };
    fn sorted<K: Ord + Clone + std::fmt::Debug>(keys: Vec<K>, list: &str, what: &str) {
        let mut want = keys.clone();
        want.sort();
        assert_eq!(keys, want, "{what}: {list} out of order");
    }
    sorted(
        arr(&doc["sources"])
            .iter()
            .map(|s| s["source_id"].as_i64().unwrap())
            .collect(),
        "sources",
        what,
    );
    for list in ["groups", "excluded_groups"] {
        let groups = arr(&doc[list]);
        sorted(
            groups
                .iter()
                .map(|g| g["content_hash"].as_str().unwrap().to_owned())
                .collect(),
            list,
            what,
        );
        for g in &groups {
            sorted(
                arr(&g["copies"]).iter().map(row_key).collect(),
                "copies",
                what,
            );
            sorted(
                arr(&g["aliases"]).iter().map(row_key).collect(),
                "aliases",
                what,
            );
            if let Some(p) = g.get("presence") {
                sorted(
                    arr(p)
                        .iter()
                        .map(|p| p["source_id"].as_i64().unwrap())
                        .collect(),
                    "presence",
                    what,
                );
            }
        }
    }
    for list in ["unknown_content", "excluded_rows"] {
        sorted(arr(&doc[list]).iter().map(row_key).collect(), list, what);
    }
}

/// Every total recounted from the emitted rows.
fn assert_totals(doc: &Value, what: &str) {
    let arr = |k: &str| doc[k].as_array().unwrap().clone();
    let groups = arr("groups");
    let excluded = arr("excluded_groups");
    let verdict = |v: &str| groups.iter().filter(|g| g["verdict"] == v).count();
    let rows = |r: &str| {
        arr("excluded_rows")
            .iter()
            .filter(|e| e["reason"] == r)
            .count()
    };
    let n = |v: &Value| v.as_u64().unwrap() as usize;
    let s = &doc["summary"];
    let expect = [
        ("min_copies", n(&doc["params"]["min_copies"])),
        ("sources", arr("sources").len()),
        (
            "sources_degraded",
            arr("sources")
                .iter()
                .filter(|s| s["evidence"] != "complete" || s["status"] != "ok")
                .count(),
        ),
        ("groups", groups.len()),
        ("meets_floor", verdict("meets_floor")),
        ("below_floor", verdict("below_floor")),
        ("inconclusive", verdict("inconclusive")),
        (
            "only_copy",
            groups.iter().filter(|g| g["only_copy"] == true).count(),
        ),
        (
            "protected_replicas",
            groups.iter().map(|g| n(&g["protected_replicas"])).sum(),
        ),
        ("excluded_groups", excluded.len()),
        ("unknown_content_rows", arr("unknown_content").len()),
        ("shadowed_rows", rows("shadowed")),
        ("symlink_rows", rows("symlink")),
        ("unmatched_hardlink_rows", rows("unmatched_hardlink")),
        (
            "hardlink_aliases",
            groups
                .iter()
                .chain(&excluded)
                .map(|g| g["aliases"].as_array().unwrap().len())
                .sum(),
        ),
    ];
    for (key, want) in expect {
        assert_eq!(n(&s[key]), want, "{what}: summary.{key}");
    }
    assert_eq!(
        s.as_object().unwrap().len(),
        expect.len(),
        "{what}: a summary field is not recounted here"
    );
    let complete = n(&s["sources_degraded"]) == 0
        && n(&s["inconclusive"]) == 0
        && n(&s["unknown_content_rows"]) == 0;
    assert_eq!(
        doc["coverage_state"],
        if complete { "complete" } else { "inconclusive" },
        "{what}"
    );
}

/// Both renderings of one run: fixtures, exit code, determinism, order and
/// totals.
fn check(
    tmp: &Path,
    runner: &dyn Fn(&[&str]) -> Output,
    flags: &[&str],
    fixture: &str,
    text_fixture: bool,
    exit: i32,
) -> Value {
    let with_json: Vec<&str> = flags.iter().copied().chain(["--json"]).collect();
    let out = runner(&with_json);
    assert_eq!(code(&out), exit, "{fixture}: {}", stderr(&out));
    assert_eq!(
        runner(&with_json).stdout,
        out.stdout,
        "{fixture}: output differs between identical runs"
    );
    golden(fixture, &normalize(&stdout(&out), tmp));
    let doc = json(&out);
    assert_eq!(doc["version"], 1);
    assert_ordered(&doc, fixture);
    assert_totals(&doc, fixture);

    let text = runner(flags);
    assert_eq!(code(&text), exit, "{fixture} (text)");
    if text_fixture {
        golden(
            &fixture.replace(".json", ".txt"),
            &normalize(&stdout(&text), tmp),
        );
    }
    doc
}

fn group_by_path<'a>(doc: &'a Value, path: &str) -> &'a Value {
    doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| {
            g["copies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["path"] == path)
        })
        .unwrap_or_else(|| panic!("no group holds {path}"))
}

fn verdict_of(doc: &Value, path: &str) -> String {
    group_by_path(doc, path)["verdict"]
        .as_str()
        .unwrap()
        .to_owned()
}

// ── Fixtures and exit codes ─────────────────────────────────────────────────

#[test]
fn clean_master_classifies_every_group_and_exits_0() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let master = master_with(tmp.path(), "master.db", &[&c.a, &c.b, &c.c]);
    let runner = |f: &[&str]| coverage_master(&master, f);
    let doc = check(tmp.path(), &runner, &[], "clean.json", true, 0);

    assert_eq!(doc["coverage_state"], "complete");
    assert_eq!(verdict_of(&doc, "shared.txt"), "meets_floor");
    assert_eq!(group_by_path(&doc, "shared.txt")["trusted_replicas"], 3);
    assert_eq!(verdict_of(&doc, "raw-\u{fffd}"), "meets_floor");
    for only in ["a-only.txt", "dup.txt", "b-\u{fffd}.bin"] {
        assert_eq!(verdict_of(&doc, only), "below_floor", "{only}");
        assert_eq!(group_by_path(&doc, only)["only_copy"], true, "{only}");
    }
    // The shadowed dup.txt, the symlink and the alias are listed, not counted.
    let reasons: Vec<&str> = doc["excluded_rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["reason"].as_str().unwrap())
        .collect();
    assert_eq!(reasons, ["shadowed", "symlink", "symlink"]);
    assert_eq!(
        group_by_path(&doc, "shared.txt")["aliases"][0]["path"],
        "alias-of-shared"
    );
    assert_eq!(doc["excluded_groups"][0]["reason"], "empty_content");
    assert_eq!(
        doc["excluded_groups"][0]["aliases"][0]["path"],
        "alias-of-empty"
    );
    // Non-UTF-8 names keep their exact bytes.
    let b_only = group_by_path(&doc, "b-\u{fffd}.bin");
    assert_eq!(b_only["copies"][0]["path_bytes"], to_hex(b"b-\xfe.bin"));
}

/// Relabel a report by source label so two registrations compare.
fn by_label(doc: &Value) -> Value {
    let labels: BTreeMap<i64, String> = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["source_id"].as_i64().unwrap(),
                s["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let rows = |list: &Value| {
        let mut out: Vec<Value> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                let mut r = r.clone();
                let id = r["source_id"].as_i64().unwrap();
                r["source_id"] = Value::String(labels[&id].clone());
                r
            })
            .collect();
        out.sort_by_key(|r| r.to_string());
        Value::Array(out)
    };
    let groups = |list: &Value| {
        Value::Array(
            list.as_array()
                .unwrap()
                .iter()
                .map(|g| {
                    let mut g = g.clone();
                    for k in ["copies", "aliases", "presence"] {
                        if g.get(k).is_some() {
                            g[k] = rows(&g[k]);
                        }
                    }
                    g
                })
                .collect(),
        )
    };
    serde_json::json!({
        "sources": rows(&doc["sources"]),
        "groups": groups(&doc["groups"]),
        "excluded_groups": groups(&doc["excluded_groups"]),
        "unknown_content": rows(&doc["unknown_content"]),
        "excluded_rows": rows(&doc["excluded_rows"]),
        "summary": doc["summary"],
        "coverage_state": doc["coverage_state"],
    })
}

#[test]
fn registration_order_changes_only_source_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let first = master_with(tmp.path(), "master.db", &[&c.a, &c.b, &c.c]);
    let permuted = master_with(tmp.path(), "permuted.db", &[&c.c, &c.a, &c.b]);
    let runner = |f: &[&str]| coverage_master(&permuted, f);
    let doc = check(tmp.path(), &runner, &[], "clean_permuted.json", false, 0);
    // Ids follow registration; everything else is the same report.
    let ids: Vec<(i64, String)> = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["source_id"].as_i64().unwrap(),
                s["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        ids,
        [
            (1, "c-dir".to_owned()),
            (2, "a.tar".to_owned()),
            (3, "b.tar".to_owned())
        ]
    );
    let base = json(&coverage_master(&first, &["--json"]));
    assert_eq!(by_label(&doc), by_label(&base));
}

#[test]
fn unreachable_and_unavailable_sources_are_inconclusive_and_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let d_tar = Tar::default()
        .file(b"d.txt", b"only in d")
        .write(tmp.path(), "d.tar");
    let d = index(&d_tar);
    // Unparsed pax metadata: the hash may not cover the logical file, and
    // the row is as long as a-only.txt, so it could be that content.
    let e_tar = Tar::default()
        .pax(b"this is not a pax record at all")
        .file(b"xattred.txt", b"nine byte")
        .write(tmp.path(), "e.tar");
    let e = index(&e_tar);
    let master = master_with(tmp.path(), "master.db", &[&c.a, &c.b, &c.c, &d, &e]);
    // The harness unplugs b and loses d's index.
    fs::remove_file(&c.b_tar).unwrap();
    fs::remove_file(&d).unwrap();
    let runner = |f: &[&str]| coverage_master(&master, f);
    let doc = check(tmp.path(), &runner, &[], "inconclusive.json", true, 2);

    assert_eq!(doc["coverage_state"], "inconclusive");
    let status: Vec<(&str, &str)> = doc["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["evidence"].as_str().unwrap(),
                s["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        status,
        [
            ("complete", "ok"),
            ("unreachable", "archive-missing"),
            ("complete", "ok"),
            ("unavailable", "db-missing"),
            ("complete", "ok"),
        ]
    );
    let unknown = &doc["unknown_content"][0];
    assert_eq!(
        (&unknown["path"], &unknown["reason"]),
        (&Value::from("xattred.txt"), &Value::from("pax_unparsed"))
    );
    let a_only = group_by_path(&doc, "a-only.txt");
    assert_eq!(a_only["presence"][4]["reason"], "unhashed_rows_may_match");
    assert_eq!(a_only["presence"][4]["unhashed_rows"], 1);
    // Content trusted twice elsewhere still meets the floor; nothing is
    // ever below it while a source is unknown.
    assert_eq!(verdict_of(&doc, "shared.txt"), "meets_floor");
    for p in ["a-only.txt", "b-\u{fffd}.bin"] {
        assert_eq!(verdict_of(&doc, p), "inconclusive", "{p}");
        assert_eq!(group_by_path(&doc, p)["only_copy"], false, "{p}");
    }
    assert_eq!(doc["summary"]["below_floor"], 0);
}

#[test]
fn a_degraded_source_exits_2_even_when_every_group_meets_the_floor() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Tar::default()
        .file(b"x.txt", SHARED)
        .write(tmp.path(), "a.tar");
    let b = Tar::default()
        .file(b"x.txt", SHARED)
        .write(tmp.path(), "b.tar");
    let (a_db, b_db) = (index(&a), index(&b));
    // Same bytes, new mtime: b's index is stale.
    fs::File::options()
        .write(true)
        .open(&b)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(MTIME + 50))
        .unwrap();
    let out = coverage_dbs(&[&a_db, &b_db], &["--min-copies", "1", "--json"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let doc = json(&out);
    assert_eq!(doc["sources"][1]["status"], "stale-index");
    assert_eq!(doc["summary"]["meets_floor"], 1);
    assert_eq!(doc["summary"]["inconclusive"], 0);
    assert_eq!(doc["summary"]["sources_degraded"], 1);
    assert_eq!(doc["coverage_state"], "inconclusive");
    // One trusted copy meets a floor of 1 and is still the only copy.
    assert_eq!(doc["groups"][0]["only_copy"], true);
    assert_totals(&doc, "degraded");
}

#[test]
fn unknown_content_alone_makes_the_result_inconclusive() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Tar::default()
        .file(b"known.txt", SHARED)
        .pax(b"this is not a pax record at all")
        .file(b"unknown.txt", b"bytes whose hash proves nothing")
        .write(tmp.path(), "e.tar");
    let e_db = index(&e);
    let out = coverage_dbs(&[&e_db], &["--min-copies", "1", "--json"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    let doc = json(&out);
    // Every source is complete and ok and every group meets its floor,
    // but a row's content is unknown: it may have no other copy anywhere.
    assert_eq!(doc["summary"]["sources_degraded"], 0);
    assert_eq!(doc["summary"]["meets_floor"], 1);
    assert_eq!(doc["summary"]["inconclusive"], 0);
    assert_eq!(doc["unknown_content"][0]["path"], "unknown.txt");
    assert_eq!(doc["coverage_state"], "inconclusive");
    assert_totals(&doc, "unknown content");
}

#[test]
fn errors_exit_1_and_name_the_problem() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let master = master_with(tmp.path(), "master.db", &[&c.a, &c.b]);
    let cases: Vec<(&str, Output, &str)> = vec![
        (
            "missing master",
            coverage_master(&tmp.path().join("nope.db"), &[]),
            "nope.db",
        ),
        (
            "floor of 0",
            coverage_master(&master, &["--min-copies", "0"]),
            "at least 1",
        ),
        (
            "unknown --archive",
            coverage_master(&master, &["--archive", "zzz"]),
            "--archive 'zzz'",
        ),
        (
            "unknown --protected",
            coverage_master(&master, &["--protected", "zzz"]),
            "--protected 'zzz'",
        ),
        (
            "--protected outside --archive",
            coverage_master(&master, &["--archive", "1", "--protected", "2"]),
            "--protected '2'",
        ),
        (
            "--db given twice",
            coverage_dbs(&[&c.a, &c.a], &[]),
            "more than once",
        ),
    ];
    for (what, out, needle) in cases {
        assert_eq!(code(&out), 1, "{what}: {}", stderr(&out));
        assert!(stdout(&out).is_empty(), "{what}: printed a report");
        assert!(stderr(&out).contains(needle), "{what}: {}", stderr(&out));
    }
    // A master in use is refused and left alone.
    let sidecar = tmp.path().join("master.db-wal");
    fs::write(&sidecar, b"").unwrap();
    let out = coverage_master(&master, &[]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("in use") || stderr(&out).contains("-wal"));
}

// ── Scope filters ───────────────────────────────────────────────────────────

#[test]
fn filters_choose_content_and_every_copy_still_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let runner = |f: &[&str]| coverage_dbs(&[&c.a, &c.b, &c.c], f);
    let doc = check(
        tmp.path(),
        &runner,
        &[
            "--ext",
            "txt",
            "--protected",
            "c-dir.db",
            "--min-size",
            "10",
        ],
        "filtered.json",
        false,
        0,
    );
    // shared.txt, a-only.txt and dup.txt match; the raw-named content and
    // b's .bin do not, and their groups are not reported.
    let paths: Vec<&str> = doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["copies"][0]["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 2, "{paths:?}");
    let shared = group_by_path(&doc, "shared.txt");
    assert_eq!(shared["copies"].as_array().unwrap().len(), 3);
    assert_eq!(shared["trusted_replicas"], 3);
    assert_eq!(shared["protected_replicas"], 1);
    // a-only.txt (9 bytes) is below --min-size: listed, not judged.
    assert_eq!(doc["excluded_groups"][0]["reason"], "below_min_size");
    // The shadowed dup.txt row matches; the symlink `sym` does not.
    assert_eq!(doc["excluded_rows"].as_array().unwrap().len(), 1);
}

/// Content whose only matching path is on the second source is still
/// reported, with the copy whose path does not match counted too.
#[test]
fn a_path_filter_never_removes_a_copy_from_the_count() {
    let tmp = tempfile::tempdir().unwrap();
    let a = Tar::default()
        .file(b"x.bin", b"picture bytes")
        .write(tmp.path(), "a.tar");
    let b = Tar::default()
        .file(b"x.JpG", b"picture bytes")
        .write(tmp.path(), "b.tar");
    let (a_db, b_db) = (index(&a), index(&b));
    for flags in [&["--ext", "jpg"][..], &["--path-glob", "*.JpG"][..]] {
        let mut all = flags.to_vec();
        all.push("--json");
        let doc = json(&coverage_dbs(&[&a_db, &b_db], &all));
        assert_eq!(doc["summary"]["groups"], 1, "{flags:?}");
        let g = &doc["groups"][0];
        assert_eq!(g["verdict"], "meets_floor", "{flags:?}");
        assert_eq!(g["trusted_replicas"], 2, "{flags:?}");
        let copies: Vec<&str> = g["copies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["path"].as_str().unwrap())
            .collect();
        assert_eq!(copies, ["x.bin", "x.JpG"], "{flags:?}");
    }
    // Like dedup: --ext folds ASCII case, GLOB does not.
    let doc = json(&coverage_dbs(
        &[&a_db, &b_db],
        &["--path-glob", "*.jpg", "--json"],
    ));
    assert_eq!(doc["summary"]["groups"], 0);
    // Choosing sources is different: it is the scope of the question.
    let doc = json(&coverage_dbs(
        &[&a_db, &b_db],
        &["--archive", "1", "--json"],
    ));
    assert_eq!(doc["params"]["archives"], serde_json::json!([1]));
    assert_eq!(doc["groups"][0]["verdict"], "below_floor");
    assert_eq!(doc["groups"][0]["only_copy"], true);
}

// ── Ordering owned by the report ────────────────────────────────────────────

fn report_json(sources: &[LoadedSource], reverse: bool) -> String {
    let scope = CoverageScope {
        floor: FloorParams::default(),
        ..CoverageScope::default()
    };
    let loaded = coverage_input::LoadedCoverage {
        sources: sources.to_vec(),
    };
    let mut coverage = loaded.coverage().unwrap();
    let mut floors = floors::evaluate(&coverage, &loaded.statuses(), &[1], &scope.floor).unwrap();
    let mut sources = sources.to_vec();
    if reverse {
        sources.reverse();
        coverage.groups.reverse();
        for g in &mut coverage.groups {
            g.presence.reverse();
        }
        floors.groups.reverse();
        for g in &mut floors.groups {
            g.copies.reverse();
            g.aliases.reverse();
        }
        floors.excluded_groups.reverse();
        for g in &mut floors.excluded_groups {
            g.copies.reverse();
            g.aliases.reverse();
        }
        floors.unknown_content.reverse();
        floors.excluded_rows.reverse();
    }
    coverage_report::build_report("db", &sources, &coverage, &floors, &scope, None, &[1])
        .unwrap()
        .to_json()
        .unwrap()
}

#[test]
fn the_report_sorts_every_list_itself_whatever_order_it_is_handed() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let loaded = coverage_input::load_from_indexes(&[c.a, c.b, c.c]).unwrap();
    let straight = report_json(&loaded.sources, false);
    let doc: Value = serde_json::from_str(&straight).unwrap();
    // The corpus exercises every list, so reversing each one matters.
    for list in ["groups", "excluded_groups", "excluded_rows"] {
        assert!(!doc[list].as_array().unwrap().is_empty(), "{list}");
    }
    assert!(doc["groups"].as_array().unwrap().iter().any(|g| g["copies"]
        .as_array()
        .unwrap()
        .len()
        > 1));
    assert_eq!(report_json(&loaded.sources, true), straight);
    assert_ordered(&doc, "library report");
}

// ── Terminal safety and -o ──────────────────────────────────────────────────

#[test]
fn terminal_text_is_sanitized_and_json_escapes_controls() {
    let tmp = tempfile::tempdir().unwrap();
    let evil = Tar::default()
        .file(b"esc-\x1b[31m-red.txt", b"bytes behind an escape")
        .write(tmp.path(), "evil.tar");
    let db = tmp.path().join("lab\x1bel.db");
    indexer::run_index(&evil, Some(&db), &IndexOptions::default()).unwrap();
    for flags in [&[][..], &["--json"][..]] {
        let out = coverage_dbs(&[&db], flags);
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        assert!(
            !out.stdout.contains(&0x1b),
            "{flags:?}: a raw escape reached the terminal"
        );
    }
    let text = stdout(&coverage_dbs(&[&db], &[]));
    assert!(text.contains("esc-␛[31m-red.txt"), "{text}");
    assert!(text.contains("lab␛el.db"), "{text}");
}

#[test]
fn output_file_goes_through_the_safety_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let c = corpus(tmp.path());
    let dbs = [c.a.as_path(), c.b.as_path(), c.c.as_path()];

    let dest = tmp.path().join("report.json");
    let dest_s = dest.to_str().unwrap();
    let out = coverage_dbs(&dbs, &["--json", "-o", dest_s]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert_eq!(
        fs::read(&dest).unwrap(),
        coverage_dbs(&dbs, &["--json"]).stdout
    );

    // Never overwrites, and the existing file is untouched.
    let out = coverage_dbs(&dbs, &["-o", dest_s]);
    assert_eq!(code(&out), 1);
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert_eq!(
        fs::read(&dest).unwrap(),
        coverage_dbs(&dbs, &["--json"]).stdout
    );

    // Never writes inside a directory source it read.
    let inside = c.c_dir.join("report.txt");
    let out = coverage_dbs(&dbs, &["-o", inside.to_str().unwrap()]);
    assert_eq!(code(&out), 1, "{}", stderr(&out));
    assert!(!inside.exists(), "wrote inside a source directory");

    // An input index and a source archive are refused by identity too,
    // even through another spelling of the path.
    for target in [&c.a, &c.a_tar] {
        let alias = tmp.path().join("sub/../").join(target.file_name().unwrap());
        fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let before = fs::read(target).unwrap();
        let out = coverage_dbs(&dbs, &["-o", alias.to_str().unwrap()]);
        assert_eq!(code(&out), 1);
        assert_eq!(fs::read(target).unwrap(), before);
    }
}
