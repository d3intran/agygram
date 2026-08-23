use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt::Write;
use std::sync::OnceLock;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[derive(Deserialize, Debug, Clone)]
pub struct OAuthTokenFile {
    pub token: Option<OAuthTokenData>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OAuthTokenData {
    pub access_token: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UserInfo {
    pub email: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaSummaryResponse {
    #[serde(default)]
    pub groups: Vec<QuotaGroup>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaGroup {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub buckets: Vec<QuotaBucket>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuotaBucket {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "resetTime")]
    pub reset_time: Option<String>,
    #[serde(rename = "remainingFraction")]
    pub remaining_fraction: Option<f64>,
}

pub struct QuotaFetcher;

impl QuotaFetcher {
    /// Loads the access token from ~/.gemini/antigravity-cli/antigravity-oauth-token asynchronously
    pub async fn get_access_token() -> Option<String> {
        let home = dirs::home_dir()?;
        let token_path = home.join(".gemini/antigravity-cli/antigravity-oauth-token");
        if !token_path.exists() {
            return None;
        }
        let content = tokio::fs::read_to_string(token_path).await.ok()?;
        let token_file: OAuthTokenFile = serde_json::from_str(&content).ok()?;
        token_file.token?.access_token
    }

    /// Fetches the user email from Google UserInfo endpoint using connection pool
    pub async fn fetch_user_email(access_token: &str) -> Option<String> {
        let client = get_http_client();
        let resp = client
            .get("https://www.googleapis.com/oauth2/v2/userinfo")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", "AntigravityCLI/1.1.17")
            .send()
            .await
            .ok()?;
        let info: UserInfo = resp.json().await.ok()?;
        info.email
    }

    /// Fetches the real Quota Summary from cloudcode-pa.googleapis.com using connection pool
    pub async fn fetch_quota_summary(access_token: &str) -> Result<QuotaSummaryResponse> {
        let client = get_http_client();
        let resp = client
            .post("https://cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .header("User-Agent", "AntigravityCLI/1.1.17")
            .body("{}")
            .send()
            .await
            .context("Failed to send request to retrieveUserQuotaSummary")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("retrieveUserQuotaSummary error {}: {}", status, text);
        }

        let summary: QuotaSummaryResponse = resp
            .json()
            .await
            .context("Failed to parse retrieveUserQuotaSummary response JSON")?;
        Ok(summary)
    }

    /// Formats the real Models & Quota into visual Telegram HTML matching agy CLI without intermediate vectors
    pub async fn format_models_and_quota() -> Result<String> {
        let access_token = match Self::get_access_token().await {
            Some(tok) => tok,
            None => anyhow::bail!("未找到 OAuth 认证 Token (路径: ~/.gemini/antigravity-cli/antigravity-oauth-token)"),
        };

        let email = Self::fetch_user_email(&access_token)
            .await
            .unwrap_or_else(|| "Unknown".to_string());

        let summary = Self::fetch_quota_summary(&access_token).await?;
        let now = Utc::now();

        let mut output = String::with_capacity(1024);
        let _ = writeln!(output, "└ <b>Models & Quota</b>\n  <b>Account:</b> <code>{}</code>\n", email);

        for group in summary.groups {
            let title = group
                .display_name
                .as_deref()
                .unwrap_or("MODELS")
                .to_uppercase();
            let _ = writeln!(output, "<b>{}</b>", title);
            if let Some(desc) = &group.description {
                let _ = writeln!(output, "  <i>{}</i>", desc);
            }
            output.push('\n');

            for bucket in group.buckets {
                let b_name = bucket.display_name.as_deref().unwrap_or("Limit Remaining");
                let frac = bucket.remaining_fraction.unwrap_or(1.0).clamp(0.0, 1.0);
                let pct = frac * 100.0;

                // 25 blocks for clean mobile layout
                let bar_len = 25;
                let filled = ((frac * bar_len as f64).round() as usize).min(bar_len);
                let empty = bar_len - filled;

                let _ = writeln!(output, "  <b>{}</b>", b_name);
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
