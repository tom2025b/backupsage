//! Coverage grouping (#95): byte-identical content groups, hardlink and
//! shadow rules, and the #40 safety rule that unknown or unavailable data
//! never reads as zero copies.

use backupsage::coverage::{
    group, Coverage, CoverageRow, CoverageSource, EntryKind, ExclusionReason, Presence,
    ReplicaCount, RowRef, SourceEvidence, UnknownContentReason, UnknownReason,
};
use backupsage::store::flags;

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

fn unhashed(file_id: i64, path: &str, size: u64, row_flags: i64) -> CoverageRow {
    CoverageRow {
        file_id,
        path_raw: path.as_bytes().to_vec(),
        entry: EntryKind::File,
        size: Some(size),
        content_hash: None,
        flags: row_flags,
    }
}

fn link(file_id: i64, path: &str, entry: EntryKind, hash: Option<u8>) -> CoverageRow {
    CoverageRow {
        file_id,
        path_raw: path.as_bytes().to_vec(),
        entry,
        size: Some(0),
        content_hash: hash.map(h),
        flags: 0,
    }
}

fn source(source_id: i64, evidence: SourceEvidence, rows: Vec<CoverageRow>) -> CoverageSource {
    CoverageSource {
        source_id,
        label: format!("src-{source_id}"),
        evidence,
        rows,
    }
}

fn complete(source_id: i64, rows: Vec<CoverageRow>) -> CoverageSource {
    source(source_id, SourceEvidence::Complete, rows)
}

fn rref(source_id: i64, path: &str, file_id: i64) -> RowRef {
    RowRef {
        source_id,
        path_raw: path.as_bytes().to_vec(),
        file_id,
    }
}

/// Run the engine and check invariants every result must satisfy.
fn run(sources: &[CoverageSource]) -> Coverage {
    let cov = group(sources).expect("valid input");
    for g in &cov.groups {
        assert!(
            g.replicas.observed() >= 1,
            "a group must never report zero replicas: {g:?}"
        );
        assert_eq!(g.presence.len(), cov.sources.len());
    }
    cov
}

fn find(cov: &Coverage, hash: u8) -> &backupsage::coverage::ContentGroup {
    cov.groups
        .iter()
        .find(|g| g.content_hash == h(hash))
        .unwrap_or_else(|| panic!("no group for hash {hash}"))
}

fn presence(cov: &Coverage, hash: u8, source_id: i64) -> Presence {
    find(cov, hash)
        .presence
        .iter()
        .find(|p| p.source_id == source_id)
        .unwrap()
        .presence
}

// ── Grouping ────────────────────────────────────────────────────────────────

#[test]
fn groups_byte_identical_content_across_sources_by_hash_not_name() {
    let cov = run(&[
        complete(1, vec![file(1, "a/x.txt", 1, 5), file(2, "a/z.txt", 2, 7)]),
        complete(2, vec![file(1, "renamed.bin", 1, 5)]),
    ]);

    assert_eq!(cov.groups.len(), 2);
    let g1 = find(&cov, 1);
    assert_eq!(g1.replicas, ReplicaCount::Exact(2));
    assert_eq!(g1.size, Some(5));
    assert_eq!(
        g1.copies,
        vec![rref(1, "a/x.txt", 1), rref(2, "renamed.bin", 1)]
    );

    let g2 = find(&cov, 2);
    assert_eq!(g2.replicas, ReplicaCount::Exact(1));
    assert_eq!(presence(&cov, 2, 1), Presence::Present { copies: 1 });
    assert_eq!(presence(&cov, 2, 2), Presence::Absent);
}

#[test]
fn repeated_content_inside_one_source_is_one_replica() {
    let cov = run(&[complete(
        1,
        vec![file(1, "a", 1, 5), file(2, "b", 1, 5), file(3, "c", 1, 5)],
    )]);
    let g = find(&cov, 1);
    assert_eq!(g.replicas, ReplicaCount::Exact(1));
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 3 });
    assert_eq!(g.copies.len(), 3);
}

#[test]
fn incomplete_source_with_an_observed_copy_is_present() {
    let cov = run(&[
        source(1, SourceEvidence::Incomplete, vec![file(1, "a", 1, 5)]),
        complete(2, vec![file(1, "b", 2, 5)]),
    ]);
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 1 });
    assert_eq!(presence(&cov, 1, 2), Presence::Absent);
    assert_eq!(find(&cov, 1).replicas, ReplicaCount::Exact(1));
}

// ── Shadow rules ────────────────────────────────────────────────────────────

