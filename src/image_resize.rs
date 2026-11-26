//! Zero-dependency image dimension probing (PNG, JPEG, WebP, BMP) and optional `ffmpeg`-backed
//! long-edge downscaling / opaque-image JPEG transcoding for inbound chat images.
//!
//! Keeps multimodal context usage and disk/IPC overhead bounded on long-lived QQ/Telegram
//! conversations without adding heavy image-codec crates to the release binary. Animated GIFs,
//! animated WebP/APNG files, and non-raster files are never modified; if `ffmpeg` is unavailable
//! or fails, the original image is preserved untouched.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, BufReader};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

const RESIZE_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_PREFIX_BYTES: usize = 64;
const JPEG_STREAM_BUF_BYTES: usize = 4096;
const MAX_JPEG_SEGMENTS: usize = 4096;
const MAX_JPEG_INTER_MARKER_BYTES: usize = 65_536;
const MAX_PNG_CHUNKS: usize = 1024;
const TALL_ASPECT_RATIO_THRESHOLD: f64 = 2.5;
const TALL_SCREENSHOT_MIN_LONG_EDGE: u32 = 2000;
const OPAQUE_TRANSCODE_MIN_BYTES: u64 = 512 * 1024;

static FFMPEG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
static RESIZE_SEM: Semaphore = Semaphore::const_new(1);
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Dimensions(u32, u32),
    SkipAnimatedWebp,
    UnsupportedOrInvalid,
}

fn cached_ffmpeg_path() -> Option<&'static Path> {
    FFMPEG_PATH
        .get_or_init(|| which::which("ffmpeg").ok())
        .as_deref()
}

/// Probes `(width, height)` directly from PNG, JPEG, WebP (non-animated), or BMP headers without
/// external crates.
#[cfg(test)]
#[must_use]
pub(crate) fn probe_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(dim) = probe_png(bytes) {
        return Some(dim);
    }
    if let Some(dim) = probe_jpeg(bytes) {
        return Some(dim);
    }
    if let ProbeOutcome::Dimensions(w, h) = probe_webp_outcome(bytes) {
        return Some((w, h));
    }
    probe_bmp(bytes)
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

fn probe_bmp(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 22 || &bytes[..2] != b"BM" {
        return None;
    }
    let dib_size = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    if dib_size == 12 {
        // OS/2 BITMAPCOREHEADER: unsigned 16-bit width/height.
        let w = u32::from(u16::from_le_bytes([bytes[18], bytes[19]]));
        let h = u32::from(u16::from_le_bytes([bytes[20], bytes[21]]));
        return (w > 0 && h > 0).then_some((w, h));
    }
    if dib_size >= 40 && bytes.len() >= 26 {
        // BITMAPINFOHEADER (v3/v4/v5): signed 32-bit width/height (negative height = top-down).
        let w_signed = i32::from_le_bytes([bytes[18], bytes[19], bytes[20], bytes[21]]);
        let h_signed = i32::from_le_bytes([bytes[22], bytes[23], bytes[24], bytes[25]]);
        if w_signed <= 0 || h_signed == 0 || h_signed == i32::MIN {
            return None;
        }
        return Some((w_signed.unsigned_abs(), h_signed.unsigned_abs()));
    }
    None
}

#[cfg(test)]
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

fn probe_webp_outcome(bytes: &[u8]) -> ProbeOutcome {
    if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return ProbeOutcome::UnsupportedOrInvalid;
    }
    let chunk = &bytes[12..16];
    if chunk == b"VP8X" {
        // VP8X flags bit 1 (`0x02`) marks animated WebP; skip so we never flatten animations.
        if (bytes[20] & 0x02) != 0 {
            return ProbeOutcome::SkipAnimatedWebp;
        }
        let w =
            1 + (u32::from(bytes[24]) | (u32::from(bytes[25]) << 8) | (u32::from(bytes[26]) << 16));
        let h =
            1 + (u32::from(bytes[27]) | (u32::from(bytes[28]) << 8) | (u32::from(bytes[29]) << 16));
        return if w > 0 && h > 0 {
            ProbeOutcome::Dimensions(w, h)
        } else {
            ProbeOutcome::UnsupportedOrInvalid
        };
    }
    if chunk == b"VP8 " && bytes[23..26] == [0x9D, 0x01, 0x2A] {
        let w = u32::from(u16::from_le_bytes([bytes[26], bytes[27]]) & 0x3FFF);
        let h = u32::from(u16::from_le_bytes([bytes[28], bytes[29]]) & 0x3FFF);
        return if w > 0 && h > 0 {
            ProbeOutcome::Dimensions(w, h)
        } else {
            ProbeOutcome::UnsupportedOrInvalid
        };
    }
    if chunk == b"VP8L" && bytes[20] == 0x2F {
        let bits = u32::from_le_bytes([bytes[21], bytes[22], bytes[23], bytes[24]]);
        let w = (bits & 0x3FFF) + 1;
        let h = ((bits >> 14) & 0x3FFF) + 1;
        return if w > 0 && h > 0 {
            ProbeOutcome::Dimensions(w, h)
        } else {
            ProbeOutcome::UnsupportedOrInvalid
        };
    }
    ProbeOutcome::UnsupportedOrInvalid
}

