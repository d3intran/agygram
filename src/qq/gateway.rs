use super::QqRuntime;
use super::client::QqClient;
use super::handler::{handle_c2c, handle_group, handle_group_passive};
use super::types::{
    C2CMessageEvent, GroupMessageEvent, WsHelloData, WsIdentifyData, WsOpcode, WsOutgoingEnvelope,
    WsReadyData, WsResumeData,
};
use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use rustc_hash::FxHashSet;
use serde::de::{self, Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::VecDeque;
use std::fmt;
use std::future::{Future, pending};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, sleep, sleep_until, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
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
/// Maximum wait for `Op 11 HeartbeatAck` after sending `Op 1 Heartbeat`, avoiding up to
/// `2 * heartbeat_interval` stalls on half-open sockets.
const HEARTBEAT_ACK_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 45_000;
const MIN_HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const MAX_HEARTBEAT_INTERVAL_MS: u64 = 120_000;
const MAX_CONCURRENT_DISPATCHES: usize = 64;
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DEDUP_ENTRIES: usize = 2048;

static JITTER_SEQ: AtomicU64 = AtomicU64::new(1);

type WsSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;

/// Bounded FIFO event deduplicator so QQ Gateway retries/resumes never run `agy` twice for
/// the same `msg_id`.
struct EventDeduplicator {
    order: VecDeque<String>,
    seen: FxHashSet<String>,
    capacity: usize,
}

impl EventDeduplicator {
    fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            order: VecDeque::with_capacity(cap),
            seen: FxHashSet::default(),
            capacity: cap,
        }
    }

    /// Returns `true` if this `(scope, event_id)` has not been seen recently and records it;
    /// returns `false` if it is a duplicate. Empty IDs are never deduplicated.
    fn check_and_insert(&mut self, scope: &str, event_id: &str) -> bool {
        let id = event_id.trim();
        if id.is_empty() {
            return true;
        }
        let key = format!("{scope}:{id}");
        if self.seen.contains(&key) {
            return false;
        }
        while self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        true
    }
}

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
    ws_connected: bool,
    attempted_resume: bool,
}

/// Decoded payload from a single-pass incoming QQ Gateway JSON frame.
#[derive(Debug)]
enum IncomingWsPayload {
    None,
    Hello(WsHelloData),
    Ready(WsReadyData),
    C2cMessage(Box<C2CMessageEvent>),
    GroupAtMessage(Box<GroupMessageEvent>),
    GroupPassiveMessage(Box<GroupMessageEvent>),
    InvalidSession(bool),
    DecodeError {
        event_type: &'static str,
        error: String,
    },
}

/// Incoming QQ WebSocket envelope decoded in a single JSON pass without double-parsing.
///
/// For standard QQ key order (`op`, `s`, `t`, `d`), `"d"` streams directly into its concrete
/// target type (or `IgnoredAny`) with zero DOM allocation. If `"d"` arrives before `"op"` or `"t"`,
/// only `"d"` is buffered as a `serde_json::Value` and decoded at the end of the same pass.
#[derive(Debug)]
struct IncomingWsFrame {
    op: u8,
    s: Option<u64>,
    t: Option<String>,
    payload: IncomingWsPayload,
}

struct LenientOptBool(Option<bool>);

impl<'de> Deserialize<'de> for LenientOptBool {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LenientOptBoolVisitor;

        impl<'de> Visitor<'de> for LenientOptBoolVisitor {
            type Value = LenientOptBool;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an optional boolean")
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(Some(v)))
            }

            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                Ok(LenientOptBool(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(LenientOptBool(None))
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(LenientOptBool(None))
            }
        }

        deserializer.deserialize_any(LenientOptBoolVisitor)
    }
}

enum WsFrameField {
    Op,
    S,
    T,
    D,
    Ignore,
}

impl<'de> Deserialize<'de> for WsFrameField {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = WsFrameField;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a gateway frame field")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(match value {
                    "op" => WsFrameField::Op,
                    "s" => WsFrameField::S,
                    "t" => WsFrameField::T,
                    "d" => WsFrameField::D,
                    _ => WsFrameField::Ignore,
                })
            }
        }

        deserializer.deserialize_identifier(FieldVisitor)
    }
}

