# Legacy index fixtures (#105)

These indexes were written by the **old writers themselves**, not by today's
writer with columns dropped. `tests/legacy_paths.rs` reads them.

| Fixture | Writer | Source archive |
|---|---|---|
| `pre101-pax00.db` | `bd9b3c2` (just before raw-path capture, no `path_raw` / `link_target_raw`) | `../sparse/sparse-pax00.tar` |
| `pre101-pax10.db` | `bd9b3c2` | `../sparse/sparse-pax10.tar` |
| `pre101-oldgnu.db` | `bd9b3c2` | `../sparse/sparse-oldgnu.tar` |
| `v101-pax10.db` | `ac32be1` (v1.0.1: raw-path columns, sparse handling before #63) | `../sparse/sparse-pax10.tar` |
| `pre101-shadow-pax10.db` | `bd9b3c2` | `shadow-pax10.tar` |
| `pre101-overwrite-pax10.db` | `bd9b3c2` | `overwrite-pax10.tar` |

The sparse archives are the GNU tar 1.35 fixtures from #64.
`shadow-pax10.tar` is a plain ustar member `holey.bin` (63 bytes),
followed by every member of `sparse-pax10.tar`. When GNU tar extracts it,
the sparse `holey.bin` overwrites the plain one. `overwrite-pax10.tar` is
the reverse: the members of `sparse-pax10.tar`, then a plain `holey.bin`
(61 bytes) that overwrites the sparse one.

What the old writers stored for a PAX-sparse member, which is the point of
these fixtures:

- **Hash:** BLAKE3 of the *condensed* stream tar-rs yields, not of the
  logical file.
- **Size:** the condensed size: 8192 for 0.0, 8704 for 1.0 (the map preamble
  plus the data), not 1048576.
- **Name:** for 0.1 and 1.0, the synthetic `./GNUSparseFile.<pid>/holey.bin`
  wrapper. For 0.0, the real name.
- **Flags:** `SPARSE` (8). No writer before #70 recorded the `content_mode`
  meta key.

Old-GNU (`'S'`) rows carry the logical hash and size, but the row alone
cannot tell them from PAX rows.

## How they were made

```sh
git worktree add old bd9b3c2        # and ac32be1 for v101-*
cargo build --manifest-path old/Cargo.toml
old/target/debug/backupsage index <archive>.tar   # writes <archive>.tar.db
```

Each index was then copied with `VACUUM INTO` and re-`VACUUM`ed at a
1024-byte page size to keep it small. The `files` rows, the `meta` rows and
the column layout were compared before and after, and are identical. The
recorded `source` meta names the path the archive had when it was indexed.
The tests rewrite it (and the recorded archive stat) to point at a copy of
the archive, so the source is reachable.
