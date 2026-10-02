//! Bounded processing and durable publication anchored to directory handles.

use std::{
    fs::{File, Permissions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use filetime::FileTime;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rustix::{
    fs::{
        AtFlags, Mode as FileMode, OFlags, RenameFlags, fchown, fsync, mkdirat, open, openat,
        renameat_with, unlinkat,
    },
    io::{Errno, dup},
    process::{Gid, Uid},
};

use crate::{
    Error, Result,
    inspection::{Classification, InspectedEntry, SourceFingerprint},
    plan::{Mode, Plan},
    processing::{ProcessingOptions, ProcessingOutcome, process_image},
};

const MAX_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
const MIN_MEMORY_BUDGET: u64 = 64 * 1024 * 1024;
const ESTIMATED_BYTES_PER_PIXEL: u64 = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Publication outcome and report details for one eligible source.
#[derive(Debug)]
pub enum EntryOutcome {
    Reduced {
        source_bytes: u64,
        output_bytes: u64,
        bytes_saved: u64,
        warnings: Vec<String>,
    },
    NotBeneficial {
        source_bytes: u64,
        candidate_bytes: u64,
    },
    FidelityConflict {
        reason: String,
    },
    Failed {
        error: String,
    },
    Interrupted,
}

impl EntryOutcome {
    /// Returns the stable report label for this processing outcome.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Reduced { .. } => "reduced",
            Self::NotBeneficial { .. } => "not_beneficial",
            Self::FidelityConflict { .. } => "fidelity_conflict",
            Self::Failed { .. } => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

/// Source-relative path paired with its processing/publication outcome.
#[derive(Debug)]
pub struct ProcessedEntry {
    pub relative_path: PathBuf,
    pub outcome: EntryOutcome,
}

/// Worker limit, processing policy, and terminal progress preference.
pub struct RunOptions {
    pub jobs: Option<usize>,
    pub processing: ProcessingOptions,
    pub show_progress: bool,
}

/// Installs the process-wide signal handler and returns its shared cancellation flag.
pub fn install_cancellation_handler() -> Result<Arc<AtomicBool>> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, Ordering::SeqCst);
    })
    .map_err(|error| Error::InvalidArgument(format!("cannot install signal handler: {error}")))?;
    Ok(cancelled)
}

