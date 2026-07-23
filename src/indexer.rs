//! Streams a source (tar archive or directory) entry-by-entry and indexes
//! text into SQLite FTS5 plus per-file metadata — size, mtime, full-content
//! BLAKE3, image dimensions, perceptual hash, EXIF date — in a single pass,
//! without extracting anything to disk.
//!
//! Tar reader chain: File → progress tracker → BLAKE3 wrapper → BufReader →
//! decompressor → tar. Each entry is read once in chunks: every byte feeds
//! the per-file hash; only the first `cap` bytes are retained for
//! text/EXIF/image work. After the last entry the underlying compressed
//! stream is drained to EOF so the whole-archive fingerprint equals
//! `b3sum(file)` (tar stops at the end-of-archive zero blocks; without the
//! drain the fingerprint would miss trailing padding and be useless for
//! `master verify --deep`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::Connection;

use crate::exif_date::{self, MediaKind};
use crate::format::{self, Format};
use crate::outpath;
use crate::phash;
use crate::store::{self, flags, EntryRecord, FinalizeCounts, SourceMeta};

/// Bytes sampled from the start of each file for the null-byte binary check.
const BINARY_PROBE: usize = 8 * 1024;

/// Archive entries per SQLite transaction.
pub(crate) const BATCH_SIZE: usize = 500;

/// Flush the in-memory word-frequency map once it holds this many words.
const WORD_FLUSH_THRESHOLD: usize = 100_000;

const MIN_WORD_CHARS: usize = 3;
const MAX_WORD_CHARS: usize = 32;

pub const DEFAULT_MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MEDIA_CAP: u64 = 64 * 1024 * 1024;

/// Read chunk size; also the hash-feed granularity.
const CHUNK: usize = 256 * 1024;

pub struct IndexOptions {
    /// Per-file cap on retained content for text indexing.
    pub max_file_size: u64,
    /// Per-file cap on retained content for media entries (image decode
    /// needs the whole file; a truncated JPEG cannot be decoded).
    pub media_cap: u64,
    /// Whether to maintain the word_freq table used by `top`.
    pub word_stats: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            media_cap: DEFAULT_MEDIA_CAP,
            word_stats: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct IndexSummary {
    pub db_path: PathBuf,
    pub format: String,
    /// Text files whose content went into FTS.
    pub files_indexed: u64,
    /// Binary (non-media) files — names only.
    pub files_skipped_binary: u64,
    /// Text files larger than the text cap.
    pub files_truncated: u64,
    /// Hard links and symlinks, indexed by name.
    pub files_link_names: u64,
    /// Entries with a full-content hash.
    pub files_hashed: u64,
    /// Images with a perceptual hash.
    pub images_phashed: u64,
    /// Images without one (HEIC/RAW/over-cap/decode-failed).
    pub images_no_phash: u64,
}

// ── Public entry point ───────────────────────────────────────────────────────

/// Index the source (tar archive or directory) into a fresh v3 database.
pub fn run_index(
    source: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
) -> Result<IndexSummary> {
    if source.is_dir() {
        crate::source_dir::index_dir(source, explicit_db, opts)
    } else {
        index_tar(source, explicit_db, opts)
    }
}

// ── Shared per-entry pipeline ────────────────────────────────────────────────

/// Everything derived from one entry's content stream.
pub(crate) struct EntryOutcome {
    pub content_hash: Option<[u8; 32]>,
    pub kind: &'static str,
    pub img_w: Option<u32>,
    pub img_h: Option<u32>,
    pub phash: Option<u64>,
    pub exif_unix: Option<i64>,
    pub exif_src: Option<&'static str>,
    pub flags: i64,
    /// `Some` ⇒ text content destined for FTS + word stats.
    pub fts_text: Option<String>,
    /// Text file exceeded the text cap (counts toward `files_truncated`).
    pub truncated_text: bool,
}

/// Read one entry's content stream to EOF: hash every byte, retain the first
/// `cap` bytes, classify, and extract media metadata.
pub(crate) fn process_reader(
    reader: &mut dyn Read,
    declared_size: u64,
    path: &str,
    opts: &IndexOptions,
    warn: &mut dyn FnMut(String),
) -> EntryOutcome {
    let mk = exif_date::media_kind(path);
    let cap = if mk == MediaKind::Other {
        opts.max_file_size
    } else {
        opts.media_cap
    };

    let mut out = EntryOutcome {
        content_hash: None,
        kind: "binary",
        img_w: None,
        img_h: None,
        phash: None,
        exif_unix: None,
        exif_src: None,
        flags: 0,
        fts_text: None,
        truncated_text: false,
    };

    let mut hasher = blake3::Hasher::new();
    // Don't trust declared_size for preallocation — a corrupt header could
    // claim petabytes.
    let mut buf: Vec<u8> = Vec::with_capacity((declared_size.min(1024 * 1024)) as usize);
    let mut chunk = [0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&chunk[..n]);
                total += n as u64;
                if (buf.len() as u64) < cap {
                    let take = ((cap - buf.len() as u64) as usize).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            Err(e) => {
                warn(format!("warning: read error in '{path}': {e}"));
                out.flags |= flags::READ_ERROR;
                return out; // name-only row; a partial hash would be a lie
            }
        }
    }
    out.content_hash = Some(*hasher.finalize().as_bytes());

