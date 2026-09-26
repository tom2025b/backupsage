//! Minimum-copy floors (#96): verdicts over the #95 coverage engine, the
//! source-status trust rule, scope exclusions, determinism and floor bounds.

use backupsage::coverage::{
    group, Coverage, CoverageRow, CoverageSource, EntryKind, ExclusionReason, RowRef,
    SourceEvidence,
};
use backupsage::floors::{
    evaluate, FloorParams, FloorReport, GroupExclusionReason, GroupFloor, SourceStatus, Verdict,
    DEFAULT_MIN_COPIES,
};
use backupsage::store::flags;

const ALL_STATUSES: [SourceStatus; 5] = [
    SourceStatus::Ok,
    SourceStatus::Incomplete,
    SourceStatus::StaleIndex,
    SourceStatus::DbMissing,
    SourceStatus::ArchiveMissing,
];

fn h(n: u8) -> [u8; 32] {
    [n; 32]
}

fn file(file_id: i64, path: &str, hash: u8, size: u64) -> CoverageRow {
    CoverageRow {
        file_id,
        path_raw: path.as_bytes().to_vec(),
        entry: EntryKind::File,
        size: Some(size),
        content_hash: Some(h(hash)),
        flags: 0,
    }
}

fn unhashed(file_id: i64, path: &str, size: u64) -> CoverageRow {
    CoverageRow {
        content_hash: None,
        ..file(file_id, path, 0, size)
    }
}

fn row(file_id: i64, path: &str, entry: EntryKind, hash: Option<u8>) -> CoverageRow {
    CoverageRow {
        entry,
        content_hash: hash.map(h),
        ..file(file_id, path, 0, 0)
    }
}

fn source(id: i64, evidence: SourceEvidence, rows: Vec<CoverageRow>) -> CoverageSource {
    CoverageSource {
        source_id: id,
        label: format!("src-{id}"),
        evidence,
        rows,
    }
}

fn complete(id: i64, rows: Vec<CoverageRow>) -> CoverageSource {
    source(id, SourceEvidence::Complete, rows)
}

fn cov(sources: &[CoverageSource]) -> Coverage {
    group(sources).expect("valid coverage input")
}

fn all_ok(c: &Coverage) -> Vec<(i64, SourceStatus)> {
    c.sources
        .iter()
        .map(|s| (s.source_id, SourceStatus::Ok))
        .collect()
}

fn floor(n: usize) -> FloorParams {
    FloorParams {
        min_copies: n,
        ..FloorParams::default()
    }
}

/// Evaluate and check invariants every report must satisfy.
fn eval(c: &Coverage, statuses: &[(i64, SourceStatus)], p: &FloorParams) -> FloorReport {
    let r = evaluate(c, statuses, p).expect("valid floor input");
    let s = &r.summary;
    assert_eq!(s.min_copies, p.min_copies);
    assert_eq!(s.groups, r.groups.len());
    assert_eq!(s.meets_floor + s.below_floor + s.inconclusive, s.groups);
    for g in &r.groups {
        if g.only_copy {
            assert_eq!(g.trusted_replicas, 1);
            assert_eq!(g.unknown_sources, 0);
        }
        if g.verdict == Verdict::MeetsFloor {
            assert!(g.trusted_replicas >= p.min_copies);
        } else {
            assert!(g.trusted_replicas < p.min_copies);
        }
        if g.verdict == Verdict::BelowFloor {
            assert_eq!(g.unknown_sources, 0, "unknown presence read as below floor");
        }
    }
    r
}

fn only(r: &FloorReport) -> &GroupFloor {
    assert_eq!(r.groups.len(), 1, "{:?}", r.groups);
    &r.groups[0]
}

fn rref(source_id: i64, path: &str, file_id: i64) -> RowRef {
    RowRef {
        source_id,
        path_raw: path.as_bytes().to_vec(),
        file_id,
    }
}

// ── Invariant 1: unknown never reads as below the floor or as met ──────────

#[test]
fn lower_bound_below_the_floor_is_inconclusive_never_below_or_met() {
    for evidence in [
        SourceEvidence::Unavailable,
        SourceEvidence::Incomplete,
        SourceEvidence::NoContentHashes,
    ] {
        let rows = match evidence {
            SourceEvidence::Unavailable => vec![],
            _ => vec![unhashed(1, "other", 99)],
        };
        let c = cov(&[
            complete(1, vec![file(1, "a", 1, 5)]),
            source(2, evidence, rows),
        ]);
        let r = eval(&c, &all_ok(&c), &floor(2));
        let g = only(&r);
        assert_eq!(g.verdict, Verdict::Inconclusive, "{evidence:?}");
        assert_eq!(g.trusted_replicas, 1);
        assert_eq!(g.unknown_sources, 1);
        assert!(!g.only_copy, "unknown presence cannot be only-copy");
        assert_eq!(r.summary.inconclusive, 1);
        assert_eq!(r.summary.below_floor, 0);
    }
}

