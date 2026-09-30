//! Read-only coverage input (#97, child of #40): the master's registry says
//! which sources exist, and each source's own index supplies its rows.
//!
//! **Nothing here writes, and nothing unreadable becomes absence.**
//!
//! - The master is read only while idle: a `-wal`, `-shm` or `-journal`
//!   beside it (another BackupSage command using it, or changes not yet
//!   folded into the file) refuses the load. An idle master is opened
//!   `mode=ro&immutable=1`, which reads the file alone and creates nothing
//!   beside it. The same guards run again after the read, and any change to
//!   the file (inode, links, size, mtime, ctime) or any new sidecar fails the
//!   load. Only [`Master::list`] is called on it.
//! - Every index is read with [`diff_input::load_index`], the locked,
//!   one-transaction, fail-closed loader from #101. An index it refuses
//!   (missing, pending journal, WAL mode, multiply linked, busy, changed
//!   during the read, unreadable, incompatible) becomes an `Unavailable`
//!   source with the loader's reason, never an empty one.
//! - The master's replica rows are never used as evidence. They are copies
//!   whose freshness only the index can confirm, so a source whose index
//!   cannot be read is unavailable, whatever the master still holds.
//!
//! See ADR 0011 for how each registry state maps to evidence and trust.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};

use crate::coverage::{self, Coverage, CoverageRow, CoverageSource, EntryKind, SourceEvidence};
use crate::diff::{EntryType, SnapshotState, SourceCurrency};
use crate::diff_input::{self, InputNote, LoadedIndex};
use crate::floors::{self, FloorParams, FloorReport, SourceStatus};
use crate::master::{self, Master};

/// One registered source, as the master lists it (or as an ad-hoc `--db`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistrySource {
    pub source_id: i64,
    pub label: String,
    pub db_path: PathBuf,
    /// The master's status string; `None` for an ad-hoc index.
    pub registry_status: Option<String>,
}

/// Why the loader set a source's evidence or status as it did, beyond the
/// index loader's own notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadCode {
    /// The index holds an entry type this version does not know; the index
    /// is refused rather than guessed at.
    UnknownEntryType,
    /// The index says metadata-only but a row carries a content hash.
    ContradictoryContentMode,
    /// The master's last recorded status lowered the source's trust below
    /// what reading the index showed.
    RegistryStatus,
    /// Nothing positively showed the source present and readable now, so
    /// its rows are history only.
    SourceUnverified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadNote {
    pub code: LoadCode,
    pub detail: String,
}

/// One source ready for the coverage engine.
#[derive(Debug, Clone)]
pub struct LoadedSource {
    pub source_id: i64,
    pub label: String,
    pub db_path: PathBuf,
    pub registry_status: Option<String>,
    /// The source path and type the index recorded, verbatim.
    pub source: Option<String>,
    pub source_type: Option<String>,
    pub evidence: SourceEvidence,
    pub status: SourceStatus,
    /// What the index loader established (#101), verbatim.
    pub index_notes: Vec<InputNote>,
    pub notes: Vec<LoadNote>,
    pub rows: Vec<CoverageRow>,
    /// Each row's content kind, by file id, as the index recorded it.
    pub kinds: BTreeMap<i64, String>,
}

/// Every source, in source-id order.
#[derive(Debug, Clone)]
pub struct LoadedCoverage {
    pub sources: Vec<LoadedSource>,
}

impl LoadedCoverage {
    /// Group the loaded rows with the #95 engine.
    pub fn coverage(&self) -> Result<Coverage> {
        let sources: Vec<CoverageSource> = self
            .sources
            .iter()
            .map(|s| CoverageSource {
                source_id: s.source_id,
                label: s.label.clone(),
                evidence: s.evidence,
                rows: s.rows.clone(),
            })
            .collect();
        coverage::group(&sources)
    }

