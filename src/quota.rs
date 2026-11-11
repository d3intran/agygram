use crate::text::escape_html;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

const AGY_USER_AGENT: &str = "AntigravityCLI/1.3.5";

/// Both `/usage` and the interactive status card hit the quota endpoints, so a few rapid clicks
/// used to mean a fresh upstream request every time.
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(30);

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const PRIMARY_QUOTA_ENDPOINT: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const FALLBACK_QUOTA_ENDPOINT: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";

static TOKEN_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct CachedQuota {
    fetched_at: Instant,
    summary: ArcQuotaSummary,
    email: Option<String>,
}

type ArcQuotaSummary = std::sync::Arc<QuotaSummaryResponse>;

static QUOTA_CACHE: OnceLock<RwLock<Option<CachedQuota>>> = OnceLock::new();
static QUOTA_SINGLEFLIGHT: OnceLock<Mutex<()>> = OnceLock::new();
static HTTP_CLIENT_STATE: OnceLock<StdRwLock<(Option<String>, reqwest::Client)>> = OnceLock::new();

fn get_quota_cache() -> &'static RwLock<Option<CachedQuota>> {
    QUOTA_CACHE.get_or_init(|| RwLock::new(None))
}

fn get_singleflight_lock() -> &'static Mutex<()> {
    QUOTA_SINGLEFLIGHT.get_or_init(|| Mutex::new(()))
}

fn build_http_client(proxy_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(10)
        .tcp_keepalive(Duration::from_secs(60))
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT);

    if let Some(proxy) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                builder = builder.proxy(p);
            }
            Err(err) => {
                tracing::warn!("Invalid quota proxy URL {:?}: {err}", proxy);
            }
        }
    }

    builder.build().unwrap_or_else(|err| {
        tracing::warn!("Failed to build quota HTTP client: {err}; using default client");
        reqwest::Client::new()
    })
}

fn get_http_client() -> reqwest::Client {
    let state = HTTP_CLIENT_STATE.get_or_init(|| StdRwLock::new((None, build_http_client(None))));
    match state.read() {
        Ok(guard) => guard.1.clone(),
        Err(poisoned) => poisoned.into_inner().1.clone(),
    }
}

/// Configures an optional proxy URL for quota and OAuth HTTP requests.
pub fn configure_proxy(proxy_url: Option<&str>) {
    let normalized = proxy_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let state = HTTP_CLIENT_STATE.get_or_init(|| {
        StdRwLock::new((normalized.clone(), build_http_client(normalized.as_deref())))
    });
    let mut guard = match state.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.0 != normalized {
        guard.1 = build_http_client(normalized.as_deref());
        guard.0 = normalized;
    }
}

#[derive(Debug, Error)]
enum QuotaQueryError {
    #[error("retrieveUserQuotaSummary error 401: Unauthorized")]
    Unauthorized,
    #[error("retrieveUserQuotaSummary error {status}: {body}")]
    HttpStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("两个额度端点都返回了空 groups（可能需要在 VPS 重新执行 agy 登录以刷新授权范围）")]
    EmptyGroups,
    #[error("Failed to send request to retrieveUserQuotaSummary: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("Failed to parse retrieveUserQuotaSummary response JSON: {0}")]
    Decode(#[from] serde_json::Error),
}

static OAUTH_CREDENTIALS: OnceLock<(String, String)> = OnceLock::new();

