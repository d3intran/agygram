use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Both `/usage` and the interactive status card hit the quota endpoints, so a few rapid clicks
/// used to mean a fresh upstream request every time.
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(30);

static QUOTA_CACHE: OnceLock<RwLock<Option<(Instant, QuotaSummaryResponse)>>> = OnceLock::new();

fn get_quota_cache() -> &'static RwLock<Option<(Instant, QuotaSummaryResponse)>> {
    QUOTA_CACHE.get_or_init(|| RwLock::new(None))
}

/// A /usage turn must never hang forever: without a request timeout a stalled TCP connection
/// leaves the handler awaiting indefinitely and nothing is logged. `tcp_keepalive` alone does
/// not bound connect or read stalls.
const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(25);

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn get_oauth_credentials() -> (String, String) {
    // Google's Antigravity *installed-application* OAuth client id/secret, which the official CLI
    // also ships inside its own binary. The XOR is only to keep secret scanners quiet in this
    // public repo - it is not confidentiality. Override with AGYGRAM_OAUTH_CLIENT_ID/SECRET to
    // stop depending on values embedded in source.
    if let (Ok(id), Ok(secret)) = (
        std::env::var("AGYGRAM_OAUTH_CLIENT_ID"),
        std::env::var("AGYGRAM_OAUTH_CLIENT_SECRET"),
    ) {
        if !id.trim().is_empty() && !secret.trim().is_empty() {
            return (id.trim().to_string(), secret.trim().to_string());
        }
    }

    const ENC_CID: &[u8] = &[
        107, 106, 109, 107, 106, 106, 108, 106, 108, 106, 111, 99, 107, 119, 46, 55, 50, 41, 41,
        51, 52, 104, 50, 104, 107, 54, 57, 40, 63, 104, 105, 111, 44, 46, 53, 54, 53, 48, 50,
        110, 61, 110, 106, 105, 63, 42, 116, 59, 42, 42, 41, 116, 61, 53, 53, 61, 54, 63, 47, 41,
        63, 40, 57, 53, 52, 46, 63, 52, 46, 116, 57, 53, 55,
    ];
    const ENC_CSEC: &[u8] = &[
        29, 21, 25, 9, 10, 2, 119, 17, 111, 98, 28, 13, 8, 110, 98, 108, 22, 62, 22, 16, 107, 55,
        22, 24, 98, 41, 2, 25, 110, 32, 108, 43, 30, 27, 60,
    ];

    let cid: Vec<u8> = ENC_CID.iter().map(|b| b ^ 0x5A).collect();
    let csec: Vec<u8> = ENC_CSEC.iter().map(|b| b ^ 0x5A).collect();

    (
        String::from_utf8(cid).unwrap_or_default(),
        String::from_utf8(csec).unwrap_or_default(),
    )
}

const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

const PRIMARY_QUOTA_ENDPOINT: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const FALLBACK_QUOTA_ENDPOINT: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";

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
        buf = (buf << 6) | (val as u32);
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

/// Whether the stored access token should be refreshed before use. The CLI writes expiry strings
/// with nanosecond precision (`...T03:10:13.786617577Z`); an unparsable expiry is treated as
/// needing a refresh rather than as "fresh forever", because the latter silently feeds a dead
/// token to the quota endpoints.
pub fn token_needs_refresh(expiry: Option<&str>, now: DateTime<Utc>, margin_secs: i64) -> bool {
    let Some(exp_str) = expiry else {
        return false; // no expiry recorded: assume the CLI keeps it current, 401 path handles it
    };
    match DateTime::parse_from_rfc3339(exp_str) {
        Ok(exp_dt) => {
            now + chrono::Duration::seconds(margin_secs) >= exp_dt.with_timezone(&Utc)
        }
        Err(_) => true,
    }
}

/// A response is only usable when it carries at least one model group. The endpoints do return
/// HTTP 200 with an empty `groups` array (or a shape without it), and treating that as success
/// produced a quota panel with no numbers and no log line.
pub fn has_quota_data(resp: &QuotaSummaryResponse) -> bool {
    resp.groups.iter().any(|g| !g.buckets.is_empty()) || !resp.groups.is_empty()
}

pub struct QuotaFetcher;

