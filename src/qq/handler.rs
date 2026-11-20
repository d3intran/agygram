use super::client::{MAX_DIRECT_MEDIA_BYTES, QqClient};
use super::context;
use super::formatter::QqFormatter;
use super::types::{
    C2CMessageEvent, GroupMessageEvent, MessageAttachment, MessageReference, MessageScene,
    MsgElement, QqMediaFileType,
};
use crate::artifacts::{
    FILE_DELIVERY_PROMPT_GUIDE, collect_explicit_deliverables_with_brain_async,
    resolve_conversation_brain_dir_async,
};
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent, TokenUsage};
use crate::models::{
    Effort, Mode, QqGroupSessionMode, QuotaPool, SYSTEM_DEFAULT_MODEL_LABEL, effective_cli_effort,
    effort_from_model_suffix, is_safe_cli_value, is_safe_free_value, is_valid_voice_reference,
    list_model_families, model_display_label, refresh_models_now, resolve_model_input,
    sibling_model_for_effort, trigger_background_model_refresh_if_stale,
};
use crate::quota::{QuotaFetcher, QuotaOverview};
use crate::session::{
    ChatSession, SessionManager, TurnGuard, TurnOutcome, legacy_qq_group_member_to_chat_id,
    legacy_qq_group_openid_to_chat_id, legacy_qq_openid_to_chat_id, qq_group_member_to_chat_id,
    qq_group_openid_to_chat_id, qq_openid_to_chat_id, resolve_user_path,
};
use crate::text::{looks_sensitive, safe_attachment_name, truncate_chars};
use anyhow::Result;
use futures_util::stream::{self, StreamExt};
use std::borrow::Cow;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

pub(crate) use super::client::QqTarget;

const QQ_MAX_CHUNK_CHARS: usize = 1400;
const QQ_MAX_CHUNK_BYTES: usize = 4500;
const TURN_QUEUE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_QQ_ATTACHMENTS: usize = 8;
const MAX_QQ_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;
const ATTACHMENT_DOWNLOAD_CONCURRENCY: usize = 4;
const ARTIFACT_UPLOAD_CONCURRENCY: usize = 3;
const MAX_AUTO_ARTIFACTS: usize = 6;
const QQ_TEXT_PREVIEW_READ_BYTES: u64 = 24 * 1024;
const QQ_TEXT_PREVIEW_MAX_CHARS: usize = 1600;

/// QQ is a chat surface, not a document reader: default to short answers. The model also tends
/// to re-issue identical searches seconds apart or poll subagents with `ls`/`manage_subagents`,
/// so those are called out explicitly.
const QQ_DEFAULT_SYSTEM_PROMPT: &str = "你正在通过 QQ 聊天窗口与用户对话。默认保持简洁：先给结论，再按需补充关键要点，尽量控制在 300 字以内；不要复述用户的问题，不要输出过程旁白和客套话；工具结果已足够时立即作答，不要重复相同的搜索或命令；调用异步子代理（如生图）后直接停止调用工具等待回调，勿用 ls/搜索/manage_subagents 轮询空转。只有用户明确要求详细展开时才输出长回答。";

/// Voice turns are read by Fish Audio, which interprets inline `[bracket]` cues for delivery
/// (<https://docs.fish.audio/developer-guide/core-features/emotions>). The cue stays in the text we
/// hand to TTS and is stripped if the turn falls back to a plain text reply.
const QQ_VOICE_STYLE_PROMPT: &str = "本轮将以语音回复。请在回答文本中自然加入 Fish Audio 情感标签：每个句子开头最多一个，例如 [calm]、[happy]、[curious]、[empathetic]，需要强调时用 [emphasis]，需要停顿时用 [break]；最多叠加两个标签，不要解释或提及这些标签本身。";

/// Multi-speaker attribution rules injected when `qq_group_session_mode` is `group` (shared session).
const QQ_GROUP_MULTIUSER_PROMPT: &str = "当前为 QQ 多人群聊共享会话：每条用户消息开头的 `[群聊发言人: ...]` 标明了当前说话的群成员（如 `群成员 #A3F2` 或 `群成员「称呼」(#A3F2)`）。不同 `#XXXX` 编号代表完全不同的群友，请严格区分发言人身份，切勿将当前群友误认为其他群友或此前提供个人账号/凭证的人；若某位群友在对话中告知了自己的名字/称呼，请在上下文中记住该编号对应的称呼。";

#[derive(Clone, Copy, Default)]
struct IncomingMeta<'a> {
    sender_openid: &'a str,
    is_group: bool,
    is_shared_group: bool,
    message_type: Option<u32>,
    message_scene: Option<&'a MessageScene>,
    message_reference: Option<&'a MessageReference>,
    msg_elements: &'a [MsgElement],
}

impl<'a> IncomingMeta<'a> {
    #[inline]
    fn msg_idx(self) -> Option<&'a str> {
        self.message_scene.and_then(MessageScene::msg_idx)
    }

    #[inline]
    fn ref_msg_idx(self) -> Option<&'a str> {
        self.message_scene.and_then(MessageScene::ref_msg_idx)
    }

    #[inline]
    fn has_quote_indicator(self) -> bool {
        self.ref_msg_idx().is_some()
            || self
                .message_reference
                .is_some_and(|r| !r.message_id.trim().is_empty())
            || self.message_type == Some(context::QQ_QUOTE_MESSAGE_TYPE)
    }
}

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

/// RAII guard that ensures `"我想想..."` / queue-notice placeholders are sent without blocking
/// turn startup and are always recalled even if an early `?` return, cancellation, or panic
/// interrupts delivery.
struct PlaceholderGuard {
    client: Arc<QqClient>,
    target: OwnedQqTarget,
    pending_send: Option<tokio::task::JoinHandle<Option<String>>>,
}

impl PlaceholderGuard {
    #[cfg(test)]
    fn new(client: Arc<QqClient>, target: QqTarget<'_>, message_id: Option<String>) -> Self {
        let handle = message_id.map(|id| tokio::spawn(async move { Some(id) }));
        Self {
            client,
            target: OwnedQqTarget::from_target(target),
            pending_send: handle,
        }
    }

    fn spawn(
        client: Arc<QqClient>,
        target: QqTarget<'_>,
        msg_id: &str,
        text: &'static str,
    ) -> Self {
        let owned_target = OwnedQqTarget::from_target(target);
        let client_c = client.clone();
        let target_c = owned_target.clone();
        let msg_id_owned = msg_id.to_string();
        let handle = tokio::spawn(async move {
            reply_ephemeral(&client_c, target_c.as_target(), &msg_id_owned, text)
                .await
                .ok()
                .flatten()
        });
        Self {
            client,
            target: owned_target,
            pending_send: Some(handle),
        }
    }

    /// Schedules asynchronous placeholder recall without delaying reply completion or releasing the
    /// turn slot. Awaits the in-flight placeholder send handle first so a slow placeholder can
    /// never land after the final reply without being recalled.
    fn spawn_cleanup(mut self) {
        self.schedule_recall();
    }

