//! Proactive notification daemon for agygram.
//!
//! Monitors genuinely user-facing background notifications (such as scheduled timers/reminders)
//! generated in Antigravity Brain workspaces
//! (`~/.gemini/antigravity-cli/brain/<conversation_id>/.system_generated/messages/`).
//!
//! Design invariants:
//! 1. **Read-Only Respect for `agy`'s `read.json`**: Antigravity CLI owns `read.json` and marks
//!    mid-turn background tasks (`task-XXX`) as `true` when the active `agy` turn consumes them.
//!    `agygram` reloads `read.json` on every directory change and NEVER writes to `read.json`,
//!    maintaining its own separate ledger (`agygram_notified.json`) instead.
//! 2. **Strict Internal Tool Plumbing Filter**: Internal LLM wakeup payloads (`run_command`,
//!    `invoke_subagent`, `swarm`, `run_workflow`, `Task id ".../task-N" finished with result:`)
//!    are silently recorded in `agygram_notified.json` and never dumped onto Telegram chats.
//! 3. **Bootstrap Flood Guard & Single-Card Cap**: On first creation of `agygram_notified.json`,
//!    historical files older than 90s are silently baselined; legitimate notifications are capped
//!    to a single compact expandable card so they never multi-message flood `(continues...)`.

use crate::bot::formatter::TelegramFormatter;
use crate::session::SessionManager;
use crate::text;
use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const NOTIFIER_POLL_INTERVAL: Duration = Duration::from_secs(2);
const MISSING_DIR_PROBE_BACKOFF: Duration = Duration::from_secs(6);
const BOOTSTRAP_MAX_MESSAGE_AGE: Duration = Duration::from_secs(90);
const PARTIAL_JSON_GRACE: Duration = Duration::from_secs(15);
const MAX_TRANSIENT_RETRIES: u32 = 5;
const TRANSIENT_BACKOFF_BASE_SECS: u64 = 4;
const TRANSIENT_BACKOFF_MAX_SECS: u64 = 120;
const MAX_CACHED_CONVERSATIONS: usize = 256;
const MAX_NOTIFICATION_BODY_CHARS: usize = 2600;
const AGY_READ_LEDGER_FILE: &str = "read.json";
const AGYGRAM_NOTIFIED_FILE: &str = "agygram_notified.json";

static LEDGER_SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Debug, Clone)]
struct BrainMessage {
    id: String,
    #[serde(default, rename = "hideFromUser")]
    hide_from_user: bool,
    #[serde(default, rename = "renderDetails")]
    render_details: Option<RenderDetails>,
    #[serde(default, rename = "sourceMetadata")]
    source_metadata: Option<SourceMetadata>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct RenderDetails {
    #[serde(rename = "messageTitle")]
    message_title: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct SourceMetadata {
    #[serde(default)]
    tool: Option<ToolSourceMetadata>,
}

#[derive(Deserialize, Debug, Clone)]
struct ToolSourceMetadata {
    #[serde(default, rename = "toolCall")]
    tool_call: Option<ToolCallMetadata>,
}

#[derive(Deserialize, Debug, Clone)]
struct ToolCallMetadata {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryOutcome {
    Delivered,
    PermanentError,
    TransientError,
}

#[derive(Default)]
struct ConversationCache {
    messages_dir: Option<PathBuf>,
    next_missing_probe: Option<tokio::time::Instant>,
    last_dir_mtime: Option<SystemTime>,
    notified_ledger: HashMap<String, bool>,
    notified_loaded: bool,
    is_fresh_bootstrap: bool,
    processed_files: FxHashSet<String>,
    failed_attempts: FxHashMap<String, (u32, tokio::time::Instant)>,
    has_pending: bool,
}

/// Resolves the system messages directory for a conversation ID across all active brain
/// directories using non-blocking `tokio::fs`.
async fn get_brain_messages_dir_async(conv_id: &str) -> Option<PathBuf> {
    for brain_dir in crate::executor::AgyExecutor::get_brain_dirs_async().await {
        let path = brain_dir
            .join(conv_id)
            .join(".system_generated")
            .join("messages");
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|meta| meta.is_dir())
        {
            return Some(path);
        }
    }
    None
}

/// Loads a JSON `id -> bool` ledger file, returning `(map, file_existed_on_disk)`.
async fn load_json_ledger(path: &Path) -> (HashMap<String, bool>, bool) {
    match tokio::fs::read_to_string(path).await {
        Ok(content) => match serde_json::from_str::<HashMap<String, bool>>(&content) {
            Ok(map) => (map, true),
            Err(err) => {
                warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to parse notification ledger JSON"
                );
                (HashMap::new(), true)
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (HashMap::new(), false),
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "failed to read notification ledger"
            );
            (HashMap::new(), false)
        }
    }
}

/// Reads Antigravity CLI's own `read.json` ledger (read-only; never written by `agygram`).
async fn load_agy_read_ledger(dir: &Path) -> HashMap<String, bool> {
    load_json_ledger(&dir.join(AGY_READ_LEDGER_FILE)).await.0
}

/// Reads `agygram`'s dedicated `agygram_notified.json` ledger, returning `(map, existed)`.
async fn load_notified_ledger(dir: &Path) -> (HashMap<String, bool>, bool) {
    load_json_ledger(&dir.join(AGYGRAM_NOTIFIED_FILE)).await
}

/// Atomically persists `agygram_notified.json` without ever touching `agy`'s `read.json`.
async fn save_notified_ledger(dir: &Path, map: &HashMap<String, bool>) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let target = dir.join(AGYGRAM_NOTIFIED_FILE);
    let seq = LEDGER_SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("agygram_notified.tmp-{}-{seq}", std::process::id()));

    let bytes = serde_json::to_vec(map).context("serialize agygram_notified ledger")?;

    let write_res = async {
        let mut file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(&bytes)
            .await
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .await
            .with_context(|| format!("sync {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &target)
            .await
            .with_context(|| format!("rename {} -> {}", tmp.display(), target.display()))?;
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Err(ref err) = write_res {
        warn!(
            target = %target.display(),
            error = %err,
            "failed to persist agygram_notified ledger"
        );
        if let Err(rm_err) = tokio::fs::remove_file(&tmp).await
            && rm_err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                tmp = %tmp.display(),
                error = %rm_err,
                "failed to clean up temporary agygram_notified ledger"
            );
        }
    }

    write_res
}

