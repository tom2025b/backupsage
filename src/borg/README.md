# Issue #82 headless handoff

Implemented by **codex** for `issue-82-borg-runtime-boundary-headless-half`.
This is the non-privileged half only. **No production backend is enabled.**
The issue fetched with `gh issue view 82 --repo tom2025b/backupsage` contains
**17**, not 18, acceptance-criteria checkboxes. Numbering below follows their
actual order. ADRs 0001, 0007 and the accepted 0008 govern the boundary.

## Consuming and extending the API

- `ValidationBackend::validate` returns an owning, opaque
  `VerifiedImmutableSnapshot`. The trait is sealed; `UnsupportedBackend` is the
  only production implementation and always refuses without running Borg.
  The only success implementation is `MockBackend` under `cfg(test)`, inside
  `capability.rs`. It wraps an ordinary writable directory. It is unavailable
  to integration tests, downstream consumers and release builds; no feature
  enables it and no unchecked constructor exists.
- The privileged implementation belongs inside the capability module. Bind its
  backend configuration to trusted registries and the complete private/read
  profile. Add its immutable identity facts, same-FD revalidation and child
  policy installation together. The current production `revalidate` and
  `child_policy_ready` gates deliberately refuse. They are not validation or
  policy stubs that silently succeed in a production build.
- `Operation` borrows the capability, allowing only metadata list, archive list,
  or one stdout extraction. `argv()` returns an inspection copy, not an argument
  extension interface. `ArchiveName` and `RegularFilePath` validate literal
  syntax. Child 2 must establish regular-file type and archive identity from
  validated metadata; path parsing alone cannot establish a Borg item's type.
- `PrivateState::create` takes a dedicated persistent state root, key directory,
  optional credential file, and all source/DB/other protected paths (including
  ordinary backup state if applicable). It reuses `outpath::FileId` and
  `ProtectedSet`; it checks actual modes, ownership, symlinks, hardlinks,
  cross-device trees and containment. DB sidecars are protected too. Keys use
  0500 directories/0400 files and credentials use 0600. These are ordinary
  permission/topology checks, **not confinement**. Same-device bind mounts,
  race-proof descriptor-relative state access and enforced read-only key and
  credential exposure remain obligations of the privileged backend.
- `BorgEnvironment::inherit` starts empty and inherits only the trusted helper
  command. No helper setter, arbitrary environment setter, shell invocation or
  environment Debug implementation is exposed. Secret conflicts produce a
  payload-free error. BackupSage never reads the credential or parses/executes
  the helper command; Borg does that.
- `Runtime::list` bounds listing allocation. `Runtime::extract` calls a streaming
  consumer with at most 64 KiB per chunk and enforces a byte ceiling. Consumers
  must return promptly and treat data as provisional until successful return;
  caller code that blocks indefinitely cannot be preempted by this synchronous
  callback API. A consumer error or unwinding panic triggers child cleanup.
- Runtime checks the exact `borg 1.4.4` version and required list/extract help
  capabilities before sealed access. All probes share the finite environment,
  FD setup, lifecycle and overall execution deadline. The production executable
  is fixed to `/usr/bin/borg`.
- The Linux supervisor is a dedicated subreaper/session leader; Borg leads a
  separate process group. It retains the repository at child FD 9, closes
  unneeded FDs with required `close_range`, uses null stdin, concurrently drains
  and discards stderr, sends SIGTERM then SIGKILL, and reaps descendants before
  returning. It does not change the application's subreaper status. The parent
  retains its pin throughout cleanup. No Landlock/seccomp code is installed.
  Deliberate group/session escape and helper closure/reuse of FD 9 are **not**
  established safe here; ADR 0008 confinement and descendant FD-lifetime evidence
  must be added before enablement.

## Acceptance-criteria status

“Satisfied” below means the specified headless mechanism has executed tests;
it never means a mock proved repository immutability.

