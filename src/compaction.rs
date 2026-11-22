//! Seamless, silent background context compaction for long-lived Telegram and QQ sessions.
//!
//! When a completed turn's real single-step context watermark crosses
//! `config.auto_compact_threshold_tokens` (default `360_000`), this scheduler waits for
//! `config.auto_compact_idle_secs` (default `30s`) of chat silence and then runs `agy -p /compact`
//! quietly in the background without posting any message to the chat:
//! - If a user sends a message during the idle delay, the timer cancels immediately (zero impact
//!   on active chat).
//! - Background compaction only starts when the chat has no running foreground or live detached
//!   tasks, no other chat is actively occupying the VPS, and a non-blocking global execution slot
//!   is available.
//! - If a user sends a message while background compaction is already mid-flight, the chat waits
//!   transparently behind a normal thinking indicator and then runs on the freshly compacted
//!   context.

use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent};
use crate::session::{ChatSession, SessionManager, TurnGuard};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const BG_COMPACT_IDLE_TIMEOUT_SECS: u64 = 90;
const BG_COMPACT_MAX_EXEC_SECS: u64 = 120;
pub(crate) const BG_COMPACT_MIN_TURN_COOLDOWN: u64 = 5;

/// Schedules a debounced silent background `/compact` for `chat_id` if the session crossed
/// `config.auto_compact_threshold_tokens` and all safety preconditions hold.
pub(crate) fn maybe_schedule_background_compact(
    session_mgr: Arc<SessionManager>,
    config: Arc<Config>,
    chat_id: i64,
    post_session: &ChatSession,
    is_compact_turn: bool,
    is_detached: bool,
    saw_checkpoint: bool,
) -> bool {
    let threshold = config.auto_compact_threshold_tokens;
    if threshold == 0 || is_compact_turn || is_detached || saw_checkpoint {
        return false;
    }

    let Some(expected_conv_id) = post_session
        .conversation_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return false;
    };

    if post_session.last_context_tokens < threshold {
        return false;
    }

    if post_session
        .turn_count
        .saturating_sub(post_session.last_auto_compact_turn)
        < BG_COMPACT_MIN_TURN_COOLDOWN
    {
        debug!(
            chat_id,
            turn_count = post_session.turn_count,
            last_auto_compact_turn = post_session.last_auto_compact_turn,
            "skipping background auto-compact due to turn cooldown"
        );
        return false;
    }

    let expected_turn_count = post_session.turn_count;
    let pre_context_tokens = post_session.last_context_tokens;
    let idle_delay = Duration::from_secs(config.auto_compact_idle_secs);
    let cancel_token = session_mgr.register_pending_bg_compact(chat_id);

    info!(
        chat_id,
        conversation_id = %expected_conv_id,
        context_tokens = pre_context_tokens,
        threshold,
        idle_delay_secs = config.auto_compact_idle_secs,
        "Scheduled silent background auto-compact"
    );

    tokio::spawn(async move {
        if !idle_delay.is_zero() {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    debug!(chat_id, "Cancelled pending silent background auto-compact timer");
                    return;
                }
                _ = tokio::time::sleep(idle_delay) => {}
            }
        }

        if cancel_token.is_cancelled() {
            return;
        }

        let Some((generation, session)) = session_mgr
            .try_begin_bg_compact(
                chat_id,
                &expected_conv_id,
                expected_turn_count,
                threshold,
                BG_COMPACT_MIN_TURN_COOLDOWN,
            )
            .await
        else {
            debug!(
                chat_id,
                "Skipped silent background auto-compact: chat or host no longer idle"
            );
            return;
        };

        let mut turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);

        let Some(exec_slot) = session_mgr.try_acquire_exec_slot() else {
            debug!(
                chat_id,
                generation, "Skipped silent background auto-compact: global exec slots busy"
            );
            session_mgr
                .finish_bg_compact(chat_id, generation, false, None, false)
                .await;
            turn_guard.disarm();
            return;
        };

        let workspace = session.get_valid_workspace();
        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(64);
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

        let params = ExecutionParams {
            prompt: "/compact",
            conversation_id: Some(&expected_conv_id),
            is_first_turn: false,
            workspace: &workspace,
            extra_dirs: &session.extra_dirs,
            model: &session.model,
            effort: &session.effort,
            mode: &session.mode,
            agent: &session.agent,
            system_prompt: "",
            sandbox: session.sandbox,
            project: &session.project,
            settings_json: None,
            json_schema: None,
            remote_agent: session.remote_agent,
            use_acp: config.use_acp_backend(),
        };

        let start = Instant::now();
        let exec_fut = AgyExecutor::run_prompt_streaming(
            params,
            stream_tx,
            Some(pid_tx),
            BG_COMPACT_IDLE_TIMEOUT_SECS,
            BG_COMPACT_MAX_EXEC_SECS,
        );

        let session_mgr_pid = session_mgr.clone();
        let drain_fut = async move {
            let mut pid_rx = Some(pid_rx);
            let mut saw_checkpoint = false;
            loop {
                tokio::select! {
                    biased;
                    pid_res = async {
                        match pid_rx.as_mut() {
                            Some(rx) => rx.await,
                            None => std::future::pending().await,
                        }
                    }, if pid_rx.is_some() => {
                        pid_rx = None;
                        if let Ok(pid) = pid_res {
                            session_mgr_pid.set_pid(chat_id, pid, generation).await;
                        }
                    }
                    maybe_ev = stream_rx.recv() => {
                        let Some(ev) = maybe_ev else { break };
                        if matches!(ev, StreamEvent::Checkpoint) {
                            saw_checkpoint = true;
                        }
                    }
                }
            }
            saw_checkpoint
        };

        let (exec_res, stream_saw_checkpoint) = tokio::join!(exec_fut, drain_fut);
        drop(exec_slot);
        let elapsed_secs = start.elapsed().as_secs_f64();

        match exec_res {
            Ok(res) => {
                let succeeded = !res.status.eq_ignore_ascii_case("TIMEOUT")
                    && !res.status.eq_ignore_ascii_case("ERROR");
                let saw_cp = stream_saw_checkpoint || res.saw_checkpoint;
                session_mgr
                    .finish_bg_compact(chat_id, generation, succeeded, res.context_tokens, saw_cp)
                    .await;
                turn_guard.disarm();
                if succeeded || saw_cp {
                    info!(
                        chat_id,
                        generation,
                        conversation_id = %expected_conv_id,
                        pre_context_tokens,
                        post_context_tokens = ?res.context_tokens,
                        saw_checkpoint = saw_cp,
                        elapsed_secs,
                        "Silent background auto-compact completed"
                    );
                } else {
                    warn!(
                        chat_id,
                        generation,
                        status = %res.status,
                        elapsed_secs,
                        "Silent background auto-compact ended without checkpoint"
                    );
                }
            }
            Err(err) => {
                warn!(
                    chat_id,
                    generation,
                    elapsed_secs,
                    error = %format!("{err:#}"),
                    "Silent background auto-compact failed"
                );
                session_mgr
                    .finish_bg_compact(chat_id, generation, false, None, false)
                    .await;
                turn_guard.disarm();
            }
        }
    });

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TurnOutcome;

    fn test_mgr(name: &str) -> Arc<SessionManager> {
        let dir = std::env::temp_dir().join(format!(
            "agygram_bg_compact_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ))
    }

    #[tokio::test]
    async fn skips_scheduling_when_below_threshold_or_disabled_or_detached() {
        let mgr = test_mgr("skip_conditions");
        let mut session =
            ChatSession::new("/tmp".into(), String::new(), "auto".into(), "high".into());
        session.conversation_id = Some("conv-1".into());
        session.turn_count = 10;
        session.last_context_tokens = 200_000;

        let cfg = Arc::new(Config {
            auto_compact_threshold_tokens: 360_000,
            auto_compact_idle_secs: 30,
            ..Config::default()
        });

        assert!(!maybe_schedule_background_compact(
            mgr.clone(),
            cfg.clone(),
            1,
            &session,
            false,
            false,
            false,
        ));

        session.last_context_tokens = 400_000;
        // Skips when disabled (threshold == 0), detached, compact turn, or checkpoint already seen.
        let disabled_cfg = Arc::new(Config {
            auto_compact_threshold_tokens: 0,
            ..Config::default()
        });
        assert!(!maybe_schedule_background_compact(
            mgr.clone(),
            disabled_cfg,
            1,
            &session,
            false,
            false,
            false,
        ));
        assert!(!maybe_schedule_background_compact(
            mgr.clone(),
            cfg.clone(),
            1,
            &session,
            true,
            false,
            false,
        ));
        assert!(!maybe_schedule_background_compact(
            mgr.clone(),
            cfg.clone(),
            1,
            &session,
            false,
            true,
            false,
        ));
        assert!(!maybe_schedule_background_compact(
            mgr.clone(),
            cfg.clone(),
            1,
            &session,
            false,
            false,
            true,
        ));
    }

    #[tokio::test]
    async fn pending_timer_cancels_when_user_starts_new_turn() {
        let mgr = test_mgr("cancel_on_new_turn");
        for _ in 0..6 {
            let g = mgr.begin_turn(42).await.unwrap();
            assert!(
                mgr.finish_turn(
                    42,
                    g,
                    TurnOutcome {
                        conversation_id: Some("conv-42".into()),
                        input_chars: 10,
                        output_chars: 10,
                        usage: None,
                        context_tokens: Some(400_000),
                        num_turns: Some(1),
                        saw_checkpoint: false,
                        injected_system_prompt_hash: Some(12345),
                        duration_seconds: Some(2.0),
                        detached: false,
                    },
                )
                .await
            );
        }

        let post = mgr.get_or_create(42).await;
        let cfg = Arc::new(Config {
            auto_compact_threshold_tokens: 360_000,
            auto_compact_idle_secs: 60,
            ..Config::default()
        });

        assert!(maybe_schedule_background_compact(
            mgr.clone(),
            cfg,
            42,
            &post,
            false,
            false,
            false,
        ));
        assert!(mgr.has_pending_bg_compact(42));

        // A real user turn arriving during the idle window immediately cancels the pending timer.
        let g_user = mgr.begin_turn(42).await.unwrap();
        assert!(!mgr.has_pending_bg_compact(42));
        mgr.release_turn(42, g_user).await;
    }
}
