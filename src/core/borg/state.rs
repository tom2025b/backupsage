use super::{Error, Result};
use crate::outpath::{file_id, sidecar, FileId, ProtectedSet};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Persistent operator-selected private state. Creation never changes existing
/// permissions or initializes trust. The operator must supply dedicated paths,
/// every source/index/DB and any ordinary backup-state roots as exclusions.
/// Mode checks are ordinary filesystem hygiene, NOT read-only enforcement;
/// the future child backend must expose keys/credentials read-only.
pub struct PrivateState {
    pub(super) base: PathBuf,
    pub(super) cache: PathBuf,
    pub(super) security: PathBuf,
    pub(super) keys: PathBuf,
    root: PathBuf,
    credential: Option<PathBuf>,
    excluded: Vec<PathBuf>,
    root_id: FileId,
}

fn resolved(path: &Path) -> Result<PathBuf> {
    if fs::symlink_metadata(path).is_ok() {
        path.canonicalize().map_err(|_| Error::UnsafeState)
    } else {
        let name = path.file_name().ok_or(Error::UnsafeState)?;
        Ok(path
            .parent()
            .ok_or(Error::UnsafeState)?
            .canonicalize()
            .map_err(|_| Error::UnsafeState)?
            .join(name))
    }
}

fn overlap(a: &Path, b: &Path) -> Result<bool> {
    let ar = resolved(a)?;
    let br = resolved(b)?;
    Ok(ar.starts_with(&br)
        || br.starts_with(&ar)
        || matches!((file_id(a), file_id(b)), (Some(a), Some(b)) if a == b))
}

fn private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => Err(Error::UnsafeState),
    }
}

// Refuse symlinks, special files, cross-device mounts, unexpected modes/owners and
// multi-linked files. Reuse ADR 0001 identity/protection, including hardlinks.
fn inspect_tree(root: &Path, writable: bool, protected: &ProtectedSet) -> Result<Vec<FileId>> {
    let root_md = fs::symlink_metadata(root).map_err(|_| Error::UnsafeState)?;
    if !root_md.is_dir() {
        return Err(Error::UnsafeState);
    }
    let mut ids = Vec::new();
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|_| Error::UnsafeState)?;
        let p = entry.path();
        let md = fs::symlink_metadata(p).map_err(|_| Error::UnsafeState)?;
        let expected = match (md.is_dir(), md.is_file(), writable) {
            (true, _, true) => 0o700,
            (true, _, false) => 0o500,
            (_, true, true) => 0o600,
            (_, true, false) => 0o400,
            _ => return Err(Error::UnsafeState),
        };
        if md.mode() & 0o7777 != expected
            || md.uid() != unsafe { libc::geteuid() }
            || md.dev() != root_md.dev()
            || (md.is_file() && md.nlink() != 1)
        {
            return Err(Error::UnsafeState);
        }
        protected.check_dest(p).map_err(|_| Error::UnsafeState)?;
        ids.push(file_id(p).ok_or(Error::UnsafeState)?);
    }
    Ok(ids)
}

impl PrivateState {
    pub fn create(
        root: &Path,
        keys: &Path,
        credential: Option<&Path>,
        sources: &[&Path],
        databases: &[&Path],
        other_protected: &[&Path],
    ) -> Result<Self> {
        let mut excluded: Vec<PathBuf> = sources
            .iter()
            .chain(other_protected)
            .map(|p| p.to_path_buf())
            .collect();
        for db in databases {
            excluded.extend([db.to_path_buf(), sidecar(db, "-wal"), sidecar(db, "-shm")]);
        }
        for (i, a) in excluded.iter().enumerate() {
            for b in excluded.iter().skip(i + 1) {
                if overlap(a, b)? {
                    return Err(Error::UnsafeState);
                }
            }
        }
        // Check all relationships before creating any state.
        let mut separate = vec![root, keys];
        separate.extend(credential);
        for (i, a) in separate.iter().enumerate() {
            for b in separate
                .iter()
                .skip(i + 1)
                .copied()
                .chain(excluded.iter().map(PathBuf::as_path))
            {
                if overlap(a, b)? {
                    return Err(Error::UnsafeState);
                }
            }
            // Reject even an otherwise safe symlink spelling for private inputs.
            if a.ancestors()
                .any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
            {
                return Err(Error::UnsafeState);
            }
        }
        private_dir(root)?;
        let root = root.canonicalize().map_err(|_| Error::UnsafeState)?;
        let state = Self {
            base: root.join("base"),
            cache: root.join("cache"),
            security: root.join("security"),
            keys: keys.canonicalize().map_err(|_| Error::UnsafeState)?,
            credential: credential.map(Path::to_path_buf),
            excluded,
            root_id: file_id(&root).ok_or(Error::UnsafeState)?,
            root,
        };
        // Refuse unsafe existing contents before creating child directories.
        state.validate()?;
        for p in [&state.base, &state.cache, &state.security] {
            private_dir(p)?;
        }
        state.validate()?;
        Ok(state)
    }

