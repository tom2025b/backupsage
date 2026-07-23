//! Output-path safety boundary (v1.0.1).
//!
//! Every place BackupSage creates or replaces a file goes through this
//! module: path identity, protected inputs, no-clobber promotion, and
//! precondition checks. No feature implements its own weaker copy.

use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Identity of an existing filesystem object (device + inode). Two
/// paths with equal `FileId`s are the same file regardless of spelling,
/// symlinks, or hardlinks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileId {
    dev: u64,
    ino: u64,
}

/// Identity of whatever `path` resolves to (follows symlinks), or None
/// if nothing exists there.
pub fn file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::metadata(path).ok()?;
    Some(FileId {
        dev: md.dev(),
        ino: md.ino(),
    })
}

/// Paths a command must never write over: source archives, directory
/// sources (whole trees), input indexes, the master catalog, and the
/// SQLite sidecars of every protected database.
#[derive(Default)]
pub struct ProtectedSet {
    files: Vec<(FileId, PathBuf)>,
    dirs: Vec<PathBuf>,
}

impl ProtectedSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Protect the file `path` currently resolves to, if any.
    pub fn add_file(&mut self, path: &Path) {
        if let Some(id) = file_id(path) {
            self.files.push((id, path.to_path_buf()));
        }
    }

    /// Protect a SQLite database plus its `-wal`/`-shm` sidecars.
    pub fn add_db(&mut self, path: &Path) {
        self.add_file(path);
        self.add_file(&sidecar(path, "-wal"));
        self.add_file(&sidecar(path, "-shm"));
    }

    /// Protect an entire directory tree.
    pub fn add_dir_tree(&mut self, path: &Path) {
        if let Ok(canon) = path.canonicalize() {
            self.dirs.push(canon);
        }
    }

    /// Fail unless a single file may be created at `dest`.
    pub fn check_dest(&self, dest: &Path) -> Result<()> {
        self.check_one(dest)
    }

    /// Fail unless a SQLite database may be created at `dest` — also
    /// gates the `-wal`/`-shm` names SQLite will create beside it.
    pub fn check_db_dest(&self, dest: &Path) -> Result<()> {
        self.check_one(dest)?;
        self.check_one(&sidecar(dest, "-wal"))?;
        self.check_one(&sidecar(dest, "-shm"))
    }

    fn check_one(&self, dest: &Path) -> Result<()> {
        let name = dest
            .file_name()
            .with_context(|| format!("output path '{}' has no file name", dest.display()))?;
        // A symlink at the destination — dangling or not — would make the
        // write land somewhere else. Always refuse.
        if let Ok(md) = fs::symlink_metadata(dest) {
            if md.file_type().is_symlink() {
                bail!("refusing to write through symlink '{}'", dest.display());
            }
        }
        // Canonicalize the parent so `sub/../x` spellings and symlinked
        // directories cannot dodge the checks below.
        let parent = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let canon_parent = parent.canonicalize().with_context(|| {
            format!("output directory '{}' is not accessible", parent.display())
        })?;
        let resolved = canon_parent.join(name);
        if let Some(id) = file_id(&resolved) {
            if let Some((_, hit)) = self.files.iter().find(|(fid, _)| *fid == id) {
                bail!(
                    "output '{}' is protected input '{}' (same file)",
                    dest.display(),
                    hit.display()
                );
            }
        }
        for root in &self.dirs {
            if resolved.starts_with(root) {
                bail!(
                    "output '{}' is inside protected source directory '{}'",
                    dest.display(),
                    root.display()
                );
            }
        }
        Ok(())
    }
}

/// SQLite sidecar naming: `x.db` → `x.db-wal` / `x.db-shm`.
pub fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Same-directory staging name for building `final_path`:
/// `dir/x.db` → `dir/.x.db.tmp.<pid>`.
pub fn stage_path(final_path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(final_path.file_name().unwrap_or_default());
    name.push(format!(".tmp.{}", std::process::id()));
    final_path.with_file_name(name)
}

/// Publish `staged` at `final_path` without replacing anything:
/// `hard_link` fails if any entry exists at `final_path` and never
/// follows a symlink there. The staged name is removed afterwards.
pub fn promote_no_clobber(staged: &Path, final_path: &Path) -> Result<()> {
    fs::hard_link(staged, final_path).with_context(|| {
        format!(
            "output '{}' already exists (BackupSage never overwrites)",
            final_path.display()
        )
    })?;
    let _ = fs::remove_file(staged);
    Ok(())
}