    fn schedule_recall(&mut self) {
        let Some(handle) = self.pending_send.take() else {
            return;
        };
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let client = self.client.clone();
            let target = self.target.clone();
            rt.spawn(async move {
                let p_id = match tokio::time::timeout(Duration::from_secs(3), handle).await {
                    Ok(Ok(id)) => id,
                    _ => None,
                };
                if let Some(id) = p_id
                    && let Err(err) = client.delete_message(target.as_target(), &id).await
                {
                    debug!(error = %err, "Failed to recall QQ placeholder message");
                }
            });
        }
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        self.schedule_recall();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum CommandOutcome<'a> {
    Handled,
    ExecutePrompt {
        prompt_text: Cow<'a, str>,
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

    let meta = IncomingMeta {
        sender_openid: openid,
        is_group: false,
        is_shared_group: false,
        message_type: event.message_type,
        message_scene: event.message_scene.as_ref(),
        message_reference: event.message_reference.as_ref(),
        msg_elements: &event.msg_elements,
    };

    if let Err(err) = process_message(
        client,
        config,
        session_mgr,
        QqTarget::C2c(openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
        meta,
    )
    .await
    {
        error!(chat_id, error = %format!("{err:#}"), "Error processing QQ C2C message");
    }
}

/// Passively caches full-group messages (`GROUP_MESSAGE_CREATE`, when enabled in QQ bot settings)
/// so later `@Bot` quote-replies can resolve un-`@`ed messages and attachments without triggering AI turns.
pub(crate) fn handle_group_passive(config: &Config, event: GroupMessageEvent) {
    let sender_openid = event.author.member_openid.trim();
    let group_openid = event.group_openid.trim();
    if !config.is_qq_group_allowed(group_openid, sender_openid) {
        return;
    }

    let scope = QqTarget::Group(group_openid).scope_key();
    let effective = context::extract_effective_content(
        &event.content,
        event.message_scene.as_ref(),
        &event.msg_elements,
    );
    let msg_idx = event.message_scene.as_ref().and_then(MessageScene::msg_idx);
    let speaker = context::format_group_speaker(sender_openid);

    context::record_message(
        &scope,
        Some(&event.id),
        msg_idx,
        speaker,
        effective,
        Vec::new(),
        event.attachments,
    );
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

    let mode = config.qq_group_session_mode_enum();
    let (chat_id, legacy_id) = match mode {
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

    let meta = IncomingMeta {
        sender_openid,
        is_group: true,
        is_shared_group: mode == QqGroupSessionMode::Group,
        message_type: event.message_type,
        message_scene: event.message_scene.as_ref(),
        message_reference: event.message_reference.as_ref(),
        msg_elements: &event.msg_elements,
    };

    if let Err(err) = process_message(
        client,
        config,
        session_mgr,
        QqTarget::Group(group_openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
        meta,
    )
    .await
    {
        error!(chat_id, error = %format!("{err:#}"), "Error processing QQ Group message");
    }
}

async fn reply_ephemeral(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    // Ephemeral status placeholders ("我想想...", queue notice) are short plain text; bypass
    // Markdown formatting/probing so they complete in a single RTT.
    client.send_plain_message(target, text, msg_id).await
}

async fn reply(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    let formatted = QqFormatter::format_markdown(text);
    let scope = target.scope_key();
    // QQ messages are limited to ~1500-2000 chars per message; fence-aware chunking preserves code blocks & box tables.
    let chunks =
        QqFormatter::split_markdown_chunks(&formatted, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
    let mut last_id = None;
    for chunk in chunks {
        let sent_meta = client
            .send_message_with_meta(target, &chunk, msg_id)
            .await?;
        context::record_message(
            &scope,
            sent_meta.id.as_deref(),
            sent_meta.ref_idx(),
            "AI 助手",
            &chunk,
            Vec::new(),
            Vec::new(),
        );
        if sent_meta.id.is_some() {
            last_id = sent_meta.id;
        }
    }
    Ok(last_id)
}

fn effective_system_prompt(custom: &str, voice: bool, is_shared_group: bool) -> String {
    let custom = custom.trim();
    let mut prompt = String::from(QQ_DEFAULT_SYSTEM_PROMPT);
    if is_shared_group {
        prompt.push('\n');
        prompt.push_str(QQ_GROUP_MULTIUSER_PROMPT);
    }
    prompt.push('\n');
    prompt.push_str(FILE_DELIVERY_PROMPT_GUIDE);
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
    meta: IncomingMeta<'_>,
) -> Result<()> {
    let content =
        context::extract_effective_content(raw_content, meta.message_scene, meta.msg_elements);
    // An image or quoted message sent without text is a valid prompt; only drop messages with none.
    if content.is_empty() && attachments.is_empty() && !meta.has_quote_indicator() {
        return Ok(());
    }

    let speaker_tag = context::member_short_tag(meta.sender_openid);
    info!(
        target_kind = target.kind_label(),
        chat_id,
        speaker = %speaker_tag,
        msg_len = content.len(),
        attachment_count = attachments.len(),
        msg_type = ?meta.message_type,
        msg_idx = meta.msg_idx().unwrap_or("-"),
        ref_msg_idx = meta.ref_msg_idx().unwrap_or("-"),
        msg_elements = meta.msg_elements.len(),
        "Received QQ message"
    );

    if handle_sender_identity_command(&client, target, meta.sender_openid, content, msg_id).await? {
        return Ok(());
    }

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
                prompt_text.as_ref(),
                voice_requested,
                msg_id,
                attachments,
                meta,
            )
            .await
        }
    }
}

async fn handle_sender_identity_command(
    client: &QqClient,
    target: QqTarget<'_>,
    sender_openid: &str,
    content: &str,
    msg_id: &str,
) -> Result<bool> {
    if !content.starts_with('/') {
        return Ok(false);
    }
    let cmd_raw = content.split_whitespace().next().unwrap_or_default();
    if !cmd_raw.eq_ignore_ascii_case("/nick")
        && !cmd_raw.eq_ignore_ascii_case("/whoami")
        && !cmd_raw.eq_ignore_ascii_case("/name")
    {
        return Ok(false);
    }

    let arg = content
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim());
    let tag = context::member_short_tag(sender_openid);

    if arg.is_empty() || cmd_raw.eq_ignore_ascii_case("/whoami") {
        let current = context::get_member_nickname(sender_openid);
        let status = match current {
            Some(nick) => format!("当前已绑定称呼: **「{nick}」**"),
            None => "当前未绑定自定义称呼".to_string(),
        };
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🪪 **你的身份标识**: `{tag}`（{status}）\n\
                • 发送 `/nick <称呼>` 绑定固定称呼（例如 `/nick 阿杰`），群聊中 AI 将按名字区分你与其他群友\n\
                • 发送 `/nick clear` 清除自定义称呼"
            ),
        )
        .await?;
        return Ok(true);
    }

    if arg.eq_ignore_ascii_case("clear")
        || arg.eq_ignore_ascii_case("reset")
        || arg.eq_ignore_ascii_case("off")
    {
        context::set_member_nickname(sender_openid, None);
        reply(
            client,
            target,
            msg_id,
            &format!("🧹 已清除自定义称呼，恢复默认群聊标识 `{tag}`。"),
        )
        .await?;
        return Ok(true);
    }

    match context::sanitize_nickname(arg) {
        Ok(clean) => {
            context::set_member_nickname(sender_openid, Some(clean.clone()));
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "✅ 已将你的身份称呼绑定为 **「{clean}」** (`{tag}`)！后续群聊中 AI 将自动区分你的身份。"
                ),
            )
            .await?;
        }
        Err(reason) => {
            reply(client, target, msg_id, &format!("⚠️ {reason}")).await?;
        }
    }
    Ok(true)
}

