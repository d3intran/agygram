use super::client::{QqClient, MAX_DIRECT_MEDIA_BYTES};
use super::formatter::QqFormatter;
use super::types::{C2CMessageEvent, GroupMessageEvent, MessageAttachment, QqMediaFileType};
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent, TokenUsage};
use crate::models::{
    effort_from_model_suffix, is_safe_cli_value, is_valid_model, is_valid_voice_reference,
    normalize_model_name, Effort, Mode, QqGroupSessionMode,
};
use crate::session::{
    legacy_qq_group_member_to_chat_id, legacy_qq_group_openid_to_chat_id,
    legacy_qq_openid_to_chat_id, qq_group_member_to_chat_id, qq_group_openid_to_chat_id,
    qq_openid_to_chat_id, resolve_user_path, SessionManager, TurnGuard, TurnOutcome,
};
use crate::text::{looks_sensitive, safe_attachment_name, strip_html_tags};
use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use std::borrow::Cow;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub(crate) use super::client::QqTarget;

const QQ_MAX_CHUNK_CHARS: usize = 1400;
const QQ_MAX_CHUNK_BYTES: usize = 4500;
const TURN_QUEUE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_QQ_ATTACHMENTS: usize = 8;
const MAX_QQ_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
const ATTACHMENT_DOWNLOAD_CONCURRENCY: usize = 4;

/// Images QQ accepts for `file_type=1`; anything else stays as a path in the text.
const QQ_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// QQ is a chat surface, not a document reader: default to short answers. The model also tends
/// to re-issue identical searches seconds apart, so that is called out explicitly.
const QQ_DEFAULT_SYSTEM_PROMPT: &str = "你正在通过 QQ 聊天窗口与用户对话。默认保持简洁：先给结论，再按需补充关键要点，尽量控制在 300 字以内；不要复述用户的问题，不要输出过程旁白和客套话；工具结果已足够时立即作答，不要重复相同的搜索或命令。只有用户明确要求详细展开时才输出长回答。";

/// Voice turns are read by Fish Audio, which interprets inline `[bracket]` cues for delivery
/// (https://docs.fish.audio/developer-guide/core-features/emotions). The cue stays in the text we
/// hand to TTS and is stripped if the turn falls back to a plain text reply.
const QQ_VOICE_STYLE_PROMPT: &str = "本轮将以语音回复。请在回答文本中自然加入 Fish Audio 情感标签：每个句子开头最多一个，例如 [calm]、[happy]、[curious]、[empathetic]，需要强调时用 [emphasis]，需要停顿时用 [break]；最多叠加两个标签，不要解释或提及这些标签本身。";

#[derive(Clone)]
enum OwnedQqTarget {
    C2c(String),
    Group(String),
}

impl OwnedQqTarget {
    fn from_target(target: QqTarget<'_>) -> Self {
        match target {
            QqTarget::C2c(id) => Self::C2c(id.to_string()),
            QqTarget::Group(id) => Self::Group(id.to_string()),
        }
    }

    fn as_target(&self) -> QqTarget<'_> {
        match self {
            Self::C2c(id) => QqTarget::C2c(id),
            Self::Group(id) => QqTarget::Group(id),
        }
    }
}

/// RAII guard that ensures `"我想想..."` / queue-notice placeholders are deleted even if an
/// early `?` return, cancellation, or panic interrupts delivery.
struct PlaceholderGuard {
    client: Arc<QqClient>,
    target: OwnedQqTarget,
    message_id: Option<String>,
}

impl PlaceholderGuard {
    fn new(client: Arc<QqClient>, target: QqTarget<'_>, message_id: Option<String>) -> Self {
        Self {
            client,
            target: OwnedQqTarget::from_target(target),
            message_id,
        }
    }

    fn set_message_id(&mut self, message_id: Option<String>) {
        self.message_id = message_id;
    }

