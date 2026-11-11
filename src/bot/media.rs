//! Telegram media ingress/egress: attachment download into the scratch directory and automatic
//! delivery of generated artifacts. Everything here is bounded by the hosted Bot API's limits:
//! files download up to 20 MB, upload up to 50 MB, `sendPhoto` up to 10 MB, captions 1024 chars.

use crate::artifacts;
use crate::text::{self, looks_sensitive, safe_attachment_name};
use std::path::{Path, PathBuf};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InputFile, ParseMode};
use tracing::{error, info, warn};

pub const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_UPLOAD_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_PHOTO_BYTES: u64 = 10 * 1024 * 1024;
const MAX_CAPTION_CHARS: usize = 1024;

/// An inbound attachment normalized across the media kinds Telegram uses.
struct Incoming<'a> {
    file_id: &'a str,
    label: &'static str,
    name_hint: Option<&'a str>,
    fallback_name: &'static str,
    size: u32, // Telegram fills an unknown size with u32::MAX, so 4 GiB is "not reported"
}

/// Telegram fills an unknown media size with `u32::MAX` rather than omitting it. Photos and voice
/// notes arrive that way, so the sentinel must not be read as "4 GiB" and rejected outright.
fn over_download_limit(raw_size: u32) -> bool {
    raw_size != u32::MAX && u64::from(raw_size) > MAX_DOWNLOAD_BYTES
}

fn incoming(msg: &Message) -> Option<Incoming<'_>> {
    if let Some(doc) = msg.document() {
        return Some(Incoming {
            file_id: &doc.file.id,
            label: "Document",
            name_hint: doc.file_name.as_deref(),
            fallback_name: "attached_file.bin",
            size: doc.file.size,
        });
    }
    if let Some(photos) = msg.photo() {
        let photo = photos.last()?;
        return Some(Incoming {
            file_id: &photo.file.id,
            label: "Image",
            name_hint: None,
            fallback_name: "photo.jpg",
            size: photo.file.size,
        });
    }
    if let Some(voice) = msg.voice() {
        return Some(Incoming {
            file_id: &voice.file.id,
            label: "Voice",
            name_hint: None,
            fallback_name: "voice.ogg",
            size: voice.file.size,
        });
    }
    if let Some(video) = msg.video() {
        return Some(Incoming {
            file_id: &video.file.id,
            label: "Video",
            name_hint: video.file_name.as_deref(),
            fallback_name: "video.mp4",
            size: video.file.size,
        });
    }
    if let Some(audio) = msg.audio() {
        return Some(Incoming {
            file_id: &audio.file.id,
            label: "Audio",
            name_hint: audio.file_name.as_deref(),
            fallback_name: "audio.mp3",
            size: audio.file.size,
        });
    }
    if let Some(animation) = msg.animation() {
        return Some(Incoming {
            file_id: &animation.file.id,
            label: "Animation",
            name_hint: animation.file_name.as_deref(),
            fallback_name: "animation.mp4",
            size: animation.file.size,
        });
    }
    None
}

/// Returns `true` if `msg` carries a downloadable media attachment.
#[inline]
#[must_use]
pub(crate) fn has_supported_media(msg: &Message) -> bool {
    incoming(msg).is_some()
}

