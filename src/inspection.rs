use std::{
    fmt,
    fs::File,
    io::BufReader,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use image::{ImageDecoder, ImageFormat, ImageReader, metadata::Orientation};

use crate::plan::Bounds;

pub const DEFAULT_MAX_PIXELS: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportedFormat {
    Jpeg,
    Png,
    WebP,
    Bmp,
    Tiff,
    Gif,
}

impl SupportedFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "JPEG",
            Self::Png => "PNG",
            Self::WebP => "WebP",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Gif => "GIF",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Jpeg => &["jpg", "jpeg", "jpe", "jfif"],
            Self::Png => &["png"],
            Self::WebP => &["webp"],
            Self::Bmp => &["bmp", "dib"],
            Self::Tiff => &["tif", "tiff"],
            Self::Gif => &["gif"],
        }
    }
}

impl fmt::Display for SupportedFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl From<(u32, u32)> for Dimensions {
    fn from((width, height): (u32, u32)) -> Self {
        Self { width, height }
    }
}

impl fmt::Display for Dimensions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ImageDetails {
    pub format: SupportedFormat,
    pub encoded_dimensions: Dimensions,
    pub displayed_dimensions: Dimensions,
    pub output_dimensions: Dimensions,
    pub orientation: u8,
    pub color_type: String,
    pub source_bytes: u64,
    pub source_fingerprint: SourceFingerprint,
    pub extension_warning: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceFingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl SourceFingerprint {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }

