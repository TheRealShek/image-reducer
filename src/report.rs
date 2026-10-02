//! Human and JSON reports for discovery, processing, and run summaries.

use image_reducer::{
    discovery::Discovery,
    inspection::{Classification, ClassificationKind, Dimensions, InspectedEntry},
    plan::{Mode, Plan},
    runner::{EntryOutcome, ProcessedEntry},
};
use serde_json::json;

/// Shows target bounds, eligible count, and the preservation/replacement policy.
pub(super) fn print_pre_run_plan(plan: &Plan, eligible: usize) {
    eprintln!("Plan: {}", plan.source.display());
    eprintln!("Target bounds: {} (orientation-aware)", plan.bounds);
    eprintln!("Eligible images: {eligible}");
    eprintln!(
        "Metadata: retain capture date and ICC profile; remove other metadata, including GPS. Use preservation mode if you need the source metadata."
    );
    match &plan.mode {
        Mode::Preserve { output, .. } => {
            eprintln!("Output: {} (sources remain unchanged)", output.display());
        }
        Mode::Replace => eprintln!(
            "Policy: each verified reduction permanently replaces its higher-resolution source."
        ),
    }
}

/// Prints reconciled totals and actionable per-file outcomes for a human reader.
pub(super) fn print_human_report(
    plan: &Plan,
    discovery: &Discovery,
    inspected: &[InspectedEntry],
    processed: &[ProcessedEntry],
    dry_run: bool,
) {
    let summary = Summary::new(discovery, inspected, processed, dry_run);
    println!("Source: {}", plan.source.display());
    println!("Target bounds: {} (orientation-aware)", plan.bounds);
    match &plan.mode {
        Mode::Preserve { output, .. } => println!("Output: {}", output.display()),
        Mode::Replace => println!("Mode: replace sources after verification"),
    }
    println!("Reduced images: {}", summary.reduced);
    println!("Within-bounds images: {}", summary.within_bounds);
    println!("Skipped entries: {}", summary.skipped);
    println!("Failed images: {}", summary.failed);
    if summary.interrupted > 0 {
        println!("Interrupted images: {}", summary.interrupted);
    }
    println!("Reduced source bytes: {}", summary.source_bytes);
    println!("Reduced output bytes: {}", summary.output_bytes);
    println!("Total bytes saved: {}", summary.bytes_saved);
    if dry_run {
        println!("Preview only; no files were changed.");
    }

    for skipped in &discovery.skipped {
        println!(
            "Skipped {}: {}",
            skipped.relative_path.display(),
            skipped.reason.as_str()
        );
    }
    for entry in inspected {
        match &entry.classification {
            Classification::Eligible(details) if dry_run => {
                println!(
                    "Would reduce {}: {} {} -> {}",
                    entry.relative_path.display(),
                    details.format,
                    details.displayed_dimensions,
                    details.output_dimensions
                );
                print_extension_warning(entry, details.extension_warning.as_deref());
            }
            Classification::Eligible(details) => {
                print_extension_warning(entry, details.extension_warning.as_deref());
            }
            Classification::WithinBounds(details) => {
                println!(
                    "Untouched {}: {} is within bounds",
                    entry.relative_path.display(),
                    details.displayed_dimensions
                );
                print_extension_warning(entry, details.extension_warning.as_deref());
            }
            Classification::Skipped { reason } => {
                println!("Skipped {}: {reason}", entry.relative_path.display());
            }
            Classification::Failed { error } => {
                eprintln!("Failed {}: {error}", entry.relative_path.display());
            }
        }
    }
    for entry in processed {
        match &entry.outcome {
            EntryOutcome::Reduced {
                source_bytes,
                output_bytes,
                bytes_saved,
                warnings,
            } => {
                println!(
                    "Reduced {}: {source_bytes} -> {output_bytes} bytes ({bytes_saved} saved)",
                    entry.relative_path.display()
                );
                for warning in warnings {
                    println!("Warning for {}: {warning}", entry.relative_path.display());
                }
            }
            EntryOutcome::NotBeneficial {
                source_bytes,
                candidate_bytes,
            } => println!(
                "Untouched {}: candidate was not smaller ({candidate_bytes} >= {source_bytes} bytes)",
                entry.relative_path.display()
            ),
            EntryOutcome::FidelityConflict { reason } => println!(
                "Skipped {}: fidelity conflict: {reason}",
                entry.relative_path.display()
            ),
            EntryOutcome::Failed { error } => {
                eprintln!("Failed {}: {error}", entry.relative_path.display());
            }
            EntryOutcome::Interrupted => {
                eprintln!(
                    "Interrupted before publishing {}",
                    entry.relative_path.display()
                );
            }
        }
    }
    for failure in &discovery.failures {
        eprintln!(
            "Failed to access {}: {}",
            failure.relative_path.display(),
            failure.error
        );
    }
}

