# Image Reducer Agent Guide

Image Reducer is a Linux Rust CLI that safely downscales oversized images. Sources are preserved by default; `--replace` is explicitly destructive and uses verified, durable, transactional publication.

## Repository Map

- `docs/REQUIREMENTS.md`: authoritative product behavior and acceptance criteria.
- `docs/adr/`: safety and architecture decisions. Read both ADRs before changing publication or source-preservation behavior.
- `src/cli.rs`: typed command-line arguments and validation.
- `src/discovery.rs`: recursive traversal, exclusions, and symlink handling.
- `src/inspection.rs`: format detection, dimensions, orientation, resource guards, and source fingerprints.
- `src/plan.rs`: target dimensions and preservation/replacement plans.
- `src/processing.rs`: metadata policy, color/orientation normalization, resize, encoding, and candidate verification.
- `src/runner.rs`: bounded concurrency, cancellation, memory budgeting, atomic publication, filesystem attributes, and per-file outcomes.
- `src/main.rs`: confirmation, human/JSON reports, summaries, and exit status.
- `tests/cli.rs`: end-to-end CLI behavior.
- `docs/BENCHMARKING.md`: required release calibration on representative x86-64 and ARM64 corpora.

## Commands

```bash
cargo fmt --all -- --check
cargo test --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo +1.89.0 test --all-targets --locked
cargo +1.89.0 clippy --all-targets --locked -- -D warnings
cargo deny check advisories licenses sources
cargo build --release --locked
```

Rust 1.89 is the minimum supported version. Run narrow tests first, then the complete stable and MSRV checks for changes affecting processing, publication, CLI behavior, dependencies, or releases.

## Safety Invariants

- Never alter a source in preservation mode.
- Never publish a candidate until format, dimensions, orientation, pixels/transparency, required metadata, and beneficial size are verified.
- Keep source and output traversal anchored to directory handles; do not replace `openat`/`mkdirat`/`renameat2` publication with path-based filesystem operations.
- Replacement must keep the displaced source linked until the candidate and directory entry are durable. Any failed validation or pre-acceptance synchronization must restore or retain a recoverable source.
- Do not follow symbolic links or allow exclusions/output paths to escape the source policy.
- Preserve format, relative path, modification time, permission bits, and replacement ownership when permitted.
- Keep one bounded Rayon pool and the weighted memory budget; do not introduce nested parallelism or async runtime machinery.
- Treat all image and metadata bytes as untrusted and retain independent resource limits.

## Change Rules

- Read the requirement, relevant ADR, callers, and nearby regression tests before changing behavior.
- Add tests for expected behavior and source-safety failure paths. Never weaken verification to make a codec succeed.
- Update human and JSON reporting together when classifications, outcomes, warnings, or summaries change.
- Update requirements/ADR/README when public behavior or safety architecture changes.
- After completing and verifying any new feature, rebuild and update the installed system binary with:

```bash
cargo build --release --locked
install -Dm0755 target/release/image-reducer /home/thakur/.local/bin/image-reducer
image-reducer --version
```

Do not install an unverified or dirty-build binary. `/home/thakur/.local/bin` is Sir's user-wide executable path and is already on `PATH`; do not target `/usr/local/bin` unless Sir explicitly requests an all-users installation and provides authorization.

## Current Verification

The implementation has unit and CLI coverage for supported formats, orientation-aware bounds, metadata policy, preservation, replacement, cancellation, symlink containment, source-change rollback, permission retention, and rollback-failure recovery. CI runs stable and Rust 1.89 checks on native Linux x86-64 and ARM64 runners.
