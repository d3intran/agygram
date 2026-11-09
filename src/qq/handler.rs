use super::client::QqClient;
use super::types::{C2CMessageEvent, GroupMessageEvent, MessageAttachment};
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent, TokenUsage};
use crate::session::{
    qq_group_openid_to_chat_id, qq_openid_to_chat_id, SessionManager, TurnGuard, TurnOutcome,
};
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub async fn handle_c2c(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    event: C2CMessageEvent,
) {
    let openid = event.author.user_openid.trim();
    if !config.is_qq_user_allowed(openid) {
        warn!("Ignoring unauthorized C2C message from openid: {}", openid);
        return;
    }

    let chat_id = qq_openid_to_chat_id(openid);
    if let Err(e) = process_message(
        client,
        config,
        session_mgr,
        openid,
        true,
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
    )
    .await
    {
        error!("Error processing QQ C2C message: {:#}", e);
    }
}

pub async fn handle_group(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    event: GroupMessageEvent,
) {
    let sender_openid = event.author.member_openid.trim();
    let group_openid = event.group_openid.trim();

    if !config.is_qq_group_allowed(group_openid, sender_openid) {
        warn!(
            "Ignoring unauthorized Group message from member: {} in group: {}",
            sender_openid, group_openid
        );
        return;
    }

    let chat_id = if config.qq_group_session_mode.trim().eq_ignore_ascii_case("member") {
        crate::session::qq_group_member_to_chat_id(group_openid, sender_openid)
    } else {
        qq_group_openid_to_chat_id(group_openid)
    };

    if let Err(e) = process_message(
        client,
        config,
        session_mgr,
        group_openid,
        false,
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
    )
    .await
    {
        error!("Error processing QQ Group message: {:#}", e);
    }
}

async fn reply(
    client: &QqClient,
    target_id: &str,
    is_c2c: bool,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    let formatted = crate::qq::formatter::QqFormatter::format_markdown(text);
    // QQ messages are limited to ~1500-2000 chars per message.
    let chunks = chunk_text(&formatted, 1400);
    let mut last_id = None;
    for chunk in chunks {
        let sent = if is_c2c {
            client.send_c2c_message(target_id, &chunk, msg_id).await?
        } else {
            client.send_group_message(target_id, &chunk, msg_id).await?
        };
        if sent.is_some() {
            last_id = sent;
        }
    }
    Ok(last_id)
}

/// QQ is a chat surface, not a document reader: default to short answers. The model also tends
/// to re-issue identical searches seconds apart, so that is called out explicitly.
const QQ_DEFAULT_SYSTEM_PROMPT: &str = "你正在通过 QQ 聊天窗口与用户对话。默认保持简洁：先给结论，再按需补充关键要点，尽量控制在 300 字以内；不要复述用户的问题，不要输出过程旁白和客套话；工具结果已足够时立即作答，不要重复相同的搜索或命令。只有用户明确要求详细展开时才输出长回答。";

/// Voice turns are read by Fish Audio, which interprets inline `[bracket]` cues for delivery
/// (https://docs.fish.audio/developer-guide/core-features/emotions). The cue stays in the text we
/// hand to TTS and is stripped if the turn falls back to a plain text reply.
const QQ_VOICE_STYLE_PROMPT: &str = "本轮将以语音回复。请在回答文本中自然加入 Fish Audio 情感标签：每个句子开头最多一个，例如 [calm]、[happy]、[curious]、[empathetic]，需要强调时用 [emphasis]，需要停顿时用 [break]；最多叠加两个标签，不要解释或提及这些标签本身。";

fn effective_system_prompt(custom: &str, voice: bool) -> String {
    let custom = custom.trim();
    let mut prompt = String::from(QQ_DEFAULT_SYSTEM_PROMPT);
    if voice {
        prompt.push('\n');
        prompt.push_str(QQ_VOICE_STYLE_PROMPT);
    }
    if !custom.is_empty() {
        prompt.push_str("\n用户自定义指令（优先于以上风格要求）：");
        prompt.push_str(custom);
    }
    prompt
}