#[test]
fn latest_row_at_a_raw_path_wins_and_shadowed_rows_never_count() {
    let cov = run(&[
        complete(1, vec![file(1, "p", 1, 5), file(2, "p", 2, 5)]),
        complete(2, vec![file(1, "q", 3, 5)]),
    ]);

    // Content 1 lives only in a shadowed row: it forms no group rather
    // than a group with zero replicas.
    assert!(cov.groups.iter().all(|g| g.content_hash != h(1)));
    assert_eq!(find(&cov, 2).copies, vec![rref(1, "p", 2)]);
    assert_eq!(cov.exclusions.len(), 1);
    assert_eq!(cov.exclusions[0].row, rref(1, "p", 1));
    assert_eq!(cov.exclusions[0].reason, ExclusionReason::Shadowed);
}

#[test]
fn shadowed_duplicate_of_effective_content_is_not_an_extra_copy() {
    let cov = run(&[complete(1, vec![file(1, "p", 1, 5), file(2, "p", 1, 5)])]);
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 1 });
    assert_eq!(find(&cov, 1).copies, vec![rref(1, "p", 2)]);
    assert_eq!(cov.exclusions[0].reason, ExclusionReason::Shadowed);
}

#[test]
fn shadowing_is_keyed_on_raw_bytes_not_display_text() {
    // Two distinct non-UTF-8 names that render identically. The stored v3
    // SHADOWED flag was computed on display text and may mark the first.
    let mut first = file(1, "", 1, 5);
    first.path_raw = vec![b'd', 0xff];
    first.flags = flags::SHADOWED;
    let mut second = file(2, "", 2, 5);
    second.path_raw = vec![b'd', 0xfe];
    assert_eq!(
        String::from_utf8_lossy(&first.path_raw),
        String::from_utf8_lossy(&second.path_raw)
    );

    let cov = run(&[complete(1, vec![first, second])]);
    assert_eq!(cov.groups.len(), 2);
    assert!(cov.exclusions.is_empty());
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 1 });
    assert_eq!(presence(&cov, 2, 1), Presence::Present { copies: 1 });
}

#[test]
fn shadowed_unhashed_row_adds_no_uncertainty() {
    let cov = run(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(
            2,
            vec![unhashed(1, "p", 5, flags::READ_ERROR), file(2, "p", 2, 9)],
        ),
    ]);
    assert_eq!(presence(&cov, 1, 2), Presence::Absent);
    assert!(cov.unknown_content.is_empty());
    assert_eq!(cov.exclusions[0].reason, ExclusionReason::Shadowed);
}

// ── Hardlink and symlink rules ──────────────────────────────────────────────

#[test]
fn hardlinks_are_aliases_never_copies() {
    let cov = run(&[complete(
        1,
        vec![
            file(1, "f", 1, 5),
            link(2, "l", EntryKind::Hardlink, Some(1)),
        ],
    )]);
    let g = find(&cov, 1);
    assert_eq!(g.copies, vec![rref(1, "f", 1)]);
    assert_eq!(g.aliases, vec![rref(1, "l", 2)]);
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 1 });
    assert_eq!(g.replicas, ReplicaCount::Exact(1));
}

#[test]
fn hardlink_without_a_same_source_copy_is_excluded_and_changes_nothing() {
    let cov = run(&[
        complete(
            1,
            vec![
                link(1, "l", EntryKind::Hardlink, Some(1)),
                link(2, "m", EntryKind::Hardlink, None),
            ],
        ),
        complete(2, vec![file(1, "f", 1, 5)]),
    ]);
    let g = find(&cov, 1);
    assert!(g.aliases.is_empty());
    assert_eq!(g.replicas, ReplicaCount::Exact(1));
    assert_eq!(presence(&cov, 1, 1), Presence::Absent);
    assert!(cov.unknown_content.is_empty());
    let reasons: Vec<_> = cov.exclusions.iter().map(|e| e.reason).collect();
    assert_eq!(
        reasons,
        vec![
            ExclusionReason::UnmatchedHardlink,
            ExclusionReason::UnmatchedHardlink
        ]
    );
}

#[test]
fn hardlink_alone_never_forms_a_group() {
    let cov = run(&[complete(
        1,
        vec![link(1, "l", EntryKind::Hardlink, Some(1))],
    )]);
    assert!(cov.groups.is_empty());
    assert_eq!(cov.exclusions[0].reason, ExclusionReason::UnmatchedHardlink);
}

#[test]
fn symlinks_are_excluded() {
    let cov = run(&[complete(
        1,
        vec![file(1, "f", 1, 5), link(2, "s", EntryKind::Symlink, None)],
    )]);
    assert_eq!(cov.exclusions.len(), 1);
    assert_eq!(cov.exclusions[0].row, rref(1, "s", 2));
    assert_eq!(cov.exclusions[0].reason, ExclusionReason::Symlink);
    assert_eq!(presence(&cov, 1, 1), Presence::Present { copies: 1 });
}

