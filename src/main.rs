use std::process::ExitCode;

use clap::Parser;
use image_reducer::{
    cli::Cli,
    discovery::{Discovery, discover},
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

    if cli.json {
        print_json_report(&plan, &discovery, cli.dry_run)?;
    } else {
        print_human_report(&plan, &discovery, cli.dry_run);
    }

    Ok(!discovery.failures.is_empty())
}

fn print_human_report(plan: &Plan, discovery: &Discovery, dry_run: bool) {
    println!("Source: {}", plan.source.display());
    println!("Target bounds: {} (orientation-aware)", plan.bounds);
    match &plan.mode {
        Mode::Preserve { output, .. } => println!("Output: {}", output.display()),
        Mode::Replace => println!("Mode: replace sources after verification"),
    }
    println!("Discovered files: {}", discovery.files.len());
    println!("Skipped entries: {}", discovery.skipped.len());
    println!("Access failures: {}", discovery.failures.len());
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
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
