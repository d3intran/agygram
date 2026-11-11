pub mod callbacks;
pub mod commands;
pub mod formatter;
pub mod media;
pub mod notifier;
pub mod pipeline;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use callbacks::callback_handler;
use commands::{command_handler, Command};
use pipeline::{message_handler, BotRuntime};
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
const TELEGRAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `teloxide` serializes updates sharing a distribution key. Commands and callbacks run
/// concurrently (`None`) so `/cancel`, `/usage`, and button presses never stall behind turns;
/// non-command messages keep `Some(m.chat.id)` for deterministic entry ordering while
/// [`message_handler`] immediately spawns the per-chat execution task, letting subsequent
/// messages trigger `interrupt_on_new_message` without waiting for `agy` to exit.
fn distribution_key(update: &Update) -> Option<ChatId> {
    use teloxide::types::UpdateKind;
    match &update.kind {
        UpdateKind::Message(m) => {
            let is_command = m.text().is_some_and(|t| t.starts_with('/'));
            if is_command {
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
    if let Some(ref proxy_str) = config.proxy_url {
        let proxy = reqwest::Proxy::all(proxy_str)
            .with_context(|| format!("invalid Telegram proxy URL: {proxy_str}"))?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .connect_timeout(TELEGRAM_CONNECT_TIMEOUT)
            .timeout(TELEGRAM_REQUEST_TIMEOUT)
            .tcp_nodelay(true)
            .build()
            .context("failed to build proxied Telegram HTTP client")?;
        Ok(Bot::with_client(&config.bot_token, client))
    } else {
        Ok(Bot::new(&config.bot_token))
    }
}

pub async fn run_bot(config: Config) -> Result<()> {
    let session_mgr = Arc::new(SessionManager::from_config(&config));
    Box::pin(run_bot_with_session_mgr(Arc::new(config), session_mgr)).await
}

pub async fn run_bot_with_session_mgr(
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> Result<()> {
    Box::pin(run_bot_with_cancel(
        config,
        session_mgr,
        CancellationToken::new(),
    ))
    .await
}

#[tracing::instrument(skip_all)]
pub async fn run_bot_with_cancel(
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    cancel: CancellationToken,
) -> Result<()> {
    info!("Starting agygram Telegram bot daemon...");
    info!(allowed_user_ids = ?config.allowed_user_ids, "loaded Telegram user whitelist");
    if config.denies_everyone() {
        warn!("allowed_user_ids 为空且未开启 allow_all_users：所有 Telegram 请求都会被拒绝。请在 config.toml 写入授权 User ID，或显式设置 allow_all_users = true。");
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
        .enable_ctrlc_handler()
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
