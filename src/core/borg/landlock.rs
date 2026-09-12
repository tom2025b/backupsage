//! Landlock ruleset construction, per ADR 0008's descendant-confinement
//! requirements. Hand-bound against `linux/landlock.h` (verified on the
//! acceptance fixture's own headers, not from memory).
//!
//! Division of labour, forced by `process.rs`'s standing invariant that fork
//! children use only raw syscalls and never allocate: the ruleset is BUILT in
//! the parent, before fork, where allocation and path opening are safe. The
//! child does exactly one thing with it — `landlock_restrict_self` on the
//! inherited ruleset FD, a bare syscall with no allocation. The ruleset FD
//! survives fork like any other descriptor.
//!
//! Landlock's own documented gaps matter here and are handled elsewhere, not
//! papered over: it cannot mediate `chmod`, `chown`, `setxattr`, `utime`,
//! `ioctl` or `fcntl`. ADR 0008 is explicit that Btrfs read-only storage —
//! not Landlock — is the primary denial for repository metadata mutation,
//! and that Landlock is the second, descendant-wide layer. Neither substitutes
//! for the other.
#![allow(dead_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

// linux/landlock.h, verified against the fixture kernel's headers.
pub const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;

pub const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13;
pub const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14;
/// Available since ABI 5 (linux/landlock.h: "available since the fifth
/// version of the Landlock ABI"). Absent from the original 15-bit mask this
/// module shipped with — found by a review run on a different, newer host
/// (ABI 8) than this module's own fixture VM (ABI 4), which structurally
/// could not have revealed the gap: the bit does not exist at ABI 4.
pub const LANDLOCK_ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

/// ADR 0008 floor: "ABI 3 is the floor because earlier ABIs cannot mediate
/// truncation." TRUNCATE (bit 14) arrived in ABI 3.
pub const REQUIRED_ABI: i32 = 3;

/// The 15 rights that exist as of ABI 3 (this module's floor). Landlock is
/// deny-by-default only for rights named in `handled_access_fs` — anything
/// omitted is silently ALLOWED. ADR 0008 enumerates exactly these: EXECUTE,
/// READ_FILE, READ_DIR, WRITE_FILE, TRUNCATE, REMOVE_FILE, REMOVE_DIR, REFER
/// "and every MAKE_* right".
///
/// This is the FLOOR, not the whole mask — see `handled_access_fs_for_abi`.
/// A fixed constant here was the bug: IOCTL_DEV (ABI 5) is a filesystem right
/// too, and a ruleset that never adds it to `handled_access_fs` allows every
/// device ioctl on any path the child can open, on every kernel newer than
/// ABI 4, silently.
const HANDLED_ACCESS_FS_ABI3: u64 = LANDLOCK_ACCESS_FS_EXECUTE
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_READ_FILE
    | LANDLOCK_ACCESS_FS_READ_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_MAKE_CHAR
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_SOCK
    | LANDLOCK_ACCESS_FS_MAKE_FIFO
    | LANDLOCK_ACCESS_FS_MAKE_BLOCK
    | LANDLOCK_ACCESS_FS_MAKE_SYM
    | LANDLOCK_ACCESS_FS_REFER
    | LANDLOCK_ACCESS_FS_TRUNCATE;

/// Every filesystem right this module has actually reviewed against
/// linux/landlock.h, as of ABI 6 (the highest ABI that added a new
/// filesystem-relevant right at time of writing — ABI 7 and 8 add no new
/// `LANDLOCK_ACCESS_FS_*` bit per the same header). `Ruleset::build` ANDs
/// this down to what the running kernel's ABI actually supports, so an
/// older kernel gets exactly the ABI-3 floor and a newer one gets every
/// right this module knows to name — never a fixed guess that silently
/// stops matching reality as the kernel moves forward.
///
/// `LANDLOCK_SCOPE_*` (ABI 6: abstract UNIX sockets, signals) is a
/// deliberate, disclosed gap, not an oversight: it lives in a separate
/// `scoped` field requiring the larger ABI-6 `landlock_ruleset_attr`
/// layout, and ADR 0008 assigns FD-receipt denial (`recvmsg`, `pidfd_getfd`)
/// to the seccomp layer instead. Wiring `scoped` properly is real remaining
/// work, not silently covered by this constant.
const HANDLED_ACCESS_FS_REVIEWED: u64 = HANDLED_ACCESS_FS_ABI3 | LANDLOCK_ACCESS_FS_IOCTL_DEV;

