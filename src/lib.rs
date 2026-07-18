//! BackupSage — index and search text inside tar backups without extracting them.
//!
//! Split into a library so the pipeline is testable; `main.rs` only parses
//! arguments and renders output.

pub mod cli;
pub mod dedup;
pub mod exif_date;
pub mod format;
pub mod indexer;
pub mod master;
pub mod phash;
pub mod report;
pub mod searcher;
pub mod source_dir;
pub mod store;
