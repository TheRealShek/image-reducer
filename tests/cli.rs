use std::{io::Write, os::unix::fs::PermissionsExt, process::Command};

use image::{GenericImageView, ImageFormat, Rgb, RgbImage};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_image-reducer"))
}

fn write_source(path: &std::path::Path) {
    let image = RgbImage::from_fn(300, 150, |x, y| {
        Rgb([
            x.wrapping_mul(17).wrapping_add(y.wrapping_mul(3)) as u8,
            x.wrapping_mul(5).wrapping_add(y.wrapping_mul(13)) as u8,
            x.wrapping_mul(23).wrapping_add(y.wrapping_mul(29)) as u8,
        ])
    });
    image.save_with_format(path, ImageFormat::Png).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o640)).unwrap();
}

#[test]
fn preservation_mode_reduces_only_eligible_images() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    let output = parent.path().join("reduced");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    write_source(&source.join("nested/photo.png"));
    std::fs::write(source.join("notes.txt"), b"not an image").unwrap();

    let result = binary()
        .args([
            source.to_str().unwrap(),
            "--max",
            "60x30",
            "--output",
            output.to_str().unwrap(),
            "--json",
            "--jobs",
            "2",
        ])
        .output()
        .unwrap();

    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["summary"]["reduced"], 1);
    assert_eq!(report["summary"]["skipped"], 1);
    assert_eq!(
        image::open(output.join("nested/photo.png"))
            .unwrap()
            .dimensions(),
        (60, 30)
    );
    assert!(!output.join("notes.txt").exists());
    assert_eq!(
        std::fs::metadata(output.join("nested/photo.png"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    assert_eq!(
        image::open(source.join("nested/photo.png"))
            .unwrap()
            .dimensions(),
        (300, 150)
    );
}

#[test]
fn dry_run_does_not_create_output() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    let output = parent.path().join("reduced");
    std::fs::create_dir(&source).unwrap();
    write_source(&source.join("photo.png"));

    let result = binary()
        .args([
            source.to_str().unwrap(),
            "--max",
            "60x30",
            "--output",
            output.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(result.status.success());
    assert!(!output.exists());
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["images"][0]["classification"], "eligible");
    assert_eq!(report["processing"].as_array().unwrap().len(), 0);
}

#[test]
fn replacement_requires_confirmation_and_yes_bypasses_it() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let path = source.join("photo.png");
    write_source(&path);
    let original = std::fs::read(&path).unwrap();

    let mut rejected = binary()
        .args([source.to_str().unwrap(), "--max", "60x30", "--replace"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    rejected.stdin.as_mut().unwrap().write_all(b"no\n").unwrap();
    let rejected = rejected.wait_with_output().unwrap();
    assert!(!rejected.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let accepted = binary()
        .args([
            source.to_str().unwrap(),
            "--max",
            "60x30",
            "--replace",
            "--yes",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(image::open(path).unwrap().dimensions(), (60, 30));
}

#[test]
fn removed_metadata_option_is_rejected_without_touching_source() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let path = source.join("photo.png");
    write_source(&path);
    let original = std::fs::read(&path).unwrap();

    let result = binary()
        .args([
            source.to_str().unwrap(),
            "--max",
            "60x30",
            "--preserve-all-metadata",
        ])
        .output()
        .unwrap();

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--preserve-all-metadata"));
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn human_and_json_preview_reports_agree_on_totals_and_failures() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    std::fs::create_dir(&source).unwrap();
    write_source(&source.join("eligible.png"));
    RgbImage::new(1, 1).save(source.join("small.png")).unwrap();
    std::fs::write(source.join("notes.txt"), b"not an image").unwrap();
    std::fs::write(source.join("truncated.png"), b"\x89PNG\r\n\x1a\n").unwrap();

    let human = binary()
        .arg(&source)
        .args(["--max", "60x30", "--dry-run"])
        .output()
        .unwrap();
    let json = binary()
        .arg(&source)
        .args(["--max", "60x30", "--dry-run", "--json"])
        .output()
        .unwrap();

    assert!(!human.status.success());
    assert_eq!(human.status.code(), json.status.code());
    let text = String::from_utf8(human.stdout).unwrap();
    let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    for (label, key, expected) in [
        ("Reduced images", "reduced", 0),
        ("Within-bounds images", "within_bounds", 1),
        ("Skipped entries", "skipped", 1),
        ("Failed images", "failed", 1),
        ("Reduced source bytes", "source_bytes", 0),
        ("Reduced output bytes", "output_bytes", 0),
        ("Total bytes saved", "bytes_saved", 0),
    ] {
        assert_eq!(report["summary"][key], expected);
        assert!(
            text.lines()
                .any(|line| line == format!("{label}: {expected}"))
        );
    }
    assert!(!parent.path().join("source-reduced").exists());
}
