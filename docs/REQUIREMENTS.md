# Image Reducer Requirements

## Product goal

Image Reducer is a Linux command-line tool for safely removing unnecessary pixel resolution from images in a directory tree. It downscales only images that exceed configurable orientation-aware bounds, preserves aspect ratio and visual fidelity, and protects source files from corrupt or partial processing.

"No data loss" means no accidental loss, corruption, or silent fidelity change beyond the explicitly requested reduction in pixel dimensions and metadata policy. Downscaling is inherently irreversible; the default workflow therefore preserves every source image.

## Primary workflow

```text
image-reducer SOURCE
```

The plain command recursively inspects `SOURCE`, uses the default target bounds, and writes beneficial reductions to a unique sibling output directory. It does not replace sources or copy files that do not need reduction.

The command-line surface should support these capabilities:

```text
image-reducer SOURCE [--max WIDTHxHEIGHT]
                     [--output DIRECTORY | --replace]
                     [--exclude RELATIVE_DIRECTORY]...
                     [--quality 1-100]
                     [--preserve-all-metadata]
                     [--dry-run]
                     [--yes]
                     [--jobs COUNT]
                     [--json]
```

Exact help text and internal organization are implementation choices, but the behavior in this document is required.

## Discovery

- Accept exactly one source directory per run.
- Traverse nested and hidden directories recursively.
- Detect image formats from file contents rather than trusting filename extensions.
- Skip symbolic links and report them, avoiding duplicate work and traversal outside the source tree.
- Allow repeatable `--exclude RELATIVE_DIRECTORY` arguments. Each exact source-relative directory and its complete subtree are excluded from discovery and reported once as an intentional skip.
- Reject exclusions that resolve outside the source tree. The initial release does not interpret exclusions as glob patterns.
- Treat unreadable paths as failures, continue with accessible paths, and make the overall run unsuccessful.
- Do not treat unsupported formats or intentional policy skips as run failures.

## Target bounds

- Default bounds are `1920 × 1080` for landscape images, `1080 × 1920` for portrait images, and `1080 × 1080` for square images.
- `--max WIDTHxHEIGHT` accepts landscape-oriented custom bounds and applies the corresponding rotated bounds to portrait images.
- Apply stored orientation before classifying the image or calculating output dimensions.
- Preserve aspect ratio within unavoidable integer-pixel rounding. Never crop, stretch, or upscale.
- Leave within-bounds images untouched and do not re-encode or copy them.

## Supported image scope

The initial release supports single-image files that can be decoded and safely re-encoded in their existing format:

- JPEG
- PNG
- WebP
- BMP
- single-page TIFF
- single-frame GIF

AVIF may be offered as optional support if it does not complicate the normal portable build. Animated images, multi-page containers, HEIC/HEIF, camera RAW, SVG, and other unsupported formats are skipped and reported. The tool never processes only the first frame or page of a multi-image file.

## Processing modes

### Preservation mode

Preservation mode is the default.

- Write only verified, beneficial reductions to the output tree.
- Preserve each reduced image's relative path beneath the source directory.
- Require the output directory to be outside the source tree.
- Infer a sibling named `<source>-reduced` when no output is given.
- If an inferred destination exists, offer or select the next unique name, such as `<source>-reduced-2`.
- If an explicitly requested output destination exists, stop rather than merge or overwrite it.
- Show the plan and begin without confirmation.

### Replacement mode

Replacement mode requires explicit `--replace` selection.

- Show the planned source, target bounds, eligible-image count, and irreversible replacement policy.
- Require confirmation unless `--yes` is present.
- Build and verify the candidate before changing its source.
- Replace each source transactionally so a crash or failure cannot expose a partial image.
- Do not retain the high-resolution source after successful replacement.
- Leave the source unchanged whenever processing or verification fails.

`--output` and `--replace` are mutually exclusive.

### Preview mode

`--dry-run` performs discovery, classification, and planning without creating directories, writing images, replacing sources, or requesting confirmation.

## Fidelity and encoding

- Preserve the source format and filename; do not perform format conversion.
- Normalize visual orientation before downscaling and prevent double rotation in the result.
- Use high-quality, color-aware, transparency-safe resampling. Throughput must not come from a lower-fidelity resize filter.
- Preserve transparency, representable color information, capture date, and color profile.
- Remove GPS metadata by default. `--preserve-all-metadata` retains location and other supported metadata, except metadata that must be normalized to represent the transformed pixels correctly.
- Preserve filesystem modification time and permission bits. Preserve ownership during replacement when permitted.
- Do not promise preservation of arbitrary extended attributes or access-control lists in the first release.
- Skip an image when the encoder cannot retain required properties. Report the specific fidelity conflict.

Lossy formats necessarily require re-encoding. The normal quality setting prioritizes high visual fidelity rather than maximizing byte savings. `--quality` permits an explicit trade-off only for supported lossy encoders and prints a visible warning below the tested default. The initial portable WebP backend encodes losslessly, so `--quality` does not affect WebP. Lossless encoding remains lossless and unaffected by this setting. The numeric default will be selected through visual comparisons and size benchmarks.

## Beneficial and verified reductions

A candidate is beneficial only when it has the calculated dimensions and occupies fewer bytes than its source. Otherwise, discard the candidate, retain the source, and report that no beneficial reduction was available.

Before accepting any result, verify every applicable condition:

- The complete output was persisted successfully.
- The output can be decoded.
- Its detected format matches the source format.
- Its dimensions equal the aspect-ratio-preserving calculation.
- Its orientation, transparency, color information, and required metadata match policy.
- Its file size is smaller than the source.

