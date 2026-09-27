# Legacy index fixtures (#105)

These indexes were written by the **old writers themselves**, not by today's
writer with columns dropped. `tests/legacy_paths.rs` reads them.

| Fixture | Writer | Source archive |
|---|---|---|
| `pre101-pax00.db` | `bd9b3c2` (just before raw-path capture, no `path_raw` / `link_target_raw`) | `../sparse/sparse-pax00.tar` |
| `pre101-pax10.db` | `bd9b3c2` | `../sparse/sparse-pax10.tar` |
| `pre101-oldgnu.db` | `bd9b3c2` | `../sparse/sparse-oldgnu.tar` |
| `v101-pax10.db` | `ac32be1` (v1.0.1: raw-path columns, sparse handling before #63) | `../sparse/sparse-pax10.tar` |
| `pre101-mismatch-pax10.db` | `bd9b3c2` | `mismatch-pax10.tar` |
| `pre101-plain.db` | `bd9b3c2` | `plain-legacy.tar` (no sparse members) |

The sparse archives are the GNU tar 1.35 fixtures from #64. The other two
archives were composed for #105:

- **`mismatch-pax10.tar`** holds an ordinary ustar member `data/real.bin`
  (66 bytes), followed by `sparse-pax10.tar`'s members. In that sparse
  member's pax header, `GNU.sparse.name=holey.bin` was rewritten to
  `GNU.sparse.name=data/real.bin`, with the record length, header size and
  checksum fixed up. Its wrapper name is still
  `./GNUSparseFile.1370099/holey.bin`. GNU tar 1.35 lists `data/real.bin`
  twice. It extracts the 1,048,576-byte sparse file there, with the same
  SHA-256 as `sparse-pax10.tar`'s `holey.bin`, over the ordinary one. The
  old indexer recorded the ordinary `data/real.bin` and the wrapper name,
  and nowhere the real name.
- **`plain-legacy.tar`** was made with
  `tar --format=gnu --sort=name --owner=0 --group=0 --mtime=@1700000001`
  from a directory holding `plain.txt`, `café.txt`, two non-UTF-8 names
  `d\xff` and `d\xfe`, and a symlink `link -> t\xff`. The old indexer
  stored both non-UTF-8 names as `./d\u{fffd}`, flagged the earlier one
  `SHADOWED`, and stored the link target as `t\u{fffd}`.

What the old writers stored for a PAX-sparse member, which is why the
loader refuses every index from before #63 that holds sparse rows:

- **Hash:** BLAKE3 of the *condensed* stream tar-rs yields, not of the
  logical file.
- **Size:** the condensed size: 8192 for 0.0, 8704 for 1.0 (the map preamble
  plus the data), not 1048576.
- **Name:** tar-rs's name, never `GNU.sparse.name`. For 0.1 and 1.0 that
  is the synthetic `./GNUSparseFile.<pid>/holey.bin` wrapper. The real name
  can be anything, as `mismatch-pax10.tar` shows.
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