    if total == 0 {
        out.kind = "empty";
        out.fts_text = Some(String::new());
        return out;
    }
    let over_cap = total > cap;

    match mk {
        MediaKind::Image => {
            out.kind = "image";
            out.set_exif(&buf);
            if !exif_date::is_decodable_image(path) {
                // HEIC in v1.0: EXIF + hash only.
            } else if over_cap {
                out.flags |= flags::IMAGE_OVER_CAP;
            } else {
                match decode_image_guarded(&buf) {
                    Some(img) => {
                        out.img_w = Some(img.width());
                        out.img_h = Some(img.height());
                        out.phash = Some(phash::phash(&img));
                    }
                    None => {
                        warn(format!("warning: could not decode image '{path}'"));
                        out.flags |= flags::DECODE_FAILED;
                    }
                }
            }
        }
        MediaKind::Raw => {
            out.kind = "raw";
            out.set_exif(&buf);
        }
        MediaKind::Video => {
            out.kind = "video";
        }
        MediaKind::Other => {
            if is_binary(&buf[..buf.len().min(BINARY_PROBE)]) {
                out.kind = "binary";
            } else {
                out.kind = "text";
                if over_cap {
                    out.flags |= flags::FTS_TRUNCATED;
                    out.truncated_text = true;
                }
                let mut content = String::from_utf8_lossy(&buf).into_owned();
                // 0x01 is the search-time highlight marker; strip it so
                // match counts cannot be inflated by file bytes.
                if content.contains('\u{1}') {
                    content = content.replace('\u{1}', "");
                }
                out.fts_text = Some(content);
            }
        }
    }
    out
}

impl EntryOutcome {
    fn set_exif(&mut self, buf: &[u8]) {
        if let Some(d) = exif_date::extract_date(buf) {
            self.exif_unix = Some(d.unix);
            self.exif_src = Some(d.src);
        }
    }
}

/// Decode with resource limits; a panicking or erroring decoder degrades to
/// None instead of aborting the index run (archives contain arbitrary bytes).
fn decode_image_guarded(buf: &[u8]) -> Option<image::DynamicImage> {
    let buf = buf.to_vec();
    catch_unwind(AssertUnwindSafe(move || {
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(16_384);
        limits.max_image_height = Some(16_384);
        limits.max_alloc = Some(512 * 1024 * 1024);
        let mut reader = image::ImageReader::new(io::Cursor::new(&buf))
            .with_guessed_format()
            .ok()?;
        reader.limits(limits);
        reader.decode().ok()
    }))
    .ok()
    .flatten()
}

/// Book-keeping shared by the tar and directory front-ends.
pub(crate) struct IndexRun<'a> {
    pub conn: &'a Connection,
    pub opts: &'a IndexOptions,
    pub summary: &'a mut IndexSummary,
    pub word_buf: HashMap<String, (u64, u64)>,
    pub in_batch: usize,
}