/// Download the message's attachment into `scratch_dir`, telling the user when it cannot be had.
/// Returning `None` means "no attachment text to inject", never silence.
#[tracing::instrument(skip_all, fields(chat_id = msg.chat.id.0, message_id = msg.id.0))]
pub async fn download_media_attachment(
    bot: &Bot,
    msg: &Message,
    scratch_dir: &Path,
) -> Option<String> {
    let att = incoming(msg)?;
    let chat_id = msg.chat.id.0;

    // u32::MAX means "size not reported" (photos and voice notes), not "4 GiB": skipping the
    // pre-check there lets get_file/download decide, and their failure is surfaced to the user.
    if over_download_limit(att.size) {
        let mb = f64::from(att.size) / 1_048_576.0;
        warn!(
            chat_id,
            label = att.label,
            mb,
            "skipping oversized attachment"
        );
        if let Err(err) = bot
            .send_message(
                msg.chat.id,
                format!(
                    "⚠️ <b>附件过大已跳过</b>\n{att_label}: <code>{mb:.1} MB</code>，超过 Bot API 单次下载上限 20 MB。可把文件直接放到工作目录后用路径引用。",
                    att_label = att.label
                ),
            )
            .parse_mode(ParseMode::Html)
            .await
        {
            warn!(chat_id, error = ?err, "failed to send oversized attachment notice");
        }
        return None;
    }

    let name = safe_attachment_name(msg.id.0, att.name_hint, att.fallback_name);
    let dest_path = scratch_dir.join(&name);

    let file_info = match bot.get_file(att.file_id).await {
        Ok(info) => info,
        Err(err) => {
            error!(chat_id, file_id = att.file_id, error = ?err, "get_file failed");
            notify_media_failure(bot, msg, att.label, &format!("{err}")).await;
            return None;
        }
    };

    let mut file_out = match tokio::fs::File::create(&dest_path).await {
        Ok(file) => file,
        Err(err) => {
            error!(chat_id, path = %dest_path.display(), error = %err, "cannot create attachment file");
            notify_media_failure(bot, msg, att.label, &format!("本地写入失败: {err}")).await;
            return None;
        }
    };

    if let Err(err) = bot.download_file(&file_info.path, &mut file_out).await {
        error!(chat_id, path = %dest_path.display(), error = ?err, "attachment download failed");
        if let Err(rm_err) = tokio::fs::remove_file(&dest_path).await
            && rm_err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(chat_id, path = %dest_path.display(), error = %rm_err, "failed to clean partial attachment");
        }
        notify_media_failure(bot, msg, att.label, &format!("{err}")).await;
        return None;
    }

    info!(chat_id, label = att.label, path = %dest_path.display(), "saved attachment");
    Some(format!(
        "[Attached {} File: {}]\n",
        att.label,
        dest_path.display()
    ))
}

async fn notify_media_failure(bot: &Bot, msg: &Message, label: &str, reason: &str) {
    if let Err(err) = bot
        .send_message(
            msg.chat.id,
            format!(
                "⚠️ <b>{label} 附件未能获取</b>\n<code>{reason}</code>\n任务将继续，但模型看不到这个文件。",
                label = text::escape_html(label),
                reason = text::escape_html(reason)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await
    {
        warn!(chat_id = msg.chat.id.0, error = ?err, "failed to notify media download failure");
    }
}

/// Which Telegram method can carry this file, respecting the API ceilings. Images above the
/// `sendPhoto` limit go out as documents rather than failing the upload outright. GIF must use
/// `sendAnimation` (Telegram rejects it as a photo) and sound-bearing containers `sendVideo`
/// (`sendAnimation` is documented as GIF or H.264 *without* sound).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryChannel {
    Animation,
    Video,
    Photo,
    Document,
    Oversize,
    Blocked,
}

impl DeliveryChannel {
    #[inline]
    const fn as_str(self) -> &'static str {
        match self {
            Self::Animation => "animation",
            Self::Video => "video",
            Self::Photo => "photo",
            Self::Document => "document",
            Self::Oversize => "oversize",
            Self::Blocked => "blocked",
        }
    }
}

fn delivery_channel(extension: &str, size: u64) -> DeliveryChannel {
    if extension.is_empty() || artifacts::is_blocked_artifact_extension(extension) {
        return DeliveryChannel::Blocked;
    }
    let is_photo = matches!(extension, "png" | "jpg" | "jpeg" | "webp");
    let is_animation = extension == "gif";
    let is_video = matches!(extension, "mp4" | "mov" | "m4v" | "webm" | "mkv");
    if is_animation && size <= MAX_UPLOAD_BYTES {
        DeliveryChannel::Animation
    } else if is_video && size <= MAX_UPLOAD_BYTES {
        DeliveryChannel::Video
    } else if is_photo && size <= MAX_PHOTO_BYTES {
        DeliveryChannel::Photo
    } else if size <= MAX_UPLOAD_BYTES {
        DeliveryChannel::Document
    } else {
        DeliveryChannel::Oversize
    }
}

/// HTML-escapes `raw` while guaranteeing the escaped output fits within `max_escaped_chars`
/// characters without slicing an HTML entity (`&amp;`, `&lt;`, etc.) or UTF-8 code point.
fn escape_html_truncated(raw: &str, max_escaped_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_escaped_chars.saturating_mul(2)));
    let mut escaped_chars = 0usize;
    let limit_with_ellipsis = max_escaped_chars.saturating_sub(3);
    let mut checkpoint_byte_len = 0usize;
    let mut truncated = false;

    for c in raw.chars() {
        let entity = match c {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '"' => Some("&quot;"),
            '\'' => Some("&#39;"),
            _ => None,
        };
        let step_chars = entity.map_or(1, str::len);
        if escaped_chars + step_chars > max_escaped_chars {
            truncated = true;
            break;
        }
        if let Some(ent) = entity {
            out.push_str(ent);
        } else {
            out.push(c);
        }
        escaped_chars += step_chars;
        if escaped_chars <= limit_with_ellipsis {
            checkpoint_byte_len = out.len();
        }
    }

    if truncated {
        out.truncate(checkpoint_byte_len);
        if max_escaped_chars >= 3 {
            out.push_str("...");
        }
    }
    out
}

