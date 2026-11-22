//! Zero-dependency image dimension probing (PNG, JPEG, WebP) and optional `ffmpeg`-backed
//! long-edge downscaling for inbound chat images.
//!
//! Keeps multimodal context usage bounded on long-lived QQ/Telegram conversations without adding
//! heavy image-codec crates to the release binary. Animated GIFs and non-raster files are never
//! modified; if `ffmpeg` is unavailable or fails, the original image is preserved untouched.

use std::path::Path;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

const RESIZE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_PROBE_BYTES: usize = 65_536;

static FFMPEG_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

fn cached_ffmpeg_path() -> Option<&'static Path> {
    FFMPEG_PATH
        .get_or_init(|| which::which("ffmpeg").ok())
        .as_deref()
}

/// Probes `(width, height)` directly from PNG, JPEG, or WebP headers without external crates.
#[must_use]
pub(crate) fn probe_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(dim) = probe_png(bytes) {
        return Some(dim);
    }
    if let Some(dim) = probe_jpeg(bytes) {
        return Some(dim);
    }
    probe_webp(bytes)
}

fn probe_png(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w > 0 && h > 0).then_some((w, h))
}

fn probe_jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 1 < bytes.len() {
        while i < bytes.len() && bytes[i] != 0xFF {
            i += 1;
        }
        while i < bytes.len() && bytes[i] == 0xFF {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let marker = bytes[i];
        i += 1;

        // Standalone markers without a length payload.
        if marker == 0x00 || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 {
                break;
            }
            continue;
        }
        // Start of Scan: image data follows, headers are finished.
        if marker == 0xDA {
            break;
        }

        if i + 2 > bytes.len() {
            break;
        }
        let seg_len = usize::from(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
        if seg_len < 2 || i + seg_len > bytes.len() {
            break;
        }

        // SOF0..SOF3, SOF5..SOF7, SOF9..SOF11, SOF13..SOF15 encode image dimensions.
        let is_sof = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        if is_sof && seg_len >= 7 {
            let h = u32::from(u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]));
            let w = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
            return (w > 0 && h > 0).then_some((w, h));
        }

        i += seg_len;
    }
    None
}

fn probe_webp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    let chunk = &bytes[12..16];
    if chunk == b"VP8X" {
        let w =
            1 + (u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16));
        let h =
            1 + (u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16));
        return (w > 0 && h > 0).then_some((w, h));
    }
    if chunk == b"VP8 " && bytes[23..26] == [0x9D, 0x01, 0x2A] {
        let w = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF);
        let h = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF);
        return (w > 0 && h > 0).then_some((w, h));
    }
    if chunk == b"VP8L" && bytes[20] == 0x2F {
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let w = (bits & 0x3FFF) + 1;
        let h = ((bits >> 14) & 0x3FFF) + 1;
        return (w > 0 && h > 0).then_some((w, h));
    }
    None
}

fn is_resizable_raster_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("jpg")
                || ext.eq_ignore_ascii_case("jpeg")
                || ext.eq_ignore_ascii_case("png")
                || ext.eq_ignore_ascii_case("webp")
                || ext.eq_ignore_ascii_case("bmp")
        })
}

async fn probe_file_dimensions(path: &Path) -> Option<(u32, u32)> {
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut buf = vec![0u8; HEADER_PROBE_BYTES];
    let mut total = 0usize;
    while total < buf.len() {
        let n = file.read(&mut buf[total..]).await.ok()?;
        if n == 0 {
            break;
        }
        total += n;
    }
    probe_image_dimensions(&buf[..total])
}

