//! QQ Bot event dispatch, turn orchestration, per-chat cancellation, global execution slot
//! enforcement, quote resolution, and `agy` streaming integration.

use super::client::{QqClient, QqTarget};
use super::commands::{CommandOutcome, handle_qq_command, handle_sender_identity_command};
use super::context;
use super::delivery::{
    PlaceholderGuard, QqFollowupParams, deliver_qq_reply, message_contains_secret_pattern, reply,
    reply_with_budget, stream_qq_detached_followups,
};
use super::media::{download_attachments_with_paths, maybe_spawn_scratch_cleanup};
use super::types::{
    C2CMessageEvent, GroupMessageEvent, MessageAttachment, MessageReference, MessageScene,
    MsgElement,
};
use super::{OwnedQqTarget, QqRuntime};
use crate::artifacts::{FILE_DELIVERY_PROMPT_GUIDE, resolve_conversation_brain_dir_async};
use crate::config::Config;
use crate::executor::{
    AgyExecutor, ExecutionParams, StreamEvent, TokenUsage, build_session_settings_json,
    will_use_acp,
};
use crate::models::QqGroupSessionMode;
use crate::session::{
    ChatSession, SessionManager, TurnGuard, TurnOutcome, hash_system_prompt,
    legacy_qq_group_member_to_chat_id, legacy_qq_group_openid_to_chat_id,
    legacy_qq_openid_to_chat_id, qq_group_member_to_chat_id, qq_group_openid_to_chat_id,
    qq_openid_to_chat_id,
};
use anyhow::Result;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const TURN_QUEUE_TIMEOUT: Duration = Duration::from_secs(120);
const QQ_INTERRUPT_GRACE_SECS: u64 = 6;
/// Fallback manual hint threshold when `auto_compact_threshold_tokens == 0` (true single-step context).
const MANUAL_COMPACT_HINT_THRESHOLD: u64 = 380_000;

/// QQ is a chat surface, not a document reader: default to short answers, readable Unicode math,
/// bounded tool usage, and no non-clickable `file://` links.
const QQ_DEFAULT_SYSTEM_PROMPT: &str = "\
QQ 聊天对话规范：\n\
- 先结论后要点，默认 ≤300 字（明确要求详述时除外），不复述问题、不写客套/过程旁白。\n\
- 日常问答 1–2 次工具收敛，不重复同类搜索/命令；多步调研委派 research 子代理返回精炼结论；异步子代理（如生图）启动后直接停手等回调，勿轮询。\n\
- 禁 LaTeX 源码（用 Unicode 如 ÷×≈≤≥≠±²√π→）；本地文件用文件名/反引号，禁 file:// 链接。";

/// Voice turns are read by Fish Audio, which interprets inline `[bracket]` cues for delivery
/// (<https://docs.fish.audio/developer-guide/core-features/emotions>). Injected at most once per
/// compaction epoch; cues are stripped whenever a turn renders as plain text.
const QQ_VOICE_STYLE_PROMPT: &str = "语音回复规范：回答中自然加入 Fish Audio 情感标签（句首最多 1 个，如 [calm]、[happy]、[curious]、[empathetic]，强调 [emphasis]，停顿 [break]，最多叠加 2 个），勿解释标签。";

/// Short 1-line cue for one-shot `/voice <prompt>` turns after `QQ_VOICE_STYLE_PROMPT` was
/// already injected in the active compaction epoch.
const QQ_VOICE_SHORT_CUE: &str = "本轮请按语音回复规范使用情感标签。";

/// Multi-speaker attribution rules injected when `qq_group_session_mode` is `group` (shared session).
const QQ_GROUP_MULTIUSER_PROMPT: &str = "QQ 共享群聊规范：消息头 `[发言人: 称呼(#XXXX)]` 标明说话人，不同 `#XXXX` 为不同成员，严禁混淆身份或个人凭证；未设称呼可提示发 `/nick <称呼>`。";

#[derive(Clone, Copy, Default)]
pub(crate) struct IncomingMeta<'a> {
    pub sender_openid: &'a str,
    pub is_group: bool,
    pub is_shared_group: bool,
    pub message_type: Option<u32>,
    pub message_scene: Option<&'a MessageScene>,
    pub message_reference: Option<&'a MessageReference>,
    pub msg_elements: &'a [MsgElement],
}

impl<'a> IncomingMeta<'a> {
    #[inline]
    pub(crate) fn msg_idx(self) -> Option<&'a str> {
        self.message_scene.and_then(MessageScene::msg_idx)
    }

    #[inline]
    pub(crate) fn ref_msg_idx(self) -> Option<&'a str> {
        self.message_scene.and_then(MessageScene::ref_msg_idx)
    }

    #[inline]
    pub(crate) fn has_quote_indicator(self) -> bool {
        self.ref_msg_idx().is_some()
            || self
                .message_reference
                .is_some_and(|r| !r.message_id.trim().is_empty())
            || self.message_type == Some(context::QQ_QUOTE_MESSAGE_TYPE)
    }
}

