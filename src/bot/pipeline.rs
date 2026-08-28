use crate::bot::formatter::TelegramFormatter;
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent};
use crate::session::SessionManager;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::sync::mpsc;
use tracing::{error, info};

async fn download_media_attachment(bot: &Bot, msg: &Message, scratch_dir: &Path) -> Option<String> {
    let (file_id, filename, label) = if let Some(doc) = msg.document() {
        let name = doc.file_name.clone().unwrap_or_else(|| "attached_file.bin".to_string());
        (&doc.file.id, name, "Document")
    } else if let Some(photos) = msg.photo() {
        let photo = photos.last()?;
        (&photo.file.id, format!("photo_{}.jpg", photo.file.id), "Image")
    } else {
        let voice = msg.voice()?;
        (&voice.file.id, format!("voice_{}.ogg", voice.file.id), "Voice")
    };

    let dest_path = scratch_dir.join(&filename);
    let file_info = bot.get_file(file_id).await.ok()?;
    let mut file_out = tokio::fs::File::create(&dest_path).await.ok()?;
    bot.download_file(&file_info.path, &mut file_out).await.ok()?;
    Some(format!("[Attached {} File: {}]\n", label, dest_path.display()))
}

async fn auto_deliver_detected_artifacts(
    bot: &Bot,
    target_chat: teloxide::types::ChatId,
    output_text: &str,
    _scratch_dir: &Path,
) {
    let mut sent_paths = std::collections::HashSet::new();

    for token in output_text.split_whitespace() {
        let clean_token = token
            .trim_matches(['(', ')', '[', ']', '"', '\'', '<', '>', '`'])
            .trim_start_matches("file://");
        let path = PathBuf::from(clean_token);
        if path.is_absolute() && path.exists() && path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(|s| s.to_lowercase()) {
                if ["png", "jpg", "jpeg", "webp", "gif"].contains(&ext.as_str())
                    && sent_paths.insert(path.clone())
                {
                    let caption = format!("🖼️ <b>Generated Image:</b> <code>{}</code>", TelegramFormatter::escape_html(&path.file_name().unwrap_or_default().to_string_lossy()));
                    let _ = bot
                        .send_photo(target_chat, teloxide::types::InputFile::file(&path))
                        .caption(caption)
                        .parse_mode(ParseMode::Html)
                        .await;
                } else if ["pdf", "zip", "tar", "gz", "json", "csv"].contains(&ext.as_str())
                    && sent_paths.insert(path.clone())
                {
                    let caption = format!("📄 <b>Generated Artifact:</b> <code>{}</code>", TelegramFormatter::escape_html(&path.file_name().unwrap_or_default().to_string_lossy()));
                    let _ = bot
                        .send_document(target_chat, teloxide::types::InputFile::file(&path))
                        .caption(caption)
                        .parse_mode(ParseMode::Html)
                        .await;
                }
            }
        }
    }
}

