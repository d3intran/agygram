pub(crate) mod client;
pub(crate) mod formatter;
pub(crate) mod gateway;
pub(crate) mod handler;
pub(crate) mod types;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;

/// Runs the QQ Bot service (WebSocket Gateway listener & event dispatcher).
///
/// # Errors
/// Returns an error if QQ credentials are missing, HTTP client initialization fails,
/// or the gateway supervisor terminates unexpectedly.
pub async fn run_qq_bot(config: Arc<Config>, session_mgr: Arc<SessionManager>) -> Result<()> {
    run_qq_bot_with_cancel(config, session_mgr, CancellationToken::new()).await
}

/// Runs the QQ Bot service with a cooperative [`CancellationToken`] for graceful shutdown.
///
/// # Errors
/// Returns an error if QQ credentials are missing, HTTP client initialization fails,
/// or the gateway supervisor fails unrecoverably.
pub async fn run_qq_bot_with_cancel(
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    cancel: CancellationToken,
) -> Result<()> {
    let app_id = config
        .qq_app_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Missing qq_app_id for QQ Bot service")?
        .to_string();
    let app_secret = config
        .qq_app_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("Missing qq_app_secret for QQ Bot service")?
        .to_string();

    info!(app_id = %app_id, "Initializing QQ Bot Client...");
    let client = Arc::new(client::QqClient::new(
        app_id,
        app_secret,
        config.proxy_url.as_deref(),
    )?);

    gateway::run_gateway(client, config, session_mgr, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_qq_bot_with_cancel_rejects_missing_credentials() {
        let dir = std::env::temp_dir().join(format!("agygram_qq_mod_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage_path = dir.join("sessions.json");

        let config = Arc::new(Config::default());
        let mgr = Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            storage_path,
        ));
        let cancel = CancellationToken::new();

        let err = run_qq_bot_with_cancel(config, mgr, cancel)
            .await
            .expect_err("missing qq_app_id must fail");
        assert!(err.to_string().contains("qq_app_id"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
