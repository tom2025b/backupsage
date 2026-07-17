# BackupSage

Fast CLI tool to index and search words inside large tar backup archives — **without extracting them**.

- Reads `.tar`, `.tar.gz` and `.tar.zst` (detected by content, not file extension)
- Streams the archive on the fly — no temp files, no extraction
- Builds a SQLite FTS5 full-text search index you can query in milliseconds
- Bounded memory: each file is indexed up to a per-file cap (default 16 MiB)
- Live progress bar during indexing

---

## Install

```bash
git clone https://github.com/tom2025b/backupsage
cd backupsage
cargo build --release
# Binary at: ./target/release/backupsage
```

---

## Commands

### `index` — Scan an archive and build the search index

```bash
backupsage index /backups/system.tar.zst
backupsage index /backups/old-server.tar.gz
backupsage index /backups/plain.tar
```

- Detects the compression from the file's magic bytes; a mislabelled archive
  still indexes correctly, and an unsupported format (xz, bz2) fails with a
  clear error instead of producing an empty index
- Skips binary files (null-byte heuristic) — their names are still indexed
- Indexes hard link and symlink names so they are findable
- Saves the index next to the archive: `system.tar.zst.db` (falls back to the
  current directory if the archive's directory is read-only)
- A corrupt archive aborts loudly; an interrupted index is marked incomplete
  and later searches warn about it

**Options:**

| Flag | Description |
|------|-------------|
| `--index <FILE>` / `-i` | Save the index to a custom path |
| `--max-file-size <SIZE>` | Per-file content cap (default `16M`; accepts K/M/G, `0` = names only). Content beyond the cap is not searchable |
| `--no-word-stats` | Skip word-frequency stats (faster; `top` won't work) |

---

### `search` — Find files containing a keyword

```bash
backupsage search password --archive /backups/system.tar.zst
backupsage search hunter2 --snippets
```

Prints matching file paths with per-file match counts, ordered by BM25
relevance. With `--snippets` it also shows the matched text in context.

**FTS5 query syntax** (note the shell quoting — the inner quotes must reach
FTS5 for a phrase search):

| Query | Meaning |
|-------|---------|
| `password` | Files containing the word "password" |
| `'"error 404"'` | Files containing the exact phrase |
| `'config*'` | Files with words starting with "config" |
| `'error AND auth'` | Files with both words |
| `'error NOT debug'` | Files with "error" but not "debug" |

Queries that aren't valid FTS5 syntax (e.g. `don't`, `foo(`) are automatically
retried as a literal phrase, with a note.

**Options:**

| Flag | Description |
|------|-------------|
| `--archive <ARCHIVE>` / `-a` | Archive path, used to auto-locate its `.db` |
| `--index <FILE>` / `-i` | Explicit index path |
| `--limit <N>` / `-n` | Max files to show (default 100; says so when truncated) |
| `--snippets` / `-s` | Show matched text in context |

---

### `top` — Show the most frequent words

```bash
backupsage top --archive /backups/system.tar.zst
backupsage top --limit 100
```

Shows total occurrences, share of the top-N, distinct-file counts and a bar
chart. Words of 3–32 characters are counted.

---

## Index discovery (no flags needed)

1. `--index <path>` if given
2. `<archive>.db` next to the archive, then `./<archive-name>.db`
3. `./backupsage.db` (legacy v0.1 name)
4. Any BackupSage database in the current directory (with a hint printed)

Indexes built by v0.1 remain searchable; re-index to get the new features
(completeness tracking, link names, corrected text extraction).

---

## Honest numbers

- **Memory**: bounded by `--max-file-size`, not archive size. Typical usage
  stays under a few hundred MB even for pathological content; ordinary text
  archives use far less.
- **Database size**: the index stores a **full copy of all indexed text**
  (that's what makes match counts and `--snippets` work). Expect the `.db`
  to be roughly the size of the text content it indexes — it is *not* 2–5%.
- **Security note**: because of the above, the `.db` contains plaintext from
  your backup (including any secrets in config files). Protect it like the
  backup itself.
- **Speed**: streaming + batched SQLite writes; indexing is typically
  decompression-bound.

---

## Architecture

```
File → progress tracker → BufReader → zstd / gzip / none → tar entries
                                                              │
                                              regular file / sparse → read once (≤ cap)
                                              hard/symlink          → name only
                                                              │
                                              binary? name only : text → FTS5 + word stats
```

- **`src/format.rs`** — magic-byte format detection
- **`src/cli.rs`** — clap derive CLI definitions
- **`src/indexer.rs`** — streaming pipeline, SQLite schema and inserts
- **`src/searcher.rs`** — FTS5 queries, index discovery (returns data)
- **`src/main.rs`** — dispatch and table rendering
- **`tests/roundtrip.rs`** — end-to-end tests over real generated archives

---

## License

MIT — Thomas Lane
