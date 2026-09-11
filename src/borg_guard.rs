//! Bounded structural refusal for Borg repository storage.
//!
//! Ordinary directory indexing must never mistake encrypted Borg segment
//! files for the archived files they represent.  This module deliberately
//! does not validate a repository or invoke Borg: it only recognizes enough
//! on-disk structure to refuse confirmed or ambiguous candidates.

use std::fs::{self, File, Metadata};
use std::io::{Read, Take};
use std::path::Path;

use anyhow::{bail, Context, Result};

/// Large enough for normal Borg configs, but bounded so a marker cannot make
/// source dispatch consume arbitrary data.  One extra byte is read to prove
/// whether the limit was exceeded.
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Directory entries examined while looking for the numeric `data/N/N`
/// segment shape.  The probe never opens a segment file.
const MAX_DATA_ENTRIES: usize = 256;

/// Refuse a source if it is, or canonically resides below, a Borg candidate.
/// This runs before source-type dispatch and therefore also protects a direct
/// selection of a segment file.
pub(crate) fn reject_borg_source(source: &Path) -> Result<()> {
    let canonical = source
        .canonicalize()
        .with_context(|| format!("cannot resolve source '{}'", source.display()))?;
    let start = if canonical.is_dir() {
        canonical.as_path()
    } else {
        canonical.parent().unwrap_or(canonical.as_path())
    };

    for ancestor in start.ancestors() {
        reject_borg_directory(ancestor)?;
    }
    Ok(())
}

/// Refuse one directory before a walker descends into it.  Canonicalization
/// catches aliases; before/after identities make a concurrent path swap an
/// error instead of an optimistic "ordinary directory" classification.
pub(crate) fn reject_borg_directory(dir: &Path) -> Result<()> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("cannot resolve directory '{}'", dir.display()))?;
    let before = fs::metadata(&canonical)
        .with_context(|| format!("cannot inspect directory '{}'", canonical.display()))?;
    if !before.is_dir() {
        bail!(
            "walked directory '{}' is no longer a directory",
            dir.display()
        );
    }

    let verdict = classify_directory(&canonical);

    let after = fs::metadata(&canonical)
        .with_context(|| format!("cannot re-inspect directory '{}'", canonical.display()))?;
    let canonical_after = dir
        .canonicalize()
        .with_context(|| format!("cannot re-resolve directory '{}'", dir.display()))?;
    if !same_file(&before, &after) || canonical_after != canonical {
        bail!(
            "directory '{}' changed while checking for Borg repository storage",
            dir.display()
        );
    }

    match verdict? {
        Candidate::Ordinary => Ok(()),
        Candidate::Borg(reason) => bail!(
            "refusing ordinary indexing of Borg repository candidate '{}': {reason}; \
             use the dedicated snapshot-backed Borg command when it is available",
            canonical.display()
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    Ordinary,
    Borg(String),
}

#[derive(Debug)]
enum DataShape {
    Absent,
    Directory { segment_file: bool },
    Suspicious(&'static str),
}

fn classify_directory(dir: &Path) -> Result<Candidate> {
    let data = inspect_data(&dir.join("data"))?;
    let config = dir.join("config");
    let config_meta = match fs::symlink_metadata(&config) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if data.has_segment_file() {
                Ok(Candidate::Borg(
                    "Borg-shaped data segments exist but config is missing".into(),
                ))
            } else {
                Ok(Candidate::Ordinary)
            };
        }
        Err(e) => {
            return if data.has_segment_file() {
                Ok(Candidate::Borg(format!(
                    "Borg-shaped data segments exist but config metadata is unreadable: {e}"
                )))
            } else {
                Err(e).with_context(|| format!("cannot inspect '{}'", config.display()))
            };
        }
    };

    if config_meta.file_type().is_symlink() {
        return if data.exists() {
            Ok(Candidate::Borg(
                "config is a symlink beside a data marker".into(),
            ))
        } else {
            Ok(Candidate::Ordinary)
        };
    }
    if !config_meta.is_file() {
        return if data.has_segment_file() {
            Ok(Candidate::Borg(
                "config is not a regular file beside Borg-shaped data segments".into(),
            ))
        } else {
            Ok(Candidate::Ordinary)
        };
    }

    let read = read_bounded_config(&config, &config_meta);
    let (bytes, oversized) = match read {
        Ok(value) => value,
        Err(e) => {
            return if data.has_segment_file() {
                Ok(Candidate::Borg(format!(
                    "Borg-shaped data segments exist but config is unreadable: {e:#}"
                )))
            } else {
                Ok(Candidate::Ordinary)
            };
        }
    };

    let parsed = parse_repository_section(&bytes);
    match parsed {
        RepositorySection::Absent => {
            if oversized && data.has_segment_file() {
                Ok(Candidate::Borg(
                    "oversized config may hide a repository section beside Borg-shaped data".into(),
                ))
            } else {
                Ok(Candidate::Ordinary)
            }
        }
        RepositorySection::Malformed(reason) => Ok(Candidate::Borg(reason)),
        RepositorySection::Fields { version, id } => {
            if oversized {
                return Ok(Candidate::Borg(
                    "repository config exceeds the bounded read limit".into(),
                ));
            }
            if version.as_deref() != Some(b"1") {
                return Ok(Candidate::Borg(match version {
                    Some(v) => format!(
                        "unsupported or malformed repository version '{}'",
                        String::from_utf8_lossy(&v)
                    ),
                    None => "repository version is missing".into(),
                }));
            }
            let valid_id = id
                .as_deref()
                .is_some_and(|value| value.len() == 64 && value.iter().all(u8::is_ascii_hexdigit));
            if !valid_id {
                return Ok(Candidate::Borg(
                    "repository ID is missing or is not 32-byte hexadecimal".into(),
                ));
            }
            match data {
                DataShape::Directory { .. } => Ok(Candidate::Borg(
                    "regular config has a supported [repository] section and data directory".into(),
                )),
                DataShape::Absent => Ok(Candidate::Borg(
                    "repository config exists but data directory is missing".into(),
                )),
                DataShape::Suspicious(reason) => Ok(Candidate::Borg(reason.into())),
            }
        }
    }
}

