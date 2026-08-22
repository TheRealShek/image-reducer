use std::{
    fs::File,
    io::{self, BufReader, Cursor, Read, Seek, SeekFrom},
    path::Path,
};

use exif::{In, Tag, Value};
use fast_image_resize::{ResizeOptions, Resizer};
use image::{
    DynamicImage, ExtendedColorType, GenericImageView, ImageBuffer, ImageDecoder, ImageEncoder,
    ImageFormat, ImageReader, metadata::Orientation,
};
use img_parts::{DynImage, ImageEXIF, ImageICC};
use moxcms::{ColorProfile, Layout, TransformOptions};

use crate::inspection::{Dimensions, ImageDetails, SupportedFormat};

pub const DEFAULT_JPEG_QUALITY: u8 = 92;
pub const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct ProcessingOptions {
    pub jpeg_quality: u8,
    pub preserve_all_metadata: bool,
}

impl Default for ProcessingOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: DEFAULT_JPEG_QUALITY,
            preserve_all_metadata: false,
        }
    }
}

#[derive(Debug)]
pub struct Candidate {
    pub bytes: Vec<u8>,
    pub format: SupportedFormat,
    pub dimensions: Dimensions,
    pub source_bytes: u64,
    pub warnings: Vec<String>,
}

impl Candidate {
    pub fn bytes_saved(&self) -> u64 {
        self.source_bytes - self.bytes.len() as u64
    }
}

#[derive(Debug)]
pub enum ProcessingOutcome {
    Reduced(Candidate),
    NotBeneficial { candidate_bytes: u64 },
    FidelityConflict { reason: String },
    Failed { error: String },
}

#[derive(Default)]
struct Metadata {
    icc: Option<Vec<u8>>,
    exif: Option<Vec<u8>>,
    warnings: Vec<String>,
}

pub fn process_image(
    path: &Path,
    details: &ImageDetails,
    options: ProcessingOptions,
) -> ProcessingOutcome {
    match process_image_inner(path, details, options) {
        Ok(candidate) if candidate.bytes.len() as u64 >= candidate.source_bytes => {
            ProcessingOutcome::NotBeneficial {
                candidate_bytes: candidate.bytes.len() as u64,
            }
        }
        Ok(candidate) => ProcessingOutcome::Reduced(candidate),
        Err(ProcessError::Fidelity(reason)) => ProcessingOutcome::FidelityConflict { reason },
        Err(ProcessError::Failure(error)) => ProcessingOutcome::Failed { error },
    }
}

enum ProcessError {
    Fidelity(String),
    Failure(String),
}

impl ProcessError {
    fn fidelity(error: impl ToString) -> Self {
        Self::Fidelity(error.to_string())
    }

    fn failure(error: impl ToString) -> Self {
        Self::Failure(error.to_string())
    }
}

fn process_image_inner(
    path: &Path,
    details: &ImageDetails,
    options: ProcessingOptions,
) -> Result<Candidate, ProcessError> {
    if !details
        .source_fingerprint
        .matches_path(path)
        .map_err(ProcessError::failure)?
    {
        return Err(ProcessError::failure(
            "source changed after image inspection",
        ));
    }
    let image_format = image_format(details.format);
    let mut file = File::open(path).map_err(ProcessError::failure)?;
    let native_text = read_native_text_metadata(&mut file, details.format)?;
    file.seek(SeekFrom::Start(0))
        .map_err(ProcessError::failure)?;
    let reader = ImageReader::with_format(BufReader::new(file), image_format);
    let mut decoder = reader.into_decoder().map_err(ProcessError::failure)?;
    let source_color = decoder.original_color_type();
    ensure_supported_color(source_color, details.format)?;
    let source_has_alpha = decoder.color_type().has_alpha();
    let orientation = decoder.orientation().map_err(ProcessError::failure)?;
    let metadata = read_metadata(
        &mut decoder,
        orientation,
        details.format,
        native_text,
        options,
    )?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(ProcessError::failure)?;
    image.apply_orientation(orientation);
    if image.dimensions()
        != (
            details.displayed_dimensions.width,
            details.displayed_dimensions.height,
        )
    {
        return Err(ProcessError::failure(
            "decoded dimensions changed since image inspection",
        ));
    }

    let source_profile = metadata
        .icc
        .as_deref()
        .map(ColorProfile::new_from_slice)
        .transpose()
        .map_err(|error| ProcessError::fidelity(format!("invalid ICC profile: {error}")))?;
    if let Some(profile) = &source_profile {
        image = transform_profile(&image, profile, &ColorProfile::new_srgb())?;
    }

    let mapper = fast_image_resize::create_srgb_mapper();
    mapper
        .forward_map_inplace(&mut image)
        .map_err(ProcessError::failure)?;
    let mut resized = new_image_like(
        &image,
        details.output_dimensions.width,
        details.output_dimensions.height,
    )?;
    Resizer::new()
        .resize(&image, &mut resized, Some(&ResizeOptions::new()))
        .map_err(ProcessError::failure)?;
    mapper
        .backward_map_inplace(&mut resized)
        .map_err(ProcessError::failure)?;

    if let Some(profile) = &source_profile {
        resized = transform_profile(&resized, &ColorProfile::new_srgb(), profile)?;
    }

    let bytes = encode(&resized, details.format, &metadata, options.jpeg_quality)?;
    verify(&bytes, details, &resized, source_has_alpha, &metadata)?;

    Ok(Candidate {
        bytes,
        format: details.format,
        dimensions: details.output_dimensions,
        source_bytes: details.source_bytes,
        warnings: metadata.warnings,
    })
}

