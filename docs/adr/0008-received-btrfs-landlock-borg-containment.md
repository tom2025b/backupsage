# ADR 0008 — Received Btrfs snapshots with Landlock for Borg containment

Date: 2026-09-11 · Status: accepted (Tom approved 2026-09-11) · Milestone: unassigned · Issue: #82

## Context

ADR 0007 requires one boundary to establish three facts before Borg is run:

1. the source has trustworthy immutable point-in-time provenance, rather than
   merely being read-only at the instant it is checked;
2. the object validated is the object Borg later opens, despite path, symlink,
   mount or alias changes; and
3. create, write, truncate, unlink and rename are denied to Borg and every
   credential-helper descendant, while the approved private Borg state remains
   writable.

None of these facts implies the others. A read-only bind mount can still view a
changing live repository. A Btrfs `ro` flag can be applied to an ordinary
subvolume after arbitrary prior changes. A path can name a different object
after validation. A command allowlist does not constrain a helper descendant.
ADR 0007 therefore deliberately left `VerifiedImmutableSnapshot` without a
backend.

The first supported profile must also be usable without creating a mount
namespace at run time. Bubblewrap 0.11.1 is installed in the current sandbox,
but the prior namespace smoke test failed with `No permissions to create a new
namespace`. Bubblewrap's own documentation says that it always creates a mount
namespace and uses user namespaces for unprivileged operation; it is therefore
not a usable dependency in this environment
([Bubblewrap README](https://github.com/containers/bubblewrap/blob/main/README.md)).

## Threat model and trust root

The boundary treats Borg 1.4.4 and the configured credential helper as capable
of accidental or hostile filesystem operations. It also treats the operator
locator, symlinks, bind aliases, writable repository clones, inherited file
descriptors and namespace creation as attack surfaces. Ordinary backup jobs are
not trusted to keep a live repository stable.

The trust root for this profile is deliberately explicit:

- the Linux kernel's Btrfs, VFS, Landlock and seccomp implementations;
- root in the initial user namespace;
- a root-operated snapshot receiver/provisioner and its root-owned provenance
  registry; and
- the fixed `/usr/bin/borg` Borg 1.4.4 executable accepted by ADR 0007.

The BackupSage service UID, Borg, helpers, repository contents and any process
without initial-namespace root authority are outside that trust root. A
malicious root, compromised kernel, malicious storage firmware, offline block
device modification and physical attacks are out of scope. This is important:
no unprivileged process can prove immutability against a root actor that can
change the subvolume flag, mount topology or kernel.

## Decision

### Supported deployment profile: `linux-btrfs-received-ro-landlock-v1`

Propose one initial profile, and no generic “Linux read-only” profile:

- Ubuntu/Linux with Btrfs, Landlock ABI 3 or newer, seccomp-filter support and
  procfs mounted at `/proc`; the root provisioner uses btrfs-progs 5.14.2 or
  newer and registers only receives performed under this profile;
- BackupSage runs as a non-root service UID with empty inheritable, permitted,
  effective and ambient capability sets;
- deployment permissions deny that UID (including its supplementary groups),
  Borg and every helper access to every Btrfs backing block device: opening
  for read or write must fail, no backing-device FD may be inherited, and
  device nodes are outside the Landlock allowlist;
- the source is the root of a Btrfs subvolume produced by `btrfs receive`, with
  nonzero `received_uuid` and `rtransid`, and with `BTRFS_SUBVOL_RDONLY` set;
- the received subvolume root and its trusted locator root are owned by initial-
  namespace root, the locator root and provenance registry are not writable by
  the service UID, and the mount is not idmapped;
- the snapshot contains no nested mount or nested subvolume that supplies Borg
  repository content; and
- a root-owned provenance record was atomically installed by the receiver at
  receive time and has never been writable by the BackupSage UID.

Backing-device denial is a deployment/enablement precondition, including for
the service before child confinement. Filesystem RO is not a raw-device access
policy. The privileged fixture must prove failed opens of its disposable loop/
block device under the service and descendant identities, not infer denial
from subvolume flags. This does not extend the threat model to malicious
initial-namespace root.

The root provisioner must keep the receive destination inaccessible to the
service UID throughout the writable receive phase, and until the completed
subvolume is RO and its provenance record is installed. This includes access
through aliases or previously obtained descriptors. The Btrfs receive manual
warns that concurrent access can corrupt the received copy before it becomes
RO; receive also requires a trusted send stream
([receive precautions](https://btrfs.readthedocs.io/en/latest/btrfs-receive.html#bugs)).
Acceptance must synchronize service-UID access attempts inside that interval
and prove denial. This refines the existing root provisioner's obligation;
it does not add a locator-publication protocol.

Btrfs documents that send consumes read-only snapshots, receive reconstructs
the sent filesystem and turns the result read-only, and a received snapshot has
a `received_uuid`. Its documented btrfs-progs workflow for changing a received
subvolume to read-write requires clearing that identity, with a safety caveat
for received subvolumes predating the btrfs-progs 5.14.2 safety checks
([send/receive overview](https://btrfs.readthedocs.io/en/stable/Send-receive.html),
[subvolume flags](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html),
[send requirements](https://btrfs.readthedocs.io/en/latest/btrfs-send.html)).
That caveat is why current flags and `received_uuid` alone are not provenance.
The root-owned receive-time record is mandatory. This userspace safety check
must not be mistaken for a kernel prohibition on direct flag-changing ioctls;
the reviewed `btrfs_ioctl_subvol_setflags` handler checks owner/capability and
does not clear `received_uuid` itself
([Btrfs flag handler](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/ioctl.c)).

The record binds a profile version and operator-selected name to the Btrfs
filesystem ID, subvolume tree ID, subvolume UUID, received UUID, generation,
change/creation/send/receive transaction IDs (`ctransid`, `otransid`,
`stransid`, `rtransid`) and corresponding times returned by the Btrfs ioctls.
`BTRFS_IOC_FS_INFO` supplies FSID in `btrfs_ioctl_fs_info_args`;
`BTRFS_IOC_GET_SUBVOL_INFO` supplies the subvolume fields in
`btrfs_ioctl_get_subvol_info_args`. These are separate same-FD requests, not
one atomic combined identity query
([Linux v6.12 Btrfs UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/btrfs.h)).
It is local operator attestation, protected by root ownership; it is not a
content hash and must never be represented as one.

BackupSage does not receive, snapshot, mount, remount or write this record. If
the operator cannot supply this profile and record, the backend is unsupported.

### Open and validate one object, then keep it open

Open the provenance record relative to a trusted, already-open registry
directory FD, never relative to the working directory or repository. Validate
the registry directory's root ownership, identity and protection from
non-root replacement/write access. Use `openat2` with
`O_RDONLY|O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK` and
`RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS|RESOLVE_NO_XDEV`.
`O_NONBLOCK` avoids waiting on an unexpected FIFO; it is not a type check.
Require `fstat`/FD-based `statx` to establish a regular, initial-namespace-root-
owned record, no special mode bits or group/other write bits, no ACL granting
non-root write access, and no unsafe hardlink alias. Record device/inode/mount
identity and check it on the opened FD. If an earlier stat is used, its identity
must match this FD. Bound, parse and validate the record's bytes from that same
FD; never stat one pathname and reopen another for parsing. Unknown record or
profile versions, unsafe modes, mismatched snapshot facts and ambiguous reads
refuse. The root provisioner replaces records atomically, never edits a record
in place while it is being validated. A replacement after open/stat must either
be detected and refused or leave validation pinned to the originally opened,
validated record; it must never switch the bytes being authorized. These are
requirements on the existing registry, not a new trust root
([openat2 UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/openat2.h),
[Linux open-file semantics](https://man7.org/linux/man-pages/man2/open.2.html)).

Open the operator-selected name relative to the already-open trusted locator
root with `openat2`, using `O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` and
`RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS`. `openat2` defines
these resolution constraints specifically to prevent escape and symlink/magic-
link traversal
([openat2(2)](https://man7.org/linux/man-pages/man2/openat2.2.html)).

All validation is performed on that descriptor, not by reopening its display
path:

- `fstat`/`statx` establish directory type, owner, device and mount identity;
- Btrfs filesystem/subvolume ioctls establish filesystem ID, tree ID, UUIDs,
  transactions and `BTRFS_SUBVOL_RDONLY`;
- those values must exactly match the root-owned provenance record;
- the descriptor must name the subvolume root, not a descendant;
- the mount must be non-idmapped and the current mount topology must contain no
  nested mount supplying repository paths; and
- the service identity/capability and kernel/LSM prerequisites must pass.

Any mismatch, unsupported ioctl, parsing ambiguity or race-style error refuses
before `/usr/bin/borg` is executed. There is no fallback to a pathname.

After validation, duplicate the same descriptor to one reserved child FD, clear
`FD_CLOEXEC` only on that duplicate, and give Borg the repository locator
`/proc/self/fd/N`. Linux procfs describes that entry as a link to the actual
open file, and the kernel pathname documentation distinguishes this magic link
as a reference to the target object rather than merely its former name
([proc_pid_fd(5)](https://man7.org/linux/man-pages/man5/proc_pid_fd.5.html),
[kernel pathname lookup](https://www.kernel.org/doc/html/v5.0/filesystems/path-lookup.html)).
The original operator path is absent from child argv and environment.

The Borg 1.4.4 implementation normalizes repository paths with
`os.path.abspath`, not `realpath`
([Repository constructor](https://github.com/borgbackup/borg/blob/1.4.4/src/borg/repository.py#L178)).
A local throwaway-repository experiment for
this decision verified that both repository JSON and archive JSONL work when
the locator is `/proc/self/fd/9`; Borg reported that exact locator. This is
version-specific evidence, not a promise for Borg 2. Descriptor survival and
path-swap behavior remain mandatory acceptance tests.

The capability's owning FD and the reserved Borg FD number/object must stay
valid for the full Borg/helper lifetime, through cancellation and until the
last descendant is reaped. Every process using `/proc/self/fd/N` must retain N
as that same object; a live parent FD alone does not establish a child's N.
Helper launch, descriptor closure and deterministic FD reuse must be tested,
not assumed safe because the integer is unchanged. Loss or substitution must
fail closed, with no path fallback or accepted output from a different object.

### Descendant confinement and alias handling

Immediately before `execve`, the single-threaded child setup must:

1. close every inherited descriptor except null stdin, bounded stdout/stderr
   pipes and the pinned read-only directory descriptor;
2. verify empty capability sets, set `no_new_privs`, and install the complete
   Landlock policy;
3. install a seccomp filter; and
4. enter the process group/session lifecycle required by ADR 0007.

Landlock handles at least `EXECUTE`, `READ_FILE`, `READ_DIR`, `WRITE_FILE`,
`TRUNCATE`, `REMOVE_FILE`, `REMOVE_DIR`, `REFER`, and every `MAKE_*` right. ABI
3 is the floor because earlier ABIs cannot mediate truncation. The kernel
documentation states that an enforced ruleset applies to subsequently created
children, cannot be removed, and identifies the rights needed for write,
truncate, removal, creation and cross-directory rename
([Landlock userspace API](https://www.kernel.org/doc/html/latest/userspace-api/landlock.html)).

The allow rules are closed:

- the pinned snapshot object: read file/read directory only;
- the root-owned Borg/Python/runtime trees and the specifically supported
  helper executable tree: read/execute only;
- the validated credential and key inputs: read only;
- the validated private Borg state tree: read plus the handled write,
  truncate, create, remove and rename rights Borg state requires; and
- no BackupSage index, source alias, home directory, ordinary Borg state,
  live repository, device node or unrelated writable tree.

Landlock's existing-descriptor exception is why descriptor closure is a
precondition, not cleanup. The pinned repository descriptor is opened read-only
and is the only inherited filesystem descriptor. The filesystem-level Btrfs
read-only flag remains the primary source-mutation denial; Landlock adds a
second, descendant-wide denial and prevents a helper from using a separately
named writable repository path.

Btrfs RO, not Landlock, covers unmediated repository metadata mutation such as
chmod/chown, timestamps and xattrs. Landlock documents these gaps; the Btrfs
`btrfs_setattr` path explicitly rejects a read-only subvolume. Root ownership
and empty capabilities remain load-bearing for subvolume authority. Acceptance
must exercise these metadata attempts as well as ordinary writes
([Landlock limitations](https://www.kernel.org/doc/html/v6.12/userspace-api/landlock.html#filesystem-flags),
[Btrfs inode operations](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/inode.c),
[Btrfs xattr handlers](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/xattr.c)).

The child must also be unable to construct a new mount view over repository
paths. The seccomp filter therefore returns a hard error for `unshare`, `setns`,
`mount`, `umount2`, `pivot_root`, `chroot`, `fsopen`, `fsmount`, `move_mount`,
`open_tree`, `mount_setattr` and `clone3`; ordinary `clone` is permitted only
when none of the `CLONE_NEW*` bits is set. It also denies `ptrace`,
`process_vm_writev`, io_uring setup and the mutating ioctl requests specified
in the acceptance suite below. Ioctl filtering is defense in depth alongside
Btrfs RO/root ownership, not a blanket ioctl allowlist that assumes Borg/Python
need no other ioctls. The filter returns `ENOSYS` for `clone3` so the
runtime can use a non-namespace fork/clone fallback.
Seccomp filters are inherited across allowed fork/clone and exec operations
([kernel seccomp documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html)).
This closes the user-namespace route by which an unprivileged task can gain
capabilities over a new mount namespace
([user_namespaces(7)](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)).

`no_new_privs` is inherited and cannot be unset, so set-user-ID and file-
capability execs cannot grant new privilege
([kernel no-new-privileges documentation](https://www.kernel.org/doc/html/latest/userspace-api/no_new_privs.html)).
It does **not** close `SCM_RIGHTS` or other post-confinement FD receipt.
Descriptor hygiene must therefore also exclude external FD acquisition:
no inherited IPC sockets; deny `recvmsg`, `recvmmsg` and `pidfd_getfd`, including
any permitted ABI's alternate syscall entry points. Helpers needing these
paths are unsupported. The existing io_uring denial must prevent a substitute
receipt path. Prove attempted receipt of an outside writable/device FD fails
under the actual descendant policy; `no_new_privs` alone is not that proof
([kernel no-new-privileges caveat](https://docs.kernel.org/userspace-api/no_new_privs.html),
[pidfd_getfd](https://man7.org/linux/man-pages/man2/pidfd_getfd.2.html)).

Aliases are handled by object and authority, not by enumerating strings:

- every symlink or bind alias of the selected subvolume reaches the same
  Btrfs read-only object;
- the FD locator cannot be redirected by replacing the operator pathname;
- no service-UID-owned subvolume root or idmapped mount is accepted, preventing
  the child from clearing the read-only flag as the inode owner;
- Landlock grants repository writes nowhere, so a helper-discovered locator or
  writable clone is denied; and
- pre-existing test aliases are required acceptance cases. A runtime claim
  that every host pathname was enumerated is neither required nor permitted.

Root can still remount, alter Btrfs metadata or replace mount topology. Root is
the declared trust root; pretending otherwise would be a false guarantee.

### Private-state exception and child boundary

The only writable filesystem hierarchy in the child policy is the validated,
BackupSage-owned private Borg base/cache/security state approved in ADR 0007.
It is outside the snapshot, credential/key inputs, indexes and ordinary Borg
state. Its directories are mode 0700 and files mode 0600. Existing entries are
opened and checked without following symlinks; mountpoints, hardlink aliases to
protected files, and unsafe containment relationships refuse.

Keys and the credential file remain read-only. The parent owns index staging
and receives only bounded/streamed Borg output through pipes. The child cannot
open the staged or final database. Every credential-helper descendant inherits
Landlock, seccomp and `no_new_privs`; no helper-specific weaker launch path
exists. Only helpers executable from the declared read/execute roots and able
to operate with the finite ADR 0007 environment are supported.

### Runtime validation and fail-closed behavior

`VerifiedImmutableSnapshot` may be constructed only after the descriptor,
provenance, Btrfs, identity, capability, Landlock and seccomp prerequisites are
validated. It owns the open descriptor and immutable identity facts. It has no
path/string constructor, deserializer, mock-to-production conversion or clone
that drops the descriptor.

Before every Borg spawn, re-read the Btrfs subvolume facts through the same
descriptor and compare them to the capability. After the final child is reaped,
repeat that comparison before results can be accepted. A root actor remains
trusted, but these checks catch accidental operator changes. Recovery writes
that receive `EROFS`/`EACCES` fail the operation; there is no repair, writable
retry, alternate path or namespace fallback.

Refuse before Borg access when any of the following holds: non-Btrfs source;
ordinary/local read-only subvolume; missing or mutable provenance record;
zero received identity; kernel below the profile floor; Landlock ABI below 3
or disabled/blocked; unavailable seccomp filter; root/capable caller;
service-owned subvolume root; idmapped or ambiguous mount; nested repository
mount; unsafe state/key/credential topology; missing procfs FD access; Borg
profile mismatch; accessible backing device; unsafe provenance-record opening;
unisolated receive history; unknown/unsupported policy generation; or inability
to install the complete child restrictions. Policy generation must account for
the executing syscall architecture and ioctl encodings, reject unreviewed
compatibility entry points, and refuse unsupported kernel/UAPI combinations
before Borg access. It must never silently drop a required denial.

The current sandbox is therefore unsupported: the workspace is ext4, the temp
surface is not a qualifying received Btrfs subvolume, namespace construction
failed in the prior investigation, and no privileged Btrfs/Landlock acceptance
test was authorized in this lane.

## CI and acceptance strategy

The existing GitHub workflow runs only Rust build/test/clippy/format on
`ubuntu-latest`; it installs neither Borg nor a privileged snapshot fixture.
GitHub's runner image inventory is explicit about installed software, and
`ubuntu-latest` is a fresh hosted image rather than a product-specific storage
environment
([Ubuntu runner inventory](https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md),
[runner selection](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)).

Ordinary CI may test parsing, sealed types, syscall-policy generation, FD
lifetime plumbing and fail-closed behavior. A mock may never produce the
production capability or make the backend enabled.

Backend acceptance requires a separate disposable, privileged Ubuntu VM or
dedicated ephemeral runner. The fixture must use a fresh loopback Btrfs
filesystem, a throwaway Borg 1.4.4 repository, a send/receive-created snapshot,
root-owned provenance, a non-root BackupSage UID and private synthetic
credentials/state. It must be destroyed after the run and must never attach a
production device, configured Borg state, backup timer or `/mnt/borgnvme`.

### Explicit implementation acceptance matrix

| Gate | Required non-vacuous evidence | Ordinary CI | Privileged acceptance | Enablement rule |
|---|---|---:|---:|---|
| Point-in-time provenance | Root provisioner creates receive-time record; same-FD ioctls match FSID/tree ID/UUID/received UUID/transactions and RO flag | Parser/negative only | Required | Any absent/mismatch refuses |
| Provenance-record FD | Symlink in any component, wrong type, owner/mode/ACL, identity mismatch and replacements before/after open/stat; parse only the validated FD | Required | Required races | Refuse or retain the validated record; never reopen its path |
| Receive-window isolation | Barrier-controlled access attempts as service UID during receive, after receive but before RO verification, and before record installation all fail | Harness logic | Required | Early access blocks enablement |
| Backing-device denial | Service UID before confinement, Borg-policy probe and recursive helpers cannot open the fixture loop/block device read-only or writable; no inherited or received device FD | Policy/FD checks | Required | Any device access blocks enablement |
| Stable object | Swap/rename/overmount the display locator after validation; all Borg opens remain on the descriptor object or fail | FD unit tests | Required with real Borg 1.4.4 | No path reopen/fallback |
| Create/write/truncate | Real child and helper attempts through FD, original, symlink and bind aliases receive denial | Policy-shape only | Required | All operations denied |
| Unlink/rename | Same aliases, same child policy, same repository fingerprint/metadata | Policy-shape only | Required | All operations denied |
| Descendant inheritance | Helper forks/execs and recursively spawns; restrictions and process-group cancellation remain effective | Process tests where available | Required | One weaker descendant blocks enablement |
| RO flag authority | Service/helper cannot issue `BTRFS_IOC_SUBVOL_SETFLAGS`; root-owned, non-idmapped checks hold | Negative validation | Required | Any service authority refuses |
| Mutating ioctls and metadata | Every applicable request/FD role below and unmediated metadata mutation has a valid target, valid arguments and an observed denial | Policy generation only | Required | Wrong-FD errors or omitted variants are not proof |
| Post-confinement FD receipt | External `SCM_RIGHTS` and `pidfd_getfd` acquisition attempts fail in descendants | Policy checks | Required | No imported writable/device authority |
| Namespace/mount escape | `unshare`, namespace-bearing clone, `clone3`, mount and new-mount API attempts fail in descendants | Seccomp bytecode/API tests | Required | Any mount escape blocks enablement |
| Borg recovery/replay | Damaged throwaway indexes/transactions reach real recovery/unlink/replay attempts, mutation is denied, operation fails, no fallback | Not sufficient | Required | Must fail closed with identical source |
| Private state | Borg security/base/cache state persists and is writable with 0700/0600; source, keys, credential and DB remain non-writable | Topology/unit tests | Required | Only approved state may change |
| Success/failure integrity | Full repository bytes and metadata match before/after success, child failure, timeout and cancellation | Harness logic | Required | Any drift blocks enablement |
| Unsupported host | ext4, plain RO mount, missing record, old/disabled Landlock and blocked seccomp refuse before Borg exec | Required | Required spot checks | No best-effort mode |
| Two-way proof and counts | Working positive controls plus every deliberately inverted fixture below makes its invariant check fail; exact executed counts retained | Harness accounting | Required | Zero matches, skips replacing required cases, or undetected inversions block enablement |

### Mutating-ioctl denial suite: valid targets, not just request numbers

The following minimum suite is derived from the
[Linux v6.12 Btrfs ioctl handlers](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/ioctl.c),
[Btrfs UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/btrfs.h),
[VFS ioctl dispatch](https://github.com/torvalds/linux/blob/v6.12/fs/ioctl.c) and
[VFS remap checks](https://github.com/torvalds/linux/blob/v6.12/fs/remap_range.c).
That source tag verifies terminology and FD roles; it does not choose a new
kernel floor or prove an Ubuntu kernel passes. Acceptance must record and
review the exact deployed kernel/UAPI, architecture and generated policy.

| Requests | Applicable FD and non-vacuous attempt |
|---|---|
| `BTRFS_IOC_SUBVOL_SETFLAGS` | Issue directly on the pinned repository-root FD with flags that actually clear RO. Root ownership/capability checks must deny; also test a sacrificial service-owned subvolume root to isolate the seccomp denial. |
| `BTRFS_IOC_SNAP_CREATE`, `BTRFS_IOC_SNAP_CREATE_V2` | The ioctl FD is the **destination directory**; the argument's `fd` is the source subvolume root. Use the pinned root as source and an appropriate writable directory in same-filesystem private state as destination; exercise V2 RO and writable snapshot requests. Also attempt creation inside the pinned RO root. A cross-filesystem `EXDEV` is not containment evidence. |
| `BTRFS_IOC_SUBVOL_CREATE`, `BTRFS_IOC_SUBVOL_CREATE_V2` | Use the pinned root as destination and a same-filesystem private-state directory as a valid writable destination control. Ordinary private-state file writes do not authorize subvolume creation. |
| `BTRFS_IOC_SNAP_DESTROY`, `BTRFS_IOC_SNAP_DESTROY_V2` | Name-based requests use the target's parent-directory FD and a real sacrificial subvolume name. Use an appropriate same-filesystem private-state parent for reachable child attempts; denied access to the repository's trusted parent is a separate gate. V2 must cover both name and `BTRFS_SUBVOL_SPEC_BY_ID`: by-ID can resolve a target beyond the ioctl FD's directory, so attempt it from the pinned root and a writable private-state FD with an existing target ID. Do not credit `ENOENT` or an impossible nested-subvolume target. |
| `BTRFS_IOC_SET_RECEIVED_SUBVOL` | Use the pinned subvolume-root FD and well-formed received metadata. Also use a sacrificial writable, service-owned subvolume root to test the filter independently of repository RO/ownership. |
| `BTRFS_IOC_DEFRAG`, `BTRFS_IOC_DEFRAG_RANGE` | Exercise directory defrag on the pinned root and file/range defrag on regular-file FDs opened relative to it, plus a private-state regular-file FD. The handler permits a read-only file FD when other permissions allow: `O_RDONLY` alone is not denial. |
| `FICLONE`/`BTRFS_IOC_CLONE`, `FICLONERANGE`/`BTRFS_IOC_CLONE_RANGE` | These are matching VFS/Btrfs request encodings. The ioctl FD is the regular-file **destination**; the argument identifies the source FD. Attempt repository-destination access relative to the pin (write-open must fail), and actual ioctls on writable same-filesystem private-state files with valid source files/ranges, including a repository source. A directory FD, zero-length no-op or cross-filesystem pair is not proof. |
| `BTRFS_IOC_ENCODED_WRITE` | Requires a writable regular-file FD and valid encoded payload, not the pinned directory FD. Repository write-open must fail; use a same-filesystem private-state file for the actual ioctl denial. The reviewed handler also requires `CAP_SYS_ADMIN`; a separate privileged sacrificial control must establish payload/operation validity, without granting that capability to Borg or helpers. Cover native/compat encodings if an ABI is permitted. |

Run the direct syscall probes under the exact Borg child policy and in recursive
helpers, alongside the real sealed Borg reads. Every row needs a separately
isolated sacrificial success control with suitable authority and arguments,
followed by the confined denial. Record which layer denied it (Btrfs RO,
ownership/capability, Landlock, or seccomp). Where layers mask one another,
isolate the layer under test on an inverted disposable fixture. Never inherit
a repository writable FD just to make the supported profile's test run.
For private-state operations, the targeted ioctl filter must still deny even
though normal state writes are allowed. An unsupported request, malformed
payload, wrong FD type, no-op, unexecuted probe or arbitrary nonzero exit is
not a passed denial. Missing coverage/unknown policy generation fails closed;
additional mutating requests found in the deployed UAPI must be reviewed before
enablement. This is not a claim that the table exhausts every future ioctl.

### Two-way privileged proof and exact accounting

Keep positive controls for all three sealed Borg 1.4.4 reads (repository JSON,
archive JSONL, and one exact-file stdout extraction), with expected identity
and synthetic content, encrypted/helper and unencrypted cases, and successful
persistent private base/cache/security writes with the approved modes. A harness
that merely fails every child cannot pass. Retain source byte/metadata checks
on success, error, recovery/replay, timeout and cancellation.

Deliberately inverted disposable fixtures must make the corresponding invariant
check go red for **each** of these changes:

- RO cleared; use a metadata operation not mediated by Landlock to isolate RO
  where other layers would mask an ordinary write.
- Landlock omitted, a required handled right missing, or an overbroad allow
  rule; use a service-writable clone/unrelated tree so Btrfs RO does not mask it.
- The pinned FD closed or deterministically reused for a different object,
  including across Borg/helper launch and late in descendant lifetime.
- Provenance mismatched or replaced, including replacement between preliminary
  stat and open, and between FD validation and parsing. The correct path refuses
  or stays on the original record; an inverted pathname reopen must be caught.
- The display locator swapped to a different repository; a deliberately broken
  path-based child launch must fail the object-identity oracle even if both
  repositories share a Borg ID. The correct FD-based launch stays pinned or
  refuses.
- The receive destination accessible to the service UID before RO and provenance
  installation; barrier-controlled probes must catch premature access, including
  the gap after RO but before the record exists.
- Backing-device access granted to the service UID, or a device FD leaked into
  the child; a pre-confinement service probe must catch a bad deployment even
  when child Landlock would mask it. Any isolated device-open success control
  uses only the disposable device and performs no raw writes.

An inverted test is successful evidence only when it demonstrates the intended
assertion failure/refusal; do not require removing one layer to defeat every
other layer. The unchanged harness must detect the injected defect, and its
failure must be attributable to that defect rather than unrelated setup or
authentication failure. Never weaken a production capability for these controls.

Record exact discovered, selected, executed, passed, failed and skipped counts
for positive, denial, race and inverted cases, with case names and the exact
filter/command and environment versions. Assert expected cases actually ran;
a zero-match filter is failure, and a required skipped/unsupported case leaves
enablement blocked. No invented fixed suite size substitutes for run evidence.

No row is satisfied by this refinement round. Executed this round: **0 privileged
acceptance tests and 0 mutation/inverted-fixture tests**. Mutation execution
remains a future privileged acceptance obligation, not evidence this round
supplies. No executable invariant changes; Cargo and failure-atlas runs are not
required here. Issue #82 must produce and preserve the privileged evidence
before the production capability can exist.

## Alternatives considered

- **Bubblewrap/mount namespace plus read-only bind.** A namespace can hide
  aliases, but does not establish snapshot provenance. It also failed to start
  in the current sandbox, so making it the only backend would provide no local
  implementation path and still need a separate snapshot trust decision.
- **Landlock alone.** Rejected as provenance. It also does not mediate several
  metadata operations and does not retroactively restrict already-open file
  descriptors. Btrfs read-only storage and descriptor hygiene remain required.
- **Current Btrfs `ro` flag or `received_uuid` without provenance.** Rejected.
  An ordinary subvolume can be made read-only after modification, and Btrfs
  documents a legacy caveat for received subvolumes predating the
  btrfs-progs 5.14.2 safety checks.
- **Local Btrfs snapshot without a trusted creation record.** Rejected for the
  same reason: runtime state says nothing trustworthy about when it became the
  selected point in time.
- **Read-only bind, chmod, Borg safe verbs, config hashes or pre/post hashes.**
  Rejected by ADR 0007. They do not provide all three properties and endpoint
  hashes do not exclude ABA changes.
- **LVM snapshot mounted read-only.** Rejected for this profile because the
  same block snapshot can be exposed writable elsewhere; mount flags are a
  view, not immutable-object provenance.
- **ZFS snapshots.** Potentially viable, but no ZFS deployment, pinning profile
  or non-vacuous Ubuntu acceptance evidence was available in this lane. It
  requires a separate proposed backend decision.
- **EROFS/SquashFS with dm-verity or fs-verity.** Potentially stronger image
  provenance, but the operator image-build trust root, backing-device pin and
  privileged mount lifecycle are not specified or evidenced here. The kernel
  describes EROFS as an immutable image filesystem, but that alone is not a
  complete BackupSage deployment profile
  ([EROFS documentation](https://docs.kernel.org/filesystems/erofs.html)).
- **OCI/container read-only volume.** Container policy can constrain a view,
  but neither proves that the input is a point-in-time object nor rules out a
  writable host alias without an approved storage and runtime profile.

## Consequences

- Initial Borg enablement is intentionally narrow: received Btrfs snapshots on
  a root-provisioned Ubuntu/Linux host only. ext4, ordinary Btrfs read-only
  subvolumes, hosted CI and namespace-only sandboxes refuse.
- The operator must provision immutable source provenance outside BackupSage.
  This is a new platform/trust-root decision beyond ADR 0007, which is why this
  document is a proposed ADR rather than implementation evidence hidden in
  issue #82.
- The runtime needs Linux-specific Btrfs ioctls, `openat2`, procfs FD paths,
  Landlock and seccomp. There is no portable fallback.
- Borg and helpers get a smaller read surface and exactly one writable private
  state hierarchy. Unexpected attempts fail the indexing operation, which can
  make some otherwise valid helpers unsupported.
- A root-owned manifest is security state. Its format must be versioned and
  validated, but it is not a public BackupSage report/contract and must never
  be accepted from repository contents.
- No `VerifiedImmutableSnapshot`, child runtime, CLI, Borg parser, test harness
  or executable behavior is implemented by this ADR.

## Safety invariant

BackupSage never rewrites or deletes from an archive. A Borg repository is read
only from the exact root-provisioned received snapshot object; no live
repository, pathname substitute or weaker child is accepted.
