//! Shared raster-image normalization for file reads and clipboard pastes.
//! Keeping it in core lets every frontend use exactly the same input budget.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ImageEncoder, ImageFormat, Limits, RgbaImage};

use crate::ContentBlock;

pub const MAX_LONG_EDGE: u32 = 1568;
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const PASSTHROUGH_BYTES: usize = 2 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 128 * 1024 * 1024;

/// Sniff a supported raster image by magic bytes; extensions are not trusted.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub struct NormalizedImage {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
}

impl NormalizedImage {
    pub fn into_block(self) -> ContentBlock {
        use base64::Engine as _;
        ContentBlock::Image {
            media_type: self.media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(self.bytes),
        }
    }
}

/// Normalize an encoded PNG/JPEG/GIF/WebP. Small compliant source files stay
/// byte-for-byte intact so freshness hashes retain their original semantics.
pub fn normalize_image(bytes: &[u8]) -> Result<NormalizedImage, String> {
    let media_type =
        detect_image_mime(bytes).ok_or_else(|| "unsupported image format".to_string())?;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format_for(media_type));
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| format!("cannot decode image: {e}"))?;
    let (width, height) = (image.width(), image.height());
    let resized = width.max(height) > MAX_LONG_EDGE;
    if !resized && bytes.len() <= PASSTHROUGH_BYTES {
        return Ok(NormalizedImage {
            media_type,
            bytes: bytes.to_vec(),
            width,
            height,
            resized: false,
        });
    }
    normalize_dynamic(image, width, height, resized)
}

/// Normalize clipboard RGBA pixels without an unnecessary decode round-trip.
pub fn normalize_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<NormalizedImage, String> {
    let pixels = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "image dimensions are too large".to_string())? as usize;
    if rgba.len() != pixels {
        return Err(format!(
            "RGBA buffer is {} bytes; expected {pixels}",
            rgba.len()
        ));
    }
    normalize_dynamic(
        DynamicImage::ImageRgba8(
            RgbaImage::from_raw(width, height, rgba).expect("validated RGBA buffer"),
        ),
        width,
        height,
        width.max(height) > MAX_LONG_EDGE,
    )
}

fn format_for(media_type: &str) -> ImageFormat {
    match media_type {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::WebP,
        _ => unreachable!("detect_image_mime returned an unsupported type"),
    }
}

fn normalize_dynamic(
    image: DynamicImage,
    source_width: u32,
    source_height: u32,
    resized: bool,
) -> Result<NormalizedImage, String> {
    let image = if resized {
        image.resize(MAX_LONG_EDGE, MAX_LONG_EDGE, FilterType::Triangle)
    } else {
        image
    };
    let (width, height) = (image.width(), image.height());
    let alpha = image.to_rgba8().pixels().any(|pixel| pixel[3] != u8::MAX);
    let mut bytes = Vec::new();
    let media_type = if alpha {
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| format!("cannot encode PNG: {e}"))?;
        "image/png"
    } else {
        let rgb = image.to_rgb8();
        JpegEncoder::new_with_quality(&mut bytes, 80)
            .write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            .map_err(|e| format!("cannot encode JPEG: {e}"))?;
        "image/jpeg"
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "normalized image is {:.1} MB, exceeding the {} MB inline limit",
            bytes.len() as f64 / (1024.0 * 1024.0),
            MAX_IMAGE_BYTES / (1024 * 1024),
        ));
    }
    Ok(NormalizedImage {
        media_type,
        bytes,
        width,
        height,
        resized: resized || width != source_width || height != source_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let pixel = if alpha {
            image::Rgba([1, 2, 3, 120])
        } else {
            image::Rgba([1, 2, 3, 255])
        };
        let image = RgbaImage::from_pixel(width, height, pixel);
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn large_images_are_resized_proportionally() {
        let normalized = normalize_image(&png(4000, 2000, false)).unwrap();
        assert_eq!((normalized.width, normalized.height), (1568, 784));
        assert!(normalized.resized);
    }

    #[test]
    fn small_images_preserve_their_source_bytes() {
        let source = png(20, 10, false);
        let normalized = normalize_image(&source).unwrap();
        assert_eq!(normalized.bytes, source);
        assert!(!normalized.resized);
    }

    #[test]
    fn transparency_stays_png_when_reencoded() {
        let normalized = normalize_image(&png(4000, 2000, true)).unwrap();
        assert_eq!(normalized.media_type, "image/png");
    }

    #[test]
    fn rgba_input_encodes_as_a_detectable_image() {
        let normalized = normalize_rgba(2, 2, vec![0; 16]).unwrap();
        assert_eq!(detect_image_mime(&normalized.bytes), Some("image/png"));
    }
}
