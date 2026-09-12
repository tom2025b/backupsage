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

/// Outcome of a real write attempt made inside a forked child.
#[derive(Debug, PartialEq, Eq)]
enum WriteAttempt {
    Succeeded,
    Denied,
    OtherFailure(i32),
}

/// Fork, optionally enforce a Landlock ruleset, then genuinely attempt to
/// create a file. Runs in a child because `landlock_restrict_self` is
/// irreversible and inherited — enforcing it in the test process itself would
/// silently poison every later test in the same binary.
///
/// Everything that allocates (the CString) happens before the fork; the child
/// touches only raw libc calls and `_exit`, matching `process.rs`'s standing
/// invariant.
fn write_attempt(
    target: &std::path::Path,
    ruleset: Option<&super::landlock::Ruleset>,
) -> WriteAttempt {
    let c_target = std::ffi::CString::new(target.as_os_str().as_encoded_bytes()).unwrap();
    let ruleset_fd = ruleset.map(|r| r.as_raw_fd());

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        unsafe {
            if let Some(fd) = ruleset_fd {
                if super::landlock::restrict_self(fd).is_err() {
                    libc::_exit(90);
                }
            }
            let fd = libc::open(c_target.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o600);
            if fd >= 0 {
                libc::_exit(0);
            }
            let err = *libc::__errno_location();
            libc::_exit(if err == libc::EACCES || err == libc::EPERM {
                1
            } else {
                50 + (err % 40)
            });
        }
    }
    let mut status = 0;
    unsafe { libc::waitpid(pid, &mut status, 0) };
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        -1
    };
    match code {
        0 => WriteAttempt::Succeeded,
        1 => WriteAttempt::Denied,
        other => WriteAttempt::OtherFailure(other),
    }
}

/// Two-way proof for the Landlock layer, deliberately on a WRITABLE ext4
/// directory rather than the Btrfs read-only snapshot. ADR 0008 requires
/// exactly this isolation: "use a service-writable clone/unrelated tree so
/// Btrfs RO does not mask it." If this ran against the RO snapshot, Btrfs
/// alone would deny the write and the test would pass while proving nothing
/// whatsoever about Landlock.
///
/// The positive control is the load-bearing half: it proves the write path
/// genuinely works when unconfined, so the denial in the confined case is
/// attributable to Landlock and not to a broken fixture, a bad path, or a
/// permissions accident.
#[test]
#[ignore = "requires Landlock ABI 3+; run only in the privileged fixture VM"]
fn landlock_denies_writes_it_handles_and_the_control_proves_it() {
    use super::landlock::{Rule, Ruleset, READ_ONLY_DIR};

    let abi = super::landlock::abi_version().expect("Landlock must be available in the fixture");
    assert!(
        abi >= super::landlock::REQUIRED_ABI,
        "ADR 0008 sets ABI 3 as the floor (TRUNCATE mediation); fixture reports {abi}"
    );

    let dir = tempfile::tempdir().unwrap();
    let unconfined_target = dir.path().join("control-write");
    let confined_target = dir.path().join("confined-write");

    // Positive control, unconfined: the write must genuinely succeed.
    assert_eq!(
        write_attempt(&unconfined_target, None),
        WriteAttempt::Succeeded,
        "control write failed while UNCONFINED — the fixture itself is broken, so a \
         denial in the confined case below would prove nothing about Landlock"
    );

    // Same directory, same operation, now under a read-only Landlock rule.
    let ruleset = Ruleset::build(&[Rule {
        path: dir.path().to_path_buf(),
        access: READ_ONLY_DIR,
    }])
    .expect("ruleset construction must succeed on a supported kernel");

    assert_eq!(
        write_attempt(&confined_target, Some(&ruleset)),
        WriteAttempt::Denied,
        "Landlock granted only READ_ONLY_DIR yet the write succeeded — either a MAKE_REG \
         right leaked into the allow rule or it is missing from HANDLED_ACCESS_FS, in \
         which case Landlock permits it by default"
    );
}

/// Landlock must not deny what the policy legitimately grants. A ruleset that
/// denies everything would pass the test above while making Borg unable to
/// write its own private state, which ADR 0008 explicitly requires to remain
/// writable. This is the overbroad-rule inversion.
#[test]
#[ignore = "requires Landlock ABI 3+; run only in the privileged fixture VM"]
fn landlock_still_permits_the_private_state_hierarchy() {
    use super::landlock::{Rule, Ruleset, PRIVATE_STATE_DIR};

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("state-write");

    let ruleset = Ruleset::build(&[Rule {
        path: dir.path().to_path_buf(),
        access: PRIVATE_STATE_DIR,
    }])
    .expect("ruleset construction must succeed on a supported kernel");

    assert_eq!(
        write_attempt(&target, Some(&ruleset)),
        WriteAttempt::Succeeded,
        "the private-state hierarchy must stay writable under Landlock; Borg's \
         base/cache/security state genuinely needs create and write rights"
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