fn build_qq_help_text() -> String {
    format!(
        "🚀 **agygram QQ 机器人已就绪** (v{})\n\n\
        **🎛️ 控制板与会话：**\n\
        • `/panel` 或 `/status` - 查看控制板、实时额度与快捷序号\n\
        • `/new` / `/reset` / `/clear` - 重置会话上下文与统计\n\
        • `/compact [<侧重点>]` - 压缩整理当前上下文摘要\n\
        • `/resume [<ID>]` / `/history` - 查看或恢复历史会话\n\
        • `/nick [<称呼>|clear]` - 绑定群聊个人称呼（让 AI 在群里精准认识你）\n\
        • `/quota` 或 `/usage` - 查询 Google Cloud 实时模型额度\n\
        • `/cancel` - 中断当前正在执行的任务\n\n\
        **🤖 模型与执行参数：**\n\
        • `/model [<序号|名称|别名>]` - 切换模型 (如 `1`, `h2`, `carbon`, `orch`, `opus`, `reset`)\n\
        • `/models` - 查看完整分组模型与序号列表\n\
        • `/effort [<级别>]` - 思考力度 (`low` / `medium` / `high` / `xhigh` / `max`)\n\
        • `/mode [<模式>]` - 执行模式 (`auto` / `accept-edits` / `plan`)\n\
        • `/agent [<名称>]` / `/agents` - 切换或查看 Agent\n\
        • `/sandbox [on|off|toggle]` - 开关沙盒隔离\n\
        • `/project [<名称>|clear]` - 绑定或清空 Project\n\
        • `/system [<Prompt>|clear]` - 设置或清空自定义提示词\n\n\
        **📂 工作区与文件：**\n\
        • `/workspace [<路径>]` - 查看或切换主目录 (兼容 `/cd`, `/pwd`, `/dirs`)\n\
        • `/adddir <路径>` / `/cleardirs` - 添加或清空附加项目目录\n\
        • `/file <路径>` - 发送 VPS 上的图片/音视频/文件或文本预览\n\n\
        **🧩 生态与语音：**\n\
        • `/skills` / `/rules` - 查看 Antigravity 技能库与规则\n\
        • `/voice [on|off|toggle|id <ID>|<问题>]` - 语音开关或单次语音回复\n\n\
        直接发送任何开发需求、问题或图片/文件，即可唤起 Antigravity CLI 执行。",
        env!("CARGO_PKG_VERSION")
    )
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
            prompt_text: Cow::Borrowed(content),
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
            reply(client, target, msg_id, &build_qq_help_text()).await?;
            Ok(CommandOutcome::Handled)
        }
        "/panel" | "/status" => {
            handle_panel_command(client, session_mgr, target, chat_id, msg_id).await?;
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
        "/compact" => {
            handle_compact_command(client, session_mgr, target, chat_id, arg, msg_id).await
        }
        "/resume" => {
            handle_resume_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/history" => {
            handle_history_command(client, target, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/usage" | "/quota" => {
            match QuotaFetcher::format_models_and_quota_plain().await {
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
            handle_models_list_command(client, session_mgr, target, chat_id, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/model" => {
            handle_model_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
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
        "/effort" => {
            handle_effort_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/mode" => {
            handle_mode_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/agent" => {
            handle_agent_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/agents" => {
            let agents = AgyExecutor::list_agents().await.unwrap_or_default();
            let body = if agents.trim().is_empty() {
                "暂无可用 Agent。".to_string()
            } else {
                format!("🕵️ **可用 Agent 列表：**\n```\n{}\n```", agents.trim())
            };
            reply(client, target, msg_id, &body).await?;
            Ok(CommandOutcome::Handled)
        }
        "/sandbox" => {
            handle_sandbox_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/project" => {
            handle_project_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/system" => {
            handle_system_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/workspace" => {
            handle_workspace_command(client, session_mgr, target, chat_id, arg, msg_id, false)
                .await?;
            Ok(CommandOutcome::Handled)
        }
        "/cd" => {
            handle_workspace_command(client, session_mgr, target, chat_id, arg, msg_id, true)
                .await?;
            Ok(CommandOutcome::Handled)
        }
        "/pwd" | "/dirs" => {
            let session = session_mgr.get_or_create(chat_id).await;
            reply(client, target, msg_id, &format_dirs_text(&session)).await?;
            Ok(CommandOutcome::Handled)
        }
        "/adddir" => {
            if arg.is_empty() {
                reply(
                    client,
                    target,
                    msg_id,
                    "⚠️ 请提供附加目录路径，例如 `/adddir ~/repo`",
                )
                .await?;
            } else {
                let session = session_mgr.get_or_create(chat_id).await;
                let resolved = resolve_user_path(arg, &session.workspace);
                let path_str = resolved.to_string_lossy().into_owned();
                session_mgr.add_extra_dir(chat_id, path_str.clone()).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("📂 已添加附加工作目录：\n`{path_str}`"),
                )
                .await?;
            }
            Ok(CommandOutcome::Handled)
        }
        "/cleardirs" => {
            session_mgr.clear_extra_dirs(chat_id).await;
            reply(client, target, msg_id, "🧹 已清空所有附加工作目录。").await?;
            Ok(CommandOutcome::Handled)
        }
        "/file" => {
            handle_file_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
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
            prompt_text: Cow::Borrowed(content),
            voice_requested: false,
        }),
    }
}

fn format_dirs_text(session: &ChatSession) -> String {
    let mut out = format!("📂 **当前工作目录：**\n• 主目录: `{}`", session.workspace);
    if !session.extra_dirs.is_empty() {
        out.push_str("\n• 附加目录:");
        for dir in &session.extra_dirs {
            let _ = write!(out, "\n  - `{dir}`");
        }
    }
    out
}

fn build_qq_panel_text(session: &ChatSession, overview: Option<&QuotaOverview>) -> String {
    let conv_id = session.conversation_id.as_deref().unwrap_or("新建会话");
    let model_disp = model_display_label(&session.model);
    let effort_note = match effective_cli_effort(&session.model, &session.effort) {
        Some(eff) if eff != session.effort => format!("{} (实际: {eff})", session.effort),
        Some(eff) => eff.to_string(),
        None => format!("{} (由模型固定/忽略 --effort)", session.effort),
    };
    let agent_disp = if session.agent.is_empty() {
        "默认"
    } else {
        session.agent.as_str()
    };
    let project_disp = if session.project.is_empty() {
        "默认"
    } else {
        session.project.as_str()
    };

    let mut out = format!(
        "🎛️ **agygram 控制面板**\n\n\
        • 会话 ID: `{conv_id}`\n\
        • 工作目录: `{}` (附加 {} 个)\n\
        • 当前模型: **{model_disp}**\n\
        • 思考力度: `{effort_note}` | 模式: `{}`\n\
        • 沙盒: {} | Agent: `{agent_disp}` | 工程: `{project_disp}`\n\
        • 语音模式: {} | 轮次: {} | Token: {} in / {} out\n",
        session.workspace,
        session.extra_dirs.len(),
        session.mode,
        if session.sandbox {
            "已开启"
        } else {
            "已关闭"
        },
        if session.voice_mode {
            "已开启"
        } else {
            "已关闭"
        },
        session.turn_count,
        session.total_input_tokens,
        session.total_output_tokens
    );

    if let Some(ov) = overview {
        out.push_str("\n📊 **实时额度概览：**");
        if let Some(g) = ov.compact_badge(QuotaPool::Gemini) {
            let _ = write!(out, "\n• Gemini: `{g}`");
        }
        if let Some(tp) = ov.compact_badge(QuotaPool::ThirdParty) {
            let _ = write!(out, "\n• Claude & GPT: `{tp}`");
        }
        out.push('\n');
    }

    out.push_str("\n🔢 **快捷模型编号 (`/model <序号>`):**\n");
    append_compact_numbered_models(&mut out, &session.model);
    out.push_str(
        "\n💡 `/model reset` 恢复默认 · `/workspace <路径>` 切目录 · `/compact` 压缩 · `/new` 新建",
    );
    out
}

fn append_compact_numbered_models(out: &mut String, active_model: &str) {
    let families = list_model_families();
    let mut idx = 1usize;
    for family in families {
        let _ = write!(out, "{} **{}**: ", family.icon, family.title);
        for (i, entry) in family.models.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            let active = !active_model.is_empty() && entry.id.eq_ignore_ascii_case(active_model);
            let mark = if active { "✅" } else { "" };
            let dyn_tag = if entry.is_dynamic { "*" } else { "" };
            let _ = write!(out, "`{idx}`.{}{dyn_tag}{mark}", entry.variant_label);
            idx += 1;
        }
        out.push('\n');
    }
}

fn build_qq_models_list_text(
    active_model: &str,
    overview: Option<&QuotaOverview>,
    compact: bool,
) -> String {
    let current_disp = model_display_label(active_model);
    let mut out = format!("📋 **可用 AI 模型列表** (当前: **{current_disp}**)\n\n");
    let families = list_model_families();
    let mut idx = 1usize;

    for family in families {
        let badge = overview
            .and_then(|ov| ov.compact_badge(family.quota_pool))
            .map(|b| format!(" [{b}]"))
            .unwrap_or_default();
        let _ = writeln!(out, "{} **{}**{badge}", family.icon, family.title);
        for entry in family.models {
            let active = !active_model.is_empty() && entry.id.eq_ignore_ascii_case(active_model);
            let check = if active { " ✅" } else { "" };
            let dyn_badge = if entry.is_dynamic { " [动态]" } else { "" };
            if compact {
                let _ = writeln!(
                    out,
                    "  `{idx}`. {} (`{}`){dyn_badge}{check}",
                    entry.variant_label, entry.id
                );
            } else {
                let _ = writeln!(
                    out,
                    "  `{idx}`. **{}** — `{}`{dyn_badge}{check}",
                    entry.label, entry.id
                );
            }
            idx += 1;
        }
    }

    let _ = write!(
        out,
        "\n💡 发送 `/model <序号|ID|别名>` 切换（如 `/model 1` 或 `/model argon`），`/model reset` 恢复默认 ({SYSTEM_DEFAULT_MODEL_LABEL})。"
    );
    out
}

async fn handle_panel_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
) -> Result<()> {
    trigger_background_model_refresh_if_stale();
    let ((overview, _), session) = tokio::join!(
        QuotaFetcher::peek_overview_and_revalidate(),
        session_mgr.get_or_create(chat_id),
    );
    let panel = build_qq_panel_text(&session, overview.as_ref());
    reply(client, target, msg_id, &panel).await?;
    Ok(())
}

async fn handle_models_list_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
) -> Result<()> {
    let (_, (overview, _), session) = tokio::join!(
        refresh_models_now(),
        QuotaFetcher::peek_overview_and_revalidate(),
        session_mgr.get_or_create(chat_id),
    );
    let text = build_qq_models_list_text(&session.model, overview.as_ref(), false);
    reply(client, target, msg_id, &text).await?;
    Ok(())
}

fn build_compact_prompt(focus: &str) -> String {
    let focus = focus.trim();
    if focus.is_empty() {
        "请将当前会话迄今为止的目标、已完成事项、关键文件与结论、以及待办事项压缩整理为一份精炼的检查点摘要（控制在 250 字以内，条理清晰），便于后续继续工作。".to_string()
    } else {
        format!(
            "请围绕「{focus}」将当前会话至今的目标、关键进展、相关文件路径与待办事项压缩整理为一份精炼的检查点摘要（控制在 250 字以内）。"
        )
    }
}