/// The `handled_access_fs` bitmask to actually request, for a kernel that
/// reported the given ABI version via `abi_version()`. Never returns a right
/// the kernel doesn't support — Landlock refuses `create_ruleset` outright if
/// `handled_access_fs` names an unsupported bit, so building the mask
/// unconditionally at the newest reviewed level would break on an older
/// kernel instead of degrading to its floor.
pub fn handled_access_fs_for_abi(abi: i32) -> u64 {
    if abi >= 5 {
        HANDLED_ACCESS_FS_REVIEWED
    } else {
        HANDLED_ACCESS_FS_ABI3
    }
}

/// Read-only access to a directory hierarchy: the repository pin, the Borg /
/// runtime trees (which additionally need EXECUTE), keys and credentials.
pub const READ_ONLY_DIR: u64 = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR;

/// Read + execute, for the Borg/Python/helper executable trees.
pub const READ_EXECUTE_DIR: u64 = READ_ONLY_DIR | LANDLOCK_ACCESS_FS_EXECUTE;

/// The single writable hierarchy ADR 0008 permits: BackupSage's own private
/// Borg base/cache/security state. Borg genuinely needs to create, rewrite,
/// truncate, remove and rename inside it.
pub const PRIVATE_STATE_DIR: u64 = READ_ONLY_DIR
    | LANDLOCK_ACCESS_FS_WRITE_FILE
    | LANDLOCK_ACCESS_FS_TRUNCATE
    | LANDLOCK_ACCESS_FS_REMOVE_FILE
    | LANDLOCK_ACCESS_FS_REMOVE_DIR
    | LANDLOCK_ACCESS_FS_MAKE_REG
    | LANDLOCK_ACCESS_FS_MAKE_DIR
    | LANDLOCK_ACCESS_FS_REFER;

#[repr(C)]
struct landlock_ruleset_attr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// NOTE the packing. The kernel declares this `__attribute__((packed))`
/// specifically "to avoid trailing reserved members"; without `packed` Rust
/// would lay it out as 16 bytes (u64 + i32 + 4 padding) and the kernel
/// rejects the size. This is a 12-byte struct.
#[repr(C, packed)]
struct landlock_path_beneath_attr {
    allowed_access: u64,
    parent_fd: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LandlockError {
    /// Landlock absent, disabled at boot, or below the ABI floor.
    Unsupported,
    /// A path named in the policy could not be opened for rule construction.
    PathUnavailable,
    /// The kernel rejected ruleset creation or a rule addition.
    RulesetFailed,
    /// `landlock_restrict_self` failed in the child.
    RestrictFailed,
}

/// Report the kernel's supported Landlock ABI version, or `Unsupported` when
/// Landlock is unavailable. This is a real syscall probe, not a check for
/// `/sys/kernel/security/landlock` — that directory's absence does NOT mean
/// Landlock is off (verified on the fixture: ABI 4 live while that path did
/// not exist).
pub fn abi_version() -> Result<i32, LandlockError> {
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<landlock_ruleset_attr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if rc < 0 {
        return Err(LandlockError::Unsupported);
    }
    Ok(rc as i32)
}

/// One allow-rule: a directory (or file) hierarchy and the rights granted
/// beneath it. The FD is opened by the builder with `O_PATH`, which is what
/// Landlock prefers and which grants no read authority of its own.
pub struct Rule {
    pub path: std::path::PathBuf,
    pub access: u64,
}

/// A constructed, not-yet-enforced ruleset. Built entirely in the parent.
/// Holds the ruleset FD; enforcement is a separate, later, child-side step.
pub struct Ruleset {
    fd: OwnedFd,
}

impl Ruleset {
    /// Build the ruleset from the closed allow-list. Fails closed on an
    /// unavailable path rather than silently granting less (or more) than the
    /// policy names — a partial policy is not a policy.
    pub fn build(rules: &[Rule]) -> Result<Self, LandlockError> {
        let abi = abi_version()?;
        if abi < REQUIRED_ABI {
            return Err(LandlockError::Unsupported);
        }

        let attr = landlock_ruleset_attr {
            handled_access_fs: handled_access_fs_for_abi(abi),
            handled_access_net: 0,
        };
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                &attr as *const landlock_ruleset_attr,
                std::mem::size_of::<landlock_ruleset_attr>(),
                0u32,
            )
        };
        if rc < 0 {
            return Err(LandlockError::RulesetFailed);
        }
        let ruleset = unsafe { OwnedFd::from_raw_fd(rc as RawFd) };

