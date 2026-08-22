use std::{
    fs::{File, Permissions},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use filetime::FileTime;
use rayon::prelude::*;
use rustix::{
    fs::fchown,
    process::{Gid, Uid},
};

use crate::{
    Error, Result,
    inspection::{Classification, InspectedEntry},
    plan::{Mode, Plan},
    processing::{ProcessingOptions, ProcessingOutcome, process_image},
};

const MAX_MEMORY_BUDGET: u64 = 2 * 1024 * 1024 * 1024;
const MIN_MEMORY_BUDGET: u64 = 64 * 1024 * 1024;
const ESTIMATED_BYTES_PER_PIXEL: u64 = 32;

#[derive(Debug)]
pub enum EntryOutcome {
    Reduced {
        source_bytes: u64,
        output_bytes: u64,
        bytes_saved: u64,
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

#[derive(Debug)]
pub struct ProcessedEntry {
    pub relative_path: PathBuf,
    pub outcome: EntryOutcome,
}

pub struct RunOptions {
    pub jobs: Option<usize>,
    pub processing: ProcessingOptions,
    pub show_progress: bool,
}

pub fn install_cancellation_handler() -> Result<Arc<AtomicBool>> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&cancelled);
    ctrlc::set_handler(move || {
        signal_flag.store(true, Ordering::SeqCst);
    })
    .map_err(|error| Error::InvalidArgument(format!("cannot install signal handler: {error}")))?;
    Ok(cancelled)
}

pub fn available_memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let available_kib = meminfo.lines().find_map(|line| {
        let value = line.strip_prefix("MemAvailable:")?;
        value.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    available_kib.checked_mul(1024)
}

pub fn default_max_pixels() -> u64 {
    available_memory_bytes()
        .map(|bytes| bytes / ESTIMATED_BYTES_PER_PIXEL)
        .unwrap_or(crate::inspection::DEFAULT_MAX_PIXELS)
        .clamp(1_000_000, crate::inspection::DEFAULT_MAX_PIXELS)
}

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
    let completed = AtomicUsize::new(0);
    let progress_lock = Mutex::new(());

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
                        } else {
                            if !details
                                .source_fingerprint
                                .matches_path(&source)
                                .unwrap_or(false)
                            {
                                EntryOutcome::Failed {
                                    error: "source changed while its reduction was being built"
                                        .to_owned(),
                                }
                            } else {
                                match publish(plan, relative_path, &source, &candidate.bytes) {
                                    Ok(()) => EntryOutcome::Reduced {
                                        source_bytes: candidate.source_bytes,
                                        output_bytes: candidate.bytes.len() as u64,
                                        bytes_saved: candidate.bytes_saved(),
                                    },
                                    Err(error) => EntryOutcome::Failed {
                                        error: error.to_string(),
                                    },
                                }
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
                let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                if options.show_progress {
                    let _guard = progress_lock
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    eprintln!(
                        "[{done}/{}] {}: {}",
                        eligible.len(),
                        relative_path.display(),
                        outcome.kind()
                    );
                }
                ProcessedEntry {
                    relative_path: relative_path.clone(),
                    outcome,
                }
            })
            .collect::<Vec<_>>()
    });
    results.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(results)
}

fn publish(plan: &Plan, relative_path: &Path, source: &Path, bytes: &[u8]) -> Result<()> {
    match &plan.mode {
        Mode::Preserve { output, .. } => {
            let destination = output.join(relative_path);
            let parent = destination.parent().ok_or_else(|| {
                Error::InvalidArgument(format!(
                    "output path has no parent: {}",
                    destination.display()
                ))
            })?;
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            publish_file(source, &destination, bytes, false)?;
        }
        Mode::Replace => publish_file(source, source, bytes, true)?,
    }
    Ok(())
}

fn publish_file(source: &Path, destination: &Path, bytes: &[u8], replace: bool) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        Error::InvalidArgument(format!(
            "destination has no parent: {}",
            destination.display()
        ))
    })?;
    let source_metadata = std::fs::metadata(source).map_err(|error| Error::Io {
        path: source.to_path_buf(),
        source: error,
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|source| Error::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary.write_all(bytes).map_err(|source| Error::Io {
        path: temporary.path().to_path_buf(),
        source,
    })?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(source_metadata.permissions().mode()))
        .map_err(|source| Error::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    filetime::set_file_mtime(
        temporary.path(),
        FileTime::from_last_modification_time(&source_metadata),
    )
    .map_err(|source| Error::Io {
        path: temporary.path().to_path_buf(),
        source,
    })?;

    if replace
        && let Err(error) = fchown(
            temporary.as_file(),
            Some(Uid::from_raw(source_metadata.uid())),
            Some(Gid::from_raw(source_metadata.gid())),
        )
        && error != rustix::io::Errno::PERM
    {
        return Err(Error::Io {
            path: temporary.path().to_path_buf(),
            source: error.into(),
        });
    }
    temporary.as_file().sync_all().map_err(|source| Error::Io {
        path: temporary.path().to_path_buf(),
        source,
    })?;
    if replace {
        temporary.persist(destination).map_err(|error| Error::Io {
            path: destination.to_path_buf(),
            source: error.error,
        })?;
    } else {
        temporary
            .persist_noclobber(destination)
            .map_err(|error| Error::Io {
                path: destination.to_path_buf(),
                source: error.error,
            })?;
    }
    sync_directory(parent).map_err(|source| Error::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

struct MemoryBudget {
    limit: u64,
    used: Mutex<u64>,
    available: Condvar,
}

impl MemoryBudget {
    fn new(limit: u64) -> Self {
        Self {
            limit,
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

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
    use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicBool};

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
}
