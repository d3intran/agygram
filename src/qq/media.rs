//! Inbound QQ attachment downloading, deduplication, scratch GC, outbound file/artifact routing,
//! inline UTF-8 previews, and Fish Audio voice synthesis delivery.

use super::client::{MAX_DIRECT_MEDIA_BYTES, QqClient, QqTarget};
use super::context;
use super::delivery::reply;
use super::types::{MessageAttachment, QqMediaFileType};
use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use crate::text::{looks_sensitive, safe_attachment_name, truncate_chars};

pub(crate) const MAX_QQ_ATTACHMENTS: usize = 8;
pub(crate) const MAX_QQ_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
const ATTACHMENT_DOWNLOAD_CONCURRENCY: usize = 4;
/// Bounded to 2 concurrent base64 uploads to protect peak RAM on 892 MB VPS deployments.
pub(crate) const ARTIFACT_UPLOAD_CONCURRENCY: usize = 2;
pub(crate) const MAX_AUTO_ARTIFACTS: usize = 6;
const QQ_TEXT_PREVIEW_READ_BYTES: u64 = 24 * 1024;
const QQ_TEXT_PREVIEW_MAX_CHARS: usize = 1600;

const SCRATCH_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
const SCRATCH_ATTACHMENT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const SCRATCH_ATTACHMENT_MAX_BYTES: u64 = 100 * 1024 * 1024;

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

static LAST_SCRATCH_GC: OnceLock<StdMutex<FxHashMap<PathBuf, Instant>>> = OnceLock::new();

/// Drops Fish Audio emotion cues (`[calm]`, `[happy]`, etc.) using the shared
/// [`crate::tts::is_emotion_tag`] predicate so voice fallback renders clean text without
/// allocating when no emotion cues are present.
#[must_use]
pub(crate) fn strip_emotion_tags(text: &str) -> Cow<'_, str> {
    let trimmed = text.trim();
    if !trimmed.contains('[') {
        return Cow::Borrowed(trimmed);
    }

    let mut scan = trimmed;
    let mut offset = 0usize;
    let mut first_tag_offset = None;
    while let Some(open) = scan.find('[') {
        let after = &scan[open + 1..];
        let Some(close) = after.find(']') else {
            break;
        };
        if crate::tts::is_emotion_tag(&after[..close]) {
            first_tag_offset = Some(offset + open);
            break;
        }
        let step = open + 1 + close + 1;
        offset += step;
        scan = &scan[step..];
    }

    let Some(first_open) = first_tag_offset else {
        return Cow::Borrowed(trimmed);
    };

    let mut out = String::with_capacity(trimmed.len());
    out.push_str(&trimmed[..first_open]);
    let mut rest = &trimmed[first_open..];
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push('[');
            out.push_str(after);
            return Cow::Owned(out.trim().to_string());
        };
        let tag = &after[..close];
        if !crate::tts::is_emotion_tag(tag) {
            out.push('[');
            out.push_str(&after[..close + 1]);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out.trim().to_string())
}

/// QQ's message id is remote-controlled and may carry separators; keep a short alnum token
/// before it becomes part of a filename.
#[must_use]
pub(crate) fn sanitize_message_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(24)
        .collect();
    if cleaned.is_empty() {
        "x".to_string()
    } else {
        cleaned
    }
}

#[inline]
fn fnv1a_64_str(s: &str) -> u64 {
    let mut h = FNV1A_OFFSET_BASIS;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV1A_PRIME);
    }
    h
}

/// Builds a deterministic 12-hex-char dedup prefix from `ref_idx`/`msg_idx` (when available)
/// or the normalized attachment URL so repeat quotes reuse existing files on disk.
fn attachment_dedup_token(dedup_id: Option<&str>, url: &str, idx: usize, ext: &str) -> String {
    let key = match dedup_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(rid) => {
            // Strip rotating query string if a stable ref_idx/msg_idx already scopes this attachment.
            let url_base = url.split('?').next().unwrap_or(url);
            format!("ref:{rid}:{idx}:{ext}:{url_base}")
        }
        None => format!("url:{ext}:{url}"),
    };
    let h = fnv1a_64_str(&key);
    format!("qq-{:012x}", h & 0x0000_FFFF_FFFF_FFFF)
}

fn normalize_attachment_url(raw: &str) -> Cow<'_, str> {
    let trimmed = raw.trim();
    if trimmed.starts_with("//") {
        Cow::Owned(format!("https:{trimmed}"))
    } else {
        Cow::Borrowed(trimmed)
    }
}

fn attachment_extension(att: &MessageAttachment) -> &'static str {
    let raw_ct = att.content_type.as_deref().unwrap_or("");
    let mime = raw_ct.split(';').next().unwrap_or("").trim();
    if mime.eq_ignore_ascii_case("image/png") {
        "png"
    } else if mime.eq_ignore_ascii_case("image/gif") {
        "gif"
    } else if mime.eq_ignore_ascii_case("image/webp") {
        "webp"
    } else if mime.eq_ignore_ascii_case("image/bmp") {
        "bmp"
    } else if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
        "jpg"
    } else if mime.eq_ignore_ascii_case("video/mp4") {
        "mp4"
    } else if mime.eq_ignore_ascii_case("video/webm") {
        "webm"
    } else if mime.eq_ignore_ascii_case("video/quicktime") {
        "mov"
    } else if mime.eq_ignore_ascii_case("voice") || mime.eq_ignore_ascii_case("audio/wav") {
        "wav"
    } else if mime.eq_ignore_ascii_case("audio/mpeg") || mime.eq_ignore_ascii_case("audio/mp3") {
        "mp3"
    } else if mime.eq_ignore_ascii_case("audio/ogg") {
        "ogg"
    } else if mime.eq_ignore_ascii_case("application/pdf") {
        "pdf"
    } else if mime.eq_ignore_ascii_case("application/zip") {
        "zip"
    } else if mime.eq_ignore_ascii_case("application/json") {
        "json"
    } else if mime.eq_ignore_ascii_case("text/plain") {
        "txt"
    } else if mime.eq_ignore_ascii_case("text/markdown") {
        "md"
    } else if mime.eq_ignore_ascii_case("text/csv") {
        "csv"
    } else {
        "bin"
    }
}

