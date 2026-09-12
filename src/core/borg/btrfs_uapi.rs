//! Raw Btrfs UAPI ioctl bindings, per the Linux v6.12 headers cited in ADR
//! 0008. Not exposed by any stable crate at the time of writing, so hand-bound
//! against `include/uapi/linux/btrfs.h` / `btrfs_tree.h`. Struct layouts are
//! `repr(C)` to match the kernel's on-wire layout exactly; correctness is
//! proven empirically in `tests.rs` against real `btrfs subvolume show`
//! output on a real filesystem, not merely asserted here.
#![allow(non_camel_case_types, dead_code)]

use std::os::fd::RawFd;

pub const BTRFS_UUID_SIZE: usize = 16;
pub const BTRFS_LABEL_SIZE: usize = 256;

/// The root-item readonly flag, as reported in `btrfs_ioctl_get_subvol_info_args::flags`
/// (linux/btrfs_tree.h `BTRFS_ROOT_SUBVOL_RDONLY`, bit 0).
///
/// This is NOT the same constant as `BTRFS_SUBVOL_RDONLY` (linux/btrfs.h, bit
/// 1), which belongs to the separate, older `BTRFS_IOC_SUBVOL_GETFLAGS`/
/// `SETFLAGS` ioctl pair and uses a different bit position for the same
/// concept. Verified empirically against a real received subvolume: its
/// `GET_SUBVOL_INFO` flags value was `0x1`, matching this constant, not `0x2`.
/// Using the wrong one here doesn't create a false-accept (it fails closed —
/// a genuinely read-only object reads as not-readonly) but it does mean the
/// backend can never validate anything, silently, which is its own kind of
/// defect worth naming precisely rather than "the readonly flag".
pub const BTRFS_ROOT_SUBVOL_RDONLY: u64 = 1 << 0;

/// `struct btrfs_ioctl_fs_info_args` (linux/btrfs.h). `fsid` is the filesystem
/// UUID; the remaining reserved fields are zeroed and not consumed here.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct btrfs_ioctl_fs_info_args {
    pub max_id: u64,
    pub num_devices: u64,
    pub fsid: [u8; BTRFS_UUID_SIZE],
    pub nodesize: u32,
    pub sectorsize: u32,
    pub clone_alignment: u32,
    pub reserved32: u32,
    pub reserved: [u64; 122],
}

/// `struct btrfs_ioctl_get_subvol_info_args` (linux/btrfs.h). Field order and
/// widths matter — this is read via raw ioctl, no serde, no derived parsing.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct btrfs_ioctl_get_subvol_info_args {
    pub treeid: u64,
    pub name: [u8; BTRFS_LABEL_SIZE],
    pub parent_id: u64,
    pub dirid: u64,
    pub generation: u64,
    pub flags: u64,
    pub uuid: [u8; BTRFS_UUID_SIZE],
    pub parent_uuid: [u8; BTRFS_UUID_SIZE],
    pub received_uuid: [u8; BTRFS_UUID_SIZE],
    pub ctransid: u64,
    pub otransid: u64,
    pub stransid: u64,
    pub rtransid: u64,
    pub ctime: btrfs_ioctl_timespec,
    pub otime: btrfs_ioctl_timespec,
    pub stime: btrfs_ioctl_timespec,
    pub rtime: btrfs_ioctl_timespec,
    pub reserved: [u64; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct btrfs_ioctl_timespec {
    pub sec: u64,
    pub nsec: u32,
}

/// `struct btrfs_ioctl_vol_args` — used by `BTRFS_IOC_SUBVOL_GETFLAGS`'s sibling
/// calls that take a plain 64-bit flag word rather than the full args struct.
/// Kept minimal; not currently used but documented for the setflags probe in
/// the mutating-ioctl denial suite (a later slice of this work).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct btrfs_ioctl_vol_args {
    pub fd: i64,
    pub name: [u8; BTRFS_LABEL_SIZE],
}

