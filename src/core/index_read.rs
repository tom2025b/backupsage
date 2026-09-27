//! The locked read-only index loader every command shares (#93, #102).
//!
//! **Nothing here writes beside an index, and nothing mixed is presented as
//! coherent.** A [`LockedIndex`] opens a `mode=ro` SQLite connection and
//! holds one read transaction until [`LockedIndex::finish`] (or drop). The
//! transaction's shared lock stops any SQLite writer from committing while
//! the caller reads, so every statement sees one snapshot. Every layout
//! SQLite could only read by writing beside the index, or by trusting a name
//! that hides its journal, is refused before opening, with a named reason:
//! - `pending_journal`: a `-wal` or `-journal` sidecar, under the given or
//!   the resolved name;
//! - `wal_mode_index`: a WAL-mode header, since reading one creates
//!   `-shm`/`-wal`;
//! - `index_multiply_linked`: more than one hard link, since a journal
//!   beside another name would be invisible from this one.
//!
//! `finish` also compares a before/after stamp of the file (inode, links,
//! size, mtime, ctime) and fails closed on an in-place writer that ignores
//! SQLite locking. ADR 0010 records the measurements behind each rule.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read};
use std::ops::Deref;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use serde::Serialize;

/// Why an input is in the state it is in, or a caveat about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteCode {
    /// Unavailable: nothing exists at the path.
    IndexMissing,
    /// Unavailable: the path exists but could not be opened or read.
    IndexUnreadable,
    /// Unavailable: readable, but not a BackupSage index.
    NotABackupsageIndex,
    /// Unavailable: a `-wal` or `-journal` sidecar holds changes that only a
    /// recovering (writing) open could apply.
    PendingJournal,
    /// Unavailable: the index is in WAL mode. Reading it would create
    /// `-shm`/`-wal` files beside it, which diff never does.
    WalModeIndex,
    /// Unavailable: the file has more than one hard link, so a journal or
    /// WAL beside another of its names would be invisible from this one.
    IndexMultiplyLinked,
    /// Unavailable: a writer held the index longer than diff waits.
    IndexBusy,
    /// Unavailable: the file changed while it was read, so the rows may mix
    /// two states.
    IndexChangedDuringRead,
    /// Incomplete: indexing stopped before it finished.
    IndexIncomplete,
    /// Incompatible: not a v3 index.
    UnsupportedSchema,
    /// Incompatible: content hashes are not BLAKE3.
    UnsupportedHashAlgo,
    /// Incompatible: no index identity recorded.
    MissingIndexUuid,
    /// Content was never read, so no row carries a content hash.
    MetadataOnlyIndex,
    /// The index records no source path to check.
    SourceNotRecorded,
    /// The recorded source path is relative to an unknown directory.
    SourcePathRelative,
    /// The recorded source path is a lossy rendering of non-UTF-8 bytes.
    SourcePathLossy,
    /// The index records no source size/mtime to compare with.
    SourceStatNotRecorded,
    SourceOffline,
    SourceDenied,
    /// Size or mtime differ from what the index recorded.
    SourceStale,
    /// Size and mtime match; the source bytes were not re-hashed.
    SourceStatMatches,
    /// A directory's own stat says nothing about the files inside it.
    SourceDirectoryUnverified,
    /// Some rows come from an index older than v1.0.1 that stored only a
    /// lossy rendering of their name or link target (#105). They carry a
    /// lossy flag and are never treated as exact.
    LegacyLossyPaths,
    /// Unavailable: an indexer older than #63 wrote sparse rows with the
    /// condensed stream's hash and size, and never recorded their real names
    /// (#105). The archive must be re-indexed.
    LegacySparseIndex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InputNote {
    pub code: NoteCode,
    pub detail: String,
}

