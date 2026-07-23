//! v1.0.1 destructive-path regression fixtures: every attack leaves
//! protected fixture digests and identities unchanged.
mod common;
use common::*;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_backupsage"))
}

fn run(args: &[&str], cwd: &Path) -> Output {
    bin().args(args).current_dir(cwd).output().unwrap()
}

fn no_staging_debris(dir: &Path) {
    let names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp.")), "{names:?}");
}

#[test]
fn index_dest_at_archive_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    let tar = write_archive(d.path(), "a.tar", &build_tar(&[("f.txt", b"hi".to_vec())]));
    let (dig, ino) = (digest_of(&tar), inode_of(&tar));
    let out = run(&["index", "a.tar", "-i", "a.tar"], d.path());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!((digest_of(&tar), inode_of(&tar)), (dig, ino));
    no_staging_debris(d.path());
}

#[test]
fn index_dest_symlink_and_hardlink_to_archive_fail_closed() {
    let d = tempfile::tempdir().unwrap();
    let tar = write_archive(d.path(), "a.tar", &build_tar(&[("f.txt", b"hi".to_vec())]));
    let (dig, ino) = (digest_of(&tar), inode_of(&tar));
    std::os::unix::fs::symlink(&tar, d.path().join("s.db")).unwrap();
    fs::hard_link(&tar, d.path().join("h.db")).unwrap();
    for dest in ["s.db", "h.db"] {
        let out = run(&["index", "a.tar", "-i", dest], d.path());
        assert_eq!(out.status.code(), Some(1), "dest {dest}");
        assert_eq!(
            (digest_of(&tar), inode_of(&tar)),
            (dig.clone(), ino),
            "dest {dest}"
        );
    }
    // symlink still a symlink, not replaced
    assert!(fs::symlink_metadata(d.path().join("s.db"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn index_dest_inside_source_dir_fails_closed() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("photos");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("m.txt"), b"member").unwrap();
    let dig = digest_of(&src.join("m.txt"));
    let out = run(&["index", "photos", "-i", "photos/m.txt"], d.path());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(digest_of(&src.join("m.txt")), dig);
}

#[test]
fn index_dest_foreign_file_and_foreign_index_rejected_unchanged() {
    let d = tempfile::tempdir().unwrap();
    write_archive(d.path(), "a.tar", &build_tar(&[("f.txt", b"hi".to_vec())]));
    write_archive(d.path(), "b.tar", &build_tar(&[("g.txt", b"yo".to_vec())]));
    // foreign non-index file
    fs::write(d.path().join("notes.db"), b"not sqlite").unwrap();
    let dig = digest_of(&d.path().join("notes.db"));
    let out = run(&["index", "a.tar", "-i", "notes.db"], d.path());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(digest_of(&d.path().join("notes.db")), dig);
    // index owned by another source
    let ok = run(&["index", "b.tar"], d.path());
    assert!(ok.status.success());
    let bdig = digest_of(&d.path().join("b.tar.db"));
    let out = run(&["index", "a.tar", "-i", "b.tar.db"], d.path());
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(digest_of(&d.path().join("b.tar.db")), bdig);
}

#[test]
fn interrupted_reindex_preserves_last_good_index() {
    let d = tempfile::tempdir().unwrap();
    let tar = write_archive(d.path(), "a.tar", &build_tar(&[("f.txt", b"v1".to_vec())]));
    assert!(run(&["index", "a.tar"], d.path()).status.success());
    let good = digest_of(&d.path().join("a.tar.db"));
    // corrupt the archive so a re-index fails mid-build: valid first
    // entry, then a garbage header the tar iterator cannot resync past
    let mut corrupt = build_tar(&[("f.txt", b"v2".to_vec())]);
    let len = corrupt.len();
    corrupt.truncate(len - 1024); // strip end-of-archive zero blocks
    corrupt.extend_from_slice(&[0xAA; 512]); // garbage header
    fs::write(&tar, &corrupt).unwrap();
    let out = run(&["index", "a.tar"], d.path());
    assert_eq!(out.status.code(), Some(1));
    // prior completed index untouched and still searchable
    assert_eq!(digest_of(&d.path().join("a.tar.db")), good);
    let s = run(&["search", "v1", "-i", "a.tar.db"], d.path());
    assert!(s.status.success());
    no_staging_debris(d.path());
}

#[test]
fn distinct_dest_and_same_source_reindex_succeed() {
    let d = tempfile::tempdir().unwrap();
    write_archive(d.path(), "a.tar", &build_tar(&[("f.txt", b"one".to_vec())]));
    assert!(run(&["index", "a.tar", "-i", "custom.db"], d.path())
        .status
        .success());
    // same-source re-index at the same dest replaces the old index
    assert!(run(&["index", "a.tar", "-i", "custom.db"], d.path())
        .status
        .success());
    let s = run(&["search", "one", "-i", "custom.db"], d.path());
    assert!(s.status.success());
    no_staging_debris(d.path());
}

#[test]
fn dedup_output_never_clobbers() {
    let d = tempfile::tempdir().unwrap();
    write_archive(
        d.path(),
        "a.tar",
        &build_tar(&[("f.txt", b"same".to_vec()), ("g.txt", b"same".to_vec())]),
    );
    assert!(run(&["index", "a.tar"], d.path()).status.success());
    let master = d.path().join("m.db");
    let m = "m.db";
    assert!(run(&["--master", m, "master", "add", "a.tar.db"], d.path())
        .status
        .success());
    let mdig = digest_of(&master);
    let adig = digest_of(&d.path().join("a.tar"));
    let idig = digest_of(&d.path().join("a.tar.db"));

    // -o at master, index, archive, and an existing unrelated file
    fs::write(d.path().join("existing.txt"), b"keep me").unwrap();
    for dest in ["m.db", "a.tar.db", "a.tar", "existing.txt"] {
        let out = run(&["--master", m, "dedup", "-o", dest], d.path());
        assert_eq!(out.status.code(), Some(1), "dest {dest}");
    }
    assert_eq!(digest_of(&master), mdig);
    assert_eq!(digest_of(&d.path().join("a.tar")), adig);
    assert_eq!(digest_of(&d.path().join("a.tar.db")), idig);
    assert_eq!(fs::read(d.path().join("existing.txt")).unwrap(), b"keep me");

    // symlink and hardlink aliases to the master are rejected, not unlinked
    std::os::unix::fs::symlink(&master, d.path().join("alias.json")).unwrap();
    fs::hard_link(&master, d.path().join("hard.json")).unwrap();
    for dest in ["alias.json", "hard.json"] {
        let out = run(&["--master", m, "dedup", "-o", dest], d.path());
        assert_eq!(out.status.code(), Some(1), "dest {dest}");
    }
    assert!(fs::symlink_metadata(d.path().join("alias.json"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(digest_of(&master), mdig);

    // a fresh distinct output receives the complete report, no staging debris
    let out = run(
        &["--master", m, "dedup", "--json", "-o", "report.json"],
        d.path(),
    );
    assert!(out.status.success());
    let report: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(d.path().join("report.json")).unwrap()).unwrap();
    assert_eq!(report["version"], 1);
    no_staging_debris(d.path());
}

#[test]
fn cwd_fallback_refuses_foreign_db() {
    let d = tempfile::tempdir().unwrap();
    let ro = d.path().join("ro");
    fs::create_dir(&ro).unwrap();
    write_archive(&ro, "a.tar", &build_tar(&[("f.txt", b"hi".to_vec())]));
    // pre-plant a foreign file at the fallback name in CWD
    fs::write(d.path().join("a.tar.db"), b"unrelated").unwrap();
    let dig = digest_of(&d.path().join("a.tar.db"));
    // make the preferred destination unwritable so the fallback engages
    use std::os::unix::fs::PermissionsExt;
    let mut perm = fs::metadata(&ro).unwrap().permissions();
    perm.set_mode(0o555);
    fs::set_permissions(&ro, perm.clone()).unwrap();
    let out = run(&["index", "ro/a.tar"], d.path());
    perm.set_mode(0o755);
    fs::set_permissions(&ro, perm).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(digest_of(&d.path().join("a.tar.db")), dig);
}
