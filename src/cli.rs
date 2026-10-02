//! Typed command-line options and source/exclusion validation.

use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    str::FromStr,
};

use clap::{Parser, ValueHint};

use crate::{Error, Result, plan::Bounds};

/// Command-line options for one source tree and its reduction policy.
#[derive(Debug, Parser)]
#[command(
    name = "image-reducer",
    version,
    about = "Safely downscale oversized images without changing sources by default",
    long_about = "Safely downscale oversized images without changing sources by default.\n\nThe plain command recursively finds supported images, uses orientation-aware 1920x1080 bounds, and writes only verified, smaller results to a new sibling directory such as SOURCE-reduced. Files that are within bounds are left untouched and are not copied.\n\nSupported formats: JPEG, PNG, WebP, BMP, single-page TIFF, and single-frame GIF.",
    after_help = "COMMON WORKFLOWS:\n  Preview only (never writes):\n    image-reducer PHOTOS --dry-run\n\n  Preserve sources (default):\n    image-reducer PHOTOS\n    Writes reductions to a new sibling such as PHOTOS-reduced.\n\n  Use custom bounds and output directory:\n    image-reducer PHOTOS --max 2560x1440 --output /path/to/NEW_OUTPUT\n\n  Replace sources (irreversible; asks for confirmation):\n    image-reducer PHOTOS --replace\n\nSAFETY:\n  Start with --dry-run. The default mode never changes source files.\n  --replace permanently discards each higher-resolution source only after its\n  reduction is verified and durably published. Use --yes only for automation."
)]
pub struct Cli {
    /// Directory to search recursively for supported images
    #[arg(value_hint = ValueHint::DirPath)]
    pub source: PathBuf,

    /// Landscape bounds; portrait uses rotated bounds [default: 1920x1080]
    #[arg(long, value_name = "WIDTHxHEIGHT")]
    pub max: Option<Bounds>,

    /// New output directory outside SOURCE; must not already exist
    #[arg(long, value_hint = ValueHint::DirPath, conflicts_with = "replace")]
    pub output: Option<PathBuf>,

    /// Irreversibly replace each source after its reduction is verified
    #[arg(long, conflicts_with = "output")]
    pub replace: bool,

    /// Exclude a source-relative directory and subtree; may be repeated
    #[arg(long, value_name = "RELATIVE_DIRECTORY", value_hint = ValueHint::DirPath)]
    pub exclude: Vec<PathBuf>,

    /// JPEG quality from 1 to 100 [default: 92; ignored by other formats]
    #[arg(
        long,
        value_name = "1-100",
        value_parser = clap::value_parser!(u8).range(1..=100)
    )]
    pub quality: Option<u8>,

    /// Require supported metadata retention; skip images that cannot retain it
    #[arg(long)]
    pub preserve_all_metadata: bool,

    /// Discover and report planned work without writing anything
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the --replace confirmation prompt; intended for automation
    #[arg(long, requires = "replace")]
    pub yes: bool,

    /// Worker limit [default: automatic from available CPUs]
    #[arg(long)]
    pub jobs: Option<NonZeroUsize>,

    /// Override the automatic memory-based decoded-pixel safety limit
    #[arg(long, value_name = "PIXELS")]
    pub max_pixels: Option<NonZeroU64>,

    /// Emit the final machine-readable report as JSON on stdout
    #[arg(long)]
    pub json: bool,
}

impl Cli {
    /// Rejects sources that are not real directories.
    pub fn validate(&self) -> Result<()> {
        let metadata = std::fs::metadata(&self.source).map_err(|source| Error::Io {
            path: self.source.clone(),
            source,
        })?;
        if !metadata.is_dir() {
            return Err(Error::InvalidArgument(format!(
                "source is not a directory: {}",
                self.source.display()
            )));
        }

        if self.source.is_symlink() {
            return Err(Error::InvalidArgument(format!(
                "source directory must not be a symbolic link: {}",
                self.source.display()
            )));
        }

        Ok(())
    }
}

/// Rejects empty, absolute, or escaping source-relative exclusions.
pub(crate) fn validate_exclusion(path: &std::path::Path) -> Result<()> {
    use std::path::Component;

    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(Error::InvalidArgument(format!(
            "exclusion must be a non-empty source-relative path: {}",
            path.display()
        )));
    }

    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(Error::InvalidArgument(format!(
            "exclusion must remain inside the source directory: {}",
            path.display()
        )));
    }

    Ok(())
}

impl FromStr for Bounds {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (width, height) = value
            .split_once(['x', 'X', '×'])
            .ok_or_else(|| "expected WIDTHxHEIGHT".to_owned())?;
        let width = width
            .parse::<u32>()
            .map_err(|_| "width must be a positive integer".to_owned())?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "height must be a positive integer".to_owned())?;
        Bounds::new(width, height).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[test]
    fn parses_complete_command_line() {
        let cli = Cli::try_parse_from([
            "image-reducer",
            "/pictures",
            "--max",
            "2560x1440",
            "--exclude",
            "cache/thumbnails",
            "--quality",
            "90",
            "--jobs",
            "4",
            "--max-pixels",
            "50000000",
            "--dry-run",
            "--json",
        ])
        .unwrap();

        assert_eq!(cli.max, Some(Bounds::new(2560, 1440).unwrap()));
        assert_eq!(cli.exclude, [PathBuf::from("cache/thumbnails")]);
        assert_eq!(cli.jobs.unwrap().get(), 4);
        assert_eq!(cli.max_pixels.unwrap().get(), 50_000_000);
        assert!(cli.dry_run);
        assert!(cli.json);
    }

    #[test]
    fn rejects_portrait_custom_bounds() {
        let error =
            Cli::try_parse_from(["image-reducer", "/pictures", "--max", "800x1200"]).unwrap_err();

        assert!(error.to_string().contains("landscape-oriented"));
    }

    #[test]
    fn rejects_output_with_replacement() {
        assert!(
            Cli::try_parse_from([
                "image-reducer",
                "/pictures",
                "--output",
                "/reduced",
                "--replace"
            ])
            .is_err()
        );
    }

    #[test]
    fn rejects_parent_directory_exclusion() {
        assert!(validate_exclusion(std::path::Path::new("../elsewhere")).is_err());
    }

    #[test]
    fn help_explains_defaults_safety_and_common_workflows() {
        let help = Cli::command().render_long_help().to_string();

        for expected in [
            "without changing sources by default",
            "orientation-aware 1920x1080 bounds",
            "Supported formats: JPEG, PNG, WebP, BMP, single-page TIFF, and single-frame GIF",
            "image-reducer PHOTOS --dry-run",
            "image-reducer PHOTOS --replace",
            "--replace permanently discards each higher-resolution source",
            "default: 92; ignored by other formats",
            "must not already exist",
        ] {
            assert!(help.contains(expected), "help is missing: {expected}");
        }
    }
}
