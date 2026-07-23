# Security advisories — status and disposition

Current as of 2026-07-23 (v1.0.1). Gate: `cargo audit` must report no
unacknowledged vulnerability; every remaining warning is documented here.

## Fixed in v1.0.1

| Advisory | Crate | Affected | Fixed | Status |
|----------|-------|----------|-------|--------|
| [GHSA-3pv8-6f4r-ffg2](https://github.com/advisories/GHSA-3pv8-6f4r-ffg2) — tar PAX header desynchronization (medium) | `tar` (direct) | ≤ 0.4.45 | 0.4.46 | **Upgraded to 0.4.46.** Not yet mirrored into RustSec at time of fix; verified against the GitHub advisory database. |
| [RUSTSEC-2026-0190](https://rustsec.org/advisories/RUSTSEC-2026-0190) — anyhow unsound `Error::downcast_mut()` after `.context()` | `anyhow` (direct + via `image` → `ravif` → `rav1e`) | ≤ 1.0.102 | 1.0.103 | **Upgraded to 1.0.104.** One lockfile bump covers all paths (every dependent takes `anyhow = "1"`). |

Also current in the lockfile (patched before v1.0.1): RUSTSEC-2026-0067 /
CVE-2026-33056 (tar symlink-chmod) and RUSTSEC-2026-0068 / CVE-2026-33055
(tar PAX size header), both fixed at 0.4.45.

## Documented unmaintained warnings (no fix shipped upstream)

| Advisory | Crate | Dependency path | Disposition |
|----------|-------|-----------------|-------------|
| [RUSTSEC-2025-0119](https://rustsec.org/advisories/RUSTSEC-2025-0119) — unmaintained | `number_prefix` 0.4.0 | `indicatif` 0.17 → `number_prefix` | Accepted. Formats byte counts for the progress bar; no unsafe code, no input from archives. Revisit when `indicatif` ships a release that drops it. |
| [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436) — unmaintained | `paste` 1.0.15 | `image` → `exr` → `pulp` → `paste`; `image` → `ravif` → `rav1e` → `paste` | Accepted. Proc-macro used at compile time only; nothing from archive content reaches it at runtime. Revisit on `image` major updates. |

## Reporting

This is a personal project; open a GitHub issue (private disclosure via
GitHub security advisories if sensitive).

## Invariant

BackupSage never rewrites or deletes from an archive. Every output path is
gated by the v1.0.1 output-safety boundary (`src/outpath.rs`); see
`docs/adr/0001-output-safety-boundary.md`.
