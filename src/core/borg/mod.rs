//! Headless Borg 1.4.4 boundary. No production snapshot backend is enabled.
//!
//! The test-only mock exercises API/error/process plumbing on writable storage;
//! it is never evidence of filesystem immutability (ADR 0008).
mod capability;
mod environment;
mod operation;
mod process;
mod runtime;
mod state;

pub use capability::{UnsupportedBackend, ValidationBackend, VerifiedImmutableSnapshot};
pub use environment::BorgEnvironment;
pub use operation::{ArchiveName, Operation, RegularFilePath};
pub use runtime::{Cancellation, Limits, Runtime};
pub use state::PrivateState;

/// Deliberately payload-free: never retains OS errors, paths, argv, environment,
/// child output or credentials. Debug, Display and source() are all sanitized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    UnsupportedBackend,
    Validation,
    UnsafeState,
    ConflictingSecretEnvironment,
    InvalidInput,
    UnsupportedBorg,
    Spawn,
    Pipe,
    OutputLimit,
    Consumer,
    ChildFailed,
    Timeout,
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Borg runtime refused: {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