async fn handle_compact_command<'a>(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &'a str,
    msg_id: &str,
) -> Result<CommandOutcome<'a>> {
    let session = session_mgr.get_or_create(chat_id).await;
    if session.conversation_id.is_none() && session.is_first_turn {
        reply(
            client,
            target,
            msg_id,
            "ℹ️ 当前为全新会话，暂无上下文可压缩。直接发送问题或使用 `/new` 重置即可。",
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    Ok(CommandOutcome::ExecutePrompt {
        prompt_text: Cow::Owned(build_compact_prompt(arg)),
        voice_requested: false,
    })
}

async fn handle_resume_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let recents = AgyExecutor::list_recent_conversations(8).await;
        if recents.is_empty() {
            session_mgr.resume_conversation(chat_id, None).await;
            reply(
                client,
                target,
                msg_id,
                "🔄 未扫描到历史会话文件，已开启接续模式 (`-c`)；下一条消息将接续最近一次 agy 上下文。",
            )
            .await?;
        } else {
            let mut out = String::from("🔄 **最近历史会话：**\n");
            for (i, item) in recents.iter().enumerate() {
                let title = truncate_chars(&item.title, 42);
                let _ = writeln!(
                    out,
                    "{}. `{}` — {} ({})",
                    i + 1,
                    item.id,
                    title,
                    item.updated_time
                );
            }
            out.push_str("\n发送 `/resume <会话ID>` 绑定并继续。");
            reply(client, target, msg_id, &out).await?;
        }
        return Ok(());
    }

    if !is_safe_cli_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的会话 ID：不能以 `-` 开头，仅允许字母、数字及 `_ - . +`。",
        )
        .await?;
        return Ok(());
    }

    session_mgr
        .resume_conversation(chat_id, Some(arg.to_string()))
        .await;
    reply(
        client,
        target,
        msg_id,
        &format!("🔄 已绑定历史会话 ID: `{arg}`，下一条消息将接续该上下文。"),
    )
    .await?;
    Ok(())
}

async fn handle_history_command(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
) -> Result<()> {
    let recents = AgyExecutor::list_recent_conversations(8).await;
    if recents.is_empty() {
        reply(client, target, msg_id, "📜 暂未发现历史会话。").await?;
        return Ok(());
    }
    let mut out = String::from("📜 **VPS 最近活跃会话：**\n");
    for (i, item) in recents.iter().enumerate() {
        let title = truncate_chars(&item.title, 42);
        let _ = writeln!(
            out,
            "{}. `{}` — {} ({})",
            i + 1,
            item.id,
            title,
            item.updated_time
        );
    }
    out.push_str("\n发送 `/resume <会话ID>` 即可接续。");
    reply(client, target, msg_id, &out).await?;
    Ok(())
}

async fn handle_model_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    trigger_background_model_refresh_if_stale();

    if arg.is_empty() {
        let (session, (overview, _)) = tokio::join!(
            session_mgr.get_or_create(chat_id),
            QuotaFetcher::peek_overview_and_revalidate(),
        );
        let text = build_qq_models_list_text(&session.model, overview.as_ref(), true);
        reply(client, target, msg_id, &text).await?;
        return Ok(());
    }

    let resolved = if let Some(found) = resolve_model_input(arg) {
        Some(found)
    } else {
        let _ = refresh_models_now().await;
        if let Some(found) = resolve_model_input(arg) {
            Some(found)
        } else if is_safe_cli_value(arg) {
            AgyExecutor::list_models().await.ok().and_then(|raw| {
                raw.lines()
                    .filter_map(|line| line.split_whitespace().next())
                    .find(|id| id.eq_ignore_ascii_case(arg))
                    .map(str::to_string)
            })
        } else {
            None
        }
    };

    let Some(target_model) = resolved else {
        reply(
            client,
            target,
            msg_id,
            &format!(
                "⚠️ 无效的模型名称或序号: `{arg}`\n发送 `/models` 或 `/model` 查看完整序号列表。"
            ),
        )
        .await?;
        return Ok(());
    };

    session_mgr.set_model(chat_id, target_model.clone()).await;
    if let Some(effort) = effort_from_model_suffix(&target_model) {
        session_mgr.set_effort_enum(chat_id, effort).await;
    }

    let updated = session_mgr.get_or_create(chat_id).await;
    let display = model_display_label(&target_model);
    let effort_status = match effective_cli_effort(&target_model, &updated.effort) {
        Some(eff) => format!("思考力度: `{eff}`"),
        None => format!("思考力度: `{}` (由该模型固定)", updated.effort),
    };
    let id_suffix = if target_model.is_empty() {
        String::new()
    } else {
        format!(" (`{target_model}`)")
    };

    reply(
        client,
        target,
        msg_id,
        &format!("✅ AI 模型已切换为: **{display}**{id_suffix}\n⚡ {effort_status}"),
    )
    .await?;
    Ok(())
}

async fn handle_effort_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!(
                "⚡ 当前思考力度: `{}`\n可选: `low`, `medium`, `high`, `xhigh`, `max`",
                session.effort
            ),
        )
        .await?;
        return Ok(());
    }

    let Ok(effort) = arg.parse::<Effort>() else {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的思考力度。可选: `low`, `medium`, `high`, `xhigh`, `max`",
        )
        .await?;
        return Ok(());
    };

    let session = session_mgr.get_or_create(chat_id).await;
    session_mgr.set_effort_enum(chat_id, effort).await;

    let note = if effective_cli_effort(&session.model, effort.as_str()).is_none() {
        if let Some(sibling) = sibling_model_for_effort(&session.model, effort) {
            format!(
                "\n💡 当前模型 `{}` 自带思考档位，如需直接切换对应变体可发送 `/model {sibling}`。",
                session.model
            )
        } else {
            format!(
                "\n💡 注意: 当前模型 `{}` 不接受 `--effort` 参数，将使用模型固定思考级别。",
                session.model
            )
        }
    } else {
        String::new()
    };

    reply(
        client,
        target,
        msg_id,
        &format!("✅ 思考力度已设置为: `{effort}`{note}"),
    )
    .await?;
    Ok(())
}

async fn handle_mode_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🛠️ 当前执行模式: `{}`\n可选: `auto`, `accept-edits`, `plan`",
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
            &format!("✅ 执行模式已设置为: `{mode}`"),
        )
        .await?;
    } else {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的执行模式。可选: `auto`, `accept-edits`, `plan`",
        )
        .await?;
    }
    Ok(())
}

