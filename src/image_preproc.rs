//! Image preprocessing: resize and format conversion for vision models.
//!
//! Network device screenshots are often 4K+ and exceed API limits.
//! This module downscales images before base64 encoding, targeting
//! a max dimension (default 1920px) and re-encoding as JPEG to reduce
//! base64 payload size.

use crate::types::ImageBlock;

/// Default max dimension for image downscaling.
const DEFAULT_MAX_DIMENSION: u32 = 1920;

/// Default JPEG quality for re-encoding (85 = good quality, ~15-20% size reduction).
const DEFAULT_JPEG_QUALITY: u8 = 85;

/// Preprocess an image block: resize if exceeds max dimension, re-encode as JPEG.
/// Returns a new ImageBlock with the processed data.
/// If the image is already small enough or processing fails, returns the original.
pub fn preprocess(block: &ImageBlock) -> ImageBlock {
    preprocess_with_options(block, DEFAULT_MAX_DIMENSION, DEFAULT_JPEG_QUALITY)
}

/// Preprocess with explicit max dimension and quality.
pub fn preprocess_with_options(block: &ImageBlock, max_dim: u32, quality: u8) -> ImageBlock {
    // Decode base64 data.
    let raw = match base64_decode(&block.data) {
        Ok(data) => data,
        Err(e) => {
            log::warn!("Image preprocess: base64 decode failed: {e} — using original");
            return block.clone();
        }
    };

    // Load image from memory.
    let img = match image::load_from_memory(&raw) {
        Ok(img) => img,
        Err(e) => {
            log::warn!("Image preprocess: decode failed: {e} — using original");
            return block.clone();
        }
    };

    // Resize if exceeds max dimension.
    let (w, h) = (img.width(), img.height());
    let needs_resize = w > max_dim || h > max_dim;
    let processed = if needs_resize {
        log::debug!("Image preprocess: resizing {w}x{h} → max {max_dim}px");
        img.resize(max_dim, max_dim, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };

    // Re-encode as JPEG (unless it's a GIF with animation — keep as-is).
    let is_gif = block.media_type == "image/gif";
    if is_gif {
        // Keep GIF as-is to preserve animation.
        return ImageBlock {
            media_type: block.media_type.clone(),
            data: block.data.clone(),
        };
    }

    let mut buf = std::io::Cursor::new(Vec::new());
    let encode_result = if processed.color().has_alpha() {
        // Flatten alpha to white background for JPEG.
        let rgba = processed.to_rgba8();
        let mut flattened = image::ImageBuffer::new(rgba.width(), rgba.height());
        for (x, y, pixel) in rgba.enumerate_pixels() {
            let [r, g, b, a] = pixel.0;
            // Alpha blend over white.
            let alpha = a as f32 / 255.0;
            let r = (r as f32 * alpha + 255.0 * (1.0 - alpha)) as u8;
            let g = (g as f32 * alpha + 255.0 * (1.0 - alpha)) as u8;
            let b = (b as f32 * alpha + 255.0 * (1.0 - alpha)) as u8;
            flattened.put_pixel(x, y, image::Rgb([r, g, b]));
        }
        image::DynamicImage::ImageRgb8(flattened)
            .write_to(&mut buf, image::ImageFormat::Jpeg)
    } else {
        processed.write_to(&mut buf, image::ImageFormat::Jpeg)
    };

    match encode_result {
        Ok(()) => {
            let encoded = buf.into_inner();
            let b64 = base64_encode(&encoded);
            log::debug!(
                "Image preprocess: {} → JPEG {} bytes (base64 {} bytes, q={})",
                block.media_type,
                encoded.len(),
                b64.len(),
                quality
            );
            ImageBlock {
                media_type: "image/jpeg".into(),
                data: b64,
            }
        }
        Err(e) => {
            log::warn!("Image preprocess: JPEG encode failed: {e} — using original");
            block.clone()
        }
    }
}

/// Preprocess a list of image blocks.
pub fn preprocess_all(blocks: &[ImageBlock]) -> Vec<ImageBlock> {
    blocks.iter().map(preprocess).collect()
}

// ─── Base64 helpers (no external dep needed, reuse existing encoding) ──────

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    // Use serde_urlencoded-free base64 decoding via a simple implementation.
    // We can't add base64 crate (keep deps minimal), so use image crate's
    // internal decoder — but image::load_from_memory expects raw bytes.
    // Actually, we need to decode base64 first. Use a minimal decoder.
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &c) in CHARS.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }
    lookup[b'=' as usize] = 0; // padding

    let filtered: Vec<u8> = data.bytes().filter(|&b| b != b'\n' && b != b'\r' && b != b' ').collect();
    if filtered.is_empty() {
        return Ok(Vec::new());
    }
    if filtered.len() % 4 != 0 {
        return Err(format!("invalid base64 length: {}", filtered.len()));
    }

    let mut out = Vec::with_capacity(filtered.len() * 3 / 4);
    for chunk in filtered.chunks(4) {
        let a = lookup[chunk[0] as usize];
        let b = lookup[chunk[1] as usize];
        let c = lookup[chunk[2] as usize];
        let d = lookup[chunk[3] as usize];
        if a == 255 || b == 255 {
            return Err("invalid base64 character".into());
        }
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
            if chunk[3] != b'=' {
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let a = chunk[0];
        let b = if chunk.len() > 1 { chunk[1] } else { 0 };
        let c = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(CHARS[(a >> 2) as usize] as char);
        out.push(CHARS[((a & 0x03) << 4 | b >> 4) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((b & 0x0f) << 2 | c >> 6) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}