| # | Status | Evidence or remaining work |
|---|---|---|
| 1 | Satisfied: API boundary | Private owning fields, sealed validation, no raw constructor/conversions, compile-fail tests; test-only success path. |
| 2 | Partial | Unsupported production backend refuses before exec; immutable provenance and enabled backend validation await privileged acceptance. |
| 3 | Reserved | No actual child-confinement mutation-denial probes run. |
| 4 | Reserved | No damaged recovery/unlink/replay fixture run. |
| 5 | Partial | Owning descriptor and fixed FD locator plumbing exist; real object pinning, descendant FD survival and path/alias attacks await privileged acceptance. |
| 6 | Reserved | No successful/error/recovery/cancellation immutability fingerprints claimed. |
| 7 | Satisfied | Exact 7/8-entry map, poison-variable stripping, actual child env equality, sentinel-free Debug/Display/source errors. |
| 8 | Satisfied: authentication plumbing | Real encrypted Borg with inherited helper; missing and failed authentication refuse; null stdin/no TTY exercised. |
| 9 | Satisfied: credential plumbing | Synthetic private file; real helper invocation argv recorded without secret; helper deliberately prints secret on stderr on success/failure; returned bytes/errors remain free of it. |
| 10 | Partial | Real 0700/0600 creation, persistence, no-clobber and FileId/topology checks pass; real Borg private state persists. Read-only key/credential enforcement and complete race/mount safety await privileged backend. |
| 11 | Satisfied: runtime behavior | No trust-answer variables are forwarded or prompt answers supplied; real unknown-unencrypted access refuses in a new private profile. Existing operator-established state is retained. No rollback/recovery fixture proof claimed. |
| 12 | Satisfied: closed API | 14 compile-fail examples plus runtime literal-input refusals and detached argv injection test. No arbitrary command/environment/output mode API. |
| 13 | Satisfied | Exact three argv arrays and format string; all three run against real 1.4.4; prefix-neighbor file excluded by exact `pf:` extraction. |
| 14 | Satisfied: process mechanics | Bounded listing refusal, 32 MiB chunked stdout and multi-megabyte concurrent stderr saturation pass. |
| 15 | Satisfied: process-group mechanics | Timeout/cancellation, nonzero/early exit, consumer failure and unwinding cleanup; three actually started SIGTERM-ignoring processes have no live or zombie PID after return. Real confinement/escape resistance remains reserved. |
| 16 | Satisfied: error boundary | Payload-free categories; raw Borg/helper stderr is discarded, not decoded/logged/returned. |
| 17 | Satisfied | Full existing tar/directory, CLI and frozen contract suite passes; no owned CLI/index/store behavior changed. |

## Reserved for the privileged follow-up

The following are the **exact issue #82 acceptance bullets** corresponding to
the unimplemented privileged work. They are not checked off by these tests.

2. “Each enabled backend proves that the source is an immutable point-in-time
   object, not merely a read-only view of a changing live repository.
   Unsupported or inconclusive platforms refuse before `/usr/bin/borg` runs.”
   This reserves real Btrfs receive provenance, same-FD facts, deployment checks,
   root-owned registry and backing-device/receive-window validation. Disposable
   loop-device, mkfs and mount setup belongs to Tom's privileged fixture.
3. “Under the actual child confinement, create, write, truncate, unlink, and
   rename attempts against the repository fail through the selected locator
   and every test-visible alias.”
   This reserves real Landlock/seccomp policy and mutation-denial tests through
   source/aliases and recursive helpers, including state/unrelated-path controls.
4. “Damaged throwaway fixtures exercise Borg index-recovery/unlink and
   transaction-replay attempts. Repository mutation is denied, the operation
   fails, and no repair fallback runs.”
   All damaged-fixture recovery, unlink and replay work is reserved.
5. “The snapshot object remains pinned across validation and child execution;
   synthetic path swaps and writable-alias attacks cannot redirect Borg or its
   helper.”
   Real-backend path replacement, writable aliases, overmounts, descendant FD
   closure/reuse, provenance replacement and race tests are reserved.
