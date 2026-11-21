use super::formatter::QqFormatter;
use super::types::{
    GatewayResponse, MarkdownPayload, MediaPayload, MessageResponse, QqMediaFileType,
    QqMessageType, SendMessageRequest, TokenRequest, TokenResponse, UploadMediaRequest,
    UploadMediaResponse, parse_qq_api_error,
};
use anyhow::{Context, Result};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const DEFAULT_TOKEN_BASE: &str = "https://bots.qq.com";
const DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
/// Once QQ rejects `msg_type=2` (Markdown) due to lack of permission and plain-text fallback
/// succeeds, skip redundant Markdown round-trips for this window before re-probing.
const MARKDOWN_DISABLE_TTL: Duration = Duration::from_secs(3600);
const DOWNLOAD_MAX_RETRIES: usize = 2;
const DOWNLOAD_RETRY_BACKOFF: Duration = Duration::from_millis(400);

/// QQ's direct-upload ceiling for `file_data`. Base64 expands the body ~4/3, but the platform
/// applies the 5 MB cap to the decoded payload.
pub(crate) const MAX_DIRECT_MEDIA_BYTES: u64 = 5 * 1024 * 1024;

static MSG_SEQ_COUNTER: OnceLock<AtomicU64> = OnceLock::new();

/// Routing target for QQ Bot REST API calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QqTarget<'a> {
    C2c(&'a str),
    Group(&'a str),
}

impl<'a> QqTarget<'a> {
    #[inline]
    #[must_use]
    pub(crate) const fn kind_label(self) -> &'static str {
        match self {
            Self::C2c(_) => "C2C",
            Self::Group(_) => "Group",
        }
    }

    #[must_use]
    pub(crate) fn scope_key(self) -> String {
        match self {
            Self::C2c(openid) => format!("c2c:{}", openid.trim()),
            Self::Group(group_openid) => format!("grp:{}", group_openid.trim()),
        }
    }

    #[must_use]
    pub(crate) fn messages_url(self, api_base: &str) -> String {
        match self {
            Self::C2c(openid) => format!("{api_base}/v2/users/{openid}/messages"),
            Self::Group(group_openid) => format!("{api_base}/v2/groups/{group_openid}/messages"),
        }
    }

    #[must_use]
    pub(crate) fn files_url(self, api_base: &str) -> String {
        match self {
            Self::C2c(openid) => format!("{api_base}/v2/users/{openid}/files"),
            Self::Group(group_openid) => format!("{api_base}/v2/groups/{group_openid}/files"),
        }
    }

    #[must_use]
    pub(crate) fn delete_message_url(self, api_base: &str, message_id: &str) -> String {
        match self {
            Self::C2c(openid) => format!("{api_base}/v2/users/{openid}/messages/{message_id}"),
            Self::Group(group_openid) => {
                format!("{api_base}/v2/groups/{group_openid}/messages/{message_id}")
            }
        }
    }
}

#[derive(Clone)]
struct CachedToken {
    token: String,
    auth_header: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub(crate) struct QqClient {
    http: reqwest::Client,
    app_id: String,
    app_secret: String,
    token_base_url: String,
    api_base_url: String,
    token_cache: Arc<RwLock<Option<CachedToken>>>,
    token_refresh_lock: Arc<Mutex<()>>,
    markdown_disabled_until: Arc<RwLock<Option<Instant>>>,
}

impl fmt::Debug for QqClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QqClient")
            .field("app_id", &self.app_id)
            .field("app_secret", &"[REDACTED]")
            .field("token_base_url", &self.token_base_url)
            .field("api_base_url", &self.api_base_url)
            .field("token_cache", &"[REDACTED]")
            .finish()
    }
}

