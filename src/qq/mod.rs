pub mod client;
pub mod gateway;
pub mod handler;
pub mod types;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

/// Runs the QQ Bot service (WebSocket Gateway listener & event dispatcher)
pub async fn run_qq_bot(config: Arc<Config>, session_mgr: Arc<SessionManager>) -> Result<()> {
    let app_id = config
        .qq_app_id
        .clone()
        .context("Missing qq_app_id for QQ Bot service")?;
    let app_secret = config
        .qq_app_secret
        .clone()
        .context("Missing qq_app_secret for QQ Bot service")?;

    info!("Initializing QQ Bot Client (AppID: {})...", app_id);
    let client = Arc::new(client::QqClient::new(
        app_id,
        app_secret,
        config.proxy_url.as_deref(),
    )?);

    gateway::run_gateway(client, config, session_mgr).await
}