    pub fn matches_path(self, path: &Path) -> std::io::Result<bool> {
        std::fs::metadata(path).map(|metadata| self == Self::from_metadata(&metadata))
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Classification {
    Eligible(ImageDetails),
    WithinBounds(ImageDetails),
    Skipped { reason: String },
    Failed { error: String },
}

impl Classification {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Eligible(_) => "eligible",
            Self::WithinBounds(_) => "within_bounds",
            Self::Skipped { .. } => "skipped",
            Self::Failed { .. } => "failed",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct InspectedEntry {
    pub relative_path: PathBuf,
    pub classification: Classification,
}

pub fn inspect_files(
    source: &Path,
    files: &[PathBuf],
    bounds: Bounds,
    max_pixels: u64,
) -> Vec<InspectedEntry> {
    files
        .iter()
        .map(|relative_path| {
            let path = source.join(relative_path);
            let classification = inspect_file(&path, bounds, max_pixels)
                .unwrap_or_else(|error| Classification::Failed { error });
            InspectedEntry {
                relative_path: relative_path.clone(),
                classification,
            }
        })
        .collect()
}

fn inspect_file(
    path: &Path,
    bounds: Bounds,
    max_pixels: u64,
) -> std::result::Result<Classification, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let Some(image_format) = reader.format() else {
        return Ok(Classification::Skipped {
            reason: "unrecognized or unsupported file format".to_owned(),
        });
    };
    let Some(format) = supported_format(image_format) else {
        return Ok(Classification::Skipped {
            reason: format!("unsupported image format: {image_format:?}"),
        });
    };

    if let Some(reason) = unsupported_container_reason(path, format)? {
        return Ok(Classification::Skipped { reason });
    }

    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let encoded_dimensions = Dimensions::from(decoder.dimensions());
    if encoded_dimensions.width == 0 || encoded_dimensions.height == 0 {
        return Err("image dimensions must be greater than zero".to_owned());
    }
    let pixels = u64::from(encoded_dimensions.width) * u64::from(encoded_dimensions.height);
    if pixels > max_pixels {
        return Ok(Classification::Skipped {
            reason: format!(
                "resource guard: {pixels} decoded pixels exceeds the configured limit of {max_pixels}"
            ),
        });
    }

    let color_type = format!("{:?}", decoder.original_color_type());
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let displayed_dimensions = displayed_dimensions(encoded_dimensions, orientation);
    let output_dimensions = Dimensions::from(
        bounds.fitted_dimensions(displayed_dimensions.width, displayed_dimensions.height),
    );
    let details = ImageDetails {
        format,
        encoded_dimensions,
        displayed_dimensions,
        output_dimensions,
        orientation: orientation.to_exif(),
        color_type,
        source_bytes: metadata.len(),
        source_fingerprint: SourceFingerprint::from_metadata(&metadata),
        extension_warning: extension_warning(path, format),
    };

    if output_dimensions == displayed_dimensions {
        Ok(Classification::WithinBounds(details))
    } else {
        Ok(Classification::Eligible(details))
    }
}

fn supported_format(format: ImageFormat) -> Option<SupportedFormat> {
    match format {
        ImageFormat::Jpeg => Some(SupportedFormat::Jpeg),
        ImageFormat::Png => Some(SupportedFormat::Png),
        ImageFormat::WebP => Some(SupportedFormat::WebP),
        ImageFormat::Bmp => Some(SupportedFormat::Bmp),
        ImageFormat::Tiff => Some(SupportedFormat::Tiff),
        ImageFormat::Gif => Some(SupportedFormat::Gif),
        _ => None,
    }
}

fn unsupported_container_reason(
    path: &Path,
    format: SupportedFormat,
) -> std::result::Result<Option<String>, String> {
    let file = || {
        File::open(path)
            .map(BufReader::new)
            .map_err(|error| error.to_string())
    };

    match format {
        SupportedFormat::Gif => {
            let mut options = gif::DecodeOptions::new();
            options.skip_frame_decoding(true);
            let mut decoder = options
                .read_info(file()?)
                .map_err(|error| error.to_string())?;
            if decoder
                .next_frame_info()
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Err("GIF contains no image frame".to_owned());
            }
            Ok(decoder
                .next_frame_info()
                .map_err(|error| error.to_string())?
                .is_some()
                .then(|| "animated GIF contains multiple frames".to_owned()))
        }
        SupportedFormat::Png => {
            let decoder =
                image::codecs::png::PngDecoder::new(file()?).map_err(|error| error.to_string())?;
            Ok(decoder
                .is_apng()
                .map_err(|error| error.to_string())?
                .then(|| "animated PNG is outside the supported image scope".to_owned()))
        }
        SupportedFormat::WebP => {
            let decoder = image::codecs::webp::WebPDecoder::new(file()?)
                .map_err(|error| error.to_string())?;
            Ok(decoder
                .has_animation()
                .then(|| "animated WebP is outside the supported image scope".to_owned()))
        }
        SupportedFormat::Tiff => {
            let decoder =
                tiff::decoder::Decoder::new(file()?).map_err(|error| error.to_string())?;
            Ok(decoder
                .more_images()
                .then(|| "multi-page TIFF is outside the supported image scope".to_owned()))
        }
        SupportedFormat::Jpeg | SupportedFormat::Bmp => Ok(None),
    }
}

fn displayed_dimensions(dimensions: Dimensions, orientation: Orientation) -> Dimensions {
    match orientation {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => Dimensions {
            width: dimensions.height,
            height: dimensions.width,
        },
        _ => dimensions,
    }
}

fn extension_warning(path: &Path, format: SupportedFormat) -> Option<String> {
    let extension = path.extension()?.to_str()?;
    if format
        .extensions()
        .iter()
        .any(|expected| extension.eq_ignore_ascii_case(expected))
    {
        return None;
    }
    Some(format!(
        "file contents are {format}, but the extension is .{extension}"
    ))
}

#[cfg(test)]
mod tests {
    use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgba};

    use super::*;

    fn write_png(path: &Path, width: u32, height: u32) {
        let image = ImageBuffer::from_pixel(width, height, Rgba([10_u8, 20, 30, 255]));
        image.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn write_oriented_jpeg(path: &Path, width: u32, height: u32) {
        let file = File::create(path).unwrap();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(file);
        let exif_orientation_6 = vec![
            b'I', b'I', 42, 0, 8, 0, 0, 0, 1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0,
            0,
        ];
        encoder.set_exif_metadata(exif_orientation_6).unwrap();
        let pixels = vec![128; width as usize * height as usize * 3];
        encoder
            .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
            .unwrap();
    }

    #[test]
    fn classifies_images_from_contents_and_warns_about_extension() {
        let source = tempfile::tempdir().unwrap();
        write_png(&source.path().join("photo.jpg"), 2000, 1000);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("photo.jpg")],
            Bounds::new(1000, 500).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        let Classification::Eligible(details) = &inspected[0].classification else {
            panic!("expected eligible image")
        };
        assert_eq!(details.format, SupportedFormat::Png);
        assert_eq!(details.output_dimensions, Dimensions::from((1000, 500)));
        assert!(details.extension_warning.is_some());
    }

