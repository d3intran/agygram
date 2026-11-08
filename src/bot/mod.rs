pub mod callbacks;
pub mod commands;
pub mod formatter;
pub mod media;
pub mod notifier;
pub mod pipeline;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::Result;
use callbacks::callback_handler;
use commands::{command_handler, Command};
use pipeline::message_handler;
use std::net::SocketAddr;
use std::sync::Arc;
use teloxide::error_handlers::LoggingErrorHandler;
use teloxide::prelude::*;
use teloxide::update_listeners::webhooks;
use teloxide::utils::command::BotCommands;
use tracing::{info, warn};
use url::Url;

/// teloxide serialises every update of a chat behind the previous one by default. A prompt turn
/// awaits the whole `agy` run, so `/usage`, `/cancel` or a button press used to sit in that queue
/// until the answer finished. Commands and callbacks therefore get no key (handled concurrently,
/// immediately), while plain prompts keep the per-chat ordering they always had.
fn distribution_key(update: &Update) -> Option<ChatId> {
    use teloxide::types::UpdateKind;
    match &update.kind {
        UpdateKind::Message(m) => {
            let is_command = m.text().map(|t| t.starts_with('/')).unwrap_or(false);
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

pub async fn run_bot(config: Config) -> Result<()> {
    let session_mgr = Arc::new(SessionManager::new(
        config.get_valid_workspace(),
        config.default_model.clone(),
        config.default_mode.clone(),
        config.default_effort.clone(),
    ));
    run_bot_with_session_mgr(Arc::new(config), session_mgr).await
}

pub async fn run_bot_with_session_mgr(
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> Result<()> {
    info!("Starting agygram Telegram bot daemon...");
    info!("Allowed Telegram user IDs: {:?}", config.allowed_user_ids);
    if config.denies_everyone() {
        warn!("allowed_user_ids 为空且未开启 allow_all_users：所有 Telegram 请求都会被拒绝。请在 config.toml 写入授权 User ID，或显式设置 allow_all_users = true。");
    }
    info!("Default workspace: {}", config.default_workspace);
    if let Some(ref proxy) = config.proxy_url {
        info!("Using Proxy: {}", proxy);
    }

    let bot = if let Some(ref proxy_str) = config.proxy_url {
        let proxy = reqwest::Proxy::all(proxy_str)?;
        let client = reqwest::Client::builder()
            .proxy(proxy)
            .build()?;
        Bot::with_client(&config.bot_token, client)
    } else {
        Bot::new(&config.bot_token)
    };

    if let Err(e) = bot.set_my_commands(Command::bot_commands()).await {
        warn!("Failed to set Telegram commands: {:?}", e);
    } else {
        info!("Successfully registered clean bot commands with Telegram!");
    }

    // Spawn proactive background notification daemon for async timers/tasks
    let notifier_bot = bot.clone();
    let notifier_session_mgr = session_mgr.clone();
    tokio::spawn(async move {
        notifier::run_proactive_notifier(notifier_bot, notifier_session_mgr).await;
    });

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(command_handler),
        )
        .branch(Update::filter_callback_query().endpoint(callback_handler))
        .branch(Update::filter_message().endpoint(message_handler));

    let builder = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![config.clone(), session_mgr])
        .distribution_function(distribution_key)
        .enable_ctrlc_handler();

    if let Some(ref webhook_url_str) = config.webhook_url {
        info!("Starting agygram bot in Webhook mode on port {} (URL: {})...", config.webhook_port, webhook_url_str);
        let url: Url = webhook_url_str.parse()?;
        let addr: SocketAddr = ([0, 0, 0, 0], config.webhook_port).into();
        let mut options = webhooks::Options::new(addr, url);
        if let Some(ref secret) = config.webhook_secret {
            options.secret_token = Some(secret.clone());
        }
        let listener = webhooks::axum(bot, options).await?;
        builder
            .build()
            .dispatch_with_listener(
                listener,
                LoggingErrorHandler::with_custom_text("An error from the update listener"),
            )
            .await;
    } else {
        info!("Starting agygram bot in Long-Polling mode...");
        builder.build().dispatch().await;
    }

    Ok(())
}