        for rule in rules {
            let c_path = std::ffi::CString::new(rule.path.as_os_str().as_encoded_bytes())
                .map_err(|_| LandlockError::PathUnavailable)?;
            // O_PATH: identifies the hierarchy without granting read access,
            // and works for directories and regular files alike.
            let parent = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if parent < 0 {
                return Err(LandlockError::PathUnavailable);
            }
            let parent = unsafe { OwnedFd::from_raw_fd(parent) };

            let beneath = landlock_path_beneath_attr {
                allowed_access: rule.access,
                parent_fd: parent.as_raw_fd(),
            };
            let rc = unsafe {
                libc::syscall(
                    SYS_LANDLOCK_ADD_RULE,
                    ruleset.as_raw_fd(),
                    LANDLOCK_RULE_PATH_BENEATH,
                    &beneath as *const landlock_path_beneath_attr,
                    0u32,
                )
            };
            if rc < 0 {
                return Err(LandlockError::RulesetFailed);
            }
        }

        Ok(Self { fd: ruleset })
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Enforce a previously built ruleset on the calling thread and every process
/// it later creates. Async-signal-safe: one bare syscall, no allocation, safe
/// to call in a forked child before `execve`.
///
/// `no_new_privs` must already be set — the kernel refuses otherwise, and
/// ADR 0008 requires it independently so a set-user-ID exec cannot regain
/// privilege. Enforcement is irreversible and inherited across fork and exec.
///
/// # Safety
/// Must be called from a child that has already closed every descriptor it
/// should not retain. Landlock does not retroactively restrict descriptors
/// that were already open, which is precisely why descriptor closure is a
/// precondition and not cleanup.
pub unsafe fn restrict_self(ruleset_fd: RawFd) -> Result<(), LandlockError> {
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(LandlockError::RestrictFailed);
    }
    if libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32) != 0 {
        return Err(LandlockError::RestrictFailed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact regression a cross-host review caught: a fixed 15-bit
    /// (ABI-3) mask silently omits IOCTL_DEV on any kernel that actually
    /// supports it. This runs in ordinary CI — no privilege, no real
    /// Landlock syscall — because it tests the pure ABI-to-mask function,
    /// not enforcement.
    #[test]
    fn ioctl_dev_is_handled_from_abi_5_onward() {
        for abi in 5..=10 {
            assert_ne!(
                handled_access_fs_for_abi(abi) & LANDLOCK_ACCESS_FS_IOCTL_DEV,
                0,
                "ABI {abi} supports IOCTL_DEV; the mask must handle it or \
                 device ioctls are silently allowed"
            );
        }
    }

    /// The inverse: never request a bit the running kernel doesn't support.
    /// Landlock refuses create_ruleset outright if handled_access_fs names
    /// an unsupported right — requesting IOCTL_DEV on ABI 3/4 would not
    /// under-protect, it would make the entire ruleset fail to construct.
    #[test]
    fn ioctl_dev_is_not_requested_below_abi_5() {
        for abi in 3..5 {
            assert_eq!(
                handled_access_fs_for_abi(abi) & LANDLOCK_ACCESS_FS_IOCTL_DEV,
                0,
                "ABI {abi} predates IOCTL_DEV; requesting it would make \
                 create_ruleset fail on a real kernel at that ABI"
            );
        }
    }

    /// The ABI-3 floor itself must never regress silently. Every right ADR
    /// 0008 names must survive whatever this function does at any ABI.
    #[test]
    fn every_adr_0008_right_is_handled_at_every_supported_abi() {
        let required = LANDLOCK_ACCESS_FS_EXECUTE
            | LANDLOCK_ACCESS_FS_WRITE_FILE
            | LANDLOCK_ACCESS_FS_READ_FILE
            | LANDLOCK_ACCESS_FS_READ_DIR
            | LANDLOCK_ACCESS_FS_TRUNCATE
            | LANDLOCK_ACCESS_FS_REMOVE_FILE
            | LANDLOCK_ACCESS_FS_REMOVE_DIR
            | LANDLOCK_ACCESS_FS_REFER
            | LANDLOCK_ACCESS_FS_MAKE_CHAR
            | LANDLOCK_ACCESS_FS_MAKE_DIR
            | LANDLOCK_ACCESS_FS_MAKE_REG
            | LANDLOCK_ACCESS_FS_MAKE_SOCK
            | LANDLOCK_ACCESS_FS_MAKE_FIFO
            | LANDLOCK_ACCESS_FS_MAKE_BLOCK
            | LANDLOCK_ACCESS_FS_MAKE_SYM;
        for abi in 3..=10 {
            assert_eq!(
                handled_access_fs_for_abi(abi) & required,
                required,
                "ABI {abi} must still handle every ADR 0008 right"
            );
        }
    }
}