// ioctl request-number construction. Btrfs uses the standard Linux ioctl
// encoding (_IOR / _IOWR) with magic number 0x94 ('\x94' = BTRFS_IOCTL_MAGIC).
// This mirrors the kernel's own <linux/ioctl.h> macros; there is no safe way
// to import them, only to reproduce the arithmetic, which is fixed ABI and
// will not silently drift.
const BTRFS_IOCTL_MAGIC: u64 = 0x94;
const IOC_NRBITS: u64 = 8;
const IOC_TYPEBITS: u64 = 8;
const IOC_SIZEBITS: u64 = 14;
const IOC_NRSHIFT: u64 = 0;
const IOC_TYPESHIFT: u64 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u64 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u64 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ: u64 = 2;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << IOC_DIRSHIFT)
        | (BTRFS_IOCTL_MAGIC << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)
        | (size << IOC_SIZESHIFT)
}

const fn ior(nr: u64, size: usize) -> u64 {
    ioc(IOC_READ, nr, size as u64)
}

pub const BTRFS_IOC_FS_INFO: u64 = ior(31, std::mem::size_of::<btrfs_ioctl_fs_info_args>());
pub const BTRFS_IOC_GET_SUBVOL_INFO: u64 =
    ior(60, std::mem::size_of::<btrfs_ioctl_get_subvol_info_args>());

/// Byte-identity facts of one Btrfs subvolume, read directly off an already-
/// open, already-validated FD via `fstat`+ioctl — never by reopening a path.
/// This is the same-FD re-derivation ADR 0008 requires before every Borg spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubvolumeFacts {
    pub filesystem_uuid: [u8; BTRFS_UUID_SIZE],
    pub subvol_tree_id: u64,
    pub subvol_uuid: [u8; BTRFS_UUID_SIZE],
    pub received_uuid: [u8; BTRFS_UUID_SIZE],
    pub generation: u64,
    pub ctransid: u64,
    pub otransid: u64,
    pub stransid: u64,
    pub rtransid: u64,
    pub readonly: bool,
    pub raw_flags: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtrfsQueryError {
    NotBtrfs,
    IoctlFailed,
}

/// Query both ioctls on the given FD and return the identity facts, or a
/// reason the FD is not a queryable Btrfs subvolume root. Takes a raw FD
/// deliberately — callers own the FD's lifetime; this never closes it.
pub fn query_subvolume_facts(fd: RawFd) -> Result<SubvolumeFacts, BtrfsQueryError> {
    // Per ioctl(2): "Usually, on success zero is returned. A few ioctl()
    // requests use the return value as an output parameter and return a
    // nonnegative value on success" — verified empirically that
    // BTRFS_IOC_GET_SUBVOL_INFO is one of these (returns 1 on a real
    // fixture, not 0). Only a negative return is failure; errno is only
    // meaningful in that case.
    let mut fs_info: btrfs_ioctl_fs_info_args = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, BTRFS_IOC_FS_INFO, &mut fs_info as *mut _) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        return Err(match err.raw_os_error() {
            Some(libc::ENOTTY) => BtrfsQueryError::NotBtrfs,
            _ => BtrfsQueryError::IoctlFailed,
        });
    }

    let mut subvol_info: btrfs_ioctl_get_subvol_info_args = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, BTRFS_IOC_GET_SUBVOL_INFO, &mut subvol_info as *mut _) };
    if rc < 0 {
        return Err(BtrfsQueryError::IoctlFailed);
    }

    Ok(SubvolumeFacts {
        filesystem_uuid: fs_info.fsid,
        subvol_tree_id: subvol_info.treeid,
        subvol_uuid: subvol_info.uuid,
        received_uuid: subvol_info.received_uuid,
        generation: subvol_info.generation,
        ctransid: subvol_info.ctransid,
        otransid: subvol_info.otransid,
        stransid: subvol_info.stransid,
        rtransid: subvol_info.rtransid,
        readonly: subvol_info.flags & BTRFS_ROOT_SUBVOL_RDONLY != 0,
        raw_flags: subvol_info.flags,
    })
}