/// Stream-seeks across JPEG segments past 64 KB EXIF/XMP/ICC/HDR APP headers with bounded RAM.
async fn probe_jpeg_stream(file: &mut tokio::fs::File) -> std::io::Result<Option<(u32, u32)>> {
    file.seek(SeekFrom::Start(2)).await?;
    let mut reader = BufReader::with_capacity(JPEG_STREAM_BUF_BYTES, file);
    let mut byte = [0u8; 1];
    let mut inter_marker_bytes = 0usize;

    for _ in 0..MAX_JPEG_SEGMENTS {
        loop {
            match reader.read_exact(&mut byte).await {
                Ok(_) => {
                    if byte[0] == 0xFF {
                        break;
                    }
                    inter_marker_bytes += 1;
                    if inter_marker_bytes > MAX_JPEG_INTER_MARKER_BYTES {
                        return Ok(None);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(err) => return Err(err),
            }
        }

        let marker = loop {
            match reader.read_exact(&mut byte).await {
                Ok(_) => {
                    if byte[0] != 0xFF {
                        break byte[0];
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(err) => return Err(err),
            }
        };

        // Standalone markers without a length payload.
        if marker == 0x00 || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            if marker == 0xD9 {
                return Ok(None);
            }
            continue;
        }
        // Start of Scan: compressed scan data follows, headers are finished.
        if marker == 0xDA {
            return Ok(None);
        }

        let mut len_buf = [0u8; 2];
        match reader.read_exact(&mut len_buf).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(err) => return Err(err),
        }
        let seg_len = u16::from_be_bytes(len_buf);
        if seg_len < 2 {
            return Ok(None);
        }
        let payload_len = usize::from(seg_len) - 2;

        let is_sof = matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        );
        if is_sof && payload_len >= 5 {
            let mut sof = [0u8; 5];
            match reader.read_exact(&mut sof).await {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(err) => return Err(err),
            }
            let h = u32::from(u16::from_be_bytes([sof[1], sof[2]]));
            let w = u32::from(u16::from_be_bytes([sof[3], sof[4]]));
            return Ok((w > 0 && h > 0).then_some((w, h)));
        }

        if payload_len > 0 {
            reader.seek(SeekFrom::Current(payload_len as i64)).await?;
        }
    }

    Ok(None)
}

/// Returns `true` when `bytes` is a static, opaque PNG (IHDR color type 0=Gray or 2=RGB, with
/// neither a `tRNS` transparency chunk nor an `acTL` APNG animation chunk before `IDAT`).
#[cfg(test)]
fn is_opaque_png_bytes(bytes: &[u8]) -> bool {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 33 || &bytes[..8] != PNG_SIG || &bytes[12..16] != b"IHDR" {
        return false;
    }
    let ihdr_len = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if ihdr_len < 13 {
        return false;
    }
    let color_type = bytes[25];
    if !matches!(color_type, 0 | 2) {
        return false;
    }

    let Some(mut pos) = 16usize.checked_add(ihdr_len).and_then(|v| v.checked_add(4)) else {
        return false;
    };
    for _ in 0..MAX_PNG_CHUNKS {
        if pos + 8 > bytes.len() {
            return false;
        }
        let chunk_len =
            u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
                as usize;
        let chunk_type = &bytes[pos + 4..pos + 8];
        match chunk_type {
            b"tRNS" | b"acTL" | b"IEND" => return false,
            b"IDAT" => return true,
            _ => {
                let Some(next) = pos
                    .checked_add(8)
                    .and_then(|v| v.checked_add(chunk_len))
                    .and_then(|v| v.checked_add(4))
                else {
                    return false;
                };
                if next > bytes.len() {
                    return false;
                }
                pos = next;
            }
        }
    }
    false
}

/// Stream-seeks PNG chunks on disk to detect whether a PNG is opaque (IHDR color_type 0 or 2 and
/// no `tRNS`/`acTL` chunk before `IDAT`).
async fn is_opaque_png_file(path: &Path) -> std::io::Result<bool> {
    const PNG_SIG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

    let mut file = tokio::fs::File::open(path).await?;
    let mut ihdr = [0u8; 33];
    match file.read_exact(&mut ihdr).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(err) => return Err(err),
    }
    if &ihdr[..8] != PNG_SIG || &ihdr[12..16] != b"IHDR" {
        return Ok(false);
    }
    let ihdr_len = u32::from_be_bytes([ihdr[8], ihdr[9], ihdr[10], ihdr[11]]);
    if ihdr_len < 13 || !matches!(ihdr[25], 0 | 2) {
        return Ok(false);
    }
    if ihdr_len > 13 {
        let extra = i64::from(ihdr_len - 13);
        file.seek(SeekFrom::Current(extra)).await?;
    }

    let mut chunk_hdr = [0u8; 8];
    for _ in 0..MAX_PNG_CHUNKS {
        match file.read_exact(&mut chunk_hdr).await {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(err) => return Err(err),
        }
        let chunk_len =
            u32::from_be_bytes([chunk_hdr[0], chunk_hdr[1], chunk_hdr[2], chunk_hdr[3]]);
        match &chunk_hdr[4..8] {
            b"tRNS" | b"acTL" | b"IEND" => return Ok(false),
            b"IDAT" => return Ok(true),
            _ => {
                let skip = i64::from(chunk_len) + 4;
                file.seek(SeekFrom::Current(skip)).await?;
            }
        }
    }
    Ok(false)
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

/// Computes the effective long-edge target, granting tall scrolling screenshots (`aspect > 2.5`)
/// up to `max_edge.max(2000)` pixels to preserve OCR legibility without extra Gemini 4 tokens.
#[must_use]
fn effective_target_max_edge(w: u32, h: u32, max_edge: u32) -> u32 {
    if max_edge == 0 || w == 0 || h == 0 {
        return max_edge;
    }
    let long = f64::from(w.max(h));
    let short = f64::from(w.min(h));
    if long / short > TALL_ASPECT_RATIO_THRESHOLD {
        max_edge.max(TALL_SCREENSHOT_MIN_LONG_EDGE)
    } else {
        max_edge
    }
}

async fn probe_file_dimensions(path: &Path) -> std::io::Result<ProbeOutcome> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut prefix = [0u8; HEADER_PREFIX_BYTES];
    let mut total = 0usize;
    while total < prefix.len() {
        let n = file.read(&mut prefix[total..]).await?;
        if n == 0 {
            break;
        }
        total += n;
    }
    let head = &prefix[..total];

    if let Some((w, h)) = probe_png(head) {
        return Ok(ProbeOutcome::Dimensions(w, h));
    }
    if let Some((w, h)) = probe_bmp(head) {
        return Ok(ProbeOutcome::Dimensions(w, h));
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Ok(probe_webp_outcome(head));
    }
    if head.len() >= 2 && head[0] == 0xFF && head[1] == 0xD8 {
        return Ok(match probe_jpeg_stream(&mut file).await? {
            Some((w, h)) => ProbeOutcome::Dimensions(w, h),
            None => ProbeOutcome::UnsupportedOrInvalid,
        });
    }
    Ok(ProbeOutcome::UnsupportedOrInvalid)
}