/// Voice notes are recorded as SILK/AMR, which agy cannot read; QQ also serves a WAV transcode,
/// so use that when present and keep SILK only as the last resort.
fn attachment_source(att: &MessageAttachment) -> (Cow<'_, str>, &'static str) {
    if att
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("voice"))
    {
        if let Some(wav) = att
            .voice_wav_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        {
            return (normalize_attachment_url(wav), "wav");
        }
        return (normalize_attachment_url(&att.url), "silk");
    }
    (
        normalize_attachment_url(&att.url),
        attachment_extension(att),
    )
}

fn attachment_is_image(att: &MessageAttachment) -> bool {
    if att
        .content_type
        .as_deref()
        .is_some_and(|kind| kind.trim().to_ascii_lowercase().starts_with("image/"))
    {
        return true;
    }
    att.filename
        .as_deref()
        .and_then(|name| Path::new(name).extension().and_then(|e| e.to_str()))
        .is_some_and(|ext| QqArtifactRoute::from_extension(ext) == QqArtifactRoute::Image)
}

/// Computes the target scratch filename for an inbound attachment, ensuring WAV voice transcodes
/// never keep misleading `.amr`/`.silk` extensions from `att.filename`.
fn build_attachment_filename(
    dedup_token: &str,
    att: &MessageAttachment,
    ext: &'static str,
) -> String {
    let fallback = format!("attachment.{ext}");
    let mut name = safe_attachment_name(dedup_token, att.filename.as_deref(), &fallback);
    if ext == "wav" {
        let current_ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !current_ext.eq_ignore_ascii_case("wav") {
            name = Path::new(&name)
                .with_extension("wav")
                .to_string_lossy()
                .into_owned();
        }
    } else if Path::new(&name).extension().is_none() {
        name.push('.');
        name.push_str(ext);
    }
    name
}

enum AttachmentFetchOutcome {
    Saved {
        dest: PathBuf,
        label: &'static str,
        bytes: u64,
        reused: bool,
    },
    Failed(String),
}

struct IndexedAttachmentFetch {
    idx: usize,
    outcome: AttachmentFetchOutcome,
}

async fn download_one_attachment(
    client: &QqClient,
    idx: usize,
    att: &MessageAttachment,
    scratch_dir: &Path,
    dedup_id: Option<&str>,
) -> IndexedAttachmentFetch {
    let (url, ext) = attachment_source(att);
    let display_name = att.filename.as_deref().unwrap_or("未命名附件");
    if url.is_empty() {
        return IndexedAttachmentFetch {
            idx,
            outcome: AttachmentFetchOutcome::Failed(format!("{}. {display_name}", idx + 1)),
        };
    }
    if att.size.is_some_and(|s| s > MAX_QQ_ATTACHMENT_BYTES) {
        warn!(
            index = idx + 1,
            reported_size = ?att.size,
            limit = MAX_QQ_ATTACHMENT_BYTES,
            "QQ attachment exceeds size limit; skipping download"
        );
        return IndexedAttachmentFetch {
            idx,
            outcome: AttachmentFetchOutcome::Failed(format!("{}. {display_name}", idx + 1)),
        };
    }

    let dedup_token = attachment_dedup_token(dedup_id, &url, idx, ext);
    let name = build_attachment_filename(&dedup_token, att, ext);
    let dest = scratch_dir.join(&name);
    let label = if attachment_is_image(att) {
        "Image"
    } else {
        "File"
    };

    // Deduplicate against an existing cached file with the same URL/ref_idx hash.
    if let Ok(meta) = tokio::fs::metadata(&dest).await
        && meta.is_file()
        && meta.len() > 0
        && meta.len() <= MAX_QQ_ATTACHMENT_BYTES
    {
        return IndexedAttachmentFetch {
            idx,
            outcome: AttachmentFetchOutcome::Saved {
                dest,
                label,
                bytes: meta.len(),
                reused: true,
            },
        };
    }

    let outcome = match client
        .download_attachment(&url, &dest, MAX_QQ_ATTACHMENT_BYTES)
        .await
    {
        Ok(bytes) => AttachmentFetchOutcome::Saved {
            dest,
            label,
            bytes,
            reused: false,
        },
        Err(err) => {
            warn!(
                dest = %dest.display(),
                error = %format!("{err:#}"),
                "QQ attachment download failed"
            );
            AttachmentFetchOutcome::Failed(format!("{}. {display_name}", idx + 1))
        }
    };
    IndexedAttachmentFetch { idx, outcome }
}

/// Formats a single prompt attachment line without duplicating `File` (e.g. `[Quoted File: ...]`
/// instead of `[Quoted File File: ...]`, while images stay `[Quoted Image File: ...]`).
fn write_attachment_prompt_line(out: &mut String, tag_prefix: &str, label: &str, dest: &Path) {
    if label == "File" {
        let _ = writeln!(out, "[{tag_prefix} File: {}]", dest.display());
    } else {
        let _ = writeln!(out, "[{tag_prefix} {label} File: {}]", dest.display());
    }
}