/// Returns `true` if `msg` is internal Antigravity CLI tool/subagent/swarm wakeup plumbing
/// intended for the LLM (`<SYSTEM_MESSAGE>`) rather than a user-facing Telegram alert.
fn is_internal_agent_plumbing(msg: &BrainMessage, raw_title: &str, raw_body: &str) -> bool {
    if msg.hide_from_user {
        return true;
    }

    if let Some(tool_name) = msg
        .source_metadata
        .as_ref()
        .and_then(|sm| sm.tool.as_ref())
        .and_then(|t| t.tool_call.as_ref())
        .and_then(|tc| tc.name.as_deref())
        .map(str::trim)
        && matches!(
            tool_name,
            "run_command"
                | "invoke_subagent"
                | "swarm"
                | "run_workflow"
                | "manage_task"
                | "manage_subagents"
                | "send_message"
                | "post_message"
        )
    {
        return true;
    }

    // Defense-in-depth signatures for raw background shell dumps & parent->child agent prompts:
    if raw_body.starts_with("Task id \"") && raw_body.contains("\" finished with result:") {
        return true;
    }
    if raw_title == "Message from Parent Agent" || raw_title.starts_with("Message from @") {
        return true;
    }

    false
}

fn notification_icon(raw_title: &str) -> &'static str {
    let title_lower = raw_title.to_ascii_lowercase();
    if title_lower.contains("timer") || title_lower.contains("schedule") {
        "⏰"
    } else if title_lower.contains("task")
        || title_lower.contains("finished")
        || title_lower.contains("result")
    {
        "📋"
    } else if title_lower.contains("subagent") || title_lower.contains("agent") {
        "🤖"
    } else {
        "🔔"
    }
}

fn format_notification_chunks(raw_title: &str, raw_body: &str) -> Vec<String> {
    let icon = notification_icon(raw_title);
    let escaped_title = text::escape_html(raw_title);
    let trimmed_body = raw_body.trim();
    let capped_body: std::borrow::Cow<'_, str> =
        match text::truncate_chars(trimmed_body, MAX_NOTIFICATION_BODY_CHARS) {
            std::borrow::Cow::Borrowed(s) => std::borrow::Cow::Borrowed(s),
            std::borrow::Cow::Owned(mut s) => {
                s.push_str("\n\n*(完整内容已截断)*");
                std::borrow::Cow::Owned(s)
            }
        };

    let formatted_html = if capped_body.is_empty() {
        format!("{icon} <b>【{escaped_title}】</b>")
    } else {
        format!(
            "{icon} <b>【{escaped_title}】</b>\n\n<blockquote expandable>{}</blockquote>",
            TelegramFormatter::to_telegram_html_for_blockquote(&capped_body)
        )
    };
    TelegramFormatter::split_html_chunks(&formatted_html, 4000)
}