impl NoteCode {
    /// The snake_case name used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            NoteCode::IndexMissing => "index_missing",
            NoteCode::IndexUnreadable => "index_unreadable",
            NoteCode::NotABackupsageIndex => "not_a_backupsage_index",
            NoteCode::PendingJournal => "pending_journal",
            NoteCode::WalModeIndex => "wal_mode_index",
            NoteCode::IndexMultiplyLinked => "index_multiply_linked",
            NoteCode::IndexBusy => "index_busy",
            NoteCode::IndexChangedDuringRead => "index_changed_during_read",
            NoteCode::IndexIncomplete => "index_incomplete",
            NoteCode::UnsupportedSchema => "unsupported_schema",
            NoteCode::UnsupportedHashAlgo => "unsupported_hash_algo",
            NoteCode::MissingIndexUuid => "missing_index_uuid",
            NoteCode::MetadataOnlyIndex => "metadata_only_index",
            NoteCode::SourceNotRecorded => "source_not_recorded",
            NoteCode::SourcePathRelative => "source_path_relative",
            NoteCode::SourcePathLossy => "source_path_lossy",
            NoteCode::SourceStatNotRecorded => "source_stat_not_recorded",
            NoteCode::SourceOffline => "source_offline",
            NoteCode::SourceDenied => "source_denied",
            NoteCode::SourceStale => "source_stale",
            NoteCode::SourceStatMatches => "source_stat_matches",
            NoteCode::SourceDirectoryUnverified => "source_directory_unverified",
            NoteCode::LegacyLossyPaths => "legacy_lossy_paths",
            NoteCode::LegacySparseIndex => "legacy_sparse_index",
        }
    }
}

impl fmt::Display for InputNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for InputNote {}

/// What must not change between checking an index and finishing its read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileStamp {
    dev: u64,
    ino: u64,
    nlink: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileStamp {
    pub(crate) fn of(md: &fs::Metadata) -> Self {
        Self {
            dev: md.dev(),
            ino: md.ino(),
            nlink: md.nlink(),
            size: md.size(),
            mtime: (md.mtime(), md.mtime_nsec()),
            ctime: (md.ctime(), md.ctime_nsec()),
        }
    }
}

/// How long a read waits for a committing writer before giving up.
const BUSY_WAIT: Duration = Duration::from_secs(2);

/// Every guard that must hold before SQLite may open the file (see the
/// module docs). Returns the stamp to compare after reading.
///
/// The header is read with a plain descriptor that is then closed. POSIX
/// record locks belong to the process, and closing any descriptor on a file
/// drops all of them, so a *different* SQLite connection in this process
/// that holds a lock on the same file loses it here (SQLite's own
/// connections share descriptors and are unaffected). No command keeps such
/// a connection in use across this point; `master sync`'s outer identity
/// handle is not read again after `add` re-reads the file.
pub(crate) fn check_index_file(db_path: &Path) -> std::result::Result<FileStamp, InputNote> {
    let md = match fs::metadata(db_path) {
        Ok(md) if md.is_file() => md,
        Ok(_) => return Err(note(NoteCode::IndexUnreadable, "not a regular file")),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(note(NoteCode::IndexMissing, "no index file at this path"));
        }
        Err(e) => return Err(note(NoteCode::IndexUnreadable, e.to_string())),
    };
    if md.nlink() > 1 {
        return Err(note(
            NoteCode::IndexMultiplyLinked,
            format!(
                "the file has {} hard links; a journal beside another name would be \
                 invisible from this one (copy the index to diff it)",
                md.nlink()
            ),
        ));
    }
    let sidecars = pending_sidecars(db_path);
    if !sidecars.is_empty() {
        return Err(pending(&sidecars));
    }
    let mut header = [0u8; 100];
    fs::File::open(db_path)
        .and_then(|mut f| f.read_exact(&mut header))
        .map_err(|_| note(NoteCode::NotABackupsageIndex, "not an SQLite database"))?;
    if &header[..16] != b"SQLite format 3\0" {
        return Err(note(
            NoteCode::NotABackupsageIndex,
            "not an SQLite database",
        ));
    }
    // Header bytes 18 and 19 are the write and read format versions:
    // 1 is rollback journal, 2 is WAL.
    if header[18] == 2 || header[19] == 2 {
        return Err(note(
            NoteCode::WalModeIndex,
            "the index is in WAL mode; reading it would create -shm/-wal files \
             beside it (BackupSage writes rollback-journal indexes)",
        ));
    }
    Ok(FileStamp::of(&md))
}