fn ensure_supported_color(
    color: ExtendedColorType,
    format: SupportedFormat,
) -> Result<(), ProcessError> {
    let supported = matches!(
        color,
        ExtendedColorType::L8
            | ExtendedColorType::La8
            | ExtendedColorType::Rgb8
            | ExtendedColorType::Rgba8
            | ExtendedColorType::L16
            | ExtendedColorType::La16
            | ExtendedColorType::Rgb16
            | ExtendedColorType::Rgba16
    );
    if !supported {
        return Err(ProcessError::fidelity(format!(
            "{format} color type {color:?} cannot be round-tripped safely"
        )));
    }
    if format == SupportedFormat::Jpeg
        && !matches!(color, ExtendedColorType::L8 | ExtendedColorType::Rgb8)
    {
        return Err(ProcessError::fidelity(format!(
            "JPEG color type {color:?} cannot be encoded without fidelity loss"
        )));
    }
    if matches!(format, SupportedFormat::WebP | SupportedFormat::Bmp)
        && !matches!(
            color,
            ExtendedColorType::L8
                | ExtendedColorType::La8
                | ExtendedColorType::Rgb8
                | ExtendedColorType::Rgba8
        )
    {
        return Err(ProcessError::fidelity(format!(
            "{format} color type {color:?} cannot be encoded without fidelity loss"
        )));
    }
    if format == SupportedFormat::Gif
        && !matches!(color, ExtendedColorType::Rgb8 | ExtendedColorType::Rgba8)
    {
        return Err(ProcessError::fidelity(format!(
            "GIF color type {color:?} cannot be encoded without fidelity loss"
        )));
    }
    if format == SupportedFormat::Tiff
        && matches!(color, ExtendedColorType::La8 | ExtendedColorType::La16)
    {
        return Err(ProcessError::fidelity(format!(
            "TIFF color type {color:?} cannot be encoded without fidelity loss"
        )));
    }
    Ok(())
}

fn read_metadata(
    decoder: &mut impl ImageDecoder,
    orientation: Orientation,
    format: SupportedFormat,
    native_text: Option<&'static str>,
    options: ProcessingOptions,
) -> Result<Metadata, ProcessError> {
    let icc = decoder.icc_profile().map_err(ProcessError::failure)?;
    check_metadata_size("ICC profile", icc.as_deref())?;
    let raw_exif = decoder.exif_metadata().map_err(ProcessError::failure)?;
    check_metadata_size("EXIF metadata", raw_exif.as_deref())?;
    let had_exif = raw_exif.is_some();
    let xmp = decoder.xmp_metadata().map_err(ProcessError::failure)?;
    check_metadata_size("XMP metadata", xmp.as_deref())?;
    let iptc = decoder.iptc_metadata().map_err(ProcessError::failure)?;
    check_metadata_size("IPTC metadata", iptc.as_deref())?;

    if options.preserve_all_metadata {
        if let Some(description) = native_text {
            return Err(ProcessError::fidelity(format!(
                "preserve-all mode cannot safely re-encode {description}"
            )));
        }
        if xmp.is_some() {
            return Err(ProcessError::fidelity(
                "preserve-all mode cannot safely re-encode XMP metadata for this image",
            ));
        }
        if iptc.is_some() {
            return Err(ProcessError::fidelity(
                "preserve-all mode cannot safely re-encode IPTC metadata for this image",
            ));
        }
    }

    let exif = match raw_exif {
        Some(mut raw) if options.preserve_all_metadata => {
            let removed = Orientation::remove_from_exif_chunk(&mut raw);
            if orientation != Orientation::NoTransforms && removed.is_none() {
                return Err(ProcessError::fidelity(
                    "stored orientation could not be normalized in EXIF metadata",
                ));
            }
            Some(raw)
        }
        Some(raw) => capture_date_exif(&raw)?,
        None => None,
    };

    let mut warnings = Vec::new();
    if !options.preserve_all_metadata {
        if let Some(description) = native_text {
            warnings.push(format!(
                "{description} was removed. Use --preserve-all-metadata to require retention."
            ));
        }
        if had_exif {
            warnings.push(
                "EXIF metadata was reduced to capture date; GPS and other fields were removed. Use --preserve-all-metadata to require retention."
                    .to_owned(),
            );
        }
        if xmp.is_some() {
            warnings.push(
                "XMP metadata was removed. Use --preserve-all-metadata to require retention."
                    .to_owned(),
            );
        }
        if iptc.is_some() {
            warnings.push(
                "IPTC metadata was removed. Use --preserve-all-metadata to require retention."
                    .to_owned(),
            );
        }
    }

    if exif.is_some()
        && !matches!(
            format,
            SupportedFormat::Jpeg | SupportedFormat::Png | SupportedFormat::WebP
        )
    {
        return Err(ProcessError::fidelity(format!(
            "{format} encoder cannot preserve required EXIF metadata"
        )));
    }
    if icc.is_some() && matches!(format, SupportedFormat::Bmp | SupportedFormat::Gif) {
        return Err(ProcessError::fidelity(format!(
            "{format} encoder cannot preserve the ICC color profile"
        )));
    }

    Ok(Metadata {
        icc,
        exif,
        warnings,
    })
}