#[inline]
fn is_permanent_telegram_error(err: &teloxide::RequestError) -> bool {
    matches!(
        err,
        teloxide::RequestError::Api(
            teloxide::ApiError::ChatNotFound
                | teloxide::ApiError::BotBlocked
                | teloxide::ApiError::UserDeactivated
        )
    )
}

#[inline]
fn transient_backoff_duration(attempts: u32) -> Duration {
    let exp = attempts.saturating_sub(1).min(5);
    let secs = TRANSIENT_BACKOFF_BASE_SECS
        .saturating_mul(1u64 << exp)
        .min(TRANSIENT_BACKOFF_MAX_SECS);
    Duration::from_secs(secs)
}

async fn is_file_within_partial_grace(path: &Path, grace: Duration) -> bool {
    if grace.is_zero() {
        return false;
    }
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(mtime) {
        Ok(age) => age < grace,
        Err(_) => true,
    }
}

async fn send_chunk_with_retry(
    bot: &Bot,
    chat_id: i64,
    msg_id: &str,
    chunk: &str,
    cancel: &CancellationToken,
) -> DeliveryOutcome {
    let target = ChatId(chat_id);
    let mut attempt = 0u8;
    loop {
        if cancel.is_cancelled() {
            return DeliveryOutcome::TransientError;
        }
        match bot
            .send_message(target, chunk)
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(_) => return DeliveryOutcome::Delivered,
            Err(ref err) if is_permanent_telegram_error(err) => {
                warn!(
                    chat_id,
                    msg_id,
                    error = ?err,
                    "permanent Telegram error delivering proactive notification; skipping permanently"
                );
                return DeliveryOutcome::PermanentError;
            }
            Err(teloxide::RequestError::RetryAfter(retry_secs)) if attempt < 2 => {
                attempt += 1;
                let wait = Duration::from_secs(u64::from(retry_secs.seconds()) + 1);
                warn!(
                    chat_id,
                    msg_id,
                    attempt,
                    wait_secs = wait.as_secs(),
                    "proactive notification hit RetryAfter; backing off"
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return DeliveryOutcome::TransientError,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(teloxide::RequestError::Api(ref api_err)) => {
                warn!(
                    chat_id,
                    msg_id,
                    error = ?api_err,
                    "HTML notification rejected; falling back to plain text"
                );
                let plain = text::strip_html_tags(chunk);
                let trimmed_plain = plain.trim();
                if trimmed_plain.is_empty() {
                    return DeliveryOutcome::Delivered;
                }
                let plain_chunks = crate::bot::pipeline::split_plain_chunks(trimmed_plain, 4000);
                for plain_part in plain_chunks {
                    if let Err(plain_err) = bot.send_message(target, plain_part).await {
                        if is_permanent_telegram_error(&plain_err) {
                            warn!(
                                chat_id,
                                msg_id,
                                error = ?plain_err,
                                "permanent Telegram error on plain-text notification fallback; skipping permanently"
                            );
                            return DeliveryOutcome::PermanentError;
                        }
                        warn!(
                            chat_id,
                            msg_id,
                            error = ?plain_err,
                            "plain-text notification fallback failed"
                        );
                        return DeliveryOutcome::TransientError;
                    }
                }
                return DeliveryOutcome::Delivered;
            }
            Err(err) => {
                warn!(
                    chat_id,
                    msg_id,
                    error = ?err,
                    "failed to deliver proactive notification chunk"
                );
                return DeliveryOutcome::TransientError;
            }
        }
    }
}

async fn deliver_notification_chunks(
    bot: &Bot,
    chat_id: i64,
    msg_id: &str,
    chunks: &[String],
    cancel: &CancellationToken,
) -> DeliveryOutcome {
    for chunk in chunks {
        let outcome = send_chunk_with_retry(bot, chat_id, msg_id, chunk, cancel).await;
        if outcome != DeliveryOutcome::Delivered {
            return outcome;
        }
    }
    DeliveryOutcome::Delivered
}

async fn resolve_cached_messages_dir(
    conv_id: &str,
    cache: &mut ConversationCache,
) -> Option<(PathBuf, Option<SystemTime>)> {
    if let Some(ref dir) = cache.messages_dir {
        match tokio::fs::metadata(dir).await {
            Ok(meta) if meta.is_dir() => {
                return Some((dir.clone(), meta.modified().ok()));
            }
            Ok(_) | Err(_) => {
                cache.messages_dir = None;
                cache.last_dir_mtime = None;
                cache.notified_loaded = false;
                cache.processed_files.clear();
                cache.failed_attempts.clear();
            }
        }
    } else if let Some(next_at) = cache.next_missing_probe
        && tokio::time::Instant::now() < next_at
    {
        return None;
    }

    let Some(dir) = get_brain_messages_dir_async(conv_id).await else {
        cache.next_missing_probe = Some(tokio::time::Instant::now() + MISSING_DIR_PROBE_BACKOFF);
        return None;
    };
    cache.next_missing_probe = None;
    let mtime = tokio::fs::metadata(&dir)
        .await
        .ok()
        .and_then(|meta| meta.modified().ok());
    cache.messages_dir = Some(dir.clone());
    Some((dir, mtime))
}

#[inline]
fn is_ignored_ledger_filename(file_name: &str) -> bool {
    !file_name.ends_with(".json")
        || file_name == AGY_READ_LEDGER_FILE
        || file_name == AGYGRAM_NOTIFIED_FILE
        || file_name.starts_with("read.tmp-")
        || file_name.starts_with("agygram_notified.tmp-")
}

/// Scans and pushes unread user-facing notifications for a single conversation using directory
/// mtime caching while always reloading `agy`'s `read.json` when new files appear.
#[tracing::instrument(skip_all, fields(chat_id, conv_id))]
async fn process_conversation_messages(
    bot: &Bot,
    chat_id: i64,
    conv_id: &str,
    cache: &mut ConversationCache,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some((messages_dir, dir_mtime)) = resolve_cached_messages_dir(conv_id, cache).await else {
        return Ok(());
    };
    process_messages_in_dir(
        bot,
        chat_id,
        conv_id,
        &messages_dir,
        dir_mtime,
        cache,
        cancel,
    )
    .await
}

async fn process_messages_in_dir(
    bot: &Bot,
    chat_id: i64,
    conv_id: &str,
    messages_dir: &Path,
    dir_mtime: Option<SystemTime>,
    cache: &mut ConversationCache,
    cancel: &CancellationToken,
) -> Result<()> {
    process_messages_in_dir_with_delivery(
        chat_id,
        conv_id,
        messages_dir,
        dir_mtime,
        cache,
        cancel,
        PARTIAL_JSON_GRACE,
        |msg_id, chunks| async move {
            deliver_notification_chunks(bot, chat_id, &msg_id, &chunks, cancel).await
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn process_messages_in_dir_with_delivery<F, Fut>(
    chat_id: i64,
    conv_id: &str,
    messages_dir: &Path,
    dir_mtime: Option<SystemTime>,
    cache: &mut ConversationCache,
    cancel: &CancellationToken,
    partial_json_grace: Duration,
    mut deliver: F,
) -> Result<()>
where
    F: FnMut(String, Vec<String>) -> Fut,
    Fut: Future<Output = DeliveryOutcome>,
{
    let mtime_settled = dir_mtime
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= Duration::from_secs(1));

    if mtime_settled
        && cache.notified_loaded
        && !cache.has_pending
        && cache.last_dir_mtime.is_some()
        && cache.last_dir_mtime == dir_mtime
    {
        return Ok(());
    }

    if !cache.notified_loaded {
        let (map, existed) = load_notified_ledger(messages_dir).await;
        cache.notified_ledger = map;
        cache.is_fresh_bootstrap = !existed;
        cache.notified_loaded = true;
    }

    let mut dir_entries = tokio::fs::read_dir(messages_dir)
        .await
        .with_context(|| format!("read_dir {}", messages_dir.display()))?;
    let mut candidate_files: Vec<(String, PathBuf)> = Vec::new();

    loop {
        match dir_entries.next_entry().await {
            Ok(Some(entry)) => {
                let file_name_os = entry.file_name();
                let Some(file_name) = file_name_os.to_str() else {
                    continue;
                };
                if is_ignored_ledger_filename(file_name)
                    || cache.processed_files.contains(file_name)
                {
                    continue;
                }
                candidate_files.push((file_name.to_owned(), entry.path()));
            }
            Ok(None) => break,
            Err(err) => {
                warn!(
                    chat_id,
                    conv_id,
                    dir = %messages_dir.display(),
                    error = %err,
                    "error iterating notification directory"
                );
                break;
            }
        }
    }

    if candidate_files.is_empty() {
        cache.last_dir_mtime = dir_mtime;
        cache.has_pending = false;
        cache.is_fresh_bootstrap = false;
        return Ok(());
    }

    // Always reload Antigravity CLI's own `read.json` whenever candidate files exist so mid-turn
    // tasks already consumed by `agy` are never re-sent after the turn ends.
    let agy_read_ledger = load_agy_read_ledger(messages_dir).await;

    candidate_files.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut ledger_modified = false;
    let mut any_failed = false;

    for (file_name, file_path) in candidate_files {
        if cancel.is_cancelled() {
            any_failed = true;
            break;
        }

        if let Some(&(_, next_retry_at)) = cache.failed_attempts.get(&file_name)
            && tokio::time::Instant::now() < next_retry_at
        {
            any_failed = true;
            continue;
        }

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    chat_id,
                    conv_id,
                    path = %file_path.display(),
                    error = %err,
                    "failed to read notification file"
                );
                any_failed = true;
                continue;
            }
        };

        let msg: BrainMessage = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(err) => {
                if is_file_within_partial_grace(&file_path, partial_json_grace).await {
                    debug!(
                        chat_id,
                        conv_id,
                        path = %file_path.display(),
                        error = %err,
                        "notification JSON incomplete on freshly modified file; deferring retry"
                    );
                    any_failed = true;
                } else {
                    warn!(
                        chat_id,
                        conv_id,
                        path = %file_path.display(),
                        error = %err,
                        "failed to parse stale notification JSON; marking processed"
                    );
                    cache.processed_files.insert(file_name);
                }
                continue;
            }
        };

        // Already notified by `agygram` in an earlier tick.
        if cache.notified_ledger.contains_key(&msg.id) {
            cache.failed_attempts.remove(&file_name);
            cache.processed_files.insert(file_name);
            continue;
        }

        // Already consumed by `agy` CLI during an active turn (`read.json` has `msg.id: true`).
        if agy_read_ledger.get(&msg.id).copied().unwrap_or(false) {
            cache.notified_ledger.insert(msg.id, true);
            cache.failed_attempts.remove(&file_name);
            cache.processed_files.insert(file_name);
            ledger_modified = true;
            continue;
        }

        let raw_title = msg
            .render_details
            .as_ref()
            .and_then(|r| r.message_title.as_deref())
            .unwrap_or("后台通知");
        let raw_body = msg.content.as_deref().unwrap_or("").trim();

        // Filter out internal tool outputs (`run_command` logs, subagent/swarm plumbing) and empty noise.
        if (raw_title.is_empty() && raw_body.is_empty())
            || is_internal_agent_plumbing(&msg, raw_title, raw_body)
        {
            cache.notified_ledger.insert(msg.id, true);
            cache.failed_attempts.remove(&file_name);
            cache.processed_files.insert(file_name);
            ledger_modified = true;
            continue;
        }

        // On first creation of `agygram_notified.json`, silently baseline historical files older than 90s.
        if cache.is_fresh_bootstrap
            && let Ok(meta) = tokio::fs::metadata(&file_path).await
            && let Ok(mtime) = meta.modified()
            && SystemTime::now()
                .duration_since(mtime)
                .is_ok_and(|age| age > BOOTSTRAP_MAX_MESSAGE_AGE)
        {
            cache.notified_ledger.insert(msg.id, true);
            cache.failed_attempts.remove(&file_name);
            cache.processed_files.insert(file_name);
            ledger_modified = true;
            continue;
        }

        let chunks = format_notification_chunks(raw_title, raw_body);
        let chunk_count = chunks.len();
        match deliver(msg.id.clone(), chunks).await {
            DeliveryOutcome::Delivered => {
                info!(
                    chat_id,
                    conv_id,
                    msg_id = %msg.id,
                    chunks = chunk_count,
                    "delivered proactive notification"
                );
                cache.notified_ledger.insert(msg.id, true);
                cache.failed_attempts.remove(&file_name);
                cache.processed_files.insert(file_name);
                ledger_modified = true;
            }
            DeliveryOutcome::PermanentError => {
                warn!(
                    chat_id,
                    conv_id,
                    msg_id = %msg.id,
                    file = %file_name,
                    "permanent notification delivery error; marking processed to stop retry loop"
                );
                cache.notified_ledger.insert(msg.id, true);
                cache.failed_attempts.remove(&file_name);
                cache.processed_files.insert(file_name);
                ledger_modified = true;
            }
            DeliveryOutcome::TransientError => {
                if cancel.is_cancelled() {
                    any_failed = true;
                    break;
                }
                let next_attempts = cache
                    .failed_attempts
                    .get(&file_name)
                    .map_or(1, |(prev, _)| prev.saturating_add(1));
                if next_attempts >= MAX_TRANSIENT_RETRIES {
                    warn!(
                        chat_id,
                        conv_id,
                        msg_id = %msg.id,
                        file = %file_name,
                        attempts = next_attempts,
                        "proactive notification exceeded max transient retries; dropping"
                    );
                    cache.notified_ledger.insert(msg.id, true);
                    cache.failed_attempts.remove(&file_name);
                    cache.processed_files.insert(file_name);
                    ledger_modified = true;
                } else {
                    let backoff = transient_backoff_duration(next_attempts);
                    cache.failed_attempts.insert(
                        file_name,
                        (next_attempts, tokio::time::Instant::now() + backoff),
                    );
                    any_failed = true;
                }
            }
        }
    }

    cache.is_fresh_bootstrap = false;

    if ledger_modified {
        if save_notified_ledger(messages_dir, &cache.notified_ledger)
            .await
            .is_ok()
        {
            cache.last_dir_mtime = tokio::fs::metadata(messages_dir)
                .await
                .ok()
                .and_then(|meta| meta.modified().ok());
        } else {
            cache.last_dir_mtime = dir_mtime;
        }
    } else {
        cache.last_dir_mtime = dir_mtime;
    }

    cache.has_pending = any_failed;
    Ok(())
}

