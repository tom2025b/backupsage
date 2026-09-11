# Borg containment backend investigation

Date: 2026-09-11
Worktree: `/home/tom/projects/bs-82-backend-architecture`
Branch/base: `issue-82-backend-architecture` at
`4eb563c1d3b3a27021ca23bea6fc14f296c552f7`
Outcome: proposed ADR 0008; no implementation, commit, PR or accepted ADR
Refinement task: `adr0008-refinement-producer` · Producer: codex

## Question and result

Issue #82 cannot be implemented honestly until one backend establishes, as a
single chain, point-in-time provenance, stable object identity and real
filesystem denial for Borg plus helpers. The available evidence supports one
narrow proposed profile:
`linux-btrfs-received-ro-landlock-v1`.

The source must be a root-provisioned, receive-time-registered, read-only
received Btrfs subvolume. BackupSage opens and validates it by descriptor,
passes Borg only `/proc/self/fd/N`, and applies Landlock ABI 3+, empty
capabilities, `no_new_privs` and seccomp to the entire child tree. The only
writable hierarchy is ADR 0007's private Borg state.

This is an architectural decision, not a locally proven backend. The current
sandbox is not a supported instance and no production capability may be
constructed until the privileged acceptance matrix in ADR 0008 passes.

## Material read completely

The following inventory and local experiments record the original draft lane.
This refinement separately re-read ADRs 0004, 0006 and 0007, the complete ADR
0008 and this report, the ADR index, and live issue #82 (OPEN, no comments),
before editing. Its initial status already contained the two untracked draft
documents and the modified proposed ADR-index row; branch and HEAD still
matched the frozen base above. No earlier experiment was rerun here.

- `README.md`; ADRs 0001 through 0007; `docs/adr/README.md`;
  `docs/investigations/2026-09-11-borg-source-design.md`
