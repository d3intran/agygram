//! QQ reply delivery, passive 5-slot budget planning, long-reply graceful degradation,
//! placeholder recall guard, credential privacy detection, and detached follow-up streaming.

use super::client::{QqClient, QqTarget, is_passive_or_proactive_limit_error};
use super::context;
use super::formatter::{QqFormatter, render_qq_plain_text, split_qq_plain_chunks};
use super::media::{
    count_planned_artifact_slots, deliver_prepared_qq_artifacts_with_slots,
    prepare_qq_artifact_uploads, sanitize_message_id, send_qq_voice, strip_emotion_tags,
};
use super::types::QqMediaFileType;
use super::{OwnedQqTarget, QqTurnCancelGuard};
use crate::artifacts::{
    collect_explicit_deliverables_with_brain_async, resolve_conversation_brain_dir_async,
};
use crate::config::Config;
use crate::executor::StreamEvent;
use crate::session::SessionManager;
use anyhow::Result;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
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

/// Byte and character headroom reserved for the `\n📁 完整归档: \`scratch/reply-<id>.md\``
/// suffix appended by [`reply_with_budget`] when an overflow reply is archived on disk.
const OVERFLOW_ARCHIVE_NOTE_RESERVE_BYTES: usize = 80;
const OVERFLOW_ARCHIVE_NOTE_RESERVE_CHARS: usize = 64;

/// Delay before sending `"我想想..."` / queue placeholders so fast replies and errors never waste
/// 1 of the 5 passive reply slots or race with the real reply.
const PLACEHOLDER_DELAY: Duration = Duration::from_millis(1200);
const PLACEHOLDER_WAITING: u8 = 0;
const PLACEHOLDER_SENDING: u8 = 1;
const PLACEHOLDER_CANCELLED: u8 = 2;

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

fn strip_quote_prefix(line: &str) -> &str {
    let mut s = line;
    loop {
        let trimmed = s.trim_start_matches([' ', '\t']);
        if let Some(rest) = trimmed.strip_prefix('>') {
            s = rest.strip_prefix(' ').unwrap_or(rest);
        } else {
            return trimmed;
        }
    }
}

fn parse_fence_opener(line: &str) -> Option<(char, usize)> {
    let trimmed = strip_quote_prefix(line);
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|&b| b == first).count();
    if count < 3 {
        return None;
    }
    if first == b'`' && trimmed.as_bytes()[count..].contains(&b'`') {
        return None;
    }
    Some((first as char, count))
}

fn is_fence_closer(line: &str, fence_char: char, min_len: usize) -> bool {
    let trimmed = strip_quote_prefix(line);
    let fence_byte = fence_char as u8;
    let count = trimmed.bytes().take_while(|&b| b == fence_byte).count();
    count >= min_len && trimmed[count..].trim_matches([' ', '\t']).is_empty()
}

/// Returns the active unclosed Markdown code fence `(fence_char, fence_len)` at the end of `s`,
/// if any.
fn open_fence_at_end(s: &str) -> Option<(char, usize)> {
    let mut open: Option<(char, usize)> = None;
    for raw_line in s.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if let Some((f_char, f_len)) = open {
            if is_fence_closer(line, f_char, f_len) {
                open = None;
            }
        } else if let Some(opened) = parse_fence_opener(line) {
            open = Some(opened);
        }
    }
    open
}

/// Returns the longest UTF-8 prefix of `s` bounded by both `max_chars` and `max_bytes`.
fn slice_within_limits(s: &str, max_chars: usize, max_bytes: usize) -> &str {
    if s.len() <= max_bytes && (s.len() <= max_chars || s.chars().count() <= max_chars) {
        return s;
    }
    let mut end = 0usize;
    for (chars, (idx, ch)) in s.char_indices().enumerate() {
        let next_end = idx + ch.len_utf8();
        if next_end > max_bytes || chars >= max_chars {
            break;
        }
        end = next_end;
    }
    &s[..end]
}

