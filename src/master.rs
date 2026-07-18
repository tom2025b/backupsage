//! Master catalog: a replicated-metadata index over many per-source
//! databases. Holds no text and no FTS — dedup and metadata queries run
//! entirely here (all sources may be offline); full-text search fans out to
//! the per-source DBs listed in the registry.
//!
//! The master lives long and may be touched by concurrent sessions, so its
//! connection always runs WAL with a busy timeout, and it is never folded
//! back to DELETE journal mode.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags};

use crate::searcher;
use crate::store;

/// Registry row statuses (spec §6).
pub const STATUS_OK: &str = "ok";
pub const STATUS_STALE_REPLICA: &str = "stale-replica";
pub const STATUS_STALE_INDEX: &str = "stale-index";
pub const STATUS_DB_MISSING: &str = "db-missing";
pub const STATUS_ARCHIVE_MISSING: &str = "archive-missing";
pub const STATUS_INCOMPLETE: &str = "incomplete";
pub const STATUS_V2_LIMITED: &str = "v2-limited";

#[derive(Debug, Clone)]
pub struct ArchiveRow {
    pub archive_id: i64,
    pub index_uuid: String,
    pub db_path: String,
    pub source_path: String,
    pub source_type: String,
    pub label: String,
    pub schema_version: i64,
    pub files_count: i64,
    pub completed: bool,
    pub indexed_unix: Option<i64>,
    pub archive_blake3: Option<String>,
    pub status: String,
}

#[derive(Debug)]
pub enum AddOutcome {
    /// v3 index registered (or refreshed) and its metadata replicated.
    Replicated { label: String, files: u64 },
    /// v2 index registered search-only; contributes no dedup rows.
    V2Limited { label: String },
}

pub struct Master {
    pub conn: Connection,
}

/// `$BACKUPSAGE_MASTER` > `--master` handling happens in the CLI; this is
/// the filesystem default: `$XDG_DATA_HOME/backupsage/master.db`.
pub fn default_master_path() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local/share")
        });
    base.join("backupsage/master.db")
}

pub fn open_at(path: &Path) -> Result<Master> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create master directory {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("cannot open master catalog at {}", path.display()))?;
    init_master_conn(&conn)?;
    Ok(Master { conn })
}

/// Throwaway master for ad-hoc `dedup --db a.db --db b.db` — same schema,
/// same replication code, nothing persisted.
pub fn open_in_memory(db_paths: &[PathBuf]) -> Result<(Master, Vec<(String, String)>)> {
    let conn = Connection::open_in_memory()?;
    init_master_conn(&conn)?;
    let mut master = Master { conn };
    let mut skipped = Vec::new();
    for p in db_paths {
        match master.add(p) {
            Ok(AddOutcome::Replicated { .. }) => {}
            Ok(AddOutcome::V2Limited { label }) => {
                skipped.push((label, STATUS_V2_LIMITED.to_string()));
            }
            Err(e) => bail!("cannot load '{}': {e:#}", p.display()),
        }
    }
    Ok((master, skipped))
}

