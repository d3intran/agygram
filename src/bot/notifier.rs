//! Proactive notification daemon for agygram.
//!
//! Monitors background events and messages generated in Antigravity Brain workspaces
//! (~/.gemini/antigravity-cli/brain/<conversation_id>/.system_generated/messages/).
//! When a background task, timer/schedule, or subagent writes a message, this daemon
//! proactively detects unread notifications and pushes them directly to the associated
//! Telegram chat without requiring user re-prompting.

use crate::bot::formatter::TelegramFormatter;
use crate::session::SessionManager;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::ChatId;
use tracing::{debug, info, warn};

#[derive(Deserialize, Debug, Clone)]
struct BrainMessage {
    id: String,
    #[serde(default)]
    sender: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
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

/// Resolves the system messages directory for a conversation ID
fn get_brain_messages_dir(conv_id: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let path = home
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("messages");

    if path.exists() && path.is_dir() {
        Some(path)
    } else {
        None
    }
}

/// Reads the read.json acknowledged ledger from the messages directory
async fn load_read_ledger(dir: &Path) -> HashMap<String, bool> {
    let read_file = dir.join("read.json");
    if read_file.exists() {
        if let Ok(content) = tokio::fs::read_to_string(&read_file).await {
            if let Ok(map) = serde_json::from_str::<HashMap<String, bool>>(&content) {
                return map;
            }
        }
    }
    HashMap::new()
}

/// Atomically persists the updated read.json ledger to prevent race conditions
async fn save_read_ledger(dir: &Path, map: &HashMap<String, bool>) {
    let target = dir.join("read.json");
    let tmp = dir.join(format!("read.tmp-{}", std::process::id()));

    if let Ok(bytes) = serde_json::to_vec(map) {
        if let Ok(mut file) = tokio::fs::File::create(&tmp).await {
            use tokio::io::AsyncWriteExt;
            if file.write_all(&bytes).await.is_ok() && file.sync_all().await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &target).await;
                return;
            }
        }
        let _ = tokio::fs::remove_file(&tmp).await;
    }
}

/// Scans and pushes unread notifications for a single conversation
async fn process_conversation_messages(
    bot: &Bot,
    chat_id: i64,
    conv_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(messages_dir) = get_brain_messages_dir(conv_id) else {
        return Ok(());
    };

    let mut read_ledger = load_read_ledger(&messages_dir).await;
    let mut dir_entries = tokio::fs::read_dir(&messages_dir).await?;
    let mut candidate_files = Vec::new();

    while let Ok(Some(entry)) = dir_entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if file_name != "read.json" && !file_name.starts_with("read.tmp-") {
                candidate_files.push(path);
            }
        }
    }

    if candidate_files.is_empty() {
        return Ok(());
    }

    // Sort files by name / timestamp to preserve chronological delivery
    candidate_files.sort();

    let mut ledger_modified = false;

    for file_path in candidate_files {
        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let msg: BrainMessage = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if read_ledger.contains_key(&msg.id) {
            continue;
        }

        // Internal control / system messages hidden from the user
        if msg.hide_from_user {
            read_ledger.insert(msg.id.clone(), true);
            ledger_modified = true;
            continue;
        }

        let raw_title = msg
            .render_details
            .as_ref()
            .and_then(|r| r.message_title.as_deref())
            .unwrap_or("后台通知");

        let raw_body = msg.content.as_deref().unwrap_or("").trim();

        // Skip completely empty noise
        if raw_title.is_empty() && raw_body.is_empty() {
            read_ledger.insert(msg.id.clone(), true);
            ledger_modified = true;
            continue;
        }

        let title_lower = raw_title.to_lowercase();
        let icon = if title_lower.contains("timer") || title_lower.contains("schedule") {
            "⏰"
        } else if title_lower.contains("task") || title_lower.contains("finished") || title_lower.contains("result") {
            "📋"
        } else if title_lower.contains("subagent") || title_lower.contains("agent") {
            "🤖"
        } else {
            "🔔"
        };

        let formatted_html = if raw_body.is_empty() {
            format!("{} <b>【{}】</b>", icon, TelegramFormatter::escape_html(raw_title))
        } else {
            format!(
                "{} <b>【{}】</b>\n\n<blockquote expandable>{}</blockquote>",
                icon,
                TelegramFormatter::escape_html(raw_title),
                TelegramFormatter::to_telegram_html(raw_body)
            )
        };

        // Deliver notification directly to Telegram
        let send_res = bot
            .send_message(ChatId(chat_id), formatted_html)
            .parse_mode(teloxide::types::ParseMode::Html)
            .await;

        match send_res {
            Ok(_) => {
                info!(
                    "Successfully delivered proactive notification (msg_id: {}) to chat_id: {}",
                    msg.id, chat_id
                );
                read_ledger.insert(msg.id.clone(), true);
                ledger_modified = true;
            }
            Err(err) => {
                warn!(
                    "Failed to deliver proactive notification {} to chat {}: {:?}",
                    msg.id, chat_id, err
                );
            }
        }
    }

    if ledger_modified {
        save_read_ledger(&messages_dir, &read_ledger).await;
    }

    Ok(())
}

/// Background loop running proactive notification polling
pub async fn run_proactive_notifier(bot: Bot, session_mgr: Arc<SessionManager>) {
    info!("Starting proactive notification daemon for agygram...");
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Retrieve conversations that are currently idle (no turn in flight)
        let idle_conversations = session_mgr.get_idle_conversations().await;
        for (chat_id, conv_id) in idle_conversations {
            if let Err(err) = process_conversation_messages(&bot, chat_id, &conv_id).await {
                debug!("Error processing proactive messages for chat {}: {:?}", chat_id, err);
            }
        }
    }
}