/// Downloads up to `MAX_QQ_ATTACHMENTS` concurrently into `scratch_dir`, reusing previously
/// downloaded attachments matching the URL/`ref_idx` hash.
/// Returns `(prompt_note, saved_paths, failed_labels)`.
pub(crate) async fn download_attachments_with_paths(
    client: &QqClient,
    attachments: &[MessageAttachment],
    scratch_dir: &Path,
    dedup_id: Option<&str>,
    tag_prefix: &str,
) -> (String, Vec<PathBuf>, Vec<String>) {
    if attachments.is_empty() {
        return (String::new(), Vec::new(), Vec::new());
    }

    let futs: Vec<_> = attachments
        .iter()
        .take(MAX_QQ_ATTACHMENTS)
        .enumerate()
        .map(|(idx, att)| download_one_attachment(client, idx, att, scratch_dir, dedup_id))
        .collect();
    let mut results: Vec<IndexedAttachmentFetch> = stream::iter(futs)
        .buffer_unordered(ATTACHMENT_DOWNLOAD_CONCURRENCY)
        .collect()
        .await;

    results.sort_unstable_by_key(|item| item.idx);

    let mut note = String::with_capacity(results.len() * 64);
    let mut saved_paths = Vec::with_capacity(results.len());
    let mut failures = Vec::new();

    for item in results {
        match item.outcome {
            AttachmentFetchOutcome::Saved {
                dest,
                label,
                bytes,
                reused,
            } => {
                write_attachment_prompt_line(&mut note, tag_prefix, label, &dest);
                if reused {
                    debug!(dest = %dest.display(), bytes, "Reused cached QQ attachment");
                } else {
                    info!(dest = %dest.display(), bytes, "QQ attachment saved");
                }
                saved_paths.push(dest);
            }
            AttachmentFetchOutcome::Failed(failure_label) => failures.push(failure_label),
        }
    }

    if attachments.len() > MAX_QQ_ATTACHMENTS {
        failures.push(format!(
            "另有 {} 个附件超出单条上限 ({MAX_QQ_ATTACHMENTS})",
            attachments.len() - MAX_QQ_ATTACHMENTS
        ));
    }

    (note, saved_paths, failures)
}

/// Identifies auto-downloaded temporary chat attachments (`scratch/m...`) while strictly
/// excluding user-created files such as `metrics.csv`, `my-report.md`, or `reply-*.md`.
#[must_use]
fn is_auto_downloaded_scratch_attachment(file_name: &str) -> bool {
    let Some(after_m) = file_name.strip_prefix('m') else {
        return false;
    };
    let Some((prefix, _rest)) = after_m.split_once('-') else {
        return false;
    };
    if prefix.is_empty() {
        return false;
    }
    prefix == "qq"
        || prefix == "att"
        || prefix.bytes().all(|b| b.is_ascii_digit())
        || prefix.starts_with("ROBOT")
        || prefix.starts_with("quote")
}

/// Spawns a throttled (at most once per hour per `scratch_dir`) background GC pass to keep
/// auto-downloaded attachments bounded on disk.
pub(crate) fn maybe_spawn_scratch_cleanup(scratch_dir: PathBuf) {
    let map_lock = LAST_SCRATCH_GC.get_or_init(|| StdMutex::new(FxHashMap::default()));
    let should_run = {
        let mut map = match map_lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        let due = map
            .get(&scratch_dir)
            .is_none_or(|last| now.duration_since(*last) >= SCRATCH_CLEANUP_INTERVAL);
        if due {
            if map.len() > 64 {
                map.retain(|_, last| now.duration_since(*last) < SCRATCH_CLEANUP_INTERVAL * 2);
            }
            map.insert(scratch_dir.clone(), now);
        }
        due
    };

    if should_run && let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            cleanup_old_scratch_attachments(&scratch_dir).await;
        });
    }
}

/// Cleans auto-downloaded temporary attachments (`scratch/m*`) older than 7 days or exceeding
/// a 100 MB total budget (LRU by `mtime`), leaving user-created artifacts untouched.
pub(crate) async fn cleanup_old_scratch_attachments(scratch_dir: &Path) {
    let _ = cleanup_old_scratch_attachments_with_limits(
        scratch_dir,
        SCRATCH_ATTACHMENT_TTL,
        SCRATCH_ATTACHMENT_MAX_BYTES,
    )
    .await;
}

async fn cleanup_old_scratch_attachments_with_limits(
    scratch_dir: &Path,
    ttl: Duration,
    max_bytes: u64,
) -> (usize, u64) {
    let Ok(mut entries) = tokio::fs::read_dir(scratch_dir).await else {
        return (0, 0);
    };

    let now = SystemTime::now();
    let mut survivors: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut removed_count = 0usize;
    let mut removed_bytes = 0u64;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if !is_auto_downloaded_scratch_attachment(name_str) {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
        let len = meta.len();
        if age > ttl {
            if tokio::fs::remove_file(entry.path()).await.is_ok() {
                removed_count += 1;
                removed_bytes = removed_bytes.saturating_add(len);
            }
        } else {
            survivors.push((entry.path(), len, modified));
        }
    }

    let mut total_bytes: u64 = survivors
        .iter()
        .fold(0u64, |acc, (_, len, _)| acc.saturating_add(*len));
    if total_bytes > max_bytes {
        survivors.sort_unstable_by_key(|(_, _, mtime)| *mtime);
        for (path, len, _) in survivors {
            if total_bytes <= max_bytes {
                break;
            }
            if tokio::fs::remove_file(&path).await.is_ok() {
                total_bytes = total_bytes.saturating_sub(len);
                removed_count += 1;
                removed_bytes = removed_bytes.saturating_add(len);
            }
        }
    }

    if removed_count > 0 {
        info!(
            dir = %scratch_dir.display(),
            removed_count,
            removed_bytes,
            "Cleaned up old QQ scratch attachments"
        );
    }

    (removed_count, removed_bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QqArtifactRoute {
    Image,
    VideoMp4,
    Voice,
    GenericFile,
}

impl QqArtifactRoute {
    #[must_use]
    pub(crate) fn from_extension(ext: &str) -> Self {
        let lower = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        match lower.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => Self::Image,
            "mp4" => Self::VideoMp4,
            "silk" | "mp3" | "wav" | "ogg" => Self::Voice,
            _ => Self::GenericFile,
        }
    }

    #[inline]
    #[must_use]
    pub(crate) const fn primary_media_type(self) -> QqMediaFileType {
        match self {
            Self::Image => QqMediaFileType::Image,
            Self::VideoMp4 => QqMediaFileType::Video,
            Self::Voice => QqMediaFileType::Voice,
            Self::GenericFile => QqMediaFileType::File,
        }
    }
}

#[inline]
fn is_text_previewable_extension(ext: &str) -> bool {
    let lower = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "md" | "markdown"
            | "txt"
            | "log"
            | "csv"
            | "tsv"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "ini"
            | "cfg"
            | "conf"
            | "xml"
            | "html"
            | "htm"
            | "svg"
            | "css"
            | "rs"
            | "py"
            | "js"
            | "ts"
            | "tsx"
            | "jsx"
            | "go"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "sh"
            | "bash"
            | "sql"
    )
}