6. “Repository bytes and metadata remain identical on successful reads, child
   errors, cancellation, and recovery/replay attempts.”
   Real enforced integrity oracles for every outcome are reserved. Ordinary
   writable mock repos are not used as substitutes for these oracles.
10. “Private Borg base/cache/security state persists across runs with mode-0700
    directories and mode-0600 files, remains outside all sources/indexes, and
    does not touch ordinary Borg state. Key inputs are exposed read-only.”
    Permission/persistence work is done; actual child write-surface enforcement,
    read-only keys/credentials and race/mount isolation remain reserved.
15. “Cancellation and timeout tests prove the entire process group is terminated
    and reaped, including a hostile or stalled credential helper.”
    The fake process-group proof is done. Repeat under the real child policy,
    including recursive helpers, escape resistance and FD lifetime guarantees.

ADR 0008's **two-way inverted-fixture proofs** are also wholly reserved. RO
cleared; Landlock omitted, missing rights or overbroad rules; pin closed/reused;
provenance mismatched/replaced; display locator swapped; receive destination
prematurely accessible; and backing-device access/FD leakage must each make the
unchanged corresponding oracle fail. These support criteria 2–6, 10 and 15;
issue #82 has no separate eighteenth acceptance checkbox for them. Preserve the
exact privileged discovered/selected/executed/passed/failed/skipped counts.

Privileged tests executed here: **0**. Real filesystem-denial, recovery/replay,
real-backend race and inverted-fixture tests executed here: **0**. This module
cannot close issue #82 or enable production Borg support.

## Validation evidence

Environment: Linux `7.0.0-31-generic`, UID 1000, `rustc 1.98.0 (88d9e12ae 2026-08-18)`,
`/usr/bin/borg --version` = `borg 1.4.4`. No sudo, root/CAP_SYS_ADMIN operation,
loop device, mount or actual Landlock ruleset was used.

- `cargo test --offline borg:: --lib`: 12 selected, 12 executed, 12 passed,
  0 failed/ignored; 79 existing tests filtered out. Eleven tests have the
  `mock_backend_` prefix and explicit API/error/process-only doc comments. One
  test checks pure finite-environment logic without obtaining a mock capability.
- `cargo test --offline --test borg_runtime`: 5 selected/executed/passed,
  0 failed/ignored. Ordinary filesystem modes/topology and public API only.
- `cargo test --offline --doc`: 14 selected/executed/passed compile-fail API
  examples, 0 failed/ignored. No backend execution.
- Full `cargo test`: 198 top-level tests passed, 0 failed, 1 pre-existing ignored
  helper (`plan_emit_helper`, explicitly invoked by the existing multi-process
  test). Its six child invocations also pass and are not double-counted.
  The first sandboxed attempt failed only at two existing `UnixListener::bind`
  fixture setups with EPERM (88 lib tests passed, 2 failed, 1 ignored); the
  automatic reviewer allowed an ordinary-user retry with Unix sockets available,
  and the whole suite passed. This was sandbox escalation, not root execution.
- `cargo clippy --all-targets --locked -- -D warnings`: passes.
- `cargo fmt --all -- --check`: passes.

New tests total: **31** (12 unit + 5 integration + 14 compile-fail), all run;
none skipped. Full logs were captured under `/tmp/issue-82-cargo-test-final.log`
and `/tmp/issue-82-clippy.log`. Real Borg tests deliberately fail if the fixed
1.4.4 executable is absent/mismatched; they do not silently skip. CI provisioning
of Borg 1.4.4 belongs to the coordinator because workflow edits are outside this
task's allowed paths.

All fixtures use fresh temporary roots, synthetic content and private credentials
and state. They never consult configured repositories, ordinary Borg state,
backup timers or `/mnt/borgnvme`. No git write was performed by codex; coordinator
checkpoints observed during this session are external to this implementation.