fn init_master_conn(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS archives (
            archive_id     INTEGER PRIMARY KEY,
            index_uuid     TEXT UNIQUE NOT NULL,
            db_path        TEXT NOT NULL,
            source_path    TEXT NOT NULL,
            source_type    TEXT NOT NULL,
            label          TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            files_count    INTEGER NOT NULL DEFAULT 0,
            completed      INTEGER NOT NULL DEFAULT 0,
            indexed_unix   INTEGER,
            archive_size   INTEGER,
            archive_mtime_unix INTEGER,
            archive_blake3 TEXT,
            db_size        INTEGER,
            db_mtime_unix  INTEGER,
            phash_algo     TEXT,
            status         TEXT NOT NULL DEFAULT 'ok',
            added_unix     INTEGER NOT NULL,
            synced_unix    INTEGER
        );
        CREATE TABLE IF NOT EXISTS files (
            archive_id   INTEGER NOT NULL REFERENCES archives(archive_id) ON DELETE CASCADE,
            file_id      INTEGER NOT NULL,
            path         TEXT NOT NULL,
            entry_type   TEXT NOT NULL,
            kind         TEXT NOT NULL,
            size         INTEGER,
            mtime_unix   INTEGER,
            exif_unix    INTEGER,
            exif_src     TEXT,
            content_hash BLOB,
            phash        INTEGER,
            img_w        INTEGER,
            img_h        INTEGER,
            flags        INTEGER NOT NULL DEFAULT 0,
            pb0 INTEGER GENERATED ALWAYS AS ((phash >> 48) & 0xFFFF) VIRTUAL,
            pb1 INTEGER GENERATED ALWAYS AS ((phash >> 32) & 0xFFFF) VIRTUAL,
            pb2 INTEGER GENERATED ALWAYS AS ((phash >> 16) & 0xFFFF) VIRTUAL,
            pb3 INTEGER GENERATED ALWAYS AS ( phash        & 0xFFFF) VIRTUAL,
            PRIMARY KEY (archive_id, file_id)
        );
        CREATE INDEX IF NOT EXISTS m_hash ON files(content_hash) WHERE content_hash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS m_pb0 ON files(pb0) WHERE phash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS m_pb1 ON files(pb1) WHERE phash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS m_pb2 ON files(pb2) WHERE phash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS m_pb3 ON files(pb3) WHERE phash IS NOT NULL;
        CREATE INDEX IF NOT EXISTS m_size ON files(size);
        CREATE INDEX IF NOT EXISTS m_path ON files(path);",
    )?;
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open a per-source index read-only and pull the identity fields out of it.
struct SourceIdentity {
    index_uuid: String,
    schema_version: i64,
    source_path: String,
    source_type: String,
    completed: bool,
    indexed_unix: Option<i64>,
    archive_size: Option<i64>,
    archive_mtime_unix: Option<i64>,
    archive_blake3: Option<String>,
    phash_algo: Option<String>,
}

fn read_identity(db_path: &Path) -> Result<(Connection, SourceIdentity)> {
    let conn = searcher::open_index(db_path)?;
    let version = store::schema_version(&conn).unwrap_or(1);
    if version < 2 {
        bail!(
            "'{}' is a v0.1 index with no metadata at all — re-index the archive first",
            db_path.display()
        );
    }
    let source_path = searcher::get_meta(&conn, "source")
        .or_else(|| searcher::get_meta(&conn, "archive"))
        .unwrap_or_default();
    let created = searcher::get_meta(&conn, "created_unix");
    let index_uuid = match searcher::get_meta(&conn, "index_uuid") {
        Some(u) => u,
        // v2 has no uuid: synthesise one that is stable across .db moves by
        // keying on the *archive* path stored in meta plus the build time.
        None => {
            let seed = format!("{}|{}", source_path, created.as_deref().unwrap_or(""));
            format!("v2:{}", blake3::hash(seed.as_bytes()).to_hex())
        }
    };
    let id = SourceIdentity {
        index_uuid,
        schema_version: version,
        source_path,
        source_type: searcher::get_meta(&conn, "source_type").unwrap_or_else(|| "tar".into()),
        completed: searcher::get_meta(&conn, "completed").as_deref() == Some("1"),
        indexed_unix: created.and_then(|v| v.parse().ok()),
        archive_size: searcher::get_meta(&conn, "archive_size").and_then(|v| v.parse().ok()),
        archive_mtime_unix: searcher::get_meta(&conn, "archive_mtime_unix")
            .and_then(|v| v.parse().ok()),
        archive_blake3: searcher::get_meta(&conn, "archive_blake3"),
        phash_algo: searcher::get_meta(&conn, "phash_algo"),
    };
    Ok((conn, id))
}