fn code_fence_lang_for_ext(ext: &str) -> &str {
    let lower = ext.trim().trim_start_matches('.');
    if lower.eq_ignore_ascii_case("rs") {
        "rust"
    } else if lower.eq_ignore_ascii_case("py") {
        "python"
    } else if lower.eq_ignore_ascii_case("md") || lower.eq_ignore_ascii_case("markdown") {
        "markdown"
    } else if lower.eq_ignore_ascii_case("sh") || lower.eq_ignore_ascii_case("bash") {
        "bash"
    } else if lower.is_ascii()
        && !lower.is_empty()
        && lower.len() <= 12
        && lower.chars().all(|c| c.is_ascii_alphanumeric())
    {
        lower
    } else {
        ""
    }
}

/// Decodes a UTF-8 prefix slice without rejecting a multi-byte character split at a read-limit
/// boundary, while rejecting NUL bytes or malformed binary sequences.
fn utf8_prefix_str(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s),
        Err(err) if err.error_len().is_none() => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

async fn read_utf8_preview(path: &Path, total_len: u64) -> Option<(String, bool)> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut buf = Vec::with_capacity(
        usize::try_from(total_len.min(QQ_TEXT_PREVIEW_READ_BYTES)).unwrap_or(4096),
    );
    file.take(QQ_TEXT_PREVIEW_READ_BYTES)
        .read_to_end(&mut buf)
        .await
        .ok()?;
    let text = utf8_prefix_str(&buf)?;
    let clipped = truncate_chars(text, QQ_TEXT_PREVIEW_MAX_CHARS);
    let was_truncated = total_len > buf.len() as u64 || matches!(clipped, Cow::Owned(_));
    Some((clipped.into_owned(), was_truncated))
}

fn format_file_size_label(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn build_inline_preview_message(
    file_name: &str,
    path: &Path,
    ext: &str,
    reason_line: &str,
    preview: &str,
    truncated: bool,
) -> String {
    let trunc_note = if truncated { "（节选前段）" } else { "" };
    if ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown") {
        return format!(
            "📄 **{file_name}** {trunc_note}\n{reason_line}\n📁 路径: `{}`\n\n{preview}",
            path.display()
        );
    }

    let lang = code_fence_lang_for_ext(ext);
    let mut max_run = 2usize;
    let mut cur = 0usize;
    for b in preview.bytes() {
        if b == b'`' {
            cur += 1;
            max_run = max_run.max(cur);
        } else {
            cur = 0;
        }
    }
    let fence: String = std::iter::repeat_n('`', max_run + 1).collect();
    let newline = if preview.ends_with('\n') { "" } else { "\n" };
    format!(
        "📄 **{file_name}** {trunc_note}\n{reason_line}\n📁 路径: `{}`\n\n{fence}{lang}\n{preview}{newline}{fence}",
        path.display()
    )
}

async fn send_file_fallback_notice(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    path: &Path,
    file_len: u64,
    reason_line: &str,
) {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact.bin");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let should_try_preview = is_text_previewable_extension(ext) || file_len <= 64 * 1024;
    if should_try_preview
        && let Some((preview, truncated)) = read_utf8_preview(path, file_len).await
        && !preview.trim().is_empty()
    {
        let msg =
            build_inline_preview_message(file_name, path, ext, reason_line, &preview, truncated);
        if let Err(err) = reply(client, target, msg_id, &msg).await {
            warn!(
                path = %path.display(),
                error = %format!("{err:#}"),
                "QQ inline text preview fallback failed"
            );
        }
        return;
    }

    let size_label = format_file_size_label(file_len);
    let notice = format!(
        "⚠️ **文件未直接发送**: `{file_name}` ({size_label})\n{reason_line}\n📁 服务器路径: `{}`",
        path.display()
    );
    if let Err(err) = reply(client, target, msg_id, &notice).await {
        warn!(
            path = %path.display(),
            error = %format!("{err:#}"),
            "QQ file fallback notice failed"
        );
    }
}

/// Uploads a local media/document file (`file_data` base64 <= 5 MB) or gracefully falls back
/// to an inline text preview / clear path notice so delivery never fails silently.
pub(crate) async fn deliver_single_file(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    path: &Path,
    file_len: u64,
) {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact.bin");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let route = QqArtifactRoute::from_extension(ext);

    if file_len == 0 {
        let _ = reply(
            client,
            target,
            msg_id,
            &format!(
                "⚠️ 文件为空，无法发送: `{file_name}`\n📁 路径: `{}`",
                path.display()
            ),
        )
        .await;
        return;
    }

    if file_len > MAX_DIRECT_MEDIA_BYTES {
        let size_label = format_file_size_label(file_len);
        let reason = format!("超过 QQ 机器人 `file_data` 直传上限 (5 MB，当前 {size_label})");
        send_file_fallback_notice(client, target, msg_id, path, file_len, &reason).await;
        return;
    }

    let primary = route.primary_media_type();
    let scope = target.scope_key();
    let media_summary = format!("[AI 发送的 {}: {file_name}]", primary.label());
    match client
        .send_media_file_with_meta(target, path, msg_id, primary)
        .await
    {
        Ok(sent_meta) => {
            context::record_message(
                &scope,
                sent_meta.id.as_deref(),
                sent_meta.ref_idx(),
                "AI 助手",
                &media_summary,
                vec![path.to_path_buf()],
                Vec::new(),
            );
            info!(
                path = %path.display(),
                file_type = primary.as_u8(),
                label = primary.label(),
                "QQ chat: delivered rich media / file"
            );
        }
        Err(primary_err) => {
            warn!(
                path = %path.display(),
                file_type = primary.as_u8(),
                error = %format!("{primary_err:#}"),
                "QQ primary media upload failed"
            );

            if allows_generic_file_retry(route, primary, ext) {
                match client
                    .send_media_file_with_meta(target, path, msg_id, QqMediaFileType::File)
                    .await
                {
                    Ok(sent_meta) => {
                        context::record_message(
                            &scope,
                            sent_meta.id.as_deref(),
                            sent_meta.ref_idx(),
                            "AI 助手",
                            &media_summary,
                            vec![path.to_path_buf()],
                            Vec::new(),
                        );
                        info!(path = %path.display(), "QQ chat: delivered via file_type=4 fallback");
                        return;
                    }
                    Err(fallback_err) => {
                        warn!(
                            path = %path.display(),
                            error = %format!("{fallback_err:#}"),
                            "QQ file_type=4 fallback also rejected"
                        );
                    }
                }
            }

            let primary_err_text = primary_err.to_string();
            let short_reason = truncate_chars(&primary_err_text, 160);
            let reason = format!("QQ 官方接口未接收直传 ({short_reason})");
            send_file_fallback_notice(client, target, msg_id, path, file_len, &reason).await;
        }
    }
}

