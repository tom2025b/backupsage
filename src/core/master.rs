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
    pub content_mode: String,
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
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local/share")
        });
    base.join("backupsage/master.db")
}

/// SQLite `application_id` stamped into every master catalog ("BSAG").
pub const MASTER_APPLICATION_ID: u32 = 0x4253_4147;

/// What a read-only identity probe found at a `--master` path.
pub enum MasterProbe {
    /// Nothing exists there — safe to create.
    Absent,
    /// A catalog carrying the BackupSage master signature.
    SignedMaster,
    /// A master created before v1.0.1 (unsigned but master-shaped).
    LegacyMaster,
    /// A per-source BackupSage index — never a master.
    PerSourceIndex,
    /// Anything else: non-SQLite data, someone else's database, an
    /// empty pre-existing file.
    Foreign,
}

/// Read-only identity probe. Never creates the file, never writes,
/// never leaves sidecars. Alias spellings (symlink, sidecar names)
/// fail here rather than classify.
pub fn probe(path: &Path) -> Result<MasterProbe> {
    // Sidecar-name alias: "<base>-wal"/"<base>-shm" beside an existing
    // base is refused whether or not the sidecar itself exists yet.
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        for suffix in ["-wal", "-shm"] {
            if let Some(base) = name.strip_suffix(suffix) {
                if !base.is_empty() && path.with_file_name(base).exists() {
                    bail!(
                        "master path '{}' is a SQLite sidecar of '{}'; refusing",
                        path.display(),
                        base
                    );
                }
            }
        }
    }
    let md = match fs::symlink_metadata(path) {
        Err(_) => return Ok(MasterProbe::Absent),
        Ok(md) => md,
    };
    if md.file_type().is_symlink() {
        bail!(
            "master path '{}' is a symlink; refusing to open it read-write",
            path.display()
        );
    }
    if md.len() == 0 {
        return Ok(MasterProbe::Foreign); // never adopt a pre-existing empty file
    }
    let mut head = [0u8; 16];
    {
        use std::io::Read;
        let n = fs::File::open(path)
            .with_context(|| format!("cannot read '{}'", path.display()))?
            .read(&mut head)?;
        if n < 16 || &head != b"SQLite format 3\0" {
            return Ok(MasterProbe::Foreign);
        }
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open '{}' read-only", path.display()))?;
    let appid: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if appid == MASTER_APPLICATION_ID {
        return Ok(MasterProbe::SignedMaster);
    }
    let has_table = |name: &str| -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE name = ?1",
            [name],
            |_| Ok(()),
        )
        .is_ok()
    };
    if has_table("files_fts") {
        return Ok(MasterProbe::PerSourceIndex);
    }
    if appid == 0 && has_table("archives") && has_table("files") {
        // v1.0.0-created master: archives registry + master-shaped files.
        // pb0 is a VIRTUAL generated column, hidden from table_info —
        // only table_xinfo lists it.
        let master_shaped = conn
            .query_row(
                "SELECT 1 FROM pragma_table_xinfo('files') WHERE name = 'pb0'",
                [],
                |_| Ok(()),
            )
            .is_ok();
        if master_shaped {
            return Ok(MasterProbe::LegacyMaster);
        }
    }
    Ok(MasterProbe::Foreign)
}

