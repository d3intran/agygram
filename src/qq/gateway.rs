use super::client::QqClient;
use super::handler::{handle_c2c, handle_group};
use super::types::{
    C2CMessageEvent, GroupMessageEvent, WsDataEnvelope, WsEnvelopeHeader, WsHelloData,
    WsIdentifyData, WsOpcode, WsOptionalBoolEnvelope, WsOutgoingEnvelope, WsReadyData,
    WsResumeData,
};
use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, timeout_at, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Gateway intent: GROUP_AND_C2C (1 << 25) | INTERACTION (1 << 26)
const GROUP_AND_C2C_INTENT: u32 = (1 << 25) | (1 << 26);

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const HEALTHY_SESSION_THRESHOLD: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const WS_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 45_000;
const MIN_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const MAX_HEARTBEAT_INTERVAL_MS: u64 = 120_000;
const MAX_CONCURRENT_DISPATCHES: usize = 64;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

static JITTER_SEQ: AtomicU64 = AtomicU64::new(1);

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeState {
    session_id: String,
    last_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DisconnectReason {
    Cancelled,
    ReconnectRequested,
    InvalidSession { resumable: bool },
    HeartbeatTimeout,
    ServerClosed(Option<u16>),
    TransportError(String),
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionExit {
    reason: DisconnectReason,
    ready_duration: Option<Duration>,
}

/// Supervises the QQ WebSocket Gateway lifecycle with bounded concurrency, exponential
/// backoff + jitter, session resumption, and graceful cancellation.
#[tracing::instrument(skip(client, config, session_mgr, cancel))]
pub(crate) async fn run_gateway(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut retry_delay = INITIAL_RETRY_DELAY;
    let mut resume_state: Option<ResumeState> = None;
    let mut join_set: JoinSet<()> = JoinSet::new();
    let dispatch_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DISPATCHES));

    info!("Starting QQ Bot WebSocket Gateway supervisor...");

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let mut ready_duration_on_err = None;
        let session_res = run_connection_session(
            &client,
            &config,
            &session_mgr,
            &mut resume_state,
            &mut join_set,
            &dispatch_semaphore,
            &cancel,
        )
        .await;

        let reason = match session_res {
            Ok(exit) => {
                ready_duration_on_err = exit.ready_duration;
                exit.reason
            }
            Err(err) => {
                error!(error = %format!("{err:#}"), "QQ Gateway session setup failed");
                DisconnectReason::TransportError(err.to_string())
            }
        };

        if reason == DisconnectReason::Cancelled || cancel.is_cancelled() {
            info!("QQ Gateway shutdown requested via cancellation token");
            break;
        }

        let (base_wait, next_delay) = next_retry_delay(retry_delay, ready_duration_on_err);
        let min_wait = if matches!(reason, DisconnectReason::InvalidSession { .. }) {
            base_wait.max(Duration::from_secs(2))
        } else {
            base_wait
        };
        let wait_duration = apply_jitter(min_wait, jitter_entropy());

        warn!(
            reason = ?reason,
            ready_secs = ready_duration_on_err.map(|d| d.as_secs()),
            wait_ms = wait_duration.as_millis(),
            can_resume = resume_state.is_some(),
            "QQ Gateway disconnected; scheduling reconnect"
        );

        tokio::select! {
            _ = sleep(wait_duration) => {}
            _ = cancel.cancelled() => {
                info!("QQ Gateway reconnect backoff cancelled");
                break;
            }
        }

        retry_delay = next_delay;
    }

    drain_dispatch_tasks(&mut join_set, SHUTDOWN_DRAIN_TIMEOUT).await;
    Ok(())
}

async fn drain_dispatch_tasks(join_set: &mut JoinSet<()>, budget: Duration) {
    if join_set.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + budget;
    while !join_set.is_empty() {
        match timeout_at(deadline, join_set.join_next()).await {
            Ok(Some(Ok(()))) => {}
            Ok(Some(Err(err))) => warn!(error = %err, "QQ dispatch task failed during shutdown"),
            Ok(None) => break,
            Err(_) => {
                warn!(
                    remaining = join_set.len(),
                    "Timed out waiting for QQ dispatch tasks; aborting remaining tasks"
                );
                join_set.abort_all();
                while join_set.join_next().await.is_some() {}
                break;
            }
        }
    }
}

