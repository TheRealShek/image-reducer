use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use walkdir::{DirEntry, WalkDir};

#[derive(Debug, Eq, PartialEq)]
pub enum SkipReason {
    SymbolicLink,
    ExcludedDirectory,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SymbolicLink => "symbolic link",
            Self::ExcludedDirectory => "excluded directory",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct SkippedEntry {
    pub relative_path: PathBuf,
    pub reason: SkipReason,
}

#[derive(Debug, Eq, PartialEq)]
pub struct AccessFailure {
    pub relative_path: PathBuf,
    pub error: String,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Discovery {
    pub files: Vec<PathBuf>,
    pub skipped: Vec<SkippedEntry>,
    pub failures: Vec<AccessFailure>,
}

pub fn discover(source: &Path, exclusions: &[PathBuf]) -> Discovery {
    let exclusions: HashSet<&Path> = exclusions.iter().map(PathBuf::as_path).collect();
    let mut result = Discovery {
        skipped: exclusions
            .iter()
            .map(|relative_path| SkippedEntry {
                relative_path: relative_path.to_path_buf(),
                reason: SkipReason::ExcludedDirectory,
            })
            .collect(),
        ..Discovery::default()
    };

    let walker = WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| should_descend(entry, source, &exclusions));

    for entry in walker {
        match entry {
            Ok(entry) if entry.depth() == 0 => {}
            Ok(entry) => {
                let relative = entry
                    .path()
                    .strip_prefix(source)
                    .expect("walkdir entry remains beneath source")
                    .to_path_buf();
                if entry.file_type().is_symlink() {
                    result.skipped.push(SkippedEntry {
                        relative_path: relative,
                        reason: SkipReason::SymbolicLink,
                    });
                } else if entry.file_type().is_file() {
                    result.files.push(relative);
                }
            }
            Err(error) => {
                let path = error
                    .path()
                    .and_then(|path| path.strip_prefix(source).ok())
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf();
                result.failures.push(AccessFailure {
                    relative_path: path,
                    error: error.to_string(),
                });
            }
        }
    }

    result.files.sort();
    result
        .skipped
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    result
        .failures
        .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    result
}

fn should_descend(entry: &DirEntry, source: &Path, exclusions: &HashSet<&Path>) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let Ok(relative) = entry.path().strip_prefix(source) else {
        return false;
    };
    !(entry.file_type().is_dir() && exclusions.contains(relative))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn discovers_hidden_and_nested_files_while_pruning_exclusions() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("nested")).unwrap();
        std::fs::create_dir_all(source.path().join("excluded/deep")).unwrap();
        std::fs::write(source.path().join(".hidden"), b"image").unwrap();
        std::fs::write(source.path().join("nested/photo.jpg"), b"image").unwrap();
        std::fs::write(source.path().join("excluded/deep/photo.png"), b"image").unwrap();

        let result = discover(source.path(), &[PathBuf::from("excluded")]);

        assert_eq!(
            result.files,
            [PathBuf::from(".hidden"), PathBuf::from("nested/photo.jpg")]
        );
        assert_eq!(
            result.skipped,
            [SkippedEntry {
                relative_path: PathBuf::from("excluded"),
                reason: SkipReason::ExcludedDirectory,
            }]
        );
    }

    #[test]
    fn reports_file_and_directory_symbolic_links_without_following_them() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.jpg"), b"image").unwrap();
        symlink(outside.path(), source.path().join("linked-directory")).unwrap();
        symlink(
            outside.path().join("outside.jpg"),
            source.path().join("linked-file.jpg"),
        )
        .unwrap();

        let result = discover(source.path(), &[]);

        assert!(result.files.is_empty());
        assert_eq!(result.skipped.len(), 2);
        assert!(
            result
                .skipped
                .iter()
                .all(|entry| entry.reason == SkipReason::SymbolicLink)
        );
    }

    #[test]
    fn reports_nested_exclusions_once_even_when_parent_is_pruned() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(source.path().join("excluded/nested")).unwrap();

        let result = discover(
            source.path(),
            &[
                PathBuf::from("excluded"),
                PathBuf::from("excluded/nested"),
                PathBuf::from("excluded"),
            ],
        );

        assert_eq!(result.skipped.len(), 2);
        assert_eq!(result.skipped[0].relative_path, PathBuf::from("excluded"));
        assert_eq!(
            result.skipped[1].relative_path,
            PathBuf::from("excluded/nested")
        );
    }
}
