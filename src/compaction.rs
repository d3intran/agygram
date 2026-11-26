//! Seamless, silent background context compaction for long-lived Telegram and QQ sessions.
//!
//! Features:
//! - Model-aware soft cap (`min(configured, cap)`: Claude `110_000`, GPT-OSS `80_000`,
//!   Gemini/default `160_000`).
//! - Tier-1 immediate idle delay (`auto_compact_idle_secs`, default `30s`) above Tier-1 threshold.
//! - Tier-2 idle-decay delay (`auto_compact_idle_decay_secs`, default `1800s`) above
//!   `auto_compact_idle_threshold_tokens` (default `120_000`).
//! - Single `+60s` reschedule when skipped solely due to host busyness.
//! - Low-cost `/compact` execution: forces `effort = "low"`, `sibling_model_for_effort(Low)`,
//!   `showThinking: false` + `permissions.deny`, one-shot `Print` backend (after evicting warm
//!   worker), and `BG_COMPACT_MAX_EXEC_SECS = 85` (< Telegram 90s transparent wait).

use crate::config::{Config, ExecutorBackend};
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent, build_session_settings_json};
use crate::models::{Effort, sibling_model_for_effort};
use crate::session::{
    BgCompactAttempt, BgCompactStartParams, ChatSession, SessionManager, TurnGuard,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const BG_COMPACT_IDLE_TIMEOUT_SECS: u64 = 75;
const BG_COMPACT_MAX_EXEC_SECS: u64 = 85;
pub(crate) const BG_COMPACT_MIN_TURN_COOLDOWN: u64 = 5;
pub(crate) const BG_COMPACT_FAILURE_BACKOFF_TURNS: u64 = 3;
const BG_COMPACT_BUSY_RESCHEDULE_SECS: u64 = 60;
const CLAUDE_SOFT_CAP_TOKENS: u64 = 110_000;
const GPT_OSS_SOFT_CAP_TOKENS: u64 = 80_000;

/// Computes the effective auto-compact context threshold after applying model-aware soft caps
/// (`min(configured, cap)`: Claude `110_000`, GPT-OSS `80_000`, Gemini/default `configured`).
#[must_use]
pub(crate) fn effective_compact_threshold(configured: u64, model: &str) -> u64 {
    if configured == 0 {
        return 0;
    }
    let lower = model.trim().to_ascii_lowercase();
    if lower.starts_with("claude") {
        configured.min(CLAUDE_SOFT_CAP_TOKENS)
    } else if lower.starts_with("gpt-oss") || lower.starts_with("gpt-") {
        configured.min(GPT_OSS_SOFT_CAP_TOKENS)
    } else {
        configured
    }
}

fn default_sensitive_deny_paths() -> Vec<String> {
    let mut paths = vec![
        "~/.config/agygram/*".to_string(),
        "~/.config/userbot/*".to_string(),
        "~/.ssh/*".to_string(),
    ];
    if let Some(home) = dirs::home_dir() {
        for rel in [".config/agygram/*", ".config/userbot/*", ".ssh/*"] {
            let abs = home.join(rel).to_string_lossy().into_owned();
            if !paths.contains(&abs) {
                paths.push(abs);
            }
        }
    }
    paths
}

/// Evaluates whether Tier-1 or Tier-2 idle-decay background compaction should be scheduled.
/// Returns `Some((effective_threshold, idle_delay, tier_label))` when eligible.
#[must_use]
fn select_compact_tier(
    config: &Config,
    post_session: &ChatSession,
) -> Option<(u64, Duration, &'static str)> {
    let tier1_threshold =
        effective_compact_threshold(config.auto_compact_threshold_tokens, &post_session.model);
    if tier1_threshold == 0 {
        return None;
    }

    if post_session.last_context_tokens >= tier1_threshold {
        return Some((
            tier1_threshold,
            Duration::from_secs(config.auto_compact_idle_secs),
            "tier1",
        ));
    }

    if config.auto_compact_idle_decay_secs > 0 && config.auto_compact_idle_threshold_tokens > 0 {
        let tier2_threshold = effective_compact_threshold(
            config.auto_compact_idle_threshold_tokens,
            &post_session.model,
        )
        .min(tier1_threshold);
        if tier2_threshold > 0 && post_session.last_context_tokens >= tier2_threshold {
            return Some((
                tier2_threshold,
                Duration::from_secs(config.auto_compact_idle_decay_secs),
                "tier2_idle_decay",
            ));
        }
    }

    None
}

/// Schedules a debounced silent background `/compact` for `chat_id` if the session crossed
/// its model-aware Tier-1 or Tier-2 idle-decay threshold and all safety preconditions hold.
pub(crate) fn maybe_schedule_background_compact(
    session_mgr: Arc<SessionManager>,
    config: Arc<Config>,
    chat_id: i64,
    post_session: &ChatSession,
    is_compact_turn: bool,
    is_detached: bool,
    saw_checkpoint: bool,
) -> bool {
    if is_compact_turn || is_detached || saw_checkpoint {
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

    let Some((threshold, idle_delay, tier_label)) = select_compact_tier(&config, post_session)
    else {
        return false;
    };

    // Allow the very first compaction (`last_auto_compact_turn.is_none()`) even on turns 1..4.
    if let Some(last_compact) = post_session.last_auto_compact_turn
        && post_session.turn_count.saturating_sub(last_compact) < BG_COMPACT_MIN_TURN_COOLDOWN
    {
        debug!(
            chat_id,
            turn_count = post_session.turn_count,
            last_auto_compact_turn = last_compact,
            "skipping background auto-compact due to turn cooldown"
        );
        return false;
    }

    if let Some(last_attempt) = post_session.last_auto_compact_attempt_turn
        && post_session.turn_count.saturating_sub(last_attempt) < BG_COMPACT_FAILURE_BACKOFF_TURNS
    {
        debug!(
            chat_id,
            turn_count = post_session.turn_count,
            last_auto_compact_attempt_turn = last_attempt,
            "skipping background auto-compact due to recent attempt backoff"
        );
        return false;
    }

    let expected_turn_count = post_session.turn_count;
    let pre_context_tokens = post_session.last_context_tokens;
    let (token_id, cancel_token) = session_mgr.register_pending_bg_compact(chat_id);

    info!(
        chat_id,
        tier = tier_label,
        conversation_id = %expected_conv_id,
        context_tokens = pre_context_tokens,
        threshold,
        idle_delay_secs = idle_delay.as_secs(),
        "Scheduled silent background auto-compact"
    );

    tokio::spawn(async move {
        if !idle_delay.is_zero() {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                    debug!(chat_id, "Cancelled pending silent background auto-compact timer");
                    return;
                }
                _ = tokio::time::sleep(idle_delay) => {}
            }
        }

        let mut rescheduled_once = false;
        let (generation, session, exec_slot) = loop {
            if cancel_token.is_cancelled() {
                session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                return;
            }

            match session_mgr
                .try_begin_bg_compact(BgCompactStartParams {
                    chat_id,
                    expected_conv_id: &expected_conv_id,
                    expected_turn_count,
                    threshold,
                    min_turn_cooldown: BG_COMPACT_MIN_TURN_COOLDOWN,
                    failure_backoff_turns: BG_COMPACT_FAILURE_BACKOFF_TURNS,
                })
                .await
            {
                BgCompactAttempt::Started(gen_id, sess) => {
                    if let Some(slot) = session_mgr.try_acquire_exec_slot() {
                        session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                        break (gen_id, sess, slot);
                    }
                    // Global exec slots busy: release without recording a failed compaction attempt.
                    session_mgr.release_turn(chat_id, gen_id).await;
                    if rescheduled_once {
                        session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                        debug!(
                            chat_id,
                            "Skipped silent background auto-compact: global exec slots still busy after reschedule"
                        );
                        return;
                    }
                }
                BgCompactAttempt::HostBusy => {
                    if rescheduled_once {
                        session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                        debug!(
                            chat_id,
                            "Skipped silent background auto-compact: host still busy after reschedule"
                        );
                        return;
                    }
                }
                BgCompactAttempt::Skipped => {
                    session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                    debug!(
                        chat_id,
                        "Skipped silent background auto-compact: session preconditions no longer hold"
                    );
                    return;
                }
            }

            rescheduled_once = true;
            info!(
                chat_id,
                delay_secs = BG_COMPACT_BUSY_RESCHEDULE_SECS,
                "Host busy during silent background auto-compact; rescheduling once"
            );
            tokio::select! {
                biased;
                _ = cancel_token.cancelled() => {
                    session_mgr.clear_pending_bg_compact_if_match(chat_id, token_id);
                    debug!(chat_id, "Cancelled rescheduled silent background auto-compact timer");
                    return;
                }
                _ = tokio::time::sleep(Duration::from_secs(BG_COMPACT_BUSY_RESCHEDULE_SECS)) => {}
            }
        };

        let mut turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);

        // Evict any warm persistent worker for this chat BEFORE spawning one-shot `Print` `/compact`
        // so two `agy` processes never touch the same conversation simultaneously.
        crate::executor::evict_chat_worker(chat_id).await;

        let workspace = session.get_valid_workspace();
        let compact_model = sibling_model_for_effort(&session.model, Effort::Low)
            .unwrap_or_else(|| session.model.clone());
        let compact_effort = Effort::Low.as_str();
        let deny_paths = default_sensitive_deny_paths();
        let settings_json = build_session_settings_json(Some(false), None, None, &deny_paths);

        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(64);
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

        let params = ExecutionParams {
            chat_id,
            prompt: "/compact",
            conversation_id: Some(&expected_conv_id),
            is_first_turn: false,
            workspace: &workspace,
            extra_dirs: &session.extra_dirs,
            model: &compact_model,
            effort: compact_effort,
            mode: &session.mode,
            agent: &session.agent,
            system_prompt: "",
            sandbox: session.sandbox,
            project: &session.project,
            settings_json,
            json_schema: None,
            remote_agent: session.remote_agent,
            backend: ExecutorBackend::Print,
        };

        let start = Instant::now();
        let exec_fut = Box::pin(AgyExecutor::run_prompt_streaming(
            params,
            stream_tx,
            Some(pid_tx),
            BG_COMPACT_IDLE_TIMEOUT_SECS,
            BG_COMPACT_MAX_EXEC_SECS,
            None,
        ));

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
                let succeeded = res.status.eq_ignore_ascii_case("SUCCESS");
                let saw_cp = stream_saw_checkpoint || res.saw_checkpoint;
                let verified_compact =
                    saw_cp || (succeeded && res.context_tokens.is_some_and(|c| c < threshold));
                session_mgr
                    .finish_bg_compact(
                        chat_id,
                        generation,
                        succeeded,
                        res.context_tokens,
                        saw_cp,
                        threshold,
                    )
                    .await;
                turn_guard.disarm();
                if verified_compact {
                    info!(
                        chat_id,
                        generation,
                        tier = tier_label,
                        conversation_id = %expected_conv_id,
                        compact_model = %compact_model,
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
                        tier = tier_label,
                        status = %res.status,
                        post_context_tokens = ?res.context_tokens,
                        elapsed_secs,
                        "Silent background auto-compact ended without checkpoint or verified drop"
                    );
                }
            }
            Err(err) => {
                warn!(
                    chat_id,
                    generation,
                    tier = tier_label,
                    elapsed_secs,
                    error = %format!("{err:#}"),
                    "Silent background auto-compact failed"
                );
                session_mgr
                    .finish_bg_compact(chat_id, generation, false, None, false, threshold)
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

    #[test]
    fn model_aware_soft_cap_applies_claude_and_gpt_limits() {
        assert_eq!(effective_compact_threshold(160_000, ""), 160_000);
        assert_eq!(
            effective_compact_threshold(160_000, "gemini-4-neon-high"),
            160_000
        );
        assert_eq!(
            effective_compact_threshold(160_000, "claude-opus-4-6-thinking"),
            110_000
        );
        assert_eq!(
            effective_compact_threshold(90_000, "claude-sonnet-4-6"),
            90_000
        );
        assert_eq!(
            effective_compact_threshold(160_000, "gpt-oss-120b-medium"),
            80_000
        );
        assert_eq!(effective_compact_threshold(0, "claude-sonnet-4-6"), 0);
    }

    #[tokio::test]
    async fn skips_scheduling_when_below_threshold_or_disabled_or_detached() {
        let mgr = test_mgr("skip_conditions");
        let mut session =
            ChatSession::new("/tmp".into(), String::new(), "auto".into(), "high".into());
        session.conversation_id = Some("conv-1".into());
        session.turn_count = 10;
        session.last_context_tokens = 100_000;

        let cfg = Arc::new(Config {
            auto_compact_threshold_tokens: 160_000,
            auto_compact_idle_secs: 30,
            auto_compact_idle_decay_secs: 1800,
            auto_compact_idle_threshold_tokens: 120_000,
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

        // Tier-2 idle-decay schedules when context is between 120K and 160K.
        session.last_context_tokens = 130_000;
        let (t2_thresh, t2_delay, t2_label) =
            select_compact_tier(&cfg, &session).expect("tier-2 idle-decay selected");
        assert_eq!(t2_thresh, 120_000);
        assert_eq!(t2_delay, Duration::from_secs(1800));
        assert_eq!(t2_label, "tier2_idle_decay");

        // Claude model caps Tier-1 down to 110K.
        session.model = "claude-sonnet-4-6".to_string();
        session.last_context_tokens = 115_000;
        let (cl_thresh, cl_delay, cl_label) =
            select_compact_tier(&cfg, &session).expect("claude tier-1 selected");
        assert_eq!(cl_thresh, 110_000);
        assert_eq!(cl_delay, Duration::from_secs(30));
        assert_eq!(cl_label, "tier1");

        // First compaction works even on Turn 2 (`last_auto_compact_turn == None`).
        session.turn_count = 2;
        session.last_auto_compact_turn = None;
        assert!(maybe_schedule_background_compact(
            mgr.clone(),
            cfg.clone(),
            1,
            &session,
            false,
            false,
            false,
        ));
        mgr.cancel_pending_bg_compact(1);

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
        let g = mgr.begin_turn(42).await.unwrap();
        let post = mgr
            .finish_turn(
                42,
                g,
                TurnOutcome {
                    conversation_id: Some("conv-42".into()),
                    status: "SUCCESS",
                    input_chars: 10,
                    output_chars: 10,
                    usage: None,
                    usage_is_cumulative: false,
                    context_tokens: Some(180_000),
                    num_turns: Some(1),
                    saw_checkpoint: false,
                    system_prompt_sent: true,
                    injected_system_prompt_hash: Some(12345),
                    injected_voice_guide: false,
                    injected_widget_guide: false,
                    duration_seconds: Some(2.0),
                    detached: false,
                },
            )
            .await
            .expect("finish_turn returns snapshot");

        let cfg = Arc::new(Config {
            auto_compact_threshold_tokens: 160_000,
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