#[tracing::instrument(skip(client, config, session_mgr, runtime, event), fields(msg_id = %event.id))]
pub(crate) async fn handle_c2c(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: QqRuntime,
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

    if let Err(err) = Box::pin(process_message(
        client,
        config,
        session_mgr,
        runtime,
        QqTarget::C2c(openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
        meta,
    ))
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
        effective.as_ref(),
        Vec::new(),
        event.attachments,
    );
}

#[tracing::instrument(skip(client, config, session_mgr, runtime, event), fields(msg_id = %event.id))]
pub(crate) async fn handle_group(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: QqRuntime,
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

    if let Err(err) = Box::pin(process_message(
        client,
        config,
        session_mgr,
        runtime,
        QqTarget::Group(group_openid),
        chat_id,
        &event.content,
        &event.id,
        &event.attachments,
        meta,
    ))
    .await
    {
        error!(chat_id, error = %format!("{err:#}"), "Error processing QQ Group message");
    }
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

/// Resolves `(system_prompt, injected_base_hash, injected_voice_guide)` for this turn.
/// In `persistent` and `print` modes, static QQ base instructions and `QQ_VOICE_STYLE_PROMPT`
/// are each injected at most once per compaction epoch (re-injected on first turn, after
/// `CHECKPOINT`, or when custom prompt / shared-group mode changes).
fn resolve_qq_turn_system_prompt(
    session: &ChatSession,
    should_send_voice: bool,
    is_shared_group: bool,
    use_acp: bool,
) -> (String, Option<u64>, bool) {
    let base_prompt = effective_system_prompt(&session.system_prompt, false, is_shared_group);
    let base_hash = hash_system_prompt(&base_prompt);

    if use_acp {
        let full =
            effective_system_prompt(&session.system_prompt, should_send_voice, is_shared_group);
        return (full, Some(base_hash), should_send_voice);
    }

    let needs_base = session.is_first_turn || session.last_system_prompt_hash != Some(base_hash);
    let needs_full_voice = should_send_voice && (needs_base || !session.last_voice_guide_injected);

    if needs_base {
        let full =
            effective_system_prompt(&session.system_prompt, needs_full_voice, is_shared_group);
        (full, Some(base_hash), needs_full_voice)
    } else if needs_full_voice {
        (QQ_VOICE_STYLE_PROMPT.to_string(), None, true)
    } else if should_send_voice && !session.voice_mode {
        // One-shot `/voice <prompt>` after the full voice guide was already injected in this epoch.
        (QQ_VOICE_SHORT_CUE.to_string(), None, false)
    } else {
        (String::new(), None, false)
    }
}

fn default_sensitive_deny_paths() -> Vec<String> {
    let mut paths = vec![
        Config::get_config_dir()
            .join("*")
            .to_string_lossy()
            .into_owned(),
    ];
    if let Some(home) = dirs::home_dir() {
        paths.push(
            home.join(".config/agygram/*")
                .to_string_lossy()
                .into_owned(),
        );
        paths.push(
            home.join(".config/userbot/*")
                .to_string_lossy()
                .into_owned(),
        );
        paths.push(home.join(".ssh/*").to_string_lossy().into_owned());
    }
    paths.sort();
    paths.dedup();
    paths
}

#[inline]
fn is_native_compact_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    first.eq_ignore_ascii_case("/compact")
}

