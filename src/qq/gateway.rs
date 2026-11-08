use super::client::QqClient;
use super::handler::{handle_c2c, handle_group};
use super::types::{C2CMessageEvent, GroupMessageEvent, WsIdentifyData, WsPayload};
use crate::config::Config;
use crate::session::SessionManager;
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Gateway intent: GROUP_AND_C2C (1 << 25) | INTERACTION (1 << 26)
const GROUP_AND_C2C_INTENT: u32 = (1 << 25) | (1 << 26);

pub async fn run_gateway(
    client: Arc<QqClient>,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> Result<()> {
    let mut retry_delay = Duration::from_secs(1);
    const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

    info!("Starting QQ Bot WebSocket Gateway supervisor...");

    loop {
        match run_connection_session(&client, &config, &session_mgr).await {
            Ok(()) => {
                info!("QQ Gateway connection closed gracefully, reconnecting...");
                retry_delay = Duration::from_secs(1);
            }
            Err(e) => {
                error!("QQ Gateway session encountered error: {:#}", e);
                info!("Reconnecting QQ Gateway in {}s...", retry_delay.as_secs());
                sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
            }
        }
    }
}

async fn run_connection_session(
    client: &Arc<QqClient>,
    config: &Arc<Config>,
    session_mgr: &Arc<SessionManager>,
) -> Result<()> {
    let gateway_url = client
        .get_gateway_url()
        .await
        .context("Failed to get gateway URL from QQ Open Platform")?;

    info!("Connecting to QQ WebSocket Gateway: {}", gateway_url);

    let (ws_stream, _) = connect_async(&gateway_url)
        .await
        .context("Failed to establish WebSocket handshake with QQ Gateway")?;

    let (mut ws_write, mut ws_read) = ws_stream.split();
    let last_s = Arc::new(AtomicU64::new(0));

    // Wait for Op 10 Hello frame
    let hello_payload = match ws_read.next().await {
        Some(Ok(Message::Text(text))) => {
            serde_json::from_str::<WsPayload>(&text).context("Failed to parse Hello payload")?
        }
        other => {
            anyhow::bail!("Expected Hello frame on initial connect, received: {:?}", other);
        }
    };

    if hello_payload.op != 10 {
        anyhow::bail!("First frame opcode was {}, expected Op 10 Hello", hello_payload.op);
    }

    let heartbeat_interval_ms = hello_payload
        .d
        .and_then(|d| d.get("heartbeat_interval").and_then(|v| v.as_u64()))
        .unwrap_or(45000);

    debug!(
        "Received Op 10 Hello from QQ Gateway. Heartbeat interval: {}ms",
        heartbeat_interval_ms
    );

    // Send Op 2 Identify
    let access_token = client.get_access_token().await?;
    let identify_body = serde_json::json!({
        "op": 2,
        "d": WsIdentifyData {
            token: &format!("QQBot {}", access_token),
            intents: GROUP_AND_C2C_INTENT,
            shard: [0, 1],
        }
    });

    ws_write
        .send(Message::Text(identify_body.to_string()))
        .await
        .context("Failed to send Op 2 Identify payload")?;

    info!("Op 2 Identify sent to QQ Gateway. Listening for events...");

    // Channel for outgoing heartbeat messages
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<Message>(32);

    // Spawn Heartbeat Loop
    let last_s_heartbeat = last_s.clone();
    let outgoing_tx_heartbeat = outgoing_tx.clone();
    let heartbeat_handle = tokio::spawn(async move {
        let interval = Duration::from_millis(heartbeat_interval_ms);
        loop {
            sleep(interval).await;
            let s_val = last_s_heartbeat.load(Ordering::Relaxed);
            let hb = serde_json::json!({
                "op": 1,
                "d": if s_val > 0 { serde_json::Value::from(s_val) } else { serde_json::Value::Null }
            });
            debug!("Sending Op 1 Heartbeat to QQ Gateway (s={})", s_val);
            if outgoing_tx_heartbeat.send(Message::Text(hb.to_string())).await.is_err() {
                break;
            }
        }
    });

    // Loop managing writes and reads
    loop {
        tokio::select! {
            // Forward outgoing messages (heartbeats, replies, etc.)
            Some(msg) = outgoing_rx.recv() => {
                if let Err(e) = ws_write.send(msg).await {
                    warn!("Failed to write to QQ WebSocket: {}", e);
                    break;
                }
            }

            // Ingest incoming gateway events
            incoming = ws_read.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(payload) = serde_json::from_str::<WsPayload>(&text) {
                            if let Some(s) = payload.s {
                                last_s.store(s, Ordering::Relaxed);
                            }

                            match payload.op {
                                0 => {
                                    // Op 0 Dispatch
                                    let event_type = payload.t.unwrap_or_default();
                                    debug!("Received Op 0 Dispatch event: {}", event_type);

                                    match event_type.as_str() {
                                        "READY" => {
                                            info!("🎉 QQ Bot connection successfully authenticated! Gateway READY.");
                                        }
                                        "C2C_MESSAGE_CREATE" => {
                                            if let Some(d) = payload.d {
                                                if let Ok(event) = serde_json::from_value::<C2CMessageEvent>(d) {
                                                    let client_c = client.clone();
                                                    let config_c = config.clone();
                                                    let session_mgr_c = session_mgr.clone();
                                                    tokio::spawn(async move {
                                                        handle_c2c(client_c, config_c, session_mgr_c, event).await;
                                                    });
                                                }
                                            }
                                        }
                                        "GROUP_AT_MESSAGE_CREATE" => {
                                            if let Some(d) = payload.d {
                                                if let Ok(event) = serde_json::from_value::<GroupMessageEvent>(d) {
                                                    let client_c = client.clone();
                                                    let config_c = config.clone();
                                                    let session_mgr_c = session_mgr.clone();
                                                    tokio::spawn(async move {
                                                        handle_group(client_c, config_c, session_mgr_c, event).await;
                                                    });
                                                }
                                            }
                                        }
                                        other => {
                                            debug!("Unhandled QQ Dispatch event: {}", other);
                                        }
                                    }
                                }
                                1 => {
                                    // Server requests heartbeat
                                    let s_val = last_s.load(Ordering::Relaxed);
                                    let hb = serde_json::json!({
                                        "op": 1,
                                        "d": if s_val > 0 { serde_json::Value::from(s_val) } else { serde_json::Value::Null }
                                    });
                                    let _ = outgoing_tx.send(Message::Text(hb.to_string())).await;
                                }
                                7 => {
                                    info!("QQ Gateway requested Op 7 Reconnect.");
                                    break;
                                }
                                9 => {
                                    warn!("QQ Gateway returned Op 9 Invalid Session. Will reconnect.");
                                    sleep(Duration::from_secs(3)).await;
                                    break;
                                }
                                11 => {
                                    debug!("Received Op 11 Heartbeat ACK from QQ Gateway.");
                                }
                                other => {
                                    debug!("Received unknown opcode {} from QQ Gateway", other);
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = outgoing_tx.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Close(c))) => {
                        warn!("QQ WebSocket connection closed by server: {:?}", c);
                        break;
                    }
                    Some(Err(e)) => {
                        error!("QQ WebSocket stream error: {}", e);
                        break;
                    }
                    None => {
                        info!("QQ WebSocket stream reached EOF.");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    heartbeat_handle.abort();
    Ok(())
}
