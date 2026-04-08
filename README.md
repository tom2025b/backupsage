# BackupSage

Fast CLI tool to index and search words inside large `.tar.zst` backup archives — **without extracting them**.

- Streams the archive on-the-fly (no temp files, flat memory usage)
- Builds a SQLite FTS5 full-text search index
- Supports 50 GB+ archives
- Shows real-time progress during indexing

---

## Install

```bash
git clone https://github.com/tom2025b/backupsage
cd backupsage
cargo build --release
# Binary at: ./target/release/backupsage
```

Or with the wrapper alias (if installed via `~/bin/r-backupsage`):

```bash
backupsage --version
```

---

## Commands

### `index` — Scan an archive and build the search index

```bash
backupsage index /backups/system.tar.zst
```

- Streams the `.tar.zst` file without extracting to disk
- Skips binary files (ELF, images, ZIP, etc.) — indexes filenames only
- Saves the SQLite index next to the archive: `system.tar.zst.db`
- Shows a live progress bar with bytes/sec and ETA

**Options:**

| Flag | Description |
|------|-------------|
| `--index <FILE>` / `-i` | Save the index to a custom path instead of the default |

```bash
# Custom index location
backupsage index /backups/system.tar.zst --index /tmp/system.db
```

---

### `search` — Find files containing a keyword

```bash
backupsage search "password"
```

Prints a table of matching file paths and per-file match counts, ordered by relevance (BM25).

**FTS5 query syntax:**

| Query | Meaning |
|-------|---------|
| `password` | Files containing the word "password" |
| `"error 404"` | Files containing the exact phrase |
| `config*` | Files with words starting with "config" |
| `error AND auth` | Files with both words |
| `error NOT debug` | Files with "error" but not "debug" |

**Options:**

| Flag | Description |
|------|-------------|
| `--archive <ARCHIVE>` / `-a` | Path to the original archive (used to auto-locate its `.db`) |
| `--index <FILE>` / `-i` | Explicit path to the index database |

```bash
# Auto-discover index next to the archive
backupsage search "TODO" --archive /backups/system.tar.zst

# Explicit index path
backupsage search "secret" --index /tmp/system.db
```

---

### `top` — Show the most frequent words

```bash
backupsage top
```

Prints a table of the 50 most common words with:
- **Occurrences** — total hits across all files
- **% of Top** — share of the top-N total
- **In Files** — how many distinct files contain the word
- **Bar** — mini visual chart

```bash
# Show top 100 words
backupsage top --limit 100

# For a specific archive's index
backupsage top --archive /backups/system.tar.zst
```

---

## Index Discovery (no flags needed)

BackupSage tries to find the index automatically so you don't have to type `--index` every time:

1. `--index <path>` if explicitly given
2. `<archive>.db` next to the archive if `--archive` is given
3. `./backupsage.db` in the current directory
4. Any `*.db` in the current directory (with a hint message)

---

## Example Session

```bash
# Index a 50 GB backup (takes a few minutes)
backupsage index /mnt/backups/prod-2026-04-08.tar.zst

# ⠸ [00:04:21] [====================>  ] 47.2 GB/50.1 GB (189 MB/s, eta 18s) — etc/nginx/nginx.conf
# Indexed  : 284,391 text files
# Skipped  : 12,847 binary files
# Database : /mnt/backups/prod-2026-04-08.tar.zst.db

# Search for a keyword
backupsage search "database_password" --archive /mnt/backups/prod-2026-04-08.tar.zst

# ┌───┬─────────┬──────────────────────────────────────────────┐
# │ # │ Matches │ File Path                                    │
# ├───┼─────────┼──────────────────────────────────────────────┤
# │ 1 │      3  │ etc/app/config.yml                           │
# │ 2 │      1  │ home/deploy/.env                             │
# │ 3 │      1  │ srv/app/settings/production.py               │
# └───┴─────────┴──────────────────────────────────────────────┘

# Top words
backupsage top --archive /mnt/backups/prod-2026-04-08.tar.zst
```

---

## Performance Notes

- **Memory**: Flat ~20–40 MB regardless of archive size (fully streaming)
- **Speed**: Indexing is I/O bound. Expect ~150–300 MB/s on an SSD
- **Database size**: Typically 2–5% of uncompressed text content size
- **Search latency**: Sub-millisecond for most queries (SQLite FTS5 inverted index)

---

## Architecture

```
File → BufReader → ProgressBar wrapper → zstd::Decoder → tar::Archive → entries
                                                                            │
                                                                  ┌─────────┴───────────┐
                                                                  │  Binary? skip content │
                                                                  │  Text?   tokenise     │
                                                                  └─────────┬───────────┘
                                                                            │
                                                                    SQLite FTS5 + word_freq
```

- **`src/cli.rs`** — clap derive CLI definitions
- **`src/indexer.rs`** — streaming pipeline, binary detection, SQLite schema + inserts
- **`src/searcher.rs`** — FTS5 MATCH queries, `top` word frequency table
- **`src/main.rs`** — entry point, arg dispatch

---

## License

MIT — Thomas Lane