/// Truncates `chunk` to fit within `(max_chars, max_bytes)` before appending an overflow suffix,
/// cleanly closing any open Markdown code fence when `close_markdown_fence` is true.
fn truncate_chunk_for_suffix(
    chunk: &str,
    max_chars: usize,
    max_bytes: usize,
    close_markdown_fence: bool,
) -> String {
    let mut prefix = slice_within_limits(chunk, max_chars, max_bytes);
    if !close_markdown_fence {
        return prefix.trim_end().to_string();
    }

    for _ in 0..4 {
        let Some((fence_ch, fence_len)) = open_fence_at_end(prefix) else {
            return prefix.trim_end().to_string();
        };
        let trimmed = prefix.trim_end_matches(['\r', '\n']);
        let close_needed = fence_len + usize::from(!trimmed.is_empty());
        if trimmed.len() + close_needed <= max_bytes
            && trimmed.chars().count() + close_needed <= max_chars
        {
            let mut out = String::with_capacity(trimmed.len() + close_needed);
            out.push_str(trimmed);
            if !trimmed.is_empty() {
                out.push('\n');
            }
            for _ in 0..fence_len {
                out.push(fence_ch);
            }
            return out;
        }
        let next_chars = max_chars.saturating_sub(close_needed);
        let next_bytes = max_bytes.saturating_sub(close_needed);
        let shrunk = slice_within_limits(trimmed, next_chars, next_bytes);
        if shrunk.len() == prefix.len() {
            return shrunk.trim_end().to_string();
        }
        prefix = shrunk;
    }

    prefix.trim_end().to_string()
}

fn plan_qq_chunks_inner(
    rendered: &str,
    text_slots: usize,
    has_scratch_dir: bool,
    is_markdown: bool,
) -> ReplyChunkPlan {
    let slots = text_slots.max(1);
    let standard = if is_markdown {
        QqFormatter::split_markdown_chunks(rendered, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES)
    } else {
        split_qq_plain_chunks(rendered, QQ_MAX_CHUNK_CHARS, QQ_MAX_CHUNK_BYTES)
    };
    if standard.len() <= slots {
        return ReplyChunkPlan {
            chunks: standard,
            overflow_full_text: None,
            expanded_limit_used: false,
            reserved_file_slot: false,
        };
    }

    let expanded = if is_markdown {
        QqFormatter::split_markdown_chunks(
            rendered,
            QQ_EXPANDED_CHUNK_CHARS,
            QQ_EXPANDED_CHUNK_BYTES,
        )
    } else {
        split_qq_plain_chunks(rendered, QQ_EXPANDED_CHUNK_CHARS, QQ_EXPANDED_CHUNK_BYTES)
    };
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
    let total_chars = rendered.chars().count();
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
        let archive_reserve_bytes = if has_scratch_dir {
            OVERFLOW_ARCHIVE_NOTE_RESERVE_BYTES
        } else {
            0
        };
        let archive_reserve_chars = if has_scratch_dir {
            OVERFLOW_ARCHIVE_NOTE_RESERVE_CHARS
        } else {
            0
        };
        let budget_bytes =
            QQ_EXPANDED_CHUNK_BYTES.saturating_sub(suffix.len() + archive_reserve_bytes);
        let budget_chars =
            QQ_EXPANDED_CHUNK_CHARS.saturating_sub(suffix.chars().count() + archive_reserve_chars);
        let needs_trunc = last.len() > budget_bytes
            || last.chars().count() > budget_chars
            || (is_markdown && open_fence_at_end(last).is_some());
        if needs_trunc {
            *last = truncate_chunk_for_suffix(last, budget_chars, budget_bytes, is_markdown);
        }
        last.push_str(&suffix);
    }

    ReplyChunkPlan {
        chunks: kept,
        overflow_full_text: Some(rendered.to_string()),
        expanded_limit_used: true,
        reserved_file_slot: reserve_file_slot,
    }
}

