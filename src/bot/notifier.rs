//! Proactive notification daemon for agygram.
//!
//! Monitors background events and messages generated in Antigravity Brain workspaces
//! (`~/.gemini/antigravity-cli/brain/<conversation_id>/.system_generated/messages/`).
//! When a background task, timer/schedule, or subagent writes a message, this daemon
//! proactively detects unread notifications and pushes them directly to the associated
//! Telegram chat without requiring user re-prompting.

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
const MAX_CACHED_CONVERSATIONS: usize = 256;
static LEDGER_SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Debug, Clone)]
struct BrainMessage {
    id: String,
    #[serde(default, rename = "hideFromUser")]
    hide_from_user: bool,
    #[serde(default, rename = "renderDetails")]
    render_details: Option<RenderDetails>,
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct RenderDetails {
    #[serde(rename = "messageTitle")]
    message_title: Option<String>,
}

#[derive(Default)]
struct ConversationCache {
    messages_dir: Option<PathBuf>,
    last_dir_mtime: Option<SystemTime>,
    read_ledger: HashMap<String, bool>,
    ledger_loaded: bool,
    processed_files: FxHashSet<String>,
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

/// Reads the `read.json` acknowledged ledger from the messages directory without blocking `.exists()`.
async fn load_read_ledger(dir: &Path) -> HashMap<String, bool> {
    let read_file = dir.join("read.json");
    match tokio::fs::read_to_string(&read_file).await {
        Ok(content) => match serde_json::from_str::<HashMap<String, bool>>(&content) {
            Ok(map) => map,
            Err(err) => {
                warn!(
                    path = %read_file.display(),
                    error = %err,
                    "failed to parse notification read ledger"
                );
                HashMap::new()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(err) => {
            warn!(
                path = %read_file.display(),
                error = %err,
                "failed to read notification ledger"
            );
            HashMap::new()
        }
    }
}

/// Atomically persists the updated `read.json` ledger using a unique temp file.
async fn save_read_ledger(dir: &Path, map: &HashMap<String, bool>) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let target = dir.join("read.json");
    let seq = LEDGER_SAVE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("read.tmp-{}-{seq}", std::process::id()));

    let bytes = serde_json::to_vec(map).context("serialize notification read ledger")?;

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
            "failed to persist notification read ledger"
        );
        if let Err(rm_err) = tokio::fs::remove_file(&tmp).await
            && rm_err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                tmp = %tmp.display(),
                error = %rm_err,
                "failed to clean up temporary read ledger"
            );
        }
    }

    write_res
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
    let formatted_html = if raw_body.is_empty() {
        format!("{icon} <b>【{escaped_title}】</b>")
    } else {
        format!(
            "{icon} <b>【{escaped_title}】</b>\n\n<blockquote expandable>{}</blockquote>",
            TelegramFormatter::to_telegram_html(raw_body)
        )
    };
    TelegramFormatter::split_html_chunks(&formatted_html, 4000)
}