/// Replace `final_path` with `staged` in one rename. Callers must have
/// verified the file being replaced is theirs to replace.
pub fn promote_replace(staged: &Path, final_path: &Path) -> Result<()> {
    fs::rename(staged, final_path).with_context(|| {
        format!(
            "could not promote staged output to '{}'",
            final_path.display()
        )
    })
}

/// Create a brand-new file at `dest` with `bytes`: gate against the
/// protected set, stage in the destination directory, then promote
/// no-clobber. Failure at any point leaves no visible partial output.
pub fn write_new_file(dest: &Path, bytes: &[u8], protected: &ProtectedSet) -> Result<()> {
    protected.check_dest(dest)?;
    if fs::symlink_metadata(dest).is_ok() {
        bail!(
            "output '{}' already exists (BackupSage never overwrites)",
            dest.display()
        );
    }
    let staged = stage_path(dest);
    let _ = fs::remove_file(&staged); // our own crashed-run debris only
    let write = (|| -> Result<()> {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = fs::remove_file(&staged);
        return Err(e).with_context(|| format!("writing '{}'", dest.display()));
    }
    if let Err(e) = promote_no_clobber(&staged, dest) {
        let _ = fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn identity_matches_across_spellings_and_hardlinks() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.tar");
        fs::write(&a, b"data").unwrap();
        fs::create_dir(d.path().join("sub")).unwrap();
        let alias = d.path().join("sub/../a.tar");
        let hard = d.path().join("h.tar");
        fs::hard_link(&a, &hard).unwrap();
        let id = file_id(&a).unwrap();
        assert_eq!(file_id(&alias).unwrap(), id);
        assert_eq!(file_id(&hard).unwrap(), id);
    }

    #[test]
    fn check_dest_rejects_protected_file_symlink_and_containment() {
        let d = tempfile::tempdir().unwrap();
        let archive = d.path().join("a.tar");
        fs::write(&archive, b"data").unwrap();
        let srcdir = d.path().join("src");
        fs::create_dir(&srcdir).unwrap();
        fs::write(srcdir.join("member.txt"), b"m").unwrap();
        let mut p = ProtectedSet::new();
        p.add_file(&archive);
        p.add_dir_tree(&srcdir);
        // direct identity
        assert!(p.check_dest(&archive).is_err());
        // dot-dot spelling of the same file
        assert!(p.check_dest(&d.path().join("src/../a.tar")).is_err());
        // hardlink alias
        let hard = d.path().join("h.tar");
        fs::hard_link(&archive, &hard).unwrap();
        assert!(p.check_dest(&hard).is_err());
        // symlink at destination (even dangling) is refused
        let link = d.path().join("link.db");
        std::os::unix::fs::symlink(d.path().join("nowhere"), &link).unwrap();
        assert!(p.check_dest(&link).is_err());
        // containment in a protected tree
        assert!(p.check_dest(&srcdir.join("out.db")).is_err());
        // unrelated new path is fine
        assert!(p.check_dest(&d.path().join("fresh.db")).is_ok());
    }

    #[test]
    fn check_db_dest_covers_sidecars() {
        let d = tempfile::tempdir().unwrap();
        let victim = d.path().join("x.db-wal");
        fs::write(&victim, b"wal").unwrap();
        let mut p = ProtectedSet::new();
        p.add_file(&victim);
        assert!(p.check_db_dest(&d.path().join("x.db")).is_err());
        assert!(p.check_db_dest(&d.path().join("y.db")).is_ok());
    }

    #[test]
    fn write_new_file_never_clobbers_and_stages_invisibly() {
        let d = tempfile::tempdir().unwrap();
        let dest = d.path().join("report.json");
        let p = ProtectedSet::new();
        write_new_file(&dest, b"{}", &p).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"{}");
        // second write to the same path fails and leaves content intact
        assert!(write_new_file(&dest, b"XX", &p).is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"{}");
        // no staging debris
        let names: Vec<_> = fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp.")), "{names:?}");
        // dangling symlink destination is refused, symlink left in place
        let link = d.path().join("link.json");
        std::os::unix::fs::symlink(d.path().join("gone"), &link).unwrap();
        assert!(write_new_file(&link, b"x", &p).is_err());
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn promote_replace_swaps_atomically() {
        let d = tempfile::tempdir().unwrap();
        let fin = d.path().join("i.db");
        fs::write(&fin, b"old").unwrap();
        let staged = stage_path(&fin);
        fs::write(&staged, b"new").unwrap();
        promote_replace(&staged, &fin).unwrap();
        assert_eq!(fs::read(&fin).unwrap(), b"new");
        assert!(!staged.exists());
    }
}
