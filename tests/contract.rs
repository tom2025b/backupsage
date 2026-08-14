//! Golden-fixture contract tests (#55): freeze every public JSON surface
//! and the 0/1/2 exit-code contract against committed fixtures.
//!
//! The five JSON surfaces: `dedup --json`, `search --json`,
//! `search --all --json`, `master list --json`, `master verify --json` —
//! plus completed-with-skips variants of the two that have one, plus the
//! keeper-star chain corpus (#9) pinned in `dedup_chain.json`, plus #71's
//! non-degenerate content-mode corpora (`search_only.json`,
//! `metadata_only_skips.json`) — ten fixtures total.
//! Run-varying fields (temp paths, `indexed_unix`) are normalized before
//! comparison; everything else in the corpus is deterministic by
//! construction (fixed mtimes, seeded PNG bytes, fixed payloads).
//!
//! Regenerate deliberately with:
//!   BACKUPSAGE_BLESS=1 cargo test --test contract
//! The contract is additive-only — a re-bless that renames or removes a
//! field is a breaking change and must not ship (see docs/CONTRACT.md).

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::*;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
}

fn run(args: &[&str]) -> Output {
    bin().args(args).output().expect("binary runs")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("no signal")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Like `common::build_tar_mtime`, but paths are raw bytes so the corpus can
/// carry a non-UTF-8 name (exercises `path_bytes` in JSON output).
fn build_tar_raw(files: &[(&[u8], Vec<u8>)], mtime: u64) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut builder = tar::Builder::new(Vec::new());
    for (path, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        header.set_cksum();
        builder
            .append_data(
                &mut header,
                std::ffi::OsStr::from_bytes(path),
                data.as_slice(),
            )
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// Replace run-varying values so the rest of the document can be compared
/// byte-for-byte against a committed fixture: any string containing the
/// temp dir becomes `<TMP>`-relative, and `indexed_unix` (wall clock at
/// index time) becomes `"<TS>"`.
fn scrub(v: &mut serde_json::Value, tmp: &str) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            if s.contains(tmp) {
                *s = s.replace(tmp, "<TMP>");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| scrub(i, tmp)),
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "indexed_unix" && val.is_number() {
                    *val = Value::String("<TS>".into());
                } else {
                    scrub(val, tmp);
                }
            }
        }
        _ => {}
    }
}

fn normalized(raw: &str, tmp: &Path) -> serde_json::Value {
    let mut v: serde_json::Value = serde_json::from_str(raw)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON: {e}\n{raw}"));
    // The master stores canonicalized db paths (src/master.rs `add`), so on
    // a machine whose temp dir resolves through a symlink the emitted path
    // differs from the tempfile-reported one — scrub both spellings.
    let canonical = tmp.canonicalize().unwrap_or_else(|_| tmp.to_path_buf());
    scrub(&mut v, canonical.to_str().unwrap());
    scrub(&mut v, tmp.to_str().unwrap());
    v
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/contract")
}

fn fixture_path(name: &str) -> PathBuf {
    fixture_dir().join(name)
}

/// Compare against the committed fixture, or (re)write it under
/// BACKUPSAGE_BLESS=1. Semantic JSON equality: a rename, removal, addition
/// or type change anywhere in the document fails the test.
fn assert_matches_fixture(name: &str, actual: serde_json::Value) {
    let path = fixture_path(name);
    let rendered = serde_json::to_string_pretty(&actual).unwrap() + "\n";
    if std::env::var_os("BACKUPSAGE_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, rendered).unwrap();
        return;
    }
    let frozen = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden fixture {} ({e}) — generate it with \
             `BACKUPSAGE_BLESS=1 cargo test --test contract`",
            path.display()
        )
    });
    let expected: serde_json::Value = serde_json::from_str(&frozen)
        .unwrap_or_else(|e| panic!("fixture {} is not valid JSON: {e}", path.display()));
    assert_eq!(
        expected, actual,
        "golden fixture '{name}' drifted.\n--- frozen ---\n{frozen}\n--- actual ---\n{rendered}\n\
         The public JSON contract is additive-only. If this change is deliberate \
         and additive, re-bless with `BACKUPSAGE_BLESS=1 cargo test --test contract` \
         and commit the fixture diff. Renames and removals must not ship."
    );
}

