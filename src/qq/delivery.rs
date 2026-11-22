//! QQ reply delivery, passive 5-slot budget planning, long-reply graceful degradation,
//! placeholder recall guard, credential privacy detection, and detached follow-up streaming.

use super::OwnedQqTarget;
use super::client::{QqClient, QqTarget, is_passive_or_proactive_limit_error};
use super::context;
use super::formatter::QqFormatter;
use super::media::{
    count_planned_artifact_slots, deliver_prepared_qq_artifacts, prepare_qq_artifact_uploads,
    sanitize_message_id, send_qq_voice, strip_emotion_tags,
};
use super::types::QqMediaFileType;
use crate::artifacts::{
    collect_explicit_deliverables_with_brain_async, resolve_conversation_brain_dir_async,
};
use crate::config::Config;
use crate::executor::StreamEvent;
use crate::session::SessionManager;
use crate::text::truncate_chars;
use anyhow::Result;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// QQ Open Platform v2 permits at most 5 passive replies per inbound `msg_id`.
pub(crate) const QQ_PASSIVE_REPLY_LIMIT: usize = 5;
pub(crate) const QQ_MAX_CHUNK_CHARS: usize = 1400;
pub(crate) const QQ_MAX_CHUNK_BYTES: usize = 4500;
/// Safe expanded single-message ceiling used to fit long replies inside remaining passive slots
/// before falling back to a Markdown file attachment.
pub(crate) const QQ_EXPANDED_CHUNK_CHARS: usize = 2200;
pub(crate) const QQ_EXPANDED_CHUNK_BYTES: usize = 6500;

/// Summary metrics for one delivered QQ reply turn.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeliveryStats {
    pub chunks_sent: usize,
    pub artifacts_sent: usize,
    pub overflow_saved: bool,
}

/// Chunking plan produced by [`plan_qq_text_chunks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplyChunkPlan {
    pub chunks: Vec<String>,
    pub overflow_full_text: Option<String>,
    pub expanded_limit_used: bool,
    pub reserved_file_slot: bool,
}

/// Computes available passive text slots for one `msg_id`, reserving space for placeholders and
/// planned media/file artifacts while guaranteeing at least 1 text slot.
#[must_use]
pub(crate) fn compute_passive_text_slots(
    slots_already_used: usize,
    planned_artifact_slots: usize,
) -> usize {
    QQ_PASSIVE_REPLY_LIMIT
        .saturating_sub(slots_already_used)
        .saturating_sub(planned_artifact_slots)
        .max(1)
}