impl<'a> IndexRun<'a> {
    pub fn new(
        conn: &'a Connection,
        opts: &'a IndexOptions,
        summary: &'a mut IndexSummary,
    ) -> Result<Self> {
        conn.execute_batch("BEGIN")?;
        Ok(IndexRun {
            conn,
            opts,
            summary,
            word_buf: HashMap::new(),
            in_batch: 0,
        })
    }

    /// Insert one record and update counters/batching.
    pub fn record(&mut self, rec: &EntryRecord, outcome: Option<&EntryOutcome>) -> Result<()> {
        store::insert_entry(self.conn, rec)?;
        if let Some(o) = outcome {
            if o.content_hash.is_some() {
                self.summary.files_hashed += 1;
            }
            if o.truncated_text {
                self.summary.files_truncated += 1;
            }
            match o.kind {
                "text" => {
                    self.summary.files_indexed += 1;
                    if self.opts.word_stats {
                        if let Some(text) = &o.fts_text {
                            accumulate_words(text, &mut self.word_buf);
                            if self.word_buf.len() >= WORD_FLUSH_THRESHOLD {
                                flush_word_stats(self.conn, &mut self.word_buf)?;
                            }
                        }
                    }
                }
                "binary" => self.summary.files_skipped_binary += 1,
                "image" => {
                    if o.phash.is_some() {
                        self.summary.images_phashed += 1;
                    } else {
                        self.summary.images_no_phash += 1;
                    }
                }
                _ => {}
            }
        }
        self.in_batch += 1;
        if self.in_batch >= BATCH_SIZE {
            self.conn.execute_batch("COMMIT; BEGIN")?;
            self.in_batch = 0;
        }
        Ok(())
    }

    /// Flush stats, run post-passes, mark complete, commit, fold WAL.
    pub fn finish(mut self, archive_blake3: Option<&str>) -> Result<()> {
        flush_word_stats(self.conn, &mut self.word_buf)?;
        let counts = FinalizeCounts {
            files_indexed: self.summary.files_indexed,
            files_skipped_binary: self.summary.files_skipped_binary,
            files_truncated: self.summary.files_truncated,
        };
        store::finalize_v3(self.conn, &counts, archive_blake3)?;
        self.conn.execute_batch("COMMIT")?;
        store::fold_wal(self.conn)?;
        Ok(())
    }
}

/// Staged/final destination pair for one index build. The database is
/// built at `staged` (same directory as `final_path`) and only replaces
/// `final_path` via [`IndexPaths::promote`] once complete; dropping the
/// pair without promoting removes the staged files, so a failed or
/// interrupted build never touches the last completed index.
pub(crate) struct IndexPaths {
    pub staged: PathBuf,
    pub final_path: PathBuf,
    promoted: bool,
}

impl IndexPaths {
    /// Atomically replace `final_path` with the completed staged index,
    /// then drop stale sidecars of the replaced index (ownership of
    /// `final_path` was verified before the build started).
    pub(crate) fn promote(mut self) -> Result<()> {
        outpath::promote_replace(&self.staged, &self.final_path)?;
        self.promoted = true;
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(outpath::sidecar(&self.final_path, suffix));
        }
        Ok(())
    }
}

