use crate::bot::formatter::TelegramFormatter;
use crate::bot::media;
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent};
use crate::session::{SessionManager, TurnGuard};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Telegram answers 400 "message is not modified" when an edit posts identical text. That is a
/// no-op, not a rendering failure.
pub(crate) fn is_message_not_modified(description: &str) -> bool {
    description.to_ascii_lowercase().contains("not modified")
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
    if let Some(attachment_str) = media::download_media_attachment(&bot, &msg, &scratch_dir).await {
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

    // The busy check earlier only drives the interrupt UX. Claiming the turn here is what makes
    // the guard authoritative: the flag flip and the check share one write lock, so a second
    // message racing this one can no longer start a parallel `agy` process on the same session.
    let Some(generation) = session_mgr.begin_turn(chat_id).await else {
        bot.send_message(
            msg.chat.id,
            "⏳ <b>上一条任务正在运行中...</b> 发送 /cancel 强行中断任务。",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    };
    // Guarantees the lock is released even if a Telegram call below returns early with `?`.
    let _turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);

    let _ = bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing).await;

    let initial_status_msg = bot
        .send_message(msg.chat.id, "<i>思考中...</i>")
        .parse_mode(ParseMode::Html)
        .await?;

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(1000);
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
        sandbox: session.sandbox,
        project: &session.project,
    };

    let session_mgr_pid = session_mgr.clone();
    tokio::spawn(async move {
        if let Ok(pid) = pid_rx.await {
            // Generation-gated: a pid arriving late from an interrupted turn must not overwrite
            // the process the replacement turn is actually running.
            session_mgr_pid.set_pid(chat_id, pid, generation).await;
        }
    });

    let idle_timeout_secs = config.idle_timeout_secs;
    let max_execution_secs = config.max_execution_secs;
    let exec_future = AgyExecutor::run_prompt_streaming(
        params,
        stream_tx,
        Some(pid_tx),
        idle_timeout_secs,
        max_execution_secs,
    );

    let draft_interval_ms = config.draft_update_interval_ms;
    let bot_clone = bot.clone();
    let status_msg_id = initial_status_msg.id;
    let target_chat_id = msg.chat.id;

    struct StreamingState {
        accumulated_text: String,
        last_tool_call: String,
        last_tool_summary: String,
        last_thinking: String,
        error_msg: Option<String>,
        is_finished: bool,
        is_dirty: bool,
    }

    let shared_state = Arc::new(tokio::sync::Mutex::new(StreamingState {
        accumulated_text: String::with_capacity(2048),
        last_tool_call: String::new(),
        last_tool_summary: String::new(),
        last_thinking: String::new(),
        error_msg: None,
        is_finished: false,
        is_dirty: false,
    }));

    let state_ingest = shared_state.clone();
    let ingest_task = tokio::spawn(async move {
        while let Some(event) = stream_rx.recv().await {
            let mut s = state_ingest.lock().await;
            s.is_dirty = true;
            match event {
                StreamEvent::Init { .. } => {}
                StreamEvent::TextDelta(delta) => {
                    s.accumulated_text.push_str(&delta);
                }
                StreamEvent::ThinkingDelta(thinking) => {
                    s.last_thinking.push_str(&thinking);
                }
                StreamEvent::ToolUse { tool, summary } => {
                    s.last_tool_call = tool;
                    s.last_tool_summary = summary;
                }
                StreamEvent::TurnComplete { text } => {
                    if !text.is_empty() {
                        s.accumulated_text = text;
                    }
                    s.is_finished = true;
                }
                StreamEvent::Result { response, .. } => {
                    if !response.is_empty() {
                        s.accumulated_text = response;
                    }
                    s.is_finished = true;
                }
                StreamEvent::Error(err) => {
                    s.error_msg = Some(err);
                }
            }
        }
        let mut s = state_ingest.lock().await;
        s.is_finished = true;
        s.is_dirty = true;
    });

    let state_render = shared_state.clone();
    let render_task = tokio::spawn(async move {
        let mut last_rendered_html = String::new();
        let mut last_typing_time = Instant::now();
        let base_interval = Duration::from_millis(draft_interval_ms.max(500));
        let mut interval_duration = base_interval;
        let mut ticker = tokio::time::interval(interval_duration);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut active_msg_id = status_msg_id;
        let mut sealed_chunks_count = 0usize;

        loop {
            ticker.tick().await;

            let (text, tool, summary, thinking, is_done, err_opt, dirty) = {
                let mut s = state_render.lock().await;
                let dirty = s.is_dirty;
                s.is_dirty = false;
                (
                    s.accumulated_text.clone(),
                    s.last_tool_call.clone(),
                    s.last_tool_summary.clone(),
                    s.last_thinking.clone(),
                    s.is_finished,
                    s.error_msg.clone(),
                    dirty,
                )
            };

            if last_typing_time.elapsed() >= Duration::from_secs(4) && !is_done {
                let action = if tool == "generate_image" {
                    teloxide::types::ChatAction::UploadPhoto
                } else if tool.contains("file") || tool.contains("resource") {
                    teloxide::types::ChatAction::UploadDocument
                } else {
                    teloxide::types::ChatAction::Typing
                };
                let _ = bot_clone
                    .send_chat_action(target_chat_id, action)
                    .await;
                last_typing_time = Instant::now();
            }

            if !dirty && !is_done && !last_rendered_html.is_empty() {
                continue;
            }

            let mut draft_html = String::new();
            if !tool.is_empty() {
                draft_html.push_str(&TelegramFormatter::format_tool_call(&tool, &summary));
            }
            if !thinking.is_empty() {
                draft_html.push_str(&TelegramFormatter::format_thinking(&thinking));
            }
            if let Some(ref err) = err_opt {
                draft_html.push_str(&format!("\n❌ <b>Error:</b> <code>{}</code>\n", TelegramFormatter::escape_html(err)));
            }

            let formatted_text = TelegramFormatter::to_telegram_html(&text);
            if formatted_text.is_empty() {
                if draft_html.is_empty() {
                    draft_html.push_str("<i>思考中...</i>");
                }
            } else {
                draft_html.push_str(&formatted_text);
            }

            let balanced_draft = TelegramFormatter::balance_streaming_html(&draft_html);
            let chunks = TelegramFormatter::split_html_chunks(&balanced_draft, 4000);

            // Multi-message chunk rollover: seal previous chunk when content exceeds 4000 chars
            while chunks.len() > sealed_chunks_count + 1 {
                let completed_chunk = &chunks[sealed_chunks_count];
                let _ = bot_clone
                    .edit_message_text(target_chat_id, active_msg_id, completed_chunk)
                    .parse_mode(ParseMode::Html)
                    .await;

                match bot_clone
                    .send_message(target_chat_id, "<i>(续)...</i>")
                    .parse_mode(ParseMode::Html)
                    .await
                {
                    Ok(new_msg) => {
                        active_msg_id = new_msg.id;
                        sealed_chunks_count += 1;
                        last_rendered_html.clear();
                    }
                    Err(err) => {
                        tracing::warn!("chat {target_chat_id}: failed to send rollover message: {err:?}");
                        break;
                    }
                }
            }

            if let Some(active_chunk) = chunks.get(sealed_chunks_count) {
                if active_chunk != &last_rendered_html {
                    let edit_res = bot_clone
                        .edit_message_text(target_chat_id, active_msg_id, active_chunk)
                        .parse_mode(ParseMode::Html)
                        .await;
                    match edit_res {
                        Ok(_) => {
                            last_rendered_html = active_chunk.clone();
                            if interval_duration != base_interval {
                                interval_duration = base_interval;
                                ticker = tokio::time::interval(interval_duration);
                                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            }
                        }
                        Err(teloxide::RequestError::RetryAfter(retry_secs)) => {
                            interval_duration = Duration::from_secs(retry_secs.seconds() as u64 + 1)
                                .max(base_interval);
                            ticker = tokio::time::interval(interval_duration);
                            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            state_render.lock().await.is_dirty = true;
                        }
                        Err(err) => {
                            if is_message_not_modified(&format!("{err}")) {
                                last_rendered_html = active_chunk.clone();
                                state_render.lock().await.is_dirty = false;
                            } else {
                                tracing::warn!("chat {target_chat_id}: draft edit rejected: {err:?}");
                                state_render.lock().await.is_dirty = false;
                            }
                        }
                    }
                }
            }

            if is_done {
                break;
            }
        }

        (active_msg_id, sealed_chunks_count)
    });

    let exec_result = exec_future.await;
    let _ = ingest_task.await;
    let (final_active_msg_id, sealed_chunks_count) = render_task.await.unwrap_or((status_msg_id, 0));

    match exec_result {
        Ok(res) => {
            let output_text = res.output;
            let usage_ref = res.usage.as_ref();
            let dur_val = res.duration_seconds;

            let owns_turn = session_mgr
                .finish_turn(
                    chat_id,
                    generation,
                    crate::session::TurnOutcome {
                        conversation_id: res.conversation_id,
                        input_chars: input_prompt.len(),
                        output_chars: output_text.len(),
                        usage: usage_ref,
                        duration_seconds: dur_val,
                    },
                )
                .await;
            if !owns_turn {
                // This turn was interrupted and has already been replaced: posting its leftovers
                // would clobber the new turn's message and its recorded conversation id.
                info!("chat {chat_id}: turn {generation} was superseded, discarding its output");
                return Ok(());
            }

            // Auto-detect and deliver generated image / artifact files, confined to the
            // directories agy was actually given write access to.
            let mut artifact_roots: Vec<PathBuf> =
                vec![PathBuf::from(&valid_workspace), scratch_dir.clone()];
            artifact_roots.extend(session.extra_dirs.iter().map(PathBuf::from));
            media::auto_deliver_detected_artifacts(&bot, msg.chat.id, &output_text, &artifact_roots).await;

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
                                    .file_name("shenhe_voice.ogg");
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

            let formatted_html = TelegramFormatter::to_telegram_html(&output_text);
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, 4000);

            // The draft already carries the streamed render. When the finished answer equals the
            // last draft, Telegram rejects the edit with 400 "message is not modified" - that is
            // a no-op, not a formatting failure, and must not trigger the plain-text fallback
            // that used to rewrite a perfectly rendered message into raw markdown.
            let mut html_rejected = false;
            if let Some(target_chunk) = chunks.get(sealed_chunks_count) {
                match bot
                    .edit_message_text(msg.chat.id, final_active_msg_id, target_chunk)
                    .parse_mode(ParseMode::Html)
                    .await
                {
                    Ok(_) => {}
                    Err(err) if is_message_not_modified(&format!("{err}")) => {}
                    Err(err) => {
                        error!("chat {chat_id}: final HTML edit rejected: {err:?}");
                        html_rejected = true;
                    }
                }
            } else if chunks.is_empty() {
                info!("chat {chat_id}: rendered output is empty, keeping the streamed draft");
                return Ok(());
            }

            if html_rejected {
                // Deliver as a fresh message instead of downgrading the draft; only if Telegram
                // rejects the HTML outright do we fall back to plain text.
                if let Some(target_chunk) = chunks.get(sealed_chunks_count) {
                    if bot
                        .send_message(msg.chat.id, target_chunk)
                        .parse_mode(ParseMode::Html)
                        .await
                        .is_err()
                    {
                        let raw_chunks = TelegramFormatter::split_html_chunks(&output_text, 4000);
                        if let Some(first_raw) = raw_chunks.get(sealed_chunks_count) {
                            let _ = bot
                                .edit_message_text(msg.chat.id, final_active_msg_id, first_raw)
                                .await;
                        }
                    }
                }
            }

            // Deliver any chunks that accumulated beyond what was streamed during rollover
            for chunk in chunks.iter().skip(sealed_chunks_count + 1) {
                let send_res = bot
                    .send_message(msg.chat.id, chunk)
                    .parse_mode(ParseMode::Html)
                    .await;
                if send_res.is_err() {
                    let _ = bot
                        .send_message(msg.chat.id, chunk)
                        .await;
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
                .edit_message_text(msg.chat.id, final_active_msg_id, error_text)
                .parse_mode(ParseMode::Html)
                .await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_message_not_modified;

    #[test]
    fn telegram_not_modified_is_recognised_as_no_op() {
        assert!(is_message_not_modified(
            "TelegramApiError: message is not modified"
        ));
        assert!(is_message_not_modified("Message Is Not Modified"));
        assert!(!is_message_not_modified(
            "TelegramApiError: can't parse entities: unsupported start tag \"\" at byte offset 12"
        ));
        assert!(!is_message_not_modified("Network(connection reset by peer)"));
    }
}