async fn handle_agent_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let (session, agents) = tokio::join!(session_mgr.get_or_create(chat_id), async {
            AgyExecutor::list_agents().await.unwrap_or_default()
        },);
        let current = if session.agent.is_empty() {
            "默认 Agent"
        } else {
            &session.agent
        };
        let list_section = if agents.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n可用列表：\n```\n{}\n```", agents.trim())
        };
        reply(
            client,
            target,
            msg_id,
            &format!("🕵️ 当前 Agent: `{current}`{list_section}\n使用 `/agent <名称>` 切换，`/agent reset` 恢复默认。"),
        )
        .await?;
    } else if arg.eq_ignore_ascii_case("default")
        || arg.eq_ignore_ascii_case("reset")
        || arg.eq_ignore_ascii_case("clear")
    {
        session_mgr.set_agent(chat_id, String::new()).await;
        reply(client, target, msg_id, "🕵️ 已恢复默认 Agent。").await?;
    } else if !is_safe_free_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的 Agent 名称：不能以 `-` 开头或含控制字符，上限 128 字符。",
        )
        .await?;
    } else {
        session_mgr.set_agent(chat_id, arg.to_string()).await;
        reply(
            client,
            target,
            msg_id,
            &format!("🕵️ Agent 已切换为: `{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_sandbox_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    let lower = arg.to_ascii_lowercase();
    let enabled = match lower.as_str() {
        "on" | "true" | "1" | "enable" => {
            session_mgr.set_sandbox(chat_id, true).await;
            true
        }
        "off" | "false" | "0" | "disable" => {
            session_mgr.set_sandbox(chat_id, false).await;
            false
        }
        _ => session_mgr.toggle_sandbox(chat_id).await,
    };
    let msg = if enabled {
        "🛡️ 沙盒隔离模式已开启 (`--sandbox`)"
    } else {
        "⚡ 沙盒隔离模式已关闭"
    };
    reply(client, target, msg_id, msg).await?;
    Ok(())
}

async fn handle_project_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let cur = if session.project.is_empty() {
            "默认工程 (未指定)"
        } else {
            &session.project
        };
        reply(
            client,
            target,
            msg_id,
            &format!("📁 当前工程: `{cur}`\n使用 `/project <名称>` 绑定，`/project clear` 清空。"),
        )
        .await?;
    } else if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("reset") {
        session_mgr.set_project(chat_id, String::new()).await;
        reply(client, target, msg_id, "🔄 已重置 Project 绑定。").await?;
    } else if !is_safe_free_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的 Project 名称：不能以 `-` 开头或含控制字符，上限 128 字符。",
        )
        .await?;
    } else {
        session_mgr.set_project(chat_id, arg.to_string()).await;
        reply(
            client,
            target,
            msg_id,
            &format!("📁 已绑定 Project: `{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_system_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        if session.system_prompt.is_empty() {
            reply(
                client,
                target,
                msg_id,
                "🧠 当前未设置自定义 System Prompt。\n发送 `/system <内容>` 设置，`/system clear` 清空。",
            )
            .await?;
        } else {
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "🧠 当前 System Prompt:\n```\n{}\n```\n发送 `/system clear` 可清空。",
                    session.system_prompt
                ),
            )
            .await?;
        }
    } else if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("reset") {
        session_mgr.set_system_prompt(chat_id, String::new()).await;
        reply(client, target, msg_id, "🧠 System Prompt 已清空。").await?;
    } else {
        session_mgr
            .set_system_prompt(chat_id, arg.to_string())
            .await;
        reply(
            client,
            target,
            msg_id,
            &format!("🧠 System Prompt 已更新：\n`{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_workspace_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
    require_arg: bool,
) -> Result<()> {
    if arg.is_empty() {
        if require_arg {
            reply(
                client,
                target,
                msg_id,
                "⚠️ 请提供目标目录路径，例如 `/cd ~/project` 或 `/workspace ~/project`",
            )
            .await?;
        } else {
            let session = session_mgr.get_or_create(chat_id).await;
            let mut text = format_dirs_text(&session);
            text.push_str("\n\n用法: `/workspace <路径>` 切换主目录；`/adddir <路径>` 添加附加目录；`/cleardirs` 清空附加目录");
            reply(client, target, msg_id, &text).await?;
        }
        return Ok(());
    }

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
        reply(client, target, msg_id, &format!("{prefix}:\n`{path_str}`")).await?;
    } else {
        reply(
            client,
            target,
            msg_id,
            &format!("❌ 无法切换工作目录: 路径 `{arg}` 无效或权限不足。"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_file_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 请提供 VPS 文件路径，例如 `/file scratch/report.pdf` 或 `/file Cargo.toml`",
        )
        .await?;
        return Ok(());
    }

    let session = session_mgr.get_or_create(chat_id).await;
    let file_path = resolve_user_path(arg, &session.workspace);
    match tokio::fs::metadata(&file_path).await {
        Ok(meta) if meta.is_file() => {
            deliver_single_file(client, target, msg_id, &file_path, meta.len()).await;
        }
        Ok(_) => {
            reply(
                client,
                target,
                msg_id,
                &format!("❌ `{}` 不是普通文件，无法发送。", file_path.display()),
            )
            .await?;
        }
        Err(_) => {
            reply(
                client,
                target,
                msg_id,
                &format!("❌ 文件不存在或无法读取: `{arg}`"),
            )
            .await?;
        }
    }
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
        prompt_text: Cow::Borrowed(arg),
        voice_requested: true,
    })
}