- live GitHub issues
  [#80](https://github.com/tom2025b/backupsage/issues/80),
  [#82](https://github.com/tom2025b/backupsage/issues/82),
  [#83](https://github.com/tom2025b/backupsage/issues/83), and
  [#84](https://github.com/tom2025b/backupsage/issues/84)
- `.github/workflows/ci.yml`, all of `src/outpath.rs`,
  `src/indexer.rs` around `run_index`, `IndexPaths`, replacement ownership and
  `create_db_with_fallback`, and all of `tests/common/mod.rs`

Repository observations that shape this decision:

- `outpath::FileId` is device/inode identity for ordinary destination safety,
  but it does not pin a directory across a child exec or attest Btrfs
  provenance.
- `ProtectedSet` protects directory trees by canonical pathname and only treats
  `source_type == "dir"` as a tree. It is not the Borg containment boundary.
- `create_db_with_fallback` may choose a second output pathname. ADR 0007 and
  issue #83 instead require an explicit Borg DB and no fallback.
- `IndexPaths` preserves a previous DB until promotion, but source containment
  must succeed before staging or Borg access.
- Current CI is an ordinary `ubuntu-latest` Rust job with no Borg install or
  privileged snapshot setup.

## Re-derived threat model

### Required guarantees

| Property | Threats that defeat a partial design | Required anchor |
|---|---|---|
| Point-in-time provenance | Live repository behind RO bind; ordinary subvolume made RO after changes; stale flags | Root receive-time record plus received-subvolume kernel identity and RO flag |
| Stable object | Symlink swap, rename, mount replacement, canonical-path reopen | One open directory FD used for validation and every Borg open |
| Mutation denial | Borg recovery, helper write, truncation without write open, unlink/rename, alternate locator | Btrfs subvolume RO plus descendant Landlock and namespace/mount denial |

### Trusted and untrusted actors

Trusted: initial-namespace root/operator provisioner, kernel/VFS/Btrfs,
Landlock/seccomp, the BackupSage parent and the exact packaged Borg profile.
Untrusted or fallible: Borg read paths, the credential helper and descendants,
repository contents, user-controlled path components, aliases and ordinary
backup writers. Malicious root/kernel/firmware is outside the achievable local
threat model and is stated rather than hidden.

The Btrfs root inode must not be owned by the service UID. Linux v6.12 source
checks `inode_owner_or_capable` before changing subvolume flags, so accepting a
service-owned root would let a child attempt to clear read-only authority
([Linux Btrfs ioctl implementation](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/ioctl.c)).
Empty capabilities, `no_new_privs`, non-idmapped mounts and root ownership are
therefore part of the profile, not hardening notes.
The documented btrfs-progs received-UUID reset workflow is a userspace safety
check, not a kernel ban on direct RO-clear ioctls: the reviewed flag handler
does not itself clear `received_uuid`. Current received identity alone is
therefore still insufficient provenance.

## What was verified locally in the original draft lane

Read-only inspection commands:

```sh
pwd
git branch --show-current
git rev-parse HEAD
git status --short --branch
/usr/bin/borg --version
bwrap --version
uname -a
lsb_release -ds
findmnt -T /home/tom/projects/bs-82-backend-architecture -no FSTYPE,OPTIONS
findmnt -T /tmp -no FSTYPE,OPTIONS
stat -c 'uid=%u gid=%g mode=%a' /usr/bin/borg
getcap /usr/bin/borg
rg -n '^NoNewPrivs|^Cap(Inh|Prm|Eff|Bnd|Amb)' /proc/self/status
gh issue view 80 --json number,title,state,body,labels,url
gh issue view 82 --json number,title,state,body,labels,url
gh issue view 83 --json number,title,state,body,labels,url
gh issue view 84 --json number,title,state,body,labels,url
```

Observed:

- branch and base exactly matched the requested values; the initial worktree
  was clean;
- `/usr/bin/borg --version` reported `borg 1.4.4`;
- the host reported Ubuntu 26.04.1, kernel 7.0.0-31-generic;
- the workspace was ext4 and no local path was a qualifying received Btrfs
  source;
- Bubblewrap was 0.11.1; the earlier ADR 0007 investigation's namespace smoke
  failed with `No permissions to create a new namespace`;
- the inspecting process had empty inherited/permitted/effective/bounding/
  ambient capability sets and `NoNewPrivs: 1`; and
- all four issues were OPEN. Issue #82 expressly requires a new ADR question
  when platform support needs policy beyond ADR 0007.

No configured Borg repository, ordinary Borg state, credential, timer,
production path or `/mnt/borgnvme` was inspected.

### Throwaway Borg FD-locator experiment

One experiment used a fresh `/tmp/backupsage-82-fd.XXXXXX` root, a new
unencrypted Borg 1.4.4 repository, one 19-byte synthetic file, and separate
private author/read base, cache, security and key directories. Fixture creation
used the unknown-unencrypted opt-in once as fixture-author setup. The actual
read profiles did not inherit it.

The material commands were:

```sh
borg init --encryption=none TEMP/repo
exec 9<TEMP/repo
BORG_UNKNOWN_UNENCRYPTED_REPO_ACCESS_IS_OK=yes \
  borg create -- /proc/self/fd/9::fixture TEMP/input
borg list --json --bypass-lock -- /proc/self/fd/9
borg list --json-lines --consider-part-files --bypass-lock \
  --format '{type}{mode}{uid}{gid}{size}{isomtime}{archiveid}{archivename}' \
  -- /proc/self/fd/9::fixture
```

Every invocation actually used `env -i`, fixed `PATH`, `LC_ALL`, `TZ`, and
private `BORG_BASE_DIR`, `BORG_CACHE_DIR`, `BORG_SECURITY_DIR` and
`BORG_KEYS_DIR` beneath the temporary root. Repository JSON reported
`"location": "/proc/self/fd/9"`; JSONL returned the expected directory and
19-byte regular file with the expected archive ID. A post-check found no
matching temporary directory, confirming cleanup.

This verifies only Borg 1.4.4 locator compatibility. It does not prove Btrfs
provenance, Landlock, path-swap resistance or denied writes.

## What official documentation promises

- Btrfs send requires read-only snapshots; receive reconstructs the sent
  filesystem and makes it read-only. Received snapshots expose a received UUID
  and receive transaction metadata. Btrfs also warns that received subvolumes
  predating the btrfs-progs 5.14.2 safety checks may have been writable while
  retaining received identity
  ([send](https://btrfs.readthedocs.io/en/latest/btrfs-send.html),
  [send/receive](https://btrfs.readthedocs.io/en/stable/Send-receive.html),
  [subvolume flags](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html)).
- The Linux Btrfs UAPI returns tree ID, flags, UUID, parent UUID, received UUID,
  generation and change/creation/send/receive transaction and time fields
  (`ctransid`, `otransid`, `stransid`, `rtransid`) from
  `BTRFS_IOC_GET_SUBVOL_INFO`. FSID comes separately from `BTRFS_IOC_FS_INFO`;
  there is no single combined atomic query for the entire provenance tuple
  ([UAPI header](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/btrfs.h)).
- `openat2` can confine lookup beneath a trusted directory and reject symlinks
  and magic links during selection
  ([openat2(2)](https://man7.org/linux/man-pages/man2/openat2.2.html)).
- `/proc/self/fd/N` refers to an open descriptor's actual target, including when
  a former pathname is unlinked or mounted over
  ([proc FD documentation](https://man7.org/linux/man-pages/man5/proc_pid_fd.5.html),
  [kernel path lookup](https://www.kernel.org/doc/html/v5.0/filesystems/path-lookup.html)).
- Landlock ABI 3 adds truncation mediation. Its filesystem rights separately
  cover write-open, truncate, removal, object creation and cross-directory
  refer/rename; enforced domains carry into descendants and cannot be removed
  ([Landlock API](https://www.kernel.org/doc/html/latest/userspace-api/landlock.html)).
- `no_new_privs` survives fork/clone/exec and prevents exec from granting
  set-ID or file capabilities. It does not prevent `SCM_RIGHTS` receipt or
  other privilege changes outside exec
  ([kernel documentation](https://www.kernel.org/doc/html/latest/userspace-api/no_new_privs.html)).
- Seccomp filters persist across allowed fork/clone/exec, and unprivileged
  installation is supported after `no_new_privs`
  ([seccomp filter documentation](https://docs.kernel.org/userspace-api/seccomp_filter.html)).
- Creating a user namespace grants capabilities in that namespace, including a
  route to mount-namespace operations, which is why the child policy must deny
  namespace creation instead of assuming an initially empty capability set is
  permanent
  ([user namespaces](https://man7.org/linux/man-pages/man7/user_namespaces.7.html)).
- GitHub's runner image inventory and this repository's workflow do not provide
  a Borg/Btrfs acceptance environment by default
  ([runner image](https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md)).

## Refinement: deployment and descriptor gates

These refine the existing proposed root-provisioned Btrfs/Landlock profile;
they do not select another backend, trust root or locator-publication protocol.
ADR 0007's sealed commands, approved private-state exception and immutable
source requirements remain unchanged. ADR 0004's non-vacuous oracle principle
and ADR 0006's distinction between display and authority remain applicable.

### 1. Backing-device denial

Deployment must deny the BackupSage service UID, including supplementary
groups, Borg and every helper both read and write opens of every Btrfs backing
block device. No backing-device FD may be inherited; device nodes are outside
the Landlock allowlist. A subvolume RO flag is not a raw-device access policy.
Acceptance must attempt opens of the disposable loop/block device as the
service UID before confinement and under the actual Borg/helper policy, and
verify descriptor hygiene. This is an enablement precondition under the already
declared root trust, not protection against malicious initial-namespace root.

### 2. Non-vacuous mutating-ioctl coverage

Btrfs RO and root ownership remain load-bearing. Seccomp's targeted mutating-
ioctl denial is defense in depth; no blanket ioctl allowlist is introduced.
The source-verified minimum suite and precise FD roles are in ADR 0008's
“Mutating-ioctl denial suite: valid targets, not just request numbers”:

| Operation family | Required target roles |
|---|---|
| `BTRFS_IOC_SUBVOL_SETFLAGS` | Pinned subvolume-root FD, with a real RO-clear request; service-owned sacrificial root isolates filter denial. |
| `BTRFS_IOC_SNAP_CREATE` / `_V2` | Destination-directory ioctl FD plus source-root argument FD; use pinned repository as source and a same-filesystem writable private-state directory as destination, and test pinned root as destination too; V2 RO and writable variants. |
| `BTRFS_IOC_SUBVOL_CREATE` / `_V2` | Pinned root or same-filesystem private-state destination directory; normal state writes do not authorize subvolume creation. |
| `BTRFS_IOC_SNAP_DESTROY` / `_V2` | Real target and parent-directory FD for name-based variants; private-state sacrificial parent makes the operation reachable. V2 by-ID also applies through the pinned root or private-state FD and can resolve beyond that directory. No nonexistent nested target as a substitute. |
| `BTRFS_IOC_SET_RECEIVED_SUBVOL` | Pinned root and sacrificial writable service-owned root, with valid received metadata. |
| `BTRFS_IOC_DEFRAG` / `_RANGE` | Pinned directory and regular files opened relative to it; private-state file controls. File defrag can use a read-only FD, subject to the handler's other permission checks. |
| `FICLONE` / `FICLONERANGE` (same encodings as `BTRFS_IOC_CLONE` / `_RANGE`) | Regular-file destination ioctl FD, source argument FD and real data/ranges on the same filesystem. Deny repository write-open and actual ioctls on writable private-state destinations, including a repository source. |
| `BTRFS_IOC_ENCODED_WRITE` | Writable regular-file FD, valid encoded payload; deny repository write-open and ioctl on private state. The reviewed kernel also requires `CAP_SYS_ADMIN`; only the separate sacrificial validity control gets that authority. |

Sources: Linux v6.12
[Btrfs ioctl handlers](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/ioctl.c),
[Btrfs UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/btrfs.h),
[VFS ioctl dispatch](https://github.com/torvalds/linux/blob/v6.12/fs/ioctl.c),
[VFS UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/fs.h),
[VFS remap validation](https://github.com/torvalds/linux/blob/v6.12/fs/remap_range.c).
The pinned tag verifies terminology, not the deployed Ubuntu kernel or a new
kernel floor. Record the deployed UAPI/syscall architecture and policy version;
account for permitted native/compat encodings and reject unsupported entry
points. Unknown or unsupported policy generation, omitted required operations
or newly encountered unreviewed mutators block enablement.

Every request needs valid arguments and an appropriate FD, an isolated
sacrificial success control, and observed denial under the exact child policy
and recursive helpers. Record the denying layer and isolate it where other
layers mask it. Wrong-type/closed-FD errors, `EXDEV`, missing targets, malformed
payloads, no-ops and skipped probes are not denial evidence. No writable
repository FD may be inherited in the supported-profile run. State-file ioctl
denial must coexist with successful approved ordinary state writes.

Landlock does not mediate chmod/chown, xattrs or timestamp updates. Btrfs RO
covers those repository metadata mutations; add direct metadata probes instead
of attributing their denial to Landlock. `btrfs_setattr` checks the RO root
before applying attributes
([Landlock limitations](https://www.kernel.org/doc/html/v6.12/userspace-api/landlock.html#filesystem-flags),
[Btrfs inode operations](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/inode.c),
[Btrfs xattr handlers](https://github.com/torvalds/linux/blob/v6.12/fs/btrfs/xattr.c)).

### 3. Descriptor-safe provenance record

Validate and retain a trusted root-owned registry directory FD protected from
non-root writes/replacement. Open its record relatively with `openat2`,
`O_RDONLY|O_NOFOLLOW|O_CLOEXEC|O_NONBLOCK` and
`RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS|RESOLVE_NO_XDEV`.
Require an FD-checked regular root-owned record, no special bits or group/other
write bits, no ACL permitting non-root writes, and no unsafe hardlink alias.
Record/check device, inode and mount identity; any preliminary stat must agree
with the opened FD. Bound, parse and match the snapshot facts from this FD,
never a pathname reopened after stat. The existing root provisioner uses atomic
replacement, not concurrent in-place edits.

Test component/final symlinks, directories/FIFOs/sockets/devices, wrong owner,
unsafe modes/ACLs, record/snapshot mismatches, and replacement before and after
open/stat, including after FD validation but before parsing. Correct behavior
is refusal or continued use of the validated opened record, never authorization
using replacement bytes. `O_NONBLOCK` prevents FIFO waiting; FD type validation
is still mandatory. Unknown record/profile versions and ambiguous reads refuse
([openat2 UAPI](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/openat2.h),
[Linux open-file description](https://man7.org/linux/man-pages/man2/open.2.html)).

### 4. Receive-window isolation

The root provisioner must make the receive destination inaccessible to the
service UID throughout writable receive and until RO is verified and the
provenance record is installed, including through aliases/pre-existing FDs.
The Btrfs receive manual explicitly warns about concurrent modification and
untrusted send streams
([receive BUGS](https://btrfs.readthedocs.io/en/latest/btrfs-receive.html#bugs)).
Use barriers to probe service-UID access during receive and at the intermediate
RO/record stages. Every premature access must be denied. This adds no new
locator-publication protocol.

### 5. Two-way privileged proof

For every protection, the unchanged harness must detect its deliberately
inverted fixture: RO cleared; Landlock omitted, a required handled right missing
or an overbroad allow rule; pinned FD closed or deterministically reused;
provenance mismatched/replaced; display locator swapped; receive destination
accessible before RO and provenance installation; and backing-device access
granted or a device FD leaked. Require an attributable red invariant check,
not an unrelated setup/authentication failure. Correct provenance/path race
handling refuses or stays pinned; deliberately reopening the replacement must
be detected, even if the substitute repository shares a Borg ID.

Use an unmediated metadata mutation to expose missing RO, a service-writable
clone/unrelated tree to expose missing Landlock, and a pre-confinement UID probe
to expose backing-device permissions even when child Landlock masks them.
Isolate layers on sacrificial fixtures as necessary; removing one protection
need not defeat all the others. Device validity controls open only the
disposable device and do not write raw blocks.

Keep successful encrypted/helper and unencrypted controls for all three sealed
Borg reads, exact synthetic identity/content, and persistent approved private
base/cache/security writes with 0700/0600 modes. Keep repository byte/metadata
integrity checks for success, error, timeout, cancellation and recovery/replay.
The owning snapshot FD and reserved Borg FD number/object must remain valid
for the full Borg/helper lifetime, until every descendant is reaped. Every
process using `/proc/self/fd/N` needs its own N bound to that object; a parent
FD does not prove a child still holds it. Test late closure/reuse and helper
launch, with fail-closed behavior and no path fallback or substitute results.

Record exact discovered, selected, executed, passed, failed and skipped test
counts by positive/denial/race/inverted category, with case names, filters and
environment versions. Assert the expected cases actually executed. A zero-match
filter is failure; skipped required cases and undetected inversions block
enablement. Counts must come from the future run, not a promised suite size.

### Post-confinement descriptor receipt

`no_new_privs` is not an `SCM_RIGHTS` boundary. The descendant policy must close
inherited IPC sockets and deny `recvmsg`, `recvmmsg`, `pidfd_getfd` and permitted
ABI alternate entry points; io_uring denial also prevents substitute receipt.
Helpers needing these acquisition paths are unsupported. Test attempted
external writable/device-FD receipt under the actual policy
([kernel caveat](https://docs.kernel.org/userspace-api/no_new_privs.html),
[pidfd_getfd](https://man7.org/linux/man-pages/man2/pidfd_getfd.2.html)).

## What remains inference or unverified

- The combination is inferred to satisfy the three properties from the
  documented kernel contracts. It has not been exercised end to end here.
- No Btrfs receive-time provenance record format or privileged provisioner has
  been implemented. Root ownership is the proposed local trust mechanism.
- Landlock availability and the complete Borg/Python/helper read allowlist were
  not probed in this sandbox. A best-effort subset is forbidden.
- The seccomp filter has not been compiled or tested against Borg/helper
  process creation. `clone3` fallback and namespace-bearing `clone` denial are
  acceptance gates.
- Borg recovery/unlink/replay on damaged received snapshots has not been
  triggered under this profile.
- Descriptor pinning survived ordinary Borg access, but locator rename,
  symlink, bind-mount, overmount and FD-number reuse attacks remain privileged
  acceptance cases.
- Root is an explicit trust root. The design does not defend against root
  remounting or changing the snapshot after validation.
- The refined device, ioctl, record-race, receive-window and inverted-fixture
  gates have not been executed. Linux v6.12 source does not prove behavior of
  the exact future Ubuntu build. Its kernel/UAPI and generated filter need
  source review and privileged acceptance before enablement.
- Full Borg/helper FD lifetime is unverified. Borg 1.4.4's repository constructor
  uses `abspath`; its passcommand code calls `subprocess.check_output` with
  `shlex.split` and does not explicitly pass the repository FD. Do not infer
  helper FD inheritance from successful unencrypted list calls
  ([repository source](https://github.com/borgbackup/borg/blob/1.4.4/src/borg/repository.py#L178),
  [passcommand source](https://github.com/borgbackup/borg/blob/1.4.4/src/borg/crypto/key.py#L520)).

## Candidate mechanisms evaluated

| Candidate | Provenance | Pinning | Exact descendant denial | Result |
|---|---|---|---|---|
| Btrfs received RO + root receive record + FD + Landlock/seccomp | Yes, under declared root/kernel trust | Same open object via proc FD | Btrfs RO plus inherited policy; privileged proof pending | Proposed profile |
| Bubblewrap RO bind | No snapshot provenance by itself | Can pin a mount view with a suitable setup | Namespace unavailable here; policy/version still needs proof | Rejected for initial profile |
| Landlock only | None | Does not select the intended object | Misses pre-open descriptors and some metadata operations | Rejected |
| Btrfs `ro`/received UUID only | Current state only; legacy caveat | Possible FD pin | Btrfs denial, but trust history absent | Rejected without receive record |
| RO bind of live repository | Explicitly not point-in-time | Pins only the view | Other writer can change source | Rejected by ADR 0007 |
| LVM snapshot + RO mount | Snapshot exists, but block device can have writable alias | Mount/device pin needed | RO is per mount view | Rejected |
| ZFS snapshot | Plausible immutable snapshot | Backend not designed | No local/CI proof or exact child profile | Separate future ADR |
| EROFS/SquashFS + verity | Plausible immutable image | Backing image/device proof needed | Strong FS denial possible | Separate future ADR |
| chmod, safe verbs, hashes/config checks | No | No | No kernel denial | Rejected by ADR 0007 |

No candidate was credited from a mock, chmod test or current mount flags.

## Bounded next experiment

Run only after approval in a later implementation/acceptance lane, on a fresh
disposable privileged Ubuntu VM:

1. Create a loopback Btrfs filesystem under a new temporary root; create a
   throwaway Borg 1.4.4 repository and private synthetic encrypted credentials/
   state.
2. Create a read-only source snapshot, send/receive it, and atomically install
   the root-owned provenance record. Keep the receive destination inaccessible
   to the service UID until RO verification and record installation, with
   barrier-controlled denial probes. Deny backing-device opens to the service
   UID, Borg and helpers; inherit no device FD. Run the backend as a dedicated
   non-root, capability-empty UID.
3. Open and validate the received subvolume once, replace the display locator,
   and invoke the three sealed Borg profiles only through `/proc/self/fd/N`.
4. From Borg and a recursively spawning helper, attempt create, write,
   `O_TRUNC`, `truncate`, `ftruncate`, unlink and same/cross-directory rename
   through the FD locator, original path, symlink, pre-created bind alias and
   test hardlink aliases. Run the complete source-verified mutating-ioctl suite
   above with its correct FD roles, metadata probes, namespace/mount escape
   syscalls and post-confinement FD-receipt attempts.
5. Exercise real Borg damaged-index unlink/recovery and transaction-replay
   paths. Require operation failure, no repair fallback and byte/metadata-
   identical repository state after success, error, timeout and cancellation.
6. Prove only private base/cache/security state changes, permissions remain
   0700/0600, and all descendants are killed before cleanup.
7. Exercise every record race and inverted fixture above, preserve successful
   controls for the three sealed reads, and record exact execution counts.
   FD number/object lifetime must cover all Borg/helper execution, including
   late cancellation. Zero matches or skipped required cases fail acceptance.

This experiment needs elevated setup and executable harness code, so it was not
run or authored in this design-only lane. It must never use a production
repository, configured state, timer, user credential or `/mnt/borgnvme`.

## Test and mutation disposition

No executable invariant changed. Cargo tests and failure-atlas mutation checks
are N/A. A future harness belongs to issue #82 only after the proposed ADR is
reviewed and approved.

Executed in this refinement: **0 privileged acceptance tests, 0 mutation/
inverted-fixture tests, and 0 new Borg fixture experiments**. Mutation execution
remains a future privileged acceptance obligation, not evidence this round
supplies. Technical terminology was checked against the primary sources linked
above; runtime composition and exact deployment behavior remain unverified as
listed explicitly. No Cargo or failure-atlas run was performed or required.

Refinement document validation: complete contents of both untracked documents
were inspected. Tracked `git diff --check` and full-file
`git diff --no-index --check /dev/null <document>` checks passed, with UTF-8,
trailing-whitespace, final-newline and conflict-marker checks. Only the two
allowed documents were edited this round; the pre-existing proposed ADR-index
row was unchanged. Final status contains only those three allowed paths, HEAD
remains the frozen base, and ADR 0008 remains proposed and uncommitted. No git
history writes, issue/PR changes, fixtures, executable code, privileged setup,
mount/namespace mutation or production/configured Borg access occurred.

## Decision boundary

ADR 0007 approved snapshot-only support and a private-state exception, but it
did not choose Btrfs, a provenance trust root, FD pinning, Landlock, seccomp or
an Ubuntu profile. Selecting them is a new platform/security decision. ADR 0008
therefore remains `proposed`; implementation issue #82 must not treat this
investigation as acceptance.