async fn process_image(
    path: &Path,
    max_edge: u32,
    allow_transcode: bool,
) -> Option<(PathBuf, u64)> {
    if max_edge == 0 || !is_resizable_raster_ext(path) {
        return None;
    }

    let (w, h) = match probe_file_dimensions(path).await {
        Ok(ProbeOutcome::Dimensions(w, h)) => (w, h),
        Ok(ProbeOutcome::SkipAnimatedWebp) => {
            debug!(path = %path.display(), "skipping animated WebP image");
            return None;
        }
        Ok(ProbeOutcome::UnsupportedOrInvalid) => {
            warn!(
                path = %path.display(),
                "failed to probe raster image dimensions; keeping original"
            );
            return None;
        }
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to read image header for dimension probe; keeping original"
            );
            return None;
        }
    };

    let target_edge = effective_target_max_edge(w, h, max_edge);
    let needs_resize = w.max(h) > target_edge;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();
    let old_bytes = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);

    let should_transcode_to_jpg = if allow_transcode && old_bytes > OPAQUE_TRANSCODE_MIN_BYTES {
        if ext == "bmp" {
            true
        } else if ext == "png" {
            match is_opaque_png_file(path).await {
                Ok(opaque) => opaque,
                Err(err) => {
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "failed to inspect PNG opacity chunks; skipping JPEG transcode"
                    );
                    false
                }
            }
        } else {
            false
        }
    } else {
        false
    };

    if !needs_resize && !should_transcode_to_jpg {
        return None;
    }

    let Some(ffmpeg) = cached_ffmpeg_path() else {
        warn!(
            path = %path.display(),
            "ffmpeg not found in PATH; keeping original image"
        );
        return None;
    };

    let Ok(_permit) = RESIZE_SEM.acquire().await else {
        return None;
    };

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let out_ext = if should_transcode_to_jpg {
        "jpg"
    } else {
        ext.as_str()
    };
    let tmp_path = path.with_file_name(format!(
        ".{stem}.resize-{}-{seq}.{out_ext}",
        std::process::id()
    ));
    let final_path = if should_transcode_to_jpg {
        path.with_extension("jpg")
    } else {
        path.to_path_buf()
    };

    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-nostdin",
        "-y",
        "-v",
        "error",
        "-filter_threads",
        "1",
        "-threads",
        "1",
        "-i",
    ])
    .arg(path);

    if needs_resize {
        let scale_filter = format!(
            "scale='min({target_edge},iw)':'min({target_edge},ih)':force_original_aspect_ratio=decrease"
        );
        cmd.args(["-vf", &scale_filter]);
    }
    cmd.args(["-threads", "1", "-frames:v", "1"]);

    if out_ext == "jpg" || out_ext == "jpeg" {
        cmd.args(["-q:v", "4"]);
    } else if out_ext == "png" {
        cmd.args(["-compression_level", "6"]);
    } else if out_ext == "webp" {
        cmd.args(["-quality", "82"]);
    }

    cmd.arg(&tmp_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    #[cfg(unix)]
    cmd.process_group(0);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "ffmpeg spawn failed during image processing; keeping original"
            );
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return None;
        }
    };
    #[cfg(unix)]
    let child_pid = child.id();

    let output = match tokio::time::timeout(RESIZE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            warn!(
                path = %path.display(),
                error = %err,
                "ffmpeg wait failed during image processing; keeping original"
            );
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return None;
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                crate::executor::signal_process_tree(pid, libc::SIGKILL);
            }
            warn!(
                path = %path.display(),
                w,
                h,
                target_edge,
                "image processing timed out; keeping original"
            );
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!(
            path = %path.display(),
            status = ?output.status.code(),
            stderr = %stderr.trim(),
            "ffmpeg image processing failed; keeping original"
        );
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    let Ok(new_meta) = tokio::fs::metadata(&tmp_path).await else {
        warn!(
            path = %path.display(),
            tmp_path = %tmp_path.display(),
            "ffmpeg produced no output file; keeping original"
        );
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    };
    if !new_meta.is_file() || new_meta.len() == 0 {
        warn!(
            path = %path.display(),
            tmp_path = %tmp_path.display(),
            "ffmpeg produced empty output; keeping original"
        );
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    if let Err(err) = tokio::fs::rename(&tmp_path, &final_path).await {
        warn!(
            path = %path.display(),
            dest = %final_path.display(),
            error = %err,
            "failed to move processed image into place; keeping original"
        );
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return None;
    }

    if should_transcode_to_jpg
        && final_path != path
        && let Err(err) = tokio::fs::remove_file(path).await
    {
        warn!(
            path = %path.display(),
            new_path = %final_path.display(),
            error = %err,
            "failed to remove original image after JPEG transcode"
        );
    }

    let new_bytes = new_meta.len();
    info!(
        path = %final_path.display(),
        orig_w = w,
        orig_h = h,
        target_edge,
        transcoded = should_transcode_to_jpg,
        old_bytes,
        new_bytes,
        "Processed inbound chat image"
    );
    Some((final_path, new_bytes))
}

