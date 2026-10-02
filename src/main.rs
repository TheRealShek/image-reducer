//! CLI startup, replacement confirmation, and run orchestration.

use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

use clap::Parser;
use image_reducer::{
    cli::Cli,
    discovery::discover,
    inspection::{ClassificationKind, inspect_files},
    plan::{Mode, Plan},
    processing::{DEFAULT_JPEG_QUALITY, ProcessingOptions},
    runner::{RunOptions, default_max_pixels, execute, install_cancellation_handler},
};
mod report;

use report::{
    count_classification, has_failures, print_human_report, print_json_report, print_pre_run_plan,
};

/// Maps run-wide errors and per-file failures to the process exit status.
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

/// Parses options, plans work, processes eligible images, and prints the final report.
fn run() -> image_reducer::Result<bool> {
    let cli = Cli::parse();
    let plan = Plan::from_cli(&cli)?;
    let discovery = discover(&plan.source, &plan.exclusions);
    let max_pixels = cli
        .max_pixels
        .map_or_else(default_max_pixels, std::num::NonZeroU64::get);
    let inspected = inspect_files(&plan.source, &discovery.files, plan.bounds, max_pixels);
    let eligible = count_classification(&inspected, ClassificationKind::Eligible);

    if !cli.dry_run && eligible > 0 {
        print_pre_run_plan(&plan, eligible);
    }

    if !cli.dry_run && matches!(plan.mode, Mode::Replace) && eligible > 0 && !cli.yes {
        confirm_replacement()?;
    }
    if let Some(quality) = cli
        .quality
        .filter(|&quality| quality < DEFAULT_JPEG_QUALITY)
    {
        eprintln!(
            "warning: JPEG quality {quality} is below the high-fidelity default of {DEFAULT_JPEG_QUALITY}"
        );
    }

    let cancelled = install_cancellation_handler()?;
    let processed = if cli.dry_run {
        Vec::new()
    } else {
        execute(
            &plan,
            &inspected,
            RunOptions {
                jobs: cli.jobs.map(std::num::NonZeroUsize::get),
                processing: ProcessingOptions {
                    jpeg_quality: cli.quality.unwrap_or(DEFAULT_JPEG_QUALITY),
                },
                show_progress: !cli.json && io::stderr().is_terminal(),
            },
            cancelled,
        )?
    };

    if cli.json {
        print_json_report(&plan, &discovery, &inspected, &processed, cli.dry_run)?;
    } else {
        print_human_report(&plan, &discovery, &inspected, &processed, cli.dry_run);
    }

    Ok(has_failures(&discovery, &inspected, &processed))
}

/// Requires an explicit affirmative response before irreversible source replacement.
fn confirm_replacement() -> image_reducer::Result<()> {
    eprint!("Continue? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|source| image_reducer::Error::Io {
            path: "stderr".into(),
            source,
        })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|source| image_reducer::Error::Io {
            path: "stdin".into(),
            source,
        })?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Err(image_reducer::Error::InvalidArgument(
            "replacement was not confirmed".to_owned(),
        ));
    }
    Ok(())
}
