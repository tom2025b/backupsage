//! Compatibility facade for the `backupsage` CLI package.
//!
//! Shared implementations live in `backupsage-core`; these re-exports preserve
//! existing library imports while frontends can depend on core directly.

pub use backupsage_core::*;

pub mod cli;
pub mod terminal;
pub mod textsafe;