/// Plans Markdown chunks within `text_slots`:
/// 1. Standard `1400 chars / 4500 B` split if it fits within `text_slots`.
/// 2. Expanded `2200 chars / 6500 B` split to compress into `text_slots` without hitting `40034128`.
/// 3. If still over budget, reserves 1 slot for `scratch/reply-<id>.md` (when `text_slots >= 2`
///    and a `scratch_dir` is available) or appends an explicit truncation notice so tails are
///    never silently dropped.
#[must_use]
pub(crate) fn plan_qq_text_chunks(
    formatted_md: &str,
    text_slots: usize,
    has_scratch_dir: bool,
) -> ReplyChunkPlan {
    let slots = text_slots.max(1);
    let standard =
        QqFormatter::split_markdown_chunks(formatted_md, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
    if standard.len() <= slots {
        return ReplyChunkPlan {
            chunks: standard,
            overflow_full_text: None,
            expanded_limit_used: false,
            reserved_file_slot: false,
        };
    }

    let expanded = QqFormatter::split_markdown_chunks(
        formatted_md,
        QQ_EXPANDED_CHUNK_CHARS,
        QQ_EXPANDED_CHUNK_BYTES,
    );
    if expanded.len() <= slots {
        return ReplyChunkPlan {
            chunks: expanded,
            overflow_full_text: None,
            expanded_limit_used: true,
            reserved_file_slot: false,
        };
    }

    let reserve_file_slot = has_scratch_dir && slots >= 2;
    let keep_text_slots = if reserve_file_slot { slots - 1 } else { slots };
    let mut kept: Vec<String> = expanded.into_iter().take(keep_text_slots).collect();
    let total_chars = formatted_md.chars().count();
    let kept_chars: usize = kept.iter().map(|c| c.chars().count()).sum();
    let omitted_chars = total_chars.saturating_sub(kept_chars);

    let suffix = if reserve_file_slot {
        format!(
            "\n\n…（后续约 {omitted_chars} 字超出 QQ 单轮被动回复上限，完整长文将尝试以 Markdown 文件发送）"
        )
    } else if has_scratch_dir {
        format!(
            "\n\n…（后续约 {omitted_chars} 字超出 QQ 单轮被动回复上限，完整文本已保存至 scratch/，可回复「继续」查看）"
        )
    } else {
        format!(
            "\n\n…（后续约 {omitted_chars} 字超出 QQ 单轮被动回复上限，可回复「继续」分段获取）"
        )
    };

    if let Some(last) = kept.last_mut() {
        let suffix_chars = suffix.chars().count();
        let budget_chars = QQ_EXPANDED_CHUNK_CHARS.saturating_sub(suffix_chars + 16);
        if last.chars().count() > budget_chars {
            *last = truncate_chars(last, budget_chars).into_owned();
        }
        last.push_str(&suffix);
    }

    ReplyChunkPlan {
        chunks: kept,
        overflow_full_text: Some(formatted_md.to_string()),
        expanded_limit_used: true,
        reserved_file_slot: reserve_file_slot,
    }
}

/// Detects obvious credential / token leaks in shared-group messages so the bot can remind the
/// sender to recall and rotate secrets immediately.
#[must_use]
pub(crate) fn message_contains_secret_pattern(text: &str) -> bool {
    if text.len() < 12 {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("begin private key")
        || lower.contains("begin rsa private key")
        || lower.contains("begin openssh private key")
        || lower.contains("begin ec private key")
    {
        return true;
    }

    for kv_key in [
        "auth_token=",
        "bot_token=",
        "app_secret=",
        "client_secret=",
        "api_key=",
    ] {
        if let Some(pos) = lower.find(kv_key) {
            let val = text[pos + kv_key.len()..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(['"', '\'', ',', ';']);
            if val.len() >= 8
                && !val.eq_ignore_ascii_case("redacted")
                && !val.starts_with('<')
                && !val.starts_with('[')
            {
                return true;
            }
        }
    }

    for token in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | ',' | ';' | '(' | ')' | '[' | ']')
    }) {
        if let Some(rest) = token.strip_prefix("sk-")
            && rest.len() >= 16
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return true;
        }
        for gh_prefix in ["ghp_", "gho_", "ghu_", "ghs_", "github_pat_"] {
            if let Some(rest) = token.strip_prefix(gh_prefix)
                && rest.len() >= 12
                && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return true;
            }
        }
        if let Some(rest) = token.strip_prefix("AIza")
            && rest.len() >= 20
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return true;
        }
        if let Some(rest) = token.strip_prefix("AKIA")
            && rest.len() == 16
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return true;
        }
    }

    false
}

/// RAII guard that ensures `"我想想..."` / queue-notice placeholders are sent without blocking
/// turn startup and are always recalled even if an early `?` return, cancellation, or panic
/// interrupts delivery.
pub(crate) struct PlaceholderGuard {
    client: Arc<QqClient>,
    target: OwnedQqTarget,
    pending_send: Option<tokio::task::JoinHandle<Option<String>>>,
}

impl PlaceholderGuard {
    #[cfg(test)]
    pub(crate) fn new(
        client: Arc<QqClient>,
        target: QqTarget<'_>,
        message_id: Option<String>,
    ) -> Self {
        let handle = message_id.map(|id| tokio::spawn(async move { Some(id) }));
        Self {
            client,
            target: OwnedQqTarget::from_target(target),
            pending_send: handle,
        }
    }