/// Reads available physical memory from Linux procfs, when accessible.
pub fn available_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let available_kib = meminfo.lines().find_map(|line| {
        let value = line.strip_prefix("MemAvailable:")?;
        value.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    available_kib.checked_mul(1024)
}

/// Selects a bounded pixel guard from available memory or the fallback limit.
pub fn default_max_pixels() -> u64 {
    available_memory_bytes()
        .map(|bytes| bytes / ESTIMATED_BYTES_PER_PIXEL)
        .unwrap_or(crate::inspection::DEFAULT_MAX_PIXELS)
        .clamp(1_000_000, crate::inspection::DEFAULT_MAX_PIXELS)
}

/// Processes eligible images in one bounded pool and publishes only verified reductions.
pub fn execute(
    plan: &Plan,
    inspected: &[InspectedEntry],
    options: RunOptions,
    cancelled: Arc<AtomicBool>,
) -> Result<Vec<ProcessedEntry>> {
    let eligible: Vec<_> = inspected
        .iter()
        .filter_map(|entry| match &entry.classification {
            Classification::Eligible(details) => Some((entry.relative_path.clone(), details)),
            _ => None,
        })
        .collect();
    if eligible.is_empty() {
        return Ok(Vec::new());
    }

    if let Mode::Preserve { output, .. } = &plan.mode {
        std::fs::create_dir(output).map_err(|source| Error::Io {
            path: output.clone(),
            source,
        })?;
        sync_directory(output.parent().unwrap_or(Path::new("."))).map_err(|source| Error::Io {
            path: output.clone(),
            source,
        })?;
    }
    let roots = PublishRoots::open(plan)?;

    let jobs = options
        .jobs
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .build()
        .map_err(|error| Error::InvalidArgument(format!("cannot create worker pool: {error}")))?;
    let memory_limit = available_memory_bytes()
        .map(|available| available / 2)
        .unwrap_or(512 * 1024 * 1024)
        .clamp(MIN_MEMORY_BUDGET, MAX_MEMORY_BUDGET);
    let memory = MemoryBudget::new(memory_limit);
    let progress = if options.show_progress {
        let progress = ProgressBar::new(eligible.len() as u64);
        progress.set_style(
            ProgressStyle::with_template("[{pos}/{len}] {wide_msg} {bar:30}")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        progress
    } else {
        ProgressBar::hidden()
    };
    let mut results = pool.install(|| {
        eligible
            .par_iter()
            .map(|(relative_path, details)| {
                if cancelled.load(Ordering::Acquire) {
                    return ProcessedEntry {
                        relative_path: relative_path.clone(),
                        outcome: EntryOutcome::Interrupted,
                    };
                }

                let pixels = u64::from(details.encoded_dimensions.width)
                    * u64::from(details.encoded_dimensions.height);
                let _permit = memory.acquire(pixels.saturating_mul(ESTIMATED_BYTES_PER_PIXEL));
                let source = plan.source.join(relative_path);
                let outcome = match process_image(&source, details, options.processing) {
                    ProcessingOutcome::Reduced(candidate) => {
                        if cancelled.load(Ordering::Acquire) {
                            EntryOutcome::Interrupted
                        } else if !details
                            .source_fingerprint
                            .matches_path(&source)
                            .unwrap_or(false)
                        {
                            EntryOutcome::Failed {
                                error: "source changed while its reduction was being built"
                                    .to_owned(),
                            }
                        } else {
                            match publish(
                                plan,
                                &roots,
                                relative_path,
                                details.source_fingerprint,
                                &candidate.bytes,
                            ) {
                                Ok(publication_warnings) => {
                                    let bytes_saved = candidate.bytes_saved();
                                    let mut warnings = candidate.warnings;
                                    warnings.extend(publication_warnings);
                                    EntryOutcome::Reduced {
                                        source_bytes: candidate.source_bytes,
                                        output_bytes: candidate.bytes.len() as u64,
                                        bytes_saved,
                                        warnings,
                                    }
                                }
                                Err(error) => EntryOutcome::Failed {
                                    error: error.to_string(),
                                },
                            }
                        }
                    }
                    ProcessingOutcome::NotBeneficial { candidate_bytes } => {
                        EntryOutcome::NotBeneficial {
                            source_bytes: details.source_bytes,
                            candidate_bytes,
                        }
                    }
                    ProcessingOutcome::FidelityConflict { reason } => {
                        EntryOutcome::FidelityConflict { reason }
                    }
                    ProcessingOutcome::Failed { error } => EntryOutcome::Failed { error },
                };
                progress.set_message(format!("{}: {}", relative_path.display(), outcome.kind()));
                progress.inc(1);
                ProcessedEntry {
                    relative_path: relative_path.clone(),
                    outcome,
                }
            })
            .collect::<Vec<_>>()
    });
    progress.finish_and_clear();
    results.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(results)
}

/// Open source/output directory handles that anchor publication paths.
struct PublishRoots {
    source: File,
    output: Option<File>,
}

impl PublishRoots {
    /// Opens source/output handles with final-component symlink checks.
    fn open(plan: &Plan) -> Result<Self> {
        let source = open_directory(&plan.source)?;
        let output = match &plan.mode {
            Mode::Preserve { output, .. } => Some(open_directory(output)?),
            Mode::Replace => None,
        };
        Ok(Self { source, output })
    }
}

/// Opens a directory handle while rejecting a symlink at the final component.
fn open_directory(path: &Path) -> Result<File> {
    open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        FileMode::empty(),
    )
    .map(File::from)
    .map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source: source.into(),
    })
}