#[test]
fn possibly_matching_unhashed_row_is_inconclusive() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![unhashed(1, "same-size", 5)]),
    ]);
    let r = eval(&c, &all_ok(&c), &floor(2));
    assert_eq!(only(&r).verdict, Verdict::Inconclusive);
}

#[test]
fn many_unknown_sources_never_add_up_to_meeting_the_floor() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        source(2, SourceEvidence::Unavailable, vec![]),
        source(3, SourceEvidence::Unavailable, vec![]),
        source(4, SourceEvidence::Unavailable, vec![]),
    ]);
    let r = eval(&c, &all_ok(&c), &floor(2));
    let g = only(&r);
    assert_eq!(g.verdict, Verdict::Inconclusive);
    assert_eq!(g.unknown_sources, 3);
    assert_eq!(r.summary.meets_floor, 0);
}

#[test]
fn lower_bound_at_or_above_the_floor_meets_it() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![file(1, "b", 1, 5)]),
        source(3, SourceEvidence::Unavailable, vec![]),
    ]);
    let r = eval(&c, &all_ok(&c), &floor(2));
    let g = only(&r);
    assert_eq!(g.verdict, Verdict::MeetsFloor);
    assert_eq!(g.unknown_sources, 1);
}

#[test]
fn exact_count_below_the_floor_is_below_floor_and_only_copy() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![file(1, "b", 2, 5)]),
    ]);
    let r = eval(&c, &all_ok(&c), &floor(2));
    for g in &r.groups {
        assert_eq!(g.verdict, Verdict::BelowFloor);
        assert!(g.only_copy);
        assert_eq!(g.trusted_replicas, 1);
    }
    assert_eq!(r.summary.below_floor, 2);
    assert_eq!(r.summary.only_copy, 2);
}

#[test]
fn only_copy_is_its_own_class_even_when_the_floor_is_met() {
    let c = cov(&[complete(1, vec![file(1, "a", 1, 5)])]);
    let r = eval(&c, &all_ok(&c), &floor(1));
    let g = only(&r);
    assert_eq!(g.verdict, Verdict::MeetsFloor);
    assert!(g.only_copy);
}

// ── Source-status trust rule ────────────────────────────────────────────────

#[test]
fn only_ok_and_incomplete_sources_count_toward_the_floor() {
    for status in ALL_STATUSES {
        let expected = matches!(status, SourceStatus::Ok | SourceStatus::Incomplete);
        assert_eq!(status.counts_toward_floor(), expected, "{status:?}");

        let c = cov(&[
            complete(1, vec![file(1, "a", 1, 5)]),
            complete(2, vec![file(1, "b", 1, 5)]),
        ]);
        let statuses = [(1, SourceStatus::Ok), (2, status)];
        let r = eval(&c, &statuses, &floor(2));
        let g = only(&r);
        if expected {
            assert_eq!(g.verdict, Verdict::MeetsFloor, "{status:?}");
            assert_eq!((g.trusted_replicas, g.untrusted_replicas), (2, 0));
            assert!(!g.only_copy);
        } else {
            // Fully known: the second copy exists but is not trustworthy.
            assert_eq!(g.verdict, Verdict::BelowFloor, "{status:?}");
            assert_eq!((g.trusted_replicas, g.untrusted_replicas), (1, 1));
            assert!(g.only_copy, "one trustworthy copy");
        }
    }
}

#[test]
fn every_copy_is_listed_with_label_and_status_trusted_or_not() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5), file(2, "a2", 1, 5)]),
        complete(2, vec![file(1, "b", 1, 5)]),
    ]);
    let statuses = [(2, SourceStatus::ArchiveMissing), (1, SourceStatus::Ok)];
    let r = eval(&c, &statuses, &floor(2));
    let got: Vec<_> = only(&r)
        .copies
        .iter()
        .map(|c| {
            (
                c.row.clone(),
                c.source_label.as_str(),
                c.status,
                c.counts_toward_floor,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (rref(1, "a", 1), "src-1", SourceStatus::Ok, true),
            (rref(1, "a2", 2), "src-1", SourceStatus::Ok, true),
            (
                rref(2, "b", 1),
                "src-2",
                SourceStatus::ArchiveMissing,
                false
            ),
        ]
    );
}