impl DataShape {
    fn exists(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    fn has_segment_file(&self) -> bool {
        matches!(self, Self::Directory { segment_file: true })
    }
}

fn inspect_data(data: &Path) -> Result<DataShape> {
    let meta = match fs::symlink_metadata(data) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DataShape::Absent),
        Err(e) => return Err(e).with_context(|| format!("cannot inspect '{}'", data.display())),
    };
    if meta.file_type().is_symlink() {
        return Ok(DataShape::Suspicious("repository data marker is a symlink"));
    }
    if !meta.is_dir() {
        return Ok(DataShape::Suspicious(
            "repository data marker is not a directory",
        ));
    }

    let entries = fs::read_dir(data)
        .with_context(|| format!("cannot inspect data directory '{}'", data.display()))?;
    for entry in entries.take(MAX_DATA_ENTRIES) {
        let entry = entry
            .with_context(|| format!("cannot inspect '{}': directory entry", data.display()))?;
        if !decimal_name(&entry.file_name()) {
            continue;
        }
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        for segment in fs::read_dir(entry.path())?.take(MAX_DATA_ENTRIES) {
            let segment = segment?;
            if decimal_name(&segment.file_name()) && fs::symlink_metadata(segment.path())?.is_file()
            {
                return Ok(DataShape::Directory { segment_file: true });
            }
        }
    }
    Ok(DataShape::Directory {
        segment_file: false,
    })
}

fn decimal_name(name: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

fn read_bounded_config(path: &Path, expected: &Metadata) -> Result<(Vec<u8>, bool)> {
    let file = File::open(path).with_context(|| format!("cannot open '{}'", path.display()))?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_file(expected, &opened) {
        bail!("config changed before it could be read");
    }
    let mut bytes = Vec::new();
    let mut limited: Take<File> = file.take(MAX_CONFIG_BYTES + 1);
    limited.read_to_end(&mut bytes)?;
    let oversized = expected.len() > MAX_CONFIG_BYTES || bytes.len() as u64 > MAX_CONFIG_BYTES;
    bytes.truncate(MAX_CONFIG_BYTES as usize);
    let after = fs::symlink_metadata(path)?;
    if !same_file(expected, &after) || after.len() != expected.len() {
        bail!("config changed while it was being read");
    }
    Ok((bytes, oversized))
}

#[derive(Debug)]
enum RepositorySection {
    Absent,
    Malformed(String),
    Fields {
        version: Option<Vec<u8>>,
        id: Option<Vec<u8>>,
    },
}

fn parse_repository_section(bytes: &[u8]) -> RepositorySection {
    let mut in_repository = false;
    let mut found = false;
    let mut version = None;
    let mut id = None;

    for raw_line in bytes.split(|byte| *byte == b'\n') {
        let line = trim_ascii(raw_line.strip_suffix(b"\r").unwrap_or(raw_line));
        if line.is_empty() || line.starts_with(b"#") || line.starts_with(b";") {
            continue;
        }
        if line.starts_with(b"[") {
            if line == b"[repository]" {
                if found {
                    return RepositorySection::Malformed(
                        "repository config contains duplicate [repository] sections".into(),
                    );
                }
                found = true;
                in_repository = true;
            } else if line.starts_with(b"[repository") {
                return RepositorySection::Malformed(
                    "repository section header is malformed".into(),
                );
            } else {
                in_repository = false;
            }
            continue;
        }
        if !in_repository {
            continue;
        }
        let Some(eq) = line.iter().position(|byte| *byte == b'=') else {
            return RepositorySection::Malformed(
                "repository section contains a malformed setting".into(),
            );
        };
        let key = trim_ascii(&line[..eq]);
        let value = trim_ascii(&line[eq + 1..]);
        let slot = match key {
            b"version" => Some(&mut version),
            b"id" => Some(&mut id),
            _ => None,
        };
        if let Some(slot) = slot {
            if slot.is_some() {
                return RepositorySection::Malformed(format!(
                    "repository config contains duplicate '{}' settings",
                    String::from_utf8_lossy(key)
                ));
            }
            *slot = Some(value.to_vec());
        }
    }

    if found {
        RepositorySection::Fields { version, id }
    } else {
        RepositorySection::Absent
    }
}

fn trim_ascii(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

#[cfg(unix)]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.len() == b.len() && a.is_dir() == b.is_dir() && a.is_file() == b.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_parser_requires_exact_unique_fields() {
        let valid = b"[repository]\nversion = 1\nid = 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
        assert!(matches!(
            parse_repository_section(valid),
            RepositorySection::Fields {
                version: Some(_),
                id: Some(_)
            }
        ));
        assert!(matches!(
            parse_repository_section(b"[repository\nversion = 1\n"),
            RepositorySection::Malformed(_)
        ));
        assert!(matches!(
            parse_repository_section(b"[application]\nversion = 1\n"),
            RepositorySection::Absent
        ));
    }
}