    async fn cleanup(mut self) {
        if let Some(p_id) = self.message_id.take() {
            if let Err(err) = self
                .client
                .delete_message(self.target.as_target(), &p_id)
                .await
            {
                debug!(error = %err, "Failed to recall QQ placeholder message");
            }
        }
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        let Some(p_id) = self.message_id.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let target = self.target.clone();
            handle.spawn(async move {
                let _ = client.delete_message(target.as_target(), &p_id).await;
            });
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CommandOutcome<'a> {
    Handled,
    ExecutePrompt {
        prompt_text: &'a str,
        voice_requested: bool,
    },
}

#[tracing::instrument(skip(client, config, session_mgr, event), fields(msg_id = %event.id))]
pub(crate) async fn handle_c2c(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    event: C2CMessageEvent,
) {
    let openid = event.author.user_openid.trim();
    if !config.is_qq_user_allowed(openid) {
        warn!(openid, "Ignoring unauthorized QQ C2C message");
        return;
    }

    let chat_id = qq_openid_to_chat_id(openid);
    let legacy_id = legacy_qq_openid_to_chat_id(openid);
    session_mgr.migrate_qq_session(legacy_id, chat_id).await;

    if let Err(err) = process_message(
        client,
        config,
        session_mgr,
        QqTarget::C2c(openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
    )
    .await
    {
        error!(chat_id, error = %format!("{err:#}"), "Error processing QQ C2C message");
    }
}

#[tracing::instrument(skip(client, config, session_mgr, event), fields(msg_id = %event.id))]
pub(crate) async fn handle_group(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    event: GroupMessageEvent,
) {
    let sender_openid = event.author.member_openid.trim();
    let group_openid = event.group_openid.trim();

    if !config.is_qq_group_allowed(group_openid, sender_openid) {
        warn!(
            group_openid,
            sender_openid, "Ignoring unauthorized QQ Group message"
        );
        return;
    }

    let (chat_id, legacy_id) = match config.qq_group_session_mode_enum() {
        QqGroupSessionMode::Member => (
            qq_group_member_to_chat_id(group_openid, sender_openid),
            legacy_qq_group_member_to_chat_id(group_openid, sender_openid),
        ),
        QqGroupSessionMode::Group => (
            qq_group_openid_to_chat_id(group_openid),
            legacy_qq_group_openid_to_chat_id(group_openid),
        ),
    };
    session_mgr.migrate_qq_session(legacy_id, chat_id).await;

    if let Err(err) = process_message(
        client,
        config,
        session_mgr,
        QqTarget::Group(group_openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
    )
    .await
    {
        error!(chat_id, error = %format!("{err:#}"), "Error processing QQ Group message");
    }
}

async fn reply(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    let formatted = QqFormatter::format_markdown(text);
    // QQ messages are limited to ~1500-2000 chars per message.
    let chunks = chunk_text(&formatted, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
    let mut last_id = None;
    for chunk in chunks {
        let sent = client.send_message(target, &chunk, msg_id).await?;
        if sent.is_some() {
            last_id = sent;
        }
    }
    Ok(last_id)
}

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

#[inline]
fn is_emotion_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, ' ' | '-' | '\''))
}

/// Drops Fish Audio cues so a failed voice turn can fall back to readable text without
/// allocating when no emotion cues are present.
fn strip_emotion_tags(text: &str) -> Cow<'_, str> {
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
        if is_emotion_tag(&after[..close]) {
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
        if !is_emotion_tag(tag) {
            out.push('[');
            out.push_str(&after[..close + 1]);
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out.trim().to_string())
}

#[allow(clippy::too_many_arguments)]
async fn process_message(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    target: QqTarget<'_>,
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

    info!(
        target_kind = target.kind_label(),
        chat_id,
        msg_len = content.len(),
        attachment_count = attachments.len(),
        "Received QQ message"
    );

    match handle_qq_command(
        &client,
        &config,
        &session_mgr,
        target,
        chat_id,
        content,
        msg_id,
    )
    .await?
    {
        CommandOutcome::Handled => Ok(()),
        CommandOutcome::ExecutePrompt {
            prompt_text,
            voice_requested,
        } => {
            execute_qq_prompt(
                client,
                config,
                session_mgr,
                target,
                chat_id,
                prompt_text,
                voice_requested,
                msg_id,
                attachments,
            )
            .await
        }
    }
}

async fn handle_qq_command<'a>(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    content: &'a str,
    msg_id: &str,
) -> Result<CommandOutcome<'a>> {
    if !content.starts_with('/') {
        return Ok(CommandOutcome::ExecutePrompt {
            prompt_text: content,
            voice_requested: false,
        });
    }

    let cmd_raw = content.split_whitespace().next().unwrap_or_default();
    let cmd = cmd_raw.to_ascii_lowercase();
    // Split on the original text so `/MODEL opus` preserves argument casing.
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
• /model [<名称/别名>] - 切换模型 (如 argon, 3.8, sonnet, opus, oss)
• /models - 查看系统支持的模型列表
• /skills - 查看已挂载的 Antigravity 技能库
• /rules - 查看当前生效的全局与项目规则
• /effort [<级别>] - 设置思考力度 (low / medium / high / xhigh / max)
• /mode [<模式>] - 设置执行模式 (auto / accept-edits / plan)
• /voice [on|off|toggle] - 语音回复开关（开启后只发语音条）；/voice id <音色ID> 更换音色；/voice <问题> 单次语音 (Fish Audio TTS)
• /cd <路径> - 切换默认工作目录
• /dirs 或 /pwd - 查看当前绑定的工作目录
• /cancel - 中断当前正在执行的任务