async fn run_connection_session(
    client: &Arc<QqClient>,
    config: &Arc<Config>,
    session_mgr: &Arc<SessionManager>,
    resume_state: &mut Option<ResumeState>,
    join_set: &mut JoinSet<()>,
    dispatch_semaphore: &Arc<Semaphore>,
    cancel: &CancellationToken,
) -> Result<SessionExit> {
    let gateway_url = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Ok(SessionExit { reason: DisconnectReason::Cancelled, ready_duration: None });
        }
        res = client.get_gateway_url() => {
            res.context("Failed to get gateway URL from QQ Open Platform")?
        }
    };

    info!(gateway_url = %gateway_url, "Connecting to QQ WebSocket Gateway");

    let (ws_stream, _) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Ok(SessionExit { reason: DisconnectReason::Cancelled, ready_duration: None });
        }
        res = timeout(CONNECT_TIMEOUT, connect_async(&gateway_url)) => {
            res
                .context("Timed out connecting to QQ WebSocket Gateway")?
                .context("Failed to establish WebSocket handshake with QQ Gateway")?
        }
    };

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Wait for Op 10 Hello frame within HELLO_TIMEOUT.
    let hello_deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    let hello_payload: WsHelloData = loop {
        let next_frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                close_ws_gracefully(&mut ws_write).await;
                return Ok(SessionExit { reason: DisconnectReason::Cancelled, ready_duration: None });
            }
            res = timeout_at(hello_deadline, ws_read.next()) => {
                res.context("Timed out waiting for Op 10 Hello from QQ Gateway")?
            }
        };

        match next_frame {
            Some(Ok(Message::Text(text))) => {
                let header: WsEnvelopeHeader<'_> =
                    serde_json::from_str(&text).context("Failed to parse Hello frame header")?;
                if WsOpcode::from_u8(header.op) != Some(WsOpcode::Hello) {
                    anyhow::bail!("First frame opcode was {}, expected Op 10 Hello", header.op);
                }
                let env: WsDataEnvelope<WsHelloData> =
                    serde_json::from_str(&text).context("Failed to parse Hello payload")?;
                break env.d;
            }
            Some(Ok(Message::Ping(ping_data))) => {
                send_ws_frame(&mut ws_write, Message::Pong(ping_data)).await?;
            }
            other => {
                anyhow::bail!("Expected Op 10 Hello frame on initial connect, received: {other:?}");
            }
        }
    };

    let heartbeat_interval = clamp_heartbeat_interval(hello_payload.heartbeat_interval);
    debug!(
        raw_interval_ms = ?hello_payload.heartbeat_interval,
        clamped_interval_ms = heartbeat_interval.as_millis(),
        "Received Op 10 Hello from QQ Gateway"
    );

    let auth_header = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            close_ws_gracefully(&mut ws_write).await;
            return Ok(SessionExit { reason: DisconnectReason::Cancelled, ready_duration: None });
        }
        res = client.get_auth_header() => res?,
    };

    // Consume any pending resume_state upfront so a failed Resume automatically falls back to
    // fresh Identify on the next attempt.
    let mut session_id: Option<String> = None;
    let mut last_s: u64 = 0;

    if let Some(resume) = resume_state.take() {
        let resume_frame = WsOutgoingEnvelope {
            op: WsOpcode::Resume.as_u8(),
            d: WsResumeData {
                token: &auth_header,
                session_id: &resume.session_id,
                seq: resume.last_seq,
            },
        };
        let payload =
            serde_json::to_string(&resume_frame).context("Failed to serialize Op 6 Resume")?;
        send_ws_frame(&mut ws_write, Message::Text(payload)).await?;
        session_id = Some(resume.session_id);
        last_s = resume.last_seq;
        info!(seq = last_s, "Op 6 Resume sent to QQ Gateway");
    } else {
        let identify_frame = WsOutgoingEnvelope {
            op: WsOpcode::Identify.as_u8(),
            d: WsIdentifyData {
                token: &auth_header,
                intents: GROUP_AND_C2C_INTENT,
                shard: [0, 1],
            },
        };
        let payload =
            serde_json::to_string(&identify_frame).context("Failed to serialize Op 2 Identify")?;
        send_ws_frame(&mut ws_write, Message::Text(payload)).await?;
        info!("Op 2 Identify sent to QQ Gateway. Listening for events...");
    }

    let mut heartbeat_timer = tokio::time::interval_at(
        tokio::time::Instant::now() + heartbeat_interval,
        heartbeat_interval,
    );
    heartbeat_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut awaiting_ack = false;
    let mut ready_at: Option<Instant> = None;
    let mut session_established = false;

    let reason = loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                close_ws_gracefully(&mut ws_write).await;
                break DisconnectReason::Cancelled;
            }

            _ = heartbeat_timer.tick() => {
                if awaiting_ack {
                    warn!(last_s, "QQ Gateway missed Op 11 Heartbeat ACK; reconnecting");
                    break DisconnectReason::HeartbeatTimeout;
                }
                debug!(s = last_s, "Sending Op 1 Heartbeat to QQ Gateway");
                if let Err(err) = send_heartbeat(&mut ws_write, last_s).await {
                    warn!(error = %err, "Failed to send Op 1 Heartbeat to QQ WebSocket");
                    break DisconnectReason::TransportError(err.to_string());
                }
                awaiting_ack = true;
            }

            Some(join_res) = join_set.join_next(), if !join_set.is_empty() => {
                if let Err(err) = join_res {
                    error!(error = %err, "QQ dispatch handler task panicked or was cancelled");
                }
            }

            incoming = ws_read.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let header: WsEnvelopeHeader<'_> = match serde_json::from_str(&text) {
                            Ok(h) => h,
                            Err(err) => {
                                warn!(
                                    error = %err,
                                    payload_len = text.len(),
                                    "Malformed QQ Gateway JSON frame"
                                );
                                continue;
                            }
                        };

                        if let Some(s) = header.s {
                            last_s = s;
                        }

                        match WsOpcode::from_u8(header.op) {
                            Some(WsOpcode::Dispatch) => {
                                let event_type = header.t.as_deref().unwrap_or("");
                                debug!(event_type, "Received Op 0 Dispatch event");

                                match event_type {
                                    "READY" => {
                                        match serde_json::from_str::<WsDataEnvelope<WsReadyData>>(&text) {
                                            Ok(env) => {
                                                if env.d.session_id.is_some() {
                                                    session_id = env.d.session_id;
                                                }
                                            }
                                            Err(err) => {
                                                warn!(error = %err, "Failed to parse QQ READY payload");
                                            }
                                        }
                                        ready_at = Some(Instant::now());
                                        session_established = true;
                                        info!("🎉 QQ Bot connection successfully authenticated! Gateway READY.");
                                    }
                                    "RESUMED" => {
                                        ready_at.get_or_insert_with(Instant::now);
                                        session_established = true;
                                        info!("QQ Bot session resumed successfully");
                                    }
                                    "C2C_MESSAGE_CREATE" => {
                                        match serde_json::from_str::<WsDataEnvelope<C2CMessageEvent>>(&text) {
                                            Ok(env) => {
                                                let event = env.d;
                                                let client_c = client.clone();
                                                let config_c = config.clone();
                                                let session_mgr_c = session_mgr.clone();
                                                spawn_bounded_dispatch(
                                                    join_set,
                                                    dispatch_semaphore,
                                                    "C2C_MESSAGE_CREATE",
                                                    async move {
                                                        handle_c2c(client_c, config_c, session_mgr_c, event).await;
                                                    },
                                                );
                                            }
                                            Err(err) => {
                                                warn!(
                                                    event_type = "C2C_MESSAGE_CREATE",
                                                    error = %err,
                                                    "Failed to decode QQ dispatch event"
                                                );
                                            }
                                        }
                                    }
                                    "GROUP_AT_MESSAGE_CREATE" => {
                                        match serde_json::from_str::<WsDataEnvelope<GroupMessageEvent>>(&text) {
                                            Ok(env) => {
                                                let event = env.d;
                                                let client_c = client.clone();
                                                let config_c = config.clone();
                                                let session_mgr_c = session_mgr.clone();
                                                spawn_bounded_dispatch(
                                                    join_set,
                                                    dispatch_semaphore,
                                                    "GROUP_AT_MESSAGE_CREATE",
                                                    async move {
                                                        handle_group(client_c, config_c, session_mgr_c, event).await;
                                                    },
                                                );
                                            }
                                            Err(err) => {
                                                warn!(
                                                    event_type = "GROUP_AT_MESSAGE_CREATE",
                                                    error = %err,
                                                    "Failed to decode QQ dispatch event"
                                                );
                                            }
                                        }
                                    }
                                    other => {
                                        debug!(event_type = other, "Unhandled QQ Dispatch event");
                                    }
                                }
                            }
                            Some(WsOpcode::Heartbeat) => {
                                // Server-initiated heartbeat request; respond immediately without
                                // altering periodic `awaiting_ack`.
                                if let Err(err) = send_heartbeat(&mut ws_write, last_s).await {
                                    warn!(error = %err, "Failed to reply to server Op 1 Heartbeat");
                                    break DisconnectReason::TransportError(err.to_string());
                                }
                            }
                            Some(WsOpcode::Reconnect) => {
                                info!("QQ Gateway requested Op 7 Reconnect.");
                                break DisconnectReason::ReconnectRequested;
                            }
                            Some(WsOpcode::InvalidSession) => {
                                let resumable = serde_json::from_str::<WsOptionalBoolEnvelope>(&text)
                                    .ok()
                                    .and_then(|env| env.d)
                                    .unwrap_or(false);
                                warn!(resumable, "QQ Gateway returned Op 9 Invalid Session");
                                if !resumable {
                                    client.invalidate_auth_header(&auth_header);
                                }
                                session_id = None;
                                session_established = false;
                                break DisconnectReason::InvalidSession { resumable };
                            }
                            Some(WsOpcode::HeartbeatAck) => {
                                awaiting_ack = false;
                                debug!("Received Op 11 Heartbeat ACK from QQ Gateway.");
                            }
                            Some(WsOpcode::Hello | WsOpcode::Identify | WsOpcode::Resume)
                            | None => {
                                debug!(op = header.op, "Ignoring unexpected opcode from QQ Gateway");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(ping_data))) => {
                        if let Err(err) = send_ws_frame(&mut ws_write, Message::Pong(ping_data)).await {
                            warn!(error = %err, "Failed to send WebSocket Pong");
                            break DisconnectReason::TransportError(err.to_string());
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code));
                        warn!(close_frame = ?frame, "QQ WebSocket connection closed by server");
                        break DisconnectReason::ServerClosed(code);
                    }
                    Some(Err(err)) => {
                        error!(error = %err, "QQ WebSocket stream error");
                        break DisconnectReason::TransportError(err.to_string());
                    }
                    None => {
                        warn!("QQ WebSocket stream reached EOF");
                        break DisconnectReason::Eof;
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    };

    if session_established
        && !matches!(
            reason,
            DisconnectReason::Cancelled | DisconnectReason::InvalidSession { .. }
        )
    {
        if let Some(sid) = session_id {
            *resume_state = Some(ResumeState {
                session_id: sid,
                last_seq: last_s,
            });
        }
    }

    Ok(SessionExit {
        reason,
        ready_duration: ready_at.map(|started| started.elapsed()),
    })
}

