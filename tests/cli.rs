//! Whole-binary integration: the exact flows a user runs, through the
//! compiled executable. Every invocation pins --master to a temp path so
//! tests can never touch a real catalog.

mod common;

use std::path::Path;
use std::process::{Command, Output};

use common::*;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
}

fn run_ok(args: &[&str]) -> Output {
    let out = bin().args(args).output().expect("binary runs");
    assert!(
        out.status.success(),
        "command {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn full_flow_index_master_dedup_search() {
    let dir = tempfile::tempdir().unwrap();
    let master = dir.path().join("m.db");
    let master_arg = master.to_str().unwrap();

    // Two archives sharing one exact duplicate (different names) and one
    // near-duplicate image pair.
    let payload = b"shared document content for the dedup e2e".to_vec();
    let a = write_archive(
        dir.path(),
        "old-laptop.tar",
        &build_tar_mtime(
            &[
                ("backup/report.pdf", payload.clone()),
                ("photos/holiday.png", png_bytes(21, 320, 240)),
                ("notes.txt", b"searchable magicword here".to_vec()),
            ],
            1_600_000_000,
        ),
    );
    let b = write_archive(
        dir.path(),
        "new-backup.tar",
        &build_tar_mtime(
            &[
                ("docs/final-report.pdf", payload.clone()),
                ("export/holiday-edit.png", png_bytes_brightened(21, 320, 240, 8)),
            ],
            1_700_000_000,
        ),
    );

    // index both archives in one call
    run_ok(&["index", a.to_str().unwrap(), b.to_str().unwrap()]);

    // register both (by archive path — sibling .db resolution)
    let out = run_ok(&[
        "--master", master_arg,
        "master", "add", a.to_str().unwrap(), b.to_str().unwrap(),
    ]);
    assert!(stdout(&out).contains("registered 'old-laptop.tar'"));

    // master list shows both, ok
    let out = run_ok(&["--master", master_arg, "master", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert!(rows[0]["status"] == "ok" && rows[1]["status"] == "ok");

    // dedup --json: one exact group + one near group, exit 0
    let out = run_ok(&["--master", master_arg, "dedup", "--json"]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["version"], 1);
    let groups = report["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "expected exact+near, got: {report}");
    let kinds: Vec<&str> = groups.iter().map(|g| g["match_kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"exact") && kinds.contains(&"near"));
    let exact = groups.iter().find(|g| g["match_kind"] == "exact").unwrap();
    let keep = exact["members"]
        .as_array().unwrap()
        .iter()
        .find(|m| m["keep"] == true)
        .unwrap();
    assert_eq!(keep["path"], "docs/final-report.pdf"); // newest mtime wins
    assert_eq!(keep["keep_reason"], "newest");

    // terminal rendering mentions KEEP and reclaimable
    let out = run_ok(&["--master", master_arg, "dedup"]);
    let text = stdout(&out);
    assert!(text.contains("KEEP"), "{text}");
    assert!(text.contains("reclaimable"), "{text}");

    // federated search finds the word in archive a only
    let out = run_ok(&["--master", master_arg, "search", "magicword", "--all"]);
    let text = stdout(&out);
    assert!(text.contains("old-laptop.tar"), "{text}");
    assert!(text.contains("notes.txt"), "{text}");

    // inspect one file
    let out = run_ok(&[
        "inspect", "backup/report.pdf",
        "-a", a.to_str().unwrap(),
    ]);
    let text = stdout(&out);
    assert!(text.contains("BLAKE3"), "{text}");
    assert!(text.contains("tar header"), "{text}");
}

#[test]
fn adhoc_dedup_without_master_and_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let payload = b"twice-stored bytes".to_vec();
    let a = write_archive(dir.path(), "x.tar", &build_tar(&[("f/one.dat", payload.clone())]));
    let b = write_archive(dir.path(), "y.tar", &build_tar(&[("g/two.dat", payload)]));
    run_ok(&["index", a.to_str().unwrap()]);
    run_ok(&["index", b.to_str().unwrap()]);

    // Ad-hoc: no master anywhere near this.
    let out = run_ok(&[
        "dedup",
        "--db", dir.path().join("x.tar.db").to_str().unwrap(),
        "--db", dir.path().join("y.tar.db").to_str().unwrap(),
        "--json",
    ]);
    let report: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["summary"]["groups"], 1);

    // Missing master → clear error, exit 1.
    let missing = dir.path().join("nope/master.db");
    let out = bin()
        .args(["--master", missing.to_str().unwrap(), "dedup"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no master catalog"));

    // Offline archive → dedup still works from replicas, exit 2.
    let master = dir.path().join("m.db");
    run_ok(&[
        "--master", master.to_str().unwrap(),
        "master", "add",
        dir.path().join("x.tar.db").to_str().unwrap(),
        dir.path().join("y.tar.db").to_str().unwrap(),
    ]);
    std::fs::remove_file(dir.path().join("y.tar.db")).unwrap();
    run_ok(&["--master", master.to_str().unwrap(), "master", "sync"]);
    let out = bin()
        .args(["--master", master.to_str().unwrap(), "dedup", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "offline archive must exit 2");
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(report["summary"]["groups"], 1, "replica rows still dedup");
    assert_eq!(report["summary"]["archives_offline"][0], "y.tar");
}

#[test]
fn dir_index_via_cli_and_v2_limited_flow() {
    let dir = tempfile::tempdir().unwrap();
    let master = dir.path().join("m.db");

    // Directory source through the binary.
    let src = dir.path().join("pics");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("photo.png"), png_bytes(3, 100, 80)).unwrap();
    run_ok(&["index", src.to_str().unwrap()]);
    assert!(dir.path().join("pics.db").exists());

    // Fabricated v2 db registers as v2-limited; dedup exits 2 and names it.
    let v2 = dir.path().join("legacy.tar.db");
    {
        let conn = rusqlite::Connection::open(&v2).unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE files_fts USING fts5(path, content, tokenize='unicode61');
             CREATE TABLE word_freq(word TEXT PRIMARY KEY,
                                    total_count INTEGER NOT NULL DEFAULT 0,
                                    doc_count INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version','2'),
                                     ('archive','/backups/legacy.tar'),
                                     ('created_unix','1500000000'),
                                     ('completed','1');
             INSERT INTO files_fts(path, content) VALUES ('old/file.txt','legacy grepterm');",
        )
        .unwrap();
    }
    let out = run_ok(&[
        "--master", master.to_str().unwrap(),
        "master", "add",
        dir.path().join("pics.db").to_str().unwrap(),
        v2.to_str().unwrap(),
    ]);
    assert!(stdout(&out).contains("v2-limited"));

    let out = bin()
        .args(["--master", master.to_str().unwrap(), "dedup", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let report: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(report["summary"]["skipped_archives"][0][0], "legacy.tar");

    // …but the v2 db is fully searchable in the federated path.
    let out = bin()
        .args(["--master", master.to_str().unwrap(), "search", "grepterm", "--all"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains("old/file.txt"));
}