fn read_native_text_metadata(
    file: &mut File,
    format: SupportedFormat,
) -> Result<Option<&'static str>, ProcessError> {
    let mut reader = BufReader::new(file);
    let found = match format {
        SupportedFormat::Png => scan_png_text_chunks(&mut reader),
        SupportedFormat::Jpeg => scan_jpeg_comments(&mut reader),
        SupportedFormat::Gif => scan_gif_comments(&mut reader),
        SupportedFormat::WebP | SupportedFormat::Bmp | SupportedFormat::Tiff => return Ok(None),
    }
    .map_err(|error| {
        ProcessError::failure(format!("cannot inspect native text metadata: {error}"))
    })?;

    if let Some((description, bytes)) = found {
        if bytes > MAX_METADATA_BYTES {
            return Err(ProcessError::fidelity(format!(
                "{description} exceeds the {} MiB metadata limit",
                MAX_METADATA_BYTES / 1024 / 1024
            )));
        }
        return Ok(Some(description));
    }
    Ok(None)
}

fn scan_png_text_chunks(
    reader: &mut (impl Read + Seek),
) -> io::Result<Option<(&'static str, usize)>> {
    let mut signature = [0_u8; 8];
    reader.read_exact(&mut signature)?;
    if signature != *b"\x89PNG\r\n\x1a\n" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid PNG signature",
        ));
    }

    let mut text_bytes = 0_usize;
    loop {
        let length = read_u32_be(reader)?;
        let mut kind = [0_u8; 4];
        reader.read_exact(&mut kind)?;
        if matches!(&kind, b"tEXt" | b"zTXt" | b"iTXt") {
            text_bytes = text_bytes.checked_add(length as usize).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "PNG text is too large")
            })?;
        }
        reader.seek(SeekFrom::Current(i64::from(length) + 4))?;
        if kind == *b"IEND" {
            break;
        }
    }

    Ok((text_bytes > 0).then_some(("PNG text metadata", text_bytes)))
}

fn scan_jpeg_comments(
    reader: &mut (impl Read + Seek),
) -> io::Result<Option<(&'static str, usize)>> {
    let mut signature = [0_u8; 2];
    reader.read_exact(&mut signature)?;
    if signature != [0xff, 0xd8] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid JPEG signature",
        ));
    }

    let mut comment_bytes = 0_usize;
    let mut byte = [0_u8; 1];
    while reader.read(&mut byte)? != 0 {
        if byte[0] != 0xff {
            continue;
        }
        loop {
            if reader.read(&mut byte)? == 0 {
                return Ok((comment_bytes > 0).then_some(("JPEG comment metadata", comment_bytes)));
            }
            if byte[0] != 0xff {
                break;
            }
        }
        let marker = byte[0];
        if marker == 0x00 || marker == 0x01 || (0xd0..=0xd8).contains(&marker) {
            continue;
        }
        if marker == 0xd9 {
            break;
        }

        let length = usize::from(read_u16_be(reader)?);
        let payload = length.checked_sub(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid JPEG segment length")
        })?;
        if marker == 0xfe {
            comment_bytes = comment_bytes.checked_add(payload).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "JPEG comments are too large")
            })?;
        }
        reader.seek(SeekFrom::Current(payload as i64))?;
    }

    Ok((comment_bytes > 0).then_some(("JPEG comment metadata", comment_bytes)))
}