async fn send_chunk_with_retry(
    bot: &Bot,
    chat_id: i64,
    msg_id: &str,
    chunk: &str,
    cancel: &CancellationToken,
) -> bool {
    let target = ChatId(chat_id);
    let mut attempt = 0u8;
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        match bot
            .send_message(target, chunk)
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(_) => return true,
            Err(teloxide::RequestError::RetryAfter(retry_secs)) if attempt == 0 => {
                attempt += 1;
                let wait = Duration::from_secs(u64::from(retry_secs.seconds()) + 1);
                warn!(
                    chat_id,
                    msg_id,
                    wait_secs = wait.as_secs(),
                    "proactive notification hit RetryAfter; backing off"
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return false,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(teloxide::RequestError::Api(ref api_err))
                if !matches!(
                    api_err,
                    teloxide::ApiError::ChatNotFound
                        | teloxide::ApiError::BotBlocked
                        | teloxide::ApiError::UserDeactivated
                ) =>
            {
                // If Telegram rejected the HTML markup, fall back to HTML-stripped plain text
                // rather than retrying the unparseable payload every 2 seconds forever.
                warn!(
                    chat_id,
                    msg_id,
                    error = ?api_err,
                    "HTML notification rejected; falling back to plain text"
                );
                let plain = text::strip_html_tags(chunk);
                let plain_clipped = text::take_chars(&plain, 4000);
                if plain_clipped.trim().is_empty() {
                    return true;
                }
                match bot.send_message(target, plain_clipped).await {
                    Ok(_) => return true,
                    Err(plain_err) => {
                        warn!(
                            chat_id,
                            msg_id,
                            error = ?plain_err,
                            "plain-text notification fallback failed"
                        );
                        return false;
                    }
                }
            }
            Err(err) => {
                warn!(
                    chat_id,
                    msg_id,
                    error = ?err,
                    "failed to deliver proactive notification chunk"
                );
                return false;
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
) -> bool {
    for chunk in chunks {
        if !send_chunk_with_retry(bot, chat_id, msg_id, chunk, cancel).await {
            return false;
        }
    }
    true
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
                cache.ledger_loaded = false;
                cache.processed_files.clear();
            }
        }
    }

    let dir = get_brain_messages_dir_async(conv_id).await?;
    let mtime = tokio::fs::metadata(&dir)
        .await
        .ok()
        .and_then(|meta| meta.modified().ok());
    cache.messages_dir = Some(dir.clone());
    Some((dir, mtime))
}

