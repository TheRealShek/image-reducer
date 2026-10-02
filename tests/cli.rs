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

fn add_png_text(path: &std::path::Path) {
    let bytes = std::fs::read(path).unwrap();
    let iend = bytes
        .windows(4)
        .rposition(|window| window == b"IEND")
        .unwrap()
        - 4;
    let kind = *b"tEXt";
    let contents = b"Comment\0native PNG text";
    let mut crc = u32::MAX;
    for byte in kind.iter().chain(contents) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
        }
    }

    let mut chunk = Vec::new();
    chunk.extend_from_slice(&(contents.len() as u32).to_be_bytes());
    chunk.extend_from_slice(&kind);
    chunk.extend_from_slice(contents);
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    let mut output = Vec::with_capacity(bytes.len() + chunk.len());
    output.extend_from_slice(&bytes[..iend]);
    output.extend_from_slice(&chunk);
    output.extend_from_slice(&bytes[iend..]);
    std::fs::write(path, output).unwrap();
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
fn preserve_all_reports_unrepresentable_png_text_as_a_fidelity_conflict() {
    let parent = tempfile::tempdir().unwrap();
    let source = parent.path().join("source");
    let output = parent.path().join("reduced");
    std::fs::create_dir(&source).unwrap();
    let path = source.join("metadata.png");
    write_source(&path);
    add_png_text(&path);

    let result = binary()
        .args([
            source.to_str().unwrap(),
            "--max",
            "60x30",
            "--output",
            output.to_str().unwrap(),
            "--preserve-all-metadata",
            "--json",
        ])
        .output()
        .unwrap();

    assert!(result.status.success());
    let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["summary"]["reduced"], 0);
    assert_eq!(report["summary"]["skipped"], 1);
    assert_eq!(report["processing"][0]["outcome"], "fidelity_conflict");
    assert!(
        report["processing"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("PNG text metadata")
    );
    assert!(!output.join("metadata.png").exists());
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