impl Drop for IndexPaths {
    fn drop(&mut self) {
        if !self.promoted {
            for p in [
                self.staged.clone(),
                outpath::sidecar(&self.staged, "-wal"),
                outpath::sidecar(&self.staged, "-shm"),
            ] {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
}

/// If something already exists at `dest` it must be a BackupSage index
/// of THIS source; anything else is refused unchanged.
fn assert_replaceable_index(dest: &Path, source: &Path, source_type: &str) -> Result<()> {
    if std::fs::symlink_metadata(dest).is_err() {
        return Ok(()); // nothing there — plain create
    }
    let conn = crate::searcher::open_index(dest).with_context(|| {
        format!(
            "existing file '{}' is not a BackupSage index; refusing to replace it",
            dest.display()
        )
    })?;
    let stored_type =
        crate::searcher::get_meta(&conn, "source_type").unwrap_or_else(|| "tar".into());
    let stored = crate::searcher::get_meta(&conn, "source")
        .or_else(|| crate::searcher::get_meta(&conn, "archive"))
        .unwrap_or_default();
    let same = stored_type == source_type
        && match (Path::new(&stored).canonicalize(), source.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
    if !same {
        anyhow::bail!(
            "existing index '{}' belongs to source '{}', not '{}'; refusing to replace it",
            dest.display(),
            stored,
            source.display()
        );
    }
    Ok(())
}

/// Gate a candidate final destination against the protected set, verify
/// ownership of anything already there, and create the staged database
/// beside it.
fn create_staged_db(
    final_path: &Path,
    meta: &SourceMeta,
    protected: &outpath::ProtectedSet,
) -> Result<(IndexPaths, Connection)> {
    protected.check_db_dest(final_path)?;
    assert_replaceable_index(final_path, meta.source, meta.source_type)?;
    let staged = outpath::stage_path(final_path);
    // The staged namespace is ours alone: clear crashed-run debris only.
    for p in [
        staged.clone(),
        outpath::sidecar(&staged, "-wal"),
        outpath::sidecar(&staged, "-shm"),
    ] {
        let _ = std::fs::remove_file(&p);
    }
    let conn = store::create_v3(&staged, meta)?;
    Ok((
        IndexPaths {
            staged,
            final_path: final_path.to_path_buf(),
            promoted: false,
        },
        conn,
    ))
}

/// Choose and gate the destination, falling back to the current
/// directory when the default location is not writable (e.g. archive on
/// a read-only mount). The fallback passes the same safety gate.
pub(crate) fn create_db_with_fallback(
    source: &Path,
    explicit_db: Option<&Path>,
    meta_type: &str,
    opts: &IndexOptions,
) -> Result<(IndexPaths, Connection)> {
    let db_path = resolve_db_path(source, explicit_db);
    let meta = SourceMeta {
        source,
        source_type: meta_type,
        text_cap: opts.max_file_size,
        media_cap: opts.media_cap,
        word_stats: opts.word_stats,
    };
    let mut protected = outpath::ProtectedSet::new();
    if meta_type == "dir" {
        protected.add_dir_tree(source);
    } else {
        protected.add_file(source);
    }
    match create_staged_db(&db_path, &meta, &protected) {
        Ok(pair) => Ok(pair),
        Err(e) if explicit_db.is_none() => {
            let fallback = PathBuf::from(db_file_name(source));
            eprintln!(
                "warning: cannot create index at '{}' ({e:#}) — using './{}'",
                db_path.display(),
                fallback.display()
            );
            create_staged_db(&fallback, &meta, &protected)
        }
        Err(e) => Err(e),
    }
}

// ── Tar front-end ────────────────────────────────────────────────────────────

/// `Read` wrapper feeding every byte into a BLAKE3 hasher.
struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
}

impl<R: Read> HashingReader<R> {
    fn new(inner: R) -> Self {
        HashingReader {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }
    fn finalize_hex(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

type TarInner = BufReader<HashingReader<indicatif::ProgressBarIter<File>>>;

/// Concrete decompressor so the compressed stream can be recovered and
/// drained after tar iteration (the archive-fingerprint contract).
enum Decoder {
    Zstd(zstd::Decoder<'static, TarInner>),
    Gzip(flate2::read::MultiGzDecoder<TarInner>),
    Plain(TarInner),
}

impl Read for Decoder {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Decoder::Zstd(r) => r.read(buf),
            Decoder::Gzip(r) => r.read(buf),
            Decoder::Plain(r) => r.read(buf),
        }
    }
}

impl Decoder {
    fn into_compressed(self) -> TarInner {
        match self {
            Decoder::Zstd(r) => r.finish(),
            Decoder::Gzip(r) => r.into_inner(),
            Decoder::Plain(r) => r,
        }
    }
}

fn index_tar(
    archive_path: &Path,
    explicit_db: Option<&Path>,
    opts: &IndexOptions,
) -> Result<IndexSummary> {
    let fmt = format::detect_file(archive_path)?;
    let (paths, conn) = create_db_with_fallback(archive_path, explicit_db, "tar", opts)?;
    let db_path = paths.final_path.clone();

    println!("Archive : {} ({fmt})", archive_path.display());
    println!("Index   : {}", db_path.display());
    println!();

    let archive_file = File::open(archive_path)
        .with_context(|| format!("cannot open archive: {}", archive_path.display()))?;
    let total_bytes = archive_file.metadata().map(|m| m.len()).unwrap_or(0);

    let pb = ProgressBar::new(total_bytes);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.cyan} [{elapsed_precise}] [{bar:45.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, eta {eta}) — {msg}",
        )
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-"),
    );

    let hashing = HashingReader::new(pb.wrap_read(archive_file));
    let tracked: TarInner = BufReader::with_capacity(CHUNK, hashing);
    let decoder = match fmt {
        Format::Zstd => Decoder::Zstd(
            zstd::Decoder::with_buffer(tracked).context("failed to initialise zstd decoder")?,
        ),
        Format::Gzip => Decoder::Gzip(flate2::read::MultiGzDecoder::new(tracked)),
        Format::PlainTar => Decoder::Plain(tracked),
    };
    let mut tar_archive = tar::Archive::new(decoder);

    let mut summary = IndexSummary {
        db_path: db_path.clone(),
        format: fmt.to_string(),
        ..IndexSummary::default()
    };
    let mut run = IndexRun::new(&conn, opts, &mut summary)?;
    let mut entry_no = 0u64;

    for entry_result in tar_archive
        .entries()
        .context("failed to read tar entries")?
    {
        // A corrupt entry header is fatal: tar's iterator cannot resync past
        // it, so "skip and continue" would silently drop the rest of the
        // archive. Bail — completed stays 0 and later searches warn.
        let mut entry = entry_result.with_context(|| {
            format!(
                "corrupt tar entry after {entry_no} readable entries — \
                 the index is incomplete"
            )
        })?;

        let entry_type = entry.header().entry_type();
        let content_entry = entry_type.is_file()
            || matches!(
                entry_type,
                tar::EntryType::GNUSparse | tar::EntryType::Continuous
            );
        let name_only_entry = matches!(entry_type, tar::EntryType::Link | tar::EntryType::Symlink);
        if !content_entry && !name_only_entry {
            continue; // directories, device nodes, fifos, pax metadata
        }

        let entry_path = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from("<unreadable-path>"));

        entry_no += 1;
        if entry_no % 64 == 1 {
            pb.set_message(truncate_path(&entry_path, 50));
        }

        let mtime = entry.header().mtime().ok().map(|m| m as i64);
        let mode = entry.header().mode().ok();

        if name_only_entry {
            let link_target = entry
                .link_name()
                .ok()
                .flatten()
                .map(|p| p.to_string_lossy().into_owned());
            let rec = EntryRecord {
                path: &entry_path,
                entry_type: if entry_type == tar::EntryType::Link {
                    "hardlink"
                } else {
                    "symlink"
                },
                link_target: link_target.as_deref(),
                size: 0,
                mtime_unix: mtime,
                mode,
                kind: "link",
                content_hash: None,
                img_w: None,
                img_h: None,
                phash: None,
                exif_unix: None,
                exif_src: None,
                flags: 0,
                fts_content: "",
            };
            run.record(&rec, None)?;
            run.summary.files_link_names += 1;
            continue;
        }

        // Sparse entries: tar-rs yields the condensed stream, so the hash is
        // not the logical file's hash — flag and let dedup exclude them.
        let mut extra_flags = 0i64;
        if entry_type == tar::EntryType::GNUSparse {
            extra_flags |= flags::SPARSE;
        }
        if let Ok(Some(pax)) = entry.pax_extensions() {
            for ext in pax.flatten() {
                if ext.key().is_ok_and(|k| k.starts_with("GNU.sparse")) {
                    extra_flags |= flags::SPARSE;
                    break;
                }
            }
        }

        let size = entry.size();
        let mut outcome = process_reader(&mut entry, size, &entry_path, opts, &mut |msg| {
            pb.suspend(|| eprintln!("{msg}"))
        });
        outcome.flags |= extra_flags;

        let rec = EntryRecord {
            path: &entry_path,
            entry_type: "file",
            link_target: None,
            size,
            mtime_unix: mtime,
            mode,
            kind: outcome.kind,
            content_hash: outcome.content_hash,
            img_w: outcome.img_w,
            img_h: outcome.img_h,
            phash: outcome.phash,
            exif_unix: outcome.exif_unix,
            exif_src: outcome.exif_src,
            flags: outcome.flags,
            fts_content: outcome.fts_text.as_deref().unwrap_or(""),
        };
        run.record(&rec, Some(&outcome))?;
    }

    // Drain the compressed stream to EOF so the fingerprint covers the whole
    // file (trailing zero blocks and all) — archive_blake3 == b3sum(file).
    let decoder = tar_archive.into_inner();
    let mut compressed = decoder.into_compressed();
    io::copy(&mut compressed, &mut io::sink())
        .context("failed to drain archive tail for fingerprint")?;
    let archive_blake3 = compressed.into_inner().finalize_hex();

    run.finish(Some(&archive_blake3))?;
    pb.finish_with_message("done");
    drop(conn); // close the staged database before promoting it
    paths.promote()?;
    Ok(summary)
}

// ── Word statistics (unchanged from v0.2) ────────────────────────────────────

/// Tokenise `text` and merge its per-document counts into `acc`.
fn accumulate_words(text: &str, acc: &mut HashMap<String, (u64, u64)>) {
    let mut doc_counts: HashMap<String, u64> = HashMap::new();
    for token in text.split(|c: char| !c.is_alphanumeric()) {
        if token.len() < MIN_WORD_CHARS || token.len() > MAX_WORD_CHARS * 4 {
            continue;
        }
        let chars = token.chars().count();
        if !(MIN_WORD_CHARS..=MAX_WORD_CHARS).contains(&chars) {
            continue;
        }
        *doc_counts.entry(token.to_lowercase()).or_insert(0) += 1;
    }
    for (word, count) in doc_counts {
        let entry = acc.entry(word).or_insert((0, 0));
        entry.0 += count;
        entry.1 += 1;
    }
}

fn flush_word_stats(conn: &Connection, acc: &mut HashMap<String, (u64, u64)>) -> Result<()> {
    if acc.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO word_freq(word, total_count, doc_count) VALUES (?1, ?2, ?3)
         ON CONFLICT(word) DO UPDATE SET
            total_count = total_count + excluded.total_count,
            doc_count   = doc_count + excluded.doc_count",
    )?;
    for (word, (total, docs)) in acc.drain() {
        stmt.execute(rusqlite::params![word, total as i64, docs as i64])?;
    }
    Ok(())
}

// ── Paths and helpers ────────────────────────────────────────────────────────

fn db_file_name(source_path: &Path) -> String {
    format!(
        "{}.db",
        source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    )
}

/// Default database location: `<source>.db` next to the source (works for
/// archives and directories alike), unless an explicit path was given.
pub fn resolve_db_path(source_path: &Path, explicit: Option<&Path>) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => source_path.with_file_name(db_file_name(source_path)),
    }
}

