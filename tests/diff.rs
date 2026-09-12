//! Pure diff contract (#92). Index ingestion and CLI coverage belong to #93.
use std::path::Path;

use backupsage::diff::{
    self, ChangeKind as Kind, Entry, EntryType, Reason, Snapshot, SnapshotInfo,
    SnapshotState as State, SourceCurrency,
};
use backupsage::store::flags;

fn entry(id: i64, path: &[u8], content: Option<&[u8]>) -> Entry {
    Entry {
        file_id: id,
        path: path.into(),
        entry_type: EntryType::File,
        link_target: None,
        size: content.map_or(12, |b| b.len() as u64),
        mtime_unix: Some(1_700_000_001),
        mode: Some(0o644),
        content_hash: content.map(|b| *blake3::hash(b).as_bytes()),
        flags: 0,
    }
}

fn snapshot(uuid: &str, entries: Vec<Entry>) -> Snapshot {
    Snapshot {
        info: SnapshotInfo {
            index_uuid: Some(uuid.into()),
            schema_version: Some(3),
            hash_algo: Some("blake3".into()),
            state: State::Complete,
            source_currency: SourceCurrency::NotChecked,
        },
        entries,
    }
}

fn corpus() -> (Snapshot, Snapshot) {
    let before = snapshot(
        "before",
        vec![
            entry(1, b"unchanged", Some(b"same")),
            entry(2, b"metadata", Some(b"metadata")),
            entry(3, b"content", Some(b"old")),
            entry(4, b"old-name", Some(b"moved")),
            entry(5, b"removed", Some(b"gone")),
        ],
    );
    let mut after = snapshot(
        "after",
        vec![
            entry(91, b"unchanged", Some(b"same")),
            entry(92, b"metadata", Some(b"metadata")),
            entry(93, b"content", Some(b"new")),
            entry(94, b"new-name", Some(b"moved")),
            entry(95, b"added", Some(b"fresh")),
        ],
    );
    after.entries[1].mode = Some(0o600);
    (before, after)
}

fn golden(name: &str, report: &diff::DiffReport) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/diff")
        .join(name);
    let actual = report.to_json().unwrap();
    if std::env::var_os("BACKUPSAGE_BLESS").as_deref() == Some(std::ffi::OsStr::new("1")) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, actual).unwrap();
    } else {
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            actual,
            "diff contract drifted: {name}"
        );
    }
}

#[test]
fn complete_golden_covers_all_six_classifications() {
    let (before, after) = corpus();
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.comparison_state, State::Complete);
    let s = &report.summary;
    assert_eq!(
        (
            s.added,
            s.removed,
            s.moved,
            s.byte_identical,
            s.metadata_only_changed,
            s.content_changed,
            s.inconclusive
        ),
        (1, 1, 1, 1, 1, 1, 0)
    );
    golden("complete.json", &report);
}

#[test]
fn report_bytes_do_not_depend_on_input_order() {
    let (mut before, mut after) = corpus();
    // Include a shadow and a display-colliding raw path in the permutation
    // corpus, so exclusions and namespace ordering are covered as well.
    before
        .entries
        .push(entry(30, b"duplicate", Some(b"hidden")));
    before
        .entries
        .push(entry(31, b"duplicate", Some(b"visible")));
    after.entries.push(entry(40, b"\xff", Some(b"raw ff")));
    after.entries.push(entry(41, b"\xfe", Some(b"raw fe")));
    let expected = diff::compare(&before, &after).unwrap().to_json().unwrap();
    for i in 0..49 {
        before.entries.rotate_left(1);
        if i % 2 == 0 {
            after.entries.reverse();
        }
        after.entries.rotate_left(2);
        assert_eq!(
            diff::compare(&before, &after).unwrap().to_json().unwrap(),
            expected
        );
    }
}