/// Rechecks source identity and publishes through anchored source/output parents.
fn publish(
    plan: &Plan,
    roots: &PublishRoots,
    relative_path: &Path,
    fingerprint: SourceFingerprint,
    bytes: &[u8],
) -> Result<Vec<String>> {
    let (source_parent, file_name) = open_relative_parent(&roots.source, relative_path, false)?;
    let source_file = openat(
        &source_parent,
        file_name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        FileMode::empty(),
    )
    .map(File::from)
    .map_err(|source| Error::Io {
        path: plan.source.join(relative_path),
        source: source.into(),
    })?;
    let source_metadata = source_file.metadata().map_err(|source| Error::Io {
        path: plan.source.join(relative_path),
        source,
    })?;
    if !fingerprint.matches_metadata(&source_metadata) {
        return Err(Error::InvalidArgument(
            "source changed while its reduction was being built".to_owned(),
        ));
    }

    match &plan.mode {
        Mode::Preserve { output, .. } => {
            let root = roots.output.as_ref().expect("preservation output is open");
            let (destination_parent, destination_name) =
                open_relative_parent(root, relative_path, true)?;
            publish_file(
                &destination_parent,
                destination_name,
                bytes,
                &source_metadata,
                false,
                None,
                || true,
            )
            .map_err(|source| Error::Io {
                path: output.join(relative_path),
                source,
            })
        }
        Mode::Replace => publish_file(
            &source_parent,
            file_name,
            bytes,
            &source_metadata,
            true,
            Some(fingerprint),
            || {
                let Ok(file) = openat(
                    &source_parent,
                    file_name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    FileMode::empty(),
                ) else {
                    return false;
                };
                File::from(file)
                    .metadata()
                    .is_ok_and(|metadata| fingerprint.matches_metadata(&metadata))
            },
        )
        .map_err(|source| Error::Io {
            path: plan.source.join(relative_path),
            source,
        }),
    }
}

/// Durably publishes bytes, retaining the displaced source until replacement is accepted.
fn publish_file(
    parent: &File,
    destination: &std::ffi::OsStr,
    bytes: &[u8],
    source_metadata: &std::fs::Metadata,
    replace: bool,
    expected_source: Option<SourceFingerprint>,
    source_unchanged: impl FnOnce() -> bool,
) -> std::io::Result<Vec<String>> {
    let mut temporary = TemporaryFile::new(parent)?;
    temporary.file.write_all(bytes)?;
    filetime::set_file_handle_times(
        &temporary.file,
        None,
        Some(FileTime::from_last_modification_time(source_metadata)),
    )?;

    if replace
        && let Err(error) = fchown(
            &temporary.file,
            Some(Uid::from_raw(source_metadata.uid())),
            Some(Gid::from_raw(source_metadata.gid())),
        )
        && error != rustix::io::Errno::PERM
    {
        return Err(error.into());
    }
    temporary
        .file
        .set_permissions(Permissions::from_mode(source_metadata.permissions().mode()))?;
    temporary.file.sync_all()?;
    if !source_unchanged() {
        return Err(std::io::Error::other(
            "source changed immediately before publication",
        ));
    }
    if replace {
        renameat_with(
            parent,
            temporary.name.as_str(),
            parent,
            destination,
            RenameFlags::EXCHANGE,
        )?;
        // The temporary name now owns the displaced source, so cleanup must
        // preserve it unless an exchange back succeeds or we explicitly unlink it.
        temporary.published = true;
        let displaced_matches = match openat(
            parent,
            temporary.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FileMode::empty(),
        ) {
            Ok(file) => File::from(file).metadata().is_ok_and(|metadata| {
                expected_source.is_some_and(|fingerprint| fingerprint.matches_metadata(&metadata))
            }),
            Err(_) => false,
        };
        if !displaced_matches {
            let rollback = renameat_with(
                parent,
                temporary.name.as_str(),
                parent,
                destination,
                RenameFlags::EXCHANGE,
            );
            if rollback.is_ok() {
                temporary.published = false;
            }
            rollback?;
            fsync(parent)?;
            return Err(std::io::Error::other(
                "source changed during atomic publication",
            ));
        }
        if let Err(sync_error) = fsync(parent) {
            let rollback = renameat_with(
                parent,
                temporary.name.as_str(),
                parent,
                destination,
                RenameFlags::EXCHANGE,
            );
            if rollback.is_ok() {
                temporary.published = false;
            }
            let rollback_sync = fsync(parent);
            return match (rollback, rollback_sync) {
                (Ok(()), Ok(())) => Err(sync_error.into()),
                (rollback, rollback_sync) => Err(std::io::Error::other(format!(
                    "replacement durability failed ({sync_error}); rollback result: {rollback:?}; rollback sync result: {rollback_sync:?}"
                ))),
            };
        }
        if let Err(error) = unlinkat(parent, temporary.name.as_str(), AtFlags::empty()) {
            temporary.published = true;
            return Ok(vec![format!(
                "replacement is durable, but the displaced source remains as {} because cleanup failed: {error}",
                temporary.name
            )]);
        }
        temporary.published = true;
        if let Err(error) = fsync(parent) {
            return Ok(vec![format!(
                "replacement is durable, but temporary-file cleanup could not be synchronized: {error}"
            )]);
        }
        Ok(Vec::new())
    } else {
        renameat_with(
            parent,
            temporary.name.as_str(),
            parent,
            destination,
            RenameFlags::NOREPLACE,
        )?;
        temporary.published = true;
        fsync(parent)?;
        Ok(Vec::new())
    }
}

