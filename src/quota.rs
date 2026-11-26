use crate::models::QuotaPool;
use crate::text::escape_html;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

const AGY_USER_AGENT: &str = "AntigravityCLI/1.5.17";

/// Both `/usage` and the interactive status card hit the quota endpoints, so a few rapid clicks
/// used to mean a fresh upstream request every time.
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(180);

/// Decoupled TTL for `agy -p /credits` (25 minutes) so 180s `/panel` refreshes never spawn a
/// second 266 MB subprocess every 3 minutes.
const CREDITS_CACHE_TTL: Duration = Duration::from_secs(1500);

/// Margin for the read-only OAuth REST fast path before falling back to `agy -p /quota`.
const READONLY_TOKEN_REFRESH_MARGIN_SECS: i64 = 120;

const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";
const PRIMARY_QUOTA_ENDPOINT: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";
const FALLBACK_QUOTA_ENDPOINT: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary";

#[derive(Clone)]
struct CachedQuota {
    fetched_at: Instant,
    summary: ArcQuotaSummary,
    email: Option<String>,
    credits: Option<CreditsInfo>,
}

#[derive(Clone)]
struct CachedCredits {
    fetched_at: Instant,
    info: Option<CreditsInfo>,
}

type ArcQuotaSummary = Arc<QuotaSummaryResponse>;

static QUOTA_CACHE: OnceLock<RwLock<Option<CachedQuota>>> = OnceLock::new();
static CREDITS_CACHE: OnceLock<RwLock<Option<CachedCredits>>> = OnceLock::new();
static QUOTA_SINGLEFLIGHT: OnceLock<Mutex<()>> = OnceLock::new();
static CREDITS_SINGLEFLIGHT: OnceLock<Mutex<()>> = OnceLock::new();
static METADATA_CLI_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static QUOTA_BG_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static HTTP_CLIENT_STATE: OnceLock<StdRwLock<(Option<String>, reqwest::Client)>> = OnceLock::new();

struct QuotaBgFlagReset;

impl Drop for QuotaBgFlagReset {
    fn drop(&mut self) {
        QUOTA_BG_IN_FLIGHT.store(false, Ordering::Release);
    }
}

fn get_quota_cache() -> &'static RwLock<Option<CachedQuota>> {
    QUOTA_CACHE.get_or_init(|| RwLock::new(None))
}

fn get_credits_cache() -> &'static RwLock<Option<CachedCredits>> {
    CREDITS_CACHE.get_or_init(|| RwLock::new(None))
}

fn get_singleflight_lock() -> &'static Mutex<()> {
    QUOTA_SINGLEFLIGHT.get_or_init(|| Mutex::new(()))
}

fn get_credits_singleflight_lock() -> &'static Mutex<()> {
    CREDITS_SINGLEFLIGHT.get_or_init(|| Mutex::new(()))
}

/// Shared async mutex serializing short-lived `agy` metadata CLI subprocesses (`/quota`,
/// `/credits`, `models`) so low-RAM hosts never spawn concurrent metadata `agy` instances.
pub(crate) fn metadata_cli_lock() -> &'static Mutex<()> {
    METADATA_CLI_LOCK.get_or_init(|| Mutex::new(()))
}

/// Executes an `agy` metadata CLI command under [`metadata_cli_lock`], isolating it in its own
/// process group and sending `SIGKILL` to the full process tree on timeout.
pub(crate) async fn run_metadata_cli_command(
    args: &[&str],
    timeout_dur: Duration,
    label: &str,
) -> Result<Output> {
    let _cli_guard = metadata_cli_lock().lock().await;
    crate::executor::evict_idle_for_budget().await;
    let mut cmd = tokio::process::Command::new(crate::executor::get_agy_path());
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("failed to execute `{label}`"))?;
    #[cfg(unix)]
    let child_pid = child.id();

    match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
        Ok(res) => res.with_context(|| format!("failed while waiting for `{label}`")),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                crate::executor::signal_process_tree(pid, libc::SIGKILL);
            }
            anyhow::bail!("timed out waiting for `{label}`");
        }
    }
}