/// Drops Fish Audio cues so a failed voice turn can fall back to readable text. Only short
/// lowercase bracket segments are treated as cues, so markdown links and code survive.
fn strip_emotion_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push('[');
            out.push_str(after);
            return out.trim().to_string();
        };
        let tag = &after[..close];
        let is_tag = !tag.is_empty()
            && tag.len() <= 32
            && tag
                .chars()
                .all(|c| c.is_ascii_lowercase() || matches!(c, ' ' | '-' | '\''));
        if !is_tag {
            out.push('[');
            out.push_str(&after[..close + 1]);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[allow(clippy::too_many_arguments)] // Fields mirror the C2C/Group event pair plus resolved routing.
async fn process_message(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    target_id: &str,
    is_c2c: bool,
    chat_id: i64,
    raw_content: &str,
    msg_id: &str,
    attachments: &[MessageAttachment],
) -> Result<()> {
    let content = raw_content.trim();
    // An image sent without text is a valid prompt; only drop messages with neither.
    if content.is_empty() && attachments.is_empty() {
        return Ok(());
    }

    info!("Received QQ message from {}: {}", target_id, content);

    let mut voice_requested = false;
    let mut prompt_text = content;

    // Command Dispatcher
    if content.starts_with('/') {
        let mut parts = content.split_whitespace();
        let cmd = parts.next().unwrap_or_default().to_lowercase();
        // Split on the original text: `cmd` is lowercased, so `/MODEL opus` would otherwise
        // lose its argument to a failed prefix match.
        let arg = content
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim());

        match cmd.as_str() {
            "/start" | "/help" => {
                let help_text = "\
🚀 agygram QQ 机器人已就绪

📌 指令速查：
• /status - 查看当前会话状态、工作目录与模型
• /new 或 /reset - 重置会话上下文与历史
• /usage 或 /quota - 查询 Google Cloud 实时配额
• /model [<名称/别名>] - 切换模型 (如 3.8, sonnet, pro)
• /models - 查看系统支持的模型列表
• /effort [<级别>] - 设置思考力度 (low / medium / high / max)
• /mode [<模式>] - 设置执行模式 (accept-edits / plan)
• /voice [on|off|toggle] - 语音回复开关；开启后只发语音条，/voice <问题> 可单次语音作答 (Fish Audio TTS)
• /cd <路径> - 切换默认工作目录
• /dirs 或 /pwd - 查看当前绑定的工作目录
• /cancel - 中断当前正在执行的任务

💬 提问方式：
直接发送任何代码开发需求或问题，即可唤起 Antigravity CLI 自动执行！";
                reply(&client, target_id, is_c2c, msg_id, help_text).await?;
                return Ok(());
            }

            "/status" => {
                let session = session_mgr.get_or_create(chat_id).await;
                let conv_id = session.conversation_id.as_deref().unwrap_or("新建会话");
                let model = if session.model.is_empty() {
                    "系统默认 (Gemini 3.8 Flash)"
                } else {
                    &session.model
                };
                let status_text = format!(
                    "📊 当前会话状态：\n\n\
                    • 会话 ID: {}\n\
                    • 工作目录: {}\n\
                    • 当前模型: {}\n\
                    • 思考力度: {}\n\
                    • 执行模式: {}\n\
                    • 对话轮数: {}\n\
                    • 累计消耗: {} in / {} out tokens",
                    conv_id,
                    session.workspace,
                    model,
                    session.effort,
                    session.mode,
                    session.turn_count,
                    session.total_input_tokens,
                    session.total_output_tokens
                );
                reply(&client, target_id, is_c2c, msg_id, &status_text).await?;
                return Ok(());
            }

            "/new" | "/reset" | "/clear" => {
                session_mgr.reset_conversation(chat_id).await;
                reply(
                    &client,
                    target_id,
                    is_c2c,
                    msg_id,
                    "🔄 会话上下文已重置，已开启全新对话轮次。",
                )
                .await?;
                return Ok(());
            }

            "/usage" | "/quota" => {
                let _ = reply(&client, target_id, is_c2c, msg_id, "⏳ 正在查询实时额度...").await;
                match crate::quota::QuotaFetcher::format_models_and_quota().await {
                    Ok(html_panel) => {
                        let plain_text = strip_html_tags(&html_panel);
                        reply(&client, target_id, is_c2c, msg_id, &plain_text).await?;
                    }
                    Err(e) => {
                        let err_msg = format!("⚠️ 实时额度获取失败: {:#}", e);
                        reply(&client, target_id, is_c2c, msg_id, &err_msg).await?;
                    }
                }
                return Ok(());
            }

            "/models" => {
                let models = AgyExecutor::list_models().await.unwrap_or_default();
                let text = format!("📋 Antigravity CLI 支持的模型列表：\n\n{}", models);
                reply(&client, target_id, is_c2c, msg_id, &text).await?;
                return Ok(());
            }

            "/model" => {
                if arg.is_empty() {
                    let session = session_mgr.get_or_create(chat_id).await;
                    let curr = if session.model.is_empty() {
                        "系统默认 (Gemini 3.8 Flash)"
                    } else {
                        &session.model
                    };
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("🤖 当前 AI 模型: {}\n可使用 /model <名称> 切换。", curr),
                    )
                    .await?;
                } else {
                    use crate::bot::callbacks::{
                        is_safe_cli_value, is_valid_model, normalize_model_name,
                    };
                    let target = normalize_model_name(arg);
                    let is_valid = is_valid_model(&target)
                        || (is_safe_cli_value(&target)
                            && AgyExecutor::list_models()
                                .await
                                .map(|models| {
                                    models
                                        .lines()
                                        .filter_map(|line| line.split_whitespace().next())
                                        .any(|m| m.eq_ignore_ascii_case(&target))
                                })
                                .unwrap_or(false));
                    if !is_valid {
                        reply(
                            &client,
                            target_id,
                            is_c2c,
                            msg_id,
                            &format!("⚠️ 无效的模型名称: {}\n发送 /models 查看当前可用列表。", arg),
                        )
                        .await?;
                    } else {
                        session_mgr.set_model(chat_id, target.to_string()).await;
                        // Keep effort in sync with the model suffix, same as the Telegram path.
                        if target.ends_with("-high") {
                            session_mgr.set_effort(chat_id, "high".to_string()).await;
                        } else if target.ends_with("-medium") || target.ends_with("-med") {
                            session_mgr.set_effort(chat_id, "medium".to_string()).await;
                        } else if target.ends_with("-low") {
                            session_mgr.set_effort(chat_id, "low".to_string()).await;
                        }
                        reply(
                            &client,
                            target_id,
                            is_c2c,
                            msg_id,
                            &format!("✅ AI 模型已切换为: {}", target),
                        )
                        .await?;
                    }
                }
                return Ok(());
            }

            "/effort" => {
                if arg.is_empty() {
                    let session = session_mgr.get_or_create(chat_id).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("⚡ 当前思考力度: {}\n可选: low, medium, high, max", session.effort),
                    )
                    .await?;
                } else if !crate::bot::callbacks::is_supported_effort(arg) {
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        "⚠️ 无效的思考力度。可选: low, medium, high, xhigh, max",
                    )
                    .await?;
                } else {
                    session_mgr.set_effort(chat_id, arg.to_string()).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("✅ 思考力度已设置为: {}", arg),
                    )
                    .await?;
                }
                return Ok(());
            }

            "/mode" => {
                if arg.is_empty() {
                    let session = session_mgr.get_or_create(chat_id).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("🛠️ 当前执行模式: {}\n可选: accept-edits, plan", session.mode),
                    )
                    .await?;
                } else if !crate::bot::callbacks::is_supported_mode(arg) {
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        "⚠️ 无效的执行模式。可选: auto, accept-edits, plan",
                    )
                    .await?;
                } else {
                    session_mgr.set_mode(chat_id, arg.to_string()).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("✅ 执行模式已设置为: {}", arg),
                    )
                    .await?;
                }
                return Ok(());
            }

            "/cd" => {
                if arg.is_empty() {
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        "⚠️ 请提供目标目录路径，例如 /cd ~/project",
                    )
                    .await?;
                } else {
                    let session = session_mgr.get_or_create(chat_id).await;
                    let target_path =
                        crate::session::resolve_user_path(arg, &session.workspace);
                    let path_str = target_path.to_string_lossy().to_string();
                    session_mgr.set_workspace(chat_id, path_str.clone()).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("📂 工作主目录已切换至:\n{}", path_str),
                    )
                    .await?;
                }
                return Ok(());
            }

            "/pwd" | "/dirs" => {
                let session = session_mgr.get_or_create(chat_id).await;
                reply(
                    &client,
                    target_id,
                    is_c2c,
                    msg_id,
                    &format!("📂 当前工作目录:\n{}", session.workspace),
                )
                .await?;
                return Ok(());
            }

            "/cancel" => {
                let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
                if was_running {
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!(
                            "🛑 任务已强行中断 (PID: {})，工作区锁已释放。",
                            pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
                        ),
                    )
                    .await?;
                } else {
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        "ℹ️ 当前没有正在执行的任务。",
                    )
                    .await?;
                }
                return Ok(());
            }

            "/voice" => {
                if arg.eq_ignore_ascii_case("on") || arg == "开启" {
                    session_mgr.set_voice_mode(chat_id, true).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        "🔊 语音回复已开启：后续每条回答会附带一条语音。",
                    )
                    .await?;
                    return Ok(());
                } else if arg.eq_ignore_ascii_case("off") || arg == "关闭" {
                    session_mgr.set_voice_mode(chat_id, false).await;
                    reply(&client, target_id, is_c2c, msg_id, "🔇 语音回复已关闭。").await?;
                    return Ok(());
                } else if arg.eq_ignore_ascii_case("toggle") || arg == "切换" {
                    let enabled = session_mgr.toggle_voice_mode(chat_id).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("🔊 语音回复: {}", if enabled { "已开启" } else { "已关闭" }),
                    )
                    .await?;
                    return Ok(());
                } else if arg.is_empty() {
                    let session = session_mgr.get_or_create(chat_id).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!(
                            "🔊 语音回复 (Fish Audio TTS)\n当前: {}\n用法: /voice on|off|toggle；或 /voice <问题> 单次语音",
                            if session.voice_mode { "已开启" } else { "已关闭" }
                        ),
                    )
                    .await?;
                    return Ok(());
                } else {
                    // `/voice <问题>`: answer this one turn with a voice reply.
                    voice_requested = true;
                    prompt_text = arg;
                }
            }

            _ => {
                // Unknown command; fall through to prompt execution if not purely an unrecognized slash command
            }
        }
    }

    // Natural Language Prompt Turn Execution
    let is_busy = session_mgr.is_running(chat_id).await;
    let placeholder_id = if is_busy {
        reply(
            &client,
            target_id,
            is_c2c,
            msg_id,
            "⏳ 当前会话已有任务正在运行，已加入等待队列，即将自动处理...",
        )
        .await
        .ok()
        .flatten()
    } else {
        reply(&client, target_id, is_c2c, msg_id, "我想想...")
            .await
            .ok()
            .flatten()
    };

    let Some(generation) = session_mgr
        .begin_turn_with_timeout(chat_id, std::time::Duration::from_secs(120))
        .await
    else {
        reply(
            &client,
            target_id,
            is_c2c,
            msg_id,
            "⚠️ 排队超时：上一任务执行时间过长，请稍后重试或发送 /cancel 强行中断任务。",
        )
        .await?;
        return Ok(());
    };

    let _turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);
    let session = session_mgr.get_or_create(chat_id).await;
    let should_send_voice = session.voice_mode || voice_requested;

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    // Spawn PID recorder
    let session_mgr_pid = session_mgr.clone();
    tokio::spawn(async move {
        if let Ok(pid) = pid_rx.await {
            session_mgr_pid.set_pid(chat_id, pid, generation).await;
        }
    });

    // Attachments arrive as short-lived signed CDN URLs. Save them next to the workspace scratch
    // dir and hand agy absolute paths, the same contract Telegram ingress uses: agy reads images
    // from disk, so this part is platform-agnostic (unlike QQ's outbound rich-media upload flow).
    let scratch_dir = std::path::PathBuf::from(session.get_valid_workspace()).join("scratch");
    let _ = tokio::fs::create_dir_all(&scratch_dir).await;
    let mut artifact_roots = vec![
        std::path::PathBuf::from(session.get_valid_workspace()),
        scratch_dir.clone(),
    ];
    artifact_roots.extend(session.extra_dirs.iter().map(std::path::PathBuf::from));
    let (attachment_note, failed_attachments) =
        download_attachments(&client, attachments, &scratch_dir, msg_id).await;
    if !failed_attachments.is_empty() {
        let _ = reply(
            &client,
            target_id,
            is_c2c,
            msg_id,
            &format!(
                "⚠️ 有 {} 个附件未能获取：{}",
                failed_attachments.len(),
                failed_attachments.join("、")
            ),
        )
        .await;
    }

    let mut prompt_owned = attachment_note;
    if prompt_text.is_empty() {
        prompt_owned.push_str("请查看我随消息发送的附件并作出回应。");
    } else {
        prompt_owned.push_str(prompt_text);
    }
    let input_chars = prompt_owned.len();

    let extra_dirs_clone = session.extra_dirs.clone();
    let conv_id_clone = session.conversation_id.clone();
    let workspace_clone = session.get_valid_workspace();
    let model_clone = session.model.clone();
    let effort_clone = session.effort.clone();
    let mode_clone = session.mode.clone();
    let agent_clone = session.agent.clone();
    let system_prompt_clone = effective_system_prompt(&session.system_prompt, should_send_voice);
    let sandbox_val = session.sandbox;
    let project_clone = session.project.clone();
    let is_first_turn_val = session.is_first_turn;
    let idle_timeout = config.idle_timeout_secs;
    let max_timeout = config.max_execution_secs;

    let run_handle = tokio::spawn(async move {
        let params = ExecutionParams {
            prompt: &prompt_owned,
            conversation_id: conv_id_clone.as_deref(),
            is_first_turn: is_first_turn_val,
            workspace: &workspace_clone,
            extra_dirs: &extra_dirs_clone,
            model: &model_clone,
            effort: &effort_clone,
            mode: &mode_clone,
            agent: &agent_clone,
            system_prompt: &system_prompt_clone,
            sandbox: sandbox_val,
            project: &project_clone,
        };

        AgyExecutor::run_prompt_streaming(
            params,
            stream_tx,
            Some(pid_tx),
            idle_timeout,
            max_timeout,
        )
        .await
    });

    // Ingest stream events
    let mut accumulated_text = String::with_capacity(2048);
    let mut captured_conv_id: Option<String> = None;
    let mut captured_usage: Option<TokenUsage> = None;
    let mut captured_duration: Option<f64> = None;

    let start_time = Instant::now();

    while let Some(event) = stream_rx.recv().await {
        match event {
            StreamEvent::Init { conversation_id } => {
                captured_conv_id = Some(conversation_id);
            }
            StreamEvent::TextDelta(delta) => {
                accumulated_text.push_str(&delta);
            }
            StreamEvent::ThinkingDelta(_) => {
                // Can omit from raw chat or prefix if desired
            }
            StreamEvent::ToolUse { tool, summary } => {
                info!("Tool use in QQ chat {}: {} - {}", chat_id, tool, summary);
            }
            StreamEvent::TurnComplete { text } => {
                if !text.is_empty() {
                    accumulated_text = text;
                }
            }
            StreamEvent::Result {
                conversation_id,
                response,
                usage,
                duration_seconds,
                ..
            } => {
                if conversation_id.is_some() {
                    captured_conv_id = conversation_id;
                }
                if !response.is_empty() {
                    accumulated_text = response;
                }
                captured_usage = usage;
                captured_duration = duration_seconds;
            }
            StreamEvent::Error(err) => {
                warn!("StreamEvent error in QQ: {}", err);
            }
        }
    }

    let exec_res = run_handle.await?;
    let duration = captured_duration.unwrap_or_else(|| start_time.elapsed().as_secs_f64());

    match exec_res {
        Ok(res) => {
            let final_text = if !accumulated_text.trim().is_empty() {
                accumulated_text
            } else if !res.output.trim().is_empty() {
                res.output
            } else {
                "✅ 任务执行完毕 (无文本输出)".to_string()
            };

            let outcome = TurnOutcome {
                conversation_id: captured_conv_id.or(res.conversation_id),
                input_chars,
                output_chars: final_text.len(),
                usage: captured_usage.as_ref().or(res.usage.as_ref()),
                duration_seconds: Some(duration),
            };

            // A `/cancel` may have retired this generation; posting its leftovers would land
            // after the cancellation notice.
            let owns_turn = session_mgr.finish_turn(chat_id, generation, outcome).await;
            if owns_turn {
                // Voice mode is voice-only: the text is just the script for TTS. Text is only
                // sent when synthesis fails or no key is configured, so the turn is never silent.
                let mut voice_sent = false;
                if should_send_voice {
                    if let Some(ref api_key) = config.fish_api_key {
                        match send_qq_voice(
                            &client,
                            target_id,
                            is_c2c,
                            msg_id,
                            &final_text,
                            api_key,
                            config.fish_reference_id.as_deref(),
                            &scratch_dir,
                        )
                        .await
                        {
                            Ok(()) => voice_sent = true,
                            Err(err) => warn!("QQ chat {}: voice delivery failed: {:#}", chat_id, err),
                        }
                    } else {
                        warn!("QQ chat {}: voice requested but fish_api_key is missing", chat_id);
                    }
                }

                if !voice_sent {
                    let fallback = if should_send_voice && config.fish_api_key.is_none() {
                        format!(
                            "{}\n\n（未配置 fish_api_key，本条为文字回复）",
                            strip_emotion_tags(&final_text)
                        )
                    } else {
                        strip_emotion_tags(&final_text)
                    };
                    reply(&client, target_id, is_c2c, msg_id, &fallback).await?;
                }

                deliver_qq_images(
                    &client,
                    target_id,
                    is_c2c,
                    msg_id,
                    &final_text,
                    &artifact_roots,
                )
                .await;
            }

            if let Some(ref p_id) = placeholder_id {
                if is_c2c {
                    let _ = client.delete_c2c_message(target_id, p_id).await;
                } else {
                    let _ = client.delete_group_message(target_id, p_id).await;
                }
            }
        }
        Err(err) => {
            error!("Execution failed in QQ chat {}: {:#}", chat_id, err);
            if session_mgr.is_generation_current(chat_id, generation).await {
                reply(
                    &client,
                    target_id,
                    is_c2c,
                    msg_id,
                    &format!("❌ 执行失败: {:#}", err),
                )
                .await?;
            }

            if let Some(ref p_id) = placeholder_id {
                if is_c2c {
                    let _ = client.delete_c2c_message(target_id, p_id).await;
                } else {
                    let _ = client.delete_group_message(target_id, p_id).await;
                }
            }
        }
    }

    Ok(())
}