pub async fn message_handler(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);
    if !config.is_user_allowed(user_id) {
        bot.send_message(
            msg.chat.id,
            format!("🚫 权限不足：用户 ID {} 未在授权名单中。", user_id),
        )
        .await?;
        return Ok(());
    }

    let chat_id = msg.chat.id.0;
    let mut session = session_mgr.get_or_create(chat_id).await;

    let should_send_voice = session.voice_mode
        || msg.text().map(|t| t.starts_with("/voice") || t.contains("/voice")).unwrap_or(false)
        || msg.caption().map(|c| c.starts_with("/voice") || c.contains("/voice")).unwrap_or(false);

    // Check interrupt-on-new-message
    if session.is_running {
        if config.interrupt_on_new_message {
            info!("Interrupting running task for chat_id {} due to new message", chat_id);
            session_mgr.cancel_task(chat_id).await;
            let _ = bot
                .send_message(
                    msg.chat.id,
                    "<b>收到新消息，已打断上一次任务并切换为最新推导...</b>",
                )
                .parse_mode(ParseMode::Html)
                .await;
            session = session_mgr.get_or_create(chat_id).await;
        } else {
            bot.send_message(
                msg.chat.id,
                "⏳ <b>上一条任务正在运行中...</b> 发送 /cancel 强行中断任务。",
            )
            .parse_mode(ParseMode::Html)
            .await?;
            return Ok(());
        }
    }

    let valid_workspace = session.get_valid_workspace();
    let scratch_dir = PathBuf::from(&valid_workspace).join("scratch");
    let _ = tokio::fs::create_dir_all(&scratch_dir).await;

    let mut prompt_content = String::new();

    // Check and download media / document attachments
    if let Some(attachment_str) = download_media_attachment(&bot, &msg, &scratch_dir).await {
        prompt_content.push_str(&attachment_str);
    }

    if let Some(text) = msg.text() {
        let clean = if let Some(rest) = text.strip_prefix("/voice") {
            let without_bot = rest.trim_start_matches(|c: char| !c.is_whitespace());
            without_bot.trim()
        } else {
            text
        };
        prompt_content.push_str(clean);
    } else if let Some(caption) = msg.caption() {
        let clean = if let Some(rest) = caption.strip_prefix("/voice") {
            let without_bot = rest.trim_start_matches(|c: char| !c.is_whitespace());
            without_bot.trim()
        } else {
            caption
        };
        prompt_content.push_str(clean);
    }

    let input_prompt = prompt_content.trim().to_string();
    if input_prompt.is_empty() {
        return Ok(());
    }

    session_mgr.set_running(chat_id, true, None).await;

    let _ = bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing).await;

    let initial_status_msg = bot
        .send_message(msg.chat.id, "<i>思考中...</i>")
        .parse_mode(ParseMode::Html)
        .await?;

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(200);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    let params = ExecutionParams {
        prompt: &input_prompt,
        conversation_id: session.conversation_id.as_deref(),
        is_first_turn: session.is_first_turn,
        workspace: &valid_workspace,
        extra_dirs: &session.extra_dirs,
        model: &session.model,
        effort: &session.effort,
        mode: &session.mode,
        agent: &session.agent,
        system_prompt: &session.system_prompt,
    };

    let session_mgr_pid = session_mgr.clone();
    tokio::spawn(async move {
        if let Ok(pid) = pid_rx.await {
            session_mgr_pid.set_pid(chat_id, pid).await;
        }
    });

    let timeout_secs = config.executor_timeout_secs;
    let exec_future = AgyExecutor::run_prompt_streaming(params, stream_tx, Some(pid_tx), timeout_secs);

    let draft_interval_ms = config.draft_update_interval_ms;
    let bot_clone = bot.clone();
    let status_msg_id = initial_status_msg.id;
    let target_chat_id = msg.chat.id;

    let stream_task = tokio::spawn(async move {
        let mut accumulated_text = String::new();
        let mut last_tool_call = String::new();
        let mut last_tool_summary = String::new();
        let mut last_thinking = String::new();
        let mut last_rendered_html = String::new();
        let mut last_edit_time = Instant::now();
        let mut last_typing_time = Instant::now();
        let min_interval = Duration::from_millis(draft_interval_ms);

        while let Some(event) = stream_rx.recv().await {
            let mut updated = false;

            match event {
                StreamEvent::Init { .. } => {}
                StreamEvent::TextDelta(delta) => {
                    accumulated_text.push_str(&delta);
                    updated = true;
                }
                StreamEvent::ThinkingDelta(thinking) => {
                    last_thinking.push_str(&thinking);
                    updated = true;
                }
                StreamEvent::ToolUse { tool, summary } => {
                    last_tool_call = tool;
                    last_tool_summary = summary;
                    updated = true;
                }
                StreamEvent::Result { .. } => {}
                StreamEvent::Error(err) => {
                    accumulated_text.push_str(&format!("\nError: {}", err));
                    updated = true;
                }
            }

            if last_typing_time.elapsed() >= Duration::from_secs(4) {
                let _ = bot_clone
                    .send_chat_action(target_chat_id, teloxide::types::ChatAction::Typing)
                    .await;
                last_typing_time = Instant::now();
            }

            if updated && last_edit_time.elapsed() >= min_interval {
                let mut draft_html = String::new();
                if !last_tool_call.is_empty() {
                    draft_html.push_str(&TelegramFormatter::format_tool_call(&last_tool_call, &last_tool_summary));
                }
                if !last_thinking.is_empty() && accumulated_text.is_empty() {
                    draft_html.push_str(&TelegramFormatter::format_thinking(&last_thinking));
                }

                let formatted_text = TelegramFormatter::to_telegram_html(&accumulated_text);
                if formatted_text.is_empty() {
                    if draft_html.is_empty() {
                        draft_html.push_str("<i>思考中...</i>");
                    }
                } else {
                    draft_html.push_str(&formatted_text);
                }

                let balanced_draft = TelegramFormatter::balance_streaming_html(&draft_html);
                let chunks = TelegramFormatter::split_html_chunks(&balanced_draft, 4000);
                if let Some(first_chunk) = chunks.first() {
                    if first_chunk != &last_rendered_html {
                        let edit_res = bot_clone
                            .edit_message_text(target_chat_id, status_msg_id, first_chunk)
                            .parse_mode(ParseMode::Html)
                            .await;
                        if edit_res.is_ok() {
                            last_rendered_html = first_chunk.clone();
                        }
                    }
                }

                last_edit_time = Instant::now();
            }
        }
    });

    let exec_result = exec_future.await;
    let _ = stream_task.await;

    session_mgr.set_running(chat_id, false, None).await;

    match exec_result {
        Ok(res) => {
            let output_text = res.output;
            let usage_ref = res.usage.as_ref();
            let dur_val = res.duration_seconds;

            session_mgr
                .mark_turn_completed(
                    chat_id,
                    res.conversation_id,
                    input_prompt.len(),
                    output_text.len(),
                    usage_ref,
                    dur_val,
                )
                .await;

            // Auto-detect and deliver generated image / artifact files
            auto_deliver_detected_artifacts(&bot, msg.chat.id, &output_text, &scratch_dir).await;

            // Spawn TTS voice task asynchronously in parallel
            if should_send_voice {
                if let Some(ref api_key) = config.fish_api_key {
                    let bot_voice = bot.clone();
                    let target_chat = msg.chat.id;
                    let api_key_cloned = api_key.clone();
                    let ref_id = config.fish_reference_id.clone();
                    let text_for_tts = output_text.clone();

                    tokio::spawn(async move {
                        info!("Generating Shenhe TTS voice reply for chat_id {}", target_chat.0);
                        let _ = bot_voice
                            .send_chat_action(target_chat, teloxide::types::ChatAction::RecordVoice)
                            .await;
                        match crate::tts::generate_speech(&text_for_tts, &api_key_cloned, ref_id.as_deref()).await {
                            Ok(audio_bytes) => {
                                info!("Successfully generated {} bytes Shenhe voice audio, sending to Telegram...", audio_bytes.len());
                                let voice_file = teloxide::types::InputFile::memory(audio_bytes)
                                    .file_name("shenhe_voice.mp3");
                                let res = bot_voice
                                    .send_voice(target_chat, voice_file)
                                    .await;
                                if let Err(e) = res {
                                    error!("Failed to send voice message to Telegram: {:?}", e);
                                }
                            }
                            Err(err) => {
                                error!("TTS generation error for chat {}: {:?}", target_chat.0, err);
                            }
                        }
                    });
                }
            }

            let mut formatted_html = TelegramFormatter::to_telegram_html(&output_text);
            let badge = TelegramFormatter::format_telemetry_badge(usage_ref, dur_val);
            if !badge.is_empty() {
                formatted_html.push_str(&badge);
            }
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, 4000);

            let edit_res = if let Some(first_chunk) = chunks.first() {
                bot.edit_message_text(msg.chat.id, status_msg_id, first_chunk)
                    .parse_mode(ParseMode::Html)
                    .await
            } else {
                Err(teloxide::RequestError::Api(teloxide::ApiError::Unknown("Empty output".to_string())))
            };

            if edit_res.is_err() {
                let raw_chunks = TelegramFormatter::split_html_chunks(&output_text, 4000);
                if let Some(first_raw) = raw_chunks.first() {
                    let _ = bot.edit_message_text(msg.chat.id, status_msg_id, first_raw).await;
                    for chunk in raw_chunks.iter().skip(1) {
                        let _ = bot.send_message(msg.chat.id, chunk).await;
                    }
                }
            } else {
                for chunk in chunks.iter().skip(1) {
                    let send_res = bot
                        .send_message(msg.chat.id, chunk)
                        .parse_mode(ParseMode::Html)
                        .await;
                    if send_res.is_err() {
                        let _ = bot.send_message(msg.chat.id, chunk).await;
                    }
                }
            }
        }
        Err(err) => {
            error!("agy execution error for chat {}: {:?}", chat_id, err);
            let error_text = format!(
                "❌ <b>执行失败:</b>\n<pre><code>{}</code></pre>",
                TelegramFormatter::escape_html(&err.to_string())
            );
            let _ = bot
                .edit_message_text(msg.chat.id, status_msg_id, error_text)
                .parse_mode(ParseMode::Html)
                .await;
        }
    }

    Ok(())
}