/// Background loop running proactive notification polling until `cancel` is triggered.
pub async fn run_proactive_notifier_with_cancel(
    bot: Bot,
    session_mgr: Arc<SessionManager>,
    cancel: CancellationToken,
) {
    info!("Starting proactive notification daemon for agygram...");
    let mut ticker = tokio::time::interval(NOTIFIER_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut caches: FxHashMap<String, ConversationCache> = FxHashMap::default();

    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                info!("Proactive notification daemon shutting down");
                break;
            }
            _ = ticker.tick() => {}
        }

        let idle_conversations = session_mgr.get_idle_conversations().await;
        if caches.len() > MAX_CACHED_CONVERSATIONS {
            let active_ids: FxHashSet<&str> = idle_conversations
                .iter()
                .map(|(_, cid)| cid.as_str())
                .collect();
            caches.retain(|cid, _| active_ids.contains(cid.as_str()));
        }

        for (chat_id, conv_id) in idle_conversations {
            if cancel.is_cancelled() {
                break;
            }
            if crate::session::is_qq_chat_id(chat_id) {
                continue;
            }
            let cache = caches.entry(conv_id.clone()).or_default();
            if let Err(err) =
                process_conversation_messages(&bot, chat_id, &conv_id, cache, &cancel).await
            {
                debug!(
                    chat_id,
                    conv_id = %conv_id,
                    error = %err,
                    "error processing proactive messages"
                );
            }
        }
    }
}