/// Accepts either a `.db` index or the source itself (resolves `<source>.db`).
fn resolve_db_arg(arg: &Path) -> Result<PathBuf> {
    if arg.is_file() {
        if let Ok(conn) =
            Connection::open_with_flags(arg, OpenFlags::SQLITE_OPEN_READ_ONLY)
        {
            let is_index: bool = conn
                .query_row("SELECT 1 FROM sqlite_master WHERE name='files_fts'", [], |_| Ok(()))
                .is_ok();
            if is_index {
                return Ok(arg.to_path_buf());
            }
        }
    }
    let sibling = crate::indexer::resolve_db_path(arg, None);
    if sibling.exists() {
        return Ok(sibling);
    }
    bail!(
        "no index found for '{}' — expected '{}'; run `backupsage index` first",
        arg.display(),
        sibling.display()
    )
}

impl Master {
    /// Register (or refresh) a per-source index and replicate its metadata.
    pub fn add(&mut self, db_or_source: &Path) -> Result<AddOutcome> {
        let db_path = resolve_db_arg(db_or_source)?;
        let db_abs = db_path
            .canonicalize()
            .unwrap_or_else(|_| db_path.clone())
            .display()
            .to_string();
        let (_src_conn, id) = read_identity(&db_path)?;
        let label = Path::new(&id.source_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| db_abs.clone());
        let (db_size, db_mtime) = stat_file(&db_path);

        // Identity resolution: same uuid = moved/unchanged index; same
        // db_path = rebuilt index (new uuid). Either way update in place.
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT archive_id FROM archives WHERE index_uuid=?1 OR db_path=?2",
                params![id.index_uuid, db_abs],
                |r| r.get(0),
            )
            .ok();

        let status = if id.schema_version == 2 {
            STATUS_V2_LIMITED
        } else if !id.completed {
            STATUS_INCOMPLETE
        } else {
            STATUS_OK
        };

        let archive_id = match existing {
            Some(aid) => {
                self.conn.execute(
                    "UPDATE archives SET index_uuid=?1, db_path=?2, source_path=?3,
                        source_type=?4, label=?5, schema_version=?6, completed=?7,
                        indexed_unix=?8, archive_size=?9, archive_mtime_unix=?10,
                        archive_blake3=?11, db_size=?12, db_mtime_unix=?13,
                        phash_algo=?14, status=?15, synced_unix=?16
                     WHERE archive_id=?17",
                    params![
                        id.index_uuid, db_abs, id.source_path, id.source_type, label,
                        id.schema_version, id.completed as i64, id.indexed_unix,
                        id.archive_size, id.archive_mtime_unix, id.archive_blake3,
                        db_size, db_mtime, id.phash_algo, status, now_unix(), aid
                    ],
                )?;
                aid
            }
            None => {
                self.conn.execute(
                    "INSERT INTO archives (index_uuid, db_path, source_path, source_type,
                        label, schema_version, completed, indexed_unix, archive_size,
                        archive_mtime_unix, archive_blake3, db_size, db_mtime_unix,
                        phash_algo, status, added_unix, synced_unix)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)",
                    params![
                        id.index_uuid, db_abs, id.source_path, id.source_type, label,
                        id.schema_version, id.completed as i64, id.indexed_unix,
                        id.archive_size, id.archive_mtime_unix, id.archive_blake3,
                        db_size, db_mtime, id.phash_algo, status, now_unix()
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };

        if id.schema_version == 2 {
            // v2 has no hashes: participates in federated search only.
            self.conn
                .execute("DELETE FROM files WHERE archive_id=?1", [archive_id])?;
            self.conn
                .execute("UPDATE archives SET files_count=0 WHERE archive_id=?1", [archive_id])?;
            return Ok(AddOutcome::V2Limited { label });
        }

        let files = self.replicate(archive_id, &db_abs)?;
        Ok(AddOutcome::Replicated { label, files })
    }

