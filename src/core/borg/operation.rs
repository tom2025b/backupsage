use super::{Error, Result, VerifiedImmutableSnapshot};

pub(super) const LOCATOR: &str = "/proc/self/fd/9";
pub(super) const FORMAT: &str = "{type}{mode}{uid}{gid}{size}{isomtime}{archiveid}{archivename}";

/// One literal archive, excluding checkpoint archives and Borg placeholders.
pub struct ArchiveName(String);
impl ArchiveName {
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.contains([':', '/', '\\', '{', '}', '*', '?', '[', ']'])
            || value.chars().any(char::is_control)
            || value.contains('\u{fffd}')
            || value.split('.').any(|s| s == "checkpoint")
        {
            return Err(Error::InvalidInput);
        }
        Ok(Self(value.to_owned()))
    }
}

/// One canonical archive-relative regular-file path. The adapter must establish
/// regular-file type from validated archive metadata before selecting it.
pub struct RegularFilePath(String);
impl RegularFilePath {
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty()
            || value.starts_with('/')
            || value
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
            || value.contains(['?', '\u{fffd}', '\\'])
            || value.chars().any(char::is_control)
        {
            return Err(Error::InvalidInput);
        }
        Ok(Self(value.to_owned()))
    }
}

enum Profile {
    Repository,
    Archive(ArchiveName),
    Extract(ArchiveName, RegularFilePath),
}

/// Closed profiles borrowing the owning capability until execution completes.
/// No command/argv builder, shell or extraction-to-disk variant is exposed.
///
/// ```compile_fail
/// use backupsage_core::borg::{Operation, VerifiedImmutableSnapshot};
/// fn inject(s: &VerifiedImmutableSnapshot) {
///     Operation::repository_metadata(s).arg("--repair");
/// }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::{Operation, VerifiedImmutableSnapshot};
/// fn inject(s: &VerifiedImmutableSnapshot) { Operation::new(s, "delete", ["--force"]); }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::{Operation, VerifiedImmutableSnapshot};
/// fn inject(s: &VerifiedImmutableSnapshot) { Operation::repository_metadata(s).format("{path}"); }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::{Operation, ArchiveName, RegularFilePath, VerifiedImmutableSnapshot};
/// fn inject(s: &VerifiedImmutableSnapshot, a: ArchiveName, p: RegularFilePath) {
///     Operation::extract_file(s, a, vec![p]);
/// }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::{Operation, VerifiedImmutableSnapshot};
/// fn inject(s: &VerifiedImmutableSnapshot) {
///     Operation::repository_metadata(s).stdout(false);
/// }
/// ```
/// ```compile_fail
/// use backupsage_core::borg::Runtime;
/// let _ = Runtime::new("/bin/sh", ["-c", "arbitrary command"]);
/// ```
pub struct Operation<'a> {
    pub(super) snapshot: &'a VerifiedImmutableSnapshot,
    profile: Profile,
}
impl<'a> Operation<'a> {
    pub fn repository_metadata(snapshot: &'a VerifiedImmutableSnapshot) -> Self {
        Self {
            snapshot,
            profile: Profile::Repository,
        }
    }
    pub fn archive_entries(snapshot: &'a VerifiedImmutableSnapshot, archive: ArchiveName) -> Self {
        Self {
            snapshot,
            profile: Profile::Archive(archive),
        }
    }
    pub fn extract_file(
        snapshot: &'a VerifiedImmutableSnapshot,
        archive: ArchiveName,
        path: RegularFilePath,
    ) -> Self {
        Self {
            snapshot,
            profile: Profile::Extract(archive, path),
        }
    }
    /// A detached inspection copy; modifying it cannot alter any operation.
    pub fn argv(&self) -> Vec<String> {
        match &self.profile {
            Profile::Repository => ["list", "--json", "--bypass-lock", "--", LOCATOR]
                .map(str::to_owned)
                .to_vec(),
            Profile::Archive(a) => [
                "list",
                "--json-lines",
                "--consider-part-files",
                "--bypass-lock",
                "--format",
                FORMAT,
                "--",
                &format!("{LOCATOR}::{}", a.0),
            ]
            .map(str::to_owned)
            .to_vec(),
            Profile::Extract(a, p) => [
                "extract",
                "--stdout",
                "--consider-part-files",
                "--bypass-lock",
                "--",
                &format!("{LOCATOR}::{}", a.0),
                &format!("pf:{}", p.0),
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    }
    pub(super) fn is_extract(&self) -> bool {
        matches!(self.profile, Profile::Extract(..))
    }
}
