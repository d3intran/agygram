pub(crate) mod callbacks;
pub(crate) mod commands;
pub(crate) mod formatter;
pub(crate) mod media;
pub(crate) mod notifier;
pub(crate) mod pipeline;
pub mod widget;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use callbacks::callback_handler;
use commands::{Command, command_handler};
use pipeline::{BotRuntime, message_handler};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::prelude::*;
use teloxide::update_listeners::webhooks;
use teloxide::utils::command::BotCommands;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

const TELEGRAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TELEGRAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const TELEGRAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const TELEGRAM_TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Returns `true` when a leading `/` command is a known non-AI control command that may safely
/// bypass per-chat serialization (`None` distribution key) so `/cancel`, `/status`, `/panel`,
/// `/new`, and toggles never stall behind an active turn.
///
/// Unknown `/skill` commands and native AI-executing commands (`/compact`, `/swarm`, `/goal`,
/// `/voice <prompt>`, `/card <custom>`) return `false` and serialize per chat (`Some(chat_id)`).
fn is_concurrent_control_command(text: &str) -> bool {
    let trimmed = text.trim_start();
    let Some(after_slash) = trimmed.strip_prefix('/') else {
        return false;
    };
    let (raw_cmd, rest) = match after_slash.find(char::is_whitespace) {
        Some(idx) => (&after_slash[..idx], after_slash[idx..].trim()),
        None => (after_slash, ""),
    };
    let cmd = raw_cmd
        .split('@')
        .next()
        .unwrap_or(raw_cmd)
        .to_ascii_lowercase();

    match cmd.as_str() {
        "cancel" | "stop" | "status" | "panel" | "settings" | "help" | "quota" | "usage"
        | "models" | "model" | "effort" | "mode" | "new" | "reset" | "clear" | "resume"
        | "history" | "session" | "sessions" | "plan" | "walkthrough" | "skills" | "rules"
        | "cd" | "ws" | "workspace" | "adddir" | "cleardirs" | "dirs" | "pwd" | "agent"
        | "agents" | "sandbox" | "project" | "system" | "file" | "mcp" | "plugin" | "plugins"
        | "changelog" | "update" | "thinking" | "tts" | "version" | "start" | "whoami" | "id"
        | "remote" => true,
        "voice" => {
            rest.is_empty()
                || rest.eq_ignore_ascii_case("on")
                || rest == "开启"
                || rest.eq_ignore_ascii_case("off")
                || rest == "关闭"
                || rest.eq_ignore_ascii_case("toggle")
                || rest == "切换"
                || rest
                    .split_whitespace()
                    .next()
                    .is_some_and(|w| w.eq_ignore_ascii_case("id"))
        }
        "card" | "widget" => {
            if rest.is_empty()
                || rest.eq_ignore_ascii_case("hub")
                || rest.eq_ignore_ascii_case("menu")
                || rest.eq_ignore_ascii_case("list")
                || rest
                    .split_whitespace()
                    .next()
                    .is_some_and(|w| w.eq_ignore_ascii_case("html"))
                || widget::BuiltinGame::parse(rest).is_some()
            {
                return true;
            }
            let lower = rest.to_ascii_lowercase();
            matches!(
                lower.as_str(),
                "agent"
                    | "demo"
                    | "card"
                    | "dash"
                    | "dashboard"
                    | "metric"
                    | "metrics"
                    | "看板"
                    | "quiz"
                    | "test"
                    | "速测"
                    | "测验"
                    | "table"
                    | "cmp"
                    | "compare"
                    | "comparison"
                    | "matrix"
                    | "对比"
                    | "status"
                    | "状态"
            )
        }
        _ => false,
    }
}