async fn resolve_quote_prompt_block(
    client: &QqClient,
    scope_key: &str,
    meta: IncomingMeta<'_>,
    prompt_text: &str,
    scratch_dir: &Path,
    msg_id: &str,
) -> String {
    let extracted = context::extract_quote_context(
        scope_key,
        meta.is_group,
        prompt_text,
        meta.message_type,
        meta.message_scene,
        meta.message_reference,
        meta.msg_elements,
    );
    if !extracted.had_quote_indicator
        && extracted.text.is_empty()
        && extracted.local_files.is_empty()
        && extracted.remote_attachments.is_empty()
    {
        return String::new();
    }

    let mut existing_local = Vec::new();
    for p in &extracted.local_files {
        if tokio::fs::try_exists(p).await.unwrap_or(false) && !existing_local.contains(p) {
            existing_local.push(p.clone());
        }
    }

    let mut downloaded_note = String::new();
    if existing_local.is_empty() && !extracted.remote_attachments.is_empty() {
        let quote_msg_key = format!("quote-{msg_id}");
        let (note, saved_paths, _) = download_attachments_with_paths(
            client,
            &extracted.remote_attachments,
            scratch_dir,
            &quote_msg_key,
            "Quoted",
        )
        .await;
        if let Some(ref cache_key) = extracted.cache_key {
            context::attach_downloaded_files_to_cached(scope_key, cache_key, &saved_paths);
        }
        downloaded_note = note;
    }

    context::format_quote_prompt_block(&extracted, &existing_local, &downloaded_note)
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
    meta: IncomingMeta<'_>,
) -> Result<()> {
    // Claim the turn atomically before emitting the thinking placeholder to prevent TOCTOU
    // duplicate placeholders under concurrent messages. The placeholder HTTP call is spawned
    // concurrently so it never adds a network RTT before attachment downloads or `agy` startup.
    let (generation, placeholder_guard) =
        if let Some(turn_gen) = session_mgr.begin_turn(chat_id).await {
            let guard = PlaceholderGuard::spawn(client.clone(), target, msg_id, "我想想...");
            (turn_gen, guard)
        } else {
            let guard = PlaceholderGuard::spawn(
                client.clone(),
                target,
                msg_id,
                "⏳ 当前会话已有任务正在运行，已加入等待队列，即将自动处理...",
            );

            let Some(turn_gen) = session_mgr
                .begin_turn_with_timeout(chat_id, TURN_QUEUE_TIMEOUT)
                .await
            else {
                guard.spawn_cleanup();
                reply(
                    &client,
                    target,
                    msg_id,
                    "⚠️ 排队超时：上一任务执行时间过长，请稍后重试或发送 /cancel 强行中断任务。",
                )
                .await?;
                return Ok(());
            };
            (turn_gen, guard)
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

    let scope_key = target.scope_key();
    let speaker_label = if meta.is_group {
        context::format_group_speaker(meta.sender_openid)
    } else {
        format!("用户 {}", context::member_short_tag(meta.sender_openid))
    };

    // Download inbound attachments and resolve any quoted message/media concurrently.
    let ((attachment_note, saved_paths, failed_attachments), quote_block) = tokio::join!(
        download_attachments_with_paths(&client, attachments, &scratch_dir, msg_id, "Attached"),
        resolve_quote_prompt_block(&client, &scope_key, meta, prompt_text, &scratch_dir, msg_id),
    );

    // Record the current inbound turn into the ring buffer so later quote-replies to it succeed.
    context::record_message(
        &scope_key,
        Some(msg_id),
        meta.msg_idx(),
        speaker_label.clone(),
        prompt_text,
        saved_paths,
        attachments.to_vec(),
    );

    let mut prompt_owned =
        String::with_capacity(quote_block.len() + attachment_note.len() + prompt_text.len() + 64);
    if meta.is_shared_group {
        let _ = writeln!(prompt_owned, "[群聊发言人: {speaker_label}]");
    }
    if !quote_block.is_empty() {
        prompt_owned.push_str(&quote_block);
    }
    if !attachment_note.is_empty() {
        prompt_owned.push_str(&attachment_note);
    }
    if prompt_text.is_empty() {
        if !attachment_note.is_empty() {
            prompt_owned.push_str("请查看我随消息发送的附件并作出回应。");
        } else {
            prompt_owned.push_str("请结合我引用的消息内容作出回应。");
        }
    } else {
        prompt_owned.push_str(prompt_text);
    }
    let input_chars = prompt_owned.len();
    let system_prompt = effective_system_prompt(
        &session.system_prompt,
        should_send_voice,
        meta.is_shared_group,
    );

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
                        StreamEvent::Result(res) => {
                            if res.conversation_id.is_some() {
                                captured_conv_id = res.conversation_id;
                            }
                            if !res.response.is_empty() {
                                accumulated_text = res.response;
                            }
                            captured_usage = res.usage;
                            captured_duration = res.duration_seconds;
                            // Break immediately rather than draining to EOF so detached background
                            // Break immediately rather than draining to EOF so detached background
                            // runs do not hang the QQ turn; `stream_rx` is returned so detached
                            // follow-up turns can still be delivered asynchronously.
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
        stream_rx
    };

    let (exec_res, stream_rx) = tokio::join!(exec_fut, ingest_fut);
    let duration = captured_duration.unwrap_or_else(|| start_time.elapsed().as_secs_f64());

    match exec_res {
        Ok(res) => {
            let is_detached = res.detached;
            let final_text = if !accumulated_text.trim().is_empty() {
                accumulated_text
            } else if !res.output.trim().is_empty() {
                res.output
            } else if let Some(err) = stream_error {
                format!("⚠️ {err}")
            } else {
                "✅ 任务执行完毕 (无文本输出)".to_string()
            };

            let resolved_conv_id = captured_conv_id
                .or(res.conversation_id)
                .or_else(|| session.conversation_id.clone());

            let outcome = TurnOutcome {
                conversation_id: resolved_conv_id.clone(),
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
                let active_brain_dir =
                    resolve_conversation_brain_dir_async(resolved_conv_id.as_deref()).await;
                deliver_qq_reply(
                    &client,
                    &config,
                    target,
                    chat_id,
                    msg_id,
                    prompt_text,
                    &final_text,
                    &failed_attachments,
                    should_send_voice,
                    effective_voice_ref,
                    &artifact_roots,
                    active_brain_dir.as_deref(),
                )
                .await?;

                if is_detached {
                    let followup = QqFollowupParams {
                        client: client.clone(),
                        config: config.clone(),
                        session_mgr: session_mgr.clone(),
                        target: OwnedQqTarget::from_target(target),
                        chat_id,
                        generation,
                        msg_id: msg_id.to_string(),
                        user_prompt: prompt_text.to_string(),
                        should_send_voice,
                        effective_voice_ref: effective_voice_ref.map(str::to_string),
                        artifact_roots,
                        conversation_id: resolved_conv_id,
                        stream_rx,
                    };
                    tokio::spawn(stream_qq_detached_followups(followup));
                }
            }

            placeholder_guard.spawn_cleanup();
        }
        Err(err) => {
            error!(chat_id, error = %format!("{err:#}"), "Execution failed in QQ chat");
            let is_current = session_mgr.is_generation_current(chat_id, generation).await;
            session_mgr.release_turn(chat_id, generation).await;
            turn_guard.disarm();

            if is_current {
                reply(&client, target, msg_id, &format!("❌ 执行失败: {err:#}")).await?;
            }
            placeholder_guard.spawn_cleanup();
        }
    }

    Ok(())
}

struct QqFollowupParams {
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    target: OwnedQqTarget,
    chat_id: i64,
    generation: u64,
    msg_id: String,
    user_prompt: String,
    should_send_voice: bool,
    effective_voice_ref: Option<String>,
    artifact_roots: Vec<PathBuf>,
    conversation_id: Option<String>,
    stream_rx: mpsc::Receiver<StreamEvent>,
}

/// Continues draining a detached `agy` process (e.g. an asynchronous `image-generator` subagent
/// that completed after Turn 1's interim reply) and delivers subsequent replies and generated
/// media back to QQ automatically.
async fn stream_qq_detached_followups(params: QqFollowupParams) {
    let QqFollowupParams {
        client,
        config,
        session_mgr,
        target,
        chat_id,
        generation,
        msg_id,
        user_prompt,
        should_send_voice,
        effective_voice_ref,
        artifact_roots,
        mut conversation_id,
        mut stream_rx,
    } = params;

    let mut pending_text = String::new();
    while let Some(event) = stream_rx.recv().await {
        match event {
            StreamEvent::Init {
                conversation_id: cid,
            } => {
                conversation_id = Some(cid);
            }
            StreamEvent::ThinkingDelta(_) => {}
            StreamEvent::ToolUse { tool, summary } => {
                info!(chat_id, tool = %tool, summary = %summary, "QQ detached tool use");
            }
            StreamEvent::TextDelta(delta) => {
                pending_text.push_str(&delta);
            }
            StreamEvent::TurnComplete { text } => {
                let turn_text = if !text.trim().is_empty() {
                    pending_text.clear();
                    text
                } else {
                    std::mem::take(&mut pending_text)
                };
                if turn_text.trim().is_empty() {
                    continue;
                }
                if !session_mgr.is_generation_current(chat_id, generation).await {
                    info!(
                        chat_id,
                        generation, "QQ detached turn superseded, stopping follow-up stream"
                    );
                    return;
                }
                let brain_dir =
                    resolve_conversation_brain_dir_async(conversation_id.as_deref()).await;
                if let Err(err) = deliver_qq_reply(
                    &client,
                    &config,
                    target.as_target(),
                    chat_id,
                    &msg_id,
                    &user_prompt,
                    &turn_text,
                    &[],
                    should_send_voice,
                    effective_voice_ref.as_deref(),
                    &artifact_roots,
                    brain_dir.as_deref(),
                )
                .await
                {
                    error!(
                        chat_id,
                        generation,
                        error = %format!("{err:#}"),
                        "Failed to deliver QQ detached follow-up reply"
                    );
                }
            }
            StreamEvent::Result(res) => {
                if res.conversation_id.is_some() {
                    conversation_id = res.conversation_id;
                }
                let final_text = if !res.response.trim().is_empty() {
                    pending_text.clear();
                    res.response
                } else {
                    std::mem::take(&mut pending_text)
                };
                if !final_text.trim().is_empty()
                    && session_mgr.is_generation_current(chat_id, generation).await
                {
                    let brain_dir =
                        resolve_conversation_brain_dir_async(conversation_id.as_deref()).await;
                    if let Err(err) = deliver_qq_reply(
                        &client,
                        &config,
                        target.as_target(),
                        chat_id,
                        &msg_id,
                        &user_prompt,
                        &final_text,
                        &[],
                        should_send_voice,
                        effective_voice_ref.as_deref(),
                        &artifact_roots,
                        brain_dir.as_deref(),
                    )
                    .await
                    {
                        error!(
                            chat_id,
                            generation,
                            error = %format!("{err:#}"),
                            "Failed to deliver QQ detached final result"
                        );
                    }
                }
                break;
            }
            StreamEvent::Error(err) => {
                warn!(chat_id, error = %err, "StreamEvent error in QQ detached follow-up");
                if session_mgr.is_generation_current(chat_id, generation).await {
                    let _ = reply(
                        &client,
                        target.as_target(),
                        &msg_id,
                        &format!("⚠️ 后台任务异常: {err}"),
                    )
                    .await;
                }
                break;
            }
        }
    }

    session_mgr.clear_detached_pid(chat_id, generation).await;
}

#[allow(clippy::too_many_arguments)]
async fn deliver_qq_reply(
    client: &QqClient,
    config: &Config,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
    user_prompt: &str,
    final_text: &str,
    failed_attachments: &[String],
    should_send_voice: bool,
    effective_voice_ref: Option<&str>,
    artifact_roots: &[PathBuf],
    active_brain_dir: Option<&Path>,
) -> Result<()> {
    let (cleaned_reply, explicit_artifacts) = collect_explicit_deliverables_with_brain_async(
        user_prompt,
        final_text,
        artifact_roots,
        active_brain_dir,
    )
    .await;
    let display_text: &str = if cleaned_reply.trim().is_empty() && !explicit_artifacts.is_empty() {
        "📎 已发送请求的文件。"
    } else {
        cleaned_reply.as_str()
    };

    let attachment_warn = (!failed_attachments.is_empty()).then(|| {
        format!(
            "⚠️ 有 {} 个附件未能获取：{}",
            failed_attachments.len(),
            failed_attachments.join("、")
        )
    });

    // Overlap rich-media `/files` uploads (`srv_send_msg: false`) with text/voice delivery so
    // images arrive immediately after the text message with zero extra sequential upload delay.
    let text_delivery_fut = async {
        let mut voice_sent = false;
        if should_send_voice {
            if let Some(ref api_key) = config.fish_api_key {
                match send_qq_voice(
                    client,
                    target,
                    msg_id,
                    display_text,
                    api_key,
                    effective_voice_ref,
                )
                .await
                {
                    Ok(()) => voice_sent = true,
                    Err(err) => {
                        warn!(chat_id, error = %format!("{err:#}"), "QQ voice delivery failed");
                    }
                }
            } else {
                warn!(chat_id, "QQ voice requested but fish_api_key is missing");
            }
        }

        if !voice_sent {
            let base_text: Cow<'_, str> = if should_send_voice {
                let cleaned = strip_emotion_tags(display_text);
                if config.fish_api_key.is_none() {
                    Cow::Owned(format!(
                        "{cleaned}\n\n（未配置 fish_api_key，本条为文字回复）"
                    ))
                } else {
                    cleaned
                }
            } else {
                Cow::Borrowed(display_text)
            };
            let combined: Cow<'_, str> = match attachment_warn {
                Some(ref warn_line) => Cow::Owned(format!("{warn_line}\n\n{base_text}")),
                None => base_text,
            };
            reply(client, target, msg_id, &combined).await?;
        } else if let Some(ref warn_line) = attachment_warn {
            let _ = reply(client, target, msg_id, warn_line).await;
        }
        Ok::<(), anyhow::Error>(())
    };

    let preupload_fut = prepare_qq_artifact_uploads(client, target, &explicit_artifacts);
    let (text_res, prepared_artifacts) = tokio::join!(text_delivery_fut, preupload_fut);
    text_res?;

    deliver_prepared_qq_artifacts(client, target, msg_id, prepared_artifacts).await;
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