#[test]
fn status_input_must_name_every_source_exactly_once() {
    let c = cov(&[complete(1, vec![file(1, "a", 1, 5)]), complete(2, vec![])]);
    let p = floor(2);
    let missing = [(1, SourceStatus::Ok)];
    let duplicate = [
        (1, SourceStatus::Ok),
        (1, SourceStatus::Ok),
        (2, SourceStatus::Ok),
    ];
    let unknown_id = [
        (1, SourceStatus::Ok),
        (2, SourceStatus::Ok),
        (3, SourceStatus::Ok),
    ];
    for (name, statuses) in [
        ("missing", &missing[..]),
        ("duplicate", &duplicate[..]),
        ("unknown id", &unknown_id[..]),
    ] {
        assert!(
            evaluate(&c, statuses, &p).is_err(),
            "{name} must be refused"
        );
    }
}

// ── Scope exclusions ────────────────────────────────────────────────────────

#[test]
fn empty_and_small_content_is_excluded_with_its_copies_listed() {
    let c = cov(&[complete(
        1,
        vec![
            file(1, "empty", 0, 0),
            file(2, "tiny", 1, 3),
            file(3, "big", 2, 500),
        ],
    )]);
    let p = FloorParams {
        min_copies: 2,
        min_size: 10,
        include_empty: false,
    };
    let r = eval(&c, &all_ok(&c), &p);
    assert_eq!(only(&r).content_hash, h(2));
    let excluded: Vec<_> = r
        .excluded_groups
        .iter()
        .map(|g| (g.content_hash, g.reason, g.copies.clone()))
        .collect();
    assert_eq!(
        excluded,
        vec![
            (
                h(0),
                GroupExclusionReason::EmptyContent,
                vec![rref(1, "empty", 1)]
            ),
            (
                h(1),
                GroupExclusionReason::BelowMinSize,
                vec![rref(1, "tiny", 2)]
            ),
        ]
    );
    assert_eq!(r.summary.excluded_groups, 2);

    let keep_empty = FloorParams {
        include_empty: true,
        min_size: 0,
        ..p
    };
    let r = eval(&c, &all_ok(&c), &keep_empty);
    assert_eq!(r.groups.len(), 3);
    assert!(r.excluded_groups.is_empty());
}

#[test]
fn unknown_length_is_never_excluded_by_size() {
    let mut sparse = file(1, "s", 1, 3);
    sparse.flags = flags::SPARSE;
    let c = cov(&[complete(1, vec![sparse])]);
    let p = FloorParams {
        min_copies: 2,
        min_size: 1000,
        include_empty: false,
    };
    let r = eval(&c, &all_ok(&c), &p);
    assert_eq!(only(&r).size, None);
    assert!(r.excluded_groups.is_empty());
}

#[test]
fn row_exclusions_aliases_and_unknown_rows_are_counted() {
    let c = cov(&[complete(
        1,
        vec![
            file(1, "p", 1, 5),
            file(2, "p", 2, 5),
            row(3, "s", EntryKind::Symlink, None),
            row(4, "l", EntryKind::Hardlink, Some(2)),
            row(5, "m", EntryKind::Hardlink, Some(9)),
            unhashed(6, "u", 77),
        ],
    )]);
    let r = eval(&c, &all_ok(&c), &floor(1));
    let s = &r.summary;
    assert_eq!(
        (
            s.shadowed_rows,
            s.symlink_rows,
            s.unmatched_hardlink_rows,
            s.hardlink_aliases,
            s.unknown_content_rows
        ),
        (1, 1, 1, 1, 1)
    );
    let reasons: Vec<_> = r.excluded_rows.iter().map(|e| e.reason).collect();
    assert_eq!(
        reasons,
        vec![
            ExclusionReason::UnmatchedHardlink,
            ExclusionReason::Shadowed,
            ExclusionReason::Symlink,
        ]
    );
}

// ── Invariant 3: ordering and totals independent of input order ────────────