fn build_http_client(proxy_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(2)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
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

#[derive(Deserialize, Debug, Clone)]
pub struct OAuthTokenFile {
    pub token: Option<OAuthTokenData>,
    #[serde(default)]
    #[allow(dead_code)]
    pub auth_method: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OAuthTokenData {
    pub access_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub token_type: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expiry: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub expires_in: Option<u64>,
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub bucket_id: Option<String>,
    #[serde(rename = "displayName", alias = "name")]
    pub display_name: Option<String>,
    #[allow(dead_code)]
    pub window: Option<String>,
    #[allow(dead_code)]
    pub description: Option<String>,
    #[serde(rename = "resetTime", alias = "reset_time")]
    pub reset_time: Option<String>,
    #[serde(rename = "remainingFraction", alias = "remaining_fraction")]
    pub remaining_fraction: Option<f64>,
}

/// Envelope of `agy -p /quota --output-format json`.
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

/// Remaining Google AI / Antigravity credits reported by `agy -p /credits --output-format json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CreditsInfo {
    #[serde(default, alias = "remainingCredits")]
    pub remaining_credits: i64,
    #[serde(default, alias = "upgradeUri")]
    pub upgrade_uri: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct AgyCreditsPrintResponse {
    #[serde(default)]
    command: Option<AgyCreditsCommandPayload>,
}

#[derive(Deserialize, Debug, Clone)]
struct AgyCreditsCommandPayload {
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
    #[serde(default)]
    data: Option<CreditsInfo>,
}

fn parse_credits_cli_output(stdout: &[u8]) -> Option<CreditsInfo> {
    let resp: AgyCreditsPrintResponse = serde_json::from_slice(stdout).ok()?;
    let mut info = resp.command?.data?;
    if info
        .upgrade_uri
        .as_deref()
        .is_some_and(|s| s.trim().is_empty())
    {
        info.upgrade_uri = None;
    }
    Some(info)
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

fn render_quota_panel(
    email: &str,
    summary: &QuotaSummaryResponse,
    credits: Option<&CreditsInfo>,
    html: bool,
) -> String {
    let now = Utc::now();
    let mut output = String::with_capacity(1024);

    if html {
        let _ = writeln!(
            output,
            "└ <b>Models & Quota</b>\n  <b>Account:</b> <code>{}</code>",
            escape_html(email)
        );
        if let Some(c) = credits {
            let _ = writeln!(
                output,
                "  <b>AI Credits:</b> <code>{}</code>",
                c.remaining_credits
            );
        }
        output.push('\n');
    } else {
        let _ = writeln!(output, "└ Models & Quota\n  Account: {email}");
        if let Some(c) = credits {
            let _ = writeln!(output, "  AI Credits: {}", c.remaining_credits);
        }
        output.push('\n');
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

/// Structured quota summary for a single model pool (`Gemini Models`, `Claude & GPT models`, etc.).
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaPoolInfo {
    pub title: String,
    pub pool: Option<QuotaPool>,
    pub weekly_pct: Option<f64>,
    pub weekly_reset_in: Option<String>,
    pub five_hour_pct: Option<f64>,
    pub five_hour_reset_in: Option<String>,
    pub effective_pct: Option<f64>,
    pub mentioned_models: Vec<String>,
}

/// Structured snapshot of account email, per-pool remaining quota fractions, and optional AI credits.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaOverview {
    pub email: String,
    pub pools: Vec<QuotaPoolInfo>,
    pub credits: Option<CreditsInfo>,
}

impl QuotaOverview {
    /// Returns the matching [`QuotaPoolInfo`] for `pool`, if reported.
    #[must_use]
    pub fn pool_info(&self, pool: QuotaPool) -> Option<&QuotaPoolInfo> {
        self.pools.iter().find(|p| p.pool == Some(pool))
    }

    /// Formats a short UI badge for `pool` (e.g. `"87%"` or `"5h:87% · W:96%"`).
    #[must_use]
    pub fn compact_badge(&self, pool: QuotaPool) -> Option<String> {
        let info = self.pool_info(pool)?;
        match (info.five_hour_pct, info.weekly_pct) {
            (Some(h5), Some(w)) => Some(format!("5h:{h5:.0}% · W:{w:.0}%")),
            (Some(h5), None) => Some(format!("{h5:.0}%")),
            (None, Some(w)) => Some(format!("{w:.0}%")),
            (None, None) => None,
        }
    }

    /// Formats a tight button-friendly percentage string (e.g. `"87%"`).
    #[must_use]
    pub fn button_pct(&self, pool: QuotaPool) -> Option<String> {
        let pct = self.pool_info(pool)?.effective_pct?;
        Some(format!("{pct:.0}%"))
    }
}

fn infer_pool_from_group(group: &QuotaGroup) -> Option<QuotaPool> {
    let title_lower = group
        .display_name
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    if title_lower.contains("gemini") {
        return Some(QuotaPool::Gemini);
    }
    if title_lower.contains("claude") || title_lower.contains("gpt") || title_lower.contains("3p") {
        return Some(QuotaPool::ThirdParty);
    }
    for b in &group.buckets {
        if let Some(id) = b.bucket_id.as_deref() {
            let id_lower = id.to_ascii_lowercase();
            if id_lower.contains("gemini") {
                return Some(QuotaPool::Gemini);
            }
            if id_lower.contains("3p") || id_lower.contains("claude") || id_lower.contains("gpt") {
                return Some(QuotaPool::ThirdParty);
            }
        }
    }
    None
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return Some(0);
    }
    h.windows(n.len())
        .position(|window| window.eq_ignore_ascii_case(n))
}

fn parse_mentioned_models(desc: Option<&str>) -> Vec<String> {
    const GROUP_PREFIX: &str = "models within this group:";
    let Some(raw) = desc else {
        return Vec::new();
    };
    let list_part = if let Some(idx) = find_ascii_case_insensitive(raw, GROUP_PREFIX) {
        &raw[idx + GROUP_PREFIX.len()..]
    } else if let Some((_, after)) = raw.split_once(':') {
        after
    } else {
        return Vec::new();
    };
    list_part
        .split(',')
        .map(|s| s.trim().trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn format_reset_countdown(reset_time: Option<&str>, now: DateTime<Utc>) -> Option<String> {
    let raw = reset_time?;
    let dt = DateTime::parse_from_rfc3339(raw).ok()?.with_timezone(&Utc);
    let diff = dt.signed_duration_since(now);
    let secs = diff.num_seconds();
    if secs <= 0 {
        return None;
    }
    let total_hours = diff.num_hours();
    let mins = diff.num_minutes() % 60;
    if total_hours >= 24 {
        let days = total_hours / 24;
        let hours = total_hours % 24;
        Some(format!("{days}d {hours}h"))
    } else {
        Some(format!("{total_hours}h {mins}m"))
    }
}

/// Converts raw [`QuotaSummaryResponse`] into a structured [`QuotaOverview`].
#[allow(dead_code)]
#[must_use]
pub fn summarize_quota(email: &str, summary: &QuotaSummaryResponse) -> QuotaOverview {
    summarize_quota_with_credits(email, summary, None)
}

/// Converts raw [`QuotaSummaryResponse`] and optional [`CreditsInfo`] into a [`QuotaOverview`].
#[must_use]
pub fn summarize_quota_with_credits(
    email: &str,
    summary: &QuotaSummaryResponse,
    credits: Option<CreditsInfo>,
) -> QuotaOverview {
    let now = Utc::now();
    let mut pools = Vec::with_capacity(summary.groups.len());
    for group in &summary.groups {
        let title = group
            .display_name
            .clone()
            .unwrap_or_else(|| "Models".to_string());
        let pool = infer_pool_from_group(group);
        let mentioned_models = parse_mentioned_models(group.description.as_deref());
        let mut weekly_pct = None;
        let mut weekly_reset_in = None;
        let mut five_hour_pct = None;
        let mut five_hour_reset_in = None;
        let mut min_pct: Option<f64> = None;

        for bucket in &group.buckets {
            let Some(frac) = bucket.remaining_fraction.filter(|v| v.is_finite()) else {
                continue;
            };
            let pct = frac.clamp(0.0, 1.0) * 100.0;
            min_pct = Some(match min_pct {
                Some(prev) => prev.min(pct),
                None => pct,
            });
            let reset_str = format_reset_countdown(bucket.reset_time.as_deref(), now);
            let win = bucket.window.as_deref().unwrap_or("").to_ascii_lowercase();
            let bname = bucket
                .display_name
                .as_deref()
                .unwrap_or("")
                .to_ascii_lowercase();
            if win == "weekly" || bname.contains("weekly") {
                weekly_pct = Some(pct);
                weekly_reset_in = reset_str;
            } else if win == "5h" || bname.contains("five hour") || bname.contains("5-hour") {
                five_hour_pct = Some(pct);
                five_hour_reset_in = reset_str;
            }
        }

        pools.push(QuotaPoolInfo {
            title,
            pool,
            weekly_pct,
            weekly_reset_in,
            five_hour_pct,
            five_hour_reset_in,
            effective_pct: min_pct,
            mentioned_models,
        });
    }
    QuotaOverview {
        email: email.to_string(),
        pools,
        credits,
    }
}

fn observe_quota_models(summary: &QuotaSummaryResponse) {
    let mut names = Vec::new();
    for group in &summary.groups {
        names.extend(parse_mentioned_models(group.description.as_deref()));
    }
    if !names.is_empty() {
        crate::models::observe_quota_model_names(&names);
    }
}

pub struct QuotaFetcher;

impl QuotaFetcher {
    /// Discovers candidate OAuth token paths using async `tokio::fs` with `HashSet` deduplication,
    /// fast-pathing `$HOME` before scanning `/home` and `/root`.
    pub async fn get_token_paths_async() -> Vec<PathBuf> {
        let mut primary = Vec::new();
        if let Some(home) = dirs::home_dir() {
            primary = expand_and_dedup_token_paths([home]);
            for path in &primary {
                if tokio::fs::try_exists(path).await.unwrap_or(false) {
                    return primary;
                }
            }
        }

        let mut homes = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir("/home").await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if entry.file_type().await.is_ok_and(|ft| ft.is_dir()) {
                    homes.push(entry.path());
                }
            }
        }
        homes.push(PathBuf::from("/root"));

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for p in primary {
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
        for home in homes {
            push_unique_token_candidates(&home, &mut seen, &mut out);
        }
        out
    }

    /// Reads a fresh local OAuth access token strictly read-only (never writing or refreshing the
    /// token file on disk, eliminating OAuth write races with `agy`).
    async fn read_fresh_token_and_email_readonly() -> Option<(String, Option<String>)> {
        let paths = Self::get_token_paths_async().await;
        let now = Utc::now();
        for path in &paths {
            let Some(token_file) = read_token_file(path).await else {
                continue;
            };
            let Some(data) = token_file.token else {
                continue;
            };
            let Some(access_token) = data.access_token.filter(|tok| !tok.trim().is_empty()) else {
                continue;
            };
            if token_needs_refresh(
                data.expiry.as_deref(),
                now,
                READONLY_TOKEN_REFRESH_MARGIN_SECS,
            ) {
                tracing::debug!(
                    path = %path.display(),
                    "local OAuth token is near/past expiry; skipping read-only REST fast path"
                );
                continue;
            }
            let email = token_file
                .id_token
                .as_deref()
                .and_then(extract_email_from_id_token);
            return Some((access_token, email));
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

    /// Reads only the local `id_token` JWT to extract the account email without ever triggering
    /// an OAuth token refresh or writing back to disk (eliminates OAuth write races with `agy`).
    async fn read_cached_email_readonly() -> Option<String> {
        let paths = Self::get_token_paths_async().await;
        for path in &paths {
            let Some(token_file) = read_token_file(path).await else {
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

    /// Queries quota via `agy -p /quota --output-format json` (serialized behind
    /// [`metadata_cli_lock`] with process-group kill on timeout).
    ///
    /// # Errors
    /// Returns an error if `agy` fails to run, times out, exits non-zero, or returns empty quota data.
    pub async fn fetch_quota_from_cli() -> Result<QuotaSummaryResponse> {
        let output = run_metadata_cli_command(
            &["-p", "/quota", "--output-format", "json"],
            Duration::from_secs(20),
            "agy -p /quota",
        )
        .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_trimmed = stderr.trim();
            if output.status.code() == Some(2) {
                anyhow::bail!(
                    "`agy -p /quota` 返回 exit code 2（当前处于 API Key 认证模式，不支持 OAuth 配额查询）{}",
                    if stderr_trimmed.is_empty() {
                        String::new()
                    } else {
                        format!(": {stderr_trimmed}")
                    }
                );
            }
            anyhow::bail!(
                "`agy -p /quota` exited with {:?}: {}",
                output.status.code(),
                stderr_trimmed
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

    /// Queries remaining AI credits via `agy -p /credits --output-format json` (serialized behind
    /// [`metadata_cli_lock`] with process-group kill on timeout).
    /// Returns `None` on timeout, non-zero exit (e.g. API Key mode), or parse failure.
    pub async fn fetch_credits_from_cli() -> Option<CreditsInfo> {
        let output = run_metadata_cli_command(
            &["-p", "/credits", "--output-format", "json"],
            Duration::from_secs(15),
            "agy -p /credits",
        )
        .await
        .ok()?;
        if !output.status.success() {
            tracing::debug!(
                code = ?output.status.code(),
                "`agy -p /credits` exited non-zero"
            );
            return None;
        }
        parse_credits_cli_output(&output.stdout)
    }

    /// Fetches AI credits via a decoupled 25-minute cache (`CREDITS_CACHE_TTL`) so 180s `/panel`
    /// quota refreshes do not spawn `agy -p /credits` every 3 minutes.
    async fn fetch_cached_credits(force_refresh: bool) -> Option<CreditsInfo> {
        if !force_refresh {
            let cache = get_credits_cache().read().await;
            if let Some(entry) = &*cache
                && entry.fetched_at.elapsed() < CREDITS_CACHE_TTL
            {
                return entry.info.clone();
            }
        }

        let _sf = get_credits_singleflight_lock().lock().await;
        if !force_refresh {
            let cache = get_credits_cache().read().await;
            if let Some(entry) = &*cache
                && entry.fetched_at.elapsed() < CREDITS_CACHE_TTL
            {
                return entry.info.clone();
            }
        }

        let info = Self::fetch_credits_from_cli().await;
        *get_credits_cache().write().await = Some(CachedCredits {
            fetched_at: Instant::now(),
            info: info.clone(),
        });
        info
    }

    async fn fetch_cached_panel_data_inner(
        force_refresh: bool,
    ) -> Result<(ArcQuotaSummary, String, Option<CreditsInfo>)> {
        if !force_refresh {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache
                && entry.fetched_at.elapsed() < QUOTA_CACHE_TTL
                && let Some(ref email) = entry.email
            {
                return Ok((
                    Arc::clone(&entry.summary),
                    email.clone(),
                    entry.credits.clone(),
                ));
            }
        }

        let _sf = get_singleflight_lock().lock().await;
        if !force_refresh {
            let cache = get_quota_cache().read().await;
            if let Some(entry) = &*cache
                && entry.fetched_at.elapsed() < QUOTA_CACHE_TTL
                && let Some(ref email) = entry.email
            {
                return Ok((
                    Arc::clone(&entry.summary),
                    email.clone(),
                    entry.credits.clone(),
                ));
            }
        }

        // Fast path: read the local OAuth token strictly read-only (~250ms, <1 MB RAM) when
        // outside the 120s expiry margin; never refresh/write the token file to avoid racing
        // with `agy`. Fall back to `agy -p /quota` CLI on expired/missing/401/error.
        let mut rest_err_note: Option<String> = None;
        let rest_fast_path = match Self::read_fresh_token_and_email_readonly().await {
            Some((access_token, offline_email)) => {
                match Self::query_quota_endpoints(&access_token).await {
                    Ok(summary) => {
                        let email = match offline_email {
                            Some(addr) => addr,
                            None => Self::fetch_user_email(&access_token)
                                .await
                                .unwrap_or_else(|| "Unknown".to_string()),
                        };
                        Some((summary, email))
                    }
                    Err(QuotaQueryError::Unauthorized) => {
                        tracing::debug!(
                            "read-only OAuth quota REST returned 401; falling back to `agy -p /quota`"
                        );
                        rest_err_note = Some("401 Unauthorized".to_string());
                        None
                    }
                    Err(err) => {
                        tracing::debug!(
                            "read-only OAuth quota REST failed ({err}); falling back to `agy -p /quota`"
                        );
                        rest_err_note = Some(err.to_string());
                        None
                    }
                }
            }
            None => None,
        };

        let (summary_raw, resolved_email) = match rest_fast_path {
            Some(res) => res,
            None => match Self::fetch_quota_from_cli().await {
                Ok(summary) => {
                    let email = Self::read_cached_email_readonly()
                        .await
                        .unwrap_or_else(|| "Authenticated CLI".to_string());
                    (summary, email)
                }
                Err(cli_err) => {
                    if let Some(rest_err) = rest_err_note {
                        anyhow::bail!(
                            "配额 REST 快速路径失败 ({rest_err})，且回退 `agy -p /quota` 失败: {cli_err}"
                        );
                    }
                    return Err(cli_err);
                }
            },
        };

        let credits = Self::fetch_cached_credits(force_refresh).await;

        observe_quota_models(&summary_raw);
        let summary = Arc::new(summary_raw);
        *get_quota_cache().write().await = Some(CachedQuota {
            fetched_at: Instant::now(),
            summary: Arc::clone(&summary),
            email: Some(resolved_email.clone()),
            credits: credits.clone(),
        });
        Ok((summary, resolved_email, credits))
    }

    async fn fetch_cached_panel_data() -> Result<(ArcQuotaSummary, String, Option<CreditsInfo>)> {
        Self::fetch_cached_panel_data_inner(false).await
    }

    /// Spawns a detached background quota refresh if none is currently running. Never kills an
    /// in-flight `agy -p /quota` subprocess on UI timeouts.
    pub fn spawn_background_refresh() -> bool {
        if QUOTA_BG_IN_FLIGHT
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _reset = QuotaBgFlagReset;
                let _ = Self::fetch_overview(false).await;
            });
            true
        } else {
            QUOTA_BG_IN_FLIGHT.store(false, Ordering::Release);
            false
        }
    }

    /// Stale-while-revalidate snapshot for instant UI rendering (`0 ms` subprocess wait):
    /// returns the latest cached [`QuotaOverview`] (even if older than TTL) and triggers a
    /// detached background refresh when missing or stale. Returns `(cached_overview, revalidating)`.
    pub async fn peek_overview_and_revalidate() -> (Option<QuotaOverview>, bool) {
        let (cached, is_stale_or_missing) = {
            let cache = get_quota_cache().read().await;
            match &*cache {
                Some(entry) => {
                    let email = entry.email.as_deref().unwrap_or("Unknown");
                    let ov =
                        summarize_quota_with_credits(email, &entry.summary, entry.credits.clone());
                    (Some(ov), entry.fetched_at.elapsed() >= QUOTA_CACHE_TTL)
                }
                None => (None, true),
            }
        };
        if is_stale_or_missing {
            Self::spawn_background_refresh();
        }
        (cached, is_stale_or_missing)
    }

    /// Fetches structured [`QuotaOverview`] (using cache unless `force_refresh` is true).
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn fetch_overview(force_refresh: bool) -> Result<QuotaOverview> {
        let (summary, email, credits) = Self::fetch_cached_panel_data_inner(force_refresh).await?;
        Ok(summarize_quota_with_credits(&email, &summary, credits))
    }

    /// Formats the real Models & Quota into Telegram HTML (optionally forcing a fresh query).
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn format_models_and_quota_with_refresh(force_refresh: bool) -> Result<String> {
        let (summary, email, credits) = Self::fetch_cached_panel_data_inner(force_refresh).await?;
        Ok(render_quota_panel(&email, &summary, credits.as_ref(), true))
    }

    /// Formats the real Models & Quota into Telegram HTML.
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn format_models_and_quota() -> Result<String> {
        let (summary, email, credits) = Self::fetch_cached_panel_data().await?;
        Ok(render_quota_panel(&email, &summary, credits.as_ref(), true))
    }

    /// Formats the real Models & Quota as HTML-free plain text (for CLI / QQ).
    ///
    /// # Errors
    /// Returns an error if credentials are missing or quota retrieval fails.
    pub async fn format_models_and_quota_plain() -> Result<String> {
        let (summary, email, credits) = Self::fetch_cached_panel_data().await?;
        Ok(render_quota_panel(
            &email,
            &summary,
            credits.as_ref(),
            false,
        ))
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

        let html = render_quota_panel("u+1@example.com", &summary, None, true);
        assert!(html.contains("<b>GEMINI &amp; MODELS</b>"));
        assert!(html.contains("<i>Shared &lt;pool&gt;</i>"));
        assert!(!html.contains("NaN"));
        assert!(html.contains("未上报额度数据"));
        assert!(html.contains("50.00%"));

        let plain = render_quota_panel("u+1@example.com", &summary, None, false);
        assert!(!plain.contains("<b>"));
        assert!(!plain.contains("<code>"));
        assert!(!plain.contains("&amp;"));
        assert!(!plain.contains("NaN"));
        assert!(plain.contains("GEMINI & MODELS"));
        assert!(plain.contains("50.00%"));
    }

    #[test]
    fn parse_credits_cli_output_handles_valid_empty_upgrade_and_invalid() {
        let raw = br#"{
            "conversation_id": "",
            "status": "SUCCESS",
            "command": {
                "name": "credits",
                "data": {
                    "remaining_credits": 420,
                    "upgrade_uri": "https://one.google.com/upgrade"
                }
            }
        }"#;
        let parsed = parse_credits_cli_output(raw).expect("valid credits JSON must parse");
        assert_eq!(parsed.remaining_credits, 420);
        assert_eq!(
            parsed.upgrade_uri.as_deref(),
            Some("https://one.google.com/upgrade")
        );

        let empty_uri =
            br#"{"command":{"name":"credits","data":{"remaining_credits":0,"upgrade_uri":"  "}}}"#;
        let parsed_empty = parse_credits_cli_output(empty_uri).expect("zero credits must parse");
        assert_eq!(parsed_empty.remaining_credits, 0);
        assert_eq!(parsed_empty.upgrade_uri, None);

        assert_eq!(parse_credits_cli_output(b"not json"), None);
        assert_eq!(parse_credits_cli_output(br#"{"status":"ERROR"}"#), None);
    }

    #[test]
    fn render_panel_displays_credits_in_html_and_plain() {
        let summary = QuotaSummaryResponse {
            description: None,
            groups: vec![QuotaGroup {
                display_name: Some("Gemini Models".into()),
                description: None,
                buckets: vec![QuotaBucket {
                    bucket_id: Some("gemini-5h".into()),
                    display_name: Some("Five Hour".into()),
                    window: Some("5h".into()),
                    description: None,
                    reset_time: None,
                    remaining_fraction: Some(0.9),
                }],
            }],
        };
        let credits = CreditsInfo {
            remaining_credits: 1250,
            upgrade_uri: Some("https://example.com/upgrade".into()),
        };

        let html = render_quota_panel("dev@example.com", &summary, Some(&credits), true);
        assert!(html.contains("<b>AI Credits:</b> <code>1250</code>"));

        let plain = render_quota_panel("dev@example.com", &summary, Some(&credits), false);
        assert!(plain.contains("AI Credits: 1250"));
        assert!(!plain.contains("<b>"));
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

    #[test]
    fn summarize_quota_extracts_pools_percentages_and_models() {
        let raw = r#"{
            "groups": [
                {
                    "name": "Gemini Models",
                    "description": "Models within this group: Gemini 4 Carbon, Gemini 4 Hydrogen, Orchestrator (Carbon)",
                    "buckets": [
                        {"id": "gemini-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 0.96},
                        {"id": "gemini-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 0.87}
                    ]
                },
                {
                    "name": "Claude and GPT models",
                    "description": "Models within this group: Claude Sonnet 4.6, Claude Opus 4.6, GPT-OSS 120B",
                    "buckets": [
                        {"id": "3p-weekly", "name": "Weekly Limit Remaining", "window": "weekly", "remaining_fraction": 1.0},
                        {"id": "3p-5h", "name": "Five Hour Limit Remaining", "window": "5h", "remaining_fraction": 0.50}
                    ]
                }
            ]
        }"#;
        let summary: QuotaSummaryResponse = serde_json::from_str(raw).unwrap();
        let ov = summarize_quota("dev@example.com", &summary);
        assert_eq!(ov.email, "dev@example.com");
        assert_eq!(ov.credits, None);
        assert_eq!(ov.button_pct(QuotaPool::Gemini).as_deref(), Some("87%"));
        assert_eq!(
            ov.compact_badge(QuotaPool::Gemini).as_deref(),
            Some("5h:87% · W:96%")
        );
        assert_eq!(ov.button_pct(QuotaPool::ThirdParty).as_deref(), Some("50%"));
        let gem = ov.pool_info(QuotaPool::Gemini).unwrap();
        assert_eq!(
            gem.mentioned_models,
            vec![
                "Gemini 4 Carbon",
                "Gemini 4 Hydrogen",
                "Orchestrator (Carbon)"
            ]
        );

        let credits = CreditsInfo {
            remaining_credits: 75,
            upgrade_uri: None,
        };
        let ov_with_credits =
            summarize_quota_with_credits("dev@example.com", &summary, Some(credits.clone()));
        assert_eq!(ov_with_credits.credits, Some(credits));
    }

    #[test]
    fn parse_mentioned_models_ignores_plain_prose_and_parses_colon_lists() {
        assert!(parse_mentioned_models(None).is_empty());
        assert!(parse_mentioned_models(Some("Shared <pool>")).is_empty());
        assert!(
            parse_mentioned_models(Some(
                "Within each group, models share a weekly limit and a 5-hour limit."
            ))
            .is_empty()
        );
        assert_eq!(
            parse_mentioned_models(Some(
                "Models within this group: Gemini 4 Carbon, Gemini 4 Hydrogen."
            )),
            vec!["Gemini 4 Carbon", "Gemini 4 Hydrogen"]
        );
        assert_eq!(
            parse_mentioned_models(Some(
                "Note: shared pool. Models within this group: Claude Sonnet 4.6, GPT-OSS 120B"
            )),
            vec!["Claude Sonnet 4.6", "GPT-OSS 120B"]
        );
        assert_eq!(
            parse_mentioned_models(Some("Supported models: Gemini 4 Neon, Gemini 3.8 Flash")),
            vec!["Gemini 4 Neon", "Gemini 3.8 Flash"]
        );
    }

    #[test]
    fn readonly_token_margin_is_120_seconds() {
        assert!(!token_needs_refresh(
            Some("2026-10-22T05:03:00Z"),
            now(),
            READONLY_TOKEN_REFRESH_MARGIN_SECS
        ));
        assert!(token_needs_refresh(
            Some("2026-10-22T05:01:30Z"),
            now(),
            READONLY_TOKEN_REFRESH_MARGIN_SECS
        ));
    }
}