    #[test]
    fn leaves_within_bounds_image_untouched() {
        let source = tempfile::tempdir().unwrap();
        write_png(&source.path().join("small.png"), 20, 10);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("small.png")],
            Bounds::new(1000, 500).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        assert!(matches!(
            inspected[0].classification,
            Classification::WithinBounds(_)
        ));
    }

    #[test]
    fn applies_resource_guard_before_decoding_pixels() {
        let source = tempfile::tempdir().unwrap();
        write_png(&source.path().join("guarded.png"), 20, 10);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("guarded.png")],
            Bounds::new(10, 5).unwrap(),
            100,
        );

        assert!(matches!(
            &inspected[0].classification,
            Classification::Skipped { reason } if reason.contains("resource guard")
        ));
    }

    #[test]
    fn skips_non_images_without_treating_them_as_failures() {
        let source = tempfile::tempdir().unwrap();
        std::fs::write(source.path().join("notes.jpg"), b"not an image").unwrap();

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("notes.jpg")],
            Bounds::new(1000, 500).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        assert!(matches!(
            inspected[0].classification,
            Classification::Skipped { .. }
        ));
    }

    #[test]
    fn identifies_orientation_that_swaps_dimensions() {
        assert_eq!(
            displayed_dimensions(Dimensions::from((4000, 3000)), Orientation::Rotate90FlipH),
            Dimensions::from((3000, 4000))
        );
    }

    #[test]
    fn applies_stored_orientation_before_classifying_dimensions() {
        let source = tempfile::tempdir().unwrap();
        write_oriented_jpeg(&source.path().join("portrait.jpg"), 40, 20);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("portrait.jpg")],
            Bounds::new(30, 15).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        let Classification::Eligible(details) = &inspected[0].classification else {
            panic!("expected eligible image")
        };
        assert_eq!(details.encoded_dimensions, Dimensions::from((40, 20)));
        assert_eq!(details.displayed_dimensions, Dimensions::from((20, 40)));
        assert_eq!(details.output_dimensions, Dimensions::from((15, 30)));
        assert_eq!(details.orientation, 6);
    }

    #[test]
    fn skips_animated_gif() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("animated.gif");
        let mut file = File::create(&path).unwrap();
        let mut encoder = gif::Encoder::new(&mut file, 2, 2, &[]).unwrap();
        let mut first_pixels = vec![0; 2 * 2 * 4];
        let mut second_pixels = vec![0; 2 * 2 * 4];
        encoder
            .write_frame(&gif::Frame::from_rgba_speed(2, 2, &mut first_pixels, 10))
            .unwrap();
        encoder
            .write_frame(&gif::Frame::from_rgba_speed(2, 2, &mut second_pixels, 10))
            .unwrap();
        drop(encoder);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("animated.gif")],
            Bounds::new(1, 1).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        assert!(matches!(
            &inspected[0].classification,
            Classification::Skipped { reason } if reason.contains("multiple frames")
        ));
    }

    #[test]
    fn skips_multi_page_tiff() {
        let source = tempfile::tempdir().unwrap();
        let path = source.path().join("pages.tiff");
        let file = File::create(&path).unwrap();
        let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(2, 2, &[0; 12])
            .unwrap();
        encoder
            .write_image::<tiff::encoder::colortype::RGB8>(2, 2, &[0; 12])
            .unwrap();
        drop(encoder);

        let inspected = inspect_files(
            source.path(),
            &[PathBuf::from("pages.tiff")],
            Bounds::new(1, 1).unwrap(),
            DEFAULT_MAX_PIXELS,
        );

        assert!(matches!(
            &inspected[0].classification,
            Classification::Skipped { reason } if reason.contains("multi-page")
        ));
    }
}