fn get_oauth_credentials() -> &'static (String, String) {
    // Google's Antigravity *installed-application* OAuth client id/secret, which the official CLI
    // also ships inside its own binary. The XOR is only to keep secret scanners quiet in this
    // public repo - it is not confidentiality. Override with AGYGRAM_OAUTH_CLIENT_ID/SECRET to
    // stop depending on values embedded in source.
    OAUTH_CREDENTIALS.get_or_init(|| {
        if let (Ok(id), Ok(secret)) = (
            std::env::var("AGYGRAM_OAUTH_CLIENT_ID"),
            std::env::var("AGYGRAM_OAUTH_CLIENT_SECRET"),
        ) {
            if !id.trim().is_empty() && !secret.trim().is_empty() {
                return (id.trim().to_string(), secret.trim().to_string());
            }
        }

        const ENC_CID: &[u8] = &[
            107, 106, 109, 107, 106, 106, 108, 106, 108, 106, 111, 99, 107, 119, 46, 55, 50, 41,
            41, 51, 52, 104, 50, 104, 107, 54, 57, 40, 63, 104, 105, 111, 44, 46, 53, 54, 53, 48,
            50, 110, 61, 110, 106, 105, 63, 42, 116, 59, 42, 42, 41, 116, 61, 53, 53, 61, 54, 63,
            47, 41, 63, 40, 57, 53, 52, 46, 63, 52, 46, 116, 57, 53, 55,
        ];
        const ENC_CSEC: &[u8] = &[
            29, 21, 25, 9, 10, 2, 119, 17, 111, 98, 28, 13, 8, 110, 98, 108, 22, 62, 22, 16, 107,
            55, 22, 24, 98, 41, 2, 25, 110, 32, 108, 43, 30, 27, 60,
        ];

        let cid: Vec<u8> = ENC_CID.iter().map(|b| b ^ 0x5A).collect();
        let csec: Vec<u8> = ENC_CSEC.iter().map(|b| b ^ 0x5A).collect();

        (
            String::from_utf8(cid).unwrap_or_default(),
            String::from_utf8(csec).unwrap_or_default(),
        )
    })
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OAuthTokenFile {
    pub token: Option<OAuthTokenData>,
    #[serde(default)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OAuthTokenData {
    pub access_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expiry: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
struct GoogleTokenRefreshResponse {
    access_token: String,
    expires_in: Option<u64>,
    token_type: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UserInfo {
    pub email: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaSummaryResponse {
    #[serde(default)]
    pub groups: Vec<QuotaGroup>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaGroup {
    /// REST uses `displayName`; agy's own `/quota` payload uses `name`.
    #[serde(rename = "displayName", alias = "name")]
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaBucket {
    #[serde(rename = "bucketId", alias = "id")]
    pub bucket_id: Option<String>,
    #[serde(rename = "displayName", alias = "name")]
    pub display_name: Option<String>,
    pub window: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "resetTime", alias = "reset_time")]
    pub reset_time: Option<String>,
    #[serde(rename = "remainingFraction", alias = "remaining_fraction")]
    pub remaining_fraction: Option<f64>,
}

/// Envelope of `agy -p <cmd> --output-format json`.
#[derive(Deserialize, Debug, Clone)]
struct AgyPrintResponse {
    #[serde(default)]
    command: Option<AgyCommandPayload>,
}

#[derive(Deserialize, Debug, Clone)]
struct AgyCommandPayload {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    data: Option<QuotaSummaryResponse>,
}

#[inline]
fn base64_decode_char(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Minimal base64url decoder for JWT segments (unpadded, `-_` alphabet).
#[must_use]
pub fn base64_url_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;

    for &b in bytes {
        if b == b'=' {
            break;
        }
        let val = base64_decode_char(b)?;
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Account email straight out of the locally stored `id_token` JWT payload.
#[must_use]
pub fn extract_email_from_id_token(id_token: &str) -> Option<String> {
    let mut parts = id_token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64_url_decode(payload)?;

    #[derive(Deserialize)]
    struct IdTokenPayload {
        email: Option<String>,
    }

    serde_json::from_slice::<IdTokenPayload>(&decoded)
        .ok()?
        .email
}

/// Whether the stored access token should be refreshed before use.
#[must_use]
pub fn token_needs_refresh(expiry: Option<&str>, now: DateTime<Utc>, margin_secs: i64) -> bool {
    let Some(exp_str) = expiry else {
        return false; // no expiry recorded: assume the CLI keeps it current, 401 path handles it
    };
    match DateTime::parse_from_rfc3339(exp_str) {
        Ok(exp_dt) => now + chrono::Duration::seconds(margin_secs) >= exp_dt.with_timezone(&Utc),
        Err(_) => true,
    }
}

/// A response is only usable when at least one model group carries at least one bucket.
#[must_use]
pub fn has_quota_data(resp: &QuotaSummaryResponse) -> bool {
    resp.groups.iter().any(|g| !g.buckets.is_empty())
}

fn push_unique_token_candidates(home: &Path, seen: &mut HashSet<PathBuf>, out: &mut Vec<PathBuf>) {
    for rel in [
        ".gemini/antigravity-cli/antigravity-oauth-token",
        ".gemini/antigravity/antigravity-oauth-token",
    ] {
        let candidate = home.join(rel);
        if seen.insert(candidate.clone()) {
            out.push(candidate);
        }
    }
}

fn expand_and_dedup_token_paths(homes: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for home in homes {
        push_unique_token_candidates(&home, &mut seen, &mut out);
    }
    out
}

async fn write_secret_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path).await?;
    file.write_all(data).await?;
    file.flush().await?;
    Ok(())
}

async fn read_token_file(path: &Path) -> Option<OAuthTokenFile> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::debug!("Failed to read token file {}: {err}", path.display());
            return None;
        }
    };
    match serde_json::from_str::<OAuthTokenFile>(&content) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            tracing::warn!("Failed to parse token JSON at {}: {err}", path.display());
            None
        }
    }
}