/// Formats an HTML file caption that strictly obeys Telegram's 1024-character limit after escaping.
#[must_use]
pub(crate) fn format_file_caption(icon: &str, label: &str, file_name: &str) -> String {
    let escaped_label = text::escape_html(label);
    let prefix = format!("{icon} <b>{escaped_label}:</b> <code>");
    let suffix = "</code>";
    let budget = MAX_CAPTION_CHARS.saturating_sub(prefix.chars().count() + suffix.len());
    let shown = escape_html_truncated(file_name, budget);
    format!("{prefix}{shown}{suffix}")
}

#[cfg(test)]
fn caption(text_icon: &str, label: &str, file_name: &str) -> String {
    format_file_caption(text_icon, label, file_name)
}

/// Scan the model's reply for artifact paths and deliver them, confined to the directories the
/// run was actually given write access to.
#[tracing::instrument(skip_all, fields(chat_id = target_chat.0))]
pub async fn auto_deliver_detected_artifacts(
    bot: &Bot,
    target_chat: ChatId,
    output_text: &str,
    allowed_dirs: &[PathBuf],
) {
    let chat_id = target_chat.0;

    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = tokio::fs::canonicalize(dir).await
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    let scratch_roots = artifacts::collect_scratch_roots_async(&canonical_roots).await;

    for resolved in artifacts::collect_artifact_paths_async(output_text, allowed_dirs).await {
        let file_name = resolved
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if looks_sensitive(&file_name) || looks_sensitive(&resolved.to_string_lossy()) {
            warn!(chat_id, path = ?resolved, "refusing to auto-deliver sensitive-looking file");
            if let Err(err) = bot
                .send_message(
                    target_chat,
                    format!(
                        "🔒 <b>已阻止自动发送疑似敏感文件</b>\n<code>{name}</code>\n需要它时请显式使用 <code>/file</code>。",
                        name = text::escape_html(&file_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
            {
                warn!(chat_id, error = ?err, "failed to send sensitive-file block notice");
            }
            continue;
        }

        let is_in_scratch = scratch_roots.iter().any(|sr| resolved.starts_with(sr));
        if !artifacts::is_deliverable_artifact_path(&resolved, is_in_scratch) {
            warn!(
                chat_id,
                path = %resolved.display(),
                is_in_scratch,
                "skipping non-deliverable workspace artifact"
            );
            continue;
        }

        let meta = match tokio::fs::metadata(&resolved).await {
            Ok(m) => m,
            Err(err) => {
                warn!(chat_id, path = %resolved.display(), error = %err, "failed to stat artifact");
                continue;
            }
        };
        let extension = resolved
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();
        let channel = delivery_channel(&extension, meta.len());

        if channel == DeliveryChannel::Blocked {
            warn!(
                chat_id,
                path = %resolved.display(),
                "skipping blocked code/binary/script artifact"
            );
            continue;
        }

        if channel == DeliveryChannel::Oversize {
            error!(
                chat_id,
                path = %resolved.display(),
                size_mb = meta.len() / 1_048_576,
                "artifact exceeds 50 MB Bot API upload ceiling"
            );
            if let Err(err) = bot
                .send_message(
                    target_chat,
                    format!(
                        "⚠️ <b>产物过大未发送</b>\n<code>{name}</code> 约 <code>{mb:.1} MB</code>，超过 Bot API 的 50 MB 上传上限。可在服务器直接取用该路径。",
                        name = text::escape_html(&file_name),
                        mb = meta.len() as f64 / 1_048_576.0
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
            {
                warn!(chat_id, error = ?err, "failed to send oversize artifact notice");
            }
            continue;
        }

        let input = InputFile::file(&resolved);
        let send_result = match channel {
            DeliveryChannel::Animation => bot
                .send_animation(target_chat, input)
                .caption(format_file_caption("🎬", "Generated Animation", &file_name))
                .parse_mode(ParseMode::Html)
                .await
                .map(|_| ()),
            DeliveryChannel::Video => bot
                .send_video(target_chat, input)
                .caption(format_file_caption("🎞️", "Generated Video", &file_name))
                .parse_mode(ParseMode::Html)
                .await
                .map(|_| ()),
            DeliveryChannel::Photo => bot
                .send_photo(target_chat, input)
                .caption(format_file_caption("🖼️", "Generated Image", &file_name))
                .parse_mode(ParseMode::Html)
                .await
                .map(|_| ()),
            DeliveryChannel::Document => bot
                .send_document(target_chat, input)
                .caption(format_file_caption("📄", "Generated Artifact", &file_name))
                .parse_mode(ParseMode::Html)
                .await
                .map(|_| ()),
            DeliveryChannel::Oversize | DeliveryChannel::Blocked => {
                unreachable!("oversize and blocked handled above")
            }
        };

        if let Err(err) = send_result {
            error!(
                chat_id,
                channel = channel.as_str(),
                path = %resolved.display(),
                error = ?err,
                "artifact upload failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_cannot_escape_the_scratch_directory() {
        for hostile in [
            "../../etc/cron.d/evil",
            "/home/user/.ssh/authorized_keys",
            "..\\..\\windows\\system32\\x",
            "\u{0}.bashrc",
            "….bashrc",
            "a/../../b",
        ] {
            let name = safe_attachment_name(7, Some(hostile), "fallback.bin");
            let path = Path::new(&name);
            assert!(
                path.is_relative() && path.components().count() == 1,
                "not a single safe component: {name}"
            );
            assert_ne!(name.as_str(), "..");
            assert!(!name.starts_with("m7-."), "dot file survived: {name}");
            assert!(name.starts_with("m7-"));
        }
    }

    #[test]
    fn missing_or_empty_names_fall_back() {
        assert_eq!(safe_attachment_name(3, None, "photo.jpg"), "m3-photo.jpg");
        assert_eq!(
            safe_attachment_name(3, Some("   "), "voice.ogg"),
            "m3-voice.ogg"
        );
        assert_eq!(
            safe_attachment_name(3, Some("..."), "video.mp4"),
            "m3-video.mp4"
        );
    }

    #[test]
    fn long_names_are_clipped_without_splitting_glyphs() {
        let name = safe_attachment_name(9, Some(&"中".repeat(120)), "f.bin");
        assert!(
            name.chars().count() <= 9 + 60,
            "not clipped: {}",
            name.len()
        );
        assert!(name.contains('中'), "CJK lost: {name}");
    }

    #[test]
    fn message_id_prefixes_prevent_overwrites() {
        assert_ne!(
            safe_attachment_name(1, Some("a.txt"), "f.bin"),
            safe_attachment_name(2, Some("a.txt"), "f.bin")
        );
    }

    #[test]
    fn unknown_media_size_is_not_read_as_four_gigabytes() {
        assert!(
            !over_download_limit(u32::MAX),
            "unknown size must not be rejected"
        );
        assert!(!over_download_limit(1024));
        assert!(!over_download_limit(MAX_DOWNLOAD_BYTES as u32));
        assert!(over_download_limit(MAX_DOWNLOAD_BYTES as u32 + 1));
    }

    #[test]
    fn channel_choice_respects_telegram_limits() {
        assert_eq!(delivery_channel("png", 1024), DeliveryChannel::Photo);
        assert_eq!(
            delivery_channel("jpg", MAX_PHOTO_BYTES),
            DeliveryChannel::Photo
        );
        assert_eq!(
            delivery_channel("png", MAX_PHOTO_BYTES + 1),
            DeliveryChannel::Document
        );
        assert_eq!(delivery_channel("gif", 4096), DeliveryChannel::Animation);
        assert_eq!(delivery_channel("mp4", 4096), DeliveryChannel::Video);
        assert_eq!(delivery_channel("mkv", 4096), DeliveryChannel::Video);
        assert_eq!(
            delivery_channel("mov", MAX_UPLOAD_BYTES),
            DeliveryChannel::Video
        );
        assert_eq!(
            delivery_channel("mp4", MAX_UPLOAD_BYTES + 1),
            DeliveryChannel::Oversize
        );
        assert_eq!(
            delivery_channel("pdf", MAX_UPLOAD_BYTES),
            DeliveryChannel::Document
        );
        assert_eq!(
            delivery_channel("zip", MAX_UPLOAD_BYTES + 1),
            DeliveryChannel::Oversize
        );
        assert_eq!(
            delivery_channel("tar", MAX_UPLOAD_BYTES + 1),
            DeliveryChannel::Oversize
        );
        assert_eq!(
            delivery_channel("bin", MAX_DOWNLOAD_BYTES * 4),
            DeliveryChannel::Blocked
        );
        assert_eq!(delivery_channel("", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("rs", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("py", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("toml", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("sh", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("md", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("txt", 1024), DeliveryChannel::Blocked);
    }

    #[test]
    fn workspace_files_and_blocked_code_are_not_auto_delivered() {
        let cargo_toml = Path::new("/home/azureuser/agygram-src/Cargo.toml");
        let agygram_bin = Path::new("/home/azureuser/agygram");
        let update_script = Path::new("/home/azureuser/update-agygram.sh");
        let report_pdf = Path::new("/home/azureuser/report.pdf");
        let plot_png = Path::new("/home/azureuser/plot.png");

        // Outside scratch_dir (general workspace):
        // Cargo.toml, agygram binary, update-agygram.sh are NOT deliverable
        assert!(!artifacts::is_deliverable_artifact_path(cargo_toml, false));
        assert!(!artifacts::is_deliverable_artifact_path(agygram_bin, false));
        assert!(!artifacts::is_deliverable_artifact_path(
            update_script,
            false
        ));
        assert_eq!(delivery_channel("toml", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("", 1024), DeliveryChannel::Blocked);
        assert_eq!(delivery_channel("sh", 1024), DeliveryChannel::Blocked);

        // While valid export deliverables and media are deliverable:
        assert!(artifacts::is_deliverable_artifact_path(report_pdf, false));
        assert!(artifacts::is_deliverable_artifact_path(plot_png, false));
        assert_eq!(delivery_channel("pdf", 1024), DeliveryChannel::Document);
        assert_eq!(delivery_channel("png", 1024), DeliveryChannel::Photo);

        // Inside scratch_dir:
        // Even in scratch, blocked code / binary / scripts cannot be delivered as Documents
        let scratch_cargo = Path::new("/home/azureuser/scratch/Cargo.toml");
        let scratch_bin = Path::new("/home/azureuser/scratch/agygram");
        let scratch_script = Path::new("/home/azureuser/scratch/update-agygram.sh");
        assert!(!artifacts::is_deliverable_artifact_path(
            scratch_cargo,
            true
        ));
        assert!(!artifacts::is_deliverable_artifact_path(scratch_bin, true));
        assert!(!artifacts::is_deliverable_artifact_path(
            scratch_script,
            true
        ));

        // Valid scratch media and exports are allowed
        let scratch_pdf = Path::new("/home/azureuser/scratch/report.pdf");
        let scratch_png = Path::new("/home/azureuser/scratch/plot.png");
        assert!(artifacts::is_deliverable_artifact_path(scratch_pdf, true));
        assert!(artifacts::is_deliverable_artifact_path(scratch_png, true));
    }

    #[test]
    fn sensitive_names_are_blocked() {
        for name in [
            ".env",
            ".gitconfig",
            "id_rsa",
            "server.pem",
            "tls.key",
            "gcp-credentials.json",
            "app_secrets.yaml",
            "api_key.txt",
            "netrc",
        ] {
            assert!(looks_sensitive(name), "{name} should be blocked");
        }
        for name in [
            "chart.png",
            "report.pdf",
            "data.csv",
            "main.rs",
            "index.html",
        ] {
            assert!(!looks_sensitive(name), "{name} should be allowed");
        }
    }

    #[test]
    fn captions_stay_under_the_telegram_limit_and_escape_html() {
        let long_name = format!("{}.png", "x".repeat(4000));
        let html = caption("🖼️", "Generated Image", &long_name);
        assert!(
            html.chars().count() <= MAX_CAPTION_CHARS,
            "caption exceeded 1024 chars: {}",
            html.chars().count()
        );
        assert!(html.contains("..."), "expected an ellipsis marker");

        let special_name = "report <v1> & \"final\" 'ok'.png";
        let escaped = caption("📄", "Generated Artifact", special_name);
        assert!(escaped.contains("report &lt;v1&gt; &amp; &quot;final&quot; &#39;ok&#39;.png"));

        let dense_entities = "<&>".repeat(500);
        let dense_caption = caption("📄", "Generated Artifact", &dense_entities);
        assert!(
            dense_caption.chars().count() <= MAX_CAPTION_CHARS,
            "escaped caption exceeded 1024 chars: {}",
            dense_caption.chars().count()
        );
        assert!(dense_caption.ends_with("...</code>"));
        assert!(!dense_caption.ends_with("&...</code>"));
        assert!(!dense_caption.ends_with("&l...</code>"));
        assert!(!dense_caption.ends_with("&lt...</code>"));
    }
}
