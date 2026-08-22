use std::process::ExitCode;

use clap::Parser;
use image_reducer::{
    cli::Cli,
    discovery::{Discovery, discover},
    inspection::{Classification, DEFAULT_MAX_PIXELS, InspectedEntry, inspect_files},
    plan::{Mode, Plan},
};
use serde_json::json;

fn main() -> ExitCode {
    match run() {
        Ok(has_failures) if has_failures => ExitCode::FAILURE,
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> image_reducer::Result<bool> {
    let cli = Cli::parse();
    let plan = Plan::from_cli(&cli)?;
    let discovery = discover(&plan.source, &plan.exclusions);
    let inspected = inspect_files(
        &plan.source,
        &discovery.files,
        plan.bounds,
        cli.max_pixels
            .map_or(DEFAULT_MAX_PIXELS, std::num::NonZeroU64::get),
    );

    if cli.json {
        print_json_report(&plan, &discovery, &inspected, cli.dry_run)?;
    } else {
        print_human_report(&plan, &discovery, &inspected, cli.dry_run);
    }

    Ok(!discovery.failures.is_empty()
        || inspected
            .iter()
            .any(|entry| matches!(entry.classification, Classification::Failed { .. })))
}

fn print_human_report(
    plan: &Plan,
    discovery: &Discovery,
    inspected: &[InspectedEntry],
    dry_run: bool,
) {
    let eligible = count(inspected, "eligible");
    let within_bounds = count(inspected, "within_bounds");
    let skipped_images = count(inspected, "skipped");
    let failed_images = count(inspected, "failed");

    println!("Source: {}", plan.source.display());
    println!("Target bounds: {} (orientation-aware)", plan.bounds);
    match &plan.mode {
        Mode::Preserve { output, .. } => println!("Output: {}", output.display()),
        Mode::Replace => println!("Mode: replace sources after verification"),
    }
    println!("Eligible images: {eligible}");
    println!("Within-bounds images: {within_bounds}");
    println!(
        "Skipped entries: {}",
        discovery.skipped.len() + skipped_images
    );
    println!("Failures: {}", discovery.failures.len() + failed_images);
    if dry_run {
        println!("Preview only; no files were changed.");
    } else {
        println!("Image classification and processing are not implemented yet.");
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
            Classification::Eligible(details) => {
                println!(
                    "Would reduce {}: {} {} -> {}",
                    entry.relative_path.display(),
                    details.format,
                    details.displayed_dimensions,
                    details.output_dimensions
                );
                if let Some(warning) = &details.extension_warning {
                    println!("Warning for {}: {warning}", entry.relative_path.display());
                }
            }
            Classification::WithinBounds(details) => {
                println!(
                    "Untouched {}: {} is within bounds",
                    entry.relative_path.display(),
                    details.displayed_dimensions
                );
                if let Some(warning) = &details.extension_warning {
                    println!("Warning for {}: {warning}", entry.relative_path.display());
                }
            }
            Classification::Skipped { reason } => {
                println!("Skipped {}: {reason}", entry.relative_path.display());
            }
            Classification::Failed { error } => {
                eprintln!("Failed {}: {error}", entry.relative_path.display());
            }
        }
    }
    for failure in &discovery.failures {
        eprintln!(
            "failed to access {}: {}",
            failure.relative_path.display(),
            failure.error
        );
    }
}

fn print_json_report(
    plan: &Plan,
    discovery: &Discovery,
    inspected: &[InspectedEntry],
    dry_run: bool,
) -> image_reducer::Result<()> {
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
        .map(|entry| {
            json!({
                "path": entry.relative_path,
                "reason": entry.reason.as_str(),
            })
        })
        .collect();
    let failures: Vec<_> = discovery
        .failures
        .iter()
        .map(|failure| {
            json!({
                "path": failure.relative_path,
                "error": failure.error,
            })
        })
        .collect();
    let images: Vec<_> = inspected
        .iter()
        .map(|entry| match &entry.classification {
            Classification::Eligible(details) | Classification::WithinBounds(details) => json!({
                "path": entry.relative_path,
                "classification": entry.classification.kind(),
                "format": details.format.as_str(),
                "encoded_dimensions": dimensions_json(details.encoded_dimensions),
                "displayed_dimensions": dimensions_json(details.displayed_dimensions),
                "output_dimensions": dimensions_json(details.output_dimensions),
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
        })
        .collect();
    let report = json!({
        "source": plan.source,
        "target_bounds": {
            "landscape_width": plan.bounds.width,
            "landscape_height": plan.bounds.height,
        },
        "mode": mode,
        "dry_run": dry_run,
        "processing_implemented": false,
        "discovery": {
            "files": discovery.files,
            "skipped": skipped,
            "failures": failures,
        },
        "images": images,
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn count(inspected: &[InspectedEntry], kind: &str) -> usize {
    inspected
        .iter()
        .filter(|entry| entry.classification.kind() == kind)
        .count()
}

fn dimensions_json(dimensions: image_reducer::inspection::Dimensions) -> serde_json::Value {
    json!({
        "width": dimensions.width,
        "height": dimensions.height,
    })
}
