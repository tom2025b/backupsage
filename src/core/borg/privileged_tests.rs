//! Privileged acceptance tests for ADR 0008. These require a real Btrfs
//! filesystem with a genuine `btrfs send`/`receive` round trip and, for later
//! additions to this file, Landlock ABI 3+ and seccomp-filter support.
//!
//! Per ADR 0008's own CI/acceptance split: "Ordinary CI may test parsing,
//! sealed types, syscall-policy generation, FD lifetime plumbing and
//! fail-closed behavior. A mock may never produce the production capability
//! or make the backend enabled... Backend acceptance requires a separate
//! disposable, privileged Ubuntu VM." These tests are `#[ignore]`d for that
//! reason — `cargo test` never runs them by default, including in ordinary
//! CI. Run explicitly, only inside the disposable fixture, with:
//!
//!   cargo test -p backupsage-core --lib borg::privileged_tests -- --ignored --nocapture
//!
//! Set BACKUPSAGE_BTRFS_FIXTURE to the path of a real received Btrfs
//! subvolume before running. Never point this at a production repository or
//! `/mnt/borgnvme` — see ADR 0008's acceptance-fixture requirements.
use super::btrfs_uapi::{query_subvolume_facts, BtrfsQueryError};
use std::os::fd::AsRawFd;

fn fixture_path() -> std::path::PathBuf {
    std::env::var_os("BACKUPSAGE_BTRFS_FIXTURE")
        .map(std::path::PathBuf::from)
        .expect(
            "set BACKUPSAGE_BTRFS_FIXTURE to a real received Btrfs subvolume path \
             before running privileged tests",
        )
}

/// Positive control: the ioctl-derived facts for a genuine `btrfs send` |
/// `btrfs receive` round trip must show a nonzero `received_uuid` and
/// `rtransid`, and the `readonly` flag set. This is the exact provenance
/// shape ADR 0008 requires and an ordinary snapshot (not received) does not
/// have — see `local_snapshot_is_not_received` below for the negative case
/// that proves this test can actually fail.
#[test]
#[ignore = "requires a real Btrfs received subvolume fixture; see file docs"]
fn received_snapshot_has_nonzero_provenance() {
    let path = fixture_path();
    let dir = std::fs::File::open(&path).expect("fixture path must be openable");
    let facts = query_subvolume_facts(dir.as_raw_fd())
        .expect("fixture path must be a real Btrfs subvolume");

    assert_ne!(
        facts.received_uuid, [0u8; 16],
        "a received subvolume must have a nonzero received_uuid; \
         got all-zero, which means this fixture was never actually received"
    );
    assert_ne!(
        facts.rtransid, 0,
        "a received subvolume must have a nonzero rtransid"
    );
    assert!(
        facts.readonly,
        "ADR 0008 requires BTRFS_SUBVOL_RDONLY set on the received object"
    );
    assert_ne!(
        facts.subvol_uuid, [0u8; 16],
        "subvol_uuid must be populated"
    );
    assert_ne!(
        facts.filesystem_uuid, [0u8; 16],
        "filesystem_uuid must be populated"
    );

    eprintln!("fs_uuid       = {:02x?}", facts.filesystem_uuid);
    eprintln!("subvol_uuid   = {:02x?}", facts.subvol_uuid);
    eprintln!("received_uuid = {:02x?}", facts.received_uuid);
    eprintln!(
        "generation={} ctransid={} otransid={} stransid={} rtransid={}",
        facts.generation, facts.ctransid, facts.otransid, facts.stransid, facts.rtransid
    );
}

/// Inverted fixture #1 of ADR 0008's two-way proof requirement: an ORDINARY
/// (non-received) read-only snapshot must NOT show received provenance, even
/// though it shares the readonly flag with a genuine received object. This is
/// exactly the case ADR 0008 names explicitly: "A Btrfs `ro` flag can be
/// applied to an ordinary subvolume after arbitrary prior changes" — the flag
/// alone proves nothing; only `received_uuid`/`rtransid` do.
///
/// Set BACKUPSAGE_BTRFS_LOCAL_SNAPSHOT to a plain `btrfs subvolume snapshot -r`
/// target (not sent/received) to run this.
#[test]
#[ignore = "requires a real Btrfs local (non-received) RO snapshot fixture"]
fn local_snapshot_is_not_received() {
    let path = std::env::var_os("BACKUPSAGE_BTRFS_LOCAL_SNAPSHOT")
        .map(std::path::PathBuf::from)
        .expect("set BACKUPSAGE_BTRFS_LOCAL_SNAPSHOT to a local RO (non-received) snapshot");
    let dir = std::fs::File::open(&path).expect("fixture path must be openable");
    let facts = query_subvolume_facts(dir.as_raw_fd())
        .expect("fixture path must be a real Btrfs subvolume");

    assert!(
        facts.readonly,
        "test setup error: this fixture must be RO to isolate the received-uuid check"
    );
    assert_eq!(
        facts.received_uuid, [0u8; 16],
        "an ordinary local snapshot must NOT carry received provenance — \
         if this assertion doesn't fire, the fixture wasn't actually a plain \
         local snapshot (it may itself have been received)"
    );
}

/// Refusal case: an ordinary ext4/non-Btrfs directory must be rejected with
/// `NotBtrfs`, never silently treated as unsupported-but-maybe-OK.
#[test]
fn non_btrfs_path_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let f = std::fs::File::open(dir.path()).unwrap();
    let result = query_subvolume_facts(f.as_raw_fd());
    assert_eq!(
        result,
        Err(BtrfsQueryError::NotBtrfs),
        "a non-Btrfs directory must be refused as NotBtrfs, not silently accepted \
         or confused with a generic ioctl failure"
    );
}