    pub(crate) fn spawn(
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
    pub(crate) fn spawn_cleanup(mut self) {
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

/// Sends an ephemeral plain-text status/queue notice (`msg_type=0`) in a single RTT and records
/// it in the per-scope context cache under `"AI 助手"` so quoting it never degrades to `历史消息`.
pub(crate) async fn reply_ephemeral(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    let sent_meta = client
        .send_plain_message_with_meta(target, text, msg_id)
        .await?;
    context::record_message(
        &target.scope_key(),
        sent_meta.id.as_deref(),
        sent_meta.ref_idx(),
        "AI 助手",
        text,
        Vec::new(),
        Vec::new(),
    );
    Ok(sent_meta.id)
}

/// Formats and sends a reply using the default full 5-slot passive budget (for commands/notices).
pub(crate) async fn reply(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
) -> Result<Option<String>> {
    let (last_id, _, _) = reply_with_budget(client, target, msg_id, text, 0, 0, None).await?;
    Ok(last_id)
}

async fn save_overflow_reply_markdown(
    scratch_dir: &Path,
    msg_id: &str,
    content: &str,
) -> Option<PathBuf> {
    let safe_id = sanitize_message_id(msg_id);
    let path = scratch_dir.join(format!("reply-{safe_id}.md"));
    match tokio::fs::write(&path, content.as_bytes()).await {
        Ok(()) => Some(path),
        Err(err) => {
            warn!(path = %path.display(), error = %err, "Failed to write QQ overflow reply markdown");
            None
        }
    }
}

/// Formats and sends a reply respecting QQ's 5-message passive reply budget per `msg_id`.
/// Automatically widens chunk bounds when needed and degrades ultra-long replies to a saved
/// `scratch/reply-<id>.md` file attachment instead of hitting `40034128` / `40034105`.
pub(crate) async fn reply_with_budget(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
    slots_already_used: usize,
    planned_artifact_slots: usize,
    scratch_dir: Option<&Path>,
) -> Result<(Option<String>, usize, bool)> {
    let formatted = QqFormatter::format_markdown(text);
    let scope = target.scope_key();
    let text_slots = compute_passive_text_slots(slots_already_used, planned_artifact_slots);
    let mut plan = plan_qq_text_chunks(&formatted, text_slots, scratch_dir.is_some());

    let mut overflow_file: Option<PathBuf> = None;
    let mut overflow_saved = false;
    if let Some(ref full_text) = plan.overflow_full_text
        && let Some(dir) = scratch_dir
        && let Some(saved_path) = save_overflow_reply_markdown(dir, msg_id, full_text).await
    {
        overflow_saved = true;
        if let Some(name) = saved_path.file_name().and_then(|n| n.to_str())
            && let Some(last) = plan.chunks.last_mut()
        {
            let _ = write!(last, "\n📁 完整归档: `scratch/{name}`");
        }
        overflow_file = Some(saved_path);
    }

    if plan.expanded_limit_used {
        info!(
            target_kind = target.kind_label(),
            text_slots,
            chunks = plan.chunks.len(),
            overflow_saved,
            "Compressed QQ long reply to fit passive reply budget"
        );
    }

    let mut last_id = None;
    let mut chunks_sent = 0usize;
    let total_chunks = plan.chunks.len();

    for (idx, chunk) in plan.chunks.into_iter().enumerate() {
        match client.send_message_with_meta(target, &chunk, msg_id).await {
            Ok(sent_meta) => {
                chunks_sent += 1;
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
            Err(err) => {
                if chunks_sent > 0 && is_passive_or_proactive_limit_error(&err) {
                    if !overflow_saved
                        && let Some(dir) = scratch_dir
                        && save_overflow_reply_markdown(dir, msg_id, &formatted)
                            .await
                            .is_some()
                    {
                        overflow_saved = true;
                    }
                    warn!(
                        target_kind = target.kind_label(),
                        chunk_index = idx + 1,
                        total_chunks,
                        overflow_saved,
                        error = %format!("{err:#}"),
                        "QQ passive/proactive reply quota exhausted mid-reply; saved full reply cleanly"
                    );
                    return Ok((last_id, chunks_sent, overflow_saved));
                }
                return Err(err);
            }
        }
    }

    if plan.reserved_file_slot
        && let Some(ref path) = overflow_file
    {
        match client
            .send_media_file_with_meta(target, path, msg_id, QqMediaFileType::File)
            .await
        {
            Ok(sent_meta) => {
                chunks_sent += 1;
                let file_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("reply.md");
                context::record_message(
                    &scope,
                    sent_meta.id.as_deref(),
                    sent_meta.ref_idx(),
                    "AI 助手",
                    &format!("[AI 发送的 file: {file_name}]"),
                    vec![path.clone()],
                    Vec::new(),
                );
                info!(path = %path.display(), "Delivered QQ overflow long reply as Markdown file");
            }
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %format!("{err:#}"),
                    "QQ overflow Markdown file attachment rejected; user already received preview + scratch path"
                );
            }
        }
    }

    Ok((last_id, chunks_sent, overflow_saved))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn deliver_qq_reply(
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
    scratch_dir: Option<&Path>,
    slots_already_used: usize,
    extra_notice_prefix: Option<&str>,
    extra_footer_hint: Option<&str>,
) -> Result<DeliveryStats> {
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

    let planned_artifact_slots = count_planned_artifact_slots(&explicit_artifacts);

    // Overlap rich-media `/files` uploads (`srv_send_msg: false`) with text/voice delivery so
    // images arrive immediately after the text message with zero extra sequential upload delay.
    let text_delivery_fut = async {
        let mut voice_sent = false;
        let mut chunks_sent = 0usize;
        let mut overflow_saved = false;

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
                    Ok(()) => {
                        voice_sent = true;
                        chunks_sent += 1;
                    }
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

            let mut composed = String::new();
            if let Some(prefix) = extra_notice_prefix
                && !prefix.trim().is_empty()
            {
                composed.push_str(prefix.trim());
                composed.push_str("\n\n");
            }
            if let Some(ref warn_line) = attachment_warn {
                composed.push_str(warn_line);
                composed.push_str("\n\n");
            }
            composed.push_str(&base_text);
            if let Some(footer) = extra_footer_hint
                && !footer.trim().is_empty()
            {
                composed.push_str("\n\n");
                composed.push_str(footer.trim());
            }

            let (_, sent, saved) = reply_with_budget(
                client,
                target,
                msg_id,
                &composed,
                slots_already_used,
                planned_artifact_slots,
                scratch_dir,
            )
            .await?;
            chunks_sent += sent;
            overflow_saved = saved;
        } else {
            let mut side_notes = Vec::new();
            if let Some(prefix) = extra_notice_prefix
                && !prefix.trim().is_empty()
            {
                side_notes.push(prefix.trim());
            }
            if let Some(ref warn_line) = attachment_warn {
                side_notes.push(warn_line.as_str());
            }
            if let Some(footer) = extra_footer_hint
                && !footer.trim().is_empty()
            {
                side_notes.push(footer.trim());
            }
            if !side_notes.is_empty() {
                let joined = side_notes.join("\n\n");
                if reply(client, target, msg_id, &joined).await.is_ok() {
                    chunks_sent += 1;
                }
            }
        }
        Ok::<(usize, bool), anyhow::Error>((chunks_sent, overflow_saved))
    };

    let preupload_fut = prepare_qq_artifact_uploads(client, target, &explicit_artifacts);
    let (text_res, prepared_artifacts) = tokio::join!(text_delivery_fut, preupload_fut);
    let (chunks_sent, overflow_saved) = text_res?;

    let artifacts_sent =
        deliver_prepared_qq_artifacts(client, target, msg_id, prepared_artifacts).await;
    Ok(DeliveryStats {
        chunks_sent,
        artifacts_sent,
        overflow_saved,
    })
}

pub(crate) struct QqFollowupParams {
    pub client: Arc<QqClient>,
    pub config: Arc<Config>,
    pub session_mgr: Arc<SessionManager>,
    pub target: OwnedQqTarget,
    pub chat_id: i64,
    pub generation: u64,
    pub msg_id: String,
    pub user_prompt: String,
    pub should_send_voice: bool,
    pub effective_voice_ref: Option<String>,
    pub artifact_roots: Vec<PathBuf>,
    pub scratch_dir: PathBuf,
    pub slots_already_used: usize,
    pub conversation_id: Option<String>,
    pub stream_rx: mpsc::Receiver<StreamEvent>,
    pub shutdown_cancel: CancellationToken,
}

/// Continues draining a detached `agy` process (e.g. an asynchronous `image-generator` subagent
/// that completed after Turn 1's interim reply) and delivers subsequent replies and generated
/// media back to QQ automatically. Cooperatively exits on gateway shutdown.
pub(crate) async fn stream_qq_detached_followups(params: QqFollowupParams) {
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
        scratch_dir,
        mut slots_already_used,
        mut conversation_id,
        mut stream_rx,
        shutdown_cancel,
    } = params;

    let mut pending_text = String::new();
    loop {
        let maybe_event = tokio::select! {
            biased;
            _ = shutdown_cancel.cancelled() => {
                info!(chat_id, generation, "QQ detached follow-up stopping on gateway shutdown");
                break;
            }
            ev = stream_rx.recv() => ev,
        };
        let Some(event) = maybe_event else {
            break;
        };

        match event {
            StreamEvent::Init {
                conversation_id: cid,
            } => {
                conversation_id = Some(cid);
            }
            StreamEvent::ThinkingDelta(_) => {}
            StreamEvent::Checkpoint => {
                info!(chat_id, generation, "QQ detached context checkpoint");
            }
            StreamEvent::ToolUse { tool, summary } => {
                info!(chat_id, tool = %tool, summary = %summary, "QQ detached tool use");
            }
            StreamEvent::ToolError { tool, message } => {
                warn!(chat_id, generation, tool = %tool, error = %message, "QQ detached tool error");
            }
            StreamEvent::SubagentUpdate { summary } => {
                info!(chat_id, generation, summary = %summary, "QQ detached subagent update");
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
                    break;
                }
                let brain_dir =
                    resolve_conversation_brain_dir_async(conversation_id.as_deref()).await;
                match deliver_qq_reply(
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
                    Some(&scratch_dir),
                    slots_already_used,
                    None,
                    None,
                )
                .await
                {
                    Ok(stats) => {
                        slots_already_used = slots_already_used
                            .saturating_add(stats.chunks_sent + stats.artifacts_sent);
                        info!(
                            chat_id,
                            generation,
                            chunks = stats.chunks_sent,
                            artifacts = stats.artifacts_sent,
                            "Delivered QQ detached follow-up turn"
                        );
                    }
                    Err(err) => {
                        error!(
                            chat_id,
                            generation,
                            error = %format!("{err:#}"),
                            "Failed to deliver QQ detached follow-up reply"
                        );
                    }
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
                    match deliver_qq_reply(
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
                        Some(&scratch_dir),
                        slots_already_used,
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(stats) => {
                            info!(
                                chat_id,
                                generation,
                                chunks = stats.chunks_sent,
                                artifacts = stats.artifacts_sent,
                                "Delivered QQ detached final result"
                            );
                        }
                        Err(err) => {
                            error!(
                                chat_id,
                                generation,
                                error = %format!("{err:#}"),
                                "Failed to deliver QQ detached final result"
                            );
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TurnOutcome;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn test_qq_formatter_split_markdown_chunks_handles_cjk_bytes_and_fences() {
        let text = "short text";
        let chunks = QqFormatter::split_markdown_chunks(text, 100, 100);
        assert_eq!(chunks, vec!["short text"]);

        let multi = "Line 1\nLine 2\nLine 3\nLine 4\n";
        let chunks = QqFormatter::split_markdown_chunks(multi, 14, 14);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks.concat(), multi);

        let cjk_1000 = "中".repeat(1000);
        let chunks =
            QqFormatter::split_markdown_chunks(&cjk_1000, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 1);

        let cjk_1500 = "文".repeat(1500);
        let chunks =
            QqFormatter::split_markdown_chunks(&cjk_1500, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), 1400);
        assert_eq!(chunks[1].chars().count(), 100);
        assert_eq!(chunks.concat(), cjk_1500);

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

        let fenced = "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\n```";
        let fenced_chunks = QqFormatter::split_markdown_chunks(fenced, 28, 28);
        assert!(fenced_chunks.len() >= 2);
        assert!(fenced_chunks[0].trim_end().ends_with("```"));
        assert!(fenced_chunks[1].starts_with("```rust\n"));
    }

    #[test]
    fn test_passive_slot_budget_and_long_reply_expansion_and_overflow_plan() {
        // Placeholder takes 1 slot -> 4 text slots left with 0 artifacts; 2 left with 2 artifacts.
        assert_eq!(compute_passive_text_slots(1, 0), 4);
        assert_eq!(compute_passive_text_slots(1, 2), 2);
        assert_eq!(compute_passive_text_slots(1, 5), 1);
        assert_eq!(compute_passive_text_slots(0, 0), 5);

        // 7200 ASCII chars: > 4 * 1400 (5600), but fits inside 4 * 2200 (8800) via expanded chunks.
        let medium_long = "a ".repeat(3600);
        let plan_expanded = plan_qq_text_chunks(&medium_long, 4, true);
        assert!(plan_expanded.expanded_limit_used);
        assert!(plan_expanded.chunks.len() <= 4);
        assert!(plan_expanded.overflow_full_text.is_none());
        assert!(!plan_expanded.reserved_file_slot);

        // 14,000 ASCII chars exceeds even 4 expanded chunks -> reserves 1 file slot + 3 text chunks + notice.
        let very_long = "b ".repeat(7000);
        let plan_overflow = plan_qq_text_chunks(&very_long, 4, true);
        assert!(plan_overflow.expanded_limit_used);
        assert!(plan_overflow.reserved_file_slot);
        assert_eq!(plan_overflow.chunks.len(), 3);
        assert!(plan_overflow.overflow_full_text.is_some());
        assert!(
            plan_overflow
                .chunks
                .last()
                .unwrap()
                .contains("超出 QQ 单轮被动回复上限")
        );
    }

    #[test]
    fn test_message_contains_secret_pattern_detects_credentials_without_false_positives() {
        assert!(message_contains_secret_pattern(
            "帮我看下 auth_token=abc1234567890"
        ));
        assert!(message_contains_secret_pattern(
            "我的 key 是 sk-proj-AbCdEfGhIjKlMnOpQrStUvWxYz"
        ));
        assert!(message_contains_secret_pattern(
            "github token ghp_1234567890abcdef1234"
        ));
        assert!(message_contains_secret_pattern(
            "-----BEGIN PRIVATE KEY-----\nMIIEv..."
        ));
        assert!(!message_contains_secret_pattern(
            "请问 Rust 的 async/await 怎么写？"
        ));
        assert!(!message_contains_secret_pattern(
            "auth_token=<REDACTED> 这是文档示例"
        ));
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
        }

        let deleted_req = tokio::time::timeout(Duration::from_secs(2), del_rx)
            .await
            .expect("Drop must spawn async placeholder deletion")
            .unwrap();
        assert!(deleted_req.contains("DELETE /v2/users/u1/messages/ph_99"));

        server.abort();
    }

    #[tokio::test]
    async fn test_reply_ephemeral_records_bot_status_in_context_cache() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let req_line = req.lines().next().unwrap_or("");
                    let body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#
                    } else {
                        r#"{"id":"bot_ephemeral_1","ext_info":{"ref_idx":"REFIDX_eph_1=="}}"#
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
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let target = QqTarget::Group("GRP_EPH_REC");
        let id = reply_ephemeral(
            &client,
            target,
            "m_in",
            "⏳ 当前会话已有任务正在运行（发送 /cancel 可中断当前任务）",
        )
        .await
        .unwrap();
        assert_eq!(id.as_deref(), Some("bot_ephemeral_1"));

        let cached = context::lookup_cached_message(&target.scope_key(), "REFIDX_eph_1==")
            .expect("ephemeral status message must be recorded in context cache");
        assert_eq!(cached.sender_label, "AI 助手");
        assert!(cached.text.contains("/cancel"));

        server.abort();
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
                    context_tokens: None,
                    num_turns: None,
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: Some(1.0),
                    detached: true,
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
            scratch_dir: scratch.clone(),
            slots_already_used: 2,
            conversation_id: Some("conv-det-1".to_string()),
            stream_rx: rx,
            shutdown_cancel: CancellationToken::new(),
        }));

        tx.send(StreamEvent::TurnComplete {
            text: format!("已将背景替换为纯白。\n[[send_file:{}]]", img.display()),
        })
        .await
        .unwrap();
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