/// Opens relative parents without following symlinks, optionally creating output directories.
fn open_relative_parent<'a>(
    root: &File,
    relative_path: &'a Path,
    create: bool,
) -> Result<(File, &'a std::ffi::OsStr)> {
    let file_name = relative_path.file_name().ok_or_else(|| {
        Error::InvalidArgument(format!(
            "path has no file name: {}",
            relative_path.display()
        ))
    })?;
    let mut current = File::from(dup(root).map_err(|source| Error::Io {
        path: relative_path.to_path_buf(),
        source: source.into(),
    })?);
    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(Error::InvalidArgument(format!(
                "unsafe relative path: {}",
                relative_path.display()
            )));
        };
        if create {
            match mkdirat(&current, name, FileMode::from_raw_mode(0o755)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(source) => {
                    return Err(Error::Io {
                        path: relative_path.to_path_buf(),
                        source: source.into(),
                    });
                }
            }
        }
        current = openat(
            &current,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FileMode::empty(),
        )
        .map(File::from)
        .map_err(|source| Error::Io {
            path: relative_path.to_path_buf(),
            source: source.into(),
        })?;
    }
    Ok((current, file_name))
}

/// Exclusive destination-local output whose guard retains a displaced source after exchange.
struct TemporaryFile<'a> {
    parent: &'a File,
    name: String,
    file: File,
    published: bool,
}

impl<'a> TemporaryFile<'a> {
    /// Creates an exclusive temporary file beneath the already-open destination parent.
    fn new(parent: &'a File) -> std::io::Result<Self> {
        loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(".image-reducer-{}-{sequence}.tmp", std::process::id());
            match openat(
                parent,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                FileMode::from_raw_mode(0o600),
            ) {
                Ok(file) => {
                    return Ok(Self {
                        parent,
                        name,
                        file: File::from(file),
                        published: false,
                    });
                }
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for TemporaryFile<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = unlinkat(self.parent, self.name.as_str(), AtFlags::empty());
        }
    }
}

/// Synchronizes a directory entry after output-root creation.
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

/// Weighted limit shared by all workers in the dedicated Rayon pool.
struct MemoryBudget {
    limit: u64,
    used: Mutex<u64>,
    available: Condvar,
}

impl MemoryBudget {
    /// Creates a shared weighted budget for decoded-image work.
    fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    /// Waits for capacity and returns a permit; oversized work takes the whole budget.
    fn acquire(&self, requested: u64) -> MemoryPermit<'_> {
        let weight = requested.clamp(1, self.limit);
        let mut used = self.used.lock().unwrap_or_else(|error| error.into_inner());
        while used.saturating_add(weight) > self.limit {
            used = self
                .available
                .wait(used)
                .unwrap_or_else(|error| error.into_inner());
        }
        *used += weight;
        MemoryPermit {
            budget: self,
            weight,
        }
    }
}