Verification is per file. One failure does not invalidate other successfully completed reductions.

## Efficiency and resource safety

- Process independent images concurrently using a bounded worker count selected from available CPU and memory.
- Allow explicit worker control with `--jobs COUNT`.
- Inspect dimensions before full decoding and guard against images whose decoded pixel count could exhaust memory.
- Skip resource-guard violations with a clear reason unless the user explicitly overrides the guard.
- Keep per-file temporary storage bounded and clean it up when safe.
- Choose the default worker count, pixel guard, and lossy quality from measurements rather than unsupported constants.

## Interruption and failures

- On interruption, retain completed reductions.
- Protect the currently processed source from partial replacement.
- Remove incomplete temporary output when safe.
- Continue after an image-specific failure unless the user interrupts the run or a run-wide precondition fails.
- Return a non-zero status for processing failures, invalid arguments, inaccessible paths, or interruption.
- Intentional skips do not by themselves make a run unsuccessful.

## Reporting

Show live human-readable progress and a final summary containing:

- reduced images;
- within-bounds images;
- skipped images and reasons;
- failed images and errors;
- source and output byte totals; and
- total bytes saved.

Provide structured JSON output for automation. Warnings about misleading extensions, metadata changes, quality overrides, symbolic links, unsupported formats, resource guards, and fidelity conflicts must be actionable.

## Delivery constraints

- Support Linux in the first release.
- Ship normal releases as self-contained executables that do not require separately installed image tools or runtimes.
- Target Linux x86-64 and ARM64.
- Keep optional format support from complicating installation of the normal build.

## Technical foundation

Use Rust 2024 with a minimum supported Rust version of 1.89. Keep the application synchronous and use one bounded concurrency layer; image processing does not require an async runtime.

| Concern | Selection |
|---|---|
| CLI | `clap` with typed arguments |
| Traversal | `walkdir`, with link following disabled and exact subtree exclusions |
| Codecs | `image` with default features disabled and only JPEG, PNG, WebP, BMP, TIFF, and GIF enabled |
| Resizing | `fast_image_resize` using SIMD Lanczos3, alpha-aware processing, and linear-light mapping |
| ICC color | `moxcms` pure-Rust color transforms |
| Metadata | `image` metadata APIs, `img-parts` container handling, and isolated `kamadak-exif` parsing/reconstruction |
| Concurrency | One dedicated Rayon pool plus a weighted memory budget |
| Durable writes | Destination-local `tempfile` output with `rustix` synchronization and atomic publication |
| File attributes | Standard Unix APIs, `filetime`, and `rustix` where needed |
| Cancellation | `ctrlc` with an atomic cancellation flag |
| Reports | `serde`, `serde_json`, and terminal-only `indicatif` progress |
| Errors | Typed errors using `thiserror` |

The processing pipeline has three stages: bounded header-only discovery and planning; memory-budgeted parallel decode, orientation/color normalization, resize, encode, and verification; then synchronized atomic publication and reporting. Disable internal Rayon features in codec and resizing dependencies to prevent nested parallelism.

Metadata is untrusted input and receives independent size limits. Default output reconstructs only promised metadata, removes GPS and opaque XMP that could duplicate location, and normalizes orientation. Preserve-all mode retains supported metadata only after round-trip verification. If metadata cannot be represented safely, skip the image. Do not use `little_exif` while its XML dependency remains below versions patched for RUSTSEC-2026-0194 and RUSTSEC-2026-0195.

The portable WebP backend encodes losslessly. Do not add native `libwebp` to v1 merely to expose lossy quality control; reconsider it only if benchmarks show that too many WebP reductions fail the beneficial-reduction gate.

Before release, benchmark discovery, decoding, resizing, encoding, metadata work, verification, and publication separately. Measure throughput, peak resident memory, temporary storage, and scaling on x86-64 and ARM64. Commit `Cargo.lock`, run dependency license and advisory checks in CI, and prefer disabling an unsafe format variant over weakening fidelity or source protection.

Primary technical references:

- [`image` codecs](https://docs.rs/image/latest/image/codecs/)
- [`image` orientation](https://docs.rs/image/latest/image/metadata/enum.Orientation.html)
- [`fast_image_resize`](https://docs.rs/fast_image_resize/latest/fast_image_resize/)
- [`moxcms`](https://docs.rs/crate/moxcms/latest/source/README.md)
- [`walkdir`](https://docs.rs/walkdir/latest/walkdir/struct.WalkDir.html)
- [RUSTSEC-2026-0194](https://rustsec.org/advisories/RUSTSEC-2026-0194.html)
- [RUSTSEC-2026-0195](https://rustsec.org/advisories/RUSTSEC-2026-0195.html)

## Explicit non-goals

- Graphical interface
- Upscaling
- Cropping or aspect-ratio changes
- Exact output resolution for every image
- Targeting a specific byte size
- Format conversion
- Animation or multi-page image processing
- Copying the entire source tree
- Whole-batch rollback
- Guaranteed preservation of arbitrary Linux extended attributes or ACLs
- Glob patterns or ignore files for directory exclusions

## Acceptance outcomes

The first release is acceptable when representative supported images can be processed recursively with correct orientation and dimensions; within-bounds and unsupported inputs remain untouched; every accepted output satisfies the verification contract; failures and interruption cannot corrupt a source; concurrency remains bounded; and human and structured reports accurately reconcile every discovered entry.