/// `teloxide` serializes updates sharing a distribution key. Explicitly whitelisted control commands
/// and callbacks run concurrently (`None`) so `/cancel`, `/status`, `/panel`, `/new`, and button
/// presses never stall behind turns; plain messages, unknown `/skill` invocations, native
/// `/swarm`/`/goal` commands, and AI-prompting commands (`/voice <prompt>`, `/card <custom>`,
/// `/compact`) keep `Some(m.chat.id)` for deterministic per-chat entry ordering.
fn distribution_key(update: &Update) -> Option<ChatId> {
    use teloxide::types::UpdateKind;
    match &update.kind {
        UpdateKind::Message(m) => {
            if m.text().is_some_and(is_concurrent_control_command) {
                None
            } else {
                Some(m.chat.id)
            }
        }
        UpdateKind::CallbackQuery(_) => None,
        _ => update.chat().map(|c| c.id),
    }
}

fn build_telegram_bot(config: &Config) -> Result<Bot> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(TELEGRAM_CONNECT_TIMEOUT)
        .timeout(TELEGRAM_REQUEST_TIMEOUT)
        .pool_idle_timeout(TELEGRAM_POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(8)
        .tcp_keepalive(TELEGRAM_TCP_KEEPALIVE)
        .tcp_nodelay(true);

    if let Some(ref proxy_str) = config.proxy_url {
        let proxy = reqwest::Proxy::all(proxy_str)
            .with_context(|| format!("invalid Telegram proxy URL: {proxy_str}"))?;
        builder = builder.proxy(proxy);
    }

    let client = builder
        .build()
        .context("failed to build Telegram HTTP client")?;
    Ok(Bot::with_client(&config.bot_token, client))
}