pub fn open_at(path: &Path) -> Result<Master> {
    match probe(path)? {
        MasterProbe::Absent => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent).with_context(|| {
                    format!("cannot create master directory {}", parent.display())
                })?;
            }
            let conn = Connection::open(path)
                .with_context(|| format!("cannot create master catalog at {}", path.display()))?;
            conn.pragma_update(None, "application_id", MASTER_APPLICATION_ID)?;
            init_master_conn(&conn)?;
            Ok(Master { conn })
        }
        probe_result @ (MasterProbe::SignedMaster | MasterProbe::LegacyMaster) => {
            let conn = Connection::open(path)
                .with_context(|| format!("cannot open master catalog at {}", path.display()))?;
            // Legacy masters are adopted by stamping the signature once;
            // an already-signed master is never rewritten just to open it.
            if matches!(probe_result, MasterProbe::LegacyMaster) {
                conn.pragma_update(None, "application_id", MASTER_APPLICATION_ID)?;
            }
            init_master_conn(&conn)?;
            // Supported migration (runs only after the identity gate above
            // classified this file as a master): masters created before
            // v1.0.1 lack the raw-path column.
            if !crate::searcher::has_column(&conn, "files", "path_raw") {
                conn.execute_batch("ALTER TABLE files ADD COLUMN path_raw BLOB")?;
            }
            if !crate::searcher::has_column(&conn, "archives", "content_mode") {
                conn.execute_batch(
                    "ALTER TABLE archives ADD COLUMN content_mode TEXT NOT NULL DEFAULT 'full'",
                )?;
            }
            Ok(Master { conn })
        }
        MasterProbe::PerSourceIndex => bail!(
            "'{}' is a per-source index, not a master catalog; refusing to modify it",
            path.display()
        ),
        MasterProbe::Foreign => bail!(
            "'{}' exists but is not a BackupSage master catalog; refusing to modify it",
            path.display()
        ),
    }
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
            content_mode   TEXT NOT NULL DEFAULT 'full',
            status         TEXT NOT NULL DEFAULT 'ok',
            added_unix     INTEGER NOT NULL,
            synced_unix    INTEGER
        );
        CREATE TABLE IF NOT EXISTS files (
            archive_id   INTEGER NOT NULL REFERENCES archives(archive_id) ON DELETE CASCADE,
            file_id      INTEGER NOT NULL,
            path         TEXT NOT NULL,
            path_raw     BLOB,
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
    content_mode: crate::indexer::ContentMode,
}

/// The returned handle holds the index's read transaction: while the caller
/// keeps it, a writer cannot commit, so the rows it then replicates belong
/// to the same snapshot as this identity (#102).
fn read_identity(db_path: &Path) -> Result<(crate::index_read::LockedIndex, SourceIdentity)> {
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
        content_mode: store::content_mode(&conn),
    };
    crate::index_read::run_mid_read_hook(crate::index_read::ReadPoint::HeldByCaller);
    Ok((conn, id))
}