const MAX_QQ_ATTACHMENTS: usize = 8;
const MAX_QQ_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// QQ's message id is remote-controlled and may carry separators; keep a short alnum token
/// before it becomes part of a filename.
fn sanitize_message_id(id: &str) -> String {
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

fn attachment_extension(att: &MessageAttachment) -> &'static str {
    match att.content_type.as_deref().unwrap_or("") {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/jpeg" | "image/jpg" => "jpg",
        "video/mp4" => "mp4",
        "voice" => "wav",
        _ => "bin",
    }
}

/// Voice notes are recorded as SILK, which agy cannot read; QQ also serves a WAV transcode,
/// so use that when present and keep SILK only as the last resort.
fn attachment_source(att: &MessageAttachment) -> (&str, &'static str) {
    if att.content_type.as_deref() == Some("voice") {
        if let Some(wav) = att.voice_wav_url.as_deref().filter(|url| !url.is_empty()) {
            return (wav, "wav");
        }
        return (att.url.as_str(), "silk");
    }
    (att.url.as_str(), attachment_extension(att))
}

fn attachment_is_image(att: &MessageAttachment) -> bool {
    att.content_type
        .as_deref()
        .is_some_and(|kind| kind.starts_with("image/"))
}

/// Downloads attachments into `scratch_dir`, returning the prompt note for agy and the labels
/// that could not be fetched. The note uses the same wording as the Telegram ingress.
async fn download_attachments(
    client: &QqClient,
    attachments: &[MessageAttachment],
    scratch_dir: &std::path::Path,
    msg_id: &str,
) -> (String, Vec<String>) {
    let mut note = String::new();
    let mut failures = Vec::new();
    let safe_id = sanitize_message_id(msg_id);

    for (idx, att) in attachments.iter().take(MAX_QQ_ATTACHMENTS).enumerate() {
        let (url, ext) = attachment_source(att);
        let display_name = att.filename.as_deref().unwrap_or("未命名附件");
        if url.is_empty() {
            failures.push(format!("{}. {display_name}", idx + 1));
            continue;
        }

        let fallback = format!("attachment.{ext}");
        let mut name = crate::bot::media::safe_attachment_name(
            format!("{safe_id}-{idx}"),
            att.filename.as_deref(),
            &fallback,
        );
        if std::path::Path::new(&name).extension().is_none() {
            name.push('.');
            name.push_str(ext);
        }

        let dest = scratch_dir.join(&name);
        match client
            .download_attachment(url, &dest, MAX_QQ_ATTACHMENT_BYTES)
            .await
        {
            Ok(bytes) => {
                let label = if attachment_is_image(att) {
                    "Image"
                } else {
                    "File"
                };
                note.push_str(&format!("[Attached {label} File: {}]\n", dest.display()));
                info!("QQ attachment saved: {} ({} bytes)", dest.display(), bytes);
            }
            Err(err) => {
                warn!(
                    "QQ attachment download failed for {}: {:#}",
                    dest.display(),
                    err
                );
                failures.push(format!("{}. {display_name}", idx + 1));
            }
        }
    }

    (note, failures)
}