💬 提问方式：
直接发送任何代码开发需求或问题，即可唤起 Antigravity CLI 自动执行！";
            reply(client, target, msg_id, help_text).await?;
            Ok(CommandOutcome::Handled)
        }

        "/status" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let conv_id = session.conversation_id.as_deref().unwrap_or("新建会话");
            let model = if session.model.is_empty() {
                "系统默认 (Gemini 3.8 Flash)"
            } else {
                &session.model
            };
            let voice_status = if session.voice_mode {
                "已开启"
            } else {
                "已关闭"
            };
            let status_text = format!(
                "📊 当前会话状态：\n\n\
                • 会话 ID: {conv_id}\n\
                • 工作目录: {}\n\
                • 当前模型: {model}\n\
                • 思考力度: {}\n\
                • 执行模式: {}\n\
                • 语音模式: {voice_status}\n\
                • 对话轮数: {}\n\
                • 累计消耗: {} in / {} out tokens",
                session.workspace,
                session.effort,
                session.mode,
                session.turn_count,
                session.total_input_tokens,
                session.total_output_tokens
            );
            reply(client, target, msg_id, &status_text).await?;
            Ok(CommandOutcome::Handled)
        }

        "/new" | "/reset" | "/clear" => {
            session_mgr.reset_conversation(chat_id).await;
            reply(
                client,
                target,
                msg_id,
                "🔄 会话上下文已重置，已开启全新对话轮次。",
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }

        "/usage" | "/quota" => {
            let _ = reply(client, target, msg_id, "⏳ 正在查询实时额度...").await;
            match crate::quota::QuotaFetcher::format_models_and_quota_plain().await {
                Ok(plain_text) => {
                    reply(client, target, msg_id, &plain_text).await?;
                }
                Err(err) => {
                    let err_msg = format!("⚠️ 实时额度获取失败: {err:#}");
                    reply(client, target, msg_id, &err_msg).await?;
                }
            }
            Ok(CommandOutcome::Handled)
        }

        "/models" => {
            match AgyExecutor::list_models().await {
                Ok(models) => {
                    let text = format!("📋 Antigravity CLI 支持的模型列表：\n\n{models}");
                    reply(client, target, msg_id, &text).await?;
                }
                Err(err) => {
                    reply(
                        client,
                        target,
                        msg_id,
                        &format!("⚠️ 获取模型列表失败: {err:#}"),
                    )
                    .await?;
                }
            }
            Ok(CommandOutcome::Handled)
        }

        "/skills" => {
            let skills = AgyExecutor::list_skills_plain().await;
            reply(
                client,
                target,
                msg_id,
                &format!("🧩 Antigravity 技能库 (Skills)：\n\n{skills}"),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }

        "/rules" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let rules = AgyExecutor::list_rules_plain(&session.workspace).await;
            reply(
                client,
                target,
                msg_id,
                &format!("🛡️ Antigravity 规则配置 (Rules)：\n\n{rules}"),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }

        "/model" => {
            handle_model_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }

        "/effort" => {
            if arg.is_empty() {
                let session = session_mgr.get_or_create(chat_id).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!(
                        "⚡ 当前思考力度: {}\n可选: low, medium, high, xhigh, max",
                        session.effort
                    ),
                )
                .await?;
            } else if let Ok(effort) = arg.parse::<Effort>() {
                session_mgr.set_effort_enum(chat_id, effort).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("✅ 思考力度已设置为: {effort}"),
                )
                .await?;
            } else {
                reply(
                    client,
                    target,
                    msg_id,
                    "⚠️ 无效的思考力度。可选: low, medium, high, xhigh, max",
                )
                .await?;
            }
            Ok(CommandOutcome::Handled)
        }

        "/mode" => {
            if arg.is_empty() {
                let session = session_mgr.get_or_create(chat_id).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!(
                        "🛠️ 当前执行模式: {}\n可选: auto, accept-edits, plan",
                        session.mode
                    ),
                )
                .await?;
            } else if let Ok(mode) = arg.parse::<Mode>() {
                session_mgr.set_mode_enum(chat_id, mode).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("✅ 执行模式已设置为: {mode}"),
                )
                .await?;
            } else {
                reply(
                    client,
                    target,
                    msg_id,
                    "⚠️ 无效的执行模式。可选: auto, accept-edits, plan",
                )
                .await?;
            }
            Ok(CommandOutcome::Handled)
        }

        "/cd" => {
            if arg.is_empty() {
                reply(
                    client,
                    target,
                    msg_id,
                    "⚠️ 请提供目标目录路径，例如 /cd ~/project",
                )
                .await?;
            } else {
                let session = session_mgr.get_or_create(chat_id).await;
                let target_path = resolve_user_path(arg, &session.workspace);
                let is_existing_dir = tokio::fs::metadata(&target_path)
                    .await
                    .is_ok_and(|meta| meta.is_dir());
                if is_existing_dir || tokio::fs::create_dir_all(&target_path).await.is_ok() {
                    let path_str = target_path.to_string_lossy().to_string();
                    session_mgr.set_workspace(chat_id, path_str.clone()).await;
                    let prefix = if is_existing_dir {
                        "📂 工作主目录已切换至"
                    } else {
                        "📂 目标目录不存在，已新建并切换至"
                    };
                    reply(client, target, msg_id, &format!("{prefix}:\n{path_str}")).await?;
                } else {
                    reply(
                        client,
                        target,
                        msg_id,
                        &format!("❌ 无法切换工作目录: 路径 {arg} 无效或权限不足。"),
                    )
                    .await?;
                }
            }
            Ok(CommandOutcome::Handled)
        }

        "/pwd" | "/dirs" => {
            let session = session_mgr.get_or_create(chat_id).await;
            reply(
                client,
                target,
                msg_id,
                &format!("📂 当前工作目录:\n{}", session.workspace),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }

        "/cancel" => {
            let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
            if was_running {
                let pid_str = pid.map_or_else(|| "-".to_string(), |p| p.to_string());
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("🛑 任务已强行中断 (PID: {pid_str})，工作区锁已释放。"),
                )
                .await?;
            } else {
                reply(client, target, msg_id, "ℹ️ 当前没有正在执行的任务。").await?;
            }
            Ok(CommandOutcome::Handled)
        }

        "/voice" => {
            handle_voice_command(client, config, session_mgr, target, chat_id, arg, msg_id).await
        }

        _ => Ok(CommandOutcome::ExecutePrompt {
            prompt_text: content,
            voice_requested: false,
        }),
    }
}

