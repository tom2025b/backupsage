//! BackupSage shared indexing, search and deduplication logic.
//!
//! Frontends own argument parsing, transport and rendering. Core reports
//! typed progress and supports cooperative indexing cancellation.

#[cfg(target_os = "linux")]
pub mod borg;
mod borg_guard;
pub mod dedup;
pub mod exif_date;
pub mod format;
pub mod indexer;
pub mod master;
pub mod outpath;
pub mod phash;
pub mod plan;
pub mod progress;
pub mod report;
pub mod searcher;
pub mod source_dir;
pub mod store;