/// Reserved weight returned to the memory budget when processing leaves scope.
struct MemoryPermit<'a> {
    budget: &'a MemoryBudget,
    weight: u64,
}

impl Drop for MemoryPermit<'_> {
    fn drop(&mut self) {
        let mut used = self
            .budget
            .used
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *used -= self.weight;
        self.budget.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        sync::atomic::AtomicBool,
    };

    use image::{GenericImageView, ImageFormat, Rgb, RgbImage};

    use crate::{
        inspection::{DEFAULT_MAX_PIXELS, inspect_files},
        plan::{Bounds, Mode, Plan},
    };

    use super::*;

    fn write_source(path: &Path) {
        let image = RgbImage::from_fn(300, 150, |x, y| {
            Rgb([
                x.wrapping_mul(17).wrapping_add(y.wrapping_mul(3)) as u8,
                x.wrapping_mul(5).wrapping_add(y.wrapping_mul(13)) as u8,
                x.wrapping_mul(23).wrapping_add(y.wrapping_mul(29)) as u8,
            ])
        });
        image.save_with_format(path, ImageFormat::Png).unwrap();
        std::fs::set_permissions(path, Permissions::from_mode(0o640)).unwrap();
        filetime::set_file_mtime(path, FileTime::from_unix_time(1_700_000_000, 0)).unwrap();
    }

    fn plan_and_inspection(replace: bool) -> (tempfile::TempDir, Plan, Vec<InspectedEntry>) {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        std::fs::create_dir(&source).unwrap();
        write_source(&source.join("photo.png"));
        let bounds = Bounds::new(60, 30).unwrap();
        let inspected = inspect_files(
            &source,
            &[PathBuf::from("photo.png")],
            bounds,
            DEFAULT_MAX_PIXELS,
        );
        let mode = if replace {
            Mode::Replace
        } else {
            Mode::Preserve {
                output: parent.path().join("output"),
                inferred: false,
            }
        };
        (
            parent,
            Plan {
                source,
                bounds,
                mode,
                exclusions: Vec::new(),
            },
            inspected,
        )
    }

    #[test]
    fn publishes_preserved_output_with_source_attributes() {
        let (_parent, plan, inspected) = plan_and_inspection(false);
        let source = plan.source.join("photo.png");
        let source_before = std::fs::read(&source).unwrap();

        let results = execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: Some(2),
                processing: ProcessingOptions::default(),
                show_progress: false,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert!(matches!(results[0].outcome, EntryOutcome::Reduced { .. }));
        assert_eq!(std::fs::read(&source).unwrap(), source_before);
        let Mode::Preserve { output, .. } = &plan.mode else {
            unreachable!()
        };
        let destination = output.join("photo.png");
        assert_eq!(image::open(&destination).unwrap().dimensions(), (60, 30));
        let source_metadata = std::fs::metadata(source).unwrap();
        let output_metadata = std::fs::metadata(destination).unwrap();
        assert_eq!(
            output_metadata.permissions().mode(),
            source_metadata.permissions().mode()
        );
        assert_eq!(output_metadata.mtime(), source_metadata.mtime());
    }

    #[test]
    fn transactionally_replaces_source() {
        let (_parent, plan, inspected) = plan_and_inspection(true);
        let source = plan.source.join("photo.png");
        let source_size = std::fs::metadata(&source).unwrap().len();

        let results = execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: Some(1),
                processing: ProcessingOptions::default(),
                show_progress: false,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert!(matches!(results[0].outcome, EntryOutcome::Reduced { .. }));
        assert!(std::fs::metadata(&source).unwrap().len() < source_size);
        assert_eq!(image::open(&source).unwrap().dimensions(), (60, 30));
        assert_eq!(
            std::fs::metadata(&source).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read_dir(&plan.source).unwrap().count(), 1);
    }

    #[test]
    fn replacement_restores_special_permission_bits_after_ownership() {
        let (_parent, plan, _inspected) = plan_and_inspection(true);
        let source = plan.source.join("photo.png");
        std::fs::set_permissions(&source, Permissions::from_mode(0o6750)).unwrap();
        let inspected = inspect_files(
            &plan.source,
            &[PathBuf::from("photo.png")],
            plan.bounds,
            DEFAULT_MAX_PIXELS,
        );

        let results = execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: Some(1),
                processing: ProcessingOptions::default(),
                show_progress: false,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert!(matches!(results[0].outcome, EntryOutcome::Reduced { .. }));
        assert_eq!(
            std::fs::metadata(source).unwrap().permissions().mode() & 0o7777,
            0o6750
        );
    }

    #[test]
    fn rolls_back_when_source_changes_at_atomic_exchange() {
        let (_parent, plan, inspected) = plan_and_inspection(true);
        let source = plan.source.join("photo.png");
        let Classification::Eligible(details) = &inspected[0].classification else {
            panic!("expected eligible image")
        };
        let parent = open_directory(&plan.source).unwrap();
        let metadata = std::fs::metadata(&source).unwrap();

        let result = publish_file(
            &parent,
            std::ffi::OsStr::new("photo.png"),
            b"candidate",
            &metadata,
            true,
            Some(details.source_fingerprint),
            || {
                std::fs::remove_file(&source).unwrap();
                std::fs::write(&source, b"new source").unwrap();
                true
            },
        );

        assert!(result.is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"new source");
        assert_eq!(std::fs::read_dir(&plan.source).unwrap().count(), 1);
    }

    #[test]
    fn temporary_guard_preserves_displaced_source_when_rollback_fails() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("photo.png");
        std::fs::write(&source, b"original source").unwrap();
        let parent = open_directory(directory.path()).unwrap();
        let mut temporary = TemporaryFile::new(&parent).unwrap();
        temporary.file.write_all(b"candidate").unwrap();
        let recovery_name = temporary.name.clone();

        renameat_with(
            &parent,
            temporary.name.as_str(),
            &parent,
            std::ffi::OsStr::new("photo.png"),
            RenameFlags::EXCHANGE,
        )
        .unwrap();
        temporary.published = true;
        drop(temporary);

        assert_eq!(std::fs::read(source).unwrap(), b"candidate");
        assert_eq!(
            std::fs::read(directory.path().join(recovery_name)).unwrap(),
            b"original source"
        );
    }

    #[test]
    fn cancellation_prevents_publication() {
        let (_parent, plan, inspected) = plan_and_inspection(false);

        let results = execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: Some(1),
                processing: ProcessingOptions::default(),
                show_progress: false,
            },
            Arc::new(AtomicBool::new(true)),
        )
        .unwrap();

        assert!(matches!(results[0].outcome, EntryOutcome::Interrupted));
        let Mode::Preserve { output, .. } = &plan.mode else {
            unreachable!()
        };
        assert_eq!(std::fs::read_dir(output).unwrap().count(), 0);
    }

    #[test]
    fn refuses_to_publish_when_source_changed_after_inspection() {
        let (_parent, plan, inspected) = plan_and_inspection(false);
        let source = plan.source.join("photo.png");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap();
        file.write_all(b"changed").unwrap();

        let results = execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: Some(1),
                processing: ProcessingOptions::default(),
                show_progress: false,
            },
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();

        assert!(matches!(results[0].outcome, EntryOutcome::Failed { .. }));
        let Mode::Preserve { output, .. } = &plan.mode else {
            unreachable!()
        };
        assert_eq!(std::fs::read_dir(output).unwrap().count(), 0);
    }

    #[test]
    fn refuses_to_follow_symlinked_output_subdirectory() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&output).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, output.join("nested")).unwrap();
        let root = open_directory(&output).unwrap();

        let result = open_relative_parent(&root, Path::new("nested/photo.png"), true);

        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    }
}