/// Deterministic two-archive corpus: an exact-duplicate trio (one member
/// under a non-UTF-8 path), a near-duplicate PNG pair at hamming distance 2
/// (delta 64 — smaller brightenings leave the DCT hash unchanged), and an
/// EXIF-dated TIFF pair so `exif_unix`/`best_ts_source` freeze non-null.
/// Fixed mtimes; every byte reproducible.
///
/// Group order in the dedup report is fully deterministic since #9: equal
/// primary sort keys tie-break on the smallest member identity in
/// src/dedup.rs, so extending the corpus with equal-reclaimable groups is
/// safe.
fn build_corpus(dir: &Path) -> (PathBuf, PathBuf, String) {
    let payload = b"shared payload magicterm bytes for exact dedup coverage\n".to_vec();
    let tiff = tiff_with_exif_date("2019:06:01 12:00:00");
    let alpha = write_archive(
        dir,
        "alpha.tar",
        &build_tar_raw(
            &[
                (b"docs/report.txt".as_slice(), payload.clone()),
                (b"photos/holiday.png".as_slice(), png_bytes(21, 320, 240)),
                (b"photos/scan.tif".as_slice(), tiff.clone()),
                (
                    b"notes.txt".as_slice(),
                    b"alpha note with alphaword inside\n".to_vec(),
                ),
            ],
            1_600_000_000,
        ),
    );
    let beta = write_archive(
        dir,
        "beta.tar",
        &build_tar_raw(
            &[
                (b"archive/report-final.txt".as_slice(), payload.clone()),
                (
                    b"export/holiday-bright.png".as_slice(),
                    png_bytes_brightened(21, 320, 240, 64),
                ),
                (b"old/scan-copy.tif".as_slice(), tiff),
                (b"exports/caf\xe9-menu.txt".as_slice(), payload),
            ],
            1_700_000_000,
        ),
    );
    let master = dir.join("master.db");
    let master_arg = master.to_str().unwrap().to_string();

    for a in [&alpha, &beta] {
        let out = run(&["index", a.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "index failed: {}", stdout(&out));
    }
    let out = run(&[
        "--master",
        &master_arg,
        "master",
        "add",
        alpha.with_extension("tar.db").to_str().unwrap(),
        beta.with_extension("tar.db").to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "master add failed");
    (alpha, beta, master_arg)
}

#[test]
fn json_surfaces_match_golden_fixtures() {
    let dir = tempfile::tempdir().unwrap();
    let (_alpha, beta, master_arg) = build_corpus(dir.path());
    let tmp = dir.path();

    // 1. search --json (single index, beta: the non-UTF-8 hit freezes
    // path_bytes on this surface too)
    let out = run(&[
        "search",
        "magicterm",
        "-i",
        beta.with_extension("tar.db").to_str().unwrap(),
        "--snippets",
        "--json",
    ]);
    assert_eq!(code(&out), 0);
    assert_matches_fixture("search.json", normalized(&stdout(&out), tmp));

    // 2. search --all --json (federated; the non-UTF-8 hit carries path_bytes)
    let out = run(&[
        "--master",
        &master_arg,
        "search",
        "magicterm",
        "--all",
        "--snippets",
        "--json",
    ]);
    assert_eq!(code(&out), 0);
    assert_matches_fixture("search_all.json", normalized(&stdout(&out), tmp));

    // 3. master list --json
    let out = run(&["--master", &master_arg, "master", "list", "--json"]);
    assert_eq!(code(&out), 0);
    assert_matches_fixture("master_list.json", normalized(&stdout(&out), tmp));

    // 4. master verify --json (all sources untouched → ok, exit 0)
    let out = run(&["--master", &master_arg, "master", "verify", "--json"]);
    assert_eq!(code(&out), 0);
    assert_matches_fixture("master_verify.json", normalized(&stdout(&out), tmp));

    // 5. dedup --json (exact trio incl. path_bytes member + near pair)
    let out = run(&["--master", &master_arg, "dedup", "--json"]);
    assert_eq!(code(&out), 0);
    let report = normalized(&stdout(&out), tmp);
    assert_eq!(
        report["version"], 1,
        "report version is part of the contract"
    );
    assert_matches_fixture("dedup.json", report);

    // Degraded phase — beta's index goes missing (the harness removes its
    // own generated .db; no archive is touched). Freezes the
    // completed-with-skips variants: populated `skipped[]` on search --all
    // and populated `archives_offline` on the dedup report.
    std::fs::remove_file(beta.with_extension("tar.db")).unwrap();
    let out = run(&["--master", &master_arg, "master", "sync"]);
    assert_eq!(code(&out), 0);

    let out = run(&[
        "--master",
        &master_arg,
        "search",
        "magicterm",
        "--all",
        "--snippets",
        "--json",
    ]);
    assert_eq!(code(&out), 2, "offline archive must exit 2");
    assert_matches_fixture("search_all_skips.json", normalized(&stdout(&out), tmp));

    let out = run(&["--master", &master_arg, "dedup", "--json"]);
    assert_eq!(code(&out), 2, "offline archive must exit 2");
    assert_matches_fixture("dedup_skips.json", normalized(&stdout(&out), tmp));

    // No orphaned fixtures: the directory holds exactly the frozen set, so
    // a renamed surface cannot leave a stale fixture silently exempt.
    if std::env::var_os("BACKUPSAGE_BLESS").is_none() {
        let mut names: Vec<String> = std::fs::read_dir(fixture_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "dedup.json",
                "dedup_chain.json",
                "dedup_skips.json",
                "master_list.json",
                "master_verify.json",
                "metadata_only_skips.json",
                "search.json",
                "search_all.json",
                "search_all_skips.json",
                "search_only.json",
            ],
            "unexpected fixture set — remove orphans or update this list"
        );
    }
}

/// #71: `search --all --json`'s per-archive `mode` field pinned at a real
/// non-default value (`search-only`), not just `full` — ADR 0003's
/// residual-gaps rule: a field frozen only at its default is not really
/// pinned.
#[test]
fn search_all_search_only_mode_matches_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let archive = write_archive(
        tmp,
        "so-contract.tar",
        &build_tar(&[("doc.txt", b"contractword body text".to_vec())]),
    );
    let out = run(&["index", archive.to_str().unwrap(), "--mode", "search-only"]);
    assert_eq!(code(&out), 0, "index failed: {}", stdout(&out));
    let master = tmp.join("so-master.db");
    let m = master.to_str().unwrap();
    let out = run(&[
        "--master",
        m,
        "master",
        "add",
        archive.with_extension("tar.db").to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "master add failed");

    let out = run(&["--master", m, "search", "contractword", "--all", "--json"]);
    assert_eq!(code(&out), 0);
    let report = normalized(&stdout(&out), tmp);
    assert_eq!(
        report["archives"][0]["mode"], "search-only",
        "fixture must pin a non-degenerate mode value: {report}"
    );
    assert_matches_fixture("search_only.json", report);
}

/// #71: `dedup --json` with a metadata-only archive present alongside a
/// normal duplicate pair — pins `summary.skipped_archives`' worded reason
/// and `archives[].content_mode` at real non-`full` values, and freezes
/// the exit-2 behavior of the previously-silent-empty gap.
#[test]
fn dedup_metadata_only_skip_matches_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let dup = b"metadata-fixture duplicate payload\n".to_vec();
    let full = write_archive(
        tmp,
        "mo-full.tar",
        &build_tar(&[
            ("keep/one.bin", dup.clone()),
            ("keep/two.bin", dup),
        ]),
    );
    let metadata_only = write_archive(
        tmp,
        "mo-only.tar",
        &build_tar(&[("secret/three.bin", b"never hashed content".to_vec())]),
    );
    let out = run(&["index", full.to_str().unwrap()]);
    assert_eq!(code(&out), 0, "index failed: {}", stdout(&out));
    let out = run(&[
        "index",
        metadata_only.to_str().unwrap(),
        "--mode",
        "metadata-only",
    ]);
    assert_eq!(code(&out), 0, "index failed: {}", stdout(&out));
    let master = tmp.join("mo-master.db");
    let m = master.to_str().unwrap();
    let out = run(&[
        "--master",
        m,
        "master",
        "add",
        full.with_extension("tar.db").to_str().unwrap(),
        metadata_only.with_extension("tar.db").to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "master add failed");

    let out = run(&["--master", m, "dedup", "--json"]);
    assert_eq!(code(&out), 2, "metadata-only archive must force exit 2");
    let report = normalized(&stdout(&out), tmp);
    assert_eq!(report["summary"]["groups"], 1, "the full-mode pair still dedups: {report}");
    assert_eq!(
        report["summary"]["skipped_archives"][0][0], "mo-only.tar",
        "{report}"
    );
    assert!(
        report["summary"]["skipped_archives"][0][1]
            .as_str()
            .unwrap()
            .contains("metadata-only"),
        "{report}"
    );
    assert_matches_fixture("metadata_only_skips.json", report);
}

/// Issue #9: freeze the keeper-star fields with NON-degenerate values — a
/// real transitive-only chain member (`actionable: false`,
/// `hamming_to_keep > 3`) pinned in a dedicated fixture, per ADR 0003's
/// residual-gaps note that a field frozen only at degenerate values does not
/// pin its real shape.
#[test]
fn dedup_chain_json_matches_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let (a_png, b_png, c_png) = near_chain_pngs();
    let chain_a = write_archive(
        tmp,
        "chain-alpha.tar",
        &build_tar_mtime(
            &[("photos/orig.png", a_png), ("photos/bright.png", b_png)],
            1_600_000_000,
        ),
    );
    let chain_b = write_archive(
        tmp,
        "chain-beta.tar",
        &build_tar_mtime(&[("export/faded.png", c_png)], 1_700_000_000),
    );
    let master = tmp.join("chain-master.db");
    let m = master.to_str().unwrap();
    for a in [&chain_a, &chain_b] {
        let out = run(&["index", a.to_str().unwrap()]);
        assert_eq!(code(&out), 0, "index failed: {}", stdout(&out));
    }
    let out = run(&[
        "--master",
        m,
        "master",
        "add",
        chain_a.with_extension("tar.db").to_str().unwrap(),
        chain_b.with_extension("tar.db").to_str().unwrap(),
    ]);
    assert_eq!(code(&out), 0, "master add failed");

    let out = run(&["--master", m, "dedup", "--json"]);
    assert_eq!(code(&out), 0);
    let report = normalized(&stdout(&out), tmp);
    // Non-degenerate guarantee before freezing: the transitive-only member
    // really is present and review-only in what gets blessed.
    assert_eq!(report["summary"]["transitive_only_files"], 1, "{report}");
    let faded = report["groups"][0]["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mm| mm["path"] == "export/faded.png")
        .unwrap();
    assert_eq!(faded["actionable"], false);
    assert!(faded["hamming_to_keep"].as_u64().unwrap() > 3);
    assert_matches_fixture("dedup_chain.json", report);
}

/// The documented exit-code contract, executed per subcommand:
/// 0 ok · 1 error · 2 completed-with-skips. Clap usage errors also exit 2
/// (clap's convention, distinguishable by stderr usage text and no work
/// performed) — frozen here as observed behavior.
#[test]
fn exit_code_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path();
    let dup = b"matrix duplicate payload\n".to_vec();
    let gamma = write_archive(
        tmp,
        "gamma.tar",
        &build_tar(&[
            ("data/keep.txt", b"gamma searchable gammaword\n".to_vec()),
            ("data/dup1.bin", dup.clone()),
            ("data/dup2.bin", dup),
        ]),
    );
    let delta = write_archive(
        tmp,
        "delta.tar",
        &build_tar(&[("other/also.txt", b"delta gammaword too\n".to_vec())]),
    );
    let gamma_db = gamma.with_extension("tar.db");
    let gamma_db = gamma_db.to_str().unwrap();
    let delta_db = delta.with_extension("tar.db");
    let master = tmp.join("master.db");
    let m = master.to_str().unwrap();
    let missing_master = tmp.join("nope/master.db");
    let missing_master = missing_master.to_str().unwrap();

    let check = |desc: &str, args: &[&str], expected: i32| {
        let out = run(args);
        assert_eq!(
            code(&out),
            expected,
            "exit-code contract violated for {desc} ({args:?})\nstdout: {}\nstderr: {}",
            stdout(&out),
            String::from_utf8_lossy(&out.stderr),
        );
    };

    // ok = 0
    check("index ok", &["index", gamma.to_str().unwrap()], 0);
    check(
        "index ok (2nd archive)",
        &["index", delta.to_str().unwrap()],
        0,
    );
    check(
        "master add ok",
        &[
            "--master",
            m,
            "master",
            "add",
            gamma_db,
            delta_db.to_str().unwrap(),
        ],
        0,
    );
    check("master list ok", &["--master", m, "master", "list"], 0);
    check("master sync ok", &["--master", m, "master", "sync"], 0);
    check(
        "master verify all ok",
        &["--master", m, "master", "verify"],
        0,
    );
    check("search hit", &["search", "gammaword", "-i", gamma_db], 0);
    check(
        "search no hits still ok",
        &["search", "zzznothing", "-i", gamma_db],
        0,
    );
    check("top ok", &["top", "-i", gamma_db], 0);
    check(
        "inspect ok",
        &["inspect", "data/keep.txt", "-i", gamma_db],
        0,
    );
    check("dedup ok (all online)", &["--master", m, "dedup"], 0);

    // error = 1
    check(
        "index missing source",
        &["index", "/nonexistent/src.tar"],
        1,
    );
    check(
        "index --index with multiple sources",
        &[
            "index",
            gamma.to_str().unwrap(),
            delta.to_str().unwrap(),
            "--index",
            "out.db",
        ],
        1,
    );
    check(
        "master add missing target",
        &["--master", m, "master", "add", "/nonexistent.db"],
        1,
    );
    check(
        "search missing index",
        &["search", "x", "-i", "/nonexistent.db"],
        1,
    );
    check("top missing index", &["top", "-i", "/nonexistent.db"], 1);
    check(
        "inspect unknown path",
        &["inspect", "no/such.txt", "-i", gamma_db],
        1,
    );
    check(
        "master list missing master",
        &["--master", missing_master, "master", "list"],
        1,
    );
    check(
        "master rm unknown key",
        &["--master", m, "master", "rm", "unknown-key"],
        1,
    );
    check(
        "dedup missing master",
        &["--master", missing_master, "dedup"],
        1,
    );
    check(
        "dedup bad --sort",
        &["--master", m, "dedup", "--sort", "bogus"],
        1,
    );

    // completed-with-skips = 2 (delta's index goes missing; replicas remain)
    std::fs::remove_file(&delta_db).unwrap();
    check(
        "master sync flags missing db, still ok",
        &["--master", m, "master", "sync"],
        0,
    );
    check("dedup with offline archive", &["--master", m, "dedup"], 2);
    check(
        "search --all with offline archive",
        &["--master", m, "search", "gammaword", "--all"],
        2,
    );
    // A second master holding only gamma isolates the archive-missing
    // detection — on the first master, delta's residual db-missing status
    // would force exit 2 even if that detection broke.
    let master2 = tmp.join("master2.db");
    let m2 = master2.to_str().unwrap();
    check(
        "second master add ok",
        &["--master", m2, "master", "add", gamma_db],
        0,
    );
    // Harness (not BackupSage) removes its own generated tar to simulate a
    // vanished source; verify must report it and exit 2.
    std::fs::remove_file(&gamma).unwrap();
    check(
        "master verify with missing source",
        &["--master", m2, "master", "verify"],
        2,
    );
    check(
        "master rm ok",
        &["--master", m, "master", "rm", "delta.tar"],
        0,
    );

    // clap usage errors = 2 (observed; distinct failure mode from skips)
    check("clap unknown flag", &["--bogus-flag"], 2);
    check("clap no subcommand", &[], 2);
}