fn scan_gif_comments(reader: &mut (impl Read + Seek)) -> io::Result<Option<(&'static str, usize)>> {
    let mut header = [0_u8; 13];
    reader.read_exact(&mut header)?;
    if &header[..6] != b"GIF87a" && &header[..6] != b"GIF89a" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid GIF signature",
        ));
    }
    skip_gif_color_table(reader, header[10])?;

    let mut comment_bytes = 0_usize;
    loop {
        let introducer = read_byte(reader)?;
        match introducer {
            0x21 => {
                let label = read_byte(reader)?;
                let bytes = skip_gif_sub_blocks(reader)?;
                if label == 0xfe {
                    comment_bytes = comment_bytes.checked_add(bytes).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "GIF comments are too large")
                    })?;
                }
            }
            0x2c => {
                let mut descriptor = [0_u8; 9];
                reader.read_exact(&mut descriptor)?;
                skip_gif_color_table(reader, descriptor[8])?;
                read_byte(reader)?;
                skip_gif_sub_blocks(reader)?;
            }
            0x3b => break,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid GIF block introducer",
                ));
            }
        }
    }

    Ok((comment_bytes > 0).then_some(("GIF comment metadata", comment_bytes)))
}

fn skip_gif_color_table(reader: &mut impl Seek, packed: u8) -> io::Result<()> {
    if packed & 0x80 != 0 {
        let entries = 1_u16 << (u32::from(packed & 0x07) + 1);
        reader.seek(SeekFrom::Current(i64::from(entries) * 3))?;
    }
    Ok(())
}

fn skip_gif_sub_blocks(reader: &mut (impl Read + Seek)) -> io::Result<usize> {
    let mut total = 0_usize;
    loop {
        let length = usize::from(read_byte(reader)?);
        if length == 0 {
            return Ok(total);
        }
        total = total.checked_add(length).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "GIF extension is too large")
        })?;
        reader.seek(SeekFrom::Current(length as i64))?;
    }
}

