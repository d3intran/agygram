pub(crate) mod client;
pub(crate) mod commands;
pub(crate) mod context;
pub(crate) mod delivery;
pub(crate) mod formatter;
pub(crate) mod gateway;
pub(crate) mod handler;
pub(crate) mod media;
pub(crate) mod types;

use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Owned routing target for spawned QQ tasks (placeholder recall, detached follow-ups).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OwnedQqTarget {
    C2c(String),
    Group(String),
}

impl OwnedQqTarget {
    #[must_use]
    pub(crate) fn from_target(target: client::QqTarget<'_>) -> Self {
        match target {
            client::QqTarget::C2c(id) => Self::C2c(id.to_string()),
            client::QqTarget::Group(id) => Self::Group(id.to_string()),
        }
    }

    #[must_use]
    pub(crate) fn as_target(&self) -> client::QqTarget<'_> {
        match self {
            Self::C2c(id) => client::QqTarget::C2c(id),
            Self::Group(id) => client::QqTarget::Group(id),
        }
    }
}

type ChatCancelMap = FxHashMap<i64, Vec<(u64, CancellationToken)>>;

/// RAII guard that removes a registered per-chat turn cancellation token when the turn exits.
pub(crate) struct QqTurnCancelGuard {
    chat_cancels: Arc<StdMutex<ChatCancelMap>>,
    chat_id: i64,
    token_id: u64,
}

impl Drop for QqTurnCancelGuard {
    fn drop(&mut self) {
        let mut map = match self.chat_cancels.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(entries) = map.get_mut(&self.chat_id) {
            entries.retain(|(id, _)| *id != self.token_id);
            if entries.is_empty() {
                map.remove(&self.chat_id);
            }
        }
    }
}

/// Shared QQ Bot runtime managing cooperative shutdown, per-chat cancellation (`/cancel`, `/new`),
/// and tracked detached follow-up tasks.
#[derive(Clone)]
pub(crate) struct QqRuntime {
    root_cancel: CancellationToken,
    chat_cancels: Arc<StdMutex<ChatCancelMap>>,
    next_token_id: Arc<AtomicU64>,
    detached_tasks: Arc<tokio::sync::Mutex<JoinSet<()>>>,
}

impl QqRuntime {
    #[must_use]
    pub(crate) fn new(root_cancel: CancellationToken) -> Self {
        Self {
            root_cancel,
            chat_cancels: Arc::new(StdMutex::new(FxHashMap::default())),
            next_token_id: Arc::new(AtomicU64::new(1)),
            detached_tasks: Arc::new(tokio::sync::Mutex::new(JoinSet::new())),
        }
    }

    #[inline]
    #[must_use]
    pub(crate) fn root_cancel(&self) -> CancellationToken {
        self.root_cancel.clone()
    }

    /// Registers a per-chat child cancellation token for an active or queued turn.
    #[must_use]
    pub(crate) fn register_turn(&self, chat_id: i64) -> (QqTurnCancelGuard, CancellationToken) {
        let token_id = self.next_token_id.fetch_add(1, Ordering::Relaxed);
        let token = self.root_cancel.child_token();
        {
            let mut map = match self.chat_cancels.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.entry(chat_id)
                .or_default()
                .push((token_id, token.clone()));
        }
        let guard = QqTurnCancelGuard {
            chat_cancels: self.chat_cancels.clone(),
            chat_id,
            token_id,
        };
        (guard, token)
    }

    /// Cancels every active or queued turn registered for `chat_id`, returning how many tokens
    /// were signalled.
    pub(crate) fn cancel_chat(&self, chat_id: i64) -> usize {
        let tokens: Vec<CancellationToken> = {
            let map = match self.chat_cancels.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.get(&chat_id)
                .map(|entries| entries.iter().map(|(_, tok)| tok.clone()).collect())
                .unwrap_or_default()
        };
        let count = tokens.len();
        for tok in tokens {
            tok.cancel();
        }
        count
    }

    /// Spawns a detached follow-up task into the tracked `JoinSet`, reaping finished handles first.
    pub(crate) async fn spawn_detached<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut set = self.detached_tasks.lock().await;
        while set.try_join_next().is_some() {}
        set.spawn(fut);
    }

    /// Signals global and per-chat cancellation across all active QQ tasks.
    pub(crate) fn begin_shutdown(&self) {
        self.root_cancel.cancel();
        let tokens: Vec<CancellationToken> = {
            let map = match self.chat_cancels.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.values()
                .flat_map(|entries| entries.iter().map(|(_, tok)| tok.clone()))
                .collect()
        };
        for tok in tokens {
            tok.cancel();
        }
    }

    /// Waits up to `budget` for tracked detached follow-up tasks to exit gracefully, aborting
    /// remaining stuck tasks only if the grace budget expires.
    pub(crate) async fn drain_detached(&self, budget: Duration) {
        let mut set = self.detached_tasks.lock().await;
        if set.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + budget;
        while !set.is_empty() {
            match tokio::time::timeout_at(deadline, set.join_next()).await {
                Ok(Some(Ok(()))) => {}
                Ok(Some(Err(err))) => {
                    warn!(error = %err, "QQ detached follow-up task failed during shutdown");
                }
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        remaining = set.len(),
                        "Timed out waiting for QQ detached follow-up tasks; aborting remaining"
                    );
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    break;
                }
            }
        }
    }
}

/// Runs the QQ Bot service with a cooperative [`CancellationToken`] for graceful shutdown.
///
/// # Errors
/// Returns an error if QQ credentials are missing, HTTP client initialization fails,
/// or the gateway supervisor fails unrecoverably.
pub(crate) async fn run_qq_bot_with_cancel(
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

    #[tokio::test]
    async fn qq_runtime_cancels_per_chat_and_drains_detached_gracefully() {
        let root = CancellationToken::new();
        let runtime = QqRuntime::new(root.clone());

        let (g1, t1) = runtime.register_turn(100);
        let (_g2, t2) = runtime.register_turn(200);
        assert!(!t1.is_cancelled());
        assert!(!t2.is_cancelled());

        assert_eq!(runtime.cancel_chat(100), 1);
        assert!(t1.is_cancelled());
        assert!(!t2.is_cancelled());
        drop(g1);
        assert_eq!(runtime.cancel_chat(100), 0);

        let shutdown_tok = runtime.root_cancel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<bool>();
        runtime
            .spawn_detached(async move {
                shutdown_tok.cancelled().await;
                let _ = done_tx.send(true);
            })
            .await;

        runtime.begin_shutdown();
        assert!(t2.is_cancelled());
        runtime.drain_detached(Duration::from_secs(2)).await;
        assert!(done_rx.await.unwrap());
    }
}