impl QqClient {
    pub(crate) fn new(app_id: String, app_secret: String, proxy_url: Option<&str>) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true);

        if let Some(proxy) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy)?);
        }

        let http = builder
            .build()
            .context("Failed to build HTTP client for QQ API")?;

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
            token_refresh_lock: Arc::new(Mutex::new(())),
            markdown_disabled_until: Arc::new(RwLock::new(None)),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        token_base_url: impl Into<String>,
        api_base_url: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .expect("test HTTP client must build");
        Self {
            http,
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token_base_url: token_base_url.into().trim_end_matches('/').to_string(),
            api_base_url: api_base_url.into().trim_end_matches('/').to_string(),
            token_cache: Arc::new(RwLock::new(None)),
            token_refresh_lock: Arc::new(Mutex::new(())),
            markdown_disabled_until: Arc::new(RwLock::new(None)),
        }
    }

    #[inline]
    fn is_markdown_disabled(&self) -> bool {
        let guard = match self.markdown_disabled_until.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.is_some_and(|until| Instant::now() < until)
    }

    fn mark_markdown_disabled(&self) {
        let mut guard = match self.markdown_disabled_until.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let was_active = guard.is_some_and(|until| Instant::now() < until);
        *guard = Some(Instant::now() + MARKDOWN_DISABLE_TTL);
        if !was_active {
            info!(
                cooldown_secs = MARKDOWN_DISABLE_TTL.as_secs(),
                "QQ Markdown (msg_type=2) unsupported; caching plain-text fast path"
            );
        }
    }

    #[inline]
    fn read_cached_token(&self) -> Option<CachedToken> {
        let guard = match self.token_cache.read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .filter(|cached| Instant::now() < cached.expires_at)
            .cloned()
    }

    fn store_cached_token(&self, cached: CachedToken) {
        let mut guard = match self.token_cache.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(cached);
    }

    pub(crate) fn invalidate_token(&self, stale_token: &str) {
        let mut guard = match self.token_cache.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.as_ref().is_some_and(|c| c.token == stale_token) {
            *guard = None;
        }
    }

    pub(crate) fn invalidate_auth_header(&self, stale_auth_header: &str) {
        let mut guard = match self.token_cache.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard
            .as_ref()
            .is_some_and(|c| c.auth_header == stale_auth_header)
        {
            *guard = None;
        }
    }

    /// Fetches a valid cached token + preformatted `"QQBot <token>"` header, refreshing via a
    /// single-flight mutex without holding any lock across network `.await`.
    async fn get_cached_token(&self) -> Result<CachedToken> {
        if let Some(cached) = self.read_cached_token() {
            return Ok(cached);
        }

        let _refresh_guard = self.token_refresh_lock.lock().await;
        if let Some(cached) = self.read_cached_token() {
            return Ok(cached);
        }

        let url = format!("{}/app/getAppAccessToken", self.token_base_url);
        debug!(url = %url, "Requesting QQ Bot AccessToken");

        let body = TokenRequest {
            app_id: &self.app_id,
            client_secret: &self.app_secret,
        };

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send getAppAccessToken request")?;

        let status = resp.status();
        let text = read_response_text(resp, "getAppAccessToken").await;

        if !status.is_success() {
            anyhow::bail!(
                "Failed to get QQ Bot access_token: HTTP {} - {}",
                status,
                text
            );
        }

        let parsed: TokenResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "Failed to parse getAppAccessToken response JSON ({} bytes, keys: [{}])",
                text.len(),
                json_keys_preview(&text)
            )
        })?;
        if parsed.access_token.trim().is_empty() {
            anyhow::bail!("QQ getAppAccessToken returned an empty access_token");
        }

        let valid_duration = compute_token_valid_duration(parsed.expires_in);
        let auth_header = format!("QQBot {}", parsed.access_token);
        let cached = CachedToken {
            token: parsed.access_token,
            auth_header,
            expires_at: Instant::now() + valid_duration,
        };

        self.store_cached_token(cached.clone());
        debug!(
            expires_in_secs = parsed.expires_in,
            cached_secs = valid_duration.as_secs(),
            "QQ Bot AccessToken acquired successfully"
        );

        Ok(cached)
    }

    /// Returns the preformatted `"QQBot <token>"` authorization header string.
    pub(crate) async fn get_auth_header(&self) -> Result<String> {
        Ok(self.get_cached_token().await?.auth_header)
    }

    async fn execute_authorized<F>(&self, mut make_req: F) -> Result<reqwest::Response>
    where
        F: FnMut(&str) -> reqwest::RequestBuilder,
    {
        let cached = self.get_cached_token().await?;
        let resp = make_req(&cached.auth_header).send().await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            warn!("QQ API returned 401 Unauthorized; invalidating cached token and retrying once");
            self.invalidate_token(&cached.token);
            let fresh = self.get_cached_token().await?;
            return Ok(make_req(&fresh.auth_header).send().await?);
        }
        Ok(resp)
    }

    /// Queries the QQ Open Platform for the WSS Gateway URL.
    pub(crate) async fn get_gateway_url(&self) -> Result<String> {
        let url = format!("{}/gateway/bot", self.api_base_url);
        let resp = self
            .execute_authorized(|auth| self.http.get(&url).header("Authorization", auth))
            .await
            .context("Failed to request QQ WebSocket gateway URL")?;

        let status = resp.status();
        let text = read_response_text(resp, "get_gateway_url").await;

        if !status.is_success() {
            anyhow::bail!("Failed to get gateway URL: HTTP {} - {}", status, text);
        }

        let parsed: GatewayResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "Failed to parse gateway response JSON ({} bytes, keys: [{}])",
                text.len(),
                json_keys_preview(&text)
            )
        })?;

        Ok(parsed.url)
    }

    async fn post_message_once(
        &self,
        url: &str,
        target: QqTarget<'_>,
        payload: OutgoingPayload<'_>,
        passive_msg_id: Option<&str>,
    ) -> Result<PostOutcome> {
        let passive_id = passive_msg_id.map(str::trim).filter(|id| !id.is_empty());
        let (msg_type, content, markdown, media, context_label) = match payload {
            OutgoingPayload::Text(text) => {
                (QqMessageType::Text, text, None, None, "send_message(plain)")
            }
            OutgoingPayload::Markdown(md) => (
                QqMessageType::Markdown,
                "",
                Some(MarkdownPayload { content: md }),
                None,
                "send_message(markdown)",
            ),
            OutgoingPayload::Media(file_info) => (
                QqMessageType::Media,
                " ",
                None,
                Some(MediaPayload { file_info }),
                "send_rich_media_message",
            ),
        };
        let req = SendMessageRequest {
            content,
            msg_type: msg_type.as_u8(),
            msg_id: passive_id,
            msg_seq: passive_id.map(|_| next_msg_seq()),
            markdown,
            media,
        };

        let resp = self
            .execute_authorized(|auth| self.http.post(url).header("Authorization", auth).json(&req))
            .await
            .with_context(|| {
                format!(
                    "Failed to send QQ {} {}",
                    target.kind_label(),
                    context_label
                )
            })?;

        let status = resp.status();
        let body = read_response_text(resp, context_label).await;
        if status.is_success() {
            Ok(PostOutcome::Sent(parse_message_response(
                &body,
                context_label,
            )))
        } else {
            Ok(PostOutcome::Rejected {
                status,
                body,
                passive_exhausted: false,
            })
        }
    }

    async fn post_message_with_proactive_retry(
        &self,
        target: QqTarget<'_>,
        payload: OutgoingPayload<'_>,
        msg_id: &str,
    ) -> Result<PostOutcome> {
        let url = target.messages_url(&self.api_base_url);
        let outcome = self
            .post_message_once(&url, target, payload, Some(msg_id))
            .await?;

        if let PostOutcome::Rejected { ref body, .. } = outcome
            && !msg_id.trim().is_empty()
            && let Some(api_err) = parse_qq_api_error(body)
            && api_err.is_passive_reply_limit()
        {
            warn!(
                target = target.kind_label(),
                api_message = api_err.message.as_deref().unwrap_or(""),
                "QQ passive reply limit or TTL exceeded (40034128); retrying once as proactive message"
            );
            return match self.post_message_once(&url, target, payload, None).await? {
                PostOutcome::Sent(meta) => Ok(PostOutcome::Sent(meta)),
                PostOutcome::Rejected { status, body, .. } => Ok(PostOutcome::Rejected {
                    status,
                    body,
                    passive_exhausted: true,
                }),
            };
        }

        Ok(outcome)
    }

    /// Sends a plain-text passive reply directly (`msg_type=0`), skipping any Markdown probe RTT.
    pub(crate) async fn send_plain_message_with_meta(
        &self,
        target: QqTarget<'_>,
        plain_text: &str,
        msg_id: &str,
    ) -> Result<MessageResponse> {
        let plain_slice = if plain_text.trim().is_empty() {
            " "
        } else {
            plain_text
        };

        match self
            .post_message_with_proactive_retry(target, OutgoingPayload::Text(plain_slice), msg_id)
            .await?
        {
            PostOutcome::Sent(meta) => Ok(meta),
            PostOutcome::Rejected {
                status,
                body,
                passive_exhausted,
            } => {
                warn!(
                    target = target.kind_label(),
                    status = %status,
                    passive_exhausted,
                    error_body = %body,
                    "QQ send_plain_message failed"
                );
                if passive_exhausted {
                    anyhow::bail!(
                        "QQ send_message ({}) failed after passive limit (40034128) and proactive rejection: HTTP {} - {}",
                        target.kind_label(),
                        status,
                        body
                    );
                }
                anyhow::bail!(
                    "QQ send_message ({}) failed: HTTP {} - {}",
                    target.kind_label(),
                    status,
                    body
                );
            }
        }
    }

    /// Sends a plain-text passive reply directly (`msg_type=0`) and returns its message ID.
    #[cfg(test)]
    pub(crate) async fn send_plain_message(
        &self,
        target: QqTarget<'_>,
        plain_text: &str,
        msg_id: &str,
    ) -> Result<Option<String>> {
        self.send_plain_message_with_meta(target, plain_text, msg_id)
            .await
            .map(|m| m.id)
    }

    /// Sends a passive reply and returns full [`MessageResponse`] metadata (`id` + `ext_info.ref_idx`).
    pub(crate) async fn send_message_with_meta(
        &self,
        target: QqTarget<'_>,
        formatted_markdown: &str,
        msg_id: &str,
    ) -> Result<MessageResponse> {
        let mut markdown_unsupported = false;
        let mut fallback_msg_id = msg_id;

        if !self.is_markdown_disabled() {
            match self
                .post_message_with_proactive_retry(
                    target,
                    OutgoingPayload::Markdown(formatted_markdown),
                    msg_id,
                )
                .await?
            {
                PostOutcome::Sent(meta) => return Ok(meta),
                PostOutcome::Rejected {
                    status,
                    body,
                    passive_exhausted,
                } => {
                    let is_md_unsupported = is_markdown_unsupported_rejection(status, &body);
                    if passive_exhausted {
                        // The passive `msg_id` already hit `40034128` and the proactive attempt also
                        // failed. Never re-probe the exhausted `msg_id` with plain text.
                        if is_proactive_permission_error(&body) || !is_md_unsupported {
                            warn!(
                                target = target.kind_label(),
                                status = %status,
                                error_body = %body,
                                "QQ passive msg_id exhausted (40034128) and proactive fallback rejected; aborting without duplicate plain retry"
                            );
                            anyhow::bail!(
                                "QQ send_message ({}) failed after passive limit (40034128) and proactive rejection: HTTP {} - {}",
                                target.kind_label(),
                                status,
                                body
                            );
                        }
                        // Proactive Markdown rejected specifically because Markdown is unsupported;
                        // fall back to proactive plain text (empty msg_id) without hitting 40034128 again.
                        fallback_msg_id = "";
                        markdown_unsupported = true;
                        debug!(
                            target = target.kind_label(),
                            status = %status,
                            error_body = %body,
                            "QQ proactive markdown unsupported after passive exhaustion; falling back to proactive plain text"
                        );
                    } else if is_proactive_permission_error(&body) {
                        warn!(
                            target = target.kind_label(),
                            status = %status,
                            error_body = %body,
                            "QQ proactive message rejected (40034105); skipping plain fallback"
                        );
                        anyhow::bail!(
                            "QQ send_message ({}) proactive message rejected: HTTP {} - {}",
                            target.kind_label(),
                            status,
                            body
                        );
                    } else if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        warn!(
                            target = target.kind_label(),
                            status = %status,
                            error_body = %body,
                            "QQ API rate limited (429); skipping plain fallback and keeping Markdown enabled"
                        );
                        anyhow::bail!(
                            "QQ send_message ({}) rate limited: HTTP {} - {}",
                            target.kind_label(),
                            status,
                            body
                        );
                    } else {
                        markdown_unsupported = is_md_unsupported;
                        debug!(
                            target = target.kind_label(),
                            status = %status,
                            markdown_unsupported,
                            error_body = %body,
                            "QQ markdown message rejected; falling back to plain text"
                        );
                    }
                }
            }
        }

        let plain_content = QqFormatter::formatted_to_plain_text(formatted_markdown);
        let meta = self
            .send_plain_message_with_meta(target, &plain_content, fallback_msg_id)
            .await?;
        if markdown_unsupported {
            self.mark_markdown_disabled();
        }
        Ok(meta)
    }

    /// Sends a passive reply to either a C2C user or a Group.
    #[cfg(test)]
    pub(crate) async fn send_message(
        &self,
        target: QqTarget<'_>,
        formatted_markdown: &str,
        msg_id: &str,
    ) -> Result<Option<String>> {
        self.send_message_with_meta(target, formatted_markdown, msg_id)
            .await
            .map(|m| m.id)
    }

    /// Streams a message attachment (image/file) into `dest`, enforcing `max_bytes` per chunk
    /// before buffering into RAM and cleaning up partial files on failure. Retries up to
    /// [`DOWNLOAD_MAX_RETRIES`] times on transient network/timeout/5xx errors.
    pub(crate) async fn download_attachment(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<u64> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            anyhow::bail!("unsupported attachment URL: {url}");
        }

        for attempt in 0..=DOWNLOAD_MAX_RETRIES {
            let mut resp = match self.http.get(url).send().await {
                Ok(r) => r,
                Err(err) => {
                    if attempt < DOWNLOAD_MAX_RETRIES && is_retryable_download_error(&err) {
                        let backoff = DOWNLOAD_RETRY_BACKOFF * (attempt as u32 + 1);
                        warn!(
                            attempt = attempt + 1,
                            max_attempts = DOWNLOAD_MAX_RETRIES + 1,
                            backoff_ms = backoff.as_millis(),
                            error = %err,
                            "QQ attachment request failed transiently; retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(err).context("Failed to download QQ attachment");
                }
            };

            let status = resp.status();
            if !status.is_success() {
                if attempt < DOWNLOAD_MAX_RETRIES && is_retryable_download_status(status) {
                    let backoff = DOWNLOAD_RETRY_BACKOFF * (attempt as u32 + 1);
                    warn!(
                        attempt = attempt + 1,
                        max_attempts = DOWNLOAD_MAX_RETRIES + 1,
                        backoff_ms = backoff.as_millis(),
                        status = %status,
                        "QQ attachment download returned transient HTTP error; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                anyhow::bail!("QQ attachment download returned HTTP {status}");
            }
            if let Some(len) = resp.content_length()
                && len > max_bytes
            {
                anyhow::bail!("QQ attachment is {len} bytes, over the {max_bytes}-byte limit");
            }

            let file = tokio::fs::File::create(dest)
                .await
                .with_context(|| format!("Failed to create attachment file {}", dest.display()))?;
            let mut writer = BufWriter::new(file);
            let mut total: u64 = 0;

            let stream_res: std::result::Result<u64, DownloadStreamError> = async {
                loop {
                    let next = match resp.chunk().await {
                        Ok(chunk) => chunk,
                        Err(err) => return Err(DownloadStreamError::Network(err)),
                    };
                    let Some(chunk) = next else {
                        break;
                    };
                    let next_total = total.checked_add(chunk.len() as u64).ok_or_else(|| {
                        DownloadStreamError::Fatal(anyhow::anyhow!("QQ attachment size overflow"))
                    })?;
                    if next_total > max_bytes {
                        return Err(DownloadStreamError::Fatal(anyhow::anyhow!(
                            "QQ attachment is {next_total} bytes, over the {max_bytes}-byte limit"
                        )));
                    }
                    writer.write_all(&chunk).await.map_err(|err| {
                        DownloadStreamError::Fatal(
                            anyhow::Error::new(err).context(format!(
                                "Failed to write attachment to {}",
                                dest.display()
                            )),
                        )
                    })?;
                    total = next_total;
                }
                writer.flush().await.map_err(|err| {
                    DownloadStreamError::Fatal(
                        anyhow::Error::new(err)
                            .context(format!("Failed to flush attachment to {}", dest.display())),
                    )
                })?;
                Ok(total)
            }
            .await;

            match stream_res {
                Ok(bytes) => return Ok(bytes),
                Err(DownloadStreamError::Network(err)) => {
                    drop(writer);
                    let _ = tokio::fs::remove_file(dest).await;
                    if attempt < DOWNLOAD_MAX_RETRIES {
                        let backoff = DOWNLOAD_RETRY_BACKOFF * (attempt as u32 + 1);
                        warn!(
                            attempt = attempt + 1,
                            max_attempts = DOWNLOAD_MAX_RETRIES + 1,
                            backoff_ms = backoff.as_millis(),
                            error = %err,
                            "QQ attachment body stream interrupted; retrying"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    return Err(err).context("Failed to read QQ attachment body chunk");
                }
                Err(DownloadStreamError::Fatal(err)) => {
                    drop(writer);
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(err);
                }
            }
        }

        anyhow::bail!("Failed to download QQ attachment after retries")
    }

    /// Uploads a local media/document file (`file_data` base64, up to 5 MB) to QQ's `/files`
    /// endpoint (`srv_send_msg: false`) and returns the opaque `file_info` handle so callers can
    /// overlap upload latency with text delivery before posting `msg_type=7`.
    pub(crate) async fn upload_media_file(
        &self,
        target: QqTarget<'_>,
        path: &Path,
        file_type: QqMediaFileType,
    ) -> Result<String> {
        let fallback_name = match file_type {
            QqMediaFileType::Image => "image.png",
            QqMediaFileType::Video => "video.mp4",
            QqMediaFileType::Voice => "voice.mp3",
            QqMediaFileType::File => "file.bin",
        };
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(fallback_name);

        let meta = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("Failed to stat media {}", path.display()))?;
        let file_len = meta.len();
        if file_len == 0 {
            anyhow::bail!("media {} is empty", path.display());
        }
        if file_len > MAX_DIRECT_MEDIA_BYTES {
            anyhow::bail!(
                "media {} is {file_len} bytes; QQ direct upload allows {MAX_DIRECT_MEDIA_BYTES}",
                path.display()
            );
        }

        let file = tokio::fs::File::open(path)
            .await
            .with_context(|| format!("Failed to open media {}", path.display()))?;
        let mut bytes = Vec::with_capacity(usize::try_from(file_len).unwrap_or(0));
        file.take(MAX_DIRECT_MEDIA_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .with_context(|| format!("Failed to read media {}", path.display()))?;

        if bytes.len() as u64 > MAX_DIRECT_MEDIA_BYTES {
            anyhow::bail!(
                "media {} grew beyond {MAX_DIRECT_MEDIA_BYTES} bytes during read",
                path.display()
            );
        }

        let encoded = base64_encode(&bytes);
        // Release raw media buffer immediately to halve peak RAM during JSON serialization/upload.
        drop(bytes);
        let upload_req = UploadMediaRequest {
            file_type: file_type.as_u8(),
            srv_send_msg: false,
            file_name: Some(file_name),
            file_data: Some(&encoded),
            url: None,
        };

        self.upload_media_payload(target, file_type, &upload_req)
            .await
    }

    /// Sends a previously uploaded `file_info` token as a rich-media reply (`msg_type=7`).
    pub(crate) async fn send_uploaded_media_with_meta(
        &self,
        target: QqTarget<'_>,
        file_info: &str,
        msg_id: &str,
        file_type: QqMediaFileType,
    ) -> Result<MessageResponse> {
        match self
            .post_message_with_proactive_retry(target, OutgoingPayload::Media(file_info), msg_id)
            .await?
        {
            PostOutcome::Sent(meta) => Ok(meta),
            PostOutcome::Rejected { status, body, .. } => anyhow::bail!(
                "QQ {} message failed: HTTP {status} - {body}",
                file_type.label()
            ),
        }
    }

    /// Uploads a local media/document file (`file_data` base64, up to 5 MB) and returns full
    /// [`MessageResponse`] metadata (`id` + `ext_info.ref_idx`).
    pub(crate) async fn send_media_file_with_meta(
        &self,
        target: QqTarget<'_>,
        path: &Path,
        msg_id: &str,
        file_type: QqMediaFileType,
    ) -> Result<MessageResponse> {
        let file_info = self.upload_media_file(target, path, file_type).await?;
        self.send_uploaded_media_with_meta(target, &file_info, msg_id, file_type)
            .await
    }

    /// Uploads a local media/document file (`file_data` base64, up to 5 MB) and sends it as a
    /// rich-media reply (`msg_type=7`) to either C2C or Group.
    #[cfg(test)]
    pub(crate) async fn send_media_file(
        &self,
        target: QqTarget<'_>,
        path: &Path,
        msg_id: &str,
        file_type: QqMediaFileType,
    ) -> Result<Option<String>> {
        self.send_media_file_with_meta(target, path, msg_id, file_type)
            .await
            .map(|m| m.id)
    }

    async fn upload_media_payload(
        &self,
        target: QqTarget<'_>,
        file_type: QqMediaFileType,
        upload_req: &UploadMediaRequest<'_>,
    ) -> Result<String> {
        let files_url = target.files_url(&self.api_base_url);
        let upload_resp = self
            .execute_authorized(|auth| {
                self.http
                    .post(&files_url)
                    .header("Authorization", auth)
                    .json(upload_req)
            })
            .await
            .with_context(|| format!("Failed to upload QQ {}", file_type.label()))?;

        let status = upload_resp.status();
        let text = read_response_text(upload_resp, "upload_media").await;
        if !status.is_success() {
            anyhow::bail!(
                "QQ {} upload failed: HTTP {status} - {text}",
                file_type.label()
            );
        }

        let parsed: UploadMediaResponse = serde_json::from_str(&text).with_context(|| {
            format!(
                "Failed to parse QQ {} upload response JSON ({} bytes, keys: [{}])",
                file_type.label(),
                text.len(),
                json_keys_preview(&text)
            )
        })?;
        debug!(
            file_type = file_type.as_u8(),
            file_uuid = parsed.file_uuid.as_deref().unwrap_or(""),
            ttl = parsed.ttl,
            id = parsed.id.as_deref().unwrap_or(""),
            "QQ media upload metadata received"
        );
        let Some(file_info) = parsed.file_info.filter(|s| !s.is_empty()) else {
            anyhow::bail!(
                "QQ {} upload returned no file_info: {text}",
                file_type.label()
            );
        };
        Ok(file_info)
    }

    async fn upload_media_and_send(
        &self,
        target: QqTarget<'_>,
        msg_id: &str,
        file_type: QqMediaFileType,
        upload_req: &UploadMediaRequest<'_>,
    ) -> Result<MessageResponse> {
        let file_info = self
            .upload_media_payload(target, file_type, upload_req)
            .await?;
        self.send_uploaded_media_with_meta(target, &file_info, msg_id, file_type)
            .await
    }

    /// Uploads in-memory media bytes (`file_data` base64) and sends a `msg_type=7` reply,
    /// returning full [`MessageResponse`] metadata (`id` + `ext_info.ref_idx`).
    pub(crate) async fn send_rich_media_bytes_with_meta(
        &self,
        target: QqTarget<'_>,
        bytes: &[u8],
        msg_id: &str,
        file_type: QqMediaFileType,
        file_name: &str,
    ) -> Result<MessageResponse> {
        if bytes.is_empty() {
            anyhow::bail!("QQ {} upload payload is empty", file_type.label());
        }
        if bytes.len() as u64 > MAX_DIRECT_MEDIA_BYTES {
            anyhow::bail!(
                "QQ {} is {} bytes; direct upload allows {MAX_DIRECT_MEDIA_BYTES}",
                file_type.label(),
                bytes.len()
            );
        }

        let encoded = base64_encode(bytes);
        let upload_req = UploadMediaRequest {
            file_type: file_type.as_u8(),
            srv_send_msg: false,
            file_name: Some(file_name),
            file_data: Some(&encoded),
            url: None,
        };

        self.upload_media_and_send(target, msg_id, file_type, &upload_req)
            .await
    }

    /// Uploads in-memory media bytes (`file_data` base64) and sends a `msg_type=7` reply without
    /// touching local disk.
    #[cfg(test)]
    pub(crate) async fn send_rich_media_bytes(
        &self,
        target: QqTarget<'_>,
        bytes: &[u8],
        msg_id: &str,
        file_type: QqMediaFileType,
        file_name: &str,
    ) -> Result<Option<String>> {
        self.send_rich_media_bytes_with_meta(target, bytes, msg_id, file_type, file_name)
            .await
            .map(|m| m.id)
    }

    /// Uploads media from a public HTTP/HTTPS `url` (`srv_send_msg: false`) and sends `msg_type=7`.
    #[cfg(test)]
    pub(crate) async fn send_rich_media_url(
        &self,
        target: QqTarget<'_>,
        media_url: &str,
        msg_id: &str,
        file_type: QqMediaFileType,
        file_name: Option<&str>,
    ) -> Result<Option<String>> {
        if !(media_url.starts_with("https://") || media_url.starts_with("http://")) {
            anyhow::bail!(
                "QQ {} URL must start with http:// or https://",
                file_type.label()
            );
        }

        let upload_req = UploadMediaRequest {
            file_type: file_type.as_u8(),
            srv_send_msg: false,
            file_name,
            file_data: None,
            url: Some(media_url),
        };

        self.upload_media_and_send(target, msg_id, file_type, &upload_req)
            .await
            .map(|m| m.id)
    }

    /// Recalls/deletes an existing C2C or Group message.
    pub(crate) async fn delete_message(
        &self,
        target: QqTarget<'_>,
        message_id: &str,
    ) -> Result<()> {
        let url = target.delete_message_url(&self.api_base_url, message_id);
        let resp = self
            .execute_authorized(|auth| self.http.delete(&url).header("Authorization", auth))
            .await
            .with_context(|| format!("Failed to recall {} message", target.kind_label()))?;

        let status = resp.status();
        if !status.is_success() {
            let text = read_response_text(resp, "delete_message").await;
            let is_expired_recall =
                parse_qq_api_error(&text).is_some_and(|e| e.error_code() == Some(40_064_004));
            if is_expired_recall {
                debug!(
                    target = target.kind_label(),
                    "QQ placeholder message exceeded 2-minute recall TTL (40064004); ignoring"
                );
            } else {
                warn!(
                    target = target.kind_label(),
                    status = %status,
                    error_body = %text,
                    "QQ delete_message returned non-success"
                );
            }
            anyhow::bail!(
                "QQ delete_message ({}) failed: HTTP {} - {}",
                target.kind_label(),
                status,
                text
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum OutgoingPayload<'a> {
    Text(&'a str),
    Markdown(&'a str),
    Media(&'a str),
}

enum PostOutcome {
    Sent(MessageResponse),
    Rejected {
        status: reqwest::StatusCode,
        body: String,
        passive_exhausted: bool,
    },
}

enum DownloadStreamError {
    Network(reqwest::Error),
    Fatal(anyhow::Error),
}

#[inline]
fn is_retryable_download_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_request() || err.is_body()
}

#[inline]
fn is_retryable_download_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// Returns `true` when the QQ API body reports code `40034105` (no proactive message permission).
#[inline]
#[must_use]
pub(crate) fn is_proactive_permission_error(body: &str) -> bool {
    parse_qq_api_error(body).is_some_and(|e| e.error_code() == Some(40_034_105))
}

/// Returns `true` when a delivery error indicates passive reply exhaustion (`40034128`) or
/// missing proactive messaging permission (`40034105`).
#[must_use]
pub(crate) fn is_passive_or_proactive_limit_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("40034128") || msg.contains("40034105")
}

/// Returns `true` ONLY for explicit client-side "Markdown unsupported / no Markdown permission"
/// rejections, never for transient server errors, rate limits (`429`), passive reply exhaustion
/// (`40034128`), or proactive permission failures (`40034105`).
#[must_use]
fn is_markdown_unsupported_rejection(status: reqwest::StatusCode, body: &str) -> bool {
    if !status.is_client_error()
        || status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        return false;
    }
    let parsed = parse_qq_api_error(body);
    if let Some(code) = parsed.as_ref().and_then(|e| e.error_code()) {
        if matches!(
            code,
            40_034_128 | 40_034_105 | 40_034_101 | 40_034_102 | 40_034_103 | 22_009
        ) {
            return false;
        }
        if matches!(code, 40_034_025 | 40_054_005 | 50_056 | 11_292) {
            return true;
        }
    }

    let raw_msg = parsed
        .as_ref()
        .and_then(|e| e.message.as_deref())
        .unwrap_or(body);
    let lower = raw_msg.to_ascii_lowercase();
    if lower.contains("proactive")
        || lower.contains("passive")
        || lower.contains("rate limit")
        || lower.contains("too many")
        || lower.contains("freq")
        || raw_msg.contains("主动")
        || raw_msg.contains("被动")
    {
        return false;
    }

    lower.contains("markdown")
        || lower.contains("msg_type")
        || lower.contains("template")
        || lower.contains("not support")
        || lower.contains("unsupported")
        || lower.contains("no permission")
        || raw_msg.contains("不支持")
        || raw_msg.contains("无权限")
}

/// Computes token cache validity duration with a clamped early-refresh margin so short-TTL
/// tokens (`<= 300s`) still stay cached for half their lifetime instead of expiring immediately.
#[inline]
#[must_use]
pub(crate) fn compute_token_valid_duration(expires_in_secs: u64) -> Duration {
    let margin = TOKEN_REFRESH_MARGIN_SECS.min(expires_in_secs / 2);
    Duration::from_secs(expires_in_secs.saturating_sub(margin))
}

fn msg_seq_counter() -> &'static AtomicU64 {
    MSG_SEQ_COUNTER.get_or_init(|| {
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| {
            let secs = d.as_secs();
            let millis = u64::from(d.subsec_millis());
            let pid = u64::from(std::process::id());
            (secs.wrapping_mul(131) ^ millis ^ pid.wrapping_mul(17)) % 65_535
        });
        AtomicU64::new(seed)
    })
}

/// Generates a monotonically advancing `msg_seq` in `1..=65535`, initialized from wall-clock low
/// bits at startup to avoid collisions across rapid restarts within QQ's 5-minute dedup window.
#[inline]
#[must_use]
pub(crate) fn next_msg_seq() -> u32 {
    ((msg_seq_counter().fetch_add(1, Ordering::Relaxed) % 65_535) + 1) as u32
}

async fn read_response_text(resp: reqwest::Response, context_label: &str) -> String {
    match resp.text().await {
        Ok(text) => text,
        Err(err) => {
            warn!(
                context = context_label,
                error = %err,
                "Failed to read QQ API response body"
            );
            String::new()
        }
    }
}

fn parse_message_response(body: &str, context_label: &str) -> MessageResponse {
    if body.trim().is_empty() {
        return MessageResponse::default();
    }
    match serde_json::from_str::<MessageResponse>(body) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(
                context = context_label,
                error = %err,
                body_len = body.len(),
                "Failed to parse QQ MessageResponse JSON"
            );
            MessageResponse::default()
        }
    }
}

/// Keys present in a JSON body, used for parse-error context. The raw body is never logged:
/// a failed `getAppAccessToken` parse used to write a live bearer token into journald.
fn json_keys_preview(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect::<Vec<_>>().join(","),
        _ => String::new(),
    }
}

/// Standard-alphabet base64 for QQ's `file_data` field without per-char UTF-8 push overhead.
pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    let (chunks, remainder) = bytes.as_chunks::<3>();

    for &[b0, b1, b2] in chunks {
        let packed = (usize::from(b0) << 16) | (usize::from(b1) << 8) | usize::from(b2);
        out.extend_from_slice(&[
            ALPHABET[(packed >> 18) & 63],
            ALPHABET[(packed >> 12) & 63],
            ALPHABET[(packed >> 6) & 63],
            ALPHABET[packed & 63],
        ]);
    }

    match remainder {
        [b0] => {
            let packed = usize::from(*b0) << 16;
            out.extend_from_slice(&[
                ALPHABET[(packed >> 18) & 63],
                ALPHABET[(packed >> 12) & 63],
                b'=',
                b'=',
            ]);
        }
        [b0, b1] => {
            let packed = (usize::from(*b0) << 16) | (usize::from(*b1) << 8);
            out.extend_from_slice(&[
                ALPHABET[(packed >> 18) & 63],
                ALPHABET[(packed >> 12) & 63],
                ALPHABET[(packed >> 6) & 63],
                b'=',
            ]);
        }
        _ => {}
    }

    String::from_utf8(out).expect("base64 alphabet is pure ASCII")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_http<F>(handler: F) -> (String, tokio::task::JoinHandle<()>)
    where
        F: Fn(String, String) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end;
                    loop {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }

                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);

                    while buf.len() < header_end + content_len {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let req_line = headers.lines().next().unwrap_or("").to_string();
                    let body = String::from_utf8_lossy(
                        &buf[header_end..(header_end + content_len).min(buf.len())],
                    )
                    .to_string();

                    let (status, resp_body) = handler(req_line, body);
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        404 => "Not Found",
                        _ => "Error",
                    };
                    let raw_resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw_resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (format!("http://{addr}"), handle)
    }

    #[test]
    fn json_keys_preview_never_echoes_values() {
        let preview = json_keys_preview(r#"{"access_token":"secret-value","expires_in":"4875"}"#);
        assert!(preview.contains("access_token"));
        assert!(preview.contains("expires_in"));
        assert!(!preview.contains("secret-value"));
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn compute_token_valid_duration_clamps_short_ttls() {
        assert_eq!(
            compute_token_valid_duration(7200),
            Duration::from_secs(6900)
        );
        assert_eq!(compute_token_valid_duration(300), Duration::from_secs(150));
        assert_eq!(compute_token_valid_duration(60), Duration::from_secs(30));
        assert_eq!(compute_token_valid_duration(1), Duration::from_secs(1));
        assert_eq!(compute_token_valid_duration(0), Duration::from_secs(0));
    }

    #[test]
    fn next_msg_seq_advances_without_collision_and_stays_within_u16() {
        let first = next_msg_seq();
        let second = next_msg_seq();
        let third = next_msg_seq();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert!(first <= 0xFFFF && second <= 0xFFFF && third <= 0xFFFF);
    }

    #[test]
    fn qq_target_builds_c2c_and_group_urls_and_redacts_debug() {
        let c2c = QqTarget::C2c("u123");
        let grp = QqTarget::Group("g456");
        assert_eq!(
            c2c.messages_url("https://api.example"),
            "https://api.example/v2/users/u123/messages"
        );
        assert_eq!(
            grp.messages_url("https://api.example"),
            "https://api.example/v2/groups/g456/messages"
        );
        assert_eq!(
            c2c.files_url("https://api.example"),
            "https://api.example/v2/users/u123/files"
        );
        assert_eq!(
            grp.files_url("https://api.example"),
            "https://api.example/v2/groups/g456/files"
        );
        assert_eq!(
            c2c.delete_message_url("https://api.example", "m1"),
            "https://api.example/v2/users/u123/messages/m1"
        );

        let client = QqClient::new_for_test(
            "app1",
            "super-secret-value",
            "http://127.0.0.1:1",
            "http://127.0.0.1:2",
        );
        let dbg = format!("{client:?}");
        assert!(dbg.contains("app1"));
        assert!(!dbg.contains("super-secret-value"));
    }

    #[tokio::test]
    async fn client_singleflight_caches_token_and_retries_once_on_401() {
        let token_calls = Arc::new(AtomicUsize::new(0));
        let api_calls = Arc::new(AtomicUsize::new(0));

        let tc = token_calls.clone();
        let ac = api_calls.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, _body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                let n = tc.fetch_add(1, Ordering::SeqCst) + 1;
                (
                    200,
                    format!(r#"{{"access_token":"tok_{n}","expires_in":7200}}"#),
                )
            } else if req_line.starts_with("GET /gateway/bot ") {
                let n = ac.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    (401, r#"{"message":"expired"}"#.to_string())
                } else {
                    (200, r#"{"url":"wss://gateway.example"}"#.to_string())
                }
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = Arc::new(QqClient::new_for_test("id", "sec", &base_url, &base_url));

        // Concurrent get_auth_header calls coalesce into a single token POST.
        let (h1, h2, h3) = tokio::join!(
            client.get_auth_header(),
            client.get_auth_header(),
            client.get_auth_header()
        );
        assert_eq!(h1.unwrap(), "QQBot tok_1");
        assert_eq!(h2.unwrap(), "QQBot tok_1");
        assert_eq!(h3.unwrap(), "QQBot tok_1");
        assert_eq!(token_calls.load(Ordering::SeqCst), 1);

        // First /gateway/bot gets 401 -> invalidates tok_1 -> refreshes to tok_2 -> succeeds.
        let gw = client.get_gateway_url().await.unwrap();
        assert_eq!(gw, "wss://gateway.example");
        assert_eq!(token_calls.load(Ordering::SeqCst), 2);
        assert_eq!(api_calls.load(Ordering::SeqCst), 2);
        assert_eq!(client.get_auth_header().await.unwrap(), "QQBot tok_2");

        server.abort();
    }

    #[tokio::test]
    async fn send_message_falls_back_on_http_rejection_but_propagates_transport_errors() {
        let sent_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sb = sent_bodies.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/users/u1/messages ") {
                sb.lock().unwrap().push(body.clone());
                if body.contains(r#""msg_type":2"#) {
                    (400, r#"{"message":"markdown disabled"}"#.to_string())
                } else {
                    (200, r#"{"id":"sent_plain_1"}"#.to_string())
                }
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let msg_id = client
            .send_message(QqTarget::C2c("u1"), "### 标题\n> • **Key**: Val", "m_1")
            .await
            .unwrap();
        assert_eq!(msg_id.as_deref(), Some("sent_plain_1"));

        {
            let bodies = sent_bodies.lock().unwrap();
            assert_eq!(bodies.len(), 2);
            assert!(bodies[0].contains(r#""msg_type":2"#));
            assert!(bodies[1].contains(r#""msg_type":0"#));
            assert!(bodies[1].contains("📌 【标题】"));
            assert!(!bodies[1].contains("**Key**"));
        }

        // Transport error: close port and ensure send_message returns Err instead of swallowing it.
        server.abort();
        let dead_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead_listener.local_addr().unwrap();
        drop(dead_listener);
        let dead_client = QqClient::new_for_test(
            "id",
            "sec",
            format!("http://{dead_addr}"),
            format!("http://{dead_addr}"),
        );
        // Seed token cache so only send_message hits the closed port.
        dead_client.store_cached_token(CachedToken {
            token: "t".into(),
            auth_header: "QQBot t".into(),
            expires_at: Instant::now() + Duration::from_secs(60),
        });
        assert!(
            dead_client
                .send_message(QqTarget::C2c("u1"), "hello", "m_2")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn download_attachment_streams_and_cleans_up_when_over_limit() {
        let (base_url, server) = spawn_mock_http(|req_line, _| {
            if req_line.starts_with("GET /big ") {
                (200, "0123456789abcdef".to_string())
            } else if req_line.starts_with("GET /ok ") {
                (200, "hello!".to_string())
            } else {
                (404, String::new())
            }
        })
        .await;

        let dir = std::env::temp_dir().join(format!("agygram_qq_dl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);

        let ok_path = dir.join("ok.bin");
        let n = client
            .download_attachment(&format!("{base_url}/ok"), &ok_path, 16)
            .await
            .unwrap();
        assert_eq!(n, 6);
        assert_eq!(std::fs::read(&ok_path).unwrap(), b"hello!");

        let big_path = dir.join("big.bin");
        let err = client
            .download_attachment(&format!("{base_url}/big"), &big_path, 8)
            .await
            .expect_err("oversized file must fail");
        assert!(err.to_string().contains("limit"));
        assert!(!big_path.exists(), "partial file must be removed on error");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rich_media_bytes_and_delete_message_errors() {
        let upload_payloads = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let up = upload_payloads.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/groups/g1/files ") {
                up.lock().unwrap().push(body);
                (200, r#"{"file_info":"fi_abc"}"#.to_string())
            } else if req_line.starts_with("POST /v2/groups/g1/messages ") {
                up.lock().unwrap().push(body);
                (200, r#"{"id":"voice_msg_1"}"#.to_string())
            } else if req_line.starts_with("DELETE /v2/groups/g1/messages/bad ") {
                (404, r#"{"message":"not found"}"#.to_string())
            } else {
                (200, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let id = client
            .send_rich_media_bytes(
                QqTarget::Group("g1"),
                b"fake-mp3-bytes",
                "m99",
                QqMediaFileType::Voice,
                "voice.mp3",
            )
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("voice_msg_1"));

        {
            let calls = upload_payloads.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert!(calls[0].contains(r#""file_type":3"#));
            assert!(calls[0].contains(&base64_encode(b"fake-mp3-bytes")));
            assert!(calls[1].contains(r#""msg_type":7"#));
            assert!(calls[1].contains(r#""file_info":"fi_abc""#));
        }

        assert!(
            client
                .delete_message(QqTarget::Group("g1"), "bad")
                .await
                .is_err()
        );

        server.abort();
    }

    #[tokio::test]
    async fn send_message_retries_proactively_on_40034128_passive_limit() {
        let sent_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sb = sent_bodies.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/groups/g1/messages ") {
                sb.lock().unwrap().push(body.clone());
                if body.contains("msg_id") {
                    (
                        400,
                        r#"{"code":40034128,"message":"passive limit reached"}"#.to_string(),
                    )
                } else {
                    (200, r#"{"id":"proactive_msg_1"}"#.to_string())
                }
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let msg_id = client
            .send_message(QqTarget::Group("g1"), "hello", "m_expired")
            .await
            .unwrap();
        assert_eq!(msg_id.as_deref(), Some("proactive_msg_1"));

        let bodies = sent_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].contains(r#""msg_id":"m_expired""#));
        assert!(bodies[0].contains(r#""msg_seq":"#));
        assert!(!bodies[1].contains("msg_id"));
        assert!(!bodies[1].contains("msg_seq"));

        server.abort();
    }

    #[tokio::test]
    async fn send_media_file_and_url_support_video_and_file_types() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let reqs = requests.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/groups/g1/files ") {
                reqs.lock().unwrap().push(body);
                (200, r#"{"file_info":"fi_file_1"}"#.to_string())
            } else if req_line.starts_with("POST /v2/groups/g1/messages ") {
                reqs.lock().unwrap().push(body);
                (200, r#"{"id":"media_msg_1"}"#.to_string())
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let dir =
            std::env::temp_dir().join(format!("agygram_qq_media_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf_path = dir.join("report.pdf");
        std::fs::write(&pdf_path, b"%PDF-1.4\nhello").unwrap();
        let mp4_path = dir.join("clip.mp4");
        std::fs::write(&mp4_path, b"mp4-bytes").unwrap();

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        assert_eq!(
            client
                .send_media_file(
                    QqTarget::Group("g1"),
                    &pdf_path,
                    "m_1",
                    QqMediaFileType::File
                )
                .await
                .unwrap()
                .as_deref(),
            Some("media_msg_1")
        );
        assert_eq!(
            client
                .send_media_file(
                    QqTarget::Group("g1"),
                    &mp4_path,
                    "m_2",
                    QqMediaFileType::Video
                )
                .await
                .unwrap()
                .as_deref(),
            Some("media_msg_1")
        );
        assert_eq!(
            client
                .send_rich_media_url(
                    QqTarget::Group("g1"),
                    "https://example.com/archive.zip",
                    "m_3",
                    QqMediaFileType::File,
                    Some("archive.zip"),
                )
                .await
                .unwrap()
                .as_deref(),
            Some("media_msg_1")
        );

        let calls = requests.lock().unwrap();
        assert_eq!(calls.len(), 6);
        assert!(calls[0].contains(r#""file_type":4"#));
        assert!(calls[0].contains(r#""file_name":"report.pdf""#));
        assert!(calls[2].contains(r#""file_type":2"#));
        assert!(calls[2].contains(r#""file_name":"clip.mp4""#));
        assert!(calls[4].contains(r#""file_type":4"#));
        assert!(calls[4].contains(r#""url":"https://example.com/archive.zip""#));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn empty_access_token_is_rejected_and_not_cached() {
        let (base_url, server) = spawn_mock_http(|req_line, _| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"  ","expires_in":7200}"#.to_string(),
                )
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let err = client
            .get_auth_header()
            .await
            .expect_err("empty token must be rejected");
        assert!(err.to_string().contains("empty access_token"));
        assert!(client.read_cached_token().is_none());

        server.abort();
    }

    #[tokio::test]
    async fn markdown_rejection_caches_plain_fast_path_and_plain_method_skips_probe() {
        let sent_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sb = sent_bodies.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/users/u1/messages ") {
                sb.lock().unwrap().push(body.clone());
                if body.contains(r#""msg_type":2"#) {
                    (
                        403,
                        r#"{"code":40034025,"message":"no markdown permission"}"#.to_string(),
                    )
                } else {
                    (200, r#"{"id":"sent_ok"}"#.to_string())
                }
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);

        // Plain-text helper always sends msg_type=0 in a single RTT without probing Markdown.
        assert_eq!(
            client
                .send_plain_message(QqTarget::C2c("u1"), "我想想...", "m_0")
                .await
                .unwrap()
                .as_deref(),
            Some("sent_ok")
        );
        assert_eq!(sent_bodies.lock().unwrap().len(), 1);
        assert!(sent_bodies.lock().unwrap()[0].contains(r#""msg_type":0"#));

        // First formatted message probes Markdown (403) and falls back to plain text (200),
        // caching the plain-text fast path.
        client
            .send_message(QqTarget::C2c("u1"), "**first**", "m_1")
            .await
            .unwrap();
        assert_eq!(sent_bodies.lock().unwrap().len(), 3);

        // Second formatted message must go straight to plain text (only 1 HTTP request).
        client
            .send_message(QqTarget::C2c("u1"), "**second**", "m_2")
            .await
            .unwrap();
        let bodies = sent_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 4);
        assert!(bodies[3].contains(r#""msg_type":0"#));
        assert!(!bodies[3].contains(r#""msg_type":2"#));

        server.abort();
    }

    #[tokio::test]
    async fn passive_40034128_and_proactive_40034105_never_reprobes_msg_id_or_blacklists_markdown()
    {
        let sent_bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sb = sent_bodies.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, body| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/groups/g1/messages ") {
                sb.lock().unwrap().push(body.clone());
                if body.contains("msg_id") {
                    (
                        400,
                        r#"{"code":40034128,"message":"passive reply limit reached"}"#.to_string(),
                    )
                } else {
                    (
                        400,
                        r#"{"code":40034105,"message":"no proactive message permission"}"#
                            .to_string(),
                    )
                }
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let err = client
            .send_message_with_meta(QqTarget::Group("g1"), "**long tail**", "m_exhausted")
            .await
            .expect_err("exhausted passive + denied proactive must error");
        assert!(is_passive_or_proactive_limit_error(&err));

        // Must make exactly 2 HTTP calls (1 passive Markdown -> 40034128, 1 proactive Markdown -> 40034105),
        // NEVER re-hitting `m_exhausted` with a duplicate plain-text probe.
        let bodies = sent_bodies.lock().unwrap();
        assert_eq!(bodies.len(), 2, "must not duplicate-probe exhausted msg_id");
        assert!(bodies[0].contains(r#""msg_id":"m_exhausted""#));
        assert!(!bodies[1].contains("msg_id"));
        assert!(
            !client.is_markdown_disabled(),
            "40034128 / 40034105 must never globally blacklist Markdown"
        );

        server.abort();
    }

    #[tokio::test]
    async fn rate_limit_429_and_unrelated_errors_do_not_blacklist_markdown() {
        assert!(!is_markdown_unsupported_rejection(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"message":"too many requests"}"#
        ));
        assert!(!is_markdown_unsupported_rejection(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":40034128,"message":"passive limit"}"#
        ));
        assert!(!is_markdown_unsupported_rejection(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":40034105,"message":"no proactive permission"}"#
        ));
        assert!(!is_markdown_unsupported_rejection(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"message":"markdown service temporary error"}"#
        ));
        assert!(is_markdown_unsupported_rejection(
            reqwest::StatusCode::FORBIDDEN,
            r#"{"code":40034025,"message":"no markdown permission"}"#
        ));
        assert!(is_markdown_unsupported_rejection(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"message":"markdown disabled"}"#
        ));

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = calls.clone();
        let (base_url, server) = spawn_mock_http(move |req_line, _| {
            if req_line.starts_with("POST /app/getAppAccessToken ") {
                (
                    200,
                    r#"{"access_token":"tok","expires_in":7200}"#.to_string(),
                )
            } else if req_line.starts_with("POST /v2/users/u1/messages ") {
                calls_c.fetch_add(1, Ordering::SeqCst);
                (429, r#"{"message":"rate limit exceeded"}"#.to_string())
            } else {
                (404, "{}".to_string())
            }
        })
        .await;

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        assert!(
            client
                .send_message(QqTarget::C2c("u1"), "hello", "m_429")
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!client.is_markdown_disabled());

        server.abort();
    }

    #[tokio::test]
    async fn download_attachment_retries_transient_5xx_but_not_404() {
        let flaky_hits = Arc::new(AtomicUsize::new(0));
        let not_found_hits = Arc::new(AtomicUsize::new(0));
        let fh = flaky_hits.clone();
        let nfh = not_found_hits.clone();

        let (base_url, server) = spawn_mock_http(move |req_line, _| {
            if req_line.starts_with("GET /flaky ") {
                let n = fh.fetch_add(1, Ordering::SeqCst) + 1;
                if n < 3 {
                    (503, "temporarily unavailable".to_string())
                } else {
                    (200, "recovered-payload".to_string())
                }
            } else if req_line.starts_with("GET /missing ") {
                nfh.fetch_add(1, Ordering::SeqCst);
                (404, "not found".to_string())
            } else {
                (404, String::new())
            }
        })
        .await;

        let dir =
            std::env::temp_dir().join(format!("agygram_qq_dl_retry_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let ok_dest = dir.join("flaky.bin");
        let bytes = client
            .download_attachment(&format!("{base_url}/flaky"), &ok_dest, 1024)
            .await
            .expect("flaky 503 should recover within 2 retries");
        assert_eq!(bytes, 17);
        assert_eq!(flaky_hits.load(Ordering::SeqCst), 3);
        assert_eq!(std::fs::read(&ok_dest).unwrap(), b"recovered-payload");

        let miss_dest = dir.join("missing.bin");
        assert!(
            client
                .download_attachment(&format!("{base_url}/missing"), &miss_dest, 1024)
                .await
                .is_err()
        );
        assert_eq!(
            not_found_hits.load(Ordering::SeqCst),
            1,
            "404 must not be retried"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
