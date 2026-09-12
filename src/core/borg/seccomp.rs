//! Seccomp-BPF filter construction, per ADR 0008's descendant-confinement
//! requirements. Hand-built BPF rather than libseccomp: the filter must be
//! installed by a forked child that may not allocate (see `process.rs`), and
//! libseccomp builds its program with internal malloc. Building the program
//! in the parent and installing it with one bare `prctl` in the child keeps
//! that invariant intact.
//!
//! What this layer is FOR, and what it is not: Landlock cannot mediate mount,
//! namespace creation, ptrace or descriptor passing. ADR 0008 therefore
//! requires seccomp to "close the user-namespace route by which an
//! unprivileged task can gain capabilities over a new mount namespace", and
//! to deny post-confinement FD receipt. It is not a general syscall allowlist
//! — Borg and Python need a large, unenumerated syscall surface, and
//! pretending otherwise would produce a filter that either breaks Borg or
//! gets loosened until it means nothing.
#![allow(dead_code)]

/// Denied syscalls return this errno rather than killing the process, so a
/// refusal surfaces as an ordinary failed operation Borg can report, not an
/// unexplained SIGSYS death that looks like a crash. `clone3` is the
/// documented exception below.
const DENY_ERRNO: u32 = libc::EPERM as u32;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_DATA: u32 = 0x0000_ffff;
const SECCOMP_SET_MODE_FILTER: libc::c_int = 1;

// linux/audit.h — the filter must pin the architecture it was generated for.
// A filter written for x86_64 that runs under a different personality would
// match the wrong syscall numbers entirely, which is why ADR 0008 requires
// policy generation to "account for the executing syscall architecture and
// ioctl encodings, reject unreviewed compatibility entry points".
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

// BPF instruction classes and modes (linux/bpf_common.h).
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JSET: u16 = 0x40;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

// Byte offsets into `struct seccomp_data`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_ARG0_LO: u32 = 16;

/// `CLONE_NEW*` bits. Ordinary `clone` is permitted only when none is set —
/// ADR 0008 allows a non-namespace fork/clone fallback while closing the
/// namespace route.
const CLONE_NEWNS: u64 = 0x0002_0000;
const CLONE_NEWCGROUP: u64 = 0x0200_0000;
const CLONE_NEWUTS: u64 = 0x0400_0000;
const CLONE_NEWIPC: u64 = 0x0800_0000;
const CLONE_NEWUSER: u64 = 0x1000_0000;
const CLONE_NEWPID: u64 = 0x2000_0000;
const CLONE_NEWNET: u64 = 0x4000_0000;
const CLONE_NEW_ANY: u64 = CLONE_NEWNS
    | CLONE_NEWCGROUP
    | CLONE_NEWUTS
    | CLONE_NEWIPC
    | CLONE_NEWUSER
    | CLONE_NEWPID
    | CLONE_NEWNET;

/// Syscalls denied outright. Grouped by the ADR 0008 sentence that requires
/// each group, so a later reader can tell which are load-bearing and why.
const DENIED: &[libc::c_long] = &[
    // "returns a hard error for unshare, setns, mount, umount2, pivot_root,
    // chroot, fsopen, fsmount, move_mount, open_tree, mount_setattr"
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_fsopen,
    libc::SYS_fsmount,
    libc::SYS_move_mount,
    libc::SYS_open_tree,
    libc::SYS_mount_setattr,
    // "It also denies ptrace, process_vm_writev, io_uring setup"
    libc::SYS_ptrace,
    libc::SYS_process_vm_writev,
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    // "no inherited IPC sockets; deny recvmsg, recvmmsg and pidfd_getfd" —
    // no_new_privs does NOT close SCM_RIGHTS, so a confined descendant could
    // otherwise be handed a writable or device descriptor from outside.
    libc::SYS_recvmsg,
    libc::SYS_recvmmsg,
    libc::SYS_pidfd_getfd,
];

#[repr(C)]
#[derive(Clone, Copy)]
struct sock_filter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct sock_fprog {
    len: u16,
    filter: *const sock_filter,
}

const fn stmt(code: u16, k: u32) -> sock_filter {
    sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter { code, jt, jf, k }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompError {
    /// The kernel rejected the filter, or seccomp filtering is unavailable.
    InstallFailed,
    /// The generated program exceeded what a `sock_fprog` can express.
    ProgramTooLong,
}

/// A generated, not-yet-installed filter program. Built in the parent; the
/// child installs it with one `prctl` and no allocation.
pub struct Filter {
    program: Vec<sock_filter>,
}

impl Filter {
    /// Generate the policy. The shape is: verify architecture, load the
    /// syscall number, deny each named syscall, special-case `clone`/`clone3`,
    /// then allow everything else.
    ///
    /// Architecture is checked FIRST and kills on mismatch rather than
    /// falling through to `ALLOW` — a filter that silently permits a foreign
    /// personality's syscall numbering is worse than no filter, because it
    /// looks installed.
    pub fn build() -> Result<Self, SeccompError> {
        // Refuse to run under an architecture this policy was not generated
        // for, before loading the syscall number. jt=1 skips the denial when
        // the architecture matches.
        let mut program: Vec<sock_filter> = vec![
            stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH),
            jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
            stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | DENY_ERRNO),
            stmt(BPF_LD | BPF_W | BPF_ABS, OFF_NR),
        ];

        for &nr in DENIED {
            program.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr as u32, 0, 1));
            program.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | DENY_ERRNO));
        }

        // clone3 cannot have its flags inspected by seccomp — they live in a
        // struct behind a pointer, and seccomp cannot dereference memory.
        // ADR 0008 therefore requires ENOSYS specifically, so glibc falls back
        // to ordinary clone, whose flags ARE a register argument we can check.
        program.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_clone3 as u32,
            0,
            1,
        ));
        program.push(stmt(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | (libc::ENOSYS as u32 & SECCOMP_RET_DATA),
        ));

        // Ordinary clone: permitted only when no CLONE_NEW* bit is set.
        program.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_clone as u32,
            0,
            3,
        ));
        program.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARG0_LO));
        program.push(jump(
            BPF_JMP | BPF_JSET | BPF_K,
            (CLONE_NEW_ANY & 0xffff_ffff) as u32,
            0,
            1,
        ));
        program.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | DENY_ERRNO));

        program.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

        if program.len() > u16::MAX as usize {
            return Err(SeccompError::ProgramTooLong);
        }
        Ok(Self { program })
    }

    pub fn instruction_count(&self) -> usize {
        self.program.len()
    }

    /// Install the filter on the calling thread and everything it later
    /// creates. Async-signal-safe: reads an already-built program through a
    /// stable pointer and makes one `prctl` call, with no allocation.
    ///
    /// `no_new_privs` must already be set; the kernel refuses otherwise for an
    /// unprivileged caller. Seccomp filters are inherited across permitted
    /// fork/clone and across exec, which is what makes a credential helper's
    /// descendants subject to the same policy.
    ///
    /// # Safety
    /// Intended for a forked child immediately before `execve`. Irreversible.
    pub unsafe fn install(&self) -> Result<(), SeccompError> {
        // Set independently of Landlock's own call: an unprivileged caller
        // cannot install a filter without it, and either layer may be
        // installed first. Setting it twice is harmless and idempotent.
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(SeccompError::InstallFailed);
        }
        let prog = sock_fprog {
            len: self.program.len() as u16,
            filter: self.program.as_ptr(),
        };
        if libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0,
            &prog as *const sock_fprog,
        ) != 0
        {
            return Err(SeccompError::InstallFailed);
        }
        Ok(())
    }
}
