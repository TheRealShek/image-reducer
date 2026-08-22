# Image Reduction Context

Image Reducer is a command-line tool that safely downscales images in a selected directory when their dimensions exceed a user-selected limit. Its purpose is to remove unnecessary resolution while preserving the image's aspect ratio and protecting source files from partial or failed processing.

## Language

**Source directory**:
The directory selected for image discovery. Nested directories are included, while symbolic links are skipped and reported.
_Avoid_: Input folder

**Source image**:
An image discovered beneath the source directory and considered for reduction.
_Avoid_: Original, when referring to an image that may already have been replaced

**Target bounds**:
The maximum width and height within which a reduced image must fit. The bounds do not prescribe an exact resolution and never change the image's aspect ratio.
_Avoid_: Target resolution, 1080p

**Default target bounds**:
The orientation-aware limit used when the user does not provide target bounds: `1920 × 1080` for landscape images, `1080 × 1920` for portrait images, and `1080 × 1080` for square images.

**Custom target bounds**:
Landscape-oriented maximum dimensions selected by the user. The bounds follow the source image's orientation and otherwise behave like the default target bounds.

**Downscaling**:
Reducing an image's pixel dimensions so that it fits within the target bounds while retaining its aspect ratio. Downscaling never enlarges an image.
_Avoid_: Compression, resizing

**Normalized orientation**:
The displayed orientation applied directly to an image's pixels before target dimensions are calculated. A reduced image is stored in that orientation so viewers do not rotate it again.

**Eligible image**:
A supported source image whose pixel dimensions exceed the target bounds and therefore requires downscaling.

**Within-bounds image**:
A supported source image that already fits within the target bounds. It remains untouched and is never upscaled or re-encoded.

**Reduced image**:
The successfully encoded and verified result of downscaling an eligible image. It retains the source image's format and occupies fewer bytes than the source image.

**Beneficial reduction**:
A reduction whose result fits within the target bounds and occupies fewer bytes than its source image. A candidate that does not meet both conditions must not replace or be presented as a reduced image.

**Replacement mode**:
An explicitly selected mode in which a reduced image replaces its source image only after the result has been successfully created and verified. A failure leaves the source image unchanged.
_Avoid_: In-place processing

**Preservation mode**:
The default mode, which writes reduced images to a separate output directory and preserves every source image.
_Avoid_: Backup mode

**Output directory**:
The destination used in preservation mode. It must be outside the source tree; when the user does not choose one, the tool selects a safe sibling directory.

**Reduced output tree**:
The relative directory structure containing only reduced images in preservation mode. Within-bounds, skipped, and failed images are not copied into this tree.

**Output collision**:
A proposed output directory or file that already exists. Existing output is never overwritten implicitly: an inferred destination advances to a unique name such as `Pictures-reduced-2`, while an explicitly selected destination causes the run to stop.

**Preserved metadata**:
Capture date and color-profile information carried into a reduced image, together with normalized visual orientation. Location metadata is removed unless the user explicitly asks to preserve all metadata.

**Supported image**:
A single-image, non-animated image in a format the tool can both decode and safely encode while retaining that format. The initial supported formats are JPEG, PNG, WebP, BMP, single-page TIFF, and single-frame GIF; optional AVIF support may be provided separately.

**Animated image**:
An image containing multiple timed frames. Animated images are outside the reduction scope and are skipped rather than partially processed.

**Multi-image container**:
A source file containing multiple pages, frames, or images. It is skipped unless the supported format policy explicitly covers all of its contents.

**Skipped entry**:
A directory entry that the tool intentionally does not process, such as a symbolic link or unsupported image. Every skipped entry is included in the processing report with a reason.

**Excluded directory**:
A directory beneath the source directory that the user explicitly removes from discovery, together with its complete subtree. Its source-relative path is reported once as an intentional skip and does not make the run unsuccessful.

**Processing report**:
The user-visible summary of reduced, untouched, skipped, and failed images produced after a run, including total byte savings. It is available as human-readable output and as structured data for automation.

**Processing failure**:
An image-specific error that leaves the source image untouched and does not prevent other discovered images from being processed. Any processing failure makes the overall run unsuccessful and appears in the processing report.

**Processing confirmation**:
Approval requested before replacement begins, after showing the source directory, target bounds, eligible-image count, and replacement policy. An explicit automation choice may grant this approval non-interactively.

**Verified reduction**:
A reduced image that decodes successfully, retains its source format and required image properties, has the exact calculated dimensions, occupies fewer bytes than its source, and has been completely persisted. Only a verified reduction may be accepted as output or replace a source image.

**Fidelity conflict**:
A condition in which downscaling would discard an image property other than the intended reduction in pixel dimensions, such as transparency, required metadata, or representable color information. The source image is skipped rather than processed with silent loss.

**Encoding quality**:
The fidelity used when a lossy image format must be encoded after downscaling. Normal processing prioritizes high visual fidelity rather than minimizing bytes, while an explicitly selected lower quality is treated as a potentially visible degradation and warned about.

**Filesystem attributes**:
The source file's modification time and permission bits, which are retained on a reduced image. Ownership is retained during replacement when permitted; arbitrary extended attributes and access-control lists are not guaranteed.

**Access failure**:
An inability to inspect a path within the selected source tree due to permissions or another input/output error. It is a processing failure rather than an intentional skip.

**Preview run**:
A non-modifying run that discovers and classifies images and reports the operations that a processing run would attempt.
_Avoid_: Test run

**Resource guard**:
A pre-processing limit that protects the host from images whose decoded pixel count could consume excessive memory. An image exceeding the limit is skipped unless the user explicitly overrides the guard.

**Interrupted run**:
A run stopped before every discovered image is handled. Completed reductions remain valid, the active source is protected from partial replacement, and safely removable temporary output is discarded.

**Hidden entry**:
A file or directory whose name begins with a dot. Hidden entries within the selected source directory participate in discovery like other entries.

**Detected format**:
The image format established from file contents rather than inferred only from its filename. A supported image with a missing or misleading extension may be processed without silently renaming it.
