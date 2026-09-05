//! read_image tool: read an image file and return it as an ImageBlock
//! for the model to analyze.
//!
//! The tool reads a local image file, base64-encodes it, runs it through
//! the image preprocessor (resize + format conversion), and returns a
//! ToolResult with the image attached. The agent loop then attaches
//! these images to the conversation so the model can see them.

use crate::types::*;

/// Read an image file and return it as an ImageBlock.
/// Called by the read_image tool plugin.
pub fn read_image_file(path: &str) -> ToolResult {
    if path.is_empty() {
        return ToolResult {
            content: "Error: `path` is required".into(),
            is_error: true,
            images: vec![],
        };
    }

    // Read the file as bytes.
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return ToolResult {
                content: format!("Error reading image file: {e}"),
                is_error: true,
                images: vec![],
            };
        }
    };

    // Detect media type from file extension.
    let media_type = detect_media_type(path);

    // Base64-encode the raw bytes.
    let b64 = base64_encode(&bytes);

    // Create ImageBlock.
    let block = ImageBlock {
        media_type,
        data: b64,
    };

    // Preprocess (resize + re-encode as JPEG if too large).
    let processed = crate::image_preproc::preprocess(&block);

    // Return result with image attached.
    // The text content is a brief description for the model.
    ToolResult {
        content: format!(
            "Image loaded from {} ({} bytes, {} → {})",
            path,
            bytes.len(),
            block.media_type,
            processed.media_type
        ),
        is_error: false,
        images: vec![processed],
    }
}

fn detect_media_type(path: &str) -> String {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        // Default to JPEG — most vision models accept it.
        "image/jpeg"
    }
    .into()
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

/// The read_image tool plugin.
pub struct ReadImageTool;

impl crate::tools::ToolPlugin for ReadImageTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_image".into(),
            description: "Read an image file and return it for visual analysis. Supports PNG, JPEG, GIF, WebP, BMP. Images are auto-resized to max 1920px and re-encoded as JPEG.".into(),
            guidance: "Use this tool to read and analyze local image files (screenshots, diagrams, photos). The image will be sent to the model for visual analysis.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image file to read"
                    }
                },
                "required": ["path"]
            }),
            timeout_ms: 10_000,
        }
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        let path = args.get("path")
            .and_then(|p| if p.is_null() { None } else { p.as_str() })
            .unwrap_or("");
        read_image_file(path)
    }
}
