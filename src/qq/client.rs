use super::formatter::QqFormatter;
use super::types::{
    GatewayResponse, MarkdownPayload, MediaPayload, MessageResponse, QqMediaFileType,
    QqMessageType, SendMessageRequest, TokenRequest, TokenResponse, UploadMediaRequest,
    UploadMediaResponse,
};
use anyhow::{Context, Result};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::sync::Mutex;
use tracing::{debug, warn};

const DEFAULT_TOKEN_BASE: &str = "https://bots.qq.com";
const DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;

/// QQ's direct-upload ceiling for `file_data`. Base64 expands the body ~4/3, but the platform
/// applies the 5 MB cap to the decoded payload.
pub(crate) const MAX_DIRECT_MEDIA_BYTES: u64 = 5 * 1024 * 1024;

static MSG_SEQ_COUNTER: AtomicU32 = AtomicU32::new(1);

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
            .tcp_keepalive(Duration::from_secs(60));

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

    /// Sends a passive reply to either a C2C user or a Group.
    ///
    /// Expects `formatted_markdown` already processed by [`QqFormatter::format_markdown`].
    /// Attempts QQ Native Markdown (`msg_type: 2`) first; only if QQ rejects the HTTP response
    /// status (e.g., markdown capability not enabled) does it fall back to plain text (`msg_type: 0`).
    /// Transport/network failures propagate immediately without duplicate delivery.
    pub(crate) async fn send_message(
        &self,
        target: QqTarget<'_>,
        formatted_markdown: &str,
        msg_id: &str,
    ) -> Result<Option<String>> {
        let url = target.messages_url(&self.api_base_url);
        let seq = next_msg_seq();

        let req_md = SendMessageRequest {
            content: "",
            msg_type: QqMessageType::Markdown.as_u8(),
            msg_id,
            msg_seq: Some(seq),
            markdown: Some(MarkdownPayload {
                content: formatted_markdown,
            }),
            media: None,
        };

        let resp_md = self
            .execute_authorized(|auth| {
                self.http
                    .post(&url)
                    .header("Authorization", auth)
                    .json(&req_md)
            })
            .await
            .with_context(|| {
                format!("Failed to send QQ {} markdown message", target.kind_label())
            })?;

        let md_status = resp_md.status();
        let md_text = read_response_text(resp_md, "send_message(markdown)").await;
        if md_status.is_success() {
            return Ok(parse_message_id(&md_text, "send_message(markdown)"));
        }

        debug!(
            target = target.kind_label(),
            status = %md_status,
            error_body = %md_text,
            "QQ markdown message rejected; falling back to plain text"
        );

        let plain_content = QqFormatter::formatted_to_plain_text(formatted_markdown);
        let plain_slice = if plain_content.trim().is_empty() {
            " "
        } else {
            plain_content.as_str()
        };
        let fallback_seq = next_msg_seq();
        let req_plain = SendMessageRequest {
            content: plain_slice,
            msg_type: QqMessageType::Text.as_u8(),
            msg_id,
            msg_seq: Some(fallback_seq),
            markdown: None,
            media: None,
        };

        let resp = self
            .execute_authorized(|auth| {
                self.http
                    .post(&url)
                    .header("Authorization", auth)
                    .json(&req_plain)
            })
            .await
            .with_context(|| format!("Failed to send QQ {} plain message", target.kind_label()))?;

        let status = resp.status();
        let text = read_response_text(resp, "send_message(plain)").await;
        if !status.is_success() {
            warn!(
                target = target.kind_label(),
                status = %status,
                error_body = %text,
                "QQ send_message failed"
            );
            anyhow::bail!(
                "QQ send_message ({}) failed: HTTP {} - {}",
                target.kind_label(),
                status,
                text
            );
        }

        Ok(parse_message_id(&text, "send_message(plain)"))
    }

    /// Streams a message attachment (image/file) into `dest`, enforcing `max_bytes` per chunk
    /// before buffering into RAM and cleaning up partial files on failure.
    pub(crate) async fn download_attachment(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
    ) -> Result<u64> {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            anyhow::bail!("unsupported attachment URL: {url}");
        }

        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .context("Failed to download QQ attachment")?;

        let status = resp.status();
        if !status.is_success() {
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

        let stream_res: Result<u64> = async {
            while let Some(chunk) = resp
                .chunk()
                .await
                .context("Failed to read QQ attachment body chunk")?
            {
                let next_total = total
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("QQ attachment size overflow"))?;
                if next_total > max_bytes {
                    anyhow::bail!(
                        "QQ attachment is {next_total} bytes, over the {max_bytes}-byte limit"
                    );
                }
                writer
                    .write_all(&chunk)
                    .await
                    .with_context(|| format!("Failed to write attachment to {}", dest.display()))?;
                total = next_total;
            }
            writer
                .flush()
                .await
                .with_context(|| format!("Failed to flush attachment to {}", dest.display()))?;
            Ok(total)
        }
        .await;

        if stream_res.is_err() {
            drop(writer);
            let _ = tokio::fs::remove_file(dest).await;
        }

        stream_res
    }

    /// Uploads a local image and sends it as a passive rich-media reply (`msg_type=7`).
    pub(crate) async fn send_image(
        &self,
        target: QqTarget<'_>,
        path: &Path,
        msg_id: &str,
    ) -> Result<Option<String>> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image.png");
        self.send_rich_media_file(target, path, msg_id, QqMediaFileType::Image, file_name)
            .await
    }

    async fn send_rich_media_file(
        &self,
        target: QqTarget<'_>,
        path: &Path,
        msg_id: &str,
        file_type: QqMediaFileType,
        file_name: &str,
    ) -> Result<Option<String>> {
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

        self.send_rich_media_bytes(target, &bytes, msg_id, file_type, file_name)
            .await
    }

    /// Uploads in-memory media bytes (`file_data` base64) and sends a `msg_type=7` reply without
    /// touching local disk.
    pub(crate) async fn send_rich_media_bytes(
        &self,
        target: QqTarget<'_>,
        bytes: &[u8],
        msg_id: &str,
        file_type: QqMediaFileType,
        file_name: &str,
    ) -> Result<Option<String>> {
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

        let files_url = target.files_url(&self.api_base_url);
        let messages_url = target.messages_url(&self.api_base_url);
        let encoded = base64_encode(bytes);
        let upload_req = UploadMediaRequest {
            file_type: file_type.as_u8(),
            srv_send_msg: false,
            file_name,
            file_data: &encoded,
        };

        let upload_resp = self
            .execute_authorized(|auth| {
                self.http
                    .post(&files_url)
                    .header("Authorization", auth)
                    .json(&upload_req)
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
        let Some(file_info) = parsed.file_info.as_deref().filter(|s| !s.is_empty()) else {
            anyhow::bail!(
                "QQ {} upload returned no file_info: {text}",
                file_type.label()
            );
        };

        let msg_req = SendMessageRequest {
            content: " ",
            msg_type: QqMessageType::Media.as_u8(),
            msg_id,
            msg_seq: Some(next_msg_seq()),
            markdown: None,
            media: Some(MediaPayload { file_info }),
        };

        let resp = self
            .execute_authorized(|auth| {
                self.http
                    .post(&messages_url)
                    .header("Authorization", auth)
                    .json(&msg_req)
            })
            .await
            .with_context(|| format!("Failed to send QQ {} message", file_type.label()))?;

        let status = resp.status();
        let text = read_response_text(resp, "send_rich_media_message").await;
        if !status.is_success() {
            anyhow::bail!(
                "QQ {} message failed: HTTP {status} - {text}",
                file_type.label()
            );
        }

        Ok(parse_message_id(&text, "send_rich_media_message"))
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
            warn!(
                target = target.kind_label(),
                status = %status,
                error_body = %text,
                "QQ delete_message returned non-success"
            );
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

/// Computes token cache validity duration with a clamped early-refresh margin so short-TTL
/// tokens (`<= 300s`) still stay cached for half their lifetime instead of expiring immediately.
#[inline]
#[must_use]
pub(crate) fn compute_token_valid_duration(expires_in_secs: u64) -> Duration {
    let margin = TOKEN_REFRESH_MARGIN_SECS.min(expires_in_secs / 2);
    Duration::from_secs(expires_in_secs.saturating_sub(margin))
}

/// Generates a monotonically advancing `msg_seq` in `0..65536`.
#[inline]
#[must_use]
pub(crate) fn next_msg_seq() -> u32 {
    MSG_SEQ_COUNTER.fetch_add(1, Ordering::Relaxed) & 0xFFFF
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

fn parse_message_id(body: &str, context_label: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<MessageResponse>(body) {
        Ok(parsed) => parsed.id,
        Err(err) => {
            warn!(
                context = context_label,
                error = %err,
                body_len = body.len(),
                "Failed to parse QQ MessageResponse JSON"
            );
            None
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
    let mut chunks = bytes.chunks_exact(3);

    for chunk in &mut chunks {
        let packed =
            (usize::from(chunk[0]) << 16) | (usize::from(chunk[1]) << 8) | usize::from(chunk[2]);
        out.extend_from_slice(&[
            ALPHABET[(packed >> 18) & 63],
            ALPHABET[(packed >> 12) & 63],
            ALPHABET[(packed >> 6) & 63],
            ALPHABET[packed & 63],
        ]);
    }

    match chunks.remainder() {
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
}