/// Voice notes are recorded as SILK, which agy cannot read; QQ also serves a WAV transcode,
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
        .download_attachment(&url, &dest, MAX_QQ_ATTACHMENT_BYTES)
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

/// Downloads up to `MAX_QQ_ATTACHMENTS` concurrently into `scratch_dir`, returning
/// `(prompt_note, saved_paths, failed_labels)`.
async fn download_attachments_with_paths(
    client: &QqClient,
    attachments: &[MessageAttachment],
    scratch_dir: &Path,
    msg_id: &str,
    tag_prefix: &str,
) -> (String, Vec<PathBuf>, Vec<String>) {
    if attachments.is_empty() {
        return (String::new(), Vec::new(), Vec::new());
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
    let mut saved_paths = Vec::with_capacity(results.len());
    let mut failures = Vec::new();

    for item in results {
        match item.outcome {
            AttachmentFetchOutcome::Saved { dest, label, bytes } => {
                let _ = writeln!(note, "[{tag_prefix} {label} File: {}]", dest.display());
                info!(dest = %dest.display(), bytes, "QQ attachment saved");
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

#[cfg(test)]
async fn download_attachments(
    client: &QqClient,
    attachments: &[MessageAttachment],
    scratch_dir: &Path,
    msg_id: &str,
) -> (String, Vec<String>) {
    let (note, _, failures) =
        download_attachments_with_paths(client, attachments, scratch_dir, msg_id, "Attached").await;
    (note, failures)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QqArtifactRoute {
    Image,
    VideoMp4,
    Voice,
    GenericFile,
}

impl QqArtifactRoute {
    #[must_use]
    fn from_extension(ext: &str) -> Self {
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
    const fn primary_media_type(self) -> QqMediaFileType {
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
async fn deliver_single_file(
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

            // Non-standard image formats (webp/bmp) or video/voice rejected by media type can
            // still be attempted as generic file_type=4.
            let allow_generic_retry = primary != QqMediaFileType::File
                && !(route == QqArtifactRoute::Image
                    && matches!(
                        ext.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "gif"
                    ));
            if allow_generic_retry {
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

enum PreparedArtifactItem {
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

struct PreparedQqArtifacts {
    items: Vec<PreparedArtifactItem>,
    skipped_overflow: usize,
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
async fn prepare_qq_artifact_uploads(
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

async fn deliver_prepared_qq_artifacts(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    prepared: PreparedQqArtifacts,
) {
    let scope = target.scope_key();
    for item in prepared.items {
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
}

/// Delivers explicitly authorized artifact paths with graceful fallback when QQ rejects or caps
/// direct `file_data` uploads.
#[cfg(test)]
async fn deliver_qq_artifact_paths(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    paths: &[PathBuf],
) {
    let prepared = prepare_qq_artifact_uploads(client, target, paths).await;
    deliver_prepared_qq_artifacts(client, target, msg_id, prepared).await;
}

#[cfg(test)]
async fn deliver_qq_artifacts(
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
    deliver_qq_artifact_paths(client, target, msg_id, &paths).await;
    cleaned
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
    fn test_qq_formatter_split_markdown_chunks_handles_cjk_bytes_and_fences() {
        let text = "short text";
        let chunks = QqFormatter::split_markdown_chunks(text, 100, 100);
        assert_eq!(chunks, vec!["short text"]);

        let multi = "Line 1\nLine 2\nLine 3\nLine 4\n";
        let chunks = QqFormatter::split_markdown_chunks(multi, 14, 14);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), multi);

        // 1000 Chinese chars (3000 UTF-8 bytes) must stay in 1 chunk under QQ limits.
        let cjk_1000 = "中".repeat(1000);
        let chunks =
            QqFormatter::split_markdown_chunks(&cjk_1000, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 1);

        // 1500 Chinese chars (4500 bytes) splits cleanly at 1400 + 100 chars.
        let cjk_1500 = "文".repeat(1500);
        let chunks =
            QqFormatter::split_markdown_chunks(&cjk_1500, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 1400);
        assert_eq!(chunks[1].chars().count(), 100);
        assert_eq!(chunks.concat(), cjk_1500);

        // Tight max_bytes=5 must NEVER overshoot past 5 bytes on 3-byte CJK or 4-byte emoji.
        let cjk_tight = "中中中中";
        let tight_chunks = QqFormatter::split_markdown_chunks(cjk_tight, 10, 5);
        assert_eq!(tight_chunks.len(), 4);
        for c in &tight_chunks {
            assert!(c.len() <= 5);
        }
        assert_eq!(tight_chunks.concat(), cjk_tight);

        let emoji_tight = "😀😀😀";
        let emoji_chunks = QqFormatter::split_markdown_chunks(emoji_tight, 10, 5);
        assert_eq!(emoji_chunks.len(), 3);
        for c in &emoji_chunks {
            assert!(c.len() <= 5);
        }
        assert_eq!(emoji_chunks.concat(), emoji_tight);

        // Splitting inside a fenced code block must close and reopen the fence across chunks.
        let fenced = "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```";
        let fenced_chunks = QqFormatter::split_markdown_chunks(fenced, 28, 28);
        assert!(fenced_chunks.len() >= 2);
        assert!(fenced_chunks[0].trim_end().ends_with("```"));
        assert!(fenced_chunks[1].starts_with("```rust\n"));
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

        // Truncated 3-byte UTF-8 CJK char at buffer end must cleanly decode the valid prefix.
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
        let expected = format!("{QQ_DEFAULT_SYSTEM_PROMPT}\n{FILE_DELIVERY_PROMPT_GUIDE}");
        assert_eq!(effective_system_prompt("   ", false, false), expected);
        assert_eq!(effective_system_prompt("", false, false), expected);

        let shared = effective_system_prompt("", false, true);
        assert!(shared.contains(QQ_GROUP_MULTIUSER_PROMPT));
    }

    #[test]
    fn test_effective_system_prompt_adds_voice_cues_and_keeps_custom_instructions() {
        let prompt = effective_system_prompt("回答用日语", true, true);
        assert!(prompt.starts_with(QQ_DEFAULT_SYSTEM_PROMPT));
        assert!(prompt.contains(QQ_GROUP_MULTIUSER_PROMPT));
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

        // /help lists synced commands (/panel, /compact, /workspace, /file, /skills, /rules)
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

        // /model opus maps to live claude-opus-4-6-thinking; /model 1 selects first indexed model
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/MODEL opus", "m4")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let session = mgr.get_or_create(chat_id).await;
        assert_eq!(session.model, "claude-opus-4-6-thinking");
        assert_eq!(session.effort, "xhigh");
        assert_eq!(session.mode, "auto");

        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/model 1", "m4b")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let expected_first = crate::models::find_model_by_index(1).unwrap().id;
        assert_eq!(mgr.get_or_create(chat_id).await.model, expected_first);

        // /model reset clears model back to system default
        handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/model reset", "m4c")
            .await
            .unwrap();
        assert_eq!(mgr.get_or_create(chat_id).await.model, "");

        // /workspace switches directory and /compact handles fresh vs active conversation
        let sub_ws = dir.join("proj_a");
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            target,
            chat_id,
            &format!("/workspace {}", sub_ws.display()),
            "m4d",
        )
        .await
        .unwrap();
        assert_eq!(
            mgr.get_or_create(chat_id).await.workspace,
            sub_ws.to_string_lossy().as_ref()
        );

        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/compact", "m4e")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        mgr.resume_conversation(chat_id, Some("conv_test_1".to_string()))
            .await;
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/compact api", "m4f")
            .await
            .unwrap();
        assert!(matches!(
            out,
            CommandOutcome::ExecutePrompt {
                voice_requested: false,
                ..
            }
        ));

        // /panel renders control panel cleanly
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/panel", "m4g")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

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
                prompt_text: Cow::Borrowed("帮我总结一下"),
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
                prompt_text: Cow::Borrowed("/explain rust"),
                voice_requested: false
            }
        );

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("xhigh"));
        assert!(sent.contains("auto"));
        assert!(sent.contains("/skills"));
        assert!(sent.contains("/rules"));
        assert!(sent.contains("/panel"));
        assert!(sent.contains("/compact"));
        assert!(sent.contains("/workspace"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
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
                        // Allow image/video and Markdown `file_type=4`, while simulating rejection for CSV/PDF.
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
        // Unrequested turn must NOT send any artifacts automatically.
        deliver_qq_artifacts(
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

        // Explicit user request delivers media/exports and falls back cleanly on rejected file_type=4.
        deliver_qq_artifacts(
            &client,
            QqTarget::Group("g1"),
            "msg_1",
            "请把 chart.png demo.mp4 metrics.csv report.pdf 发到群里",
            &output,
            std::slice::from_ref(&dir),
        )
        .await;

        // Explicit [[send_file:...]] marker delivers .md via file_type=4 and strips marker text.
        let cleaned = deliver_qq_artifacts(
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
        // Image and MP4 uploaded directly via file_data (file_type 1 and 2).
        assert!(all_bodies.contains(r#""file_type":1"#));
        assert!(all_bodies.contains(r#""file_type":2"#));
        // Markdown uploaded via file_type=4.
        assert!(all_bodies.contains(r#""file_type":4"#));
        assert!(all_bodies.contains("han_dynasty_50_questions.md"));
        // CSV file_type=4 was rejected -> fell back to inline preview containing col1,col2.
        assert!(all_bodies.contains("col1,col2"));
        // Binary PDF file_type=4 was rejected -> fell back to clear diagnostic + path notice.
        assert!(all_bodies.contains("report.pdf"));

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
                        if req_line.starts_with("DELETE ")
                            && let Some(tx) = del_tx.lock().unwrap().take()
                        {
                            let _ = tx.send(req_line);
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

    #[tokio::test]
    async fn test_handle_file_command_validates_and_falls_back_cleanly() {
        let (base_url, recorded, server) = spawn_recording_qq_server().await;
        let dir =
            std::env::temp_dir().join(format!("agygram_qq_file_cmd_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let note_file = sub.join("summary.md");
        std::fs::write(&note_file, "# Summary\nHello from agygram\n").unwrap();

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
        let target = QqTarget::Group("g_file_test");
        let chat_id = qq_openid_to_chat_id("g_file_test");

        // 1. Missing arg prompts usage hint
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/file", "mf1")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // 2. Non-existent path informs user clearly
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            target,
            chat_id,
            "/file missing.txt",
            "mf2",
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // 3. Directory path is rejected cleanly
        let out = handle_qq_command(&client, &cfg, &mgr, target, chat_id, "/file subdir", "mf3")
            .await
            .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // 4. Relative path inside workspace resolves & falls back to inline preview when /files lacks file_info
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            target,
            chat_id,
            "/file subdir/summary.md",
            "mf4",
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("请提供 VPS 文件路径"));
        assert!(sent.contains("文件不存在或无法读取"));
        assert!(sent.contains("不是普通文件"));
        assert!(sent.contains("Hello from agygram"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_passive_group_cache_and_quote_resolution_downloads_quoted_image() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let body = b"\x89PNG\r\n\x1a\nmikasa";
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });

        let dir =
            std::env::temp_dir().join(format!("agygram_qq_quote_dl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let base = format!("http://{addr}");
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let cfg = Config {
            qq_allow_all_groups: true,
            ..Config::default()
        };

        // 1. Member A sends a passive group photo message (`GROUP_MESSAGE_CREATE`)
        let passive_evt = GroupMessageEvent {
            id: "ROBOT1.0_passive_photo".into(),
            content: "大家看看这张图".into(),
            group_openid: "GRP_QUOTE_E2E".into(),
            author: super::super::types::AuthorMember {
                member_openid: "MEMBER_A_OPENID".into(),
            },
            attachments: vec![MessageAttachment {
                url: format!("{base}/quoted.png"),
                content_type: Some("image/png".into()),
                filename: Some("quoted.png".into()),
                size: Some(14),
                voice_wav_url: None,
            }],
            message_type: Some(0),
            message_scene: Some(MessageScene {
                source: Some("default".into()),
                ext: vec!["msg_idx=REFIDX_photo_a==".into()],
            }),
            message_reference: None,
            msg_elements: Vec::new(),
        };
        handle_group_passive(&cfg, passive_evt);

        // 2. Member B quotes Member A's photo (`ref_msg_idx=REFIDX_photo_a==`) and @AI asks who is in it
        let quote_scene = MessageScene {
            source: Some("default".into()),
            ext: vec![
                "msg_idx=REFIDX_reply_b==".into(),
                "ref_msg_idx=REFIDX_photo_a==".into(),
            ],
        };
        let meta = IncomingMeta {
            sender_openid: "MEMBER_B_OPENID",
            is_group: true,
            is_shared_group: true,
            message_type: Some(context::QQ_QUOTE_MESSAGE_TYPE),
            message_scene: Some(&quote_scene),
            message_reference: None,
            msg_elements: &[],
        };
        let scope = QqTarget::Group("GRP_QUOTE_E2E").scope_key();
        let block = resolve_quote_prompt_block(
            &client,
            &scope,
            meta,
            "图中人物的名字是什么",
            &dir,
            "m_reply_b",
        )
        .await;

        assert!(block.contains(&context::format_group_speaker("MEMBER_A_OPENID")));
        assert!(block.contains("大家看看这张图"));
        assert!(block.contains("[Quoted Image File:") && block.contains("quoted.png"));
        assert_ne!(
            context::format_group_speaker("MEMBER_A_OPENID"),
            context::format_group_speaker("MEMBER_B_OPENID")
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_detached_qq_followup_delivers_turn2_text_and_image_and_stops_when_superseded() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
        let rec_srv = recorded.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let rec = rec_srv.clone();
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
                    let resp_body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#.to_string()
                    } else if req_line.contains("/files ") {
                        rec.lock().unwrap().push((req_line, body));
                        r#"{"file_info":"fi_detached_img"}"#.to_string()
                    } else {
                        rec.lock().unwrap().push((req_line, body));
                        r#"{"id":"m_detached_ok"}"#.to_string()
                    };
                    let raw = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let dir =
            std::env::temp_dir().join(format!("agygram_qq_detached_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let scratch = dir.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let img = scratch.join("white_bg_output.jpg");
        std::fs::write(&img, b"\xff\xd8\xff\xe0test-jpeg").unwrap();

        let base = format!("http://{addr}");
        let client = Arc::new(QqClient::new_for_test("id", "sec", &base, &base));
        let config = Arc::new(Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            ..Config::default()
        });
        let session_mgr = Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ));
        let chat_id = -424242_i64;
        let turn_gen = session_mgr.begin_turn(chat_id).await.unwrap();
        let _ = session_mgr
            .finish_turn(
                chat_id,
                turn_gen,
                TurnOutcome {
                    conversation_id: Some("conv-det-1".to_string()),
                    input_chars: 10,
                    output_chars: 10,
                    usage: None,
                    duration_seconds: Some(1.0),
                },
            )
            .await;

        let (tx, rx) = mpsc::channel::<StreamEvent>(16);
        let followup_handle = tokio::spawn(stream_qq_detached_followups(QqFollowupParams {
            client: client.clone(),
            config: config.clone(),
            session_mgr: session_mgr.clone(),
            target: OwnedQqTarget::C2c("u_det".to_string()),
            chat_id,
            generation: turn_gen,
            msg_id: "m_orig".to_string(),
            user_prompt: "将这张图片的背景改为纯白".to_string(),
            should_send_voice: false,
            effective_voice_ref: None,
            artifact_roots: vec![dir.clone(), scratch.clone()],
            conversation_id: Some("conv-det-1".to_string()),
            stream_rx: rx,
        }));

        // Subagent finishes and parent emits Turn 2 reply with [[send_file:...]]
        tx.send(StreamEvent::TurnComplete {
            text: format!("已将背景替换为纯白。\n[[send_file:{}]]", img.display()),
        })
        .await
        .unwrap();
        // Subsequent turn superseded by a newer generation -> must NOT deliver stale Turn 3.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _next_gen = session_mgr.begin_turn(chat_id).await.unwrap();
        tx.send(StreamEvent::TurnComplete {
            text: "stale turn 3 must be dropped".to_string(),
        })
        .await
        .unwrap();
        drop(tx);

        followup_handle.await.unwrap();

        let calls = recorded.lock().unwrap();
        let all_bodies: String = calls
            .iter()
            .map(|(_, b)| b.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all_bodies.contains("已将背景替换为纯白。"));
        assert!(all_bodies.contains(r#""file_type":1"#));
        assert!(all_bodies.contains(r#""file_info":"fi_detached_img""#));
        assert!(
            !all_bodies.contains("stale turn 3 must be dropped"),
            "superseded detached turn must not post leftovers"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
