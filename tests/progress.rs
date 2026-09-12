//! Progress/cancellation contracts independent of terminal rendering.
mod common;

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write;
use std::path::Path;

use backupsage::indexer::{run_index, run_index_with_control, IndexOptions};
use backupsage::progress::{
    CancellationToken, Cancelled, OperationControl, ProgressEvent, ProgressObserver, ProgressUnit,
};

#[derive(Default)]
struct Recorder {
    events: RefCell<Vec<&'static str>>,
    bytes: Cell<u64>,
    files: Cell<u64>,
    byte_units: Cell<bool>,
    total: Cell<u64>,
    warnings: RefCell<Vec<String>>,
    cancellation: CancellationToken,
    cancel_at: Option<&'static str>,
}

impl ProgressObserver for Recorder {
    fn on_event(&self, event: ProgressEvent<'_>) {
        let name = match event {
            ProgressEvent::IndexStarted { .. } => "start",
            ProgressEvent::Total { amount, unit } => {
                self.total.set(amount);
                self.byte_units.set(matches!(unit, ProgressUnit::Bytes));
                "total"
            }
            ProgressEvent::Advanced { amount } => {
                let counter = if self.byte_units.get() {
                    &self.bytes
                } else {
                    &self.files
                };
                counter.set(counter.get() + amount);
                "advance"
            }
            ProgressEvent::Entry { .. } => "entry",
            ProgressEvent::Warning { message } => {
                self.warnings.borrow_mut().push(message.to_owned());
                "warning"
            }
            ProgressEvent::ReadyToPromote => "promote",
            ProgressEvent::Finished => "finish",
            ProgressEvent::IndexDiscovered { .. } => "discovered",
        };
        self.events.borrow_mut().push(name);
        if self.cancel_at == Some(name) {
            self.cancellation.cancel();
        }
    }
}

fn archives(root: &Path) -> Vec<std::path::PathBuf> {
    // Include enough trailing bytes that the fingerprint drain must read more
    // compressed input after tar reaches its end marker.
    let mut tar = common::build_tar(&[("hello.txt", b"hello world".repeat(100_000))]);
    tar.extend_from_slice(&vec![0; 512 * 1024]);
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar).unwrap();
    [
        ("source.tar", tar.clone()),
        ("source.tar.gz", gzip.finish().unwrap()),
        (
            "source.tar.zst",
            zstd::encode_all(tar.as_slice(), 1).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(name, bytes)| common::write_archive(root, name, &bytes))
    .collect()
}

fn assert_clean(root: &Path) {
    for entry in fs::read_dir(root).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(!name.contains(".tmp."), "staged output survived: {name}");
    }
}

#[test]
fn events_count_all_archive_bytes_including_the_tail() {
    let temp = tempfile::tempdir().unwrap();
    for source in archives(temp.path()) {
        let observer = Recorder::default();
        let summary = run_index_with_control(
            &source,
            None,
            &IndexOptions::default(),
            &OperationControl::new(&observer, observer.cancellation.clone()),
        )
        .unwrap();
        assert_eq!(observer.total.get(), fs::metadata(&source).unwrap().len());
        assert_eq!(observer.bytes.get(), observer.total.get());
        let events = observer.events.borrow();
        assert_eq!(events.first(), Some(&"start"));
        assert_eq!(&events[events.len() - 2..], &["promote", "finish"]);
        assert!(summary.db_path.exists());
        let conn = backupsage::searcher::open_index(&summary.db_path).unwrap();
        assert_eq!(
            backupsage::searcher::get_meta(&conn, "archive_blake3").unwrap(),
            common::digest_of(&source)
        );
    }
}

#[test]
fn directory_events_and_unsanitized_diagnostics_reach_the_observer() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("bad\x1b[31m.png"), b"not an image").unwrap();
    fs::write(source.join("hello.txt"), b"hello").unwrap();
    let observer = Recorder::default();
    run_index_with_control(
        &source,
        None,
        &IndexOptions::default(),
        &OperationControl::new(&observer, observer.cancellation.clone()),
    )
    .unwrap();
    assert!(!observer.byte_units.get());
    assert_eq!(observer.total.get(), 2);
    assert_eq!(observer.files.get(), 2);
    assert_eq!(
        &*observer.warnings.borrow(),
        &["warning: could not decode image 'bad\x1b[31m.png'"]
    );
    assert_eq!(observer.events.borrow().last(), Some(&"finish"));
}

#[test]
fn pre_cancelled_run_never_creates_an_output() {
    let temp = tempfile::tempdir().unwrap();
    let observer = Recorder::default();
    observer.cancellation.cancel();
    let error = run_index_with_control(
        &temp.path().join("absent.tar"),
        None,
        &IndexOptions::default(),
        &OperationControl::new(&observer, observer.cancellation.clone()),
    )
    .unwrap_err();
    assert!(error.is::<Cancelled>(), "{error:#}");
    assert!(observer.events.borrow().is_empty());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn cancellation_preserves_old_index_and_cleans_staging_at_each_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let mut sources = archives(temp.path());
    let dir = temp.path().join("directory");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("hello.txt"), b"hello world").unwrap();
    sources.push(dir);
    for source in sources {
        let previous = run_index(&source, None, &IndexOptions::default())
            .unwrap()
            .db_path;
        let previous_bytes = fs::read(&previous).unwrap();
        for boundary in ["start", "total", "advance", "entry", "promote"] {
            let observer = Recorder {
                cancel_at: Some(boundary),
                ..Recorder::default()
            };
            let error = run_index_with_control(
                &source,
                None,
                &IndexOptions::default(),
                &OperationControl::new(&observer, observer.cancellation.clone()),
            )
            .unwrap_err();
            assert!(
                error.is::<Cancelled>(),
                "{} at {boundary}: {error:#}",
                source.display()
            );
            assert!(
                observer.events.borrow().contains(&boundary),
                "{boundary} was never exercised"
            );
            assert!(!observer.events.borrow().contains(&"finish"));
            assert_eq!(fs::read(&previous).unwrap(), previous_bytes);
            assert_clean(temp.path());
        }
    }
}

#[test]
fn cancellation_before_promotion_leaves_no_new_index() {
    let temp = tempfile::tempdir().unwrap();
    for source in archives(temp.path()) {
        let destination = temp.path().join("new.db");
        let observer = Recorder {
            cancel_at: Some("promote"),
            ..Recorder::default()
        };
        let error = run_index_with_control(
            &source,
            Some(&destination),
            &IndexOptions::default(),
            &OperationControl::new(&observer, observer.cancellation.clone()),
        )
        .unwrap_err();
        assert!(error.is::<Cancelled>());
        assert!(!destination.exists());
        assert_clean(temp.path());
    }
}