    /// Each source's status, for [`floors::evaluate`].
    pub fn statuses(&self) -> Vec<(i64, SourceStatus)> {
        self.sources
            .iter()
            .map(|s| (s.source_id, s.status))
            .collect()
    }

    /// Group, then classify against the floor. `protected` names the
    /// protected/reference sources by source id (ADR 0011 rule 7).
    pub fn floors(&self, protected: &[i64], params: &FloorParams) -> Result<FloorReport> {
        let coverage = self.coverage()?;
        floors::evaluate(&coverage, &self.statuses(), protected, params)
    }
}

// ── The master's registry ───────────────────────────────────────────────────

/// What must not change between checking the master and finishing its read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    nlink: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileStamp {
    fn of(md: &fs::Metadata) -> Self {
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

/// Sidecars that mean the master is in use, or that its file alone is not
/// the whole database.
fn master_sidecars(path: &Path) -> Vec<String> {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .filter(|suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            fs::symlink_metadata(PathBuf::from(name)).is_ok()
        })
        .map(str::to_owned)
        .collect()
}

/// Every guard that must hold before and after reading the master.
fn check_master_file(path: &Path) -> Result<FileStamp> {
    let md = fs::symlink_metadata(path)
        .with_context(|| format!("no master catalog at '{}'", path.display()))?;
    if md.file_type().is_symlink() {
        bail!(
            "master path '{}' is a symlink; refusing to read through it",
            path.display()
        );
    }
    if !md.is_file() {
        bail!("master path '{}' is not a regular file", path.display());
    }
    if md.nlink() > 1 {
        bail!(
            "master '{}' has {} hard links; sidecars beside another name would be \
             invisible from this one",
            path.display(),
            md.nlink()
        );
    }
    let sidecars = master_sidecars(path);
    if !sidecars.is_empty() {
        bail!(
            "master '{}' is in use or holds changes not yet folded into the file \
             ({} beside it); run again when no other BackupSage command is using it",
            path.display(),
            sidecars.join(" and ")
        );
    }
    let mut header = [0u8; 16];
    fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut header))
        .with_context(|| format!("cannot read '{}'", path.display()))?;
    if &header != b"SQLite format 3\0" {
        bail!("'{}' is not a BackupSage master catalog", path.display());
    }
    Ok(FileStamp::of(&md))
}

/// `mode=ro&immutable=1`: SQLite reads the file alone, takes no locks and
/// creates nothing beside it. The guards around the read stand in for the
/// locks it does not take.
fn open_master_immutable(path: &Path) -> Result<Connection> {
    let absolute = std::path::absolute(path)?;
    let mut uri = b"file:".to_vec();
    for &byte in absolute.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte);
        } else {
            uri.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
    uri.extend_from_slice(b"?mode=ro&immutable=1");
    Connection::open_with_flags(
        PathBuf::from(OsString::from_vec(uri)),
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("cannot open '{}' read-only", path.display()))
}