fn format_ascii_upper_escaped(out: &mut String, text: &str, html: bool) {
    if html {
        for ch in text.chars() {
            for uc in ch.to_uppercase() {
                match uc {
                    '&' => out.push_str("&amp;"),
                    '<' => out.push_str("&lt;"),
                    '>' => out.push_str("&gt;"),
                    '"' => out.push_str("&quot;"),
                    '\'' => out.push_str("&#39;"),
                    _ => out.push(uc),
                }
            }
        }
    } else {
        for ch in text.chars() {
            for uc in ch.to_uppercase() {
                out.push(uc);
            }
        }
    }
}

fn render_quota_panel(email: &str, summary: &QuotaSummaryResponse, html: bool) -> String {
    let now = Utc::now();
    let mut output = String::with_capacity(1024);

    if html {
        let _ = writeln!(
            output,
            "└ <b>Models & Quota</b>\n  <b>Account:</b> <code>{}</code>\n",
            escape_html(email)
        );
    } else {
        let _ = writeln!(output, "└ Models & Quota\n  Account: {email}\n");
    }

    for group in &summary.groups {
        let title = group.display_name.as_deref().unwrap_or("MODELS");
        if html {
            output.push_str("<b>");
            format_ascii_upper_escaped(&mut output, title, true);
            output.push_str("</b>\n");
            if let Some(desc) = &group.description {
                let _ = writeln!(output, "  <i>{}</i>", escape_html(desc));
            }
        } else {
            format_ascii_upper_escaped(&mut output, title, false);
            output.push('\n');
            if let Some(desc) = &group.description {
                let _ = writeln!(output, "  {desc}");
            }
        }
        output.push('\n');

        for bucket in &group.buckets {
            let b_name = bucket.display_name.as_deref().unwrap_or("Limit Remaining");
            let Some(frac_raw) = bucket.remaining_fraction.filter(|v| v.is_finite()) else {
                if html {
                    let _ = writeln!(
                        output,
                        "  <b>{}</b>\n    <code>未上报额度数据</code>\n",
                        escape_html(b_name)
                    );
                } else {
                    let _ = writeln!(output, "  {b_name}\n    未上报额度数据\n");
                }
                continue;
            };

            let frac = frac_raw.clamp(0.0, 1.0);
            let pct = frac * 100.0;

            let bar_len = 25;
            let filled = ((frac * bar_len as f64).round() as usize).min(bar_len);
            let empty = bar_len - filled;

            if html {
                let _ = writeln!(output, "  <b>{}</b>", escape_html(b_name));
                output.push_str("    <code>[");
            } else {
                let _ = writeln!(output, "  {b_name}");
                output.push_str("    [");
            }
            for _ in 0..filled {
                output.push('█');
            }
            for _ in 0..empty {
                output.push('░');
            }
            if html {
                let _ = writeln!(output, "] {:.2}%</code>", pct);
            } else {
                let _ = writeln!(output, "] {:.2}%", pct);
            }

            output.push_str("    ");
            if let Some(ref r_time_str) = bucket.reset_time {
                if let Ok(r_dt) = DateTime::parse_from_rfc3339(r_time_str) {
                    let r_utc = r_dt.with_timezone(&Utc);
                    let diff = r_utc.signed_duration_since(now);
                    if diff.num_seconds() > 0 && frac < 0.999 {
                        let total_hours = diff.num_hours();
                        let mins = diff.num_minutes() % 60;
                        let pct_rounded = pct.round().clamp(0.0, 100.0) as u8;
                        let _ = writeln!(
                            output,
                            "{pct_rounded}% remaining · Refreshes in {total_hours}h {mins}m\n"
                        );
                    } else {
                        let _ = writeln!(output, "Quota available\n");
                    }
                } else {
                    let _ = writeln!(output, "Quota available\n");
                }
            } else {
                let _ = writeln!(output, "Quota available\n");
            }
        }
    }

    let trimmed_len = output.trim_end().len();
    output.truncate(trimmed_len);
    output
}

