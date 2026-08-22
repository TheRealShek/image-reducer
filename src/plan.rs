use std::{
    fmt,
    path::{Path, PathBuf},
};

use crate::{Error, Result, cli::Cli};

pub const DEFAULT_LANDSCAPE_BOUNDS: Bounds = Bounds {
    width: 1920,
    height: 1080,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bounds {
    pub width: u32,
    pub height: u32,
}

impl Bounds {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidArgument(
                "target bounds must be greater than zero".to_owned(),
            ));
        }
        if width < height {
            return Err(Error::InvalidArgument(
                "custom target bounds must be landscape-oriented (WIDTH >= HEIGHT)".to_owned(),
            ));
        }
        Ok(Self { width, height })
    }

    pub fn for_image(self, image_width: u32, image_height: u32) -> Self {
        match image_width.cmp(&image_height) {
            std::cmp::Ordering::Greater => self,
            std::cmp::Ordering::Less => Self {
                width: self.height,
                height: self.width,
            },
            std::cmp::Ordering::Equal => Self {
                width: self.height,
                height: self.height,
            },
        }
    }

    pub fn fitted_dimensions(self, image_width: u32, image_height: u32) -> (u32, u32) {
        let bounds = self.for_image(image_width, image_height);
        if image_width <= bounds.width && image_height <= bounds.height {
            return (image_width, image_height);
        }

        let width_limited = u64::from(image_width) * u64::from(bounds.height)
            > u64::from(image_height) * u64::from(bounds.width);
        if width_limited {
            let height =
                (u64::from(image_height) * u64::from(bounds.width) / u64::from(image_width)) as u32;
            (bounds.width, height.max(1))
        } else {
            let width = (u64::from(image_width) * u64::from(bounds.height)
                / u64::from(image_height)) as u32;
            (width.max(1), bounds.height)
        }
    }
}

impl fmt::Display for Bounds {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Mode {
    Preserve { output: PathBuf, inferred: bool },
    Replace,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Plan {
    pub source: PathBuf,
    pub bounds: Bounds,
    pub mode: Mode,
    pub exclusions: Vec<PathBuf>,
}

impl Plan {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        cli.validate()?;
        let source = std::fs::canonicalize(&cli.source).map_err(|source| Error::Io {
            path: cli.source.clone(),
            source,
        })?;
        let exclusions = validate_exclusions(&source, &cli.exclude)?;

        let mode = if cli.replace {
            Mode::Replace
        } else if let Some(output) = &cli.output {
            Mode::Preserve {
                output: validate_explicit_output(&source, output)?,
                inferred: false,
            }
        } else {
            Mode::Preserve {
                output: infer_output(&source)?,
                inferred: true,
            }
        };