    // Bind the private profile to the actual selected source, even if a caller
    // omitted or confused its source exclusion list. Pinning/confinement races
    // still require the real backend; this is an ordinary topology check.
    pub(super) fn validate_source(&self, source: &fs::File) -> Result<()> {
        use std::os::fd::AsRawFd;
        let source = PathBuf::from(format!("/proc/self/fd/{}", source.as_raw_fd()));
        for p in [&self.root, &self.keys]
            .into_iter()
            .chain(self.credential.iter())
            .chain(
                self.excluded
                    .iter()
                    .filter(|p| file_id(p) != file_id(&source)),
            )
        {
            if overlap(p, &source)? {
                return Err(Error::UnsafeState);
            }
        }
        self.validate()
    }

    pub(super) fn validate(&self) -> Result<()> {
        if file_id(&self.root) != Some(self.root_id) {
            return Err(Error::UnsafeState);
        }
        let mut protected = ProtectedSet::new();
        for p in self
            .excluded
            .iter()
            .chain(self.credential.iter())
            .chain(std::iter::once(&self.keys))
        {
            if overlap(&self.root, p)? {
                return Err(Error::UnsafeState);
            }
            protected.add_file(p);
            if p.is_dir() {
                protected.add_dir_tree(p);
                for entry in walkdir::WalkDir::new(p).follow_links(false) {
                    protected.add_file(entry.map_err(|_| Error::UnsafeState)?.path());
                }
            }
        }
        let state_ids = inspect_tree(&self.root, true, &protected)?;
        let mut input_protected = ProtectedSet::new();
        for p in &self.excluded {
            input_protected.add_file(p);
            if p.is_dir() {
                input_protected.add_dir_tree(p);
            }
        }
        let key_ids = inspect_tree(&self.keys, false, &input_protected)?;
        if key_ids.iter().any(|id| state_ids.contains(id)) {
            return Err(Error::UnsafeState);
        }
        if let Some(p) = &self.credential {
            let md = fs::symlink_metadata(p).map_err(|_| Error::UnsafeState)?;
            if !md.is_file()
                || md.mode() & 0o7777 != 0o600
                || md.nlink() != 1
                || md.uid() != unsafe { libc::geteuid() }
            {
                return Err(Error::UnsafeState);
            }
            input_protected
                .check_dest(p)
                .map_err(|_| Error::UnsafeState)?;
            if key_ids.contains(&file_id(p).ok_or(Error::UnsafeState)?) {
                return Err(Error::UnsafeState);
            }
        }
        Ok(())
    }

    /// Create one new private metadata file without clobbering existing state.
    /// Never use for plaintext content or credentials.
    pub fn create_metadata_file(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return Err(Error::InvalidInput);
        }
        self.validate()?;
        let path = self.base.join(name);
        let mut protected = ProtectedSet::new();
        for p in &self.excluded {
            protected.add_file(p);
            if p.is_dir() {
                protected.add_dir_tree(p);
            }
        }
        protected
            .check_dest(&path)
            .map_err(|_| Error::UnsafeState)?;
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| Error::UnsafeState)?;
        f.write_all(bytes).map_err(|_| Error::UnsafeState)?;
        self.validate()
    }
}