#[test]
fn source_currency_does_not_erase_historical_evidence() {
    let (mut before, mut after) = corpus();
    before.info.source_currency = SourceCurrency::Offline;
    after.info.source_currency = SourceCurrency::Stale;
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.comparison_state, State::Complete);
    assert_eq!(report.summary.moved, 1);
    golden("offline_stale.json", &report);
    for currency in [
        SourceCurrency::Denied,
        SourceCurrency::StatMatches,
        SourceCurrency::DirectoryUnverified,
    ] {
        after.info.source_currency = currency;
        let report = diff::compare(&before, &after).unwrap();
        assert_eq!(report.after.source_currency, currency);
        assert_eq!(report.summary.moved, 1);
    }
}

#[test]
fn incomplete_peer_never_proves_absence_or_moves() {
    let (mut before, mut after) = corpus();
    before.info.state = State::Incomplete;
    after.info.state = State::Incomplete;
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.comparison_state, State::Incomplete);
    assert_eq!(
        (
            report.summary.added,
            report.summary.removed,
            report.summary.moved
        ),
        (0, 0, 0)
    );
    assert_eq!(report.summary.inconclusive, 4);
    golden("incomplete.json", &report);
    // Direction matters: an observed entry in a partial index can still
    // be absent from the complete peer, but not vice versa.
    before.info.state = State::Complete;
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!((report.summary.added, report.summary.removed), (2, 0));
}

#[test]
fn unavailable_is_not_an_empty_snapshot() {
    let before = snapshot("before", vec![entry(1, b"only", Some(b"data"))]);
    let mut after = snapshot("missing", vec![]);
    after.info.state = State::Unavailable;
    after.info.index_uuid = None;
    after.info.schema_version = None;
    after.info.hash_algo = None;
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.comparison_state, State::Unavailable);
    assert_eq!(report.summary.removed, 0);
    assert_eq!(report.changes[0].reason, Reason::OtherSnapshotIncomplete);
    golden("unavailable.json", &report);
    let reverse = diff::compare(&after, &before).unwrap();
    assert_eq!(reverse.summary.added, 0);
    assert_eq!(reverse.summary.inconclusive, 1);
}

#[test]
fn incompatible_evidence_never_classifies_bytes_or_absence() {
    let (before, mut after) = corpus();
    after.info.hash_algo = Some("sha256".into());
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.comparison_state, State::Incompatible);
    assert!(report.changes.iter().all(|c| c.kind == Kind::Inconclusive));
    golden("incompatible.json", &report);
    after.info.hash_algo = Some("blake3".into());
    for version in [None, Some(2), Some(4)] {
        after.info.schema_version = version;
        assert_eq!(
            diff::compare(&before, &after).unwrap().comparison_state,
            State::Incompatible
        );
    }
    after.info.schema_version = Some(3);
    after.info.index_uuid = None;
    assert_eq!(
        diff::compare(&before, &after).unwrap().comparison_state,
        State::Incompatible
    );
}

#[test]
fn ambiguous_content_including_matched_and_shadowed_rows_never_moves() {
    for duplicate_path in [b"old".as_slice(), b"other", b"matched"] {
        let before = snapshot(
            "before",
            vec![
                entry(1, b"old", Some(b"same")),
                entry(2, duplicate_path, Some(b"same")),
            ],
        );
        let after = snapshot(
            "after",
            vec![
                entry(1, b"new", Some(b"same")),
                entry(2, b"matched", Some(b"same")),
            ],
        );
        assert_eq!(diff::compare(&before, &after).unwrap().summary.moved, 0);
        assert_eq!(diff::compare(&after, &before).unwrap().summary.moved, 0);
    }
}

#[test]
fn duplicate_paths_use_latest_id_and_raw_names_never_alias() {
    let mut first_raw = entry(4, b"raw-\xff", Some(b"ff"));
    first_raw.flags = flags::SHADOWED; // v3 display-path shadowing artifact
    let before = snapshot(
        "before",
        vec![
            entry(3, b"duplicate", Some(b"last")),
            entry(1, b"duplicate", Some(b"first")),
            first_raw,
            entry(5, b"raw-\xfe", Some(b"fe")),
        ],
    );
    let after = snapshot(
        "after",
        vec![
            entry(7, b"duplicate", Some(b"last")),
            entry(8, b"raw-\xfe", Some(b"fe")),
            entry(9, b"raw-\xff", Some(b"different")),
        ],
    );
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(
        (
            report.summary.byte_identical,
            report.summary.content_changed,
            report.summary.excluded
        ),
        (2, 1, 1)
    );
    assert_eq!(report.excluded[0].entry.file_id, 1);
    golden("raw_shadows.json", &report);
}