async fn handle_model_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let curr = if session.model.is_empty() {
            "系统默认 (Gemini 3.8 Flash)"
        } else {
            &session.model
        };
        reply(
            client,
            target,
            msg_id,
            &format!("🤖 当前 AI 模型: {curr}\n可使用 /model <名称/别名> 切换 (如 argon, 3.8, sonnet, opus, oss)。"),
        )
        .await?;
        return Ok(());
    }

    let normalized = normalize_model_name(arg);
    let is_valid = is_valid_model(&normalized)
        || (is_safe_cli_value(&normalized)
            && AgyExecutor::list_models()
                .await
                .map(|models| {
                    models
                        .lines()
                        .filter_map(|line| line.split_whitespace().next())
                        .any(|m| m.eq_ignore_ascii_case(&normalized))
                })
                .unwrap_or(false));

    if !is_valid {
        reply(
            client,
            target,
            msg_id,
            &format!("⚠️ 无效的模型名称: {arg}\n发送 /models 查看当前可用列表。"),
        )
        .await?;
        return Ok(());
    }

    session_mgr.set_model(chat_id, normalized.to_string()).await;
    if let Some(effort) = effort_from_model_suffix(&normalized) {
        session_mgr.set_effort_enum(chat_id, effort).await;
    }

    let display = if normalized.is_empty() {
        "系统默认 (Gemini 3.8 Flash)"
    } else {
        normalized.as_ref()
    };
    reply(
        client,
        target,
        msg_id,
        &format!("✅ AI 模型已切换为: {display}"),
    )
    .await?;
    Ok(())
}