fn read_byte(reader: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0_u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_u16_be(reader: &mut impl Read) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn read_u32_be(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn check_metadata_size(name: &str, metadata: Option<&[u8]>) -> Result<(), ProcessError> {
    if metadata.is_some_and(|bytes| bytes.len() > MAX_METADATA_BYTES) {
        return Err(ProcessError::fidelity(format!(
            "{name} exceeds the {} MiB metadata limit",
            MAX_METADATA_BYTES / 1024 / 1024
        )));
    }
    Ok(())
}

fn capture_date_exif(raw: &[u8]) -> Result<Option<Vec<u8>>, ProcessError> {
    let parsed = exif::Reader::new()
        .read_raw(raw.to_vec())
        .map_err(|error| ProcessError::fidelity(format!("cannot parse EXIF metadata: {error}")))?;
    let date = parsed
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .and_then(|field| match &field.value {
            Value::Ascii(values) => values.first().cloned(),
            _ => None,
        });
    Ok(date.map(|date| build_capture_date_exif(&date)))
}

fn build_capture_date_exif(date: &[u8]) -> Vec<u8> {
    let mut date = date.to_vec();
    if !date.ends_with(&[0]) {
        date.push(0);
    }

    let mut exif = Vec::with_capacity(56 + date.len());
    exif.extend_from_slice(b"II\x2a\0\x08\0\0\0");
    push_u16(&mut exif, 2);
    push_u16(&mut exif, 0x0112);
    push_u16(&mut exif, 3);
    push_u32(&mut exif, 1);
    push_u16(&mut exif, 1);
    push_u16(&mut exif, 0);
    push_u16(&mut exif, 0x8769);
    push_u16(&mut exif, 4);
    push_u32(&mut exif, 1);
    push_u32(&mut exif, 38);
    push_u32(&mut exif, 0);
    push_u16(&mut exif, 1);
    push_u16(&mut exif, 0x9003);
    push_u16(&mut exif, 2);
    push_u32(&mut exif, date.len() as u32);
    push_u32(&mut exif, 56);
    push_u32(&mut exif, 0);
    exif.extend_from_slice(&date);
    exif
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn new_image_like(
    source: &DynamicImage,
    width: u32,
    height: u32,
) -> Result<DynamicImage, ProcessError> {
    let image = match source {
        DynamicImage::ImageLuma8(_) => DynamicImage::new_luma8(width, height),
        DynamicImage::ImageLumaA8(_) => DynamicImage::new_luma_a8(width, height),
        DynamicImage::ImageRgb8(_) => DynamicImage::new_rgb8(width, height),
        DynamicImage::ImageRgba8(_) => DynamicImage::new_rgba8(width, height),
        DynamicImage::ImageLuma16(_) => DynamicImage::new_luma16(width, height),
        DynamicImage::ImageLumaA16(_) => DynamicImage::new_luma_a16(width, height),
        DynamicImage::ImageRgb16(_) => DynamicImage::new_rgb16(width, height),
        DynamicImage::ImageRgba16(_) => DynamicImage::new_rgba16(width, height),
        _ => {
            return Err(ProcessError::fidelity(
                "decoded color representation cannot be resized without conversion",
            ));
        }
    };
    Ok(image)
}

fn transform_profile(
    image: &DynamicImage,
    source: &ColorProfile,
    destination: &ColorProfile,
) -> Result<DynamicImage, ProcessError> {
    macro_rules! transform_8 {
        ($buffer:expr, $layout:expr, $variant:ident) => {{
            let raw = $buffer.as_raw();
            let transform = source
                .create_transform_8bit($layout, destination, $layout, TransformOptions::default())
                .map_err(ProcessError::fidelity)?;
            let mut output = vec![0_u8; raw.len()];
            transform
                .transform(raw, &mut output)
                .map_err(ProcessError::fidelity)?;
            DynamicImage::$variant(
                ImageBuffer::from_raw(image.width(), image.height(), output)
                    .ok_or_else(|| ProcessError::failure("invalid transformed image buffer"))?,
            )
        }};
    }
    macro_rules! transform_16 {
        ($buffer:expr, $layout:expr, $variant:ident) => {{
            let raw = $buffer.as_raw();
            let transform = source
                .create_transform_16bit($layout, destination, $layout, TransformOptions::default())
                .map_err(ProcessError::fidelity)?;
            let mut output = vec![0_u16; raw.len()];
            transform
                .transform(raw, &mut output)
                .map_err(ProcessError::fidelity)?;
            DynamicImage::$variant(
                ImageBuffer::from_raw(image.width(), image.height(), output)
                    .ok_or_else(|| ProcessError::failure("invalid transformed image buffer"))?,
            )
        }};
    }

    Ok(match image {
        DynamicImage::ImageRgb8(buffer) => transform_8!(buffer, Layout::Rgb, ImageRgb8),
        DynamicImage::ImageRgba8(buffer) => transform_8!(buffer, Layout::Rgba, ImageRgba8),
        DynamicImage::ImageRgb16(buffer) => transform_16!(buffer, Layout::Rgb, ImageRgb16),
        DynamicImage::ImageRgba16(buffer) => transform_16!(buffer, Layout::Rgba, ImageRgba16),
        DynamicImage::ImageLuma8(_)
        | DynamicImage::ImageLumaA8(_)
        | DynamicImage::ImageLuma16(_)
        | DynamicImage::ImageLumaA16(_) => {
            return Err(ProcessError::fidelity(
                "embedded ICC profiles on grayscale images are not yet safely transformable",
            ));
        }
        _ => {
            return Err(ProcessError::fidelity(
                "embedded ICC profile uses an unsupported pixel representation",
            ));
        }
    })
}

fn encode(
    image: &DynamicImage,
    format: SupportedFormat,
    metadata: &Metadata,
    jpeg_quality: u8,
) -> Result<Vec<u8>, ProcessError> {
    let mut output = Vec::new();
    let color: ExtendedColorType = image.color().into();
    let (width, height) = image.dimensions();

    match format {
        SupportedFormat::Jpeg => {
            let mut encoder =
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, jpeg_quality);
            set_metadata(&mut encoder, metadata)?;
            encoder
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
        SupportedFormat::Png => {
            let mut encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut output,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            set_metadata(&mut encoder, metadata)?;
            encoder
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
        SupportedFormat::WebP => {
            let mut encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut output);
            set_metadata(&mut encoder, metadata)?;
            encoder
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
        SupportedFormat::Bmp => {
            image::codecs::bmp::BmpEncoder::new(&mut output)
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
        SupportedFormat::Tiff => {
            let mut cursor = Cursor::new(&mut output);
            let mut encoder = image::codecs::tiff::TiffEncoder::new(&mut cursor);
            if let Some(icc) = &metadata.icc {
                encoder
                    .set_icc_profile(icc.clone())
                    .map_err(ProcessError::fidelity)?;
            }
            encoder
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
        SupportedFormat::Gif => {
            image::codecs::gif::GifEncoder::new(&mut output)
                .write_image(image.as_bytes(), width, height, color)
                .map_err(ProcessError::failure)?;
        }
    }
    Ok(output)
}

fn set_metadata(encoder: &mut impl ImageEncoder, metadata: &Metadata) -> Result<(), ProcessError> {
    if let Some(icc) = &metadata.icc {
        encoder
            .set_icc_profile(icc.clone())
            .map_err(ProcessError::fidelity)?;
    }
    if let Some(exif) = &metadata.exif {
        encoder
            .set_exif_metadata(exif.clone())
            .map_err(ProcessError::fidelity)?;
    }
    Ok(())
}

fn verify(
    bytes: &[u8],
    details: &ImageDetails,
    resized: &DynamicImage,
    source_has_alpha: bool,
    expected_metadata: &Metadata,
) -> Result<(), ProcessError> {
    verify_container_metadata(bytes, details.format, expected_metadata)?;
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(ProcessError::failure)?;
    if reader.format() != Some(image_format(details.format)) {
        return Err(ProcessError::failure(
            "candidate format differs from the source format",
        ));
    }
    let mut decoder = reader.into_decoder().map_err(ProcessError::failure)?;
    if decoder.dimensions()
        != (
            details.output_dimensions.width,
            details.output_dimensions.height,
        )
    {
        return Err(ProcessError::failure(
            "candidate dimensions differ from the calculated dimensions",
        ));
    }
    if decoder.orientation().map_err(ProcessError::failure)? != Orientation::NoTransforms {
        return Err(ProcessError::failure(
            "candidate still requests a visual orientation transform",
        ));
    }
    if source_has_alpha && (!resized.color().has_alpha() || !decoder.color_type().has_alpha()) {
        return Err(ProcessError::fidelity(
            "candidate did not retain the source alpha channel",
        ));
    }
    if decoder.icc_profile().map_err(ProcessError::failure)? != expected_metadata.icc {
        return Err(ProcessError::failure(
            "candidate ICC profile differs from the source profile",
        ));
    }
    if decoder.exif_metadata().map_err(ProcessError::failure)? != expected_metadata.exif {
        return Err(ProcessError::failure(
            "candidate EXIF metadata differs from the required metadata",
        ));
    }

    let candidate = DynamicImage::from_decoder(decoder).map_err(ProcessError::failure)?;
    if !matches!(details.format, SupportedFormat::Jpeg | SupportedFormat::Gif)
        && (candidate.color() != resized.color() || candidate.as_bytes() != resized.as_bytes())
    {
        return Err(ProcessError::fidelity(
            "candidate pixel colors or transparency differ after round-trip verification",
        ));
    }
    if source_has_alpha && alpha_samples(&candidate) != alpha_samples(resized) {
        return Err(ProcessError::fidelity(
            "candidate transparency values differ after round-trip verification",
        ));
    }
    Ok(())
}

fn verify_container_metadata(
    bytes: &[u8],
    format: SupportedFormat,
    expected: &Metadata,
) -> Result<(), ProcessError> {
    if !matches!(
        format,
        SupportedFormat::Jpeg | SupportedFormat::Png | SupportedFormat::WebP
    ) {
        return Ok(());
    }
    let container = DynImage::from_bytes(bytes.to_vec().into())
        .map_err(ProcessError::failure)?
        .ok_or_else(|| ProcessError::failure("candidate container could not be identified"))?;
    if container.icc_profile().as_deref() != expected.icc.as_deref() {
        return Err(ProcessError::failure(
            "container ICC profile differs from the required profile",
        ));
    }
    if container.exif().as_deref() != expected.exif.as_deref() {
        return Err(ProcessError::failure(
            "container EXIF metadata differs from the required metadata",
        ));
    }
    Ok(())
}

fn alpha_samples(image: &DynamicImage) -> Option<Vec<u16>> {
    match image {
        DynamicImage::ImageLumaA8(buffer) => {
            Some(buffer.pixels().map(|pixel| u16::from(pixel[1])).collect())
        }
        DynamicImage::ImageRgba8(buffer) => {
            Some(buffer.pixels().map(|pixel| u16::from(pixel[3])).collect())
        }
        DynamicImage::ImageLumaA16(buffer) => Some(buffer.pixels().map(|pixel| pixel[1]).collect()),
        DynamicImage::ImageRgba16(buffer) => Some(buffer.pixels().map(|pixel| pixel[3]).collect()),
        _ => None,
    }
}

fn image_format(format: SupportedFormat) -> ImageFormat {
    match format {
        SupportedFormat::Jpeg => ImageFormat::Jpeg,
        SupportedFormat::Png => ImageFormat::Png,
        SupportedFormat::WebP => ImageFormat::WebP,
        SupportedFormat::Bmp => ImageFormat::Bmp,
        SupportedFormat::Tiff => ImageFormat::Tiff,
        SupportedFormat::Gif => ImageFormat::Gif,
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, path::PathBuf};

    use image::{Rgba, RgbaImage};

    use crate::{
        inspection::{Classification, DEFAULT_MAX_PIXELS, inspect_files},
        plan::Bounds,
    };

    use super::*;

    fn eligible_details(path: &Path, bounds: Bounds) -> ImageDetails {
        let source = path.parent().unwrap();
        let relative = PathBuf::from(path.file_name().unwrap());
        let inspected = inspect_files(source, &[relative], bounds, DEFAULT_MAX_PIXELS);
        match inspected.into_iter().next().unwrap().classification {
            Classification::Eligible(details) => details,
            other => panic!("expected eligible image, got {other:?}"),
        }
    }

    fn write_rgba_png(path: &Path, width: u32, height: u32) {
        let mut image = RgbaImage::new(width, height);
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            *pixel = Rgba([
                (x.wrapping_mul(31) ^ y.wrapping_mul(7)) as u8,
                (x.wrapping_mul(11) ^ y.wrapping_mul(19)) as u8,
                x.wrapping_add(y) as u8,
                ((x + y) % 255) as u8,
            ]);
        }
        image.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn insert_png_text_chunk(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let iend = bytes
            .windows(4)
            .rposition(|window| window == b"IEND")
            .unwrap()
            - 4;
        let kind = *b"tEXt";
        let contents = b"Comment\0native PNG text";
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(contents.len() as u32).to_be_bytes());
        chunk.extend_from_slice(&kind);
        chunk.extend_from_slice(contents);
        chunk.extend_from_slice(&png_crc(kind, contents).to_be_bytes());

        let mut output = Vec::with_capacity(bytes.len() + chunk.len());
        output.extend_from_slice(&bytes[..iend]);
        output.extend_from_slice(&chunk);
        output.extend_from_slice(&bytes[iend..]);
        std::fs::write(path, output).unwrap();
    }

    fn png_crc(kind: [u8; 4], contents: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in kind.iter().chain(contents) {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    fn insert_jpeg_comment(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let comment = b"native JPEG comment";
        let mut output = Vec::with_capacity(bytes.len() + comment.len() + 4);
        output.extend_from_slice(&bytes[..2]);
        output.extend_from_slice(&[0xff, 0xfe]);
        output.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
        output.extend_from_slice(comment);
        output.extend_from_slice(&bytes[2..]);
        std::fs::write(path, output).unwrap();
    }

    fn insert_gif_comment(path: &Path) {
        let bytes = std::fs::read(path).unwrap();
        let trailer = bytes.iter().rposition(|byte| *byte == 0x3b).unwrap();
        let comment = b"native GIF comment";
        let mut output = Vec::with_capacity(bytes.len() + comment.len() + 4);
        output.extend_from_slice(&bytes[..trailer]);
        output.extend_from_slice(&[0x21, 0xfe, comment.len() as u8]);
        output.extend_from_slice(comment);
        output.push(0);
        output.extend_from_slice(&bytes[trailer..]);
        std::fs::write(path, output).unwrap();
    }

    #[test]
    fn creates_verified_beneficial_png_without_touching_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.png");
        write_rgba_png(&path, 300, 150);
        let source_before = std::fs::read(&path).unwrap();
        let details = eligible_details(&path, Bounds::new(60, 30).unwrap());

        let outcome = process_image(&path, &details, ProcessingOptions::default());

        let ProcessingOutcome::Reduced(candidate) = outcome else {
            panic!("expected a reduced candidate")
        };
        assert_eq!(candidate.dimensions, Dimensions::from((60, 30)));
        assert!(candidate.bytes.len() < source_before.len());
        assert_eq!(std::fs::read(path).unwrap(), source_before);
        let decoded = image::load_from_memory_with_format(&candidate.bytes, ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (60, 30));
        assert!(decoded.pixels().any(|pixel| pixel.0[3] < 255));
    }

    #[test]
    fn reports_candidate_that_is_not_smaller() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("large.png");
        write_rgba_png(&path, 100, 50);
        let mut details = eligible_details(&path, Bounds::new(20, 10).unwrap());
        details.source_bytes = 1;

        let outcome = process_image(&path, &details, ProcessingOptions::default());

        assert!(matches!(outcome, ProcessingOutcome::NotBeneficial { .. }));
    }

    #[test]
    fn default_metadata_keeps_capture_date_and_normalizes_orientation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("oriented.jpg");
        let file = File::create(&path).unwrap();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 95);
        encoder
            .set_exif_metadata(exif_with_date_orientation_and_gps())
            .unwrap();
        let pixels = vec![100; 300 * 150 * 3];
        encoder
            .write_image(&pixels, 300, 150, ExtendedColorType::Rgb8)
            .unwrap();
        let details = eligible_details(&path, Bounds::new(60, 30).unwrap());

        let outcome = process_image(&path, &details, ProcessingOptions::default());

        let ProcessingOutcome::Reduced(candidate) = outcome else {
            panic!("expected a reduced candidate")
        };
        let reader = ImageReader::with_format(Cursor::new(&candidate.bytes), ImageFormat::Jpeg);
        let mut decoder = reader.into_decoder().unwrap();
        assert_eq!(decoder.orientation().unwrap(), Orientation::NoTransforms);
        let exif = decoder.exif_metadata().unwrap().unwrap();
        let parsed = exif::Reader::new().read_raw(exif).unwrap();
        assert!(
            parsed
                .get_field(Tag::DateTimeOriginal, In::PRIMARY)
                .is_some()
        );
        assert!(parsed.get_field(Tag::GPSLatitudeRef, In::PRIMARY).is_none());
        assert!(candidate.warnings.iter().any(|warning| {
            warning.contains("GPS") && warning.contains("--preserve-all-metadata")
        }));
    }

    #[test]
    fn preserve_all_rejects_native_text_metadata_that_cannot_be_reencoded() {
        type MetadataCase = (&'static str, ImageFormat, fn(&Path));

        let directory = tempfile::tempdir().unwrap();
        let cases: [MetadataCase; 3] = [
            ("text.png", ImageFormat::Png, insert_png_text_chunk),
            ("comment.jpg", ImageFormat::Jpeg, insert_jpeg_comment),
            ("comment.gif", ImageFormat::Gif, insert_gif_comment),
        ];

        for (name, format, add_metadata) in cases {
            let path = directory.path().join(name);
            let image = image::RgbImage::from_fn(240, 120, |x, y| {
                image::Rgb([
                    x.wrapping_mul(17).wrapping_add(y.wrapping_mul(3)) as u8,
                    x.wrapping_mul(5).wrapping_add(y.wrapping_mul(13)) as u8,
                    x.wrapping_mul(23).wrapping_add(y.wrapping_mul(29)) as u8,
                ])
            });
            let mut file = File::create(&path).unwrap();
            DynamicImage::ImageRgb8(image)
                .write_to(&mut file, format)
                .unwrap();
            file.flush().unwrap();
            add_metadata(&path);
            let details = eligible_details(&path, Bounds::new(48, 24).unwrap());

            let default_outcome = process_image(&path, &details, ProcessingOptions::default());
            assert!(
                matches!(default_outcome, ProcessingOutcome::Reduced(ref candidate) if candidate.warnings.iter().any(|warning| warning.contains("was removed"))),
                "expected a native metadata removal warning for {name}, got {default_outcome:?}"
            );

            let outcome = process_image(
                &path,
                &details,
                ProcessingOptions {
                    preserve_all_metadata: true,
                    ..ProcessingOptions::default()
                },
            );

            assert!(
                matches!(outcome, ProcessingOutcome::FidelityConflict { ref reason } if reason.contains("text") || reason.contains("comment")),
                "expected a native metadata fidelity conflict for {name}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn processes_every_supported_static_format() {
        let directory = tempfile::tempdir().unwrap();
        let formats = [
            ("photo.jpg", ImageFormat::Jpeg),
            ("photo.png", ImageFormat::Png),
            ("photo.webp", ImageFormat::WebP),
            ("photo.bmp", ImageFormat::Bmp),
            ("photo.tiff", ImageFormat::Tiff),
            ("photo.gif", ImageFormat::Gif),
        ];

        for (name, format) in formats {
            let path = directory.path().join(name);
            let image = image::RgbImage::from_fn(240, 120, |x, y| {
                image::Rgb([
                    x.wrapping_mul(17).wrapping_add(y.wrapping_mul(3)) as u8,
                    x.wrapping_mul(5).wrapping_add(y.wrapping_mul(13)) as u8,
                    x.wrapping_mul(23).wrapping_add(y.wrapping_mul(29)) as u8,
                ])
            });
            DynamicImage::ImageRgb8(image)
                .save_with_format(&path, format)
                .unwrap();
            let details = eligible_details(&path, Bounds::new(48, 24).unwrap());

            let outcome = process_image(&path, &details, ProcessingOptions::default());

            assert!(
                matches!(
                    outcome,
                    ProcessingOutcome::Reduced(_) | ProcessingOutcome::NotBeneficial { .. }
                ),
                "unexpected processing outcome for {name}: {outcome:?}"
            );
        }
    }

    fn exif_with_date_orientation_and_gps() -> Vec<u8> {
        let date = b"2026:08:22 10:11:12\0";
        let mut output = Vec::new();
        output.extend_from_slice(b"II\x2a\0\x08\0\0\0");
        push_u16(&mut output, 3);
        push_u16(&mut output, 0x0112);
        push_u16(&mut output, 3);
        push_u32(&mut output, 1);
        push_u16(&mut output, 6);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0x8769);
        push_u16(&mut output, 4);
        push_u32(&mut output, 1);
        push_u32(&mut output, 50);
        push_u16(&mut output, 0x8825);
        push_u16(&mut output, 4);
        push_u32(&mut output, 1);
        push_u32(&mut output, 68);
        push_u32(&mut output, 0);
        push_u16(&mut output, 1);
        push_u16(&mut output, 0x9003);
        push_u16(&mut output, 2);
        push_u32(&mut output, date.len() as u32);
        push_u32(&mut output, 86);
        push_u32(&mut output, 0);
        push_u16(&mut output, 1);
        push_u16(&mut output, 0x0001);
        push_u16(&mut output, 2);
        push_u32(&mut output, 2);
        output.extend_from_slice(b"N\0\0\0");
        push_u32(&mut output, 0);
        output.extend_from_slice(date);
        output
    }
}