fn open_checked(db_path: &Path) -> std::result::Result<Connection, InputNote> {
    let absolute =
        std::path::absolute(db_path).map_err(|e| note(NoteCode::IndexUnreadable, e.to_string()))?;
    let mut uri = b"file:".to_vec();
    for &byte in absolute.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte);
        } else {
            uri.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
    // readonly_shm: should the file turn WAL between the header check and
    // the open, SQLite must still never create a -shm beside it.
    uri.extend_from_slice(b"?mode=ro&readonly_shm=1");
    let conn = Connection::open_with_flags(
        PathBuf::from(OsString::from_vec(uri)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| sqlite_note(&e))?;
    conn.busy_timeout(BUSY_WAIT).map_err(|e| sqlite_note(&e))?;
    Ok(conn)
}

/// Open an index strictly read-only, after every guard in the module docs.
/// Read it inside one transaction, as [`LockedIndex`] does.
pub fn open_index_readonly(db_path: &Path) -> std::result::Result<Connection, InputNote> {
    check_index_file(db_path)?;
    open_checked(db_path)
}

pub(crate) fn sqlite_note(e: &rusqlite::Error) -> InputNote {
    match e.sqlite_error_code() {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => note(
            NoteCode::IndexBusy,
            format!(
                "a writer held the index for more than {}s",
                BUSY_WAIT.as_secs()
            ),
        ),
        _ => note(NoteCode::IndexUnreadable, e.to_string()),
    }
}

pub(crate) fn read_note(e: &anyhow::Error) -> InputNote {
    match e.downcast_ref::<rusqlite::Error>() {
        Some(sqlite) => sqlite_note(sqlite),
        None => note(NoteCode::IndexUnreadable, format!("{e:#}")),
    }
}

pub(crate) fn pending(sidecars: &[String]) -> InputNote {
    note(
        NoteCode::PendingJournal,
        format!(
            "{} beside the index: it is being written or was interrupted, \
             and reading it would require recovery that modifies it",
            sidecars.join(" and ")
        ),
    )
}

/// Where a test hook may run during a read.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadPoint {
    /// Right after [`LockedIndex::open`] has taken the read lock, before the
    /// caller's first statement.
    AfterOpen,
    /// Master registration: after `read_identity`'s statements, while the
    /// handle it returns keeps the lock for the rows its caller replicates.
    HeldByCaller,
    /// Master registration: the first thing replication does, before any
    /// further statement on the handle `read_identity` returned.
    BeforeReplication,
    /// Master registration: rows are about to be copied through the handle,
    /// inside the catalog transaction.
    DuringReplication,
    /// `diff`: after the metadata statements, before the rows statement.
    BetweenStatements,
    /// `diff`: after the first row, while the rows statement is still stepping.
    BetweenRows,
}

type MidReadHook = (ReadPoint, Box<dyn FnOnce()>);

thread_local! {
    static MID_READ_HOOK: RefCell<Option<MidReadHook>> = const { RefCell::new(None) };
}

/// Test support: run `hook` once, on this thread, at `point` of the next
/// index read, so a test can act as a writer in the middle of a read.
#[doc(hidden)]
pub fn set_mid_read_hook(point: ReadPoint, hook: impl FnOnce() + 'static) {
    MID_READ_HOOK.with(|slot| *slot.borrow_mut() = Some((point, Box::new(hook))));
}

pub(crate) fn run_mid_read_hook(point: ReadPoint) {
    let hook = MID_READ_HOOK.with(|slot| {
        let mut slot = slot.borrow_mut();
        match slot.take() {
            Some((at, hook)) if at == point => Some(hook),
            other => {
                *slot = other;
                None
            }
        }
    });
    if let Some(hook) = hook {
        hook();
    }
}