async fn handle_voice_command<'a>(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &'a str,
    msg_id: &str,
) -> Result<CommandOutcome<'a>> {
    if arg.eq_ignore_ascii_case("on") || arg == "开启" {
        session_mgr.set_voice_mode(chat_id, true).await;
        reply(
            client,
            target,
            msg_id,
            "🔊 语音回复已开启：后续每条回答会附带一条语音。",
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }
    if arg.eq_ignore_ascii_case("off") || arg == "关闭" {
        session_mgr.set_voice_mode(chat_id, false).await;
        reply(client, target, msg_id, "🔇 语音回复已关闭。").await?;
        return Ok(CommandOutcome::Handled);
    }
    if arg.eq_ignore_ascii_case("toggle") || arg == "切换" {
        let enabled = session_mgr.toggle_voice_mode(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!("🔊 语音回复: {}", if enabled { "已开启" } else { "已关闭" }),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    if arg
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("id"))
    {
        let id_arg = arg
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim());
        if id_arg.is_empty() {
            let session = session_mgr.get_or_create(chat_id).await;
            let current = session
                .voice_reference_id
                .as_deref()
                .unwrap_or("(未设置，使用默认)");
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "🎙 当前会话音色ID: {current}\n用法: /voice id <音色ID> 更换；/voice id reset 恢复默认"
                ),
            )
            .await?;
        } else if id_arg.eq_ignore_ascii_case("reset")
            || id_arg.eq_ignore_ascii_case("default")
            || id_arg == "重置"
        {
            session_mgr.set_voice_reference(chat_id, None).await;
            let fallback = config
                .fish_reference_id
                .as_deref()
                .unwrap_or("Fish Audio 默认音色");
            reply(
                client,
                target,
                msg_id,
                &format!("✅ 已恢复默认音色: {fallback}"),
            )
            .await?;
        } else if is_valid_voice_reference(id_arg) {
            session_mgr
                .set_voice_reference(chat_id, Some(id_arg.to_string()))
                .await;
            reply(
                client,
                target,
                msg_id,
                &format!("✅ 音色已切换: {id_arg}\n下一轮语音回复生效。"),
            )
            .await?;
        } else {
            reply(
                client,
                target,
                msg_id,
                "⚠️ 音色ID格式不正确：只允许字母、数字、- 和 _，长度 8-128。",
            )
            .await?;
        }
        return Ok(CommandOutcome::Handled);
    }

    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let current_ref = session
            .voice_reference_id
            .as_deref()
            .or(config.fish_reference_id.as_deref())
            .unwrap_or("Fish Audio 默认音色");
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🔊 语音回复 (Fish Audio TTS)\n状态: {}\n音色ID: {current_ref}\n用法: /voice on|off|toggle；/voice id <音色ID> 更换音色；/voice id reset 恢复默认；/voice <问题> 单次语音",
                if session.voice_mode { "已开启" } else { "已关闭" }
            ),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    Ok(CommandOutcome::ExecutePrompt {
        prompt_text: arg,
        voice_requested: true,
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute_qq_prompt(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    target: QqTarget<'_>,
    chat_id: i64,
    prompt_text: &str,
    voice_requested: bool,
    msg_id: &str,
    attachments: &[MessageAttachment],
) -> Result<()> {
    // Claim the turn atomically before emitting the thinking placeholder to prevent TOCTOU
    // duplicate placeholders under concurrent messages.
    let mut placeholder_guard = PlaceholderGuard::new(client.clone(), target, None);
    let generation = if let Some(gen) = session_mgr.begin_turn(chat_id).await {
        let p_id = reply(&client, target, msg_id, "我想想...")
            .await
            .ok()
            .flatten();
        placeholder_guard.set_message_id(p_id);
        gen
    } else {
        let q_id = reply(
            &client,
            target,
            msg_id,
            "⏳ 当前会话已有任务正在运行，已加入等待队列，即将自动处理...",
        )
        .await
        .ok()
        .flatten();
        placeholder_guard.set_message_id(q_id);

        let Some(gen) = session_mgr
            .begin_turn_with_timeout(chat_id, TURN_QUEUE_TIMEOUT)
            .await
        else {
            placeholder_guard.cleanup().await;
            reply(
                &client,
                target,
                msg_id,
                "⚠️ 排队超时：上一任务执行时间过长，请稍后重试或发送 /cancel 强行中断任务。",
            )
            .await?;
            return Ok(());
        };
        gen
    };

    let mut turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);
    let session = session_mgr.get_or_create(chat_id).await;
    let should_send_voice = session.voice_mode || voice_requested;
    let effective_voice_ref = session
        .voice_reference_id
        .as_deref()
        .or(config.fish_reference_id.as_deref());

    let workspace = session.get_valid_workspace();
    let scratch_dir = PathBuf::from(&workspace).join("scratch");
    if let Err(err) = tokio::fs::create_dir_all(&scratch_dir).await {
        warn!(dir = %scratch_dir.display(), error = %err, "Failed to create QQ scratch dir");
    }

    let mut artifact_roots = vec![PathBuf::from(&workspace), scratch_dir.clone()];
    artifact_roots.extend(session.extra_dirs.iter().map(PathBuf::from));

    let (attachment_note, failed_attachments) =
        download_attachments(&client, attachments, &scratch_dir, msg_id).await;
    if !failed_attachments.is_empty() {
        let _ = reply(
            &client,
            target,
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
    let system_prompt = effective_system_prompt(&session.system_prompt, should_send_voice);

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    let params = ExecutionParams {
        prompt: &prompt_owned,
        conversation_id: session.conversation_id.as_deref(),
        is_first_turn: session.is_first_turn,
        workspace: &workspace,
        extra_dirs: &session.extra_dirs,
        model: &session.model,
        effort: &session.effort,
        mode: &session.mode,
        agent: &session.agent,
        system_prompt: &system_prompt,
        sandbox: session.sandbox,
        project: &session.project,
    };

    let mut accumulated_text = String::with_capacity(2048);
    let mut captured_conv_id: Option<String> = None;
    let mut captured_usage: Option<TokenUsage> = None;
    let mut captured_duration: Option<f64> = None;
    let mut stream_error: Option<String> = None;

    let start_time = Instant::now();

    let exec_fut = AgyExecutor::run_prompt_streaming(
        params,
        stream_tx,
        Some(pid_tx),
        config.idle_timeout_secs,
        config.max_execution_secs,
    );

    let ingest_fut = async {
        let mut pid_rx = Some(pid_rx);
        loop {
            tokio::select! {
                biased;

                pid_res = async {
                    match pid_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                }, if pid_rx.is_some() => {
                    pid_rx = None;
                    if let Ok(pid) = pid_res {
                        session_mgr.set_pid(chat_id, pid, generation).await;
                    }
                }

                maybe_event = stream_rx.recv() => {
                    let Some(event) = maybe_event else {
                        break;
                    };
                    match event {
                        StreamEvent::Init { conversation_id } => {
                            captured_conv_id = Some(conversation_id);
                        }
                        StreamEvent::TextDelta(delta) => {
                            accumulated_text.push_str(&delta);
                        }
                        StreamEvent::ThinkingDelta(_) => {}
                        StreamEvent::ToolUse { tool, summary } => {
                            info!(chat_id, tool = %tool, summary = %summary, "QQ tool use");
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
                            // Break immediately rather than draining to EOF so detached background
                            // runs do not hang the QQ turn.
                            break;
                        }
                        StreamEvent::Error(err) => {
                            warn!(chat_id, error = %err, "StreamEvent error in QQ");
                            stream_error = Some(err);
                            break;
                        }
                    }
                }
            }
        }
    };

    let (exec_res, ()) = tokio::join!(exec_fut, ingest_fut);
    let duration = captured_duration.unwrap_or_else(|| start_time.elapsed().as_secs_f64());

    match exec_res {
        Ok(res) => {
            let final_text = if !accumulated_text.trim().is_empty() {
                accumulated_text
            } else if !res.output.trim().is_empty() {
                if res.status == "TIMEOUT" {
                    strip_html_tags(&res.output)
                } else {
                    res.output
                }
            } else if let Some(err) = stream_error {
                format!("⚠️ {}", strip_html_tags(&err))
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
            turn_guard.disarm();

            if owns_turn {
                deliver_qq_reply(
                    &client,
                    &config,
                    target,
                    chat_id,
                    msg_id,
                    &final_text,
                    should_send_voice,
                    effective_voice_ref,
                    &artifact_roots,
                )
                .await?;
            }

            placeholder_guard.cleanup().await;
        }
        Err(err) => {
            error!(chat_id, error = %format!("{err:#}"), "Execution failed in QQ chat");
            let is_current = session_mgr.is_generation_current(chat_id, generation).await;
            session_mgr.release_turn(chat_id, generation).await;
            turn_guard.disarm();

            if is_current {
                reply(&client, target, msg_id, &format!("❌ 执行失败: {err:#}")).await?;
            }
            placeholder_guard.cleanup().await;
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn deliver_qq_reply(
    client: &QqClient,
    config: &Config,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
    final_text: &str,
    should_send_voice: bool,
    effective_voice_ref: Option<&str>,
    artifact_roots: &[PathBuf],
) -> Result<()> {
    // Voice mode is voice-only: the text is just the script for TTS. Text is only sent when
    // synthesis fails or no key is configured, so the turn is never silent.
    let mut voice_sent = false;
    if should_send_voice {
        if let Some(ref api_key) = config.fish_api_key {
            match send_qq_voice(
                client,
                target,
                msg_id,
                final_text,
                api_key,
                effective_voice_ref,
            )
            .await
            {
                Ok(()) => voice_sent = true,
                Err(err) => warn!(chat_id, error = %format!("{err:#}"), "QQ voice delivery failed"),
            }
        } else {
            warn!(chat_id, "QQ voice requested but fish_api_key is missing");
        }
    }

    if !voice_sent {
        let fallback: Cow<'_, str> = if should_send_voice {
            let cleaned = strip_emotion_tags(final_text);
            if config.fish_api_key.is_none() {
                Cow::Owned(format!(
                    "{cleaned}\n\n（未配置 fish_api_key，本条为文字回复）"
                ))
            } else {
                cleaned
            }
        } else {
            Cow::Borrowed(final_text)
        };
        reply(client, target, msg_id, &fallback).await?;
    }

    deliver_qq_images(client, target, msg_id, final_text, artifact_roots).await;
    Ok(())
}

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

enum AttachmentFetchOutcome {
    Saved {
        dest: PathBuf,
        label: &'static str,
        bytes: u64,
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
    safe_id: &str,
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

    let fallback = format!("attachment.{ext}");
    let mut name = safe_attachment_name(
        format!("{safe_id}-{idx}"),
        att.filename.as_deref(),
        &fallback,
    );
    if Path::new(&name).extension().is_none() {
        name.push('.');
        name.push_str(ext);
    }

    let dest = scratch_dir.join(&name);
    let outcome = match client
        .download_attachment(url, &dest, MAX_QQ_ATTACHMENT_BYTES)
        .await
    {
        Ok(bytes) => {
            let label = if attachment_is_image(att) {
                "Image"
            } else {
                "File"
            };
            AttachmentFetchOutcome::Saved { dest, label, bytes }
        }
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

/// Downloads up to `MAX_QQ_ATTACHMENTS` concurrently into `scratch_dir`, returning the prompt
/// note for agy and the labels that could not be fetched.
async fn download_attachments(
    client: &QqClient,
    attachments: &[MessageAttachment],
    scratch_dir: &Path,
    msg_id: &str,
) -> (String, Vec<String>) {
    if attachments.is_empty() {
        return (String::new(), Vec::new());
    }

    let safe_id = sanitize_message_id(msg_id);
    let futs: Vec<_> = attachments
        .iter()
        .take(MAX_QQ_ATTACHMENTS)
        .enumerate()
        .map(|(idx, att)| download_one_attachment(client, idx, att, scratch_dir, &safe_id))
        .collect();
    let mut results: Vec<IndexedAttachmentFetch> = stream::iter(futs)
        .buffer_unordered(ATTACHMENT_DOWNLOAD_CONCURRENCY)
        .collect()
        .await;

    results.sort_unstable_by_key(|item| item.idx);

    let mut note = String::with_capacity(results.len() * 64);
    let mut failures = Vec::new();

    for item in results {
        match item.outcome {
            AttachmentFetchOutcome::Saved { dest, label, bytes } => {
                let _ = writeln!(note, "[Attached {label} File: {}]", dest.display());
                info!(dest = %dest.display(), bytes, "QQ attachment saved");
            }
            AttachmentFetchOutcome::Failed(failure_label) => failures.push(failure_label),
        }
    }

    (note, failures)
}

/// Scans the finished answer for artifact paths and pushes the images back as rich media.
async fn deliver_qq_images(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    output_text: &str,
    artifact_roots: &[PathBuf],
) {
    if output_text.is_empty() || artifact_roots.is_empty() {
        return;
    }

    let text_owned = output_text.to_string();
    let roots_owned = artifact_roots.to_vec();
    let paths = tokio::task::spawn_blocking(move || {
        crate::bot::media::collect_artifact_paths(&text_owned, &roots_owned)
    })
    .await
    .unwrap_or_default();

    for path in paths {
        let Some(ext_str) = path.extension().and_then(|ext| ext.to_str()) else {
            continue;
        };
        if !QQ_IMAGE_EXTENSIONS
            .iter()
            .any(|allowed| ext_str.eq_ignore_ascii_case(allowed))
        {
            continue;
        }

        let is_dotfile = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.starts_with('.'));
        if is_dotfile || looks_sensitive(&path.to_string_lossy()) {
            warn!(path = %path.display(), "QQ chat: refusing to auto-send sensitive-looking artifact");
            continue;
        }

        let Ok(meta) = tokio::fs::metadata(&path).await else {
            continue;
        };
        if meta.len() == 0 || meta.len() > MAX_DIRECT_MEDIA_BYTES {
            warn!(
                path = %path.display(),
                bytes = meta.len(),
                limit = MAX_DIRECT_MEDIA_BYTES,
                "QQ chat: skipping oversized or empty image artifact"
            );
            continue;
        }

        match client.send_image(target, &path, msg_id).await {
            Ok(_) => info!(path = %path.display(), "QQ chat: delivered image"),
            Err(err) => warn!(
                path = %path.display(),
                error = %format!("{err:#}"),
                "QQ chat: image delivery failed"
            ),
        }
    }
}

/// Generates Fish Audio speech as MP3 (QQ voice accepts mp3/wav/silk) and uploads the
/// in-memory `Bytes` directly via `send_rich_media_bytes` with zero disk I/O.
async fn send_qq_voice(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
) -> Result<()> {
    let audio = crate::tts::generate_speech_bytes(text, api_key, reference_id, "mp3").await?;
    if audio.len() as u64 > MAX_DIRECT_MEDIA_BYTES {
        anyhow::bail!(
            "voice is {} bytes, over the {} byte direct upload limit",
            audio.len(),
            MAX_DIRECT_MEDIA_BYTES
        );
    }
    client
        .send_rich_media_bytes(target, &audio, msg_id, QqMediaFileType::Voice, "voice.mp3")
        .await
        .map(|_| ())
}

/// Zero-copy line-aware chunker enforcing both character count (`max_chars`) and UTF-8 byte
/// length (`max_bytes`) without splitting multi-byte characters or overshooting limits.
fn chunk_text(text: &str, max_chars: usize, max_bytes: usize) -> Vec<Cow<'_, str>> {
    let max_chars = max_chars.max(1);
    let max_bytes = max_bytes.max(1);

    if text.is_empty() {
        return vec![Cow::Borrowed("")];
    }
    if text.len() <= max_bytes && (text.len() <= max_chars || text.chars().count() <= max_chars) {
        return vec![Cow::Borrowed(text)];
    }

    let approx_chunk_bytes = max_bytes.min(max_chars.saturating_mul(3)).max(1);
    let mut chunks = Vec::with_capacity((text.len() / approx_chunk_bytes).max(2));

    let mut chunk_start = 0usize;
    let mut chunk_end = 0usize;
    let mut chunk_chars = 0usize;
    let mut line_start = 0usize;

    for line in text.split_inclusive('\n') {
        let line_bytes = line.len();
        let line_chars = line.chars().count();
        let cur_bytes = chunk_end - chunk_start;

        if cur_bytes + line_bytes <= max_bytes && chunk_chars + line_chars <= max_chars {
            chunk_end += line_bytes;
            chunk_chars += line_chars;
        } else {
            if chunk_end > chunk_start {
                chunks.push(Cow::Borrowed(&text[chunk_start..chunk_end]));
            }
            if line_bytes <= max_bytes && line_chars <= max_chars {
                chunk_start = line_start;
                chunk_end = line_start + line_bytes;
                chunk_chars = line_chars;
            } else {
                let mut sub_start = 0usize;
                let mut sub_bytes = 0usize;
                let mut sub_chars = 0usize;

                for (idx, ch) in line.char_indices() {
                    let ch_len = ch.len_utf8();
                    if sub_bytes > 0
                        && (sub_bytes + ch_len > max_bytes || sub_chars + 1 > max_chars)
                    {
                        chunks.push(Cow::Borrowed(&line[sub_start..idx]));
                        sub_start = idx;
                        sub_bytes = 0;
                        sub_chars = 0;
                    }
                    sub_bytes += ch_len;
                    sub_chars += 1;
                }

                chunk_start = line_start + sub_start;
                chunk_end = line_start + line_bytes;
                chunk_chars = sub_chars;
            }
        }
        line_start += line_bytes;
    }

    if chunk_end > chunk_start {
        chunks.push(Cow::Borrowed(&text[chunk_start..chunk_end]));
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_recording_qq_server() -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let messages = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = messages.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = recorded.clone();
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

                    let req_line = headers.lines().next().unwrap_or("");
                    let body = String::from_utf8_lossy(
                        &buf[header_end..(header_end + content_len).min(buf.len())],
                    )
                    .to_string();

                    let resp_body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#.to_string()
                    } else {
                        recorded.lock().unwrap().push(body);
                        r#"{"id":"msg_out_1"}"#.to_string()
                    };

                    let raw_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw_resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (format!("http://{addr}"), messages, handle)
    }

    #[test]
    fn test_shared_strip_html_tags_handles_entities_and_unmatched_lt() {
        let input = "<b>Hello</b> <code>world &amp; test</code>";
        assert_eq!(strip_html_tags(input), "Hello world & test");

        let input_entities = "&lt;code&gt;let x = 1;&lt;/code&gt;";
        assert_eq!(strip_html_tags(input_entities), "<code>let x = 1;</code>");

        // Regression checks: no double-unescape on `&amp;quot;` and preserve unmatched `<`.
        assert_eq!(strip_html_tags("&amp;quot;x&amp;quot;"), "&quot;x&quot;");
        assert_eq!(
            strip_html_tags("if x < 5 and y > 2 then <b>ok</b>"),
            "if x < 5 and y > 2 then ok"
        );
    }

    #[test]
    fn test_chunk_text_short_multiline_and_single_line() {
        let text = "short text";
        let chunks = chunk_text(text, 100, 100);
        assert_eq!(chunks, vec!["short text"]);
        assert!(matches!(chunks[0], Cow::Borrowed(_)));

        let multi = "Line 1\nLine 2\nLine 3\nLine 4\n";
        let chunks = chunk_text(multi, 14, 14);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), multi);

        let single = "a".repeat(30);
        let chunks = chunk_text(&single, 10, 10);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), single);
    }

    #[test]
    fn test_chunk_text_cjk_char_limit_and_byte_overshoot_prevention() {
        // 1000 Chinese chars (3000 UTF-8 bytes) must stay in 1 chunk under QQ limits.
        let cjk_1000 = "中".repeat(1000);
        let chunks = chunk_text(&cjk_1000, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(chunks[0], Cow::Borrowed(_)));

        // 1500 Chinese chars (4500 bytes) splits cleanly at 1400 + 100 chars.
        let cjk_1500 = "文".repeat(1500);
        let chunks = chunk_text(&cjk_1500, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 1400);
        assert_eq!(chunks[1].chars().count(), 100);
        assert_eq!(chunks.concat(), cjk_1500);

        // Tight max_bytes=5 must NEVER overshoot past 5 bytes on 3-byte CJK or 4-byte emoji.
        let cjk_tight = "中中中中";
        let tight_chunks = chunk_text(cjk_tight, 10, 5);
        assert_eq!(tight_chunks.len(), 4);
        for c in &tight_chunks {
            assert!(c.len() <= 5);
        }
        assert_eq!(tight_chunks.concat(), cjk_tight);

        let emoji_tight = "😀😀😀";
        let emoji_chunks = chunk_text(emoji_tight, 10, 5);
        assert_eq!(emoji_chunks.len(), 3);
        for c in &emoji_chunks {
            assert!(c.len() <= 5);
        }
        assert_eq!(emoji_chunks.concat(), emoji_tight);
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
    fn test_voice_reference_validation() {
        assert!(is_valid_voice_reference("7f92f8afb8ec43bf81429cc1c9199cb1"));
        assert!(is_valid_voice_reference("clone_voice-01"));
        assert!(!is_valid_voice_reference("../../etc/passwd"));
        assert!(!is_valid_voice_reference("short"));
        assert!(!is_valid_voice_reference("含中文的音色ID"));
    }

    #[tokio::test]
    async fn test_handle_qq_command_updates_session_and_syncs_help() {
        let (base_url, recorded, server) = spawn_recording_qq_server().await;
        let dir = std::env::temp_dir().join(format!("agygram_qq_cmd_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            ..Config::default()
        };
        let mgr = SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        );
        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let target = QqTarget::C2c("u_test");
        let chat_id = qq_openid_to_chat_id("u_test");

        // /help lists synced xhigh, auto, /skills, /rules
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/help", "m1")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // /effort xhigh and /mode auto work case-insensitively and update session
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/effort XHigh", "m2")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/mode AUTO", "m3")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // /model opus sets model & syncs effort to high; /model reset clears model
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/MODEL opus", "m4")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let session = mgr.get_or_create(chat_id).await;
        assert_eq!(session.model, "claude-opus-5-5-high");
        assert_eq!(session.effort, "high");
        assert_eq!(session.mode, "auto");

        // /voice <question> passes through as a one-turn voice prompt
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            target,
            chat_id,
            "/voice 帮我总结一下",
            "m5",
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            CommandOutcome::ExecutePrompt {
                prompt_text: "帮我总结一下",
                voice_requested: true
            }
        );

        // /voice on/off updates voice_mode
        handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/voice on", "m6")
            .await
            .unwrap();
        assert!(mgr.get_or_create(chat_id).await.voice_mode);

        // Unknown /explain falls through as a prompt
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/explain rust", "m7")
            .await
            .unwrap();
        assert_eq!(
            out,
            CommandOutcome::ExecutePrompt {
                prompt_text: "/explain rust",
                voice_requested: false
            }
        );

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("xhigh"));
        assert!(sent.contains("auto"));
        assert!(sent.contains("/skills"));
        assert!(sent.contains("/rules"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_download_attachments_runs_concurrently_and_preserves_order() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let (status, body) = if req.starts_with("GET /a ") {
                        ("200 OK", "img-a")
                    } else if req.starts_with("GET /b ") {
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

        let (note, failures) = download_attachments(&client, &atts, &dir, "m42").await;
        assert_eq!(failures, vec!["2. broken.txt"]);
        let lines: Vec<_> = note.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("[Attached Image File:") && lines[0].contains("first.png"));
        assert!(lines[1].contains("[Attached File File:") && lines[1].contains("third.pdf"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_placeholder_guard_drop_recalls_message_on_early_exit() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (del_tx, del_rx) = tokio::sync::oneshot::channel::<String>();
        let del_tx = Arc::new(std::sync::Mutex::new(Some(del_tx)));

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let del_tx = del_tx.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let req_line = req.lines().next().unwrap_or("").to_string();
                    let body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#
                    } else {
                        if req_line.starts_with("DELETE ") {
                            if let Some(tx) = del_tx.lock().unwrap().take() {
                                let _ = tx.send(req_line);
                            }
                        }
                        "{}"
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                });
            }
        });

        let base = format!("http://{addr}");
        let client = Arc::new(QqClient::new_for_test("id", "sec", &base, &base));
        {
            let _guard =
                PlaceholderGuard::new(client, QqTarget::C2c("u1"), Some("ph_99".to_string()));
            // Dropped without calling cleanup(), simulating an early `?` return.
        }

        let deleted_req = tokio::time::timeout(Duration::from_secs(2), del_rx)
            .await
            .expect("Drop must spawn async placeholder deletion")
            .unwrap();
        assert!(deleted_req.contains("DELETE /v2/users/u1/messages/ph_99"));

        server.abort();
    }
}
