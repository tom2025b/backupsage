//! BackupSage shared indexing, search and deduplication logic.
//!
//! Frontends own argument parsing, transport and rendering. Core reports
//! typed progress and supports cooperative indexing cancellation.

#[cfg(target_os = "linux")]
pub mod borg;
mod borg_guard;
pub mod coverage;
pub mod coverage_input;
pub mod coverage_report;
pub mod dedup;
pub mod diff;
pub mod diff_input;
pub mod exif_date;
pub mod floors;
pub mod format;
pub mod index_read;
pub mod indexer;
pub mod legacy;
pub mod master;
pub mod outpath;
pub mod phash;
pub mod plan;
pub mod progress;
pub mod report;
pub mod searcher;
pub mod source_dir;
pub mod store;