/// Downscales `path` in-place when its longest edge exceeds `max_edge`, returning the new byte
/// size if resized. Returns `None` when no resize was needed, `max_edge == 0`, `ffmpeg` is
/// unavailable, or resizing fails (preserving the original file).
pub(crate) async fn maybe_downscale_image(path: &Path, max_edge: u32) -> Option<u64> {
    if max_edge == 0 || !is_resizable_raster_ext(path) {
        return None;
    }

    let (w, h) = probe_file_dimensions(path).await?;
    let longest = w.max(h);
    if longest <= max_edge {
        return None;
    }

    let ffmpeg = cached_ffmpeg_path()?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let tmp_path = path.with_file_name(format!(".{stem}.resize-{}.{ext}", std::process::id()));

    let scale_filter = format!(
        "scale='min({max_edge},iw)':'min({max_edge},ih)':force_original_aspect_ratio=decrease"
    );

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-nostdin", "-y", "-v", "error", "-i"])
        .arg(path)
        .args(["-vf", &scale_filter, "-frames:v", "1"]);

    if ext == "jpg" || ext == "jpeg" {
        cmd.args(["-q:v", "4"]);
    } else if ext == "webp" {
        cmd.args(["-quality", "82"]);
    }

    cmd.arg(&tmp_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    cmd.process_group(0);

    let output = match tokio::time::timeout(RESIZE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            debug!(path = %path.display(), error = %err, "ffmpeg spawn failed during image downscale");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return None;
        }
        Err(_) => {
            warn!(path = %path.display(), w, h, max_edge, "image downscale timed out; keeping original");
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            path = %path.display(),
            status = ?output.status.code(),
            stderr = %stderr.trim(),
            "ffmpeg image downscale failed; keeping original"
        );
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    let Ok(new_meta) = tokio::fs::metadata(&tmp_path).await else {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    };
    if !new_meta.is_file() || new_meta.len() == 0 {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    let old_bytes = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if let Err(err) = tokio::fs::rename(&tmp_path, path).await {
        warn!(path = %path.display(), error = %err, "failed to replace original with resized image");
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    let new_bytes = new_meta.len();
    info!(
        path = %path.display(),
        orig_w = w,
        orig_h = h,
        max_edge,
        old_bytes,
        new_bytes,
        "Downscaled inbound chat image"
    );
    Some(new_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_png_dimensions() {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&2560u32.to_be_bytes());
        png.extend_from_slice(&1440u32.to_be_bytes());
        assert_eq!(probe_image_dimensions(&png), Some((2560, 1440)));
    }

    #[test]
    fn probes_jpeg_sof0_after_app0_segment() {
        let mut jpg = vec![0xFF, 0xD8];
        // APP0 segment (length 6 = 2 len bytes + 4 payload bytes)
        jpg.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x06, b'J', b'F', b'I', b'F']);
        // SOF0 segment (length 11): precision=8, height=1920, width=1080
        jpg.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x07, 0x80, 0x04, 0x38, 0x03, 0x01, 0x11, 0x00,
        ]);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        assert_eq!(probe_image_dimensions(&jpg), Some((1080, 1920)));
    }

    #[test]
    fn probes_webp_vp8x_and_vp8l() {
        // VP8X: width=3000 (2999 = 0x000BB7), height=2000 (1999 = 0x0007CF)
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF\x20\x00\x00\x00WEBPVP8X");
        webp.extend_from_slice(&[0x0A, 0x00, 0x00, 0x00]); // chunk size
        webp.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // flags
        webp.extend_from_slice(&[0xB7, 0x0B, 0x00]); // width - 1
        webp.extend_from_slice(&[0xCF, 0x07, 0x00]); // height - 1
        assert_eq!(probe_image_dimensions(&webp), Some((3000, 2000)));

        // VP8L: width=1600 (1599), height=900 (899)
        let bits: u32 = 1599 | (899 << 14);
        let mut webp_l = Vec::new();
        webp_l.extend_from_slice(b"RIFF\x20\x00\x00\x00WEBPVP8L");
        webp_l.extend_from_slice(&[0x0A, 0x00, 0x00, 0x00]);
        webp_l.push(0x2F);
        webp_l.extend_from_slice(&bits.to_le_bytes());
        webp_l.extend_from_slice(&[0u8; 8]);
        assert_eq!(probe_image_dimensions(&webp_l), Some((1600, 900)));
    }

    #[test]
    fn ignores_gif_and_invalid_headers() {
        assert_eq!(probe_image_dimensions(b"GIF89a\x00\x05\x00\x05"), None);
        assert_eq!(probe_image_dimensions(b"not an image"), None);
        assert!(!is_resizable_raster_ext(Path::new("sticker.gif")));
        assert!(is_resizable_raster_ext(Path::new("photo.JPG")));
        assert!(is_resizable_raster_ext(Path::new("shot.webp")));
    }
}