/// Images QQ accepts for `file_type=1`; anything else stays as a path in the text.
const QQ_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
/// QQ's direct `file_data` upload ceiling (5 MB decoded); larger files need the chunked flow.
const QQ_MAX_MEDIA_BYTES: u64 = 5 * 1024 * 1024;

/// Scans the finished answer for artifact paths and pushes the images back as rich media.
/// QQ needs an upload+`msg_type=7` round-trip per file, so only files the platform accepts as
/// images are attempted; other artifacts stay discoverable via their printed paths.
async fn deliver_qq_images(
    client: &QqClient,
    target_id: &str,
    is_c2c: bool,
    msg_id: &str,
    output_text: &str,
    artifact_roots: &[std::path::PathBuf],
) {
    for path in crate::bot::media::collect_artifact_paths(output_text, artifact_roots) {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_lowercase)
            .unwrap_or_default();
        if !QQ_IMAGE_EXTENSIONS.contains(&extension.as_str()) {
            continue;
        }
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        if crate::bot::media::looks_sensitive(&file_name)
            || crate::bot::media::looks_sensitive(&path.to_string_lossy())
        {
            warn!("QQ chat: refusing to auto-send sensitive-looking {}", path.display());
            continue;
        }
        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if meta.len() == 0 || meta.len() > QQ_MAX_MEDIA_BYTES {
            warn!(
                "QQ chat: skipping {} ({} bytes); direct upload limit is {}",
                path.display(),
                meta.len(),
                QQ_MAX_MEDIA_BYTES
            );
            continue;
        }
        match client.send_image(target_id, is_c2c, &path, msg_id).await {
            Ok(_) => info!("QQ chat: delivered image {}", path.display()),
            Err(err) => warn!(
                "QQ chat: image delivery failed for {}: {:#}",
                path.display(),
                err
            ),
        }
    }
}

