# Image Reducer

Image Reducer is a Linux command-line tool that safely downscales images which exceed orientation-aware bounds. By default, it preserves every source and writes only verified, smaller results to a separate directory tree.

## Build

The minimum supported Rust version is 1.89.

```text
cargo build --release
./target/release/image-reducer --help
```

The normal build is self-contained and uses pure-Rust image backends. It does not require ImageMagick, `libwebp`, or another image-processing runtime.

## Usage

Preview the default plan without changing the filesystem:

```text
image-reducer ~/Pictures --dry-run
```

Create a unique sibling output such as `Pictures-reduced`:

```text
image-reducer ~/Pictures
```

Use custom landscape-oriented bounds and exclude a subtree:

```text
image-reducer ~/Pictures --max 2560x1440 --exclude cache/thumbnails
```

Write to a specific new destination:

```text
image-reducer ~/Pictures --output /mnt/archive/Pictures-reduced
```

Replace sources only after reviewing a preview. Replacement asks for confirmation unless `--yes` is supplied:

```text
image-reducer ~/Pictures --replace --dry-run
image-reducer ~/Pictures --replace
```

Use `--json` for a structured report and `--jobs` to set the worker limit. `--max-pixels` overrides the per-image decoded-pixel guard when a trusted large image needs processing.

## Behavior

- JPEG, PNG, and WebP are detected from their contents. GIF, TIFF, BMP, and other formats are reported as unsupported.
- Stored orientation is applied before dimensions are classified and is normalized in reduced output.
- Images are never cropped, stretched, or enlarged.
- Animated images, symbolic links, unsupported formats, and resource-guard violations are reported and skipped.
- A candidate is decoded and verified before publication. It is discarded when it is not smaller than its source.
- Preservation output uses the source-relative path and retains modification time and permission bits.
- Replacement uses a destination-local temporary file, durable synchronization, and atomic rename. The source is unchanged if candidate creation or verification fails.
- Processing is parallel but bounded by both a worker pool and a memory budget derived from available system memory.

JPEG uses a high-fidelity quality default of 92. `--quality` changes JPEG encoding only; WebP is encoded losslessly, and lossless formats ignore it.

The metadata policy retains the EXIF capture date and ICC profile, normalizes orientation, and removes other metadata, including GPS, native text, XMP, and IPTC. The tool reports removals for processed images. Review this policy before using `--replace`.

## Exit status

The command exits successfully when all entries are reduced, untouched, or intentionally skipped. Invalid arguments, access errors, image-processing failures, publication failures, and interruption produce a non-zero exit status. One image-specific failure does not stop other images from being processed.

## Verification

```text
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo deny check advisories licenses sources
```

The requirements and safety terminology are documented in [`docs/REQUIREMENTS.md`](docs/REQUIREMENTS.md) and [`CONTEXT.md`](CONTEXT.md).
Use [`docs/BENCHMARKING.md`](docs/BENCHMARKING.md) to calibrate release defaults against a representative personal image corpus before tagging a release.