/// True if `data` looks like binary content (contains a null byte).
fn is_binary(data: &[u8]) -> bool {
    data.contains(&0)
}

/// Truncate a path to `max_chars` characters, keeping the tail visible.
pub(crate) fn truncate_path(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count + 1 - max_chars).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_detection() {
        assert!(is_binary(b"abc\x00def"));
        assert!(!is_binary(b"plain text, nothing to see"));
        assert!(!is_binary(b""));
    }

    #[test]
    fn tokeniser_filters_and_folds() {
        let mut acc = HashMap::new();
        accumulate_words("Foo foo BAR ab x 123 Grüße", &mut acc);
        assert_eq!(acc.get("foo"), Some(&(2, 1)));
        assert_eq!(acc.get("bar"), Some(&(1, 1)));
        assert_eq!(acc.get("123"), Some(&(1, 1)));
        assert_eq!(acc.get("grüße"), Some(&(1, 1)));
        assert!(!acc.contains_key("ab"));
        assert!(!acc.contains_key("x"));
    }

    #[test]
    fn tokeniser_drops_monster_tokens() {
        let mut acc = HashMap::new();
        accumulate_words(&"a".repeat(MAX_WORD_CHARS + 1), &mut acc);
        assert!(acc.is_empty());
    }

    #[test]
    fn doc_count_across_documents() {
        let mut acc = HashMap::new();
        accumulate_words("apple apple", &mut acc);
        accumulate_words("apple banana", &mut acc);
        assert_eq!(acc.get("apple"), Some(&(3, 2)));
        assert_eq!(acc.get("banana"), Some(&(1, 1)));
    }

    #[test]
    fn truncate_keeps_tail() {
        assert_eq!(truncate_path("short", 10), "short");
        let t = truncate_path("a/very/long/path/to/some/file.txt", 12);
        assert_eq!(t.chars().count(), 12);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("file.txt"));
    }

    #[test]
    fn default_db_path_next_to_source() {
        let p = resolve_db_path(Path::new("/backups/sys.tar.zst"), None);
        assert_eq!(p, Path::new("/backups/sys.tar.zst.db"));
        let d = resolve_db_path(Path::new("/photos/2020"), None);
        assert_eq!(d, Path::new("/photos/2020.db"));
        let e = resolve_db_path(
            Path::new("/backups/sys.tar.zst"),
            Some(Path::new("/tmp/x.db")),
        );
        assert_eq!(e, Path::new("/tmp/x.db"));
    }

    #[test]
    fn process_reader_hashes_past_the_cap() {
        let data = vec![b'x'; 10_000];
        let opts = IndexOptions {
            max_file_size: 1024,
            ..IndexOptions::default()
        };
        let mut warns = Vec::new();
        let out = process_reader(
            &mut &data[..],
            data.len() as u64,
            "big.log",
            &opts,
            &mut |m| warns.push(m),
        );
        assert_eq!(
            out.content_hash,
            Some(*blake3::hash(&data).as_bytes()),
            "hash must cover content beyond the cap"
        );
        assert!(out.truncated_text);
        assert_ne!(out.flags & flags::FTS_TRUNCATED, 0);
        assert_eq!(out.fts_text.as_ref().unwrap().len(), 1024);
        assert!(warns.is_empty());
    }

    #[test]
    fn process_reader_classifies_png_with_phash_and_dims() {
        // Encode a small deterministic PNG in memory.
        let img = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 3) as u8, (y * 5) as u8, ((x + y) % 255) as u8])
        }));
        let mut png = Vec::new();
        img.write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let out = process_reader(
            &mut &png[..],
            png.len() as u64,
            "photos/test.png",
            &IndexOptions::default(),
            &mut |_| {},
        );
        assert_eq!(out.kind, "image");
        assert_eq!((out.img_w, out.img_h), (Some(64), Some(48)));
        assert!(out.phash.is_some());
        assert_eq!(out.content_hash, Some(*blake3::hash(&png).as_bytes()));
    }

    #[test]
    fn process_reader_empty_and_garbage_image() {
        let out = process_reader(
            &mut io::empty(),
            0,
            "empty.txt",
            &IndexOptions::default(),
            &mut |_| {},
        );
        assert_eq!(out.kind, "empty");
        assert_eq!(out.content_hash, Some(*blake3::hash(b"").as_bytes()));

        let garbage = b"not actually a jpeg".to_vec();
        let out = process_reader(
            &mut &garbage[..],
            garbage.len() as u64,
            "fake.jpg",
            &IndexOptions::default(),
            &mut |_| {},
        );
        assert_eq!(out.kind, "image");
        assert!(out.phash.is_none());
        assert_ne!(out.flags & flags::DECODE_FAILED, 0);
    }
}