#[tracing::instrument(skip_all)]
pub(crate) async fn run_bot_with_cancel(
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    cancel: CancellationToken,
) -> Result<()> {
    info!("Starting agygram Telegram bot daemon...");
    info!(allowed_user_ids = ?config.allowed_user_ids, "loaded Telegram user whitelist");
    if config.denies_everyone() {
        warn!(
            "allowed_user_ids 为空且未开启 allow_all_users：所有 Telegram 请求都会被拒绝。请在 config.toml 写入授权 User ID，或显式设置 allow_all_users = true。"
        );
    }
    info!(default_workspace = %config.default_workspace, "configured default workspace");
    if let Some(ref proxy) = config.proxy_url {
        info!(proxy_url = %proxy, "using Telegram HTTP proxy");
    }

    let bot = build_telegram_bot(&config)?;

    if let Err(err) = bot.set_my_commands(Command::bot_commands()).await {
        warn!(error = ?err, "failed to set Telegram bot commands");
    } else {
        info!("Successfully registered clean bot commands with Telegram!");
    }

    let runtime = BotRuntime::new(cancel.clone());

    let notifier_bot = bot.clone();
    let notifier_session_mgr = session_mgr.clone();
    let notifier_cancel = cancel.child_token();
    let notifier_handle = tokio::spawn(async move {
        notifier::run_proactive_notifier_with_cancel(
            notifier_bot,
            notifier_session_mgr,
            notifier_cancel,
        )
        .await;
    });

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(command_handler),
        )
        .branch(Update::filter_callback_query().endpoint(callback_handler))
        .branch(Update::filter_message().endpoint(message_handler));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![config.clone(), session_mgr, runtime.clone()])
        .distribution_function(distribution_key)
        .build();

    let shutdown_token = dispatcher.shutdown_token();
    let dispatch_cancel = cancel.clone();
    let shutdown_watcher = tokio::spawn(async move {
        dispatch_cancel.cancelled().await;
        if let Ok(fut) = shutdown_token.shutdown() {
            fut.await;
        }
    });

    if let Some(ref webhook_url_str) = config.webhook_url {
        info!(
            port = config.webhook_port,
            url = %webhook_url_str,
            "starting agygram bot in Webhook mode"
        );
        let url: Url = webhook_url_str.parse()?;
        let addr: SocketAddr = ([0, 0, 0, 0], config.webhook_port).into();
        let mut options = webhooks::Options::new(addr, url);
        if let Some(ref secret) = config.webhook_secret {
            options.secret_token = Some(secret.clone());
        }
        let listener = webhooks::axum(bot, options).await?;
        Box::pin(dispatcher.dispatch_with_listener(
            listener,
            LoggingErrorHandler::with_custom_text("An error from the update listener"),
        ))
        .await;
    } else {
        info!("Starting agygram bot in Long-Polling mode...");
        Box::pin(dispatcher.dispatch()).await;
    }

    // Ensure Ctrl+C or dispatcher exit cleanly stops notifier and active per-chat turns.
    cancel.cancel();
    shutdown_watcher.abort();
    let _ = tokio::time::timeout(Duration::from_secs(3), notifier_handle).await;
    runtime.shutdown_all().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distribution_key_separates_commands_callbacks_and_messages() {
        let plain_msg: Update = serde_json::from_str(
            r#"{
                "update_id": 1,
                "message": {
                    "message_id": 10,
                    "date": 1700000000,
                    "chat": { "id": 12345, "type": "private" },
                    "text": "hello world"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(distribution_key(&plain_msg), Some(ChatId(12345)));

        let cmd_msg: Update = serde_json::from_str(
            r#"{
                "update_id": 2,
                "message": {
                    "message_id": 11,
                    "date": 1700000000,
                    "chat": { "id": 12345, "type": "private" },
                    "text": "/cancel"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(distribution_key(&cmd_msg), None);

        let cb_update: Update = serde_json::from_str(
            r#"{
                "update_id": 3,
                "callback_query": {
                    "id": "cb-1",
                    "from": { "id": 42, "is_bot": false, "first_name": "Test" },
                    "chat_instance": "ci-1",
                    "data": "menu:status"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(distribution_key(&cb_update), None);

        let make_msg = |update_id: u32, text: &str| -> Update {
            let json = serde_json::json!({
                "update_id": update_id,
                "message": {
                    "message_id": update_id,
                    "date": 1700000000,
                    "chat": { "id": 12345, "type": "private" },
                    "text": text
                }
            })
            .to_string();
            serde_json::from_str(&json).unwrap()
        };

        // AI-executing commands, native `/swarm`/`/goal`, and unknown `/skill` commands serialize per chat.
        for (i, text) in [
            "/compact",
            "/compact 保留架构决策",
            "/compact@my_bot focus",
            "/voice 介绍一下你自己",
            "/voice@my_bot tell me a joke",
            "/card 做一个复利计算器",
            "/widget@my_bot 做一个五子棋",
            "/swarm build feature",
            "/goal run overnight",
            "/my-custom-skill arg",
            "  /unknown_skill",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                distribution_key(&make_msg(100 + i as u32, text)),
                Some(ChatId(12345)),
                "AI-triggering command should serialize per chat: {text}"
            );
        }

        // Whitelisted control commands (including leading whitespace) and built-in card/voice toggles remain concurrent (`None`).
        for (i, text) in [
            "/cancel",
            "  /cancel",
            " /stop@my_bot",
            "/status",
            "/panel",
            "/new",
            "/sessions",
            "/ws",
            "/workspace",
            "/tts",
            "/version",
            "/whoami",
            "/id",
            "/thinking on",
            "/remote off",
            "/voice",
            "/voice on",
            "/voice off",
            "/voice toggle",
            "/voice 开启",
            "/voice id abcdef1234",
            "/card",
            "/card hub",
            "/card 2048",
            "/card 扫雷",
            "/card dash",
            "/card status",
            "/card html 2048",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                distribution_key(&make_msg(200 + i as u32, text)),
                None,
                "control command should stay concurrent: {text}"
            );
        }
    }

    #[test]
    fn build_telegram_bot_accepts_valid_proxy_and_rejects_invalid_proxy() {
        let mut cfg: Config = toml::from_str(
            r#"
bot_token = "123456:TEST_TOKEN"
allowed_user_ids = [1]
proxy_url = "http://127.0.0.1:7890"
"#,
        )
        .unwrap();
        assert!(build_telegram_bot(&cfg).is_ok());

        cfg.proxy_url = Some("not a valid url ://".to_string());
        assert!(build_telegram_bot(&cfg).is_err());
    }
}