fn spawn_bounded_dispatch<F>(
    join_set: &mut JoinSet<()>,
    semaphore: &Arc<Semaphore>,
    event_name: &'static str,
    fut: F,
) where
    F: Future<Output = ()> + Send + 'static,
{
    match semaphore.clone().try_acquire_owned() {
        Ok(permit) => {
            join_set.spawn(async move {
                let _permit = permit;
                fut.await;
            });
        }
        Err(_) => {
            warn!(
                event_name,
                limit = MAX_CONCURRENT_DISPATCHES,
                "QQ Gateway dispatch concurrency limit reached; dropping event"
            );
        }
    }
}

async fn send_heartbeat(ws_write: &mut WsSink, last_s: u64) -> Result<()> {
    let hb = WsOutgoingEnvelope {
        op: WsOpcode::Heartbeat.as_u8(),
        d: (last_s > 0).then_some(last_s),
    };
    let payload = serde_json::to_string(&hb).context("Failed to serialize Op 1 Heartbeat")?;
    send_ws_frame(ws_write, Message::Text(payload)).await
}

async fn send_ws_frame(ws_write: &mut WsSink, frame: Message) -> Result<()> {
    timeout(WS_WRITE_TIMEOUT, ws_write.send(frame))
        .await
        .context("Timed out writing frame to QQ WebSocket")?
        .context("Failed to write frame to QQ WebSocket")
}