/// Generates Fish Audio speech as MP3 (QQ voice accepts mp3/wav/silk) and sends it as
/// `file_type=3`. The temp file lives in scratch and is removed after the upload.
#[allow(clippy::too_many_arguments)] // Routing + TTS credentials, each already owned by the caller.
async fn send_qq_voice(
    client: &QqClient,
    target_id: &str,
    is_c2c: bool,
    msg_id: &str,
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
    scratch_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let audio =
        crate::tts::generate_speech_with_format(text, api_key, reference_id, "mp3").await?;
    if audio.len() as u64 > QQ_MAX_MEDIA_BYTES {
        anyhow::bail!(
            "voice is {} bytes, over the {} byte direct upload limit",
            audio.len(),
            QQ_MAX_MEDIA_BYTES
        );
    }
    let path = scratch_dir.join(format!("voice-{}.mp3", sanitize_message_id(msg_id)));
    tokio::fs::write(&path, &audio).await?;
    let result = client.send_voice(target_id, is_c2c, &path, msg_id).await;
    let _ = tokio::fs::remove_file(&path).await;
    result.map(|_| ())
}

/// Helper to chunk long output into <= max_len blocks
fn chunk_text(text: &str, max_len: usize) -> Vec<String> {
    if text.len() <= max_len {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::with_capacity(max_len);

    for line in text.split_inclusive('\n') {
        if current.len() + line.len() > max_len {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            if line.len() > max_len {
                // Line itself exceeds max_len, split by chars
                let mut sub = String::new();
                for ch in line.chars() {
                    sub.push(ch);
                    if sub.len() >= max_len {
                        chunks.push(std::mem::take(&mut sub));
                    }
                }
                if !sub.is_empty() {
                    current = sub;
                }
            } else {
                current.push_str(line);
            }
        } else {
            current.push_str(line);
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

/// Strips HTML tags for clean plain-text presentation in QQ
fn strip_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;

    for ch in input.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(ch);
        }
    }

    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_html_tags() {
        let input = "<b>Hello</b> <code>world &amp; test</code>";
        assert_eq!(strip_html_tags(input), "Hello world & test");

        let input_entities = "&lt;code&gt;let x = 1;&lt;/code&gt;";
        assert_eq!(strip_html_tags(input_entities), "<code>let x = 1;</code>");
    }

    #[test]
    fn test_chunk_text_short() {
        let text = "short text";
        let chunks = chunk_text(text, 100);
        assert_eq!(chunks, vec!["short text"]);
    }

    #[test]
    fn test_chunk_text_long_multiline() {
        let text = "Line 1\nLine 2\nLine 3\nLine 4\n";
        let chunks = chunk_text(text, 14);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_chunk_text_long_single_line() {
        let text = "a".repeat(30);
        let chunks = chunk_text(&text, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn test_attachment_extension_from_content_type() {
        let mut att = MessageAttachment {
            url: "https://example.com/x".into(),
            content_type: Some("image/jpeg".into()),
            filename: None,
            size: None,
            voice_wav_url: None,
        };
        assert_eq!(attachment_extension(&att), "jpg");
        att.content_type = Some("image/png".into());
        assert_eq!(attachment_extension(&att), "png");
        att.content_type = Some("video/mp4".into());
        assert_eq!(attachment_extension(&att), "mp4");
        att.content_type = Some("unknown/type".into());
        assert_eq!(attachment_extension(&att), "bin");
    }

    #[test]
    fn test_voice_prefers_wav_transcode() {
        let att = MessageAttachment {
            url: "https://example.com/x.silk".into(),
            content_type: Some("voice".into()),
            filename: None,
            size: None,
            voice_wav_url: Some("https://example.com/x.wav".into()),
        };
        let (url, ext) = attachment_source(&att);
        assert_eq!(url, "https://example.com/x.wav");
        assert_eq!(ext, "wav");

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
    fn test_effective_system_prompt_defaults_to_brevity() {
        assert_eq!(
            effective_system_prompt("   ", false),
            QQ_DEFAULT_SYSTEM_PROMPT
        );
        assert_eq!(effective_system_prompt("", false), QQ_DEFAULT_SYSTEM_PROMPT);
    }

    #[test]
    fn test_effective_system_prompt_adds_voice_cues_and_keeps_custom_instructions() {
        let prompt = effective_system_prompt("回答用日语", true);
        assert!(prompt.starts_with(QQ_DEFAULT_SYSTEM_PROMPT));
        assert!(prompt.contains(QQ_VOICE_STYLE_PROMPT));
        assert!(prompt.ends_with("回答用日语"));
    }

    #[test]
    fn test_strip_emotion_tags_keeps_normal_brackets() {
        assert_eq!(
            strip_emotion_tags("[calm]先说结论。[excited][emphasis]太好了！"),
            "先说结论。太好了！"
        );
        assert_eq!(
            strip_emotion_tags("参考 [文档](https://example.com) 与 [1, 2]"),
            "参考 [文档](https://example.com) 与 [1, 2]"
        );
    }
}