pub(crate) enum PreparedArtifactItem {
    Sensitive {
        path: PathBuf,
        file_name: String,
    },
    Empty {
        path: PathBuf,
        file_name: String,
    },
    Oversize {
        path: PathBuf,
        file_len: u64,
    },
    Uploaded {
        path: PathBuf,
        file_len: u64,
        file_type: QqMediaFileType,
        used_generic_fallback: bool,
        file_info: String,
    },
    UploadFailed {
        path: PathBuf,
        file_len: u64,
        reason: String,
    },
}

pub(crate) struct PreparedQqArtifacts {
    pub items: Vec<PreparedArtifactItem>,
    pub skipped_overflow: usize,
}

#[must_use]
pub(crate) fn count_planned_artifact_slots(paths: &[PathBuf]) -> usize {
    if paths.is_empty() {
        return 0;
    }
    let capped = paths.len().min(MAX_AUTO_ARTIFACTS);
    let overflow_slot = usize::from(paths.len() > MAX_AUTO_ARTIFACTS);
    capped + overflow_slot
}

fn allows_generic_file_retry(route: QqArtifactRoute, primary: QqMediaFileType, ext: &str) -> bool {
    primary != QqMediaFileType::File
        && !(route == QqArtifactRoute::Image
            && matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif"
            ))
}

async fn upload_one_artifact_for_preupload(
    client: &QqClient,
    target: QqTarget<'_>,
    idx: usize,
    path: PathBuf,
    file_len: u64,
) -> (usize, PreparedArtifactItem) {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact.bin")
        .to_string();
    if file_len == 0 {
        return (idx, PreparedArtifactItem::Empty { path, file_name });
    }
    if file_len > MAX_DIRECT_MEDIA_BYTES {
        return (idx, PreparedArtifactItem::Oversize { path, file_len });
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let route = QqArtifactRoute::from_extension(ext);
    let primary = route.primary_media_type();

    match client.upload_media_file(target, &path, primary).await {
        Ok(file_info) => (
            idx,
            PreparedArtifactItem::Uploaded {
                path,
                file_len,
                file_type: primary,
                used_generic_fallback: false,
                file_info,
            },
        ),
        Err(primary_err) => {
            warn!(
                path = %path.display(),
                file_type = primary.as_u8(),
                error = %format!("{primary_err:#}"),
                "QQ primary media upload failed"
            );
            if allows_generic_file_retry(route, primary, ext) {
                match client
                    .upload_media_file(target, &path, QqMediaFileType::File)
                    .await
                {
                    Ok(file_info) => {
                        return (
                            idx,
                            PreparedArtifactItem::Uploaded {
                                path,
                                file_len,
                                file_type: QqMediaFileType::File,
                                used_generic_fallback: true,
                                file_info,
                            },
                        );
                    }
                    Err(fallback_err) => {
                        warn!(
                            path = %path.display(),
                            error = %format!("{fallback_err:#}"),
                            "QQ file_type=4 fallback also rejected"
                        );
                    }
                }
            }

            let primary_err_text = primary_err.to_string();
            let short_reason = truncate_chars(&primary_err_text, 160);
            let reason = format!("QQ 官方接口未接收直传 ({short_reason})");
            (
                idx,
                PreparedArtifactItem::UploadFailed {
                    path,
                    file_len,
                    reason,
                },
            )
        }
    }
}

/// Validates and concurrently pre-uploads up to `MAX_AUTO_ARTIFACTS` files (`srv_send_msg: false`)
/// so upload latency overlaps with text reply delivery while preserving deterministic send order.
pub(crate) async fn prepare_qq_artifact_uploads(
    client: &QqClient,
    target: QqTarget<'_>,
    paths: &[PathBuf],
) -> PreparedQqArtifacts {
    if paths.is_empty() {
        return PreparedQqArtifacts {
            items: Vec::new(),
            skipped_overflow: 0,
        };
    }

    let mut ready_items: Vec<(usize, PreparedArtifactItem)> = Vec::new();
    let mut upload_jobs: Vec<(usize, PathBuf, u64)> = Vec::new();
    let mut delivered = 0usize;
    let mut skipped_overflow = 0usize;
    let mut seq = 0usize;

    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.starts_with('.') || looks_sensitive(&path.to_string_lossy()) {
            ready_items.push((
                seq,
                PreparedArtifactItem::Sensitive {
                    path: path.clone(),
                    file_name: file_name.to_string(),
                },
            ));
            seq += 1;
            continue;
        }

        let Ok(meta) = tokio::fs::metadata(path).await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }

        if delivered >= MAX_AUTO_ARTIFACTS {
            skipped_overflow += 1;
            continue;
        }
        delivered += 1;
        upload_jobs.push((seq, path.clone(), meta.len()));
        seq += 1;
    }

    if !upload_jobs.is_empty() {
        let futs: Vec<_> = upload_jobs
            .into_iter()
            .map(|(idx, path, file_len)| {
                upload_one_artifact_for_preupload(client, target, idx, path, file_len)
            })
            .collect();
        let uploaded: Vec<(usize, PreparedArtifactItem)> = stream::iter(futs)
            .buffer_unordered(ARTIFACT_UPLOAD_CONCURRENCY)
            .collect()
            .await;
        ready_items.extend(uploaded);
    }

    ready_items.sort_unstable_by_key(|(idx, _)| *idx);
    PreparedQqArtifacts {
        items: ready_items.into_iter().map(|(_, item)| item).collect(),
        skipped_overflow,
    }
}

