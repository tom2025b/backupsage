//! Issue #79: ordinary indexing refuses structural Borg repository
//! candidates without invoking Borg or opening repository segment content.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use backupsage::indexer::{self, IndexOptions};

const REPO_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn config(version: &str, id: &str) -> String {
    format!("[repository]\nversion = {version}\nid = {id}\nsegments_per_dir = 1000\n")
}

fn tar_bytes(contents: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    builder
        .append_data(&mut header, "must-not-be-indexed.txt", contents)
        .unwrap();
    builder.into_inner().unwrap()
}

fn make_candidate(root: &Path, segment: &[u8]) -> PathBuf {
    fs::create_dir_all(root.join("data/0")).unwrap();
    fs::write(root.join("data/0/0"), segment).unwrap();
    fs::write(root.join("config"), config("1", REPO_ID)).unwrap();
    root.to_path_buf()
}

fn assert_refused(source: &Path, db: &Path) -> String {
    let error = indexer::run_index(source, Some(db), &IndexOptions::default()).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("Borg repository candidate"),
        "unexpected error: {rendered}"
    );
    assert!(!db.exists(), "refusal published {}", db.display());
    let parent = db.parent().unwrap();
    let names: Vec<_> = fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|name| !name.contains(".tmp.")),
        "staging debris: {names:?}"
    );
    rendered
}

#[test]
fn confirmed_repository_root_is_refused_before_staging() {
    let temp = tempfile::tempdir().unwrap();
    let repo = make_candidate(&temp.path().join("repo"), b"encrypted segment sentinel");
    assert_refused(&repo, &temp.path().join("root.db"));
}

#[cfg(unix)]
#[test]
fn canonical_symlink_alias_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let repo = make_candidate(&temp.path().join("real-repo"), b"segment");
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&repo, &alias).unwrap();
    assert_refused(&alias, &temp.path().join("alias.db"));
}

#[test]
fn descendant_segment_is_refused_before_its_readable_payload_is_opened() {
    let temp = tempfile::tempdir().unwrap();
    let payload = tar_bytes(b"sentinel plaintext that a missing guard would index");
    let repo = make_candidate(&temp.path().join("repo"), &payload);
    let segment = repo.join("data/0/0");

    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&segment)
        .unwrap();
    file.set_times(fs::FileTimes::new().set_accessed(old).set_modified(old))
        .unwrap();
    drop(file);
    let before_atime = fs::metadata(&segment).unwrap().accessed().unwrap();

    assert_refused(&segment, &temp.path().join("segment.db"));
    assert_eq!(
        fs::metadata(&segment).unwrap().accessed().unwrap(),
        before_atime,
        "the structural probe must not open the segment payload"
    );
}

#[test]
fn nested_repository_is_refused_by_precount_and_leaves_no_partial_db() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("a")).unwrap();
    fs::write(source.join("a/ordinary.txt"), b"ordinary").unwrap();
    make_candidate(&source.join("b/nested"), b"segment");

    assert_refused(&source, &temp.path().join("nested.db"));
}

#[test]
fn malformed_and_unsupported_repository_configs_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let repo = make_candidate(&temp.path().join("repo"), b"segment");
    let cases = [
        ("malformed-id", config("1", "not-a-32-byte-id")),
        ("missing-version", format!("[repository]\nid = {REPO_ID}\n")),
        ("malformed-section", "[repository\nversion = 1\n".into()),
        ("unsupported-version", config("2", REPO_ID)),
    ];
    for (name, contents) in cases {
        fs::write(repo.join("config"), contents).unwrap();
        assert_refused(&repo, &temp.path().join(format!("{name}.db")));
    }
}

#[test]
fn unreadable_config_fails_closed_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let repo = make_candidate(&temp.path().join("repo"), b"segment");
    let config_path = repo.join("config");
    let mut permissions = fs::metadata(&config_path).unwrap().permissions();
    permissions.set_mode(0o000);
    fs::set_permissions(&config_path, permissions).unwrap();

    let platform_enforced = fs::File::open(&config_path).is_err();
    let error = assert_refused(&repo, &temp.path().join("unreadable.db"));
    if platform_enforced {
        assert!(error.contains("config is unreadable"), "{error}");
    } else {
        // Privileged test runners can bypass mode 000.  Keep a robust
        // inconclusive-marker leg so the test never becomes vacuous there.
        fs::remove_file(&config_path).unwrap();
        fs::create_dir(&config_path).unwrap();
        assert_refused(&repo, &temp.path().join("nonregular.db"));
    }
}

#[test]
fn oversized_repository_config_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let repo = make_candidate(&temp.path().join("repo"), b"segment");
    let mut oversized = config("1", REPO_ID).into_bytes();
    oversized.resize(70 * 1024, b'#');
    fs::write(repo.join("config"), oversized).unwrap();
    let error = assert_refused(&repo, &temp.path().join("oversized.db"));
    assert!(error.contains("bounded read limit"), "{error}");
}

#[cfg(unix)]
#[test]
fn symlinked_config_marker_fails_closed_without_following_it() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("data")).unwrap();
    let target = temp.path().join("elsewhere-config");
    fs::write(&target, config("1", REPO_ID)).unwrap();
    std::os::unix::fs::symlink(&target, repo.join("config")).unwrap();

    let error = assert_refused(&repo, &temp.path().join("symlink.db"));
    assert!(error.contains("config is a symlink"), "{error}");
}

#[test]
fn ordinary_config_and_data_tree_is_not_a_false_positive() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("application");
    fs::create_dir_all(source.join("data/0")).unwrap();
    fs::write(
        source.join("config"),
        b"[application]\nversion = 1\nid = ordinary\n",
    )
    .unwrap();
    // Even a numeric data path alone is insufficient without repository
    // section evidence: generic applications use this layout too.
    fs::write(source.join("data/0/0"), b"ordinary application data").unwrap();

    let db = temp.path().join("ordinary.db");
    let summary = indexer::run_index(&source, Some(&db), &IndexOptions::default()).unwrap();
    assert_eq!(summary.files_hashed, 2);
    assert!(db.exists());
}

#[test]
fn directory_merely_named_borg_retains_ordinary_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("borg");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("notes.txt"), b"ordinary directory").unwrap();

    let db = temp.path().join("named-borg.db");
    let summary = indexer::run_index(&source, Some(&db), &IndexOptions::default()).unwrap();
    assert_eq!(summary.files_hashed, 1);
    assert!(db.exists());
}

#[test]
fn nested_refusal_preserves_byte_identical_last_good_index() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("last-good.txt"), b"last good contents").unwrap();
    let db = temp.path().join("source.db");

    indexer::run_index(&source, Some(&db), &IndexOptions::default()).unwrap();
    let before = fs::read(&db).unwrap();
    make_candidate(&source.join("later/nested-repo"), b"segment sentinel");

    let error = indexer::run_index(&source, Some(&db), &IndexOptions::default()).unwrap_err();
    assert!(format!("{error:#}").contains("Borg repository candidate"));
    assert_eq!(fs::read(&db).unwrap(), before);
    let names: Vec<_> = fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|name| !name.contains(".tmp.")),
        "{names:?}"
    );
}