/// Downscales `path` in-place when its longest edge exceeds `max_edge`, returning the new byte
/// size if resized. Returns `None` when no resize was needed, `max_edge == 0`, `ffmpeg` is
/// unavailable, or resizing fails (preserving the original file).
#[allow(dead_code)]
pub(crate) async fn maybe_downscale_image(path: &Path, max_edge: u32) -> Option<u64> {
    process_image(path, max_edge, false)
        .await
        .map(|(_, bytes)| bytes)
}

/// Downscales `path` when its longest edge exceeds the effective target edge, and transcodes large
/// opaque PNG/BMP files (> 512 KB) to `.jpg` (`-q:v 4`) atomically, removing the original file.
/// Returns the resulting path (the new `.jpg` path if transcoded, or `path.to_path_buf()`).
pub async fn maybe_downscale_image_or_transcode(path: &Path, max_edge: u32) -> PathBuf {
    process_image(path, max_edge, true)
        .await
        .map_or_else(|| path.to_path_buf(), |(out_path, _)| out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png(w: u32, h: u32, color_type: u8, extra_chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut png = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&w.to_be_bytes());
        png.extend_from_slice(&h.to_be_bytes());
        png.push(8); // bit depth
        png.push(color_type);
        png.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
        png.extend_from_slice(&[0u8; 4]); // CRC placeholder
        for &(chunk_type, payload) in extra_chunks {
            png.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            png.extend_from_slice(chunk_type);
            png.extend_from_slice(payload);
            png.extend_from_slice(&[0u8; 4]); // CRC placeholder
        }
        png
    }

    #[test]
    fn probes_png_dimensions() {
        let png = make_png(2560, 1440, 2, &[(b"IDAT", &[0u8; 4])]);
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

    #[tokio::test]
    async fn probes_jpeg_stream_past_large_app1_exif_segments() {
        let mut jpg = vec![0xFF, 0xD8];
        // Two 40,000-byte APP1 segments (~80 KB total > 64 KB) before SOF0.
        for _ in 0..2 {
            let payload_len = 40_000usize;
            let seg_len = (payload_len + 2) as u16;
            jpg.extend_from_slice(&[0xFF, 0xE1]);
            jpg.extend_from_slice(&seg_len.to_be_bytes());
            jpg.resize(jpg.len() + payload_len, 0x42);
        }
        // SOF0: height=4096 (0x1000), width=3072 (0x0C00)
        jpg.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x10, 0x00, 0x0C, 0x00, 0x03, 0x01, 0x11, 0x00,
        ]);
        jpg.extend_from_slice(&[0xFF, 0xD9]);

        let tmp = std::env::temp_dir().join(format!(
            "agygram_large_exif_{}_{}.jpg",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        tokio::fs::write(&tmp, &jpg).await.unwrap();
        let outcome = probe_file_dimensions(&tmp).await.unwrap();
        let _ = tokio::fs::remove_file(&tmp).await;

        assert_eq!(outcome, ProbeOutcome::Dimensions(3072, 4096));
    }

    #[test]
    fn probes_bmp_headers() {
        // BITMAPINFOHEADER (40 bytes), positive height (bottom-up)
        let mut bmp = vec![0u8; 54];
        bmp[0..2].copy_from_slice(b"BM");
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        bmp[18..22].copy_from_slice(&1920i32.to_le_bytes());
        bmp[22..26].copy_from_slice(&1080i32.to_le_bytes());
        assert_eq!(probe_image_dimensions(&bmp), Some((1920, 1080)));

        // Top-down DIB with negative height
        bmp[22..26].copy_from_slice(&(-1080i32).to_le_bytes());
        assert_eq!(probe_image_dimensions(&bmp), Some((1920, 1080)));

        // OS/2 BITMAPCOREHEADER (12 bytes)
        let mut bmp_core = vec![0u8; 26];
        bmp_core[0..2].copy_from_slice(b"BM");
        bmp_core[14..18].copy_from_slice(&12u32.to_le_bytes());
        bmp_core[18..20].copy_from_slice(&800u16.to_le_bytes());
        bmp_core[20..22].copy_from_slice(&600u16.to_le_bytes());
        assert_eq!(probe_image_dimensions(&bmp_core), Some((800, 600)));

        // Zero/negative width rejected
        bmp[18..22].copy_from_slice(&0i32.to_le_bytes());
        assert_eq!(probe_image_dimensions(&bmp), None);
    }

    #[test]
    fn probes_webp_vp8x_and_vp8l_and_skips_animated_vp8x() {
        // Static VP8X: width=3000 (2999 = 0x000BB7), height=2000 (1999 = 0x0007CF)
        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF\x20\x00\x00\x00WEBPVP8X");
        webp.extend_from_slice(&[0x0A, 0x00, 0x00, 0x00]); // chunk size
        webp.extend_from_slice(&[0x10, 0x00, 0x00, 0x00]); // alpha flag only (static)
        webp.extend_from_slice(&[0xB7, 0x0B, 0x00]); // width - 1
        webp.extend_from_slice(&[0xCF, 0x07, 0x00]); // height - 1
        assert_eq!(probe_image_dimensions(&webp), Some((3000, 2000)));
        assert_eq!(
            probe_webp_outcome(&webp),
            ProbeOutcome::Dimensions(3000, 2000)
        );

        // Animated VP8X: flags bit 1 (`0x02`) set -> must be skipped
        webp[20] = 0x02;
        assert_eq!(probe_image_dimensions(&webp), None);
        assert_eq!(probe_webp_outcome(&webp), ProbeOutcome::SkipAnimatedWebp);

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
    fn tall_screenshot_target_sizing_preserves_ocr_legibility() {
        // Tall scrolling screenshot (1080x6000, ratio ~5.55 > 2.5) gets raised to 2000px
        assert_eq!(effective_target_max_edge(1080, 6000, 1280), 2000);
        assert_eq!(effective_target_max_edge(6000, 1080, 1280), 2000);
        // Respects already higher configured max_edge
        assert_eq!(effective_target_max_edge(1080, 6000, 2560), 2560);
        // Normal portrait/landscape aspect ratios stay at configured max_edge
        assert_eq!(effective_target_max_edge(1080, 1920, 1280), 1280);
        assert_eq!(effective_target_max_edge(1080, 2700, 1280), 1280); // exact 2.5
        assert_eq!(effective_target_max_edge(1080, 2701, 1280), 2000); // > 2.5
        // Disabled when max_edge == 0
        assert_eq!(effective_target_max_edge(1080, 6000, 0), 0);
    }

    #[tokio::test]
    async fn detects_opaque_vs_alpha_and_animated_pngs() {
        let rgb_opaque = make_png(100, 100, 2, &[(b"iCCP", &[1u8; 64]), (b"IDAT", &[0u8; 8])]);
        let gray_opaque = make_png(100, 100, 0, &[(b"IDAT", &[0u8; 8])]);
        let rgb_trns = make_png(100, 100, 2, &[(b"tRNS", &[0u8; 6]), (b"IDAT", &[0u8; 8])]);
        let rgba = make_png(100, 100, 6, &[(b"IDAT", &[0u8; 8])]);
        let gray_alpha = make_png(100, 100, 4, &[(b"IDAT", &[0u8; 8])]);
        let indexed = make_png(100, 100, 3, &[(b"PLTE", &[0u8; 3]), (b"IDAT", &[0u8; 8])]);
        let apng = make_png(100, 100, 2, &[(b"acTL", &[0u8; 8]), (b"IDAT", &[0u8; 8])]);

        assert!(is_opaque_png_bytes(&rgb_opaque));
        assert!(is_opaque_png_bytes(&gray_opaque));
        assert!(!is_opaque_png_bytes(&rgb_trns));
        assert!(!is_opaque_png_bytes(&rgba));
        assert!(!is_opaque_png_bytes(&gray_alpha));
        assert!(!is_opaque_png_bytes(&indexed));
        assert!(!is_opaque_png_bytes(&apng));

        let dir = std::env::temp_dir();
        let pid = std::process::id();
        let opaque_path = dir.join(format!("agygram_png_opaque_{pid}.png"));
        let trns_path = dir.join(format!("agygram_png_trns_{pid}.png"));
        tokio::fs::write(&opaque_path, &rgb_opaque).await.unwrap();
        tokio::fs::write(&trns_path, &rgb_trns).await.unwrap();

        assert!(is_opaque_png_file(&opaque_path).await.unwrap());
        assert!(!is_opaque_png_file(&trns_path).await.unwrap());

        let _ = tokio::fs::remove_file(&opaque_path).await;
        let _ = tokio::fs::remove_file(&trns_path).await;
    }

    #[test]
    fn ignores_gif_and_invalid_headers() {
        assert_eq!(probe_image_dimensions(b"GIF89a\x00\x05\x00\x05"), None);
        assert_eq!(probe_image_dimensions(b"not an image"), None);
        assert!(!is_resizable_raster_ext(Path::new("sticker.gif")));
        assert!(is_resizable_raster_ext(Path::new("photo.JPG")));
        assert!(is_resizable_raster_ext(Path::new("shot.webp")));
        assert!(is_resizable_raster_ext(Path::new("capture.bmp")));
    }
}