/// Sends prepared artifacts and returns how many artifact/notice items were dispatched.
pub(crate) async fn deliver_prepared_qq_artifacts(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    prepared: PreparedQqArtifacts,
) -> usize {
    let scope = target.scope_key();
    let mut sent_count = 0usize;
    for item in prepared.items {
        sent_count += 1;
        match item {
            PreparedArtifactItem::Sensitive { path, file_name } => {
                warn!(path = %path.display(), "QQ chat: refusing to auto-send sensitive-looking artifact");
                let _ = reply(
                    client,
                    target,
                    msg_id,
                    &format!(
                        "🔒 已阻止自动发送疑似敏感文件: `{file_name}`（如需读取请显式使用 `/file`）"
                    ),
                )
                .await;
            }
            PreparedArtifactItem::Empty { path, file_name } => {
                let _ = reply(
                    client,
                    target,
                    msg_id,
                    &format!(
                        "⚠️ 文件为空，无法发送: `{file_name}`\n📁 路径: `{}`",
                        path.display()
                    ),
                )
                .await;
            }
            PreparedArtifactItem::Oversize { path, file_len } => {
                let size_label = format_file_size_label(file_len);
                let reason =
                    format!("超过 QQ 机器人 `file_data` 直传上限 (5 MB，当前 {size_label})");
                send_file_fallback_notice(client, target, msg_id, &path, file_len, &reason).await;
            }
            PreparedArtifactItem::Uploaded {
                path,
                file_len,
                file_type,
                used_generic_fallback,
                file_info,
            } => {
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("artifact.bin");
                let media_summary = format!("[AI 发送的 {}: {file_name}]", file_type.label());
                match client
                    .send_uploaded_media_with_meta(target, &file_info, msg_id, file_type)
                    .await
                {
                    Ok(sent_meta) => {
                        context::record_message(
                            &scope,
                            sent_meta.id.as_deref(),
                            sent_meta.ref_idx(),
                            "AI 助手",
                            &media_summary,
                            vec![path.clone()],
                            Vec::new(),
                        );
                        if used_generic_fallback {
                            info!(path = %path.display(), "QQ chat: delivered via file_type=4 fallback");
                        } else {
                            info!(
                                path = %path.display(),
                                file_type = file_type.as_u8(),
                                label = file_type.label(),
                                "QQ chat: delivered rich media / file"
                            );
                        }
                    }
                    Err(err) => {
                        warn!(
                            path = %path.display(),
                            file_type = file_type.as_u8(),
                            error = %format!("{err:#}"),
                            "QQ uploaded media message send failed"
                        );
                        let err_text = err.to_string();
                        let short_reason = truncate_chars(&err_text, 160);
                        let reason = format!("QQ 官方接口发送媒体失败 ({short_reason})");
                        send_file_fallback_notice(client, target, msg_id, &path, file_len, &reason)
                            .await;
                    }
                }
            }
            PreparedArtifactItem::UploadFailed {
                path,
                file_len,
                reason,
            } => {
                send_file_fallback_notice(client, target, msg_id, &path, file_len, &reason).await;
            }
        }
    }

    if prepared.skipped_overflow > 0 {
        sent_count += 1;
        let skipped = prepared.skipped_overflow;
        let _ = reply(
            client,
            target,
            msg_id,
            &format!(
                "ℹ️ 其余 {skipped} 个产物未自动发送（单轮上限 {MAX_AUTO_ARTIFACTS} 个），可使用 `/file <路径>` 按需获取。"
            ),
        )
        .await;
    }

    sent_count
}