fn decode_deferred_payload(
    op: u8,
    event_type: Option<&str>,
    raw_d: serde_json::Value,
) -> IncomingWsPayload {
    match WsOpcode::from_u8(op) {
        Some(WsOpcode::Hello) => match serde_json::from_value::<WsHelloData>(raw_d) {
            Ok(hello) => IncomingWsPayload::Hello(hello),
            Err(err) => IncomingWsPayload::DecodeError {
                event_type: "HELLO",
                error: err.to_string(),
            },
        },
        Some(WsOpcode::InvalidSession) => {
            IncomingWsPayload::InvalidSession(raw_d.as_bool().unwrap_or(false))
        }
        Some(WsOpcode::Dispatch) => match event_type.unwrap_or("") {
            "READY" => match serde_json::from_value::<WsReadyData>(raw_d) {
                Ok(ready) => IncomingWsPayload::Ready(ready),
                Err(err) => IncomingWsPayload::DecodeError {
                    event_type: "READY",
                    error: err.to_string(),
                },
            },
            "C2C_MESSAGE_CREATE" => match serde_json::from_value::<Box<C2CMessageEvent>>(raw_d) {
                Ok(ev) => IncomingWsPayload::C2cMessage(ev),
                Err(err) => IncomingWsPayload::DecodeError {
                    event_type: "C2C_MESSAGE_CREATE",
                    error: err.to_string(),
                },
            },
            "GROUP_AT_MESSAGE_CREATE" => {
                match serde_json::from_value::<Box<GroupMessageEvent>>(raw_d) {
                    Ok(ev) => IncomingWsPayload::GroupAtMessage(ev),
                    Err(err) => IncomingWsPayload::DecodeError {
                        event_type: "GROUP_AT_MESSAGE_CREATE",
                        error: err.to_string(),
                    },
                }
            }
            "GROUP_MESSAGE_CREATE" => {
                match serde_json::from_value::<Box<GroupMessageEvent>>(raw_d) {
                    Ok(ev) => IncomingWsPayload::GroupPassiveMessage(ev),
                    Err(err) => IncomingWsPayload::DecodeError {
                        event_type: "GROUP_MESSAGE_CREATE",
                        error: err.to_string(),
                    },
                }
            }
            _ => IncomingWsPayload::None,
        },
        _ => IncomingWsPayload::None,
    }
}

impl<'de> Deserialize<'de> for IncomingWsFrame {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FrameVisitor;

        impl<'de> Visitor<'de> for FrameVisitor {
            type Value = IncomingWsFrame;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a QQ WebSocket Gateway JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut op: Option<u8> = None;
                let mut s: Option<u64> = None;
                let mut t: Option<String> = None;
                let mut payload: Option<IncomingWsPayload> = None;
                let mut deferred_d: Option<serde_json::Value> = None;

                while let Some(field) = map.next_key::<WsFrameField>()? {
                    match field {
                        WsFrameField::Op => {
                            op = Some(map.next_value()?);
                        }
                        WsFrameField::S => {
                            s = map.next_value()?;
                        }
                        WsFrameField::T => {
                            t = map.next_value()?;
                        }
                        WsFrameField::Ignore => {
                            let _ = map.next_value::<IgnoredAny>()?;
                        }
                        WsFrameField::D => match op.and_then(WsOpcode::from_u8) {
                            Some(WsOpcode::Hello) => {
                                if let Some(hello) = map.next_value::<Option<WsHelloData>>()? {
                                    payload = Some(IncomingWsPayload::Hello(hello));
                                }
                            }
                            Some(WsOpcode::InvalidSession) => {
                                let resumable = map.next_value::<LenientOptBool>()?.0;
                                payload = Some(IncomingWsPayload::InvalidSession(
                                    resumable.unwrap_or(false),
                                ));
                            }
                            Some(WsOpcode::Dispatch) => match t.as_deref() {
                                Some("READY") => {
                                    if let Some(ready) = map.next_value::<Option<WsReadyData>>()? {
                                        payload = Some(IncomingWsPayload::Ready(ready));
                                    }
                                }
                                Some("C2C_MESSAGE_CREATE") => {
                                    let ev = map.next_value::<Box<C2CMessageEvent>>()?;
                                    payload = Some(IncomingWsPayload::C2cMessage(ev));
                                }
                                Some("GROUP_AT_MESSAGE_CREATE") => {
                                    let ev = map.next_value::<Box<GroupMessageEvent>>()?;
                                    payload = Some(IncomingWsPayload::GroupAtMessage(ev));
                                }
                                Some("GROUP_MESSAGE_CREATE") => {
                                    let ev = map.next_value::<Box<GroupMessageEvent>>()?;
                                    payload = Some(IncomingWsPayload::GroupPassiveMessage(ev));
                                }
                                Some(_) => {
                                    let _ = map.next_value::<IgnoredAny>()?;
                                    payload = Some(IncomingWsPayload::None);
                                }
                                None => {
                                    deferred_d = Some(map.next_value::<serde_json::Value>()?);
                                }
                            },
                            Some(_) => {
                                let _ = map.next_value::<IgnoredAny>()?;
                                payload = Some(IncomingWsPayload::None);
                            }
                            None if op.is_some() => {
                                let _ = map.next_value::<IgnoredAny>()?;
                                payload = Some(IncomingWsPayload::None);
                            }
                            None => {
                                deferred_d = Some(map.next_value::<serde_json::Value>()?);
                            }
                        },
                    }
                }