/// Background loop running proactive notification polling (without external cancellation).
#[allow(dead_code)]
pub async fn run_proactive_notifier(bot: Bot, session_mgr: Arc<SessionManager>) {
    run_proactive_notifier_with_cancel(bot, session_mgr, CancellationToken::new()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_notifications_are_capped_to_single_compact_card_under_4000_chars() {
        let long_body = "This is a detailed proactive alert line.\n".repeat(300);
        let chunks = format_notification_chunks("Timer Alert <prod>", &long_body);
        assert_eq!(
            chunks.len(),
            1,
            "proactive notifications must be capped to a single card rather than flooding multi-part messages"
        );
        assert!(chunks[0].contains("⏰ <b>【Timer Alert &lt;prod&gt;】</b>"));
        assert!(chunks[0].len() <= 4000);
        assert!(chunks[0].contains("<blockquote expandable>"));
        assert!(chunks[0].contains("</blockquote>"));
    }

    #[test]
    fn notification_body_with_quotes_and_wide_tables_never_nests_blockquotes() {
        let body = "> Quoted alert detail\n\n| Component | Status | Detailed Description |\n|---|---|---|\n| Worker | OK | Everything looks healthy across all nodes |";
        let chunks = format_notification_chunks("Subagent Report", body);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].matches("<blockquote").count(), 1);
        assert_eq!(chunks[0].matches("</blockquote>").count(), 1);
        assert!(chunks[0].contains("▎ Quoted alert detail"));
    }

    #[tokio::test]
    async fn notified_ledger_save_and_load_roundtrips_without_touching_agy_read_json() {
        let dir =
            std::env::temp_dir().join(format!("agygram_notifier_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let (empty, existed) = load_notified_ledger(&dir).await;
        assert!(empty.is_empty() && !existed);

        let mut map = HashMap::new();
        map.insert("msg-1".to_string(), true);
        map.insert("msg-2".to_string(), true);
        save_notified_ledger(&dir, &map).await.unwrap();

        let (loaded, existed_after) = load_notified_ledger(&dir).await;
        assert!(existed_after);
        assert_eq!(loaded, map);
        assert!(
            !dir.join(AGY_READ_LEDGER_FILE).exists(),
            "notifier must never create or overwrite agy's read.json"
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn internal_tool_plumbing_is_filtered_while_timers_pass() {
        let run_cmd_raw = r#"{
            "id": "t-713",
            "sender": "conv/task-713",
            "hideFromUser": false,
            "renderDetails": { "messageTitle": "Run cargo test finished" },
            "sourceMetadata": { "tool": { "toolCall": { "name": "run_command" } } },
            "content": "Task id \"conv/task-713\" finished with result:\nCompiling..."
        }"#;
        let run_cmd: BrainMessage = serde_json::from_str(run_cmd_raw).unwrap();
        assert!(is_internal_agent_plumbing(
            &run_cmd,
            "Run cargo test finished",
            run_cmd.content.as_deref().unwrap()
        ));

        let swarm_raw = r#"{
            "id": "sw-1",
            "hideFromUser": false,
            "renderDetails": { "messageTitle": "Message from @coordinator" },
            "sourceMetadata": { "tool": { "toolCall": { "name": "swarm" } } },
            "content": "brief"
        }"#;
        let sw: BrainMessage = serde_json::from_str(swarm_raw).unwrap();
        assert!(is_internal_agent_plumbing(
            &sw,
            "Message from @coordinator",
            "brief"
        ));

        let timer_raw = r#"{
            "id": "sched-1",
            "sender": "conv/task-96",
            "hideFromUser": false,
            "renderDetails": { "messageTitle": "4-min Water Reminder: Timer has expired" },
            "content": "Time to drink water!"
        }"#;
        let timer: BrainMessage = serde_json::from_str(timer_raw).unwrap();
        assert!(!is_internal_agent_plumbing(
            &timer,
            "4-min Water Reminder: Timer has expired",
            "Time to drink water!"
        ));
    }

    #[tokio::test]
    async fn reloads_agy_read_json_after_mid_turn_updates_and_skips_consumed_messages() {
        let dir =
            std::env::temp_dir().join(format!("agygram_notifier_cache_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        // 1. Initial tick: `read.json` has `m1`, notifier baselines `agygram_notified.json`.
        tokio::fs::write(dir.join("read.json"), r#"{"m1":true}"#)
            .await
            .unwrap();
        tokio::fs::write(
            dir.join("001.json"),
            r#"{"id":"m1","hideFromUser":true,"content":"internal"}"#,
        )
        .await
        .unwrap();

        // Invalid token guarantees any attempted network send would fail (`any_failed = true`).
        let bot = Bot::new("123456:TEST_TOKEN");
        let cancel = CancellationToken::new();
        let mut cache = ConversationCache::default();
        let mtime1 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();

        process_messages_in_dir(&bot, 42, "conv-1", &dir, mtime1, &mut cache, &cancel)
            .await
            .unwrap();
        assert!(cache.notified_ledger.contains_key("m1"));
        assert!(!cache.has_pending);

        // 2. Simulate an active `agy` turn writing `002.json` AND marking `m2: true` in `read.json`,
        // plus an unread `run_command` `003.json` (`m3`). Neither should trigger a Telegram send!
        tokio::fs::write(dir.join("read.json"), r#"{"m1":true,"m2":true}"#)
            .await
            .unwrap();
        tokio::fs::write(
            dir.join("002.json"),
            r#"{"id":"m2","hideFromUser":false,"renderDetails":{"messageTitle":"20s timer: Timer has expired"},"content":"wake"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.join("003.json"),
            r#"{"id":"m3","hideFromUser":false,"renderDetails":{"messageTitle":"Build finished"},"sourceMetadata":{"tool":{"toolCall":{"name":"run_command"}}},"content":"Task id \"x\" finished with result:\nok"}"#,
        )
        .await
        .unwrap();

        let mtime2 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();
        process_messages_in_dir(&bot, 42, "conv-1", &dir, mtime2, &mut cache, &cancel)
            .await
            .unwrap();

        assert!(
            !cache.has_pending,
            "consumed m2 and run_command m3 must be skipped without attempting Telegram HTTP send"
        );
        assert!(cache.notified_ledger.contains_key("m2"));
        assert!(cache.notified_ledger.contains_key("m3"));

        // Verify `agy`'s `read.json` was NEVER overwritten (m3 is NOT in read.json):
        let agy_read = load_agy_read_ledger(&dir).await;
        assert_eq!(agy_read.get("m2"), Some(&true));
        assert!(!agy_read.contains_key("m3"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn internal_plumbing_filters_send_message_and_post_message() {
        for tool in ["send_message", "post_message"] {
            let raw = format!(
                r#"{{"id":"p-{tool}","hideFromUser":false,"renderDetails":{{"messageTitle":"Peer Update"}},"sourceMetadata":{{"tool":{{"toolCall":{{"name":"{tool}"}}}}}},"content":"internal peer msg"}}"#
            );
            let msg: BrainMessage = serde_json::from_str(&raw).unwrap();
            assert!(
                is_internal_agent_plumbing(&msg, "Peer Update", "internal peer msg"),
                "{tool} must be filtered as internal agent plumbing"
            );
        }
    }

    #[test]
    fn permanent_telegram_errors_are_classified_accurately() {
        assert!(is_permanent_telegram_error(&teloxide::RequestError::Api(
            teloxide::ApiError::ChatNotFound
        )));
        assert!(is_permanent_telegram_error(&teloxide::RequestError::Api(
            teloxide::ApiError::BotBlocked
        )));
        assert!(is_permanent_telegram_error(&teloxide::RequestError::Api(
            teloxide::ApiError::UserDeactivated
        )));
        assert!(!is_permanent_telegram_error(&teloxide::RequestError::Api(
            teloxide::ApiError::MessageNotModified
        )));
    }

    #[tokio::test]
    async fn partial_fresh_json_is_retried_while_stale_corrupt_json_is_skipped() {
        let dir = std::env::temp_dir().join(format!(
            "agygram_notifier_partial_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let file_path = dir.join("001.json");
        tokio::fs::write(&file_path, r#"{"id":"m-partial","content":"half"#)
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let mut cache = ConversationCache::default();
        let mtime1 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();
        let mut delivered: Vec<String> = Vec::new();

        // Tick 1: freshly written truncated JSON -> deferred, NOT marked processed.
        process_messages_in_dir_with_delivery(
            42,
            "conv-partial",
            &dir,
            mtime1,
            &mut cache,
            &cancel,
            PARTIAL_JSON_GRACE,
            |id, _| {
                delivered.push(id);
                async { DeliveryOutcome::Delivered }
            },
        )
        .await
        .unwrap();
        assert!(cache.has_pending);
        assert!(!cache.processed_files.contains("001.json"));
        assert!(delivered.is_empty());

        // Tick 2: writer finishes flushing complete JSON -> delivered cleanly!
        tokio::fs::write(
            &file_path,
            r#"{"id":"m-partial","hideFromUser":false,"renderDetails":{"messageTitle":"Timer"},"content":"Done!"}"#,
        )
        .await
        .unwrap();
        let mtime2 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();
        process_messages_in_dir_with_delivery(
            42,
            "conv-partial",
            &dir,
            mtime2,
            &mut cache,
            &cancel,
            PARTIAL_JSON_GRACE,
            |id, _| {
                delivered.push(id);
                async { DeliveryOutcome::Delivered }
            },
        )
        .await
        .unwrap();
        assert!(!cache.has_pending);
        assert!(cache.processed_files.contains("001.json"));
        assert_eq!(delivered, vec!["m-partial"]);

        // Stale corrupt file (simulated via zero grace) is marked processed so it does not spin.
        tokio::fs::write(dir.join("002.json"), "{broken json")
            .await
            .unwrap();
        let mtime3 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();
        process_messages_in_dir_with_delivery(
            42,
            "conv-partial",
            &dir,
            mtime3,
            &mut cache,
            &cancel,
            Duration::ZERO,
            |_, _| async { DeliveryOutcome::Delivered },
        )
        .await
        .unwrap();
        assert!(!cache.has_pending);
        assert!(cache.processed_files.contains("002.json"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn permanent_delivery_error_marks_processed_without_infinite_retry_loop() {
        let dir = std::env::temp_dir().join(format!(
            "agygram_notifier_perm_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        tokio::fs::write(
            dir.join("001.json"),
            r#"{"id":"m-perm","hideFromUser":false,"renderDetails":{"messageTitle":"Reminder"},"content":"Ping"}"#,
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut cache = ConversationCache::default();
        let mtime = tokio::fs::metadata(&dir).await.unwrap().modified().ok();
        let mut attempts = 0usize;

        for _ in 0..3 {
            process_messages_in_dir_with_delivery(
                42,
                "conv-perm",
                &dir,
                mtime,
                &mut cache,
                &cancel,
                PARTIAL_JSON_GRACE,
                |_, _| {
                    attempts += 1;
                    async { DeliveryOutcome::PermanentError }
                },
            )
            .await
            .unwrap();
        }

        assert_eq!(attempts, 1, "permanent error must not spin on later ticks");
        assert!(!cache.has_pending);
        assert!(cache.processed_files.contains("001.json"));
        assert!(cache.notified_ledger.contains_key("m-perm"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