/// Test support: run `hook` once, on this thread, after the next master
/// read and before its post-read guards.
#[doc(hidden)]
pub fn set_master_read_hook(hook: impl FnOnce() + 'static) {
    MASTER_READ_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

thread_local! {
    static MASTER_READ_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

fn run_master_read_hook() {
    if let Some(hook) = MASTER_READ_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// Read the master's registry without writing anything beside it.
pub fn load_registry(master_path: &Path) -> Result<Vec<RegistrySource>> {
    let before = check_master_file(master_path)?;
    let conn = open_master_immutable(master_path)?;
    let app_id: u32 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    if app_id != master::MASTER_APPLICATION_ID {
        bail!(
            "'{}' is not a signed BackupSage master catalog (a per-source index, \
             another database, or a master from before v1.0.1 that no master \
             command has adopted yet)",
            master_path.display()
        );
    }
    let rows = Master { conn }
        .list()
        .with_context(|| format!("cannot read the registry in '{}'", master_path.display()))?;

    run_master_read_hook();
    let after = fs::metadata(master_path).ok().map(|md| FileStamp::of(&md));
    if after != Some(before) || !master_sidecars(master_path).is_empty() {
        bail!(
            "master '{}' changed while it was read; run again when no other \
             BackupSage command is using it",
            master_path.display()
        );
    }
    Ok(rows
        .into_iter()
        .map(|r| RegistrySource {
            source_id: r.archive_id,
            label: r.label,
            db_path: PathBuf::from(r.db_path),
            registry_status: Some(r.status),
        })
        .collect())
}

/// The registry from the master, then every source from its own index.
pub fn load_from_master(master_path: &Path) -> Result<LoadedCoverage> {
    build(&load_registry(master_path)?)
}

/// Ad-hoc indexes: source ids 1, 2, … in argument order, labelled by file
/// name, with no registry status. The same index given twice is refused:
/// it would count as two replicas.
pub fn load_from_indexes(db_paths: &[PathBuf]) -> Result<LoadedCoverage> {
    build(&adhoc_registry(db_paths)?)
}

/// The registry [`load_from_indexes`] builds, without loading anything.
pub fn adhoc_registry(db_paths: &[PathBuf]) -> Result<Vec<RegistrySource>> {
    let mut seen = BTreeSet::new();
    let mut registry = Vec::new();
    for (i, path) in db_paths.iter().enumerate() {
        let identity = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity) {
            bail!("index '{}' is given more than once", path.display());
        }
        registry.push(RegistrySource {
            source_id: i as i64 + 1,
            label: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            db_path: path.clone(),
            registry_status: None,
        });
    }
    Ok(registry)
}

// ── Mapping one index onto coverage input ───────────────────────────────────

/// Load every registered source's index and map it onto coverage input.
/// The result is in source-id order whatever order `registry` is in.
pub fn build(registry: &[RegistrySource]) -> Result<LoadedCoverage> {
    let mut ordered: Vec<&RegistrySource> = registry.iter().collect();
    ordered.sort_by_key(|r| r.source_id);
    for pair in ordered.windows(2) {
        if pair[0].source_id == pair[1].source_id {
            bail!("source id {} is registered twice", pair[0].source_id);
        }
    }
    let mut sources = Vec::with_capacity(ordered.len());
    let mut uuids: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in ordered {
        // Refuse an unknown registry status before touching the index.
        let registry_status = entry
            .registry_status
            .as_deref()
            .map(registry_trust)
            .transpose()?
            .flatten();
        let loaded = diff_input::load_index(&entry.db_path);
        // A copied index is the same evidence under another name, never a
        // second replica.
        if let Some(uuid) = loaded.snapshot.info.index_uuid.clone() {
            if let Some(first) = uuids.insert(uuid.clone(), entry.db_path.clone()) {
                bail!(
                    "'{}' and '{}' are the same index (index_uuid {uuid}); a copy of \
                     an index is not a second replica",
                    first.display(),
                    entry.db_path.display()
                );
            }
        }
        sources.push(map_source(entry, registry_status, loaded));
    }
    Ok(LoadedCoverage { sources })
}

/// Whether a source currency can show that the source is present now: only
/// a currency that actually observed the source can. This is an allow-list,
/// and the match is exhaustive with no `_` arm on purpose: a new
/// `SourceCurrency` variant stops this compiling until someone decides, so
/// it can never default to a trusted, present source.
pub fn currency_can_show_presence(currency: SourceCurrency) -> bool {
    match currency {
        SourceCurrency::StatMatches
        | SourceCurrency::Stale
        | SourceCurrency::DirectoryUnverified => true,
        SourceCurrency::NotChecked | SourceCurrency::Offline | SourceCurrency::Denied => false,
    }
}

/// Positive proof that the recorded source can be read now: the file is
/// opened for reading, or the directory opened and listed. Nothing is read
/// from it and nothing is written. A stat alone is not enough: an unreadable
/// archive can keep the size and mtime its index recorded.
fn source_readable(source: Option<&str>, source_type: Option<&str>) -> Result<(), String> {
    let source = source.ok_or("the index records no source path")?;
    if source.contains('\u{fffd}') {
        return Err(
            "the recorded source path is a lossy rendering; its exact bytes are \
                    unknown, so it cannot be opened"
                .into(),
        );
    }
    let path = Path::new(source);
    if !path.is_absolute() {
        return Err("the recorded source path is relative, so it cannot be opened".into());
    }
    let want_dir = source_type == Some("dir");
    let kind_ok = |t: fs::FileType| if want_dir { t.is_dir() } else { t.is_file() };
    let wrong_kind = || {
        if want_dir {
            "the source path is not a directory, so it is not the indexed source".to_owned()
        } else {
            "the source path is not a regular file (a FIFO, device, socket or \
             directory now stands there), so it is not the indexed archive"
                .to_owned()
        }
    };
    // The type check comes before any open: opening a FIFO with no writer
    // blocks forever, and a device open can have side effects.
    let meta =
        fs::metadata(path).map_err(|e| format!("the source cannot be opened for reading: {e}"))?;
    if !kind_ok(meta.file_type()) {
        return Err(wrong_kind());
    }
    if want_dir {
        return fs::read_dir(path)
            .map(|_| ())
            .map_err(|e| format!("the source cannot be opened for reading: {e}"));
    }
    // Nonblocking, and re-checked on the open handle, so a swap between the
    // stat and the open can neither hang the load nor pass as the archive.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("the source cannot be opened for reading: {e}"))?;
    let opened = file
        .metadata()
        .map_err(|e| format!("the source cannot be opened for reading: {e}"))?;
    if !kind_ok(opened.file_type()) {
        return Err(wrong_kind());
    }
    Ok(())
}

/// What a registry status says about trust. `None` means it says nothing
/// beyond what reading the index shows (ADR 0011).
fn registry_trust(status: &str) -> Result<Option<SourceStatus>> {
    Ok(match status {
        master::STATUS_OK => Some(SourceStatus::Ok),
        master::STATUS_INCOMPLETE => Some(SourceStatus::Incomplete),
        master::STATUS_STALE_INDEX => Some(SourceStatus::StaleIndex),
        master::STATUS_DB_MISSING => Some(SourceStatus::DbMissing),
        master::STATUS_ARCHIVE_MISSING => Some(SourceStatus::ArchiveMissing),
        // Both describe the master's replica, which is never used here:
        // the rows come from the index itself.
        master::STATUS_STALE_REPLICA | master::STATUS_V2_LIMITED => None,
        other => bail!("unknown registry status '{other}'; refusing to guess its meaning"),
    })
}

/// Higher is less trusted; only `Ok` and `Incomplete` count (ADR 0011).
fn distrust(status: SourceStatus) -> u8 {
    match status {
        SourceStatus::Ok => 0,
        SourceStatus::Incomplete => 1,
        SourceStatus::StaleIndex => 2,
        SourceStatus::DbMissing => 3,
        SourceStatus::ArchiveMissing => 4,
    }
}

fn map_source(
    entry: &RegistrySource,
    registry: Option<SourceStatus>,
    loaded: LoadedIndex,
) -> LoadedSource {
    let LoadedIndex { snapshot, health } = loaded;
    let mut notes = Vec::new();
    let mut evidence = match snapshot.info.state {
        SnapshotState::Complete => SourceEvidence::Complete,
        SnapshotState::Incomplete => SourceEvidence::Incomplete,
        // Incompatible rows (another schema, hash algorithm or identity)
        // are not comparable content evidence.
        SnapshotState::Incompatible | SnapshotState::Unavailable => SourceEvidence::Unavailable,
    };
    let mut rows = Vec::new();
    let mut kinds = BTreeMap::new();
    if evidence != SourceEvidence::Unavailable {
        for e in &snapshot.entries {
            let entry_kind = match e.entry_type {
                EntryType::File => EntryKind::File,
                EntryType::Hardlink => EntryKind::Hardlink,
                EntryType::Symlink => EntryKind::Symlink,
                EntryType::Unsupported => {
                    notes.push(LoadNote {
                        code: LoadCode::UnknownEntryType,
                        detail: format!(
                            "files row {} has an entry type this version does not know",
                            e.file_id
                        ),
                    });
                    evidence = SourceEvidence::Unavailable;
                    break;
                }
            };
            if let Some(kind) = &e.kind {
                kinds.insert(e.file_id, kind.clone());
            }
            rows.push(CoverageRow {
                file_id: e.file_id,
                path_raw: e.path.clone(),
                entry: entry_kind,
                size: Some(e.size),
                content_hash: e.content_hash,
                flags: e.flags,
            });
        }
    }
    if evidence != SourceEvidence::Unavailable
        && health.content_mode.as_deref() == Some("metadata-only")
    {
        if rows.iter().any(|r| r.content_hash.is_some()) {
            notes.push(LoadNote {
                code: LoadCode::ContradictoryContentMode,
                detail: "the index says metadata-only but a row carries a content hash".into(),
            });
            evidence = SourceEvidence::Unavailable;
        } else if evidence == SourceEvidence::Complete {
            evidence = SourceEvidence::NoContentHashes;
        }
    }
    if evidence == SourceEvidence::Unavailable {
        rows.clear();
        kinds.clear();
    }
    // Rows stay proof of a copy only when the source was positively shown
    // present and readable now. Anything else (unplugged, denied, never
    // checked, lossy or unrecorded path, unreadable) leaves them history,
    // listed but never counted (ADR 0011).
    let currency = snapshot.info.source_currency;
    if evidence != SourceEvidence::Unavailable {
        let shown = if currency_can_show_presence(currency) {
            source_readable(health.source.as_deref(), health.source_type.as_deref())
        } else {
            Err(format!(
                "the source's presence was not established ({currency:?})"
            ))
        };
        if let Err(why) = shown {
            notes.push(LoadNote {
                code: LoadCode::SourceUnverified,
                detail: why,
            });
            evidence = SourceEvidence::Unreachable;
        }
    }

    // Trust from what reading showed now: an unusable index, then the live
    // source, then how far indexing got.
    let live = if evidence == SourceEvidence::Unavailable {
        SourceStatus::DbMissing
    } else if evidence == SourceEvidence::Unreachable {
        SourceStatus::ArchiveMissing
    } else {
        // Exhaustive on purpose: see `currency_can_show_presence`.
        match currency {
            SourceCurrency::Stale => SourceStatus::StaleIndex,
            SourceCurrency::StatMatches | SourceCurrency::DirectoryUnverified => {
                if evidence == SourceEvidence::Incomplete {
                    SourceStatus::Incomplete
                } else {
                    SourceStatus::Ok
                }
            }
            SourceCurrency::NotChecked | SourceCurrency::Offline | SourceCurrency::Denied => {
                SourceStatus::ArchiveMissing
            }
        }
    };
    // The registry records what `master sync`/`verify` last found (a deep
    // verify sees changes a stat cannot); it may only lower trust.
    let status = match registry {
        Some(recorded) if distrust(recorded) > distrust(live) => {
            notes.push(LoadNote {
                code: LoadCode::RegistryStatus,
                detail: format!(
                    "the master last recorded '{}' for this source",
                    entry.registry_status.as_deref().unwrap_or_default()
                ),
            });
            recorded
        }
        _ => live,
    };

    LoadedSource {
        source_id: entry.source_id,
        label: entry.label.clone(),
        db_path: entry.db_path.clone(),
        registry_status: entry.registry_status.clone(),
        source: health.source,
        source_type: health.source_type,
        evidence,
        status,
        index_notes: health.notes,
        notes,
        rows,
        kinds,
    }
}
