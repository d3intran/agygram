use super::types::{
    GatewayResponse, MarkdownPayload, MessageResponse, SendMessageRequest, StreamMessageRequest,
    TokenResponse,
};
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

const DEFAULT_TOKEN_BASE: &str = "https://bots.qq.com";
const DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";

#[derive(Clone)]
pub struct QqClient {
    http: reqwest::Client,
    pub app_id: String,
    pub app_secret: String,
    token_base_url: String,
    api_base_url: String,
    token_cache: Arc<RwLock<Option<(String, Instant)>>>,
}

impl QqClient {
    pub fn new(app_id: String, app_secret: String, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60));

        if let Some(proxy) = proxy_url {
            if !proxy.trim().is_empty() {
                builder = builder.proxy(reqwest::Proxy::all(proxy)?);
            }
        }

        let http = builder.build().context("Failed to build HTTP client for QQ API")?;

        let token_base_url = std::env::var("QQBOT_TOKEN_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_TOKEN_BASE.to_string())
            .trim_end_matches('/')
            .to_string();

        let api_base_url = std::env::var("QQBOT_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
            .trim_end_matches('/')
            .to_string();

        Ok(Self {
            http,
            app_id,
            app_secret,
            token_base_url,
            api_base_url,
            token_cache: Arc::new(RwLock::new(None)),
        })
    }

    /// Fetches a valid access_token, caching it in-memory with automatic expiration tracking.
    pub async fn get_access_token(&self) -> Result<String> {
        {
            let cache = self.token_cache.read().await;
            if let Some((token, expires_at)) = cache.as_ref() {
                if Instant::now() < *expires_at {
                    return Ok(token.clone());
                }
            }
        }

        let mut cache = self.token_cache.write().await;
        // Double-check write lock
        if let Some((token, expires_at)) = cache.as_ref() {
            if Instant::now() < *expires_at {
                return Ok(token.clone());
            }
        }

        let url = format!("{}/app/getAppAccessToken", self.token_base_url);
        debug!("Requesting QQ Bot AccessToken from {}", url);

        let body = serde_json::json!({
            "appId": self.app_id,
            "clientSecret": self.app_secret
        });

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send getAppAccessToken request")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!(
                "Failed to get QQ Bot access_token: HTTP {} - {}",
                status,
                text
            );
        }

        let parsed: TokenResponse = serde_json::from_str(&text).with_context(|| {
            format!("Failed to parse getAppAccessToken response JSON: {text}")
        })?;

        // Expire 5 minutes early to avoid edge-of-expiry 401s
        let safe_margin = Duration::from_secs(300);
        let valid_duration = Duration::from_secs(parsed.expires_in).saturating_sub(safe_margin);
        let expires_at = Instant::now() + valid_duration;

        *cache = Some((parsed.access_token.clone(), expires_at));
        debug!("QQ Bot AccessToken acquired successfully (expires in {}s)", parsed.expires_in);

        Ok(parsed.access_token)
    }

    /// Queries the QQ Open Platform for the WSS Gateway URL.
    pub async fn get_gateway_url(&self) -> Result<String> {
        let token = self.get_access_token().await?;
        let url = format!("{}/gateway/bot", self.api_base_url);

        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .context("Failed to request QQ WebSocket gateway URL")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!("Failed to get gateway URL: HTTP {} - {}", status, text);
        }

        let parsed: GatewayResponse = serde_json::from_str(&text).with_context(|| {
            format!("Failed to parse gateway response JSON: {text}")
        })?;

        Ok(parsed.url)
    }

    /// Sends a standard C2C (single user) message.
    /// Attempts QQ Native Markdown (msg_type: 2) first, falling back to clean plain text (msg_type: 0).
    pub async fn send_c2c_message(
        &self,
        openid: &str,
        content: &str,
        msg_id: &str,
    ) -> Result<Option<String>> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v2/users/{openid}/messages", self.api_base_url);
        let seq = get_next_msg_seq(msg_id);

        // First attempt sending as msg_type: 2 (Markdown)
        let req_md = SendMessageRequest {
            content: "",
            msg_type: 2,
            msg_id,
            msg_seq: Some(seq),
            markdown: Some(MarkdownPayload { content }),
        };

        let resp_md = self
            .http
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&req_md)
            .send()
            .await;

        if let Ok(resp) = resp_md {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let parsed: Option<MessageResponse> = serde_json::from_str(&text).ok();
                return Ok(parsed.and_then(|r| r.id));
            }
            debug!(
                "QQ send_c2c_message (markdown) returned HTTP {}: {}, falling back to plain text",
                status, text
            );
        }

        // Fallback to msg_type: 0 (plain text)
        let plain_content = crate::qq::formatter::QqFormatter::to_plain_text(content);
        let req_plain = SendMessageRequest {
            content: &plain_content,
            msg_type: 0,
            msg_id,
            msg_seq: Some(seq),
            markdown: None,
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&req_plain)
            .send()
            .await
            .context("Failed to send C2C message")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            warn!("QQ send_c2c_message failed: HTTP {} - {}", status, text);
            anyhow::bail!("QQ send_c2c_message failed: HTTP {} - {}", status, text);
        }

        let parsed: Option<MessageResponse> = serde_json::from_str(&text).ok();
        Ok(parsed.and_then(|r| r.id))
    }

    /// Sends a C2C stream message frame (`/v2/users/{openid}/stream_messages`).
    pub async fn send_c2c_stream(
        &self,
        openid: &str,
        req: &StreamMessageRequest<'_>,
    ) -> Result<Option<String>> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v2/users/{openid}/stream_messages", self.api_base_url);

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(req)
            .send()
            .await
            .context("Failed to send C2C stream message")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            debug!("QQ stream_messages returned non-success: HTTP {} - {}", status, text);
            anyhow::bail!("QQ stream_messages returned non-success: HTTP {} - {}", status, text);
        }

        let parsed: Option<MessageResponse> = serde_json::from_str(&text).ok();
        Ok(parsed.and_then(|r| r.id))
    }

    /// Sends a Group (@Bot) message.
    /// Attempts QQ Native Markdown (msg_type: 2) first, falling back to clean plain text (msg_type: 0).
    pub async fn send_group_message(
        &self,
        group_openid: &str,
        content: &str,
        msg_id: &str,
    ) -> Result<Option<String>> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v2/groups/{group_openid}/messages", self.api_base_url);
        let seq = get_next_msg_seq(msg_id);

        // First attempt sending as msg_type: 2 (Markdown)
        let req_md = SendMessageRequest {
            content: "",
            msg_type: 2,
            msg_id,
            msg_seq: Some(seq),
            markdown: Some(MarkdownPayload { content }),
        };

        let resp_md = self
            .http
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&req_md)
            .send()
            .await;

        if let Ok(resp) = resp_md {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let parsed: Option<MessageResponse> = serde_json::from_str(&text).ok();
                return Ok(parsed.and_then(|r| r.id));
            }
            debug!(
                "QQ send_group_message (markdown) returned HTTP {}: {}, falling back to plain text",
                status, text
            );
        }

        // Fallback to msg_type: 0 (plain text)
        let plain_content = crate::qq::formatter::QqFormatter::to_plain_text(content);
        let req_plain = SendMessageRequest {
            content: &plain_content,
            msg_type: 0,
            msg_id,
            msg_seq: Some(seq),
            markdown: None,
        };

        let resp = self
            .http
            .post(&url)
            .header("Authorization", format!("QQBot {token}"))
            .json(&req_plain)
            .send()
            .await
            .context("Failed to send Group message")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            warn!("QQ send_group_message failed: HTTP {} - {}", status, text);
            anyhow::bail!("QQ send_group_message failed: HTTP {} - {}", status, text);
        }

        let parsed: Option<MessageResponse> = serde_json::from_str(&text).ok();
        Ok(parsed.and_then(|r| r.id))
    }

    /// Recalls/deletes an existing Group message.
    pub async fn delete_group_message(&self, group_openid: &str, message_id: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v2/groups/{group_openid}/messages/{message_id}", self.api_base_url);
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .context("Failed to recall Group message")?;

        if !resp.status().is_success() {
            debug!("QQ delete_group_message returned non-success: HTTP {}", resp.status());
        }
        Ok(())
    }

    /// Recalls/deletes an existing C2C message.
    pub async fn delete_c2c_message(&self, openid: &str, message_id: &str) -> Result<()> {
        let token = self.get_access_token().await?;
        let url = format!("{}/v2/users/{openid}/messages/{message_id}", self.api_base_url);
        let resp = self
            .http
            .delete(&url)
            .header("Authorization", format!("QQBot {token}"))
            .send()
            .await
            .context("Failed to recall C2C message")?;

        if !resp.status().is_success() {
            debug!("QQ delete_c2c_message returned non-success: HTTP {}", resp.status());
        }
        Ok(())
    }
}

/// Generates a msg_seq number in 0..65535 according to Tencent standard.
pub fn get_next_msg_seq(seed: &str) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    seed.hash(&mut hasher);
    let time_part = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        % 100_000_000) as u32;
    (time_part ^ (hasher.finish() as u32)) % 65536
}