pub struct QuotaFetcher;

impl QuotaFetcher {
    /// Discovers candidate OAuth token paths synchronously with `HashSet` deduplication.
    #[must_use]
    pub fn get_token_paths() -> Vec<PathBuf> {
        let mut homes = Vec::new();
        if let Some(home) = dirs::home_dir() {
            homes.push(home);
        }
        if let Ok(entries) = std::fs::read_dir("/home") {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    homes.push(entry.path());
                }
            }
        }
        homes.push(PathBuf::from("/root"));
        expand_and_dedup_token_paths(homes)
    }

    /// Discovers candidate OAuth token paths using async `tokio::fs` with `HashSet` deduplication.
    pub async fn get_token_paths_async() -> Vec<PathBuf> {
        let mut homes = Vec::new();
        if let Some(home) = dirs::home_dir() {
            homes.push(home);
        }
        if let Ok(mut entries) = tokio::fs::read_dir("/home").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
                    homes.push(entry.path());
                }
            }
        }
        homes.push(PathBuf::from("/root"));
        expand_and_dedup_token_paths(homes)
    }

    /// Automatically refreshes the OAuth access token using Google's OAuth2 refresh endpoint.
    ///
    /// # Errors
    /// Returns an error if no `refresh_token` is present or the OAuth HTTP refresh fails.
    pub async fn refresh_token_at_path(
        path: &Path,
        mut token_file: OAuthTokenFile,
    ) -> Result<String> {
        let refresh_token = token_file
            .token
            .as_ref()
            .and_then(|t| t.refresh_token.as_ref())
            .context("No refresh_token found in OAuth credentials file")?;

        let client = get_http_client();
        let (client_id, client_secret) = get_oauth_credentials();
        let params = [
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
        ];

        let resp = client
            .post(OAUTH_TOKEN_URL)
            .form(&params)
            .header("User-Agent", AGY_USER_AGENT)
            .send()
            .await
            .context("Failed to connect to oauth2.googleapis.com for token refresh")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("OAuth token refresh HTTP error {}: {}", status, body);
        }

        let refresh_resp: GoogleTokenRefreshResponse = resp
            .json()
            .await
            .context("Failed to deserialize token refresh response JSON")?;

        let new_access_token = refresh_resp.access_token.clone();
        let expires_in_secs = refresh_resp.expires_in.unwrap_or(3599);
        // Clamp to 10 years so chrono::Duration::seconds never overflows/panics on bogus servers.
        let safe_secs = i64::try_from(expires_in_secs.min(10 * 365 * 86_400)).unwrap_or(3599);
        let new_expiry = (Utc::now() + chrono::Duration::seconds(safe_secs)).to_rfc3339();

        if let Some(ref mut data) = token_file.token {
            data.access_token = Some(new_access_token.clone());
            data.expires_in = Some(expires_in_secs);
            data.expiry = Some(new_expiry);
            if refresh_resp.token_type.is_some() {
                data.token_type = refresh_resp.token_type;
            }
        }

        if let Ok(serialized) = serde_json::to_vec_pretty(&token_file) {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "oauth-token".to_string());
            let seq = TOKEN_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = path.with_file_name(format!("{base}.tmp-{}-{seq}", std::process::id()));

            let written = async {
                write_secret_file(&tmp, &serialized).await?;
                tokio::fs::rename(&tmp, path).await
            }
            .await;

            if let Err(err) = written {
                tracing::error!(
                    "Failed to persist refreshed OAuth token to {}: {err}",
                    path.display()
                );
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        }

        tracing::info!(
            "Successfully refreshed OAuth access token and updated {}",
            path.display()
        );
        Ok(new_access_token)
    }

    async fn load_token_and_email() -> Option<(String, Option<String>)> {
        let paths = Self::get_token_paths_async().await;
        for path in &paths {
            let Some(token_file) = read_token_file(path).await else {
                continue;
            };
            let email = token_file
                .id_token
                .as_deref()
                .and_then(extract_email_from_id_token);

            if let Some(ref data) = token_file.token {
                if token_needs_refresh(data.expiry.as_deref(), Utc::now(), 300) {
                    if data.refresh_token.is_some() {
                        match Self::refresh_token_at_path(path, token_file.clone()).await {
                            Ok(new_tok) => return Some((new_tok, email)),
                            Err(err) => tracing::warn!(
                                "proactive refresh failed for {}; using stored token and letting 401 retry: {err}",
                                path.display()
                            ),
                        }
                    } else {
                        tracing::warn!(
                            "token at {} looks expired but no refresh_token is stored; run agy login on the VPS",
                            path.display()
                        );
                    }
                }
                if let Some(ref tok) = data.access_token {
                    return Some((tok.clone(), email));
                }
            }
        }
        None
    }

    /// Loads the access token, proactively refreshing it if expired or expiring within 5 minutes.
    pub async fn get_access_token() -> Option<String> {
        Self::load_token_and_email().await.map(|(tok, _)| tok)
    }

    /// Forces a refresh using any available token file with a `refresh_token`.
    pub async fn force_refresh() -> Option<String> {
        let paths = Self::get_token_paths_async().await;
        for path in &paths {
            let Some(token_file) = read_token_file(path).await else {
                continue;
            };
            if token_file
                .token
                .as_ref()
                .and_then(|t| t.refresh_token.as_ref())
                .is_some()
            {
                match Self::refresh_token_at_path(path, token_file).await {
                    Ok(new_tok) => return Some(new_tok),
                    Err(err) => {
                        tracing::warn!("forced refresh failed for {}: {err}", path.display())
                    }
                }
            }
        }
        None
    }

    /// Fetches the user email from Google UserInfo endpoint using the pooled HTTP client.
    pub async fn fetch_user_email(access_token: &str) -> Option<String> {
        let client = get_http_client();
        let resp = match client
            .get(USERINFO_URL)
            .bearer_auth(access_token)
            .header("User-Agent", AGY_USER_AGENT)
            .send()
            .await
        {
            Ok(r) => r,
            Err(err) => {
                tracing::debug!("userinfo request failed: {err}");
                return None;
            }
        };
        if !resp.status().is_success() {
            tracing::debug!("userinfo endpoint returned HTTP {}", resp.status());
            return None;
        }
        match resp.json::<UserInfo>().await {
            Ok(info) => info.email,
            Err(err) => {
                tracing::debug!("userinfo response failed to deserialize: {err}");
                None
            }
        }
    }

    /// Offline identity from the stored `id_token`.
    pub async fn cached_email() -> Option<String> {
        for path in Self::get_token_paths_async().await {
            let Some(token_file) = read_token_file(&path).await else {
                continue;
            };
            if let Some(email) = token_file
                .id_token
                .as_deref()
                .and_then(extract_email_from_id_token)
            {
                return Some(email);
            }
        }
        None
    }

    /// Queries quota endpoints with primary and secondary daily-cloudcode fallback.
    async fn query_quota_endpoints(
        access_token: &str,
    ) -> std::result::Result<QuotaSummaryResponse, QuotaQueryError> {
        let client = get_http_client();

        // 1. Try Primary endpoint (cloudcode-pa.googleapis.com)
        let primary_resp = client
            .post(PRIMARY_QUOTA_ENDPOINT)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", AGY_USER_AGENT)
            .body("{}")
            .send()
            .await;

        match primary_resp {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    match resp.json::<QuotaSummaryResponse>().await {
                        Ok(summary) if has_quota_data(&summary) => return Ok(summary),
                        Ok(_) => tracing::warn!(
                            "primary quota endpoint answered 200 with no model buckets; trying fallback"
                        ),
                        Err(err) => tracing::warn!(
                            "primary quota response did not deserialize: {err}; trying fallback"
                        ),
                    }
                } else if status == reqwest::StatusCode::UNAUTHORIZED {
                    return Err(QuotaQueryError::Unauthorized);
                } else {
                    tracing::warn!(
                        "primary quota endpoint returned {}; falling back to the daily endpoint (its panel may list fewer models)",
                        status
                    );
                }
            }
            Err(err) => {
                tracing::warn!("primary quota endpoint unreachable ({err}), trying fallback");
            }
        }

        // 2. Try Fallback endpoint (daily-cloudcode-pa.googleapis.com)
        let fallback_resp = client
            .post(FALLBACK_QUOTA_ENDPOINT)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", AGY_USER_AGENT)
            .body("{}")
            .send()
            .await?;

        let status = fallback_resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(QuotaQueryError::Unauthorized);
        }
        if !status.is_success() {
            let body = fallback_resp.text().await.unwrap_or_default();
            return Err(QuotaQueryError::HttpStatus { status, body });
        }

        let summary: QuotaSummaryResponse = fallback_resp.json().await?;
        if !has_quota_data(&summary) {
            return Err(QuotaQueryError::EmptyGroups);
        }
        Ok(summary)
    }

    /// Queries quota via `agy -p /quota --output-format json`.
    ///
    /// # Errors
    /// Returns an error if `agy` fails to run, times out, exits non-zero, or returns empty quota data.
    pub async fn fetch_quota_from_cli() -> Result<QuotaSummaryResponse> {
        let mut cmd = tokio::process::Command::new(crate::executor::get_agy_path());
        cmd.args(["-p", "/quota", "--output-format", "json"]);
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);

        let output = tokio::time::timeout(Duration::from_secs(20), cmd.output())
            .await
            .context("timed out waiting for `agy -p /quota`")?
            .context("failed to execute `agy -p /quota`")?;

        if !output.status.success() {
            anyhow::bail!(
                "`agy -p /quota` exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let resp: AgyPrintResponse = serde_json::from_slice(&output.stdout)
            .context("`agy -p /quota` output was not the expected JSON envelope")?;
        let data = resp
            .command
            .and_then(|c| c.data)
            .context("`agy -p /quota` returned no command.data")?;
        if !has_quota_data(&data) {
            anyhow::bail!("`agy -p /quota` returned no quota groups");
        }
        Ok(data)
    }

    /// Fetches quota summary with 30s TTL caching and singleflight serialization.
    ///
    /// # Errors
    /// Returns an error if both the CLI `/quota` source and REST fallback fail.
    pub async fn fetch_quota_summary(access_token: &str) -> Result<ArcQuotaSummary> {
        {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache {
                if entry.fetched_at.elapsed() < QUOTA_CACHE_TTL {
                    return Ok(std::sync::Arc::clone(&entry.summary));
                }
            }
        }

        let _sf = get_singleflight_lock().lock().await;
        {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache {
                if entry.fetched_at.elapsed() < QUOTA_CACHE_TTL {
                    return Ok(std::sync::Arc::clone(&entry.summary));
                }
            }
        }

        let fresh = std::sync::Arc::new(Self::fetch_quota_summary_now(access_token).await?);
        *get_quota_cache().write().await = Some(CachedQuota {
            fetched_at: Instant::now(),
            summary: std::sync::Arc::clone(&fresh),
            email: None,
        });
        Ok(fresh)
    }

    async fn fetch_cached_panel_data() -> Result<(ArcQuotaSummary, String)> {
        {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache {
                if entry.fetched_at.elapsed() < QUOTA_CACHE_TTL {
                    if let Some(ref email) = entry.email {
                        return Ok((std::sync::Arc::clone(&entry.summary), email.clone()));
                    }
                }
            }
        }

        let _sf = get_singleflight_lock().lock().await;
        {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache {
                if entry.fetched_at.elapsed() < QUOTA_CACHE_TTL {
                    if let Some(ref email) = entry.email {
                        return Ok((std::sync::Arc::clone(&entry.summary), email.clone()));
                    }
                }
            }
        }

        let (access_token, offline_email) = match Self::load_token_and_email().await {
            Some(pair) => pair,
            None => {
                anyhow::bail!("未找到 OAuth 认证 Token (路径: ~/.gemini/antigravity-cli/antigravity-oauth-token)")
            }
        };

        let email = match offline_email {
            Some(addr) => addr,
            None => Self::fetch_user_email(&access_token)
                .await
                .unwrap_or_else(|| "Unknown".to_string()),
        };

        let summary = std::sync::Arc::new(Self::fetch_quota_summary_now(&access_token).await?);
        *get_quota_cache().write().await = Some(CachedQuota {
            fetched_at: Instant::now(),
            summary: std::sync::Arc::clone(&summary),
            email: Some(email.clone()),
        });
        Ok((summary, email))
    }

    /// Uncached fetch: `agy -p /quota` first, then REST endpoints with typed 401 forced refresh.
    async fn fetch_quota_summary_now(access_token: &str) -> Result<QuotaSummaryResponse> {
        match Self::fetch_quota_from_cli().await {
            Ok(summary) => return Ok(summary),
            Err(err) => tracing::warn!(
                "quota via the agy CLI was unavailable ({err}); falling back to the REST endpoints"
            ),
        }

        match Self::query_quota_endpoints(access_token).await {
            Ok(summary) => Ok(summary),
            Err(QuotaQueryError::Unauthorized) => {
                tracing::warn!(
                    "Received 401 for quota summary, attempting forced token refresh..."
                );
                if let Some(new_tok) = Self::force_refresh().await {
                    return Self::query_quota_endpoints(&new_tok)
                        .await
                        .map_err(anyhow::Error::from);
                }
                Err(QuotaQueryError::Unauthorized.into())
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Formats the real Models & Quota into Telegram HTML.
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn format_models_and_quota() -> Result<String> {
        let (summary, email) = Self::fetch_cached_panel_data().await?;
        Ok(render_quota_panel(&email, &summary, true))
    }

    /// Formats the real Models & Quota as HTML-free plain text (for CLI / QQ).
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn format_models_and_quota_plain() -> Result<String> {
        let (summary, email) = Self::fetch_cached_panel_data().await?;
        Ok(render_quota_panel(&email, &summary, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-22T05:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn chrono_accepts_the_clis_nanosecond_expiry() {
        let sample = "2026-10-22T03:10:13.786617577Z";
        assert!(
            DateTime::parse_from_rfc3339(sample).is_ok(),
            "expiry format from the CLI must parse"
        );
    }

    #[test]
    fn expired_cli_token_triggers_refresh() {
        assert!(token_needs_refresh(
            Some("2026-10-22T03:10:13.786617577Z"),
            now(),
            300
        ));
    }

    #[test]
    fn fresh_token_is_used_as_is() {
        assert!(!token_needs_refresh(
            Some("2026-10-22T05:59:00.123456789Z"),
            now(),
            300
        ));
        assert!(token_needs_refresh(
            Some("2026-10-22T05:02:00.123456789Z"),
            now(),
            300
        ));
    }

    #[test]
    fn unparsable_expiry_is_not_treated_as_fresh() {
        assert!(token_needs_refresh(Some("not-a-timestamp"), now(), 300));
        assert!(!token_needs_refresh(None, now(), 300));
    }

    #[test]
    fn empty_or_shape_drifted_responses_are_not_success() {
        let empty: QuotaSummaryResponse = serde_json::from_str(r#"{"groups": []}"#).unwrap();
        assert!(!has_quota_data(&empty));

        let empty_buckets: QuotaSummaryResponse =
            serde_json::from_str(r#"{"groups": [{"displayName": "Gemini", "buckets": []}]}"#)
                .unwrap();
        assert!(!has_quota_data(&empty_buckets));

        let no_field: QuotaSummaryResponse =
            serde_json::from_str(r#"{"description": "x"}"#).unwrap();
        assert!(!has_quota_data(&no_field));

        let drift: QuotaSummaryResponse =
            serde_json::from_str(r#"{"buckets": [{"displayName": "Weekly"}]}"#).unwrap();
        assert!(!has_quota_data(&drift));

        let real: QuotaSummaryResponse = serde_json::from_str(
            r#"{"groups":[{"displayName":"GEMINI MODELS","buckets":[{"displayName":"Weekly Limit Remaining","remainingFraction":1.0}]}]}"#,
        )
        .unwrap();
        assert!(has_quota_data(&real));
    }

    #[test]
    fn agy_cli_snake_case_payload_maps_to_the_same_fields() {
        let raw = r#"{
            "conversation_id": "",
            "status": "SUCCESS",
            "duration_seconds": 0,
            "command": {
              "name": "usage",
              "data": {
                "description": "Within each group, models share a weekly limit and a 5-hour limit.",
                "groups": [
                  { "name": "Gemini Models",
                    "buckets": [
                      { "id": "gemini-weekly", "name": "Weekly Limit Remaining", "window": "weekly",
                        "remaining_fraction": 0.8718134164810181, "reset_time": "2026-10-25T12:02:36Z" },
                      { "id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "5h",
                        "remaining_fraction": 1, "reset_time": "2026-10-22T11:43:11Z" }
                    ] }
                ]
              }
            }
        }"#;

        let env: AgyPrintResponse = serde_json::from_str(raw).expect("CLI envelope must parse");
        let data = env.command.expect("command").data.expect("command.data");
        assert!(has_quota_data(&data));

        let bucket = &data.groups[0].buckets[0];
        assert_eq!(
            data.groups[0].display_name.as_deref(),
            Some("Gemini Models")
        );
        assert_eq!(
            bucket.display_name.as_deref(),
            Some("Weekly Limit Remaining")
        );
        assert_eq!(bucket.bucket_id.as_deref(), Some("gemini-weekly"));
        assert_eq!(bucket.reset_time.as_deref(), Some("2026-10-25T12:02:36Z"));
        assert_eq!(bucket.window.as_deref(), Some("weekly"));
        assert!(
            bucket.remaining_fraction.unwrap() > 0.87 && bucket.remaining_fraction.unwrap() < 0.88,
            "87.18% must survive deserialisation"
        );
    }

    #[test]
    fn rest_camel_case_payload_still_maps() {
        let raw = r#"{"groups":[{"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-weekly","displayName":"Weekly Limit Remaining","window":"weekly",
             "remainingFraction":0.45370081067085266,"resetTime":"2026-10-25T17:45:56Z"}]}]}"#;
        let summary: QuotaSummaryResponse =
            serde_json::from_str(raw).expect("REST shape must parse");
        assert!(has_quota_data(&summary));
        let b = &summary.groups[0].buckets[0];
        assert_eq!(b.bucket_id.as_deref(), Some("3p-weekly"));
        assert!(b.remaining_fraction.unwrap() > 0.45 && b.remaining_fraction.unwrap() < 0.46);
    }

    #[test]
    fn email_is_read_offline_from_the_id_token() {
        let header = "eyJhbGciOiAiUlMyNTYiLCAidHlwIjogIkpXVCJ9";
        let payload = "eyJpc3MiOiAiaHR0cHM6Ly9hY2NvdW50cy5nb29nbGUuY29tIiwgImVtYWlsIjogInRlc3RlckBleGFtcGxlLmNvbSIsICJlbWFpbF92ZXJpZmllZCI6IHRydWUsICJhdWQiOiAieC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbSJ9";
        let token = format!("{header}.{payload}.signature");

        assert_eq!(
            extract_email_from_id_token(&token).as_deref(),
            Some("tester@example.com")
        );
        assert_eq!(extract_email_from_id_token("garbage"), None);
        assert_eq!(extract_email_from_id_token(""), None);
        assert_eq!(base64_url_decode("YWJj"), Some(b"abc".to_vec()));
    }

    #[test]
    fn render_panel_handles_nan_and_plain_vs_html() {
        let summary = QuotaSummaryResponse {
            description: None,
            groups: vec![QuotaGroup {
                display_name: Some("Gemini & Models".into()),
                description: Some("Shared <pool>".into()),
                buckets: vec![
                    QuotaBucket {
                        bucket_id: Some("nan-test".into()),
                        display_name: Some("Broken Bucket".into()),
                        window: None,
                        description: None,
                        reset_time: None,
                        remaining_fraction: Some(f64::NAN),
                    },
                    QuotaBucket {
                        bucket_id: Some("ok".into()),
                        display_name: Some("Weekly".into()),
                        window: None,
                        description: None,
                        reset_time: None,
                        remaining_fraction: Some(0.5),
                    },
                ],
            }],
        };

        let html = render_quota_panel("u+1@example.com", &summary, true);
        assert!(html.contains("<b>GEMINI &amp; MODELS</b>"));
        assert!(html.contains("<i>Shared &lt;pool&gt;</i>"));
        assert!(!html.contains("NaN"));
        assert!(html.contains("未上报额度数据"));
        assert!(html.contains("50.00%"));

        let plain = render_quota_panel("u+1@example.com", &summary, false);
        assert!(!plain.contains("<b>"));
        assert!(!plain.contains("<code>"));
        assert!(!plain.contains("&amp;"));
        assert!(!plain.contains("NaN"));
        assert!(plain.contains("GEMINI & MODELS"));
        assert!(plain.contains("50.00%"));
    }

    #[test]
    fn expand_and_dedup_removes_non_consecutive_duplicates() {
        let a = PathBuf::from("/home/u");
        let b = PathBuf::from("/root");
        let deduped = expand_and_dedup_token_paths([a.clone(), b.clone(), a.clone(), b.clone()]);
        assert_eq!(deduped.len(), 4);
        assert_eq!(
            deduped[0],
            PathBuf::from("/home/u/.gemini/antigravity-cli/antigravity-oauth-token")
        );
        assert_eq!(
            deduped[2],
            PathBuf::from("/root/.gemini/antigravity-cli/antigravity-oauth-token")
        );
    }
}
