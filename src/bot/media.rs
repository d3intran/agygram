//! Telegram media ingress/egress: attachment download into the scratch directory and automatic
//! delivery of generated artifacts. Everything here is bounded by the hosted Bot API's limits:
//! files download up to 20 MB, upload up to 50 MB, `sendPhoto` up to 10 MB, captions 1024 chars.

use crate::bot::formatter::TelegramFormatter;
use crate::bot::pipeline::is_message_not_modified;
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
    raw_size != u32::MAX && raw_size as u64 > MAX_DOWNLOAD_BYTES
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

/// Turn an attacker-supplied name into one that is safe to join onto the scratch directory.
///
/// Telegram's `file_name` is attacker controlled - a forwarded file keeps the name its original
/// sender chose - and `Path::join` replaces the base entirely when the argument is absolute, so
/// the previous direct join could write outside the workspace (or overwrite `~/.bashrc`).
/// Separators, control characters, leading dots and `..` are all neutralized, and the message id
/// prefixes the name so two attachments cannot silently overwrite each other.
#[must_use]
pub fn safe_attachment_name(message_id: impl std::fmt::Display, hint: Option<&str>, fallback: &str) -> String {
    let flattened: String = hint
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Trimming dots removes leading dot-files (".env") and collapses ".." traversals.
    let cleaned = flattened.trim().trim_matches(['.', ' ']);
    let base = if cleaned.is_empty() {
        fallback
    } else {
        TelegramFormatter::take_chars(cleaned, 60)
    };
    format!("m{message_id}-{base}")
}

/// Download the message's attachment into `scratch_dir`, telling the user when it cannot be had.
/// Returning None means "no attachment text to inject", never silence.
pub async fn download_media_attachment(bot: &Bot, msg: &Message, scratch_dir: &Path) -> Option<String> {
    let att = incoming(msg)?;

    // u32::MAX means "size not reported" (photos and voice notes), not "4 GiB": skipping the
    // pre-check there lets get_file/download decide, and their failure is surfaced to the user.
    if over_download_limit(att.size) {
        let mb = att.size as f64 / 1_048_576.0;
        warn!("chat {}: skipping {} attachment of {mb:.1} MB", msg.chat.id.0, att.label);
        let _ = bot
            .send_message(
                msg.chat.id,
                format!(
                    "⚠️ <b>附件过大已跳过</b>\n{att_label}: <code>{mb:.1} MB</code>，超过 Bot API 单次下载上限 20 MB。可把文件直接放到工作目录后用路径引用。",
                    att_label = att.label
                ),
            )
            .parse_mode(ParseMode::Html)
            .await;
        return None;
    }

    let name = safe_attachment_name(msg.id.0, att.name_hint, att.fallback_name);
    let dest_path = scratch_dir.join(&name);

    let file_info = match bot.get_file(att.file_id).await {
        Ok(info) => info,
        Err(err) => {
            error!("chat {}: get_file failed for {}: {err:?}", msg.chat.id.0, att.file_id);
            notify_media_failure(bot, msg, att.label, &format!("{err}")).await;
            return None;
        }
    };

    let mut file_out = match tokio::fs::File::create(&dest_path).await {
        Ok(file) => file,
        Err(err) => {
            error!("chat {}: cannot create {}: {err}", msg.chat.id.0, dest_path.display());
            notify_media_failure(bot, msg, att.label, &format!("本地写入失败: {err}")).await;
            return None;
        }
    };

    if let Err(err) = bot.download_file(&file_info.path, &mut file_out).await {
        error!("chat {}: download failed for {}: {err:?}", msg.chat.id.0, dest_path.display());
        let _ = tokio::fs::remove_file(&dest_path).await;
        notify_media_failure(bot, msg, att.label, &format!("{err}")).await;
        return None;
    }

    info!("chat {}: saved {} attachment to {}", msg.chat.id.0, att.label, dest_path.display());
    Some(format!("[Attached {} File: {}]\n", att.label, dest_path.display()))
}