/// Plans Markdown chunks within `text_slots`:
/// 1. Standard `1400 chars / 4500 B` split if it fits within `text_slots`.
/// 2. Expanded `2200 chars / 6500 B` split to compress into `text_slots` without hitting `40034128`.
/// 3. If still over budget, reserves 1 slot for `scratch/reply-<id>.md` (when `text_slots >= 2`
///    and a `scratch_dir` is available) or appends an explicit truncation notice while enforcing
///    both char and byte limits (including archive-note headroom) and closing open code fences.
#[must_use]
pub(crate) fn plan_qq_text_chunks(
    formatted_md: &str,
    text_slots: usize,
    has_scratch_dir: bool,
) -> ReplyChunkPlan {
    plan_qq_chunks_inner(formatted_md, text_slots, has_scratch_dir, true)
}

/// Plans fence-free plain-text chunks within `text_slots` for single-pass delivery when QQ
/// Markdown is disabled.
#[must_use]
pub(crate) fn plan_qq_plain_chunks(
    plain_text: &str,
    text_slots: usize,
    has_scratch_dir: bool,
) -> ReplyChunkPlan {
    plan_qq_chunks_inner(plain_text, text_slots, has_scratch_dir, false)
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

/// RAII guard that delays `"我想想..."` / queue-notice placeholders by ~1.2s with a cancellable
/// timer so fast replies/errors never waste 1/5 passive reply slots or race with the real reply,
/// while always recalling placeholders that did get sent even on early `?`, cancellation, or panic.
pub(crate) struct PlaceholderGuard {
    client: Arc<QqClient>,
    target: OwnedQqTarget,
    cancel: CancellationToken,
    state: Arc<AtomicU8>,
    pending_send: Option<tokio::task::JoinHandle<Option<String>>>,
}

impl PlaceholderGuard {
    #[cfg(test)]
    pub(crate) fn new(
        client: Arc<QqClient>,
        target: QqTarget<'_>,
        message_id: Option<String>,
    ) -> Self {
        let initial_state = if message_id.is_some() {
            PLACEHOLDER_SENDING
        } else {
            PLACEHOLDER_CANCELLED
        };
        let handle = message_id.map(|id| tokio::spawn(async move { Some(id) }));
        Self {
            client,
            target: OwnedQqTarget::from_target(target),
            cancel: CancellationToken::new(),
            state: Arc::new(AtomicU8::new(initial_state)),
            pending_send: handle,
        }
    }

    pub(crate) fn spawn(
        client: Arc<QqClient>,
        target: QqTarget<'_>,
        msg_id: &str,
        text: &'static str,
    ) -> Self {
        Self::spawn_with_delay(client, target, msg_id, text, PLACEHOLDER_DELAY)
    }

    pub(crate) fn spawn_with_delay(
        client: Arc<QqClient>,
        target: QqTarget<'_>,
        msg_id: &str,
        text: &'static str,
        delay: Duration,
    ) -> Self {
        let owned_target = OwnedQqTarget::from_target(target);
        let cancel = CancellationToken::new();
        let state = Arc::new(AtomicU8::new(PLACEHOLDER_WAITING));
        let client_c = client.clone();
        let target_c = owned_target.clone();
        let cancel_c = cancel.clone();
        let state_c = state.clone();
        let msg_id_owned = msg_id.to_string();
        let handle = tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::select! {
                    biased;
                    _ = cancel_c.cancelled() => {
                        let _ = state_c.compare_exchange(
                            PLACEHOLDER_WAITING,
                            PLACEHOLDER_CANCELLED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        return None;
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if state_c
                .compare_exchange(
                    PLACEHOLDER_WAITING,
                    PLACEHOLDER_SENDING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return None;
            }
            reply_ephemeral(&client_c, target_c.as_target(), &msg_id_owned, text)
                .await
                .ok()
                .flatten()
        });
        Self {
            client,
            target: owned_target,
            cancel,
            state,
            pending_send: Some(handle),
        }
    }

    /// Suppresses the delayed placeholder if its timer has not elapsed yet.
    /// Returns `1` if sending has already started (consuming 1 passive slot) or `0` if suppressed
    /// in time (preserving all 5 passive slots for the real reply).
    pub(crate) fn suppress_if_unsent(&self) -> usize {
        let _ = self.state.compare_exchange(
            PLACEHOLDER_WAITING,
            PLACEHOLDER_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.cancel.cancel();
        usize::from(self.state.load(Ordering::Acquire) == PLACEHOLDER_SENDING)
    }

    /// Schedules asynchronous placeholder recall without delaying reply completion or releasing the
    /// turn slot. Skips spawning a recall task altogether if the delayed placeholder was never sent.
    pub(crate) fn spawn_cleanup(mut self) {
        self.schedule_recall();
    }

    fn schedule_recall(&mut self) {
        let used_slot = self.suppress_if_unsent();
        let Some(handle) = self.pending_send.take() else {
            return;
        };
        if used_slot == 0 {
            return;
        }
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
/// Uses a single-pass plain-text render + chunk path when Markdown is disabled, widens chunk
/// bounds when needed, and degrades ultra-long replies to a saved `scratch/reply-<id>.md` file
/// attachment instead of hitting `40034128` / `40034105`.
pub(crate) async fn reply_with_budget(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
    text: &str,
    slots_already_used: usize,
    planned_artifact_slots: usize,
    scratch_dir: Option<&Path>,
) -> Result<(Option<String>, usize, bool)> {
    let trimmed_input = text.trim_end();
    let plain_mode = client.is_markdown_disabled();
    let rendered = if plain_mode {
        render_qq_plain_text(trimmed_input)
    } else {
        QqFormatter::format_markdown(trimmed_input)
    };
    let rendered_trimmed = rendered.trim_end();
    let scope = target.scope_key();
    let text_slots = compute_passive_text_slots(slots_already_used, planned_artifact_slots);
    let mut plan = if plain_mode {
        plan_qq_plain_chunks(rendered_trimmed, text_slots, scratch_dir.is_some())
    } else {
        plan_qq_text_chunks(rendered_trimmed, text_slots, scratch_dir.is_some())
    };

    let archive_source = if plain_mode {
        trimmed_input
    } else {
        rendered_trimmed
    };
    let mut overflow_file: Option<PathBuf> = None;
    let mut overflow_saved = false;
    if plan.overflow_full_text.is_some()
        && let Some(dir) = scratch_dir
        && let Some(saved_path) = save_overflow_reply_markdown(dir, msg_id, archive_source).await
    {
        overflow_saved = true;
        if let Some(name) = saved_path.file_name().and_then(|n| n.to_str())
            && let Some(last) = plan.chunks.last_mut()
        {
            if plain_mode {
                let _ = write!(last, "\n📁 完整归档: scratch/{name}");
            } else {
                let _ = write!(last, "\n📁 完整归档: `scratch/{name}`");
            }
        }
        overflow_file = Some(saved_path);
    }

    if plan.expanded_limit_used {
        info!(
            target_kind = target.kind_label(),
            plain_mode,
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
        let send_res = if plain_mode {
            client
                .send_plain_message_with_meta(target, &chunk, msg_id)
                .await
        } else {
            client.send_message_with_meta(target, &chunk, msg_id).await
        };
        match send_res {
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
                        && save_overflow_reply_markdown(dir, msg_id, archive_source)
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
        final_text.trim_end(),
        artifact_roots,
        active_brain_dir,
    )
    .await;
    let display_text: &str = if cleaned_reply.trim().is_empty() && !explicit_artifacts.is_empty() {
        "📎 已发送请求的文件。"
    } else {
        cleaned_reply.trim_end()
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
            // Always run zero-alloc `strip_emotion_tags` on QQ text replies so leaked `[calm]`
            // cues never reach the chat after turning voice mode off.
            let cleaned = strip_emotion_tags(display_text);
            let base_text: Cow<'_, str> = if should_send_voice && config.fish_api_key.is_none() {
                Cow::Owned(format!(
                    "{}\n\n（未配置 fish_api_key，本条为文字回复）",
                    cleaned.trim_end()
                ))
            } else {
                cleaned
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
            composed.push_str(base_text.trim_end());
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
                if let Ok((_, sent, _)) = reply_with_budget(
                    client,
                    target,
                    msg_id,
                    &joined,
                    slots_already_used.saturating_add(chunks_sent),
                    planned_artifact_slots,
                    None,
                )
                .await
                {
                    chunks_sent += sent;
                }
            }
        }
        Ok::<(usize, bool), anyhow::Error>((chunks_sent, overflow_saved))
    };

    let preupload_fut = prepare_qq_artifact_uploads(client, target, &explicit_artifacts);
    let (text_res, prepared_artifacts) = tokio::join!(text_delivery_fut, preupload_fut);
    let (chunks_sent, overflow_saved) = text_res?;

    let slots_before_artifacts = slots_already_used.saturating_add(chunks_sent);
    let artifacts_sent = deliver_prepared_qq_artifacts_with_slots(
        client,
        target,
        msg_id,
        prepared_artifacts,
        slots_before_artifacts,
    )
    .await;
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
    pub turn_cancel: CancellationToken,
    pub shutdown_cancel: CancellationToken,
    pub cancel_guard: Option<QqTurnCancelGuard>,
}

/// Continues draining a detached `agy` process (e.g. an asynchronous `image-generator` subagent
/// that completed after Turn 1's interim reply) and delivers subsequent replies and generated
/// media back to QQ automatically. Cooperatively exits on gateway shutdown or per-chat cancel
/// (`/cancel`, `/new`), and propagates final `Checkpoint` / context watermarks into compaction.
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
        turn_cancel,
        shutdown_cancel,
        cancel_guard: _cancel_guard,
    } = params;

    let mut pending_text = String::new();
    let mut followup_saw_checkpoint = false;
    let mut followup_context_tokens: Option<u64> = None;
    let mut saw_final_result = false;
    let mut cancelled = false;
    loop {
        let maybe_event = tokio::select! {
            biased;
            _ = shutdown_cancel.cancelled() => {
                info!(chat_id, generation, "QQ detached follow-up stopping on gateway shutdown");
                cancelled = true;
                break;
            }
            _ = turn_cancel.cancelled() => {
                info!(chat_id, generation, "QQ detached follow-up stopping on chat cancel");
                cancelled = true;
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
                let trimmed = cid.trim();
                if !trimmed.is_empty() {
                    conversation_id = Some(trimmed.to_string());
                }
            }
            StreamEvent::ThinkingDelta(_) => {}
            StreamEvent::Checkpoint => {
                followup_saw_checkpoint = true;
                info!(chat_id, generation, "QQ detached context checkpoint");
                session_mgr
                    .record_detached_followup_result(chat_id, generation, None, true)
                    .await;
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
                let turn_trimmed = turn_text.trim_end();
                if turn_trimmed.is_empty() {
                    continue;
                }
                if turn_cancel.is_cancelled()
                    || !session_mgr.is_generation_current(chat_id, generation).await
                {
                    info!(
                        chat_id,
                        generation,
                        "QQ detached turn superseded or cancelled, stopping follow-up stream"
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
                    turn_trimmed,
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
                saw_final_result = true;
                if let Some(cid) = res
                    .conversation_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    conversation_id = Some(cid.to_string());
                }
                if res.context_tokens.is_some() {
                    followup_context_tokens = res.context_tokens;
                }
                followup_saw_checkpoint |= res.saw_checkpoint;
                if !res.status.eq_ignore_ascii_case("SUCCESS") {
                    warn!(
                        chat_id,
                        generation,
                        status = %res.status,
                        finished_reason = ?res.finished_reason,
                        "QQ detached follow-up finished with non-SUCCESS status"
                    );
                }
                session_mgr
                    .record_detached_followup_result(
                        chat_id,
                        generation,
                        followup_context_tokens,
                        followup_saw_checkpoint,
                    )
                    .await;
                let final_text = if !res.response.trim().is_empty() {
                    pending_text.clear();
                    res.response
                } else {
                    std::mem::take(&mut pending_text)
                };
                let final_trimmed = final_text.trim_end();
                if !final_trimmed.is_empty()
                    && !turn_cancel.is_cancelled()
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
                        final_trimmed,
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
                if !turn_cancel.is_cancelled()
                    && session_mgr.is_generation_current(chat_id, generation).await
                {
                    let _ = reply_with_budget(
                        &client,
                        target.as_target(),
                        &msg_id,
                        &format!("⚠️ 后台任务异常: {err}"),
                        slots_already_used,
                        0,
                        None,
                    )
                    .await;
                }
                break;
            }
        }
    }

    session_mgr.clear_detached_pid(chat_id, generation).await;

    if !cancelled
        && (saw_final_result || followup_context_tokens.is_some() || followup_saw_checkpoint)
        && session_mgr.is_generation_current(chat_id, generation).await
    {
        let post_session = session_mgr.get_or_create(chat_id).await;
        crate::compaction::maybe_schedule_background_compact(
            session_mgr.clone(),
            config.clone(),
            chat_id,
            &post_session,
            false,
            false,
            followup_saw_checkpoint,
        );
    }
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
                    status: "SUCCESS",
                    input_chars: 10,
                    output_chars: 10,
                    usage: None,
                    usage_is_cumulative: false,
                    context_tokens: None,
                    num_turns: None,
                    saw_checkpoint: false,
                    system_prompt_sent: false,
                    injected_system_prompt_hash: None,
                    injected_voice_guide: false,
                    injected_widget_guide: false,
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
            turn_cancel: CancellationToken::new(),
            shutdown_cancel: CancellationToken::new(),
            cancel_guard: None,
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

    #[test]
    fn test_plan_qq_text_chunks_enforces_byte_reserve_and_closes_open_fence() {
        // Multi-byte CJK overflow: 3 bytes/char would previously blow past QQ_EXPANDED_CHUNK_BYTES.
        let cjk_overflow = "中".repeat(5000);
        let plan_cjk = plan_qq_text_chunks(&cjk_overflow, 2, true);
        assert!(plan_cjk.overflow_full_text.is_some());
        assert!(plan_cjk.reserved_file_slot);
        assert_eq!(plan_cjk.chunks.len(), 1);
        let last_cjk = plan_cjk.chunks.last().unwrap();
        assert!(
            last_cjk.len() + OVERFLOW_ARCHIVE_NOTE_RESERVE_BYTES <= QQ_EXPANDED_CHUNK_BYTES,
            "CJK overflow chunk ({} B) must leave archive note reserve inside {} B",
            last_cjk.len(),
            QQ_EXPANDED_CHUNK_BYTES
        );
        assert!(
            last_cjk.chars().count() + OVERFLOW_ARCHIVE_NOTE_RESERVE_CHARS
                <= QQ_EXPANDED_CHUNK_CHARS
        );

        // Code fence overflow: truncating inside a ```rust block must close the fence cleanly
        // before appending the overflow notice.
        let mut fenced = String::from("```rust\n");
        for i in 0..900 {
            let _ = writeln!(fenced, "let value_{i} = \"中文代码行内容\";");
        }
        fenced.push_str("```\n");
        let plan_fence = plan_qq_text_chunks(&fenced, 2, true);
        assert!(plan_fence.overflow_full_text.is_some());
        let last_fence = plan_fence.chunks.last().unwrap();
        assert!(last_fence.len() + OVERFLOW_ARCHIVE_NOTE_RESERVE_BYTES <= QQ_EXPANDED_CHUNK_BYTES);
        assert_eq!(
            open_fence_at_end(last_fence),
            None,
            "truncated overflow chunk must not leave an unclosed code fence"
        );
        let notice_pos = last_fence
            .find("…（后续约")
            .expect("overflow notice must be present");
        assert_eq!(
            open_fence_at_end(&last_fence[..notice_pos]),
            None,
            "code fence must be closed BEFORE the overflow notice"
        );
    }

    #[tokio::test]
    async fn test_placeholder_guard_delayed_cancel_skips_send_and_slot_on_fast_turn() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reqs_srv = requests.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let reqs = reqs_srv.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let req_line = req.lines().next().unwrap_or("").to_string();
                    let body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#
                    } else {
                        reqs.lock().unwrap().push(req_line);
                        r#"{"id":"ph_delayed_1"}"#
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

        // Fast turn: suppressed before delay elapses -> 0 slots consumed, 0 HTTP calls.
        let fast_guard = PlaceholderGuard::spawn_with_delay(
            client.clone(),
            QqTarget::C2c("u_fast"),
            "m_fast",
            "我想想...",
            Duration::from_millis(250),
        );
        assert_eq!(fast_guard.suppress_if_unsent(), 0);
        fast_guard.spawn_cleanup();
        tokio::time::sleep(Duration::from_millis(320)).await;
        assert!(
            requests.lock().unwrap().is_empty(),
            "suppressed delayed placeholder must send neither POST nor DELETE"
        );

        // Slow turn: delay elapses -> sends placeholder (1 slot) and recalls it on cleanup.
        let slow_guard = PlaceholderGuard::spawn_with_delay(
            client,
            QqTarget::C2c("u_slow"),
            "m_slow",
            "我想想...",
            Duration::from_millis(20),
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let has_post = requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.starts_with("POST /v2/users/u_slow/messages "));
            if has_post || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(slow_guard.suppress_if_unsent(), 1);
        slow_guard.spawn_cleanup();

        let del_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let has_delete = requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.starts_with("DELETE /v2/users/u_slow/messages/ph_delayed_1 "));
            if has_delete || tokio::time::Instant::now() >= del_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        let logged = requests.lock().unwrap();
        assert!(
            logged
                .iter()
                .any(|r| r.starts_with("POST /v2/users/u_slow/messages "))
        );
        assert!(
            logged
                .iter()
                .any(|r| r.starts_with("DELETE /v2/users/u_slow/messages/ph_delayed_1 "))
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_single_pass_plain_delivery_and_emotion_strip_without_voice_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let cap_srv = captured.clone();

        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let cap = cap_srv.clone();
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

                    let (status_line, resp_body) =
                        if req_line.starts_with("POST /app/getAppAccessToken ") {
                            (
                                "HTTP/1.1 200 OK",
                                r#"{"access_token":"tok","expires_in":7200}"#,
                            )
                        } else {
                            cap.lock().unwrap().push(body.clone());
                            if body.contains(r#""msg_type":2"#) {
                                (
                                    "HTTP/1.1 400 Bad Request",
                                    r#"{"code":50006,"message":"markdown not allowed"}"#,
                                )
                            } else {
                                ("HTTP/1.1 200 OK", r#"{"id":"msg_plain_ok"}"#)
                            }
                        };

                    let raw = format!(
                        "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        let base = format!("http://{addr}");
        let client = QqClient::new_for_test("id", "sec", &base, &base);
        let target = QqTarget::C2c("u_plain");

        // First call probes Markdown -> rejected with 50006 -> disables Markdown on client.
        let _ = reply(&client, target, "m_probe", "**init**").await.unwrap();
        assert!(client.is_markdown_disabled());
        captured.lock().unwrap().clear();

        // Second call via deliver_qq_reply with `should_send_voice = false` must:
        // 1) take single-pass plain path (`msg_type: 0` directly, ZERO `msg_type: 2` probes),
        // 2) strip leaked `[calm]` emotion tag, and
        // 3) trim trailing excessive newlines.
        let cfg = Config::default();
        let stats = deliver_qq_reply(
            &client,
            &cfg,
            target,
            -1001,
            "m_plain",
            "hi",
            "[calm] **已完成** 处理。\n\n\n\n",
            &[],
            false,
            None,
            &[],
            None,
            None,
            0,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(stats.chunks_sent, 1);

        let sent = captured.lock().unwrap();
        assert_eq!(
            sent.len(),
            1,
            "disabled Markdown must skip msg_type=2 probe"
        );
        assert!(sent[0].contains(r#""msg_type":0"#));
        assert!(!sent[0].contains("[calm]"));
        assert!(!sent[0].contains("**"));
        assert!(sent[0].contains("已完成 处理。"));
        assert!(!sent[0].contains(r"\n\n\n"));

        server.abort();
    }

    #[tokio::test]
    async fn test_detached_qq_followup_honors_turn_cancel_and_propagates_checkpoint() {
        let dir = std::env::temp_dir().join(format!(
            "agygram_qq_detached_cancel_test_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let client = Arc::new(QqClient::new_for_test(
            "id",
            "sec",
            "http://127.0.0.1:1",
            "http://127.0.0.1:1",
        ));
        let config = Arc::new(Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            auto_compact_threshold_tokens: 0,
            ..Config::default()
        });
        let session_mgr = Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ));

        let chat_id = -424243_i64;
        let turn_gen = session_mgr.begin_turn(chat_id).await.unwrap();
        let _ = session_mgr
            .finish_turn(
                chat_id,
                turn_gen,
                TurnOutcome {
                    conversation_id: Some("conv-det-cp".to_string()),
                    status: "SUCCESS",
                    input_chars: 10,
                    output_chars: 10,
                    usage: None,
                    usage_is_cumulative: false,
                    context_tokens: Some(180_000),
                    num_turns: Some(1),
                    saw_checkpoint: false,
                    system_prompt_sent: true,
                    injected_system_prompt_hash: Some(0xDEAD_BEEF),
                    injected_voice_guide: false,
                    injected_widget_guide: false,
                    duration_seconds: Some(1.0),
                    detached: true,
                },
            )
            .await;

        let (tx, rx) = mpsc::channel::<StreamEvent>(8);
        let turn_cancel = CancellationToken::new();
        let handle = tokio::spawn(stream_qq_detached_followups(QqFollowupParams {
            client,
            config,
            session_mgr: session_mgr.clone(),
            target: OwnedQqTarget::C2c("u_cancel".to_string()),
            chat_id,
            generation: turn_gen,
            msg_id: "m_det".to_string(),
            user_prompt: "run async".to_string(),
            should_send_voice: false,
            effective_voice_ref: None,
            artifact_roots: vec![dir.clone()],
            scratch_dir: dir.clone(),
            slots_already_used: 1,
            conversation_id: Some("conv-det-cp".to_string()),
            stream_rx: rx,
            turn_cancel: turn_cancel.clone(),
            shutdown_cancel: CancellationToken::new(),
            cancel_guard: None,
        }));

        // Emit Checkpoint in detached stream -> clears cached system prompt hash & watermark.
        tx.send(StreamEvent::Checkpoint).await.unwrap();
        let cp_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let post = session_mgr.get_or_create(chat_id).await;
            if post.last_system_prompt_hash.is_none() || tokio::time::Instant::now() >= cp_deadline
            {
                assert_eq!(post.last_system_prompt_hash, None);
                break;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Per-chat turn_cancel immediately stops detached follower even while `tx` stays open.
        turn_cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("detached follow-up must stop promptly on turn_cancel")
            .unwrap();

        drop(tx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