#[test]
fn unknown_hashes_hardlinks_and_sparse_evidence_are_honest() {
    let mut before = snapshot(
        "before",
        vec![
            entry(1, b"metadata-only", None),
            entry(2, b"hardlink", Some(b"target bytes")),
            entry(3, b"logical-sparse", Some(b"expanded holes")),
            entry(4, b"unsupported-sparse", None),
        ],
    );
    before.entries[1].entry_type = EntryType::Hardlink;
    before.entries[1].link_target = Some(b"raw-\xff".to_vec());
    before.entries[2].flags = flags::SPARSE;
    before.entries[3].flags = flags::SPARSE;
    let mut after = before.clone();
    after.info.index_uuid = Some("after".into());
    after.entries[1].link_target = Some(b"raw-\xfe".to_vec());
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(
        (report.summary.byte_identical, report.summary.inconclusive),
        (1, 3)
    );
    golden("unknown_sparse_links.json", &report);
    // Even an unmatched unknown row could contain identical content.
    // A globally unique move cannot be proved in that case.
    before.entries.push(entry(5, b"old", Some(b"rename")));
    after.entries.push(entry(5, b"new", Some(b"rename")));
    assert_eq!(diff::compare(&before, &after).unwrap().summary.moved, 0);
}

#[test]
fn missing_metadata_and_read_errors_do_not_become_equality() {
    let before = snapshot("before", vec![entry(1, b"a", Some(b"data"))]);
    for case in 0..6 {
        let mut after = before.clone();
        match case {
            0 => after.entries[0].mtime_unix = None,
            1 => after.entries[0].mode = None,
            2 => after.entries[0].flags = flags::PAX_UNPARSED,
            3 => after.entries[0].flags = flags::READ_ERROR,
            4 => after.entries[0].size += 1, // inconsistent hash/size
            _ => after.entries[0].entry_type = EntryType::Symlink,
        }
        assert_eq!(
            diff::compare(&before, &after).unwrap().summary.inconclusive,
            1,
            "case {case}"
        );
    }
    let mut after = before.clone();
    after.entries[0].flags = flags::FTS_TRUNCATED | flags::DECODE_FAILED | flags::IMAGE_OVER_CAP;
    assert_eq!(
        diff::compare(&before, &after)
            .unwrap()
            .summary
            .byte_identical,
        1
    );
    // Index enrichment flags are not filesystem metadata changes.
}

#[test]
fn move_requires_size_agreement_and_does_not_pair_over_existing_paths() {
    let before = snapshot("before", vec![entry(1, b"a", Some(b"old"))]);
    let mut after = snapshot("after", vec![entry(1, b"b", Some(b"old"))]);
    after.entries[0].size += 1;
    assert_eq!(diff::compare(&before, &after).unwrap().summary.moved, 0);
    after.entries[0].size -= 1;
    after.entries.push(entry(2, b"a", Some(b"new")));
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(
        (
            report.summary.moved,
            report.summary.content_changed,
            report.summary.added
        ),
        (0, 1, 1)
    );
}

#[test]
fn empty_snapshots_and_invalid_identity_do_not_hide_evidence() {
    let empty = snapshot("empty", vec![]);
    assert_eq!(
        diff::compare(&empty, &empty).unwrap().comparison_state,
        State::Complete
    );
    let mut invalid = snapshot("invalid", vec![entry(1, b"a", None), entry(1, b"b", None)]);
    assert!(diff::compare(&invalid, &empty).is_err());
    invalid.entries.pop();
    invalid.entries[0].file_id = 0;
    assert!(diff::compare(&invalid, &empty).is_err());
    invalid.entries[0].file_id = 1;
    invalid.info.state = State::Unavailable;
    assert!(diff::compare(&invalid, &empty).is_err());
}