fn mixed() -> Vec<CoverageSource> {
    vec![
        complete(
            3,
            vec![
                file(3, "z", 2, 7),
                file(1, "b", 1, 5),
                file(2, "a", 1, 5),
                file(4, "e", 0, 0),
            ],
        ),
        source(1, SourceEvidence::Unavailable, vec![]),
        complete(2, vec![file(2, "y", 3, 9), file(1, "x", 1, 5)]),
        source(4, SourceEvidence::Incomplete, vec![file(1, "q", 2, 7)]),
    ]
}

fn mixed_statuses() -> Vec<(i64, SourceStatus)> {
    vec![
        (4, SourceStatus::Incomplete),
        (2, SourceStatus::StaleIndex),
        (1, SourceStatus::DbMissing),
        (3, SourceStatus::Ok),
    ]
}

#[test]
fn report_is_identical_for_any_input_order() {
    let p = floor(2);
    let base = eval(&cov(&mixed()), &mixed_statuses(), &p);
    let expected = format!("{base:?}");

    let hashes: Vec<[u8; 32]> = base.groups.iter().map(|g| g.content_hash).collect();
    let mut sorted = hashes.clone();
    sorted.sort();
    assert_eq!(hashes, sorted);
    let copies: Vec<RowRef> = base
        .groups
        .iter()
        .find(|g| g.content_hash == h(1))
        .unwrap()
        .copies
        .iter()
        .map(|c| c.row.clone())
        .collect();
    assert_eq!(
        copies,
        vec![rref(2, "x", 1), rref(3, "a", 2), rref(3, "b", 1)]
    );

    for shift in 0..4 {
        for reverse in [false, true] {
            let mut sources = mixed();
            sources.rotate_left(shift);
            let mut statuses = mixed_statuses();
            statuses.rotate_left(shift);
            if reverse {
                sources.reverse();
                statuses.reverse();
            }
            for s in &mut sources {
                let n = s.rows.len().max(1);
                s.rows.rotate_left(shift % n);
                if reverse {
                    s.rows.reverse();
                }
            }
            let got = eval(&cov(&sources), &statuses, &p);
            assert_eq!(format!("{got:?}"), expected);
        }
    }
}

#[test]
fn totals_are_derived_from_the_emitted_rows() {
    let r = eval(&cov(&mixed()), &mixed_statuses(), &floor(2));
    let count = |v: Verdict| r.groups.iter().filter(|g| g.verdict == v).count();
    let s = &r.summary;
    assert_eq!(s.meets_floor, count(Verdict::MeetsFloor));
    assert_eq!(s.below_floor, count(Verdict::BelowFloor));
    assert_eq!(s.inconclusive, count(Verdict::Inconclusive));
    assert_eq!(s.only_copy, r.groups.iter().filter(|g| g.only_copy).count());
    assert_eq!(s.excluded_groups, r.excluded_groups.len());
    // Source 1 is unavailable, so no in-scope group can be conclusive here.
    assert_eq!(
        (s.groups, s.inconclusive, s.excluded_groups),
        (3, 3, 1),
        "{r:?}"
    );
}

// ── Invariant 4: floor bounds ───────────────────────────────────────────────

#[test]
fn default_floor_is_two() {
    assert_eq!(DEFAULT_MIN_COPIES, 2);
    assert_eq!(FloorParams::default().min_copies, 2);
}

#[test]
fn floor_of_zero_is_refused() {
    let c = cov(&[complete(1, vec![file(1, "a", 1, 5)])]);
    let err = evaluate(&c, &all_ok(&c), &floor(0)).unwrap_err();
    assert!(err.to_string().contains("at least 1"), "{err}");
}

#[test]
fn floor_above_the_number_of_sources_is_never_met_and_never_panics() {
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5), file(2, "only", 2, 5)]),
        complete(2, vec![file(1, "b", 1, 5)]),
        source(3, SourceEvidence::Unavailable, vec![]),
    ]);
    for min_copies in [4, 1000, usize::MAX] {
        let r = eval(&c, &all_ok(&c), &floor(min_copies));
        assert_eq!(r.summary.meets_floor, 0, "floor {min_copies}");
        // Unknown presence in source 3: inconclusive, never below floor.
        assert_eq!(r.summary.inconclusive, 2, "floor {min_copies}");
    }

    // Without unknown presence, the same floors give fully known results.
    let c = cov(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![file(1, "b", 1, 5)]),
    ]);
    for min_copies in [3, usize::MAX] {
        let r = eval(&c, &all_ok(&c), &floor(min_copies));
        assert_eq!(only(&r).verdict, Verdict::BelowFloor, "floor {min_copies}");
        assert_eq!(only(&r).trusted_replicas, 2);
    }
}