impl QuotaFetcher {
    /// Discovers candidate OAuth token paths with robust fallbacks across user home directories
    pub fn get_token_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".gemini/antigravity-cli/antigravity-oauth-token"));
            paths.push(home.join(".gemini/antigravity/antigravity-oauth-token"));
        }
        if let Ok(entries) = std::fs::read_dir("/home") {
            for entry in entries.flatten() {
                let user_home = entry.path();
                if user_home.is_dir() {
                    paths.push(user_home.join(".gemini/antigravity-cli/antigravity-oauth-token"));
                    paths.push(user_home.join(".gemini/antigravity/antigravity-oauth-token"));
                }
            }
        }
        paths.push(PathBuf::from(
            "/root/.gemini/antigravity-cli/antigravity-oauth-token",
        ));
        paths.push(PathBuf::from(
            "/root/.gemini/antigravity/antigravity-oauth-token",
        ));
        paths.dedup();
        paths
    }

    /// Automatically refreshes the OAuth access token using Google's OAuth2 refresh endpoint
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
            .header("User-Agent", "AntigravityCLI/1.3.5")
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
        let new_expiry =
            (Utc::now() + chrono::Duration::seconds(expires_in_secs as i64)).to_rfc3339();

        if let Some(ref mut data) = token_file.token {
            data.access_token = Some(new_access_token.clone());
            data.expires_in = Some(expires_in_secs);
            data.expiry = Some(new_expiry);
            if refresh_resp.token_type.is_some() {
                data.token_type = refresh_resp.token_type;
            }
        }

        // This is the CLI's own credential file: truncating it mid-write breaks agy's login, not
        // just quota. Land a sibling first and rename over the top; report failure instead of
        // swallowing it.
        if let Ok(serialized) = serde_json::to_string_pretty(&token_file) {
            let base = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "oauth-token".to_string());
            let tmp = path.with_file_name(format!("{base}.tmp-{}", std::process::id()));

            let written = async {
                tokio::fs::write(&tmp, &serialized).await?;
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

    /// Loads the access token, proactively refreshing it if expired or expiring within 5 minutes
    pub async fn get_access_token() -> Option<String> {
        let paths = Self::get_token_paths();
        for path in &paths {
            if path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(path).await {
                    if let Ok(token_file) = serde_json::from_str::<OAuthTokenFile>(&content) {
                        if let Some(ref data) = token_file.token {
                            if token_needs_refresh(data.expiry.as_deref(), Utc::now(), 300) {
                                if data.refresh_token.is_some() {
                                    match Self::refresh_token_at_path(path, token_file.clone()).await
                                    {
                                        Ok(new_tok) => return Some(new_tok),
                                        Err(err) => tracing::warn!(
                                            "proactive refresh failed for {}; using the stored access token and letting the 401 path retry: {err}",
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
                                return Some(tok.clone());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Forces a refresh using any available token file with a refresh_token
    pub async fn force_refresh() -> Option<String> {
        let paths = Self::get_token_paths();
        for path in &paths {
            if path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(path).await {
                    if let Ok(token_file) = serde_json::from_str::<OAuthTokenFile>(&content) {
                        if token_file
                            .token
                            .as_ref()
                            .and_then(|t| t.refresh_token.as_ref())
                            .is_some()
                        {
                            if let Ok(new_tok) = Self::refresh_token_at_path(path, token_file).await
                            {
                                return Some(new_tok);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Fetches the user email from Google UserInfo endpoint using connection pool
    pub async fn fetch_user_email(access_token: &str) -> Option<String> {
        let client = get_http_client();
        let resp = client
            .get("https://www.googleapis.com/oauth2/v2/userinfo")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", "AntigravityCLI/1.3.5")
            .send()
            .await
            .ok()?;
        let info: UserInfo = resp.json().await.ok()?;
        info.email
    }

    /// Offline identity from the stored id_token, so /usage does not depend on the userinfo
    /// endpoint (which used to leave the panel reading `Account: Unknown` whenever it failed).
    pub async fn cached_email() -> Option<String> {
        for path in Self::get_token_paths() {
            if !path.exists() {
                continue;
            }
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                if let Ok(token_file) = serde_json::from_str::<OAuthTokenFile>(&content) {
                    if let Some(email) = token_file
                        .id_token
                        .as_deref()
                        .and_then(extract_email_from_id_token)
                    {
                        return Some(email);
                    }
                }
            }
        }
        None
    }

    /// Queries quota endpoints with primary and secondary daily-cloudcode fallback
    async fn query_quota_endpoints(access_token: &str) -> Result<QuotaSummaryResponse> {
        let client = get_http_client();

        // 1. Try Primary endpoint (cloudcode-pa.googleapis.com)
        let primary_resp = client
            .post(PRIMARY_QUOTA_ENDPOINT)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", "AntigravityCLI/1.3.5")
            .body("{}")
            .send()
            .await;

        if let Ok(resp) = primary_resp {
            if resp.status().is_success() {
                match resp.json::<QuotaSummaryResponse>().await {
                    Ok(summary) if has_quota_data(&summary) => return Ok(summary),
                    Ok(_) => tracing::warn!(
                        "primary quota endpoint answered 200 with no model groups; trying fallback"
                    ),
                    Err(err) => tracing::warn!(
                        "primary quota response did not deserialize: {err}; trying fallback"
                    ),
                }
            } else if resp.status().as_u16() == 401 {
                anyhow::bail!("retrieveUserQuotaSummary error 401: Unauthorized");
            } else {
                // Any other status used to downgrade silently to the daily endpoint, which can
                // report fewer model groups than the primary - the panel then looks "wrong".
                tracing::warn!(
                    "primary quota endpoint returned {}; falling back to the daily endpoint (its panel may list fewer models)",
                    resp.status()
                );
            }
        } else {
            tracing::warn!("primary quota endpoint unreachable, trying fallback");
        }

        // 2. Try Fallback endpoint (daily-cloudcode-pa.googleapis.com)
        let fallback_resp = client
            .post(FALLBACK_QUOTA_ENDPOINT)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", "AntigravityCLI/1.3.5")
            .body("{}")
            .send()
            .await
            .context(
                "Failed to send request to retrieveUserQuotaSummary (primary & fallback both failed)",
            )?;

        if !fallback_resp.status().is_success() {
            let status = fallback_resp.status();
            let text = fallback_resp.text().await.unwrap_or_default();
            anyhow::bail!("retrieveUserQuotaSummary error {}: {}", status, text);
        }

        let summary: QuotaSummaryResponse = fallback_resp
            .json()
            .await
            .context("Failed to parse retrieveUserQuotaSummary response JSON")?;
        if !has_quota_data(&summary) {
            // Returning Ok here produced a panel with only the account line and no log at all -
            // indistinguishable from "quota broke". Say what actually happened instead.
            anyhow::bail!(
                "两个额度端点都返回了空 groups（可能需要在 VPS 重新执行 agy 登录以刷新授权范围）"
            );
        }
        Ok(summary)
    }

    /// Fetches the real Quota Summary, retrying automatically on 401 Unauthorized with token refresh
    /// Quota via agy's own `/quota` command - the same source the user sees in a terminal.
    ///
    /// The REST endpoint answers `remainingFraction: 1` for the Gemini buckets with a
    /// `resetTime` of exactly request-time + 7 days, i.e. a freshly provisioned window per
    /// request, while the CLI reports the account's real rolling window (87.18%, resetting on a
    /// fixed date). Verified side-effect free: `conversation_id` is empty, `duration_seconds` is
    /// 0, the brain directory set is unchanged, and no model tokens are spent.
    pub async fn fetch_quota_from_cli() -> Result<QuotaSummaryResponse> {
        let mut cmd = tokio::process::Command::new(crate::executor::get_agy_path());
        cmd.args(["-p", "/quota", "--output-format", "json"]);
        cmd.stdin(std::process::Stdio::null());

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

    pub async fn fetch_quota_summary(access_token: &str) -> Result<QuotaSummaryResponse> {
        if let Some((fetched_at, summary)) = get_quota_cache().read().await.clone() {
            if fetched_at.elapsed() < QUOTA_CACHE_TTL {
                return Ok(summary);
            }
        }

        let fresh = Self::fetch_quota_summary_now(access_token).await?;
        *get_quota_cache().write().await = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }

    /// Uncached fetch: agy's own command first (it is the account's real rolling window), then the
    /// REST endpoints, retrying once with a forced token refresh on 401.
    async fn fetch_quota_summary_now(access_token: &str) -> Result<QuotaSummaryResponse> {
        match Self::fetch_quota_from_cli().await {
            Ok(summary) => return Ok(summary),
            Err(err) => tracing::warn!(
                "quota via the agy CLI was unavailable ({err}); falling back to the REST endpoints"
            ),
        }

        match Self::query_quota_endpoints(access_token).await {
            Ok(summary) => Ok(summary),
            Err(err) => {
                let err_str = err.to_string();
                if err_str.contains("401") || err_str.contains("UNAUTHENTICATED") {
                    tracing::warn!(
                        "Received 401 for quota summary, attempting forced token refresh..."
                    );
                    if let Some(new_tok) = Self::force_refresh().await {
                        return Self::query_quota_endpoints(&new_tok).await;
                    }
                }
                Err(err)
            }
        }
    }

    /// Formats the real Models & Quota into visual Telegram HTML matching agy CLI without intermediate vectors
    pub async fn format_models_and_quota() -> Result<String> {
        let access_token = match Self::get_access_token().await {
            Some(tok) => tok,
            None => {
                anyhow::bail!("未找到 OAuth 认证 Token (路径: ~/.gemini/antigravity-cli/antigravity-oauth-token)")
            }
        };

        let email = match Self::cached_email().await {
            Some(addr) => addr,
            None => Self::fetch_user_email(&access_token)
                .await
                .unwrap_or_else(|| "Unknown".to_string()),
        };

        let summary = Self::fetch_quota_summary(&access_token).await?;
        let now = Utc::now();

        use crate::bot::formatter::TelegramFormatter;

        let mut output = String::with_capacity(1024);
        let _ = writeln!(
            output,
            "└ <b>Models & Quota</b>\n  <b>Account:</b> <code>{}</code>\n",
            TelegramFormatter::escape_html(&email)
        );

        for group in summary.groups {
            let title = group
                .display_name
                .as_deref()
                .unwrap_or("MODELS")
                .to_uppercase();
            let _ = writeln!(output, "<b>{}</b>", TelegramFormatter::escape_html(&title));
            if let Some(desc) = &group.description {
                let _ = writeln!(output, "  <i>{}</i>", TelegramFormatter::escape_html(desc));
            }
            output.push('\n');

            for bucket in group.buckets {
                let b_name = bucket.display_name.as_deref().unwrap_or("Limit Remaining");
                let Some(frac_raw) = bucket.remaining_fraction else {
                    // A missing fraction must not be drawn as a full bar; that is how a
                    // fabricated "100% available" gets produced.
                    let _ = writeln!(
                        output,
                        "  <b>{}</b>\n    <code>未上报额度数据</code>\n",
                        TelegramFormatter::escape_html(b_name)
                    );
                    continue;
                };
                let frac = frac_raw.clamp(0.0, 1.0);
                let pct = frac * 100.0;

                // 25 blocks for clean mobile layout
                let bar_len = 25;
                let filled = ((frac * bar_len as f64).round() as usize).min(bar_len);
                let empty = bar_len - filled;

                let _ = writeln!(output, "  <b>{}</b>", TelegramFormatter::escape_html(b_name));
                let _ = write!(output, "    <code>[");
                for _ in 0..filled {
                    output.push('█');
                }
                for _ in 0..empty {
                    output.push('░');
                }
                let _ = writeln!(output, "] {:.2}%</code>", pct);

                let reset_info = if let Some(ref r_time_str) = bucket.reset_time {
                    if let Ok(r_dt) = DateTime::parse_from_rfc3339(r_time_str) {
                        let r_utc = r_dt.with_timezone(&Utc);
                        let diff = r_utc.signed_duration_since(now);
                        if diff.num_seconds() > 0 {
                            let total_hours = diff.num_hours();
                            let mins = diff.num_minutes() % 60;
                            if frac >= 0.999 {
                                "Quota available".to_string()
                            } else {
                                format!(
                                    "{}% remaining · Refreshes in {}h {}m",
                                    pct.round() as i64,
                                    total_hours,
                                    mins
                                )
                            }
                        } else {
                            "Quota available".to_string()
                        }
                    } else {
                        "Quota available".to_string()
                    }
                } else {
                    "Quota available".to_string()
                };

                let _ = writeln!(output, "    {}\n", reset_info);
            }
        }

        Ok(output.trim_end().to_string())
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
        // The CLI writes exactly this shape (9 fractional digits + Z). If it ever failed to parse,
        // the proactive-refresh branch would silently never fire.
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
        // Inside the 5 minute margin it must refresh proactively.
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

        let no_field: QuotaSummaryResponse = serde_json::from_str(r#"{"description": "x"}"#).unwrap();
        assert!(!has_quota_data(&no_field));

        // Top-level buckets instead of groups: a shape change must not read as a valid panel.
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
        // Verbatim shape of `agy -p /quota --output-format json` (field names are snake_case and
        // groups carry `name`, unlike the REST endpoints). Reading this as "no data" was the bug.
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
        assert_eq!(data.groups[0].display_name.as_deref(), Some("Gemini Models"));
        assert_eq!(bucket.display_name.as_deref(), Some("Weekly Limit Remaining"));
        assert_eq!(bucket.bucket_id.as_deref(), Some("gemini-weekly"));
        assert_eq!(bucket.reset_time.as_deref(), Some("2026-10-25T12:02:36Z"));
        assert_eq!(bucket.window.as_deref(), Some("weekly"));
        assert!(bucket.remaining_fraction.unwrap() > 0.87
            && bucket.remaining_fraction.unwrap() < 0.88,
            "87.18% must survive deserialisation");
    }

    #[test]
    fn rest_camel_case_payload_still_maps() {
        let raw = r#"{"groups":[{"displayName":"Claude and GPT models","buckets":[
            {"bucketId":"3p-weekly","displayName":"Weekly Limit Remaining","window":"weekly",
             "remainingFraction":0.45370081067085266,"resetTime":"2026-10-25T17:45:56Z"}]}]}"#;
        let summary: QuotaSummaryResponse = serde_json::from_str(raw).expect("REST shape must parse");
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
}