async fn close_ws_gracefully(ws_write: &mut WsSink) {
    let _ = timeout(Duration::from_secs(2), ws_write.send(Message::Close(None))).await;
    let _ = timeout(Duration::from_secs(1), ws_write.close()).await;
}

/// Clamps server-supplied heartbeat intervals into a safe `[5s, 120s]` window.
#[inline]
#[must_use]
fn clamp_heartbeat_interval(raw_ms: Option<u64>) -> Duration {
    let clamped = raw_ms
        .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_MS)
        .clamp(MIN_HEARTBEAT_INTERVAL_MS, MAX_HEARTBEAT_INTERVAL_MS);
    Duration::from_millis(clamped)
}

/// Computes `(current_sleep_base, next_retry_delay)`. Only resets backoff after a session
/// stays `READY` at least [`HEALTHY_SESSION_THRESHOLD`].
#[inline]
#[must_use]
fn next_retry_delay(current: Duration, ready_elapsed: Option<Duration>) -> (Duration, Duration) {
    if ready_elapsed.is_some_and(|elapsed| elapsed >= HEALTHY_SESSION_THRESHOLD) {
        (INITIAL_RETRY_DELAY, INITIAL_RETRY_DELAY * 2)
    } else {
        let clamped_current = current.clamp(INITIAL_RETRY_DELAY, MAX_RETRY_DELAY);
        let next = (clamped_current * 2).min(MAX_RETRY_DELAY);
        (clamped_current, next)
    }
}

