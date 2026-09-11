use super::{Error, Result};
use std::{fs::File, path::Path};

mod sealed {
    pub trait Sealed {}
}

/// Contract for reviewed in-tree validation backends, sealed against downstream
/// implementations that could assert validation without the ADR 0008 proof.
///
/// A successful implementation must validate same-FD provenance/identity,
/// immutable point-in-time Btrfs facts, all deployment prerequisites, and a
/// complete inherited child policy. It must own the pin and policy through the
/// last descendant's reap, and revalidate that pin before/after each execution.
/// Add the real implementation inside this module after privileged acceptance;
/// no raw-path constructor or generic "validation succeeded" token is provided.
/// Backend configuration must bind the approved private state/read roots as well
/// as the trusted locator/provenance registries; source alone is not sufficient.
///
/// ```compile_fail
/// use backupsage::borg::{ValidationBackend, VerifiedImmutableSnapshot, Result};
/// struct UncheckedBackend;
/// impl ValidationBackend for UncheckedBackend {
///     fn validate(&self, _: &std::path::Path) -> Result<VerifiedImmutableSnapshot> { unimplemented!() }
/// }
/// ```
/// ```compile_fail
/// use backupsage::borg::MockBackend; // absent from every production build
/// ```
pub trait ValidationBackend: sealed::Sealed {
    fn validate(&self, source: &Path) -> Result<VerifiedImmutableSnapshot>;
}

/// Fail-closed default on every current deployment. Does not execute Borg.
pub struct UnsupportedBackend;
impl sealed::Sealed for UnsupportedBackend {}
impl ValidationBackend for UnsupportedBackend {
    fn validate(&self, _: &Path) -> Result<VerifiedImmutableSnapshot> {
        Err(Error::UnsupportedBackend)
    }
}

/// Opaque owning capability; intentionally neither Clone nor serializable.
/// There is no enabled production success path yet.
///
/// ```compile_fail
/// use backupsage::borg::VerifiedImmutableSnapshot;
/// let _ = VerifiedImmutableSnapshot::new("/tmp/repo");
/// ```
/// ```compile_fail
/// use backupsage::borg::VerifiedImmutableSnapshot;
/// let _: VerifiedImmutableSnapshot = std::path::PathBuf::from("/tmp/repo").into();
/// ```
/// ```compile_fail
/// use backupsage::borg::VerifiedImmutableSnapshot;
/// let _: VerifiedImmutableSnapshot = "/tmp/repo".into();
/// ```
/// ```compile_fail
/// use backupsage::borg::VerifiedImmutableSnapshot;
/// let _ = VerifiedImmutableSnapshot { pin: std::fs::File::open("/").unwrap() };
/// ```
pub struct VerifiedImmutableSnapshot {
    pin: File,
}

impl VerifiedImmutableSnapshot {
    pub(super) fn pin(&self) -> &File {
        &self.pin
    }

    // The real backend must replace these gates with same-FD pre/post facts and
    // child policy installation. Production refuses even if a future caller
    // accidentally obtains a capability before implementing these gates.
    pub(super) fn revalidate(&self) -> Result<()> {
        #[cfg(test)]
        {
            self.pin.metadata().map_err(|_| Error::Validation)?;
            Ok(())
        }
        #[cfg(not(test))]
        {
            Err(Error::UnsupportedBackend)
        }
    }

    pub(super) fn child_policy_ready(&self) -> Result<()> {
        #[cfg(test)]
        {
            Ok(())
        }
        #[cfg(not(test))]
        {
            Err(Error::UnsupportedBackend)
        }
    }
}

/// API/error plumbing ONLY. Wraps an ordinary writable directory and proves no
/// immutability. Compiled only into this crate's unit-test binary; no feature,
/// public constructor or mock-to-production conversion exists.
#[cfg(test)]
pub(super) struct MockBackend;
#[cfg(test)]
impl sealed::Sealed for MockBackend {}
#[cfg(test)]
impl ValidationBackend for MockBackend {
    fn validate(&self, source: &Path) -> Result<VerifiedImmutableSnapshot> {
        use std::os::unix::fs::OpenOptionsExt;
        let pin = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(source)
            .map_err(|_| Error::Validation)?;
        Ok(VerifiedImmutableSnapshot { pin })
    }
}
