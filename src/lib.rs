//! BackupSage — index and search text inside tar backups without extracting them.
//!
//! Split into a library so the pipeline is testable; `main.rs` only parses
//! arguments and renders output.

pub mod cli;
pub mod exif_date;
pub mod format;
pub mod indexer;
pub mod phash;
pub mod searcher;
pub mod store;