/// Generates Fish Audio speech as MP3 (QQ voice accepts mp3/wav/silk) and uploads the
/// in-memory `Bytes` directly via `send_rich_media_bytes_with_meta` with zero disk I/O.
pub(crate) async fn send_qq_voice(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
) -> Result<()> {
    let audio = crate::tts::generate_speech_bytes(
        text,
        api_key,
        reference_id,
        crate::tts::TtsFormat::Mp3.as_str(),
    )
    .await?;
    if audio.len() as u64 > MAX_DIRECT_MEDIA_BYTES {
        anyhow::bail!(
            "voice is {} bytes, over the {} byte direct upload limit",
            audio.len(),
            MAX_DIRECT_MEDIA_BYTES
        );
    }
    let sent_meta = client
        .send_rich_media_bytes_with_meta(
            target,
            &audio,
            msg_id,
            QqMediaFileType::Voice,
            "voice.mp3",
        )
        .await?;
    let clean_script = strip_emotion_tags(text);
    context::record_message(
        &target.scope_key(),
        sent_meta.id.as_deref(),
        sent_meta.ref_idx(),
        "AI 助手",
        &clean_script,
        Vec::new(),
        Vec::new(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::collect_explicit_deliverables_with_brain_async;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn deliver_qq_artifacts_for_test(
        client: &QqClient,
        target: QqTarget<'_>,
        msg_id: &str,
        user_prompt: &str,
        output_text: &str,
        artifact_roots: &[PathBuf],
    ) -> String {
        let (cleaned, paths) = collect_explicit_deliverables_with_brain_async(
            user_prompt,
            output_text,
            artifact_roots,
            None,
        )
        .await;
        let prepared = prepare_qq_artifact_uploads(client, target, &paths).await;
        let _ = deliver_prepared_qq_artifacts(client, target, msg_id, prepared).await;
        cleaned
    }

    #[test]
    fn test_artifact_routing_and_utf8_prefix_safety() {
        assert_eq!(
            QqArtifactRoute::from_extension("PNG"),
            QqArtifactRoute::Image
        );
        assert_eq!(
            QqArtifactRoute::from_extension(".webp"),
            QqArtifactRoute::Image
        );
        assert_eq!(
            QqArtifactRoute::from_extension("mp4"),
            QqArtifactRoute::VideoMp4
        );
        assert_eq!(
            QqArtifactRoute::from_extension("silk"),
            QqArtifactRoute::Voice
        );
        assert_eq!(
            QqArtifactRoute::from_extension("mp3"),
            QqArtifactRoute::Voice
        );
        assert_eq!(
            QqArtifactRoute::from_extension("pdf"),
            QqArtifactRoute::GenericFile
        );
        assert_eq!(
            QqArtifactRoute::from_extension("csv"),
            QqArtifactRoute::GenericFile
        );
        assert_eq!(
            QqArtifactRoute::from_extension("zip"),
            QqArtifactRoute::GenericFile
        );

        assert_eq!(
            QqArtifactRoute::Image.primary_media_type(),
            QqMediaFileType::Image
        );
        assert_eq!(
            QqArtifactRoute::VideoMp4.primary_media_type(),
            QqMediaFileType::Video
        );
        assert_eq!(
            QqArtifactRoute::Voice.primary_media_type(),
            QqMediaFileType::Voice
        );
        assert_eq!(
            QqArtifactRoute::GenericFile.primary_media_type(),
            QqMediaFileType::File
        );

        let mut partial = "你好".as_bytes().to_vec();
        partial.extend_from_slice(&"世".as_bytes()[..2]);
        assert_eq!(utf8_prefix_str(&partial), Some("你好"));
        assert_eq!(utf8_prefix_str(b"abc\0def"), None);
        assert_eq!(utf8_prefix_str(&[0xFF, 0xFE]), None);
    }

    #[test]
    fn test_attachment_extension_from_content_type() {
        let mut att = MessageAttachment {
            url: "//gchat.qpic.cn/x".into(),
            content_type: Some("image/jpeg; charset=utf-8".into()),
            filename: None,
            size: None,
            voice_wav_url: None,
        };
        assert_eq!(attachment_extension(&att), "jpg");
        assert_eq!(attachment_source(&att).0, "https://gchat.qpic.cn/x");
        assert!(attachment_is_image(&att));

        att.content_type = Some("image/png".into());
        assert_eq!(attachment_extension(&att), "png");
        att.content_type = Some("video/mp4".into());
        assert_eq!(attachment_extension(&att), "mp4");
        att.content_type = Some("application/pdf".into());
        assert_eq!(attachment_extension(&att), "pdf");
        att.content_type = Some("unknown/type".into());
        assert_eq!(attachment_extension(&att), "bin");
    }

    #[test]
    fn test_voice_prefers_wav_transcode_and_overrides_amr_extension() {
        let att = MessageAttachment {
            url: "https://example.com/x.silk".into(),
            content_type: Some("voice".into()),
            filename: Some("record_001.amr".into()),
            size: None,
            voice_wav_url: Some("https://example.com/x.wav".into()),
        };
        let (url, ext) = attachment_source(&att);
        assert_eq!(url, "https://example.com/x.wav");
        assert_eq!(ext, "wav");

        let built = build_attachment_filename("qq-abc123", &att, ext);
        assert!(
            built.ends_with(".wav"),
            "WAV voice transcode must override .amr extension, got: {built}"
        );
        assert!(!built.ends_with(".amr"));

        let silk_only = MessageAttachment {
            voice_wav_url: None,
            ..att
        };
        let (url, ext) = attachment_source(&silk_only);
        assert_eq!(url, "https://example.com/x.silk");
        assert_eq!(ext, "silk");
    }

    #[test]
    fn test_message_id_is_reduced_to_a_safe_token() {
        assert_eq!(sanitize_message_id("ROBOT1.0_abc/def"), "ROBOT10abcdef");
        assert_eq!(sanitize_message_id(""), "x");
        assert_eq!(sanitize_message_id("../../etc/passwd"), "etcpasswd");
    }

    #[test]
    fn test_strip_emotion_tags_borrows_when_clean_and_strips_cues() {
        assert_eq!(
            strip_emotion_tags("[calm]先说结论。[excited][emphasis]太好了！"),
            "先说结论。太好了！"
        );
        let clean = "参考 [文档](https://example.com) 与 [1, 2]";
        let res = strip_emotion_tags(clean);
        assert_eq!(res, clean);
        assert!(matches!(res, Cow::Borrowed(_)));
    }

    #[test]
    fn test_quoted_file_prompt_line_avoids_double_file_typo() {
        let mut out = String::new();
        write_attachment_prompt_line(&mut out, "Quoted", "File", Path::new("/tmp/doc.pdf"));
        write_attachment_prompt_line(&mut out, "Quoted", "Image", Path::new("/tmp/pic.png"));
        assert_eq!(
            out,
            "[Quoted File: /tmp/doc.pdf]\n[Quoted Image File: /tmp/pic.png]\n"
        );
        assert!(!out.contains("File File"));
    }

    #[tokio::test]
    async fn test_deliver_qq_artifacts_routes_media_and_falls_back_gracefully() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let rec = recorded.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let rec = rec.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end;
                    loop {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }

                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_len {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let req_line = headers.lines().next().unwrap_or("").to_string();
                    let body = String::from_utf8_lossy(
                        &buf[header_end..(header_end + content_len).min(buf.len())],
                    )
                    .to_string();

                    let (status, resp_body) = if req_line
                        .starts_with("POST /app/getAppAccessToken ")
                    {
                        (
                            200,
                            r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                        )
                    } else if req_line.contains("/files ") {
                        rec.lock().unwrap().push((req_line, body.clone()));
                        if body.contains(r#""file_type":4"#)
                            && !body.contains("han_dynasty_50_questions.md")
                        {
                            (
                                400,
                                r#"{"code":850019,"message":"unsupported file type"}"#.to_string(),
                            )
                        } else {
                            (200, r#"{"file_info":"fi_ok"}"#.to_string())
                        }
                    } else {
                        rec.lock().unwrap().push((req_line, body));
                        (200, r#"{"id":"m_ok"}"#.to_string())
                    };
                    let raw = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!("agygram_qq_art_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let scratch = dir.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();

        let png = scratch.join("chart.png");
        let mp4 = scratch.join("demo.mp4");
        let csv = dir.join("metrics.csv");
        let pdf = dir.join("report.pdf");
        std::fs::write(&png, b"\x89PNG\r\n").unwrap();
        std::fs::write(&mp4, b"mp4-data").unwrap();
        std::fs::write(&csv, b"col1,col2\n1,2\n").unwrap();
        std::fs::write(&pdf, b"%PDF-1.4\nbinary\x00data").unwrap();

        let base = format!("http://{addr}");
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let md = scratch.join("han_dynasty_50_questions.md");
        std::fs::write(&md, b"# Han Dynasty\n50 questions").unwrap();

        let output = format!(
            "Artifacts: {} {} {} {}",
            png.display(),
            mp4.display(),
            csv.display(),
            pdf.display()
        );
        deliver_qq_artifacts_for_test(
            &client,
            QqTarget::Group("g1"),
            "msg_0",
            "帮我检查一下工作区里的文件状态",
            &output,
            std::slice::from_ref(&dir),
        )
        .await;
        assert!(
            recorded.lock().unwrap().is_empty(),
            "unrequested turn must not deliver artifacts"
        );

        deliver_qq_artifacts_for_test(
            &client,
            QqTarget::Group("g1"),
            "msg_1",
            "请把 chart.png demo.mp4 metrics.csv report.pdf 发到群里",
            &output,
            std::slice::from_ref(&dir),
        )
        .await;

        let cleaned = deliver_qq_artifacts_for_test(
            &client,
            QqTarget::Group("g1"),
            "msg_2",
            "把刚才的题库发过来",
            &format!("已发出。\n[[send_file:{}]]", md.display()),
            std::slice::from_ref(&dir),
        )
        .await;
        assert_eq!(cleaned, "已发出。");

        let calls = recorded.lock().unwrap();
        let all_bodies: String = calls
            .iter()
            .map(|(_, b)| b.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_bodies.contains(r#""file_type":1"#));
        assert!(all_bodies.contains(r#""file_type":2"#));
        assert!(all_bodies.contains(r#""file_type":4"#));
        assert!(all_bodies.contains("han_dynasty_50_questions.md"));
        assert!(all_bodies.contains("col1,col2"));
        assert!(all_bodies.contains("report.pdf"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_download_attachments_runs_concurrently_preserves_order_and_deduplicates() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits_c = hits_srv.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (status, body) = if req.starts_with("GET /a ") {
                        hits_c.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", "img-a")
                    } else if req.starts_with("GET /b ") {
                        hits_c.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", "file-b")
                    } else {
                        ("404 Not Found", "")
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!("agygram_qq_att_conc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let base = format!("http://{addr}");
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let atts = vec![
            MessageAttachment {
                url: format!("{base}/a"),
                content_type: Some("image/png".into()),
                filename: Some("first.png".into()),
                size: Some(5),
                voice_wav_url: None,
            },
            MessageAttachment {
                url: format!("{base}/missing"),
                content_type: Some("text/plain".into()),
                filename: Some("broken.txt".into()),
                size: None,
                voice_wav_url: None,
            },
            MessageAttachment {
                url: format!("{base}/b"),
                content_type: Some("application/pdf".into()),
                filename: Some("third.pdf".into()),
                size: Some(6),
                voice_wav_url: None,
            },
        ];

        let (note, paths1, failures) =
            download_attachments_with_paths(&client, &atts, &dir, Some("REF_IDX_42"), "Attached")
                .await;
        assert_eq!(failures, vec!["2. broken.txt"]);
        assert_eq!(paths1.len(), 2);
        let lines: Vec<_> = note.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[Attached Image File:") && lines[0].contains("first.png"));
        assert!(
            lines[1].contains("[Attached File:")
                && !lines[1].contains("File File")
                && lines[1].contains("third.pdf")
        );
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        // Repeat download with the same dedup ref_idx reuses cached files with 0 extra HTTP hits.
        let (_, paths2, _) =
            download_attachments_with_paths(&client, &atts, &dir, Some("REF_IDX_42"), "Quoted")
                .await;
        assert_eq!(paths1, paths2);
        assert_eq!(hits.load(Ordering::SeqCst), 2);

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_cleanup_old_scratch_attachments_respects_lru_and_preserves_user_files() {
        let dir = std::env::temp_dir().join(format!("agygram_qq_gc_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let auto1 = dir.join("mqq-111111111111-old.bin");
        let auto2 = dir.join("mqq-222222222222-new.bin");
        let user_m = dir.join("metrics.csv");
        let user_reply = dir.join("reply-123.md");

        std::fs::write(&auto1, vec![0u8; 60]).unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        std::fs::write(&auto2, vec![0u8; 60]).unwrap();
        std::fs::write(&user_m, vec![0u8; 200]).unwrap();
        std::fs::write(&user_reply, b"full reply").unwrap();

        assert!(is_auto_downloaded_scratch_attachment(
            "mqq-111111111111-old.bin"
        ));
        assert!(is_auto_downloaded_scratch_attachment(
            "mROBOT10abc-0-pic.png"
        ));
        assert!(!is_auto_downloaded_scratch_attachment("metrics.csv"));
        assert!(!is_auto_downloaded_scratch_attachment("my-notes.md"));

        // Budget of 80 bytes must evict the older auto1 (60B) while keeping auto2 (60B) and user files.
        let (removed, bytes) =
            cleanup_old_scratch_attachments_with_limits(&dir, Duration::from_secs(3600), 80).await;
        assert_eq!(removed, 1);
        assert_eq!(bytes, 60);
        assert!(!auto1.exists());
        assert!(auto2.exists());
        assert!(user_m.exists());
        assert!(user_reply.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