                let op = op.ok_or_else(|| de::Error::missing_field("op"))?;
                let resolved_payload = if let Some(p) = payload {
                    p
                } else if let Some(raw_d) = deferred_d {
                    decode_deferred_payload(op, t.as_deref(), raw_d)
                } else if WsOpcode::from_u8(op) == Some(WsOpcode::InvalidSession) {
                    IncomingWsPayload::InvalidSession(false)
                } else {
                    IncomingWsPayload::None
                };

                Ok(IncomingWsFrame {
                    op,
                    s,
                    t,
                    payload: resolved_payload,
                })
            }
        }

        deserializer.deserialize_map(FrameVisitor)
    }
}

#[derive(Clone, Copy)]
struct GatewayContext<'a> {
    client: &'a Arc<QqClient>,
    config: &'a Arc<Config>,
    session_mgr: &'a Arc<SessionManager>,
    runtime: &'a QqRuntime,
    dispatch_semaphore: &'a Arc<Semaphore>,
    cancel: &'a CancellationToken,
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
    let mut prev_connect_failed = false;
    let mut consecutive_non_resumable_op9: u32 = 0;
    let mut resume_state: Option<ResumeState> = None;
    let mut dedup = EventDeduplicator::new(MAX_DEDUP_ENTRIES);
    let mut join_set: JoinSet<()> = JoinSet::new();
    let dispatch_semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_DISPATCHES));
    let runtime = QqRuntime::new(cancel.clone());
    let ctx = GatewayContext {
        client: &client,
        config: &config,
        session_mgr: &session_mgr,
        runtime: &runtime,
        dispatch_semaphore: &dispatch_semaphore,
        cancel: &cancel,
    };

    info!("Starting QQ Bot WebSocket Gateway supervisor...");

    loop {
        if cancel.is_cancelled() {
            break;
        }

        let session_res =
            run_connection_session(ctx, &mut resume_state, &mut dedup, &mut join_set).await;

        let exit = match session_res {
            Ok(exit) => exit,
            Err(err) => {
                error!(error = %format!("{err:#}"), "QQ Gateway session setup failed");
                SessionExit {
                    reason: DisconnectReason::TransportError(err.to_string()),
                    ready_duration: None,
                    ws_connected: false,
                    attempted_resume: false,
                }
            }
        };

        if exit.reason == DisconnectReason::Cancelled || cancel.is_cancelled() {
            info!("QQ Gateway shutdown requested via cancellation token");
            break;
        }

        let (min_wait, next_delay) = compute_reconnect_delay(
            retry_delay,
            &exit,
            prev_connect_failed,
            consecutive_non_resumable_op9,
        );
        let wait_duration = apply_jitter(min_wait, jitter_entropy());

        if exit.reason == DisconnectReason::ReconnectRequested && resume_state.is_some() {
            info!(
                reason = ?exit.reason,
                ready_secs = exit.ready_duration.map(|d| d.as_secs()),
                wait_ms = wait_duration.as_millis(),
                can_resume = true,
                "QQ Gateway requested routine reconnect; scheduling resume"
            );
        } else {
            warn!(
                reason = ?exit.reason,
                ready_secs = exit.ready_duration.map(|d| d.as_secs()),
                wait_ms = wait_duration.as_millis(),
                can_resume = resume_state.is_some(),
                "QQ Gateway disconnected; scheduling reconnect"
            );
        }

        prev_connect_failed = !exit.ws_connected;
        if exit.ready_duration.is_some() {
            consecutive_non_resumable_op9 = 0;
        } else if matches!(
            exit.reason,
            DisconnectReason::InvalidSession { resumable: false }
        ) && !exit.attempted_resume
        {
            consecutive_non_resumable_op9 = consecutive_non_resumable_op9.saturating_add(1);
        } else if exit.ws_connected {
            consecutive_non_resumable_op9 = 0;
        }

        tokio::select! {
            _ = sleep(wait_duration) => {}
            _ = cancel.cancelled() => {
                info!("QQ Gateway reconnect backoff cancelled");
                break;
            }
        }

        retry_delay = next_delay;
    }

    // Cooperatively cancel active & detached QQ tasks and send SIGINT to in-flight agy processes
    // before waiting for graceful exit within the shutdown drain budget.
    runtime.begin_shutdown();
    session_mgr.shutdown_all_processes().await;
    tokio::join!(
        drain_dispatch_tasks(&mut join_set, SHUTDOWN_DRAIN_TIMEOUT),
        runtime.drain_detached(SHUTDOWN_DRAIN_TIMEOUT),
    );
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
    ctx: GatewayContext<'_>,
    resume_state: &mut Option<ResumeState>,
    dedup: &mut EventDeduplicator,
    join_set: &mut JoinSet<()>,
) -> Result<SessionExit> {
    let GatewayContext {
        client,
        config,
        session_mgr,
        runtime,
        dispatch_semaphore,
        cancel,
    } = ctx;
    let gateway_url = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Ok(SessionExit {
                reason: DisconnectReason::Cancelled,
                ready_duration: None,
                ws_connected: false,
                attempted_resume: false,
            });
        }
        res = client.get_gateway_url() => {
            res.context("Failed to get gateway URL from QQ Open Platform")?
        }
    };

    info!(gateway_url = %gateway_url, "Connecting to QQ WebSocket Gateway");

    let (ws_stream, _) = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            return Ok(SessionExit {
                reason: DisconnectReason::Cancelled,
                ready_duration: None,
                ws_connected: false,
                attempted_resume: false,
            });
        }
        res = timeout(CONNECT_TIMEOUT, connect_async(&gateway_url)) => {
            res
                .context("Timed out connecting to QQ WebSocket Gateway")?
                .context("Failed to establish WebSocket handshake with QQ Gateway")?
        }
    };

    let (mut ws_write, mut ws_read) = ws_stream.split();

    // Wait for Op 10 Hello frame within HELLO_TIMEOUT (single-pass JSON decode).
    let hello_deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    let hello_payload: WsHelloData = loop {
        let next_frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                close_ws_gracefully(&mut ws_write).await;
                return Ok(SessionExit {
                    reason: DisconnectReason::Cancelled,
                    ready_duration: None,
                    ws_connected: true,
                    attempted_resume: false,
                });
            }
            res = timeout_at(hello_deadline, ws_read.next()) => {
                res.context("Timed out waiting for Op 10 Hello from QQ Gateway")?
            }
        };

        match next_frame {
            Some(Ok(Message::Text(text))) => {
                let frame: IncomingWsFrame =
                    serde_json::from_str(&text).context("Failed to parse Hello frame")?;
                if WsOpcode::from_u8(frame.op) != Some(WsOpcode::Hello) {
                    anyhow::bail!("First frame opcode was {}, expected Op 10 Hello", frame.op);
                }
                match frame.payload {
                    IncomingWsPayload::Hello(hello) => break hello,
                    IncomingWsPayload::DecodeError { error, .. } => {
                        anyhow::bail!("Failed to parse Hello payload: {error}");
                    }
                    _ => anyhow::bail!("Missing Op 10 Hello payload `d`"),
                }
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
    let ack_timeout = heartbeat_ack_timeout(heartbeat_interval);
    debug!(
        raw_interval_ms = ?hello_payload.heartbeat_interval,
        clamped_interval_ms = heartbeat_interval.as_millis(),
        ack_timeout_secs = ack_timeout.as_secs(),
        "Received Op 10 Hello from QQ Gateway"
    );

    let auth_header = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            close_ws_gracefully(&mut ws_write).await;
            return Ok(SessionExit {
                reason: DisconnectReason::Cancelled,
                ready_duration: None,
                ws_connected: true,
                attempted_resume: false,
            });
        }
        res = client.get_auth_header() => res?,
    };

    // Consume any pending resume_state upfront so a failed Resume automatically falls back to
    // fresh Identify on the next attempt.
    let mut session_id: Option<String> = None;
    let mut last_s: Option<u64> = None;
    let attempted_resume;

    if let Some(resume) = resume_state.take() {
        attempted_resume = true;
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
        last_s = Some(resume.last_seq);
        info!(seq = resume.last_seq, "Op 6 Resume sent to QQ Gateway");
    } else {
        attempted_resume = false;
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

    let mut ack_deadline: Option<tokio::time::Instant> = None;
    let mut ready_at: Option<Instant> = None;
    let mut session_established = false;

    let reason = loop {
        tokio::select! {
            biased;

            _ = cancel.cancelled() => {
                close_ws_gracefully(&mut ws_write).await;
                break DisconnectReason::Cancelled;
            }

            _ = async {
                match ack_deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => pending().await,
                }
            }, if ack_deadline.is_some() => {
                warn!(
                    ?last_s,
                    timeout_secs = ack_timeout.as_secs(),
                    "QQ Gateway missed Op 11 Heartbeat ACK within deadline; reconnecting"
                );
                break DisconnectReason::HeartbeatTimeout;
            }

            _ = heartbeat_timer.tick() => {
                if ack_deadline.is_some() {
                    warn!(?last_s, "QQ Gateway missed Op 11 Heartbeat ACK; reconnecting");
                    break DisconnectReason::HeartbeatTimeout;
                }
                debug!(s = ?last_s, "Sending Op 1 Heartbeat to QQ Gateway");
                if let Err(err) = send_heartbeat(&mut ws_write, last_s).await {
                    warn!(error = %err, "Failed to send Op 1 Heartbeat to QQ WebSocket");
                    break DisconnectReason::TransportError(err.to_string());
                }
                ack_deadline = Some(tokio::time::Instant::now() + ack_timeout);
            }

            Some(join_res) = join_set.join_next(), if !join_set.is_empty() => {
                if let Err(err) = join_res {
                    error!(error = %err, "QQ dispatch handler task panicked or was cancelled");
                }
            }

            incoming = ws_read.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let frame: IncomingWsFrame = match serde_json::from_str(&text) {
                            Ok(f) => f,
                            Err(err) => {
                                warn!(
                                    error = %err,
                                    payload_len = text.len(),
                                    "Malformed QQ Gateway JSON frame"
                                );
                                continue;
                            }
                        };

                        if let Some(s) = frame.s {
                            last_s = Some(s);
                        }

                        match WsOpcode::from_u8(frame.op) {
                            Some(WsOpcode::Dispatch) => {
                                let event_type = frame.t.as_deref().unwrap_or("");
                                debug!(event_type, "Received Op 0 Dispatch event");

                                match frame.payload {
                                    IncomingWsPayload::Ready(ready) => {
                                        if ready.session_id.is_some() {
                                            session_id = ready.session_id;
                                        }
                                        ready_at = Some(Instant::now());
                                        session_established = true;
                                        info!("🎉 QQ Bot connection successfully authenticated! Gateway READY.");
                                    }
                                    IncomingWsPayload::C2cMessage(event_box) => {
                                        let event = *event_box;
                                        if !dedup.check_and_insert("c2c", &event.id) {
                                            debug!(msg_id = %event.id, "Skipping duplicate QQ C2C dispatch event");
                                            continue;
                                        }
                                        let client_c = client.clone();
                                        let config_c = config.clone();
                                        let session_mgr_c = session_mgr.clone();
                                        let runtime_c = runtime.clone();
                                        spawn_bounded_dispatch(
                                            join_set,
                                            dispatch_semaphore,
                                            "C2C_MESSAGE_CREATE",
                                            async move {
                                                handle_c2c(
                                                    client_c,
                                                    config_c,
                                                    session_mgr_c,
                                                    runtime_c,
                                                    event,
                                                )
                                                .await;
                                            },
                                        );
                                    }
                                    IncomingWsPayload::GroupAtMessage(event_box) => {
                                        let event = *event_box;
                                        if !dedup.check_and_insert("grp", &event.id) {
                                            debug!(msg_id = %event.id, "Skipping duplicate QQ Group dispatch event");
                                            continue;
                                        }
                                        let client_c = client.clone();
                                        let config_c = config.clone();
                                        let session_mgr_c = session_mgr.clone();
                                        let runtime_c = runtime.clone();
                                        spawn_bounded_dispatch(
                                            join_set,
                                            dispatch_semaphore,
                                            "GROUP_AT_MESSAGE_CREATE",
                                            async move {
                                                handle_group(
                                                    client_c,
                                                    config_c,
                                                    session_mgr_c,
                                                    runtime_c,
                                                    event,
                                                )
                                                .await;
                                            },
                                        );
                                    }
                                    IncomingWsPayload::GroupPassiveMessage(event_box) => {
                                        let event = *event_box;
                                        if !dedup.check_and_insert("grp_all", &event.id) {
                                            continue;
                                        }
                                        handle_group_passive(config, event);
                                    }
                                    IncomingWsPayload::DecodeError { event_type: ev, error } => {
                                        if ev == "GROUP_MESSAGE_CREATE" {
                                            debug!(
                                                event_type = ev,
                                                error = %error,
                                                "Failed to decode passive QQ Group dispatch event"
                                            );
                                        } else {
                                            warn!(
                                                event_type = ev,
                                                error = %error,
                                                "Failed to decode QQ dispatch event"
                                            );
                                        }
                                        if ev == "READY" {
                                            ready_at = Some(Instant::now());
                                            session_established = true;
                                            info!("🎉 QQ Bot connection successfully authenticated! Gateway READY.");
                                        }
                                    }
                                    IncomingWsPayload::None => match event_type {
                                        "READY" => {
                                            ready_at = Some(Instant::now());
                                            session_established = true;
                                            info!("🎉 QQ Bot connection successfully authenticated! Gateway READY.");
                                        }
                                        "RESUMED" => {
                                            ready_at.get_or_insert_with(Instant::now);
                                            session_established = true;
                                            info!("QQ Bot session resumed successfully");
                                        }
                                        other => {
                                            debug!(event_type = other, "Unhandled QQ Dispatch event");
                                        }
                                    },
                                    IncomingWsPayload::Hello(_)
                                    | IncomingWsPayload::InvalidSession(_) => {
                                        debug!(event_type, "Ignoring mismatched dispatch payload");
                                    }
                                }
                            }
                            Some(WsOpcode::Heartbeat) => {
                                // Server-initiated heartbeat request; respond immediately without
                                // altering periodic `ack_deadline`.
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
                                let resumable = match frame.payload {
                                    IncomingWsPayload::InvalidSession(r) => r,
                                    _ => false,
                                };
                                warn!(resumable, "QQ Gateway returned Op 9 Invalid Session");
                                if !resumable {
                                    client.invalidate_auth_header(&auth_header);
                                }
                                session_id = None;
                                session_established = false;
                                break DisconnectReason::InvalidSession { resumable };
                            }
                            Some(WsOpcode::HeartbeatAck) => {
                                ack_deadline = None;
                                debug!("Received Op 11 Heartbeat ACK from QQ Gateway.");
                            }
                            Some(WsOpcode::Hello | WsOpcode::Identify | WsOpcode::Resume)
                            | None => {
                                debug!(op = frame.op, "Ignoring unexpected opcode from QQ Gateway");
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
                        if code == Some(4004) {
                            client.invalidate_auth_header(&auth_header);
                        }
                        if code.is_some_and(is_non_resumable_close_code) {
                            session_id = None;
                            session_established = false;
                        }
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
        && let Some(sid) = session_id
    {
        *resume_state = Some(ResumeState {
            session_id: sid,
            last_seq: last_s.unwrap_or(0),
        });
    }

    Ok(SessionExit {
        reason,
        ready_duration: ready_at.map(|started| started.elapsed()),
        ws_connected: true,
        attempted_resume,
    })
}

#[inline]
#[must_use]
const fn is_non_resumable_close_code(code: u16) -> bool {
    matches!(code, 4004 | 4006 | 4007 | 4009..=4014)
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

async fn send_heartbeat(ws_write: &mut WsSink, last_s: Option<u64>) -> Result<()> {
    let hb = WsOutgoingEnvelope {
        op: WsOpcode::Heartbeat.as_u8(),
        d: last_s,
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

/// Bounds `Op 11 HeartbeatAck` wait to at most 15s (or `heartbeat_interval` if shorter).
#[inline]
#[must_use]
fn heartbeat_ack_timeout(heartbeat_interval: Duration) -> Duration {
    HEARTBEAT_ACK_TIMEOUT.min(heartbeat_interval)
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

/// Computes `(min_wait_before_jitter, next_retry_delay)` for a gateway exit, resetting stale
/// outage backoff cleanly upon WS connection recovery or non-resumable `Op 9 Invalid Session`
/// (e.g. expired pre-outage `Op 6 Resume` -> fresh `Op 2 Identify`) while still backing off if
/// fresh `Identify` repeatedly fails with `Op 9`.
#[inline]
#[must_use]
fn compute_reconnect_delay(
    current: Duration,
    exit: &SessionExit,
    prev_connect_failed: bool,
    consecutive_non_resumable_op9: u32,
) -> (Duration, Duration) {
    let reset_for_op9 = matches!(
        exit.reason,
        DisconnectReason::InvalidSession { resumable: false }
    ) && (exit.attempted_resume || consecutive_non_resumable_op9 == 0);
    let reset_for_connect_recovery = exit.ws_connected && prev_connect_failed;
    let effective_current = if reset_for_op9 || reset_for_connect_recovery {
        INITIAL_RETRY_DELAY
    } else {
        current
    };
    let (base_wait, next_delay) = next_retry_delay(effective_current, exit.ready_duration);
    let min_wait = if matches!(exit.reason, DisconnectReason::InvalidSession { .. }) {
        base_wait.max(Duration::from_secs(2))
    } else {
        base_wait
    };
    (min_wait, next_delay)
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
    use super::super::types::{WsDataEnvelope, WsEnvelopeHeader, WsOptionalBoolEnvelope};
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

        sleep(Duration::from_millis(200)).await;
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

    #[test]
    fn event_deduplicator_filters_duplicates_and_evicts_bounded_fifo() {
        let mut dedup = EventDeduplicator::new(2);
        assert!(dedup.check_and_insert("c2c", "m1"));
        assert!(!dedup.check_and_insert("c2c", "m1"));
        assert!(dedup.check_and_insert("grp", "m1"));
        assert!(dedup.check_and_insert("c2c", "m2"));
        // Capacity is 2, so c2c:m1 was evicted while grp:m1 and c2c:m2 remain.
        assert!(!dedup.check_and_insert("grp", "m1"));
        assert!(!dedup.check_and_insert("c2c", "m2"));
        assert!(dedup.check_and_insert("c2c", "m1"));
        // Empty event ids are never suppressed.
        assert!(dedup.check_and_insert("c2c", "  "));
        assert!(dedup.check_and_insert("c2c", ""));
    }

    #[test]
    fn non_resumable_close_codes_force_fresh_identify() {
        for code in [4004, 4006, 4007, 4009, 4010, 4014] {
            assert!(is_non_resumable_close_code(code));
        }
        for code in [1000, 1001, 4000, 4008] {
            assert!(!is_non_resumable_close_code(code));
        }
    }

    #[test]
    fn heartbeat_ack_timeout_caps_at_15s_and_respects_short_interval() {
        assert_eq!(
            heartbeat_ack_timeout(Duration::from_secs(45)),
            Duration::from_secs(15)
        );
        assert_eq!(
            heartbeat_ack_timeout(Duration::from_secs(120)),
            Duration::from_secs(15)
        );
        assert_eq!(
            heartbeat_ack_timeout(Duration::from_secs(5)),
            Duration::from_secs(5)
        );
    }

    #[test]
    fn reconnect_delay_resets_on_connect_recovery_and_non_resumable_op9_without_storming() {
        // Outage pushed backoff to 30s; WS connects, sends Op 6 Resume, gets Op 9 (resumable=false).
        // Must reset immediately to ~2s instead of stalling 30s!
        let resume_op9 = SessionExit {
            reason: DisconnectReason::InvalidSession { resumable: false },
            ready_duration: None,
            ws_connected: true,
            attempted_resume: true,
        };
        let (wait, next) = compute_reconnect_delay(MAX_RETRY_DELAY, &resume_op9, true, 0);
        assert_eq!(wait, Duration::from_secs(2));
        assert_eq!(next, Duration::from_secs(2));

        // First fresh-Identify Op 9 (e.g. stale token) also resets from 30s -> 2s.
        let first_id_op9 = SessionExit {
            reason: DisconnectReason::InvalidSession { resumable: false },
            ready_duration: None,
            ws_connected: true,
            attempted_resume: false,
        };
        let (wait2, next2) = compute_reconnect_delay(MAX_RETRY_DELAY, &first_id_op9, false, 0);
        assert_eq!(wait2, Duration::from_secs(2));
        assert_eq!(next2, Duration::from_secs(2));

        // Repeated fresh-Identify Op 9 without reaching READY must back off exponentially (no storm).
        let (wait3, next3) =
            compute_reconnect_delay(Duration::from_secs(8), &first_id_op9, false, 1);
        assert_eq!(wait3, Duration::from_secs(8));
        assert_eq!(next3, Duration::from_secs(16));

        // Successful WS connect after connect failures resets stale 30s outage backoff to 1s.
        let recovered_eof = SessionExit {
            reason: DisconnectReason::Eof,
            ready_duration: None,
            ws_connected: true,
            attempted_resume: false,
        };
        let (wait4, next4) = compute_reconnect_delay(MAX_RETRY_DELAY, &recovered_eof, true, 0);
        assert_eq!(wait4, Duration::from_secs(1));
        assert_eq!(next4, Duration::from_secs(2));
    }

    #[test]
    fn single_pass_ws_frame_parser_handles_in_order_and_out_of_order_frames() {
        let hello: IncomingWsFrame =
            serde_json::from_str(r#"{"op":10,"s":null,"t":null,"d":{"heartbeat_interval":45000}}"#)
                .unwrap();
        assert_eq!(WsOpcode::from_u8(hello.op), Some(WsOpcode::Hello));
        assert!(matches!(
            hello.payload,
            IncomingWsPayload::Hello(WsHelloData {
                heartbeat_interval: Some(45_000)
            })
        ));

        let ready: IncomingWsFrame = serde_json::from_str(
            r#"{"op":0,"s":7,"t":"READY","d":{"version":1,"session_id":"s1"}}"#,
        )
        .unwrap();
        assert_eq!(ready.s, Some(7));
        assert!(matches!(
            ready.payload,
            IncomingWsPayload::Ready(WsReadyData {
                session_id: Some(ref sid)
            }) if sid == "s1"
        ));

        let resumed: IncomingWsFrame =
            serde_json::from_str(r#"{"op":0,"s":8,"t":"RESUMED","d":""}"#).unwrap();
        assert_eq!(resumed.s, Some(8));
        assert_eq!(resumed.t.as_deref(), Some("RESUMED"));
        assert!(matches!(resumed.payload, IncomingWsPayload::None));

        // Out-of-order JSON where `d` appears before `op` and `t`.
        let out_of_order: IncomingWsFrame = serde_json::from_str(
            r#"{"d":{"id":"m1","content":"hi","author":{"user_openid":"u1"}},"op":0,"s":42,"t":"C2C_MESSAGE_CREATE"}"#,
        )
        .unwrap();
        assert_eq!(out_of_order.s, Some(42));
        assert!(matches!(
            out_of_order.payload,
            IncomingWsPayload::C2cMessage(ref ev) if ev.id == "m1" && ev.author.user_openid == "u1"
        ));

        let op9_false: IncomingWsFrame = serde_json::from_str(r#"{"op":9,"d":false}"#).unwrap();
        assert!(matches!(
            op9_false.payload,
            IncomingWsPayload::InvalidSession(false)
        ));

        let op9_true: IncomingWsFrame = serde_json::from_str(r#"{"op":9,"d":true}"#).unwrap();
        assert!(matches!(
            op9_true.payload,
            IncomingWsPayload::InvalidSession(true)
        ));

        let op9_missing: IncomingWsFrame = serde_json::from_str(r#"{"op":9}"#).unwrap();
        assert!(matches!(
            op9_missing.payload,
            IncomingWsPayload::InvalidSession(false)
        ));

        let ack: IncomingWsFrame = serde_json::from_str(r#"{"op":11,"s":null,"t":null}"#).unwrap();
        assert_eq!(WsOpcode::from_u8(ack.op), Some(WsOpcode::HeartbeatAck));
        assert!(matches!(ack.payload, IncomingWsPayload::None));
    }
}
