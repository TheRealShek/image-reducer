# Release Benchmark Protocol

Run this protocol before changing the default JPEG quality, decoded-pixel guard, memory estimate, or automatic worker count. Use a representative copy of real images, never the only copy. Include every supported format, large camera images, transparency, ICC profiles, EXIF/XMP, nested directories, and both compressible and noisy content.

## Platforms

Collect the same measurements on Linux x86-64 and ARM64. Record CPU, logical cores, total memory, filesystem, Rust version, commit, and corpus counts/bytes. CI executes correctness tests natively on both architectures, but CI is not a performance baseline.

## Build

```text
cargo build --release --locked
```

## Discovery and inspection

This isolates traversal, header decoding, classification, and reporting without creating output:

```text
/usr/bin/time -v ./target/release/image-reducer CORPUS --dry-run --json > discovery.json
```

Record elapsed time, maximum resident set size, filesystem reads, discovered entries, eligible images, and failures.

## End-to-end processing and scaling

Use a fresh explicit output directory for every run. Test `--jobs 1`, half the logical CPUs, the automatic default, and the logical CPU count. Record elapsed time, maximum resident set size, filesystem writes, output bytes, skipped images, and failures.

```text
/usr/bin/time -v ./target/release/image-reducer CORPUS --output NEW_OUTPUT --jobs 1 --json > run.json
```

Peak memory divided by the largest concurrently decoded pixel count validates the weighted-memory estimate. Increase the estimate or lower the pixel guard if measured use approaches the memory budget. The selected automatic worker count must avoid throughput regressions and memory pressure on both architectures.

## Stage profiling

Profile one-worker runs first so concurrent samples do not overlap. Use a sampling profiler to attribute time and allocation peaks to these call paths:

- discovery and inspection: `discover`, `inspect_files`;
- metadata: `read_metadata`, EXIF parsing, and container checks;
- decoding: `DynamicImage::from_decoder`;
- color/orientation and resizing: `transform_profile`, mapper calls, and `Resizer::resize`;
- encoding: `encode` and codec encoders;
- verification: `verify` and `verify_container_metadata`;
- publication: `publish_file`, synchronization, rename/exchange, and cleanup.

Record wall time or sampled CPU share for every stage, maximum resident memory, and peak temporary storage. Keep the profiler command and raw report with release artifacts so later runs are comparable.

## JPEG quality calibration

Repeat the same JPEG subset at qualities 85, 88, 90, 92, 95, and 100. Compare each candidate against the orientation-normalized, resized lossless reference using PSNR/SSIM plus side-by-side inspection at 100% and 200%. Include edges, text, gradients, skin, foliage, and high-ISO noise. Record candidate size, encode/decode time, metrics, and visible artifacts.

The default must be the lowest tested quality with no material visible regression across the representative corpus. Lower values remain explicit user trade-offs and must produce a warning.

## Acceptance record

Store a dated result table containing:

- platform and corpus identity;
- discovery throughput;
- per-stage time or CPU share;
- peak resident memory and temporary storage;
- scaling by worker count;
- JPEG quality size/metric/visual decision;
- chosen defaults and the evidence supporting each one.

Do not tag a release when either architecture is missing, failures are unexplained, memory is unbounded, or the visual comparison does not support the JPEG default.