/// Adds up to +25% deterministic-from-entropy jitter capped at [`MAX_RETRY_DELAY`].
#[inline]
#[must_use]
fn apply_jitter(base: Duration, entropy: u64) -> Duration {
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let max_ms = u64::try_from(MAX_RETRY_DELAY.as_millis()).unwrap_or(30_000);
    let span_ms = (base_ms / 4).max(1);
    let jitter_ms = entropy % (span_ms + 1);
    Duration::from_millis(base_ms.saturating_add(jitter_ms).min(max_ms))
}

fn jitter_entropy() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    let seq = JITTER_SEQ.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    mixed ^ (mixed >> 29)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_heartbeat_interval_enforces_bounds() {
        assert_eq!(
            clamp_heartbeat_interval(None),
            Duration::from_millis(45_000)
        );
        assert_eq!(
            clamp_heartbeat_interval(Some(0)),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            clamp_heartbeat_interval(Some(1_500)),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            clamp_heartbeat_interval(Some(45_000)),
            Duration::from_millis(45_000)
        );
        assert_eq!(
            clamp_heartbeat_interval(Some(600_000)),
            Duration::from_millis(120_000)
        );
    }

    #[test]
    fn backoff_resets_only_after_healthy_ready_duration() {
        // Immediate disconnect doubles up to MAX_RETRY_DELAY (30s).
        let (sleep1, d1) = next_retry_delay(Duration::from_secs(1), None);
        assert_eq!(sleep1, Duration::from_secs(1));
        assert_eq!(d1, Duration::from_secs(2));

        // Short READY (<30s) still backs off exponentially.
        let (sleep2, d2) = next_retry_delay(d1, Some(Duration::from_secs(10)));
        assert_eq!(sleep2, Duration::from_secs(2));
        assert_eq!(d2, Duration::from_secs(4));

        let (sleep_cap, d_cap) = next_retry_delay(Duration::from_secs(20), None);
        assert_eq!(sleep_cap, Duration::from_secs(20));
        assert_eq!(d_cap, Duration::from_secs(30));

        // Healthy READY (>=30s) resets sleep base back to 1s.
        let (sleep_ok, d_ok) =
            next_retry_delay(Duration::from_secs(30), Some(Duration::from_secs(30)));
        assert_eq!(sleep_ok, Duration::from_secs(1));
        assert_eq!(d_ok, Duration::from_secs(2));
    }

    #[test]
    fn apply_jitter_bounds_and_caps_at_max_delay() {
        for entropy in [0, 1, 17, 999_999] {
            let d = apply_jitter(Duration::from_secs(4), entropy);
            assert!(d >= Duration::from_secs(4));
            assert!(d <= Duration::from_secs(5));

            let capped = apply_jitter(MAX_RETRY_DELAY, entropy);
            assert_eq!(capped, MAX_RETRY_DELAY);
        }
    }

    #[test]
    fn gateway_envelope_parsing_handles_ready_and_invalid_session() {
        let ready = r#"{"op":0,"s":7,"t":"READY","d":{"version":1,"session_id":"sess-xyz"}}"#;
        let hdr: WsEnvelopeHeader<'_> = serde_json::from_str(ready).unwrap();
        assert_eq!(WsOpcode::from_u8(hdr.op), Some(WsOpcode::Dispatch));
        assert_eq!(hdr.s, Some(7));
        assert_eq!(hdr.t.as_deref(), Some("READY"));
        let data: WsDataEnvelope<WsReadyData> = serde_json::from_str(ready).unwrap();
        assert_eq!(data.d.session_id.as_deref(), Some("sess-xyz"));

        let inv_false = r#"{"op":9,"d":false}"#;
        let inv: WsOptionalBoolEnvelope = serde_json::from_str(inv_false).unwrap();
        assert_eq!(inv.d, Some(false));

        let inv_missing = r#"{"op":9}"#;
        let inv2: WsOptionalBoolEnvelope = serde_json::from_str(inv_missing).unwrap();
        assert_eq!(inv2.d, None);
    }

    #[tokio::test]
    async fn bounded_dispatch_enforces_concurrency_cap_and_drains() {
        let mut join_set = JoinSet::new();
        let sem = Arc::new(Semaphore::new(2));

        let (tx1, rx1) = tokio::sync::oneshot::channel::<()>();
        let (tx2, rx2) = tokio::sync::oneshot::channel::<()>();

        spawn_bounded_dispatch(&mut join_set, &sem, "T1", async move {
            let _ = rx1.await;
        });
        spawn_bounded_dispatch(&mut join_set, &sem, "T2", async move {
            let _ = rx2.await;
        });
        // Third dispatch while 2 permits are held is dropped cleanly instead of spawning unbounded.
        spawn_bounded_dispatch(&mut join_set, &sem, "T3", async {});

        assert_eq!(join_set.len(), 2);
        let _ = tx1.send(());
        let _ = tx2.send(());
        drain_dispatch_tasks(&mut join_set, Duration::from_secs(2)).await;
        assert!(join_set.is_empty());
        assert_eq!(sem.available_permits(), 2);
    }

    #[tokio::test]
    async fn run_gateway_connects_ready_sends_heartbeat_and_cancels_cleanly() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let ws_url = format!("ws://{addr}");

        let http_task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut s, _)) = http_listener.accept().await else {
                    break;
                };
                let ws_url = ws_url.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.contains("/app/getAppAccessToken") {
                        r#"{"access_token":"gw_tok","expires_in":7200}"#.to_string()
                    } else {
                        format!(r#"{{"url":"{ws_url}"}}"#)
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<(String, bool)>();
        let ws_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // Send Op 10 Hello (interval 100ms gets clamped to 5000ms).
            ws.send(Message::Text(
                r#"{"op":10,"d":{"heartbeat_interval":100}}"#.into(),
            ))
            .await
            .unwrap();

            let identify = ws.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(identify.contains(r#""op":2"#));
            assert!(identify.contains("QQBot gw_tok"));

            ws.send(Message::Text(
                r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"s_1"}}"#.into(),
            ))
            .await
            .unwrap();

            // Wait for close frame upon cancellation.
            let mut saw_close = false;
            while let Some(msg) = ws.next().await {
                if let Ok(Message::Close(_)) = msg {
                    saw_close = true;
                    break;
                }
            }
            let _ = done_tx.send((identify, saw_close));
        });

        let base = format!("http://{http_addr}");
        let client = Arc::new(QqClient::new_for_test("id", "sec", &base, &base));
        let dir = std::env::temp_dir().join(format!("agygram_qq_gw_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Arc::new(Config::default());
        let mgr = Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ));
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        let gw_handle = tokio::spawn(run_gateway(client, cfg, mgr, cancel));

        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel_trigger.cancel();

        timeout(Duration::from_secs(3), gw_handle)
            .await
            .expect("gateway must shut down promptly")
            .unwrap()
            .unwrap();

        let (_identify, saw_close) = timeout(Duration::from_secs(2), done_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(saw_close, "gateway must send WS Close frame on cancel");

        http_task.abort();
        ws_task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_gateway_resumes_after_op7_reconnect_and_survives_malformed_json() {
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_addr = http_listener.local_addr().unwrap();
        let ws_url = format!("ws://{addr}");

        let http_task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut s, _)) = http_listener.accept().await else {
                    break;
                };
                let ws_url = ws_url.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.contains("/app/getAppAccessToken") {
                        r#"{"access_token":"gw_tok","expires_in":7200}"#.to_string()
                    } else {
                        format!(r#"{{"url":"{ws_url}"}}"#)
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });

        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel::<String>();
        let ws_task = tokio::spawn(async move {
            // Connection 1: Hello -> Identify -> READY -> malformed JSON -> Op 7 Reconnect
            let (stream1, _) = listener.accept().await.unwrap();
            let mut ws1 = accept_async(stream1).await.unwrap();
            ws1.send(Message::Text(
                r#"{"op":10,"d":{"heartbeat_interval":10000}}"#.into(),
            ))
            .await
            .unwrap();
            let id_frame = ws1.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(id_frame.contains(r#""op":2"#));
            ws1.send(Message::Text(
                r#"{"op":0,"s":12,"t":"READY","d":{"session_id":"sess_resume_1"}}"#.into(),
            ))
            .await
            .unwrap();
            // Malformed payload must be logged & skipped without dropping the session.
            ws1.send(Message::Text("{not valid json".into()))
                .await
                .unwrap();
            ws1.send(Message::Text(r#"{"op":7}"#.into())).await.unwrap();

            // Connection 2: Hello -> expect Op 6 Resume with session_id & seq 12 -> RESUMED
            let (stream2, _) = listener.accept().await.unwrap();
            let mut ws2 = accept_async(stream2).await.unwrap();
            ws2.send(Message::Text(
                r#"{"op":10,"d":{"heartbeat_interval":10000}}"#.into(),
            ))
            .await
            .unwrap();
            let resume_frame = ws2.next().await.unwrap().unwrap().into_text().unwrap();
            ws2.send(Message::Text(
                r#"{"op":0,"s":13,"t":"RESUMED","d":""}"#.into(),
            ))
            .await
            .unwrap();
            let _ = resume_tx.send(resume_frame);
            while ws2.next().await.is_some() {}
        });

        let base = format!("http://{http_addr}");
        let client = Arc::new(QqClient::new_for_test("id", "sec", &base, &base));
        let dir =
            std::env::temp_dir().join(format!("agygram_qq_resume_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Arc::new(Config::default());
        let mgr = Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ));
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();

        let gw_handle = tokio::spawn(run_gateway(client, cfg, mgr, cancel));

        let resume_frame = timeout(Duration::from_secs(5), resume_rx)
            .await
            .expect("gateway must reconnect and send Op 6 Resume")
            .unwrap();
        assert!(resume_frame.contains(r#""op":6"#));
        assert!(resume_frame.contains("sess_resume_1"));
        assert!(resume_frame.contains(r#""seq":12"#));

        cancel_trigger.cancel();
        timeout(Duration::from_secs(3), gw_handle)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        http_task.abort();
        ws_task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