/// Scans and pushes unread notifications for a single conversation using directory mtime and
/// processed-filename caching to skip historical notification files between ticks.
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
    let mtime_settled = dir_mtime
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age >= Duration::from_secs(1));

    if mtime_settled
        && cache.ledger_loaded
        && !cache.has_pending
        && cache.last_dir_mtime.is_some()
        && cache.last_dir_mtime == dir_mtime
    {
        return Ok(());
    }

    if !cache.ledger_loaded {
        cache.read_ledger = load_read_ledger(messages_dir).await;
        cache.ledger_loaded = true;
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
                if !file_name.ends_with(".json")
                    || file_name == "read.json"
                    || file_name.starts_with("read.tmp-")
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
        return Ok(());
    }

    // Sort files by name / timestamp to preserve chronological delivery.
    candidate_files.sort_unstable_by(|a, b| a.0.cmp(&b.0));

    let mut ledger_modified = false;
    let mut any_failed = false;

    for (file_name, file_path) in candidate_files {
        if cancel.is_cancelled() {
            any_failed = true;
            break;
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
                warn!(
                    chat_id,
                    conv_id,
                    path = %file_path.display(),
                    error = %err,
                    "failed to parse notification JSON"
                );
                // Avoid looping on permanently malformed static files; directory mtime changes
                // if the file is rewritten/replaced.
                cache.processed_files.insert(file_name);
                continue;
            }
        };

        if cache.read_ledger.contains_key(&msg.id) {
            cache.processed_files.insert(file_name);
            continue;
        }

        // Internal control / system messages hidden from the user.
        if msg.hide_from_user {
            cache.read_ledger.insert(msg.id, true);
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

        // Skip completely empty noise.
        if raw_title.is_empty() && raw_body.is_empty() {
            cache.read_ledger.insert(msg.id, true);
            cache.processed_files.insert(file_name);
            ledger_modified = true;
            continue;
        }

        let chunks = format_notification_chunks(raw_title, raw_body);
        if deliver_notification_chunks(bot, chat_id, &msg.id, &chunks, cancel).await {
            info!(
                chat_id,
                conv_id,
                msg_id = %msg.id,
                chunks = chunks.len(),
                "delivered proactive notification"
            );
            cache.read_ledger.insert(msg.id, true);
            cache.processed_files.insert(file_name);
            ledger_modified = true;
        } else {
            any_failed = true;
        }
    }

    if ledger_modified {
        if save_read_ledger(messages_dir, &cache.read_ledger)
            .await
            .is_ok()
        {
            // Refresh mtime so our own `read.json` update does not trigger a re-scan next tick.
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
            // QQ sessions share this map but their chat ids are not Telegram addresses; pushing
            // to one fails with "chat not found" every tick and never clears the read ledger.
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
    fn oversized_notifications_are_split_under_4000_chars() {
        let long_body = "This is a detailed proactive alert line.\n".repeat(300);
        let chunks = format_notification_chunks("Timer Alert <prod>", &long_body);
        assert!(
            chunks.len() > 1,
            "oversized notification must split into multiple chunks"
        );
        assert!(chunks[0].contains("⏰ <b>【Timer Alert &lt;prod&gt;】</b>"));
        for (idx, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.len() <= 4000,
                "notification chunk {idx} exceeded 4000 bytes: {}",
                chunk.len()
            );
            assert!(
                chunk.contains("<blockquote expandable>"),
                "chunk {idx} lost expandable blockquote wrapper"
            );
            assert!(
                chunk.contains("</blockquote>"),
                "chunk {idx} missed closing blockquote"
            );
        }
    }

    #[tokio::test]
    async fn read_ledger_save_and_load_roundtrips_and_ignores_missing() {
        let dir =
            std::env::temp_dir().join(format!("agygram_notifier_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let empty = load_read_ledger(&dir).await;
        assert!(empty.is_empty());

        let mut map = HashMap::new();
        map.insert("msg-1".to_string(), true);
        map.insert("msg-2".to_string(), true);
        save_read_ledger(&dir, &map).await.unwrap();

        let loaded = load_read_ledger(&dir).await;
        assert_eq!(loaded, map);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn brain_message_ignores_unknown_wire_fields() {
        let raw = r#"{
            "id": "n-1",
            "sender": "subagent",
            "priority": "HIGH",
            "timestamp": "2026-11-11T00:00:00Z",
            "hideFromUser": false,
            "renderDetails": { "messageTitle": "Task Finished" },
            "content": "All tests pass."
        }"#;
        let msg: BrainMessage = serde_json::from_str(raw).unwrap();
        assert_eq!(msg.id, "n-1");
        assert!(!msg.hide_from_user);
        assert_eq!(
            msg.render_details
                .as_ref()
                .and_then(|r| r.message_title.as_deref()),
            Some("Task Finished")
        );
        assert_eq!(msg.content.as_deref(), Some("All tests pass."));
    }

    #[tokio::test]
    async fn dir_cache_skips_previously_processed_notification_files() {
        let dir =
            std::env::temp_dir().join(format!("agygram_notifier_cache_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        tokio::fs::write(
            dir.join("001.json"),
            r#"{"id":"m1","hideFromUser":true,"content":"internal"}"#,
        )
        .await
        .unwrap();
        tokio::fs::write(dir.join("read.tmp-999"), "ignore")
            .await
            .unwrap();

        let bot = Bot::new("123456:TEST_TOKEN");
        let cancel = CancellationToken::new();
        let mut cache = ConversationCache::default();
        let mtime1 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();

        process_messages_in_dir(&bot, 42, "conv-1", &dir, mtime1, &mut cache, &cancel)
            .await
            .unwrap();

        assert!(cache.read_ledger.get("m1").copied().unwrap_or(false));
        assert!(cache.processed_files.contains("001.json"));

        // Corrupt 001.json on disk and add a hidden 002.json: 001.json must not be re-read/parsed.
        tokio::fs::write(dir.join("001.json"), "{invalid-json")
            .await
            .unwrap();
        tokio::fs::write(
            dir.join("002.json"),
            r#"{"id":"m2","hideFromUser":true,"content":""}"#,
        )
        .await
        .unwrap();
        let mtime2 = tokio::fs::metadata(&dir).await.unwrap().modified().ok();

        process_messages_in_dir(&bot, 42, "conv-1", &dir, mtime2, &mut cache, &cancel)
            .await
            .unwrap();

        assert!(cache.read_ledger.get("m2").copied().unwrap_or(false));
        assert!(cache.processed_files.contains("002.json"));
        let persisted = load_read_ledger(&dir).await;
        assert!(persisted.contains_key("m1"));
        assert!(persisted.contains_key("m2"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