/// Sidecars whose presence means the main file alone is not the database.
/// SQLite names them after the resolved file, so a symlinked index path is
/// checked under both spellings.
pub(crate) fn pending_sidecars(db_path: &Path) -> Vec<String> {
    let mut spellings = vec![db_path.to_path_buf()];
    spellings.extend(fs::canonicalize(db_path).ok());
    ["-wal", "-journal"]
        .into_iter()
        .filter(|suffix| {
            spellings.iter().any(|path| {
                let mut name = path.as_os_str().to_owned();
                name.push(suffix);
                fs::symlink_metadata(PathBuf::from(name)).is_ok()
            })
        })
        .map(str::to_owned)
        .collect()
}

pub(crate) fn note(code: NoteCode, detail: impl Into<String>) -> InputNote {
    InputNote {
        code,
        detail: detail.into(),
    }
}

/// Test support in debug builds only: with this variable set, every locked
/// open rewrites the index in place with its own bytes right after taking
/// the lock. The content is unchanged but the file stamp is not, so tests
/// can drive a change-during-read through a real command, which the
/// in-process hooks cannot reach. Release builds contain no such path.
#[cfg(debug_assertions)]
pub const TEST_TOUCH_AFTER_OPEN: &str = "BACKUPSAGE_TEST_TOUCH_INDEX_AFTER_OPEN";

#[cfg(debug_assertions)]
fn touch_for_tests(db_path: &Path) {
    if std::env::var_os(TEST_TOUCH_AFTER_OPEN).is_none() {
        return;
    }
    if let Ok(bytes) = fs::read(db_path) {
        if let Ok(mut file) = fs::OpenOptions::new().write(true).open(db_path) {
            let _ = std::io::Write::write_all(&mut file, &bytes);
        }
    }
}

/// One index, open read-only inside one read transaction (see the module
/// docs). Dereferences to the connection for the caller's statements.
pub struct LockedIndex {
    conn: Connection,
    before: FileStamp,
    path: PathBuf,
}

impl LockedIndex {
    /// Check every guard, open, and take the read lock. Fails closed with a
    /// named reason; nothing is created or changed beside the file.
    pub fn open(db_path: &Path) -> std::result::Result<Self, InputNote> {
        let before = check_index_file(db_path)?;
        let conn = open_checked(db_path)?;
        // The first read after BEGIN takes the shared lock, held until
        // finish or drop.
        conn.execute_batch("BEGIN").map_err(|e| sqlite_note(&e))?;
        let has_fts = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE name = 'files_fts'",
                [],
                |_| Ok(()),
            )
            .optional()
            .map_err(|e| sqlite_note(&e))?;
        if has_fts.is_none() {
            return Err(note(NoteCode::NotABackupsageIndex, "no files_fts table"));
        }
        run_mid_read_hook(ReadPoint::AfterOpen);
        #[cfg(debug_assertions)]
        touch_for_tests(db_path);
        Ok(Self {
            conn,
            before,
            path: db_path.to_path_buf(),
        })
    }

    /// End the read and prove it was coherent: no sidecar appeared and the
    /// file is exactly as it was when it was checked. Callers present
    /// results only after this succeeds.
    pub fn finish(self) -> std::result::Result<(), InputNote> {
        let Self { conn, before, path } = self;
        conn.execute_batch("COMMIT").map_err(|e| sqlite_note(&e))?;
        drop(conn);
        let sidecars = pending_sidecars(&path);
        if !sidecars.is_empty() {
            return Err(pending(&sidecars));
        }
        if fs::metadata(&path).ok().map(|md| FileStamp::of(&md)) != Some(before) {
            return Err(note(
                NoteCode::IndexChangedDuringRead,
                "the index file changed while it was read (inode, links, size, mtime or \
                 ctime), so its rows may mix two states",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for LockedIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockedIndex")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Deref for LockedIndex {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}
