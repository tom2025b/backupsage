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

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::coverage::{Coverage, CoverageRow, SourceEvidence};
use crate::diff_input::InputNote;
use crate::floors::{self, FloorParams, FloorReport, SourceStatus};

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
    pub evidence: SourceEvidence,
    pub status: SourceStatus,
    /// What the index loader established (#101), verbatim.
    pub index_notes: Vec<InputNote>,
    pub notes: Vec<LoadNote>,
    pub rows: Vec<CoverageRow>,
}

/// Every source, in source-id order.
#[derive(Debug, Clone)]
pub struct LoadedCoverage {
    pub sources: Vec<LoadedSource>,
}

impl LoadedCoverage {
    /// Group the loaded rows with the #95 engine.
    pub fn coverage(&self) -> Result<Coverage> {
        let _ = &self.sources;
        bail!("not implemented")
    }

    /// Each source's status, for [`floors::evaluate`].
    pub fn statuses(&self) -> Vec<(i64, SourceStatus)> {
        Vec::new()
    }

    /// Group, then classify against the floor. `protected` names the
    /// protected/reference sources by source id (ADR 0011 rule 7).
    pub fn floors(&self, protected: &[i64], params: &FloorParams) -> Result<FloorReport> {
        let coverage = self.coverage()?;
        floors::evaluate(&coverage, &self.statuses(), protected, params)
    }
}

/// Read the master's registry without writing anything beside it.
pub fn load_registry(master_path: &Path) -> Result<Vec<RegistrySource>> {
    let _ = master_path;
    bail!("not implemented")
}

/// The registry from the master, then every source from its own index.
pub fn load_from_master(master_path: &Path) -> Result<LoadedCoverage> {
    build(&load_registry(master_path)?)
}

/// Ad-hoc indexes: source ids 1, 2, … in argument order, labelled by file
/// name, with no registry status.
pub fn load_from_indexes(db_paths: &[PathBuf]) -> Result<LoadedCoverage> {
    let _ = db_paths;
    bail!("not implemented")
}

/// Load every registered source's index and map it onto coverage input.
/// The result is in source-id order whatever order `registry` is in.
pub fn build(registry: &[RegistrySource]) -> Result<LoadedCoverage> {
    let _ = registry;
    bail!("not implemented")
}