/// Accepts either a `.db` index or the source itself (resolves `<source>.db`).
/// An SQLite file is probed through the locked loader (#102), so nothing is
/// written beside it; only a file that is not a BackupSage index falls
/// through to the sibling lookup, and any other refusal says why.
fn resolve_db_arg(arg: &Path) -> Result<PathBuf> {
    let is_sqlite = arg.is_file() && {
        let mut head = [0u8; 16];
        fs::File::open(arg)
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
            .is_ok()
            && &head == b"SQLite format 3\0"
    };
    if is_sqlite {
        match crate::index_read::LockedIndex::open(arg) {
            Ok(_probe) => return Ok(arg.to_path_buf()),
            Err(note) if note.code == crate::index_read::NoteCode::NotABackupsageIndex => {}
            Err(note) => bail!("cannot read index '{}' safely — {note}", arg.display()),
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
    ///
    /// The catalog row and the replicated rows commit together, and only
    /// once the source read is proven coherent (#102). Identity and rows are
    /// read through one locked connection, one read transaction on one inode;
    /// the path is never reopened, so a file renamed over it mid-way cannot
    /// lend its rows to the other file's identity.
    pub fn add(&mut self, db_or_source: &Path) -> Result<AddOutcome> {
        let db_path = resolve_db_arg(db_or_source)?;
        let db_abs = db_path
            .canonicalize()
            .unwrap_or_else(|_| db_path.clone())
            .display()
            .to_string();
        let (src, id) = read_identity(&db_path)?;
        let label = Path::new(&id.source_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| db_abs.clone());
        let (db_size, db_mtime) = stat_file(&db_path);

        self.conn.execute_batch("BEGIN")?;
        let registered = self
            .register_tx(&src, &id, &db_abs, &label, db_size, db_mtime)
            .and_then(|outcome| {
                searcher::finish_index(src, &db_path)
                    .context("registration abandoned, nothing was recorded")?;
                Ok(outcome)
            });
        match registered {
            Ok(outcome) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(outcome)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// The body of [`Self::add`], inside its master transaction.
    fn register_tx(
        &mut self,
        src: &Connection,
        id: &SourceIdentity,
        db_abs: &str,
        label: &str,
        db_size: Option<i64>,
        db_mtime: Option<i64>,
    ) -> Result<AddOutcome> {
        let label = label.to_owned();
        let db_abs = db_abs.to_owned();
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
                        phash_algo=?14, content_mode=?15, status=?16, synced_unix=?17
                     WHERE archive_id=?18",
                    params![
                        id.index_uuid,
                        db_abs,
                        id.source_path,
                        id.source_type,
                        label,
                        id.schema_version,
                        id.completed as i64,
                        id.indexed_unix,
                        id.archive_size,
                        id.archive_mtime_unix,
                        id.archive_blake3,
                        db_size,
                        db_mtime,
                        id.phash_algo,
                        id.content_mode.as_str(),
                        status,
                        now_unix(),
                        aid
                    ],
                )?;
                aid
            }
            None => {
                self.conn.execute(
                    "INSERT INTO archives (index_uuid, db_path, source_path, source_type,
                        label, schema_version, completed, indexed_unix, archive_size,
                        archive_mtime_unix, archive_blake3, db_size, db_mtime_unix,
                        phash_algo, content_mode, status, added_unix, synced_unix)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?17)",
                    params![
                        id.index_uuid,
                        db_abs,
                        id.source_path,
                        id.source_type,
                        label,
                        id.schema_version,
                        id.completed as i64,
                        id.indexed_unix,
                        id.archive_size,
                        id.archive_mtime_unix,
                        id.archive_blake3,
                        db_size,
                        db_mtime,
                        id.phash_algo,
                        id.content_mode.as_str(),
                        status,
                        now_unix()
                    ],
                )?;
                self.conn.last_insert_rowid()
            }
        };

        if id.schema_version == 2 {
            // v2 has no hashes: participates in federated search only.
            self.conn
                .execute("DELETE FROM files WHERE archive_id=?1", [archive_id])?;
            self.conn.execute(
                "UPDATE archives SET files_count=0 WHERE archive_id=?1",
                [archive_id],
            )?;
            return Ok(AddOutcome::V2Limited { label });
        }

        let files = self.replicate_from(src, archive_id)?;
        Ok(AddOutcome::Replicated { label, files })
    }

    /// Copy the source's rows through its own locked connection, so they
    /// come from the snapshot the identity was read from. Values pass through
    /// untouched, as `INSERT … SELECT` did.
    fn replicate_from(&mut self, src: &Connection, archive_id: i64) -> Result<u64> {
        // Older per-source indexes have no path_raw column; replicate NULL.
        let raw_col = if searcher::has_column(src, "files", "path_raw") {
            "path_raw"
        } else {
            "NULL"
        };
        self.conn
            .execute("DELETE FROM files WHERE archive_id=?1", [archive_id])?;
        crate::index_read::run_mid_read_hook(crate::index_read::ReadPoint::DuringReplication);
        let mut read = src.prepare(&format!(
            "SELECT id, path, {raw_col}, entry_type, kind, size, mtime_unix, exif_unix,
                    exif_src, content_hash, phash, img_w, img_h, flags
             FROM files"
        ))?;
        let mut insert = self.conn.prepare(
            "INSERT INTO files (archive_id, file_id, path, path_raw, entry_type, kind,
                size, mtime_unix, exif_unix, exif_src, content_hash, phash, img_w,
                img_h, flags)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        )?;
        let mut rows = read.query([])?;
        let mut n = 0u64;
        while let Some(row) = rows.next()? {
            let mut values = vec![rusqlite::types::Value::Integer(archive_id)];
            for i in 0..14 {
                values.push(row.get::<_, rusqlite::types::Value>(i)?);
            }
            insert.execute(rusqlite::params_from_iter(values))?;
            n += 1;
        }
        drop(insert);
        self.conn.execute(
            "UPDATE archives SET files_count=?1, synced_unix=?2 WHERE archive_id=?3",
            params![n as i64, now_unix(), archive_id],
        )?;
        Ok(n)
    }

    pub fn list(&self) -> Result<Vec<ArchiveRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT archive_id, index_uuid, db_path, source_path, source_type, label,
                    schema_version, files_count, completed, indexed_unix, archive_blake3, status,
                    content_mode
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
                    content_mode: r.get(12)?,
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
                    if row.completed {
                        STATUS_OK
                    } else {
                        STATUS_INCOMPLETE
                    },
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
            many => bail!("'{key}' is ambiguous — matches archive ids {many:?}; use the id"),
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