    /// One-ATTACH-at-a-time metadata replication (never near the limit).
    fn replicate(&mut self, archive_id: i64, db_path: &str) -> Result<u64> {
        self.conn
            .execute(
                "ATTACH DATABASE ?1 AS src",
                [format!("file:{db_path}?mode=ro")],
            )
            .with_context(|| format!("cannot attach '{db_path}'"))?;
        let result = (|| -> Result<u64> {
            let tx_result: Result<u64> = (|| {
                self.conn.execute_batch("BEGIN")?;
                self.conn
                    .execute("DELETE FROM files WHERE archive_id=?1", [archive_id])?;
                let n = self.conn.execute(
                    "INSERT INTO files (archive_id, file_id, path, entry_type, kind, size,
                        mtime_unix, exif_unix, exif_src, content_hash, phash, img_w, img_h, flags)
                     SELECT ?1, id, path, entry_type, kind, size, mtime_unix, exif_unix,
                        exif_src, content_hash, phash, img_w, img_h, flags
                     FROM src.files",
                    [archive_id],
                )? as u64;
                self.conn.execute(
                    "UPDATE archives SET files_count=?1, synced_unix=?2 WHERE archive_id=?3",
                    params![n as i64, now_unix(), archive_id],
                )?;
                self.conn.execute_batch("COMMIT")?;
                Ok(n)
            })();
            if tx_result.is_err() {
                let _ = self.conn.execute_batch("ROLLBACK");
            }
            tx_result
        })();
        let _ = self.conn.execute_batch("DETACH DATABASE src");
        result
    }

    pub fn list(&self) -> Result<Vec<ArchiveRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT archive_id, index_uuid, db_path, source_path, source_type, label,
                    schema_version, files_count, completed, indexed_unix, archive_blake3, status
             FROM archives ORDER BY archive_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ArchiveRow {
                    archive_id: r.get(0)?,
                    index_uuid: r.get(1)?,
                    db_path: r.get(2)?,
                    source_path: r.get(3)?,
                    source_type: r.get(4)?,
                    label: r.get(5)?,
                    schema_version: r.get(6)?,
                    files_count: r.get(7)?,
                    completed: r.get::<_, i64>(8)? == 1,
                    indexed_unix: r.get(9)?,
                    archive_blake3: r.get(10)?,
                    status: r.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Replica-vs-index staleness: refresh replicas whose index was rebuilt,
    /// flag missing DBs. Returns (label, action) pairs for reporting.
    pub fn sync(&mut self, prune_days: Option<u32>) -> Result<Vec<(String, String)>> {
        let mut actions = Vec::new();
        for row in self.list()? {
            let db_path = PathBuf::from(&row.db_path);
            if !db_path.exists() {
                if row.status != STATUS_DB_MISSING {
                    self.set_status(row.archive_id, STATUS_DB_MISSING)?;
                    actions.push((row.label.clone(), "db-missing (rows retained)".into()));
                }
                if let Some(days) = prune_days {
                    let cutoff = now_unix() - days as i64 * 86_400;
                    let synced: Option<i64> = self
                        .conn
                        .query_row(
                            "SELECT synced_unix FROM archives WHERE archive_id=?1",
                            [row.archive_id],
                            |r| r.get(0),
                        )
                        .ok()
                        .flatten();
                    if synced.map(|s| s < cutoff).unwrap_or(true) {
                        self.rm(&row.archive_id.to_string())?;
                        actions.push((row.label.clone(), "pruned".into()));
                    }
                }
                continue;
            }
            let (_conn, id) = match read_identity(&db_path) {
                Ok(v) => v,
                Err(e) => {
                    self.set_status(row.archive_id, STATUS_DB_MISSING)?;
                    actions.push((row.label.clone(), format!("unreadable: {e:#}")));
                    continue;
                }
            };
            if id.index_uuid != row.index_uuid || row.status == STATUS_DB_MISSING {
                // Rebuilt (or back online): re-register through add().
                self.add(&db_path)?;
                actions.push((row.label.clone(), "re-replicated".into()));
            } else {
                let status = if id.schema_version == 2 {
                    STATUS_V2_LIMITED
                } else if !id.completed {
                    STATUS_INCOMPLETE
                } else {
                    STATUS_OK
                };
                let (db_size, db_mtime) = stat_file(&db_path);
                self.conn.execute(
                    "UPDATE archives SET status=?1, db_size=?2, db_mtime_unix=?3,
                            synced_unix=?4 WHERE archive_id=?5",
                    params![status, db_size, db_mtime, now_unix(), row.archive_id],
                )?;
            }
        }
        Ok(actions)
    }

    /// Index-vs-archive staleness: does the index still describe the source?
    /// Cheap stat compare; `deep` re-hashes tar sources.
    pub fn verify(&mut self, deep: bool) -> Result<Vec<ArchiveRow>> {
        for row in self.list()? {
            if row.status == STATUS_DB_MISSING || row.status == STATUS_V2_LIMITED {
                continue; // nothing to verify against / no fingerprint
            }
            let source = PathBuf::from(&row.source_path);
            if !source.exists() {
                self.set_status(row.archive_id, STATUS_ARCHIVE_MISSING)?;
                continue;
            }
            if row.source_type == "dir" {
                continue; // no meaningful whole-dir fingerprint in v1.0
            }
            let (stored_size, stored_mtime): (Option<i64>, Option<i64>) = self.conn.query_row(
                "SELECT archive_size, archive_mtime_unix FROM archives WHERE archive_id=?1",
                [row.archive_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let (cur_size, cur_mtime) = stat_file(&source);
            let mut stale = stored_size.is_some() && stored_size != cur_size
                || stored_mtime.is_some() && stored_mtime != cur_mtime;
            if !stale && deep {
                if let Some(expected) = &row.archive_blake3 {
                    let actual = hash_file(&source)?;
                    stale = &actual != expected;
                }
            }
            if stale {
                self.set_status(row.archive_id, STATUS_STALE_INDEX)?;
            } else if row.status == STATUS_STALE_INDEX || row.status == STATUS_ARCHIVE_MISSING {
                self.set_status(
                    row.archive_id,
                    if row.completed { STATUS_OK } else { STATUS_INCOMPLETE },
                )?;
            }
        }
        self.list()
    }

    /// Remove a registration (and, via CASCADE, its replica rows). Never
    /// touches the per-source .db. Key: archive_id, label, or path.
    pub fn rm(&mut self, key: &str) -> Result<()> {
        let by_id: Option<i64> = key.parse().ok();
        let matches: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT archive_id FROM archives
                 WHERE archive_id=?1 OR label=?2 OR db_path=?2 OR source_path=?2",
            )?;
            let collected = stmt
                .query_map(params![by_id, key], |r| r.get(0))?
                .collect::<std::result::Result<Vec<i64>, _>>()?;
            collected
        };
        match matches.as_slice() {
            [] => bail!("no registered archive matches '{key}'"),
            [one] => {
                self.conn.execute("PRAGMA foreign_keys=ON", [])?;
                self.conn
                    .execute("DELETE FROM files WHERE archive_id=?1", [one])?;
                self.conn
                    .execute("DELETE FROM archives WHERE archive_id=?1", [one])?;
                Ok(())
            }
            many => bail!(
                "'{key}' is ambiguous — matches archive ids {many:?}; use the id"
            ),
        }
    }

    fn set_status(&self, archive_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE archives SET status=?1 WHERE archive_id=?2",
            params![status, archive_id],
        )?;
        Ok(())
    }
}

fn stat_file(path: &Path) -> (Option<i64>, Option<i64>) {
    match fs::metadata(path) {
        Ok(md) => {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64);
            (Some(md.len() as i64), mtime)
        }
        Err(_) => (None, None),
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)
        .with_context(|| format!("cannot open '{}' for hashing", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}