        Ok(Self {
            source,
            bounds: cli.max.unwrap_or(DEFAULT_LANDSCAPE_BOUNDS),
            mode,
            exclusions,
        })
    }
}

fn validate_exclusions(source: &Path, exclusions: &[PathBuf]) -> Result<Vec<PathBuf>> {
    exclusions
        .iter()
        .map(|exclusion| {
            crate::cli::validate_exclusion(exclusion)?;
            let relative = normalize_lexically(exclusion);
            if relative.as_os_str().is_empty() {
                return Err(Error::InvalidArgument(
                    "the source directory itself cannot be excluded".to_owned(),
                ));
            }
            let requested = source.join(&relative);
            let resolved = std::fs::canonicalize(&requested).map_err(|error| {
                Error::InvalidArgument(format!(
                    "cannot resolve excluded directory {}: {error}",
                    exclusion.display()
                ))
            })?;
            if !resolved.starts_with(source) {
                return Err(Error::InvalidArgument(format!(
                    "excluded directory resolves outside the source tree: {}",
                    exclusion.display()
                )));
            }
            if resolved != requested || !resolved.is_dir() {
                return Err(Error::InvalidArgument(format!(
                    "exclusion must name a real directory beneath the source: {}",
                    exclusion.display()
                )));
            }
            Ok(relative)
        })
        .collect()
}

fn validate_explicit_output(source: &Path, output: &Path) -> Result<PathBuf> {
    let output = absolute_path(output)?;
    if output.exists() {
        return Err(Error::InvalidArgument(format!(
            "explicit output destination already exists: {}",
            output.display()
        )));
    }
    let resolved_output = resolve_nonexistent_path(&output)?;
    if resolved_output.starts_with(source) {
        return Err(Error::InvalidArgument(format!(
            "output directory must be outside the source tree: {}",
            output.display()
        )));
    }
    Ok(output)
}

fn resolve_nonexistent_path(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            Error::InvalidArgument(format!(
                "output path has no accessible existing ancestor: {}",
                path.display()
            ))
        })?;
        missing.push(name.to_owned());
        existing = existing.parent().ok_or_else(|| {
            Error::InvalidArgument(format!(
                "output path has no accessible existing ancestor: {}",
                path.display()
            ))
        })?;
    }

    let mut resolved = std::fs::canonicalize(existing).map_err(|source| Error::Io {
        path: existing.to_path_buf(),
        source,
    })?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn infer_output(source: &Path) -> Result<PathBuf> {
    let parent = source.parent().ok_or_else(|| {
        Error::InvalidArgument(
            "cannot infer an output directory for the filesystem root".to_owned(),
        )
    })?;
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            Error::InvalidArgument(format!(
                "source directory name is not valid UTF-8: {}",
                source.display()
            ))
        })?;

    for suffix in 1_u32.. {
        let candidate_name = if suffix == 1 {
            format!("{name}-reduced")
        } else {
            format!("{name}-reduced-{suffix}")
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    unreachable!("the output suffix space cannot be exhausted")
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(normalize_lexically(path));
    }
    let current = std::env::current_dir().map_err(|source| Error::Io {
        path: PathBuf::from("."),
        source,
    })?;
    Ok(normalize_lexically(&current.join(path)))
}

fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn calculates_landscape_portrait_and_square_bounds() {
        let bounds = DEFAULT_LANDSCAPE_BOUNDS;

        assert_eq!(bounds.fitted_dimensions(4000, 3000), (1440, 1080));
        assert_eq!(bounds.fitted_dimensions(3000, 4000), (1080, 1440));
        assert_eq!(bounds.fitted_dimensions(3000, 3000), (1080, 1080));
    }

    #[test]
    fn never_upscales_within_bounds_image() {
        assert_eq!(
            DEFAULT_LANDSCAPE_BOUNDS.fitted_dimensions(800, 600),
            (800, 600)
        );
    }

    #[test]
    fn chooses_next_available_inferred_output() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("pictures");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(parent.path().join("pictures-reduced")).unwrap();
        let cli = Cli::try_parse_from(["image-reducer", source.to_str().unwrap()]).unwrap();

        let plan = Plan::from_cli(&cli).unwrap();

        assert_eq!(
            plan.mode,
            Mode::Preserve {
                output: parent.path().join("pictures-reduced-2"),
                inferred: true,
            }
        );
    }

    #[test]
    fn rejects_explicit_output_inside_source() {
        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("pictures");
        std::fs::create_dir(&source).unwrap();
        let output = source.join("reduced");
        let cli = Cli::try_parse_from([
            "image-reducer",
            source.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .unwrap();

        assert!(Plan::from_cli(&cli).is_err());
    }

    #[test]
    fn rejects_exclusion_that_resolves_outside_source() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("pictures");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, source.join("linked")).unwrap();
        let cli = Cli::try_parse_from([
            "image-reducer",
            source.to_str().unwrap(),
            "--exclude",
            "linked",
        ])
        .unwrap();

        assert!(Plan::from_cli(&cli).is_err());
    }

    #[test]
    fn rejects_excluding_source_itself() {
        let source = tempfile::tempdir().unwrap();
        let cli = Cli::try_parse_from([
            "image-reducer",
            source.path().to_str().unwrap(),
            "--exclude",
            ".",
        ])
        .unwrap();

        assert!(Plan::from_cli(&cli).is_err());
    }

    #[test]
    fn rejects_output_that_enters_source_through_symlink() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("pictures");
        let alias = parent.path().join("alias");
        std::fs::create_dir(&source).unwrap();
        symlink(&source, &alias).unwrap();
        let output = alias.join("reduced");
        let cli = Cli::try_parse_from([
            "image-reducer",
            source.to_str().unwrap(),
            "--output",
            output.to_str().unwrap(),
        ])
        .unwrap();

        assert!(Plan::from_cli(&cli).is_err());
    }
}