/// Prints a filename/content mismatch warning when inspection recorded one.
fn print_extension_warning(entry: &InspectedEntry, warning: Option<&str>) {
    if let Some(warning) = warning {
        println!("Warning for {}: {warning}", entry.relative_path.display());
    }
}

/// Prints the structured plan, per-file outcomes, and reconciled summary.
pub(super) fn print_json_report(
    plan: &Plan,
    discovery: &Discovery,
    inspected: &[InspectedEntry],
    processed: &[ProcessedEntry],
    dry_run: bool,
) -> image_reducer::Result<()> {
    let summary = Summary::new(discovery, inspected, processed, dry_run);
    let mode = match &plan.mode {
        Mode::Preserve { output, inferred } => json!({
            "kind": "preserve",
            "output": output,
            "output_inferred": inferred,
        }),
        Mode::Replace => json!({ "kind": "replace" }),
    };
    let skipped: Vec<_> = discovery
        .skipped
        .iter()
        .map(|entry| json!({ "path": entry.relative_path, "reason": entry.reason.as_str() }))
        .collect();
    let access_failures: Vec<_> = discovery
        .failures
        .iter()
        .map(|failure| json!({ "path": failure.relative_path, "error": failure.error }))
        .collect();
    let images: Vec<_> = inspected.iter().map(inspection_json).collect();
    let processing: Vec<_> = processed.iter().map(processed_json).collect();
    let report = json!({
        "source": plan.source,
        "target_bounds": {
            "landscape_width": plan.bounds.width,
            "landscape_height": plan.bounds.height,
        },
        "mode": mode,
        "dry_run": dry_run,
        "discovery": {
            "skipped": skipped,
            "failures": access_failures,
        },
        "images": images,
        "processing": processing,
        "summary": {
            "reduced": summary.reduced,
            "within_bounds": summary.within_bounds,
            "skipped": summary.skipped,
            "failed": summary.failed,
            "interrupted": summary.interrupted,
            "source_bytes": summary.source_bytes,
            "output_bytes": summary.output_bytes,
            "bytes_saved": summary.bytes_saved,
        },
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Serializes inspected properties or a skip/failure without altering classification.
fn inspection_json(entry: &InspectedEntry) -> serde_json::Value {
    match &entry.classification {
        Classification::Eligible(details) | Classification::WithinBounds(details) => json!({
            "path": entry.relative_path,
            "classification": entry.classification.kind().as_str(),
            "format": details.format.as_str(),
            "encoded_dimensions": dimensions_json(details.encoded_dimensions),
            "displayed_dimensions": dimensions_json(details.displayed_dimensions),
            "planned_dimensions": dimensions_json(details.output_dimensions),
            "orientation": details.orientation,
            "color_type": details.color_type,
            "source_bytes": details.source_bytes,
            "warning": details.extension_warning,
        }),
        Classification::Skipped { reason } => json!({
            "path": entry.relative_path,
            "classification": "skipped",
            "reason": reason,
        }),
        Classification::Failed { error } => json!({
            "path": entry.relative_path,
            "classification": "failed",
            "error": error,
        }),
    }
}

/// Serializes a processing/publication outcome and its available report details.
fn processed_json(entry: &ProcessedEntry) -> serde_json::Value {
    match &entry.outcome {
        EntryOutcome::Reduced {
            source_bytes,
            output_bytes,
            bytes_saved,
            warnings,
        } => json!({
            "path": entry.relative_path,
            "outcome": "reduced",
            "source_bytes": source_bytes,
            "output_bytes": output_bytes,
            "bytes_saved": bytes_saved,
            "warnings": warnings,
        }),
        EntryOutcome::NotBeneficial {
            source_bytes,
            candidate_bytes,
        } => json!({
            "path": entry.relative_path,
            "outcome": "not_beneficial",
            "source_bytes": source_bytes,
            "candidate_bytes": candidate_bytes,
        }),
        EntryOutcome::FidelityConflict { reason } => json!({
            "path": entry.relative_path,
            "outcome": "fidelity_conflict",
            "reason": reason,
        }),
        EntryOutcome::Failed { error } => json!({
            "path": entry.relative_path,
            "outcome": "failed",
            "error": error,
        }),
        EntryOutcome::Interrupted => json!({
            "path": entry.relative_path,
            "outcome": "interrupted",
        }),
    }
}

/// Serializes pixel dimensions using the public report field names.
fn dimensions_json(dimensions: Dimensions) -> serde_json::Value {
    json!({ "width": dimensions.width, "height": dimensions.height })
}

/// Counts inspected entries in a selected category.
pub(super) fn count_classification(
    inspected: &[InspectedEntry],
    kind: ClassificationKind,
) -> usize {
    inspected
        .iter()
        .filter(|entry| entry.classification.kind() == kind)
        .count()
}

/// Reports access failures, inspection/processing failures, or interrupted work.
pub(super) fn has_failures(
    discovery: &Discovery,
    inspected: &[InspectedEntry],
    processed: &[ProcessedEntry],
) -> bool {
    !discovery.failures.is_empty()
        || inspected
            .iter()
            .any(|entry| matches!(entry.classification, Classification::Failed { .. }))
        || processed.iter().any(|entry| {
            matches!(
                entry.outcome,
                EntryOutcome::Failed { .. } | EntryOutcome::Interrupted
            )
        })
}

/// Totals shared by human and JSON reports.
#[derive(Default)]
struct Summary {
    reduced: usize,
    within_bounds: usize,
    skipped: usize,
    failed: usize,
    interrupted: usize,
    source_bytes: u64,
    output_bytes: u64,
    bytes_saved: u64,
}

impl Summary {
    /// Reconciles discovery and processing totals, excluding unperformed work in preview mode.
    fn new(
        discovery: &Discovery,
        inspected: &[InspectedEntry],
        processed: &[ProcessedEntry],
        dry_run: bool,
    ) -> Self {
        let mut summary = Self {
            within_bounds: count_classification(inspected, ClassificationKind::WithinBounds),
            skipped: discovery.skipped.len()
                + count_classification(inspected, ClassificationKind::Skipped),
            failed: discovery.failures.len()
                + count_classification(inspected, ClassificationKind::Failed),
            ..Self::default()
        };
        if dry_run {
            return summary;
        }
        for entry in processed {
            match entry.outcome {
                EntryOutcome::Reduced {
                    source_bytes,
                    output_bytes,
                    bytes_saved,
                    ..
                } => {
                    summary.reduced += 1;
                    summary.source_bytes += source_bytes;
                    summary.output_bytes += output_bytes;
                    summary.bytes_saved += bytes_saved;
                }
                EntryOutcome::NotBeneficial { .. } | EntryOutcome::FidelityConflict { .. } => {
                    summary.skipped += 1;
                }
                EntryOutcome::Failed { .. } => summary.failed += 1,
                EntryOutcome::Interrupted => summary.interrupted += 1,
            }
        }
        summary
    }
}
