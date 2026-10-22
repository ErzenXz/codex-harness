//! Bounded workspace image input; no image decoder or subprocess is needed.
use crate::{agent::AgentContext, computer::IMAGE_LIMIT, multimodal::image_dimensions};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

pub fn definition() -> Value {
    json!({"type":"function","name":"view_image","description":"View a local workspace PNG or JPEG. Maximum 96KiB, 4096 pixels per side and 8 million pixels. Resize/compress larger images first; protected paths are denied.","strict":true,"parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}})
}

pub async fn execute(ctx: &AgentContext, args: &Value, _call_id: &str) -> Result<Value> {
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    let name = args["path"].as_str().context("Missing string: path")?;
    let path = ctx.workspace.resolve(name, false).await?;
    ensure!(
        !path.starts_with(&ctx.config.state_dir),
        "Harness state is protected"
    );
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    let file = options.open(&path).await?;
    let metadata = file.metadata().await?;
    ensure!(metadata.is_file(), "Image must be a regular file");
    crate::workspace::ensure_private_file(&metadata)?;
    ensure!(
        metadata.len() <= IMAGE_LIMIT as u64,
        "Image exceeds 96KiB; resize or compress it to <=96KiB PNG/JPEG before viewing"
    );
    let mut bytes = Vec::new();
    let mut limited = file.take(IMAGE_LIMIT as u64 + 1);
    tokio::select! {
        _ = ctx.cancel.cancelled() => anyhow::bail!("Cancelled"),
        result = limited.read_to_end(&mut bytes) => { result?; }
    }
    let (mime, width, height) = validate(&bytes)?;
    ensure!(!ctx.cancel.is_cancelled(), "Cancelled");
    Ok(
        json!({"content":[{"type":"text","text":format!("Workspace image: {width}x{height} ({mime}).")},{"type":"image","mimeType":mime,"data":STANDARD.encode(&bytes)}]}),
    )
}

fn validate(bytes: &[u8]) -> Result<(&'static str, u32, u32)> {
    ensure!(
        bytes.len() <= IMAGE_LIMIT,
        "Image exceeds 96KiB; resize or compress it before viewing"
    );
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        ensure!(
            bytes.ends_with(b"\0\0\0\0IEND\xaeB`\x82"),
            "Truncated PNG: missing IEND trailer"
        );
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8]) {
        ensure!(
            bytes.ends_with(&[0xff, 0xd9]),
            "Truncated JPEG: missing end marker"
        );
        "image/jpeg"
    } else {
        anyhow::bail!("Unsupported image; save it as PNG or JPEG before viewing")
    };
    let (width, height) = image_dimensions(mime, bytes).context("Invalid PNG/JPEG image header")?;
    ensure!(width > 0 && height > 0, "Image has invalid zero dimensions");
    ensure!(
        width <= 4096 && height <= 4096 && u64::from(width) * u64::from(height) <= 8_000_000,
        "Image dimensions exceed 4096 per side or 8 million pixels; resize it before viewing"
    );
    Ok((mime, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_png_round_trips_through_provider_and_acp_content() {
        let bytes = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=").unwrap();
        assert_eq!(validate(&bytes).unwrap(), ("image/png", 1, 1));
        let output = crate::multimodal::normalize(
            &json!({"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(bytes)}]}),
            1024,
        );
        assert_eq!(output.images.len(), 1);
        assert_eq!(
            crate::multimodal::acp_content(&Value::Array(output.images))[1]["content"]["mimeType"],
            "image/png"
        );
    }

    #[test]
    fn rejects_unsupported_truncated_and_over_budget_images() {
        assert!(validate(b"GIF89a").is_err());
        assert!(validate(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(validate(&[0xff, 0xd8, 0xff]).is_err());
        assert!(validate(&vec![0; IMAGE_LIMIT + 1]).is_err());
        let mut header = vec![0; 33];
        header[..16].copy_from_slice(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        for (w, h) in [(0u32, 1u32), (4097, 1), (3000, 3000)] {
            header[16..20].copy_from_slice(&w.to_be_bytes());
            header[20..24].copy_from_slice(&h.to_be_bytes());
            assert!(validate(&header).is_err());
        }
    }
}