// ── Safety rule: unknown data never reads as zero copies ────────────────────

#[test]
fn unavailable_source_is_unknown_never_zero_copies() {
    let cov = run(&[
        complete(1, vec![file(1, "only-here", 1, 5)]),
        source(2, SourceEvidence::Unavailable, vec![]),
    ]);
    let p = presence(&cov, 1, 2);
    assert_ne!(
        p,
        Presence::Absent,
        "unavailable source read as zero copies"
    );
    assert_eq!(p, Presence::Unknown(UnknownReason::SourceUnavailable));
    assert_eq!(find(&cov, 1).replicas, ReplicaCount::AtLeast(1));
    assert!(!find(&cov, 1).replicas.is_exact());
}

#[test]
fn every_non_complete_evidence_state_is_unknown_never_absent() {
    let cases = [
        (
            SourceEvidence::Unavailable,
            vec![],
            UnknownReason::SourceUnavailable,
        ),
        (
            SourceEvidence::Incomplete,
            vec![file(1, "other", 9, 5)],
            UnknownReason::SourceIncomplete,
        ),
        (
            SourceEvidence::NoContentHashes,
            vec![unhashed(1, "other", 99, 0)],
            UnknownReason::SourceHasNoContentHashes,
        ),
    ];
    for (evidence, rows, reason) in cases {
        let cov = run(&[
            complete(1, vec![file(1, "a", 1, 5)]),
            source(2, evidence, rows),
        ]);
        assert_eq!(
            presence(&cov, 1, 2),
            Presence::Unknown(reason),
            "{evidence:?} must not read as absent"
        );
        assert_eq!(
            find(&cov, 1).replicas,
            ReplicaCount::AtLeast(1),
            "{evidence:?} must turn the count into a lower bound"
        );
    }
}

#[test]
fn unhashed_row_that_could_match_makes_a_complete_source_unknown() {
    let cov = run(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![unhashed(1, "broken", 5, flags::READ_ERROR)]),
    ]);
    assert_eq!(
        presence(&cov, 1, 2),
        Presence::Unknown(UnknownReason::UnhashedRowsMayMatch { rows: 1 })
    );
    assert_eq!(find(&cov, 1).replicas, ReplicaCount::AtLeast(1));
    assert_eq!(cov.unknown_content.len(), 1);
    assert_eq!(cov.unknown_content[0].row, rref(2, "broken", 1));
    assert_eq!(
        cov.unknown_content[0].reason,
        UnknownContentReason::ReadError
    );
}

#[test]
fn hashed_row_with_a_read_error_is_unknown_content_not_a_copy() {
    let mut bad = file(1, "a", 1, 5);
    bad.flags = flags::READ_ERROR;
    let cov = run(&[
        complete(1, vec![bad]),
        complete(2, vec![file(1, "b", 1, 5)]),
    ]);
    assert_eq!(find(&cov, 1).copies, vec![rref(2, "b", 1)]);
    assert_eq!(
        presence(&cov, 1, 1),
        Presence::Unknown(UnknownReason::UnhashedRowsMayMatch { rows: 1 })
    );
    assert_eq!(
        cov.unknown_content[0].reason,
        UnknownContentReason::ReadError
    );
}

#[test]
fn trusted_differing_size_rules_out_an_unhashed_row() {
    let cov = run(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![unhashed(1, "other", 99, 0)]),
    ]);
    assert_eq!(presence(&cov, 1, 2), Presence::Absent);
    assert_eq!(find(&cov, 1).replicas, ReplicaCount::Exact(1));
    assert_eq!(
        cov.unknown_content[0].reason,
        UnknownContentReason::NotHashed
    );
}

#[test]
fn sparse_pax_unparsed_and_unknown_sizes_are_not_trusted() {
    let mut unknown_size = unhashed(1, "u", 0, 0);
    unknown_size.size = None;
    let cases = [
        (
            unhashed(1, "s", 99, flags::SPARSE),
            UnknownContentReason::UnsupportedSparse,
        ),
        (
            unhashed(1, "p", 99, flags::PAX_UNPARSED),
            UnknownContentReason::NotHashed,
        ),
        (unknown_size, UnknownContentReason::NotHashed),
    ];
    for (row, reason) in cases {
        let cov = run(&[
            complete(1, vec![file(1, "a", 1, 5)]),
            complete(2, vec![row]),
        ]);
        assert_eq!(
            presence(&cov, 1, 2),
            Presence::Unknown(UnknownReason::UnhashedRowsMayMatch { rows: 1 })
        );
        assert_eq!(cov.unknown_content[0].reason, reason);
    }
}

