use super::client::QqClient;
use super::types::{C2CMessageEvent, GroupMessageEvent};
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

    if !config.is_qq_user_allowed(sender_openid) && !config.is_qq_user_allowed(group_openid) {
        warn!(
            "Ignoring unauthorized Group message from member: {} in group: {}",
            sender_openid, group_openid
        );
        return;
    }

    let chat_id = qq_group_openid_to_chat_id(group_openid);
    if let Err(e) = process_message(
        client,
        config,
        session_mgr,
        group_openid,
        false,
        chat_id,
        &event.content,
        &event.id,
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
) -> Result<()> {
    // QQ messages are limited to ~1500-2000 chars per message.
    let chunks = chunk_text(text, 1400);
    for chunk in chunks {
        if is_c2c {
            let _ = client.send_c2c_message(target_id, &chunk, msg_id).await?;
        } else {
            let _ = client.send_group_message(target_id, &chunk, msg_id).await?;
        }
    }
    Ok(())
}

async fn process_message(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    target_id: &str,
    is_c2c: bool,
    chat_id: i64,
    raw_content: &str,
    msg_id: &str,
) -> Result<()> {
    let content = raw_content.trim();
    if content.is_empty() {
        return Ok(());
    }

    info!("Received QQ message from {}: {}", target_id, content);

    // Command Dispatcher
    if content.starts_with('/') {
        let mut parts = content.split_whitespace();
        let cmd = parts.next().unwrap_or_default().to_lowercase();
        let arg = content.strip_prefix(&cmd).unwrap_or("").trim();

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
                    session_mgr.set_model(chat_id, arg.to_string()).await;
                    reply(
                        &client,
                        target_id,
                        is_c2c,
                        msg_id,
                        &format!("✅ AI 模型已切换为: {}", arg),
                    )
                    .await?;
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

            _ => {
                // Unknown command; fall through to prompt execution if not purely an unrecognized slash command
            }
        }
    }

    // Natural Language Prompt Turn Execution
    let Some(generation) = session_mgr.begin_turn(chat_id).await else {
        reply(
            &client,
            target_id,
            is_c2c,
            msg_id,
            "⏳ 上一条任务正在运行中... 发送 /cancel 强行中断任务。",
        )
        .await?;
        return Ok(());
    };

    let _turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);
    let session = session_mgr.get_or_create(chat_id).await;

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    // Spawn PID recorder
    let session_mgr_pid = session_mgr.clone();
    tokio::spawn(async move {
        if let Ok(pid) = pid_rx.await {
            session_mgr_pid.set_pid(chat_id, pid, generation).await;
        }
    });

    let extra_dirs_clone = session.extra_dirs.clone();
    let prompt_owned = content.to_string();
    let conv_id_clone = session.conversation_id.clone();
    let workspace_clone = session.get_valid_workspace();
    let model_clone = session.model.clone();
    let effort_clone = session.effort.clone();
    let mode_clone = session.mode.clone();
    let agent_clone = session.agent.clone();
    let system_prompt_clone = session.system_prompt.clone();
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
                input_chars: content.len(),
                output_chars: final_text.len(),
                usage: captured_usage.as_ref().or(res.usage.as_ref()),
                duration_seconds: Some(duration),
            };

            session_mgr.finish_turn(chat_id, generation, outcome).await;
            reply(&client, target_id, is_c2c, msg_id, &final_text).await?;
        }
        Err(err) => {
            error!("Execution failed in QQ chat {}: {:#}", chat_id, err);
            reply(
                &client,
                target_id,
                is_c2c,
                msg_id,
                &format!("❌ 执行失败: {:#}", err),
            )
            .await?;
        }
    }

    Ok(())
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
}
