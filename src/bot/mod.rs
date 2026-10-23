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
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::utils::command::BotCommands;
use tracing::{info, warn};

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
    info!("Starting agygram bot daemon...");
    info!("Allowed user IDs: {:?}", config.allowed_user_ids);
    if config.denies_everyone() {
        // Deliberately not a startup abort: the unit is Restart=always with a 5-per-10s burst,
        // so exiting here would leave the service permanently failed and need manual reset.
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

    let session_mgr = Arc::new(SessionManager::new(
        config.get_valid_workspace(),
        config.default_model.clone(),
        config.default_mode.clone(),
        config.default_effort.clone(),
    ));

    // Spawn proactive background notification daemon for async timers/tasks
    let notifier_bot = bot.clone();
    let notifier_session_mgr = session_mgr.clone();
    tokio::spawn(async move {
        notifier::run_proactive_notifier(notifier_bot, notifier_session_mgr).await;
    });

    let config_arc = Arc::new(config);

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(command_handler),
        )
        .branch(Update::filter_callback_query().endpoint(callback_handler))
        .branch(Update::filter_message().endpoint(message_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![config_arc, session_mgr])
        .distribution_function(distribution_key)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}