#[test]
fn group_without_a_trusted_size_cannot_rule_out_unhashed_rows() {
    // An old-GNU sparse entry's hash covers the logical stream, so it groups,
    // but its recorded size is not the content length.
    let mut sparse = file(1, "a", 1, 3);
    sparse.flags = flags::SPARSE;
    let cov = run(&[
        complete(1, vec![sparse]),
        complete(2, vec![unhashed(1, "other", 99, 0)]),
    ]);
    assert_eq!(find(&cov, 1).size, None);
    assert_eq!(
        presence(&cov, 1, 2),
        Presence::Unknown(UnknownReason::UnhashedRowsMayMatch { rows: 1 })
    );
}

#[test]
fn disagreeing_trusted_sizes_leave_the_length_unknown() {
    let cov = run(&[
        complete(1, vec![file(1, "a", 1, 5)]),
        complete(2, vec![file(1, "b", 1, 6)]),
        complete(3, vec![unhashed(1, "c", 99, 0)]),
    ]);
    assert_eq!(find(&cov, 1).size, None);
    assert!(matches!(presence(&cov, 1, 3), Presence::Unknown(_)));
}

// ── Accounting, determinism and input validation ────────────────────────────

fn mixed_sources() -> Vec<CoverageSource> {
    vec![
        complete(
            3,
            vec![
                file(4, "z", 2, 7),
                file(1, "a", 1, 5),
                link(2, "l", EntryKind::Hardlink, Some(1)),
                link(3, "s", EntryKind::Symlink, None),
                file(5, "a", 3, 5),
            ],
        ),
        source(1, SourceEvidence::Unavailable, vec![]),
        complete(
            2,
            vec![
                unhashed(2, "broken", 5, flags::READ_ERROR),
                file(1, "copy", 1, 5),
            ],
        ),
        source(4, SourceEvidence::Incomplete, vec![file(1, "p", 2, 7)]),
    ]
}

#[test]
fn every_row_is_accounted_for_exactly_once() {
    let sources = mixed_sources();
    let cov = run(&sources);

    let mut seen: Vec<RowRef> = Vec::new();
    for g in &cov.groups {
        seen.extend(g.copies.iter().cloned());
        seen.extend(g.aliases.iter().cloned());
    }
    seen.extend(cov.unknown_content.iter().map(|u| u.row.clone()));
    seen.extend(cov.exclusions.iter().map(|e| e.row.clone()));
    seen.sort();

    let mut expected: Vec<RowRef> = sources
        .iter()
        .flat_map(|s| {
            s.rows.iter().map(|r| RowRef {
                source_id: s.source_id,
                path_raw: r.path_raw.clone(),
                file_id: r.file_id,
            })
        })
        .collect();
    expected.sort();
    assert_eq!(seen, expected);
}

#[test]
fn output_is_ordered_and_independent_of_input_order() {
    let forward = run(&mixed_sources());

    let mut reversed = mixed_sources();
    reversed.reverse();
    for s in &mut reversed {
        s.rows.reverse();
    }
    let backward = run(&reversed);
    assert_eq!(format!("{forward:?}"), format!("{backward:?}"));

    let ids: Vec<i64> = forward.sources.iter().map(|s| s.source_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    let hashes: Vec<[u8; 32]> = forward.groups.iter().map(|g| g.content_hash).collect();
    let mut sorted = hashes.clone();
    sorted.sort();
    assert_eq!(hashes, sorted);
    for g in &forward.groups {
        let presence_ids: Vec<i64> = g.presence.iter().map(|p| p.source_id).collect();
        assert_eq!(presence_ids, ids);
        let mut copies = g.copies.clone();
        copies.sort();
        assert_eq!(g.copies, copies);
    }
}

#[test]
fn contradictory_input_is_refused() {
    let dup_source = [complete(1, vec![]), complete(1, vec![])];
    let dup_file = [complete(1, vec![file(1, "a", 1, 5), file(1, "b", 2, 5)])];
    let rows_on_unavailable = [source(
        1,
        SourceEvidence::Unavailable,
        vec![file(1, "a", 1, 5)],
    )];
    let hash_on_hashless = [source(
        1,
        SourceEvidence::NoContentHashes,
        vec![file(1, "a", 1, 5)],
    )];
    for (name, input) in [
        ("duplicate source id", &dup_source[..]),
        ("duplicate file id", &dup_file[..]),
        ("rows on an unavailable source", &rows_on_unavailable[..]),
        ("hash on a hash-less source", &hash_on_hashless[..]),
    ] {
        assert!(group(input).is_err(), "{name} must be refused");
    }
}

#[test]
fn display_path_is_lossy_but_raw_bytes_are_kept() {
    let r = RowRef {
        source_id: 1,
        path_raw: vec![b'x', 0xff],
        file_id: 1,
    };
    assert_eq!(r.display_path(), "x\u{fffd}");
    assert_eq!(r.path_raw, vec![b'x', 0xff]);
}
