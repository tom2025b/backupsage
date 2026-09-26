//! Pure diff contract (#92). Index ingestion and CLI coverage belong to #93.
//! last_edited_by: codex
//! **Signed:** codex · 2026-09-26T12:05:09-04:00
use std::path::Path;

use backupsage::diff::{
    self, ChangeKind as Kind, Entry, EntryType, Reason, Side, Snapshot, SnapshotInfo,
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

// Compare exact identities and raw paths, so no row can silently disappear or
// be consumed twice while the aggregate move count still looks plausible.
type RowIdentity<'a> = Option<(i64, &'a str)>;

fn change_rows(report: &diff::DiffReport) -> Vec<(Kind, RowIdentity<'_>, RowIdentity<'_>)> {
    report
        .changes
        .iter()
        .map(|c| {
            (
                c.kind,
                c.before
                    .as_ref()
                    .map(|e| (e.file_id, e.path_bytes.as_str())),
                c.after.as_ref().map(|e| (e.file_id, e.path_bytes.as_str())),
            )
        })
        .collect()
}

#[test]
fn duplicate_sources_cannot_claim_one_move_target() {
    let before = snapshot(
        "before",
        vec![entry(1, b"a", Some(b"H")), entry(2, b"b", Some(b"H"))],
    );
    let after = snapshot("after", vec![entry(3, b"c", Some(b"H"))]);
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.summary.moved, 0);
    assert_eq!(
        change_rows(&report),
        vec![
            (Kind::Removed, Some((1, "61")), None),
            (Kind::Removed, Some((2, "62")), None),
            (Kind::Added, None, Some((3, "63"))),
        ]
    );
    assert!(report.excluded.is_empty());
}

#[test]
fn same_path_change_cannot_also_be_a_move_target() {
    // Exercise both processing orders: the same-path row may be visited
    // before or after the would-be move source in the raw-path namespace.
    for (source, source_hex) in [(b"x", "78"), (b"z", "7a")] {
        let before = snapshot(
            "before",
            vec![entry(1, source, Some(b"H")), entry(2, b"y", Some(b"Q"))],
        );
        let after = snapshot("after", vec![entry(3, b"y", Some(b"H"))]);
        let report = diff::compare(&before, &after).unwrap();
        assert_eq!(report.summary.moved, 0);
        let mut expected = vec![
            (Kind::Removed, Some((1, source_hex)), None),
            (Kind::ContentChanged, Some((2, "79")), Some((3, "79"))),
        ];
        expected.sort_by_key(|row| row.1.unwrap().1);
        assert_eq!(change_rows(&report), expected);
        assert_eq!(report.summary.added, 0);
        assert!(report.excluded.is_empty());
    }
}

#[test]
fn excluded_shadow_cannot_be_a_move_target() {
    let before = snapshot("before", vec![entry(1, b"x", Some(b"H"))]);
    // The hidden row's hash is unique on each side. Only the winning-row
    // guard prevents a move to it; no duplicate hash or unknown row masks it.
    let after = snapshot(
        "after",
        vec![entry(2, b"p", Some(b"H")), entry(3, b"p", Some(b"Z"))],
    );
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.summary.moved, 0);
    assert_eq!(
        change_rows(&report),
        vec![
            (Kind::Added, None, Some((3, "70"))),
            (Kind::Removed, Some((1, "78")), None),
        ]
    );
    assert_eq!(report.excluded.len(), 1);
    let excluded = &report.excluded[0];
    assert_eq!(excluded.side, Side::After);
    assert_eq!(excluded.reason, Reason::ShadowedPath);
    assert_eq!(excluded.entry.file_id, 2);
    assert_eq!(excluded.entry.path_bytes, "70");
}

#[test]
fn copied_hardlink_hash_cannot_prove_a_move() {
    let before = snapshot("before", vec![entry(1, b"f", Some(b"X"))]);
    let mut link = entry(2, b"g", Some(b"X"));
    link.entry_type = EntryType::Hardlink;
    link.link_target = Some(b"target".to_vec());
    let after = snapshot("after", vec![link]);
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.summary.moved, 0);
    assert_eq!(
        change_rows(&report),
        vec![
            (Kind::Removed, Some((1, "66")), None),
            (Kind::Added, None, Some((2, "67"))),
        ]
    );
    assert_eq!(
        report.changes[1].after.as_ref().unwrap().entry_type,
        EntryType::Hardlink
    );
    assert!(report.excluded.is_empty());
    // The copied hash must not become identity evidence in either direction.
    let reverse = diff::compare(&after, &before).unwrap();
    assert_eq!(reverse.summary.moved, 0);
    assert_eq!(
        change_rows(&reverse),
        vec![
            (Kind::Added, None, Some((1, "66"))),
            (Kind::Removed, Some((2, "67")), None),
        ]
    );
}

#[test]
fn several_excluded_rows_have_byte_identical_reports_in_any_input_order() {
    let mut before = snapshot(
        "before",
        vec![
            entry(4, b"d", Some(b"d old")),
            entry(3, b"c", Some(b"c middle")),
            entry(2, b"c", Some(b"c old")),
            entry(6, b"d", Some(b"d winner")),
            entry(5, b"c", Some(b"c winner")),
        ],
    );
    let mut after = before.clone();
    after.info.index_uuid = Some("after".into());
    after.entries.reverse();
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.summary.excluded, 6);
    assert_eq!(
        report
            .excluded
            .iter()
            .map(|e| (e.side, e.entry.path_bytes.as_str(), e.entry.file_id))
            .collect::<Vec<_>>(),
        vec![
            (Side::Before, "63", 2),
            (Side::Before, "63", 3),
            (Side::Before, "64", 4),
            (Side::After, "63", 2),
            (Side::After, "63", 3),
            (Side::After, "64", 4),
        ]
    );
    let expected = report.to_json().unwrap();
    // Vary the two inputs independently, including reversed ID order within
    // one shadowed path and reversed path order among different shadows.
    for _ in 0..before.entries.len() {
        before.entries.rotate_left(1);
        for _ in 0..after.entries.len() {
            after.entries.rotate_left(1);
            assert_eq!(
                diff::compare(&before, &after).unwrap().to_json().unwrap(),
                expected
            );
            after.entries.reverse();
            assert_eq!(
                diff::compare(&before, &after).unwrap().to_json().unwrap(),
                expected
            );
            after.entries.reverse();
        }
    }
}

#[test]
fn empty_content_does_not_prove_a_move_but_can_prove_same_path_equality() {
    let before = snapshot("before", vec![entry(1, b"a", Some(b""))]);
    let after = snapshot("after", vec![entry(2, b"z", Some(b""))]);
    let report = diff::compare(&before, &after).unwrap();
    assert_eq!(report.summary.moved, 0);
    assert_eq!(
        change_rows(&report),
        vec![
            (Kind::Removed, Some((1, "61")), None),
            (Kind::Added, None, Some((2, "7a"))),
        ]
    );
    let same = diff::compare(&before, &before).unwrap();
    assert_eq!(same.comparison_state, State::Complete);
    assert_eq!(
        change_rows(&same),
        vec![(Kind::ByteIdentical, Some((1, "61")), Some((1, "61")))]
    );
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
