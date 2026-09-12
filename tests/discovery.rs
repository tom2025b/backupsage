//! Discovery reports raw events to library callers and hints only through CLI.
mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use backupsage::indexer::{run_index, IndexOptions};
use backupsage::progress::{CancellationToken, OperationControl, ProgressEvent, ProgressObserver};
use backupsage::searcher::{discover_db_path, discover_db_path_with_control};

const INDEX_NAME: &str = "chosen\x1b[31m.db";

fn fixture(root: &Path) {
    let archive = common::write_archive(
        root,
        "source.tar",
        &common::build_tar(&[("hello.txt", b"hello world".to_vec())]),
    );
    run_index(
        &archive,
        Some(&root.join(INDEX_NAME)),
        &IndexOptions::default(),
    )
    .unwrap();
}

#[test]
fn discovery_emits_raw_event_without_terminal_output() {
    const CHILD: &str = "BACKUPSAGE_DISCOVERY_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        struct Discovered(RefCell<Vec<PathBuf>>);
        impl ProgressObserver for Discovered {
            fn on_event(&self, event: ProgressEvent<'_>) {
                match event {
                    ProgressEvent::IndexDiscovered { path } => {
                        self.0.borrow_mut().push(path.to_owned())
                    }
                    other => panic!("unexpected discovery event: {other:?}"),
                }
            }
        }
        let observer = Discovered(RefCell::new(Vec::new()));
        let control = OperationControl::new(&observer, CancellationToken::default());
        let expected = Path::new(".").join(INDEX_NAME);
        assert_eq!(
            discover_db_path_with_control(None, None, &control).unwrap(),
            expected
        );
        assert_eq!(
            observer.0.borrow().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert_eq!(discover_db_path(None, None).unwrap(), expected);
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    // Re-exec exactly this test, uncaptured, to inspect actual library I/O.
    // Child-only cwd avoids changing global process state under parallel tests.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "discovery_emits_raw_event_without_terminal_output",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("1 passed"),
        "child test did not execute: {stdout}"
    );
    assert!(
        output.stderr.is_empty(),
        "library wrote stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stdout.contains("hint:"),
        "library printed a hint: {stdout}"
    );
    assert!(
        !stdout.contains(INDEX_NAME),
        "library printed its raw path: {stdout}"
    );
}

#[test]
fn cli_discovery_renders_exactly_one_sanitized_hint() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let output = Command::new(env!("CARGO_BIN_EXE_backupsage"))
        .arg("--master")
        .arg(temp.path().join("unused-master.db"))
        .args(["search", "hello", "--json"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "hint: using index './chosen␛[31m.db' — pass --index to be explicit\n"
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(result.to_string().contains("hello.txt"));
}