async fn notify_media_failure(bot: &Bot, msg: &Message, label: &str, reason: &str) {
    let _ = bot
        .send_message(
            msg.chat.id,
            format!(
                "⚠️ <b>{label} 附件未能获取</b>\n<code>{reason}</code>\n任务将继续，但模型看不到这个文件。",
                label = TelegramFormatter::escape_html(label),
                reason = TelegramFormatter::escape_html(reason)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await;
}

/// Keep a candidate only if it resolves to a regular file inside one of `canonical_roots`.
/// The tokens come from model output, which any page or file `agy` reads can steer, so without
/// confinement a reply mentioning /home/user/secrets.json would ship that file to Telegram.
/// Canonicalizing is what also defeats `../` traversal and symlink escapes.
fn resolve_confined_artifact(candidate: &Path, canonical_roots: &[PathBuf]) -> Option<PathBuf> {
    let resolved = candidate.canonicalize().ok()?;
    if !resolved.is_file() {
        return None;
    }
    canonical_roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

/// Names that must never leave the machine just because a model mentioned them. Confinement is
/// the primary guard; this is the second layer for secrets that live inside a workspace.
#[must_use]
pub fn looks_sensitive(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.starts_with('.') {
        return true;
    }
    [
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        ".pem",
        ".p12",
        ".pfx",
        ".key",
        "netrc",
        "credentials",
        "secret",
        "passwd",
        "shadow",
        "api_key",
        "apikey",
        "/.git/",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Which Telegram method can carry this file, respecting the API ceilings. Images above the
/// `sendPhoto` limit go out as documents rather than failing the upload outright. GIF must use
/// `sendAnimation` (Telegram rejects it as a photo) and sound-bearing containers `sendVideo`
/// (`sendAnimation` is documented as GIF or H.264 *without* sound).
fn delivery_channel(extension: &str, size: u64) -> &'static str {
    let is_photo = matches!(extension, "png" | "jpg" | "jpeg" | "webp");
    let is_animation = extension == "gif";
    let is_video = matches!(extension, "mp4" | "mov" | "m4v" | "webm" | "mkv");
    if is_animation && size <= MAX_UPLOAD_BYTES {
        "animation"
    } else if is_video && size <= MAX_UPLOAD_BYTES {
        "video"
    } else if is_photo && size <= MAX_PHOTO_BYTES {
        "photo"
    } else if size <= MAX_UPLOAD_BYTES {
        "document"
    } else {
        "oversize"
    }
}

fn caption(text: &str, label: &str, file_name: &str) -> String {
    let shown = TelegramFormatter::truncate_chars(file_name, MAX_CAPTION_CHARS.saturating_sub(label.chars().count() + 8));
    format!("{} <b>{label}:</b> <code>{shown}</code>", TelegramFormatter::escape_html(text))
}

/// Scan the model's reply for artifact paths and deliver them, confined to the directories the
/// run was actually given write access to.
pub async fn auto_deliver_detected_artifacts(
    bot: &Bot,
    target_chat: ChatId,
    output_text: &str,
    allowed_dirs: &[PathBuf],
) {
    let canonical_roots: Vec<PathBuf> = allowed_dirs
        .iter()
        .filter_map(|dir| dir.canonicalize().ok())
        .collect();
    if canonical_roots.is_empty() {
        return;
    }
    let chat_id = target_chat.0;

    let mut sent_paths = std::collections::HashSet::new();

    for token in output_text.split_whitespace() {
        let clean_token = token
            .trim_matches(['(', ')', '[', ']', '"', '\'', '<', '>', '`'])
            .trim_start_matches("file://");
        let path = PathBuf::from(clean_token);
        if !path.is_absolute() {
            continue;
        }
        let Some(resolved) = resolve_confined_artifact(&path, &canonical_roots) else {
            continue;
        };
        if !sent_paths.insert(resolved.clone()) {
            continue;
        }

        let file_name = resolved.file_name().unwrap_or_default().to_string_lossy().to_string();
        if looks_sensitive(&file_name) || looks_sensitive(&resolved.to_string_lossy()) {
            warn!("chat {chat_id}: refusing to auto-deliver sensitive-looking {resolved:?}");
            let _ = bot
                .send_message(
                    target_chat,
                    format!(
                        "🔒 <b>已阻止自动发送疑似敏感文件</b>\n<code>{name}</code>\n需要它时请显式使用 <code>/file</code>。",
                        name = TelegramFormatter::escape_html(&file_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
            continue;
        }

        let Ok(meta) = tokio::fs::metadata(&resolved).await else {
            continue;
        };
        let extension = resolved
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        let channel = delivery_channel(&extension, meta.len());
        if channel == "oversize" {
            error!(
                "chat {chat_id}: {} is {} MB, above the 50 MB Bot API upload ceiling",
                resolved.display(),
                meta.len() / 1_048_576
            );
            let _ = bot
                .send_message(
                    target_chat,
                    format!(
                        "⚠️ <b>产物过大未发送</b>\n<code>{name}</code> 约 <code>{mb:.1} MB</code>，超过 Bot API 的 50 MB 上传上限。可在服务器直接取用该路径。",
                        name = TelegramFormatter::escape_html(&file_name),
                        mb = meta.len() as f64 / 1_048_576.0
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
            continue;
        }

        let input = InputFile::file(&resolved);
        let send_result = match channel {
            "animation" => {
                bot.send_animation(target_chat, input)
                    .caption(caption("🎬", "Generated Animation", &file_name))
                    .parse_mode(ParseMode::Html)
                    .await
                    .map(|_| ())
            }
            "video" => {
                bot.send_video(target_chat, input)
                    .caption(caption("🎞️", "Generated Video", &file_name))
                    .parse_mode(ParseMode::Html)
                    .await
                    .map(|_| ())
            }
            "photo" => {
                bot.send_photo(target_chat, input)
                    .caption(caption("🖼️", "Generated Image", &file_name))
                    .parse_mode(ParseMode::Html)
                    .await
                    .map(|_| ())
            }
            _ => {
                bot.send_document(target_chat, input)
                    .caption(caption("📄", "Generated Artifact", &file_name))
                    .parse_mode(ParseMode::Html)
                    .await
                    .map(|_| ())
            }
        };

        if let Err(err) = send_result {
            if !is_message_not_modified(&format!("{err}")) {
                error!("chat {chat_id}: {channel} send failed for {}: {err:?}", resolved.display());
            }
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
            // The security property: joined onto the scratch dir it stays one component deep.
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
        assert_eq!(safe_attachment_name(3, Some("   "), "voice.ogg"), "m3-voice.ogg");
        assert_eq!(safe_attachment_name(3, Some("..."), "video.mp4"), "m3-video.mp4");
    }

    #[test]
    fn long_names_are_clipped_without_splitting_glyphs() {
        let name = safe_attachment_name(9, Some(&"中".repeat(120)), "f.bin");
        assert!(name.chars().count() <= 9 + 60, "not clipped: {}", name.len());
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
        assert!(!over_download_limit(u32::MAX), "unknown size must not be rejected");
        assert!(!over_download_limit(1024));
        assert!(!over_download_limit(MAX_DOWNLOAD_BYTES as u32));
        assert!(over_download_limit(MAX_DOWNLOAD_BYTES as u32 + 1));
    }

    #[test]
    fn channel_choice_respects_telegram_limits() {
        assert_eq!(delivery_channel("png", 1024), "photo");
        assert_eq!(delivery_channel("jpg", MAX_PHOTO_BYTES), "photo");
        // Over the photo ceiling an image must still be deliverable as a document.
        assert_eq!(delivery_channel("png", MAX_PHOTO_BYTES + 1), "document");
        assert_eq!(delivery_channel("gif", 4096), "animation");
        // Sound-bearing containers need sendVideo, not sendAnimation.
        assert_eq!(delivery_channel("mp4", 4096), "video");
        assert_eq!(delivery_channel("mkv", 4096), "video");
        assert_eq!(delivery_channel("mov", MAX_UPLOAD_BYTES), "video");
        assert_eq!(delivery_channel("mp4", MAX_UPLOAD_BYTES + 1), "oversize");
        assert_eq!(delivery_channel("pdf", MAX_UPLOAD_BYTES), "document");
        assert_eq!(delivery_channel("zip", MAX_UPLOAD_BYTES + 1), "oversize");
        assert_eq!(delivery_channel("bin", MAX_DOWNLOAD_BYTES * 4), "oversize");
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
        for name in ["chart.png", "report.pdf", "data.csv", "main.rs", "index.html"] {
            assert!(!looks_sensitive(name), "{name} should be allowed");
        }
    }

    #[test]
    fn captions_stay_under_the_telegram_limit() {
        let long_name = format!("{}.png", "x".repeat(4000));
        let html = caption("🖼️", "Generated Image", &long_name);
        assert!(html.chars().count() <= MAX_CAPTION_CHARS + 64, "caption too long: {}", html.chars().count());
        assert!(html.contains("..."), "expected an ellipsis marker");
    }

    fn fixture(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp fixture should be creatable");
        base
    }

    #[test]
    fn files_outside_the_allowed_roots_are_refused() {
        let base = fixture("agygram_confine_plain");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let inside = workspace.join("chart.png");
        std::fs::write(&inside, b"png-bytes").unwrap();
        let secret = outside.join("credentials.json");
        std::fs::write(&secret, b"secret").unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&inside, &roots),
            Some(inside.canonicalize().unwrap())
        );
        assert_eq!(resolve_confined_artifact(&secret, &roots), None);
        assert_eq!(resolve_confined_artifact(Path::new("/etc/hostname"), &roots), None);
    }

    #[test]
    fn directories_are_never_delivered() {
        let base = fixture("agygram_confine_dir");
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("nested.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("nested.json"), &roots),
            None,
            "a directory must not be treated as an artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn traversal_and_symlink_escapes_are_refused() {
        let base = fixture("agygram_confine_escape");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let secret = outside.join("wallet.json");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, workspace.join("art.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("art.json"), &roots),
            None,
            "symlink escape must not deliver the target"
        );
        assert_eq!(
            resolve_confined_artifact(&workspace.join("../../outside/wallet.json"), &roots),
            None,
            "dot-dot traversal must not leave the workspace"
        );
    }
}