/// Assembles the user prompt sent to `agy`. When `prompt_text` begins with a leading `/`
/// (native commands like `/compact`, `/plan`, `/swarm` or installed skills), keeps `/` at byte 0
/// so `agy` recognizes the command instead of having `[发言人: ...]` swallow it.
fn assemble_qq_turn_prompt(
    prompt_text: &str,
    speaker_label: &str,
    is_shared_group: bool,
    quote_block: &str,
    attachment_note: &str,
) -> String {
    let trimmed_prompt = prompt_text.trim_start();
    if is_native_compact_prompt(trimmed_prompt) {
        return trimmed_prompt.trim().to_string();
    }

    let mut prompt_owned =
        String::with_capacity(quote_block.len() + attachment_note.len() + prompt_text.len() + 64);

    if trimmed_prompt.starts_with('/') {
        prompt_owned.push_str(trimmed_prompt);
        let has_context = is_shared_group || !quote_block.is_empty() || !attachment_note.is_empty();
        if has_context {
            prompt_owned.push_str("\n\n");
            if is_shared_group {
                let _ = writeln!(
                    prompt_owned,
                    "{}",
                    context::format_speaker_tag(speaker_label)
                );
            }
            if !quote_block.is_empty() {
                prompt_owned.push_str(quote_block);
            }
            if !attachment_note.is_empty() {
                prompt_owned.push_str(attachment_note);
            }
        }
        return prompt_owned;
    }

    if is_shared_group {
        let _ = writeln!(
            prompt_owned,
            "{}",
            context::format_speaker_tag(speaker_label)
        );
    }
    if !quote_block.is_empty() {
        prompt_owned.push_str(quote_block);
    }
    if !attachment_note.is_empty() {
        prompt_owned.push_str(attachment_note);
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
    prompt_owned
}

#[allow(clippy::too_many_arguments)]
async fn process_message(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: QqRuntime,
    target: QqTarget<'_>,
    chat_id: i64,
    raw_content: &str,
    msg_id: &str,
    attachments: &[MessageAttachment],
    meta: IncomingMeta<'_>,
) -> Result<()> {
    let content =
        context::extract_effective_content(raw_content, meta.message_scene, meta.msg_elements);
    let content_str = content.as_ref();
    // An image or quoted message sent without text is a valid prompt; only drop messages with none.
    if content_str.is_empty() && attachments.is_empty() && !meta.has_quote_indicator() {
        return Ok(());
    }

    let speaker_tag = context::member_short_tag(meta.sender_openid);
    info!(
        target_kind = target.kind_label(),
        chat_id,
        speaker = %speaker_tag,
        msg_len = content_str.len(),
        attachment_count = attachments.len(),
        msg_type = ?meta.message_type,
        msg_idx = meta.msg_idx().unwrap_or("-"),
        ref_msg_idx = meta.ref_msg_idx().unwrap_or("-"),
        msg_elements = meta.msg_elements.len(),
        "Received QQ message"
    );

    if handle_sender_identity_command(
        &client,
        target,
        chat_id,
        meta.sender_openid,
        content_str,
        msg_id,
    )
    .await?
    {
        return Ok(());
    }

    match handle_qq_command(
        &client,
        &config,
        &session_mgr,
        &runtime,
        target,
        chat_id,
        content_str,
        msg_id,
        meta,
    )
    .await?
    {
        CommandOutcome::Handled => Ok(()),
        CommandOutcome::ExecutePrompt {
            prompt_text,
            voice_requested,
        } => {
            Box::pin(execute_qq_prompt(
                client,
                config,
                session_mgr,
                runtime,
                target,
                chat_id,
                prompt_text.as_ref(),
                voice_requested,
                msg_id,
                attachments,
                meta,
            ))
            .await
        }
    }
}

async fn resolve_quote_prompt_block(
    client: &QqClient,
    scope_key: &str,
    meta: IncomingMeta<'_>,
    prompt_text: &str,
    scratch_dir: &Path,
    image_max_edge: u32,
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
    let mut quote_download_failures = Vec::new();
    if existing_local.len() < extracted.remote_attachments.len() {
        let dedup_id = extracted
            .cache_key
            .as_deref()
            .or_else(|| meta.ref_msg_idx());
        let (note, saved_paths, failures) = download_attachments_with_paths(
            client,
            &extracted.remote_attachments,
            scratch_dir,
            dedup_id,
            "Quoted",
            image_max_edge,
        )
        .await;
        existing_local.retain(|p| !saved_paths.contains(p));
        if let Some(ref cache_key) = extracted.cache_key {
            context::attach_downloaded_files_to_cached(scope_key, cache_key, &saved_paths);
        } else if let Some(ref_idx) = meta.ref_msg_idx()
            && !saved_paths.is_empty()
        {
            context::record_message(
                scope_key,
                None,
                Some(ref_idx),
                extracted.sender_label.clone().unwrap_or_default(),
                &extracted.text,
                saved_paths,
                extracted.remote_attachments.clone(),
            );
        }
        downloaded_note = note;
        quote_download_failures = failures;
    }

    context::format_quote_prompt_block(
        &extracted,
        &existing_local,
        &downloaded_note,
        &quote_download_failures,
    )
}

#[allow(clippy::too_many_arguments)]
async fn execute_qq_prompt(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: QqRuntime,
    target: QqTarget<'_>,
    chat_id: i64,
    prompt_text: &str,
    voice_requested: bool,
    msg_id: &str,
    attachments: &[MessageAttachment],
    meta: IncomingMeta<'_>,
) -> Result<()> {
    let (cancel_guard, turn_cancel) = runtime.register_turn(chat_id);
    let queue_start = Instant::now();

    // Claim the turn atomically before emitting the thinking placeholder to prevent TOCTOU
    // duplicate placeholders under concurrent messages.
    let (generation, placeholder_guard, queue_wait_ms) = if let Some(turn_gen) =
        session_mgr.begin_turn(chat_id).await
    {
        let guard = PlaceholderGuard::spawn(client.clone(), target, msg_id, "我想想...");
        (turn_gen, guard, 0u64)
    } else {
        // If a silent background auto-compact happens to be finishing, show the normal thinking
        // placeholder instead of a scary busy/queue notice so compaction remains invisible.
        let placeholder_text = if session_mgr.is_bg_compacting(chat_id).await {
            "我想想..."
        } else {
            "⏳ 当前会话已有任务正在运行，已加入等待队列，即将自动处理…（发送 /cancel 可中断当前任务）"
        };
        let guard = PlaceholderGuard::spawn(client.clone(), target, msg_id, placeholder_text);

        let wait_res = tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => {
                let waited = queue_start.elapsed().as_millis() as u64;
                info!(chat_id, queue_wait_ms = waited, "QQ queued turn cancelled before claiming slot");
                guard.spawn_cleanup();
                return Ok(());
            }
            res = session_mgr.begin_turn_with_timeout(chat_id, TURN_QUEUE_TIMEOUT) => res,
        };

        let waited = queue_start.elapsed().as_millis() as u64;
        let Some(turn_gen) = wait_res else {
            warn!(chat_id, queue_wait_ms = waited, "QQ turn queue timed out");
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
        (turn_gen, guard, waited)
    };

    let mut turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);
    if turn_cancel.is_cancelled() {
        info!(
            chat_id,
            generation, queue_wait_ms, "QQ turn cancelled right after acquiring turn lock"
        );
        session_mgr.release_turn(chat_id, generation).await;
        turn_guard.disarm();
        placeholder_guard.spawn_cleanup();
        return Ok(());
    }

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
    } else {
        maybe_spawn_scratch_cleanup(scratch_dir.clone());
    }

    let mut artifact_roots = vec![PathBuf::from(&workspace), scratch_dir.clone()];
    artifact_roots.extend(session.extra_dirs.iter().map(PathBuf::from));

    let scope_key = target.scope_key();
    let speaker_label = if meta.is_group {
        context::format_group_speaker(meta.sender_openid)
    } else {
        format!("用户 {}", context::member_short_tag(meta.sender_openid))
    };

    // Download inbound attachments and resolve any quoted message/media concurrently,
    // aborting immediately if `/cancel` or `/new` cancels this chat turn.
    let fetch_pair = tokio::select! {
        biased;
        _ = turn_cancel.cancelled() => {
            info!(chat_id, generation, "QQ turn cancelled during attachment/quote fetch");
            session_mgr.release_turn(chat_id, generation).await;
            turn_guard.disarm();
            placeholder_guard.spawn_cleanup();
            return Ok(());
        }
        pair = async {
            tokio::join!(
                download_attachments_with_paths(
                    &client,
                    attachments,
                    &scratch_dir,
                    meta.msg_idx(),
                    "Attached",
                    config.image_max_edge,
                ),
                resolve_quote_prompt_block(
                    &client,
                    &scope_key,
                    meta,
                    prompt_text,
                    &scratch_dir,
                    config.image_max_edge,
                ),
            )
        } => pair,
    };
    let ((attachment_note, saved_paths, failed_attachments), quote_block) = fetch_pair;

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

    if turn_cancel.is_cancelled() {
        info!(
            chat_id,
            generation, "QQ turn cancelled before acquiring global exec slot"
        );
        session_mgr.release_turn(chat_id, generation).await;
        turn_guard.disarm();
        placeholder_guard.spawn_cleanup();
        return Ok(());
    }

    // Acquire global concurrency slot to prevent OOM on low-RAM VPS hosts.
    let slot_start = Instant::now();
    let Some(exec_slot) = session_mgr.acquire_exec_slot(Some(&turn_cancel)).await else {
        info!(
            chat_id,
            generation,
            queue_wait_ms,
            "QQ turn cancelled while waiting for global agy execution slot"
        );
        session_mgr.release_turn(chat_id, generation).await;
        turn_guard.disarm();
        placeholder_guard.spawn_cleanup();
        return Ok(());
    };
    let slot_wait_ms = slot_start.elapsed().as_millis() as u64;

    if turn_cancel.is_cancelled() {
        drop(exec_slot);
        info!(
            chat_id,
            generation, "QQ turn cancelled immediately after acquiring exec slot"
        );
        session_mgr.release_turn(chat_id, generation).await;
        turn_guard.disarm();
        placeholder_guard.spawn_cleanup();
        return Ok(());
    }

    let is_compact_turn = is_native_compact_prompt(prompt_text);
    let backend = config.executor_backend_enum();
    let effective_use_acp = will_use_acp(backend, false, session.remote_agent);
    let prompt_owned = assemble_qq_turn_prompt(
        prompt_text,
        &speaker_label,
        meta.is_shared_group,
        &quote_block,
        &attachment_note,
    );
    let input_chars = prompt_owned.len();
    let (system_prompt, injected_system_prompt_hash, injected_voice_guide) = if is_compact_turn {
        (String::new(), None, false)
    } else {
        resolve_qq_turn_system_prompt(
            &session,
            should_send_voice,
            meta.is_shared_group,
            effective_use_acp,
        )
    };
    let deny_paths = default_sensitive_deny_paths();
    let settings_json = build_session_settings_json(
        session.show_thinking,
        session.orchestrator_planner.as_deref(),
        session.orchestrator_executor.as_deref(),
        &deny_paths,
    );

    let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(128);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    let params = ExecutionParams {
        chat_id,
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
        settings_json,
        json_schema: None,
        remote_agent: session.remote_agent,
        backend,
    };

    let mut accumulated_text = String::with_capacity(2048);
    let mut captured_conv_id: Option<String> = None;
    let mut captured_usage: Option<TokenUsage> = None;
    let mut captured_context_tokens: Option<u64> = None;
    let mut captured_num_turns: Option<u64> = None;
    let mut saw_checkpoint = false;
    let mut stream_error: Option<String> = None;

    let start_time = Instant::now();
    let shutdown_cancel = runtime.root_cancel();
    let turn_cancel_exec = turn_cancel.clone();
    let exec_aborted = CancellationToken::new();
    let exec_aborted_tx = exec_aborted.clone();
    let idle_timeout_secs = config.idle_timeout_secs;
    let max_execution_secs = config.max_execution_secs;

    let exec_with_cancel = async move {
        let mut exec_fut = Box::pin(AgyExecutor::run_prompt_streaming(
            params,
            stream_tx,
            Some(pid_tx),
            idle_timeout_secs,
            max_execution_secs,
            Some(turn_cancel_exec.clone()),
        ));
        let outcome = tokio::select! {
            biased;
            res = &mut exec_fut => Some(res),
            _ = turn_cancel_exec.cancelled() => {
                if shutdown_cancel.is_cancelled() {
                    None
                } else {
                    let grace = if effective_use_acp {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_secs(QQ_INTERRUPT_GRACE_SECS)
                    };
                    tokio::select! {
                        biased;
                        res = &mut exec_fut => Some(res),
                        _ = shutdown_cancel.cancelled() => None,
                        _ = tokio::time::sleep(grace) => None,
                    }
                }
            }
        };
        drop(exec_fut);
        if outcome.is_none() {
            exec_aborted_tx.cancel();
        }
        outcome
    };

    let ingest_fut = async {
        let mut pid_rx = Some(pid_rx);
        loop {
            tokio::select! {
                biased;

                _ = exec_aborted.cancelled() => {
                    break;
                }

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
                            let trimmed = conversation_id.trim();
                            if !trimmed.is_empty() {
                                captured_conv_id = Some(trimmed.to_string());
                            }
                        }
                        StreamEvent::TextDelta(delta) => {
                            accumulated_text.push_str(&delta);
                        }
                        StreamEvent::ThinkingDelta(_) => {}
                        StreamEvent::Checkpoint => {
                            saw_checkpoint = true;
                            info!(chat_id, generation, "QQ context checkpoint reached");
                        }
                        StreamEvent::ToolUse { tool, summary } => {
                            info!(chat_id, tool = %tool, summary = %summary, "QQ tool use");
                        }
                        StreamEvent::ToolError { tool, message } => {
                            warn!(chat_id, tool = %tool, error = %message, "QQ tool error");
                        }
                        StreamEvent::SubagentUpdate { summary } => {
                            info!(chat_id, summary = %summary, "QQ subagent update");
                        }
                        StreamEvent::TurnComplete { text } => {
                            if !text.is_empty() {
                                accumulated_text = text;
                            }
                        }
                        StreamEvent::Result(res) => {
                            if let Some(cid) = res
                                .conversation_id
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                            {
                                captured_conv_id = Some(cid.to_string());
                            }
                            if !res.response.is_empty() {
                                accumulated_text = res.response;
                            }
                            captured_usage = res.usage;
                            if res.context_tokens.is_some() {
                                captured_context_tokens = res.context_tokens;
                            }
                            if res.num_turns.is_some() {
                                captured_num_turns = res.num_turns;
                            }
                            saw_checkpoint |= res.saw_checkpoint;
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

    let (exec_opt, stream_rx) = tokio::join!(exec_with_cancel, ingest_fut);
    // Release the global concurrency slot as soon as foreground execution finishes/detaches/cancels.
    drop(exec_slot);
    // Always measure true single-turn wall-clock elapsed seconds (agy's result.duration_seconds
    // reports the entire multi-day conversation lifetime span).
    let turn_duration = start_time.elapsed().as_secs_f64();

    let Some(exec_res) = exec_opt else {
        info!(
            chat_id,
            generation,
            backend = backend.as_str(),
            queue_wait_ms,
            slot_wait_ms,
            duration_secs = turn_duration,
            "QQ turn cancelled during execution; global exec slot released"
        );
        session_mgr.release_turn(chat_id, generation).await;
        turn_guard.disarm();
        placeholder_guard.spawn_cleanup();
        return Ok(());
    };

    match exec_res {
        Ok(res) => {
            if !res.status.eq_ignore_ascii_case("SUCCESS") {
                warn!(
                    chat_id,
                    generation,
                    backend = backend.as_str(),
                    status = %res.status,
                    finished_reason = ?res.finished_reason,
                    duration_secs = turn_duration,
                    "QQ agy execution finished with non-SUCCESS status"
                );
            }

            let is_detached = res.detached;
            let final_saw_checkpoint = saw_checkpoint || res.saw_checkpoint;
            let final_context_tokens = captured_context_tokens.or(res.context_tokens);
            let final_num_turns = captured_num_turns.or(res.num_turns);
            let mut final_text = if !accumulated_text.trim().is_empty() {
                accumulated_text
            } else if !res.output.trim().is_empty() {
                res.output
            } else if let Some(err) = stream_error {
                format!("⚠️ {err}")
            } else {
                "✅ 任务执行完毕 (无文本输出)".to_string()
            };
            let trimmed_len = final_text.trim_end().len();
            final_text.truncate(trimmed_len);

            let resolved_conv_id = captured_conv_id
                .filter(|s| !s.trim().is_empty())
                .or_else(|| res.conversation_id.filter(|s| !s.trim().is_empty()))
                .or_else(|| session.conversation_id.clone());

            let outcome = TurnOutcome {
                conversation_id: resolved_conv_id.clone(),
                status: &res.status,
                input_chars,
                output_chars: final_text.len(),
                usage: captured_usage.as_ref().or(res.usage.as_ref()),
                usage_is_cumulative: res.usage_is_cumulative,
                context_tokens: final_context_tokens,
                num_turns: final_num_turns,
                saw_checkpoint: final_saw_checkpoint,
                system_prompt_sent: res.system_prompt_sent,
                injected_system_prompt_hash,
                injected_voice_guide,
                injected_widget_guide: false,
                duration_seconds: Some(turn_duration),
                detached: is_detached,
            };

            let post_session_opt = session_mgr.finish_turn(chat_id, generation, outcome).await;
            turn_guard.disarm();

            let Some(post_session) = post_session_opt else {
                info!(
                    chat_id,
                    generation,
                    detached = is_detached,
                    turn_cancelled = turn_cancel.is_cancelled(),
                    queue_wait_ms,
                    slot_wait_ms,
                    "Dropping QQ turn output because turn generation was superseded or cancelled (owns_turn=false)"
                );
                if is_detached {
                    session_mgr.clear_detached_pid(chat_id, generation).await;
                }
                placeholder_guard.spawn_cleanup();
                return Ok(());
            };

            if turn_cancel.is_cancelled() {
                info!(
                    chat_id,
                    generation,
                    detached = is_detached,
                    "Dropping QQ turn output because turn was cancelled before delivery"
                );
                if is_detached {
                    session_mgr.clear_detached_pid(chat_id, generation).await;
                }
                placeholder_guard.spawn_cleanup();
                return Ok(());
            }

            // When seamless background auto-compaction is enabled (default), never nag users with
            // manual `/compact` hints. Only show a cooled-down hint if auto-compact is explicitly disabled.
            let compact_hint = if config.auto_compact_threshold_tokens == 0
                && !is_compact_turn
                && !final_saw_checkpoint
                && session_mgr
                    .should_show_compact_hint(chat_id, MANUAL_COMPACT_HINT_THRESHOLD)
                    .await
            {
                session_mgr.record_compact_hint_shown(chat_id).await;
                Some("💡 当前上下文较长，可发 /compact 压缩提速")
            } else {
                None
            };
            let secret_warning = (meta.is_shared_group
                && message_contains_secret_pattern(prompt_text))
            .then_some(
                "🔒 安全提醒：检测到你在共享群消息中可能包含密钥/Token 凭证，请尽快撤回并轮换该凭证，切勿在共享群聊中发送敏感凭据。",
            );

            let active_brain_dir =
                resolve_conversation_brain_dir_async(resolved_conv_id.as_deref()).await;
            let placeholder_slots = placeholder_guard.suppress_if_unsent();
            let delivery_res = deliver_qq_reply(
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
                Some(&scratch_dir),
                placeholder_slots,
                secret_warning,
                compact_hint,
            )
            .await;

            placeholder_guard.spawn_cleanup();

            let slots_used_after_turn1 = match &delivery_res {
                Ok(stats) => {
                    info!(
                        chat_id,
                        generation,
                        backend = backend.as_str(),
                        status = %res.status,
                        placeholder_slots,
                        chunks = stats.chunks_sent,
                        artifacts = stats.artifacts_sent,
                        overflow_saved = stats.overflow_saved,
                        detached = is_detached,
                        context_tokens = post_session.last_context_tokens,
                        queue_wait_ms,
                        slot_wait_ms,
                        duration_secs = turn_duration,
                        "Delivered QQ turn reply"
                    );
                    placeholder_slots.saturating_add(stats.chunks_sent + stats.artifacts_sent)
                }
                Err(err) => {
                    warn!(
                        chat_id,
                        generation,
                        backend = backend.as_str(),
                        detached = is_detached,
                        queue_wait_ms,
                        slot_wait_ms,
                        error = %format!("{err:#}"),
                        "QQ Turn 1 reply delivery failed"
                    );
                    placeholder_slots.saturating_add(1)
                }
            };

            // Schedule invisible idle background compaction if real single-step context crossed
            // threshold; zero extra latency on this reply and cancels cleanly if chat resumes.
            crate::compaction::maybe_schedule_background_compact(
                session_mgr.clone(),
                config.clone(),
                chat_id,
                &post_session,
                is_compact_turn,
                is_detached,
                final_saw_checkpoint,
            );

            // Even if Turn 1 HTTP delivery transiently failed, ALWAYS attach the detached follow-up
            // drain task to `QqRuntime` so `stream_rx` is never dropped and detached processes never
            // become untracked orphans. Moving `cancel_guard` keeps per-chat `/cancel` wired.
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
                    scratch_dir,
                    slots_already_used: slots_used_after_turn1,
                    conversation_id: resolved_conv_id,
                    stream_rx,
                    turn_cancel,
                    shutdown_cancel: runtime.root_cancel(),
                    cancel_guard: Some(cancel_guard),
                };
                runtime
                    .spawn_detached(stream_qq_detached_followups(followup))
                    .await;
            }

            delivery_res.map(|_| ())
        }
        Err(err) => {
            error!(
                chat_id,
                generation,
                backend = backend.as_str(),
                queue_wait_ms,
                slot_wait_ms,
                error = %format!("{err:#}"),
                "Execution failed in QQ chat"
            );
            let is_current = session_mgr.is_generation_current(chat_id, generation).await;
            session_mgr.release_turn(chat_id, generation).await;
            turn_guard.disarm();

            let placeholder_slots = placeholder_guard.suppress_if_unsent();
            if is_current && !turn_cancel.is_cancelled() {
                let _ = reply_with_budget(
                    &client,
                    target,
                    msg_id,
                    &format!("❌ 执行失败: {err:#}"),
                    placeholder_slots,
                    0,
                    None,
                )
                .await?;
            }
            placeholder_guard.spawn_cleanup();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_effective_system_prompt_defaults_to_brevity_and_unicode_rules() {
        let expected = format!("{QQ_DEFAULT_SYSTEM_PROMPT}\n{FILE_DELIVERY_PROMPT_GUIDE}");
        assert_eq!(effective_system_prompt("   ", false, false), expected);
        assert_eq!(effective_system_prompt("", false, false), expected);
        assert!(expected.contains("LaTeX"));
        assert!(expected.contains("file://"));

        let shared = effective_system_prompt("", false, true);
        assert!(shared.contains(QQ_GROUP_MULTIUSER_PROMPT));
        assert!(shared.contains("/nick"));
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
    fn test_assemble_qq_turn_prompt_preserves_leading_slash_in_shared_groups() {
        let compact = assemble_qq_turn_prompt(
            "/compact focus on tests",
            "#A3F2",
            true,
            "[引用消息上下文]\n",
            "",
        );
        assert_eq!(compact, "/compact focus on tests");

        let plan = assemble_qq_turn_prompt(
            "/plan refactor qq",
            "阿杰(#A3F2)",
            true,
            "",
            "[Attached File: /tmp/a.txt]\n",
        );
        assert!(plan.starts_with("/plan refactor qq"));
        assert!(plan.contains("[发言人: 阿杰(#A3F2)]"));
        assert!(plan.contains("[Attached File: /tmp/a.txt]"));

        let normal = assemble_qq_turn_prompt("你好", "#A3F2", true, "", "");
        assert_eq!(normal, "[发言人: #A3F2]\n你好");
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
        let block =
            resolve_quote_prompt_block(&client, &scope, meta, "图中人物的名字是什么", &dir, 0)
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

    #[test]
    fn test_resolve_qq_turn_system_prompt_deduplicates_static_instructions() {
        use crate::config::ExecutorBackend;

        let mut session =
            ChatSession::new("/tmp".into(), String::new(), "auto".into(), "high".into());

        // Persistent & Print & ACP-with-remote-agent all deduplicate identically via `will_use_acp`.
        assert!(!will_use_acp(ExecutorBackend::Persistent, false, false));
        assert!(!will_use_acp(ExecutorBackend::Print, false, false));
        assert!(!will_use_acp(ExecutorBackend::Acp, false, true));
        assert!(will_use_acp(ExecutorBackend::Acp, false, false));

        // Turn 1 (first turn): injects full base system prompt and returns its hash.
        let (p1, h1, v1) = resolve_qq_turn_system_prompt(&session, false, true, false);
        assert!(p1.contains(QQ_DEFAULT_SYSTEM_PROMPT));
        assert!(p1.contains(QQ_GROUP_MULTIUSER_PROMPT));
        assert!(!v1);
        let h1 = h1.expect("first turn returns base prompt hash");

        // Follow-up turn with matching hash skips static base instructions completely.
        session.is_first_turn = false;
        session.last_system_prompt_hash = Some(h1);
        let (p2, h2, v2) = resolve_qq_turn_system_prompt(&session, false, true, false);
        assert!(
            p2.is_empty(),
            "follow-up turn must omit duplicate base prompt"
        );
        assert_eq!(h2, None);
        assert!(!v2);

        // First `/voice on` turn in this epoch injects ONLY `QQ_VOICE_STYLE_PROMPT` once.
        session.voice_mode = true;
        let (p_voice1, h_voice1, v_voice1) =
            resolve_qq_turn_system_prompt(&session, true, true, false);
        assert_eq!(p_voice1, QQ_VOICE_STYLE_PROMPT);
        assert_eq!(h_voice1, None);
        assert!(v_voice1);

        // Subsequent `/voice on` turns in the SAME compaction epoch inject nothing!
        session.last_voice_guide_injected = true;
        let (p_voice2, h_voice2, v_voice2) =
            resolve_qq_turn_system_prompt(&session, true, true, false);
        assert!(
            p_voice2.is_empty(),
            "permanent voice_mode must not repeat QQ_VOICE_STYLE_PROMPT every turn"
        );
        assert_eq!(h_voice2, None);
        assert!(!v_voice2);

        // One-shot `/voice <prompt>` when `voice_mode == false` after guide already injected emits short cue.
        session.voice_mode = false;
        let (p_oneshot, h_oneshot, v_oneshot) =
            resolve_qq_turn_system_prompt(&session, true, true, false);
        assert_eq!(p_oneshot, QQ_VOICE_SHORT_CUE);
        assert_eq!(h_oneshot, None);
        assert!(!v_oneshot);

        // After CHECKPOINT clears `last_system_prompt_hash` & `last_voice_guide_injected`,
        // base + voice guide are cleanly re-injected once for the new epoch.
        session.voice_mode = true;
        session.last_system_prompt_hash = None;
        session.last_voice_guide_injected = false;
        let (p_after_cp, h_after_cp, v_after_cp) =
            resolve_qq_turn_system_prompt(&session, true, true, false);
        assert!(p_after_cp.contains(QQ_DEFAULT_SYSTEM_PROMPT));
        assert!(p_after_cp.contains(QQ_VOICE_STYLE_PROMPT));
        assert_eq!(h_after_cp, Some(h1));
        assert!(v_after_cp);
    }

    #[tokio::test]
    async fn test_resolve_quote_prompt_block_downloads_remaining_uncached_attachments() {
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
                    let body = b"\x89PNG\r\n\x1a\nsecond-image";
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "agygram_qq_partial_quote_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cached_first = dir.join("already_cached_first.png");
        std::fs::write(&cached_first, b"\x89PNG\r\n\x1a\nfirst-image").unwrap();

        let base = format!("http://{addr}");
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let scope = QqTarget::Group("GRP_PARTIAL_QUOTE").scope_key();
        let attachments = vec![
            MessageAttachment {
                url: format!("{base}/img1.png"),
                content_type: Some("image/png".into()),
                filename: Some("img1.png".into()),
                size: Some(20),
                voice_wav_url: None,
            },
            MessageAttachment {
                url: format!("{base}/img2.png"),
                content_type: Some("image/png".into()),
                filename: Some("img2.png".into()),
                size: Some(20),
                voice_wav_url: None,
            },
        ];
        // Pre-record a cached message that has 1 local file already on disk, but 2 remote attachments.
        context::record_message(
            &scope,
            Some("ROBOT1.0_partial_q"),
            Some("REFIDX_partial_q=="),
            "阿杰(#1234)",
            "两张图对比下",
            vec![cached_first.clone()],
            attachments,
        );

        let quote_scene = MessageScene {
            source: Some("default".into()),
            ext: vec![
                "msg_idx=REFIDX_partial_reply==".into(),
                "ref_msg_idx=REFIDX_partial_q==".into(),
            ],
        };
        let meta = IncomingMeta {
            sender_openid: "MEMBER_PARTIAL",
            is_group: true,
            is_shared_group: true,
            message_type: Some(context::QQ_QUOTE_MESSAGE_TYPE),
            message_scene: Some(&quote_scene),
            message_reference: None,
            msg_elements: &[],
        };

        let block = resolve_quote_prompt_block(&client, &scope, meta, "看第二张图", &dir, 0).await;
        assert!(
            block.contains("img2.png"),
            "uncached 2nd quoted image must be downloaded: {block}"
        );
        assert!(
            block.contains("img1.png"),
            "1st quoted image must also be present: {block}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
