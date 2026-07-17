//! Archive format detection by magic bytes.
//!
//! The file extension is never trusted: a mislabelled archive silently
//! produced an empty index in v0.1, so we sniff the leading bytes instead.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zstd,
    Gzip,
    PlainTar,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Format::Zstd => write!(f, "tar + zstd"),
            Format::Gzip => write!(f, "tar + gzip"),
            Format::PlainTar => write!(f, "uncompressed tar"),
        }
    }
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];
const XZ_MAGIC: [u8; 6] = [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00];
const BZIP2_MAGIC: [u8; 3] = *b"BZh";
// Offset of the "ustar" magic in a tar header (POSIX and GNU both carry it).
const TAR_MAGIC_OFFSET: usize = 257;

/// Detect the archive format from the file's leading bytes.
///
/// `head` should hold at least the first 262 bytes when the file is that
/// large; shorter input is handled but plain tar cannot then be recognised.
pub fn detect(head: &[u8]) -> Result<Format> {
    if head.starts_with(&ZSTD_MAGIC) {
        return Ok(Format::Zstd);
    }
    if head.starts_with(&GZIP_MAGIC) {
        return Ok(Format::Gzip);
    }
    if head.starts_with(&XZ_MAGIC) {
        bail!("xz-compressed archives are not supported — recompress with zstd or gzip");
    }
    if head.starts_with(&BZIP2_MAGIC) {
        bail!("bzip2-compressed archives are not supported — recompress with zstd or gzip");
    }
    if head.len() >= TAR_MAGIC_OFFSET + 5
        && &head[TAR_MAGIC_OFFSET..TAR_MAGIC_OFFSET + 5] == b"ustar"
    {
        return Ok(Format::PlainTar);
    }
    bail!("unrecognised archive format — expected .tar, .tar.gz or .tar.zst")
}

/// Sniff the format of the archive at `path`.
pub fn detect_file(path: &Path) -> Result<Format> {
    let mut file =
        File::open(path).with_context(|| format!("cannot open archive: {}", path.display()))?;
    let mut head = [0u8; 512];
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    detect(&head[..filled]).with_context(|| format!("while inspecting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_zstd() {
        assert_eq!(
            detect(&[0x28, 0xB5, 0x2F, 0xFD, 0, 0]).unwrap(),
            Format::Zstd
        );
    }

    #[test]
    fn detects_gzip() {
        assert_eq!(detect(&[0x1F, 0x8B, 0x08, 0]).unwrap(), Format::Gzip);
    }

    #[test]
    fn detects_plain_tar() {
        let mut head = vec![0u8; 512];
        head[TAR_MAGIC_OFFSET..TAR_MAGIC_OFFSET + 5].copy_from_slice(b"ustar");
        assert_eq!(detect(&head).unwrap(), Format::PlainTar);
    }

    #[test]
    fn rejects_xz_with_clear_error() {
        let err = detect(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0, 0]).unwrap_err();
        assert!(err.to_string().contains("xz"));
    }

    #[test]
    fn rejects_unknown() {
        assert!(detect(b"hello world, definitely not a tar").is_err());
    }
}
