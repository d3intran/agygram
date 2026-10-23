use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub bot_token: String,
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
    /// Serving everyone is opt-in: an empty `allowed_user_ids` denies every request unless this
    /// is explicitly set. This daemon runs `agy` with --dangerously-skip-permissions, so the
    /// historical "empty list means allow all" default was remote code execution for anyone who
    /// found the bot.
    #[serde(default)]
    pub allow_all_users: bool,
    #[serde(default = "default_workspace")]
    pub default_workspace: String,
    #[serde(default)]
    pub default_model: String,
    #[serde(default = "default_effort")]
    pub default_effort: String,
    #[serde(default = "default_mode")]
    pub default_mode: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default = "default_draft_update_interval_ms")]
    pub draft_update_interval_ms: u64,
    #[serde(default = "default_true")]
    pub interrupt_on_new_message: bool,
    #[serde(default = "default_executor_timeout_secs")]
    pub executor_timeout_secs: u64,
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
    #[serde(default = "default_max_execution_secs")]
    pub max_execution_secs: u64,
    #[serde(default)]
    pub fish_api_key: Option<String>,
    #[serde(default)]
    pub fish_reference_id: Option<String>,
    #[serde(default)]
    pub api_id: Option<i64>,
    #[serde(default)]
    pub api_hash: Option<String>,
}

fn default_workspace() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .to_string_lossy()
        .to_string()
}

fn default_effort() -> String {
    "high".to_string()
}

fn default_mode() -> String {
    "accept-edits".to_string()
}

fn default_draft_update_interval_ms() -> u64 {
    1000
}

fn default_true() -> bool {
    true
}

fn default_executor_timeout_secs() -> u64 {
    300
}

fn default_idle_timeout_secs() -> u64 {
    180
}

fn default_max_execution_secs() -> u64 {
    3600
}

/// Comma-separated numeric Telegram user IDs, for systemd units that configure via Environment=.
fn env_user_ids() -> Vec<u64> {
    std::env::var("AGYGRAM_ALLOWED_USER_IDS")
        .or_else(|_| std::env::var("ALLOWED_USER_IDS"))
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse::<u64>().ok())
        .collect()
}

fn env_flag_set(key: &str) -> bool {
    std::env::var(key)
        .map(|value| matches!(value.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::get_config_path()?;
        if !config_path.exists() {
            // Check if bot_token is supplied via environment variable
            if let Ok(env_token) = std::env::var("AGYGRAM_BOT_TOKEN").or_else(|_| std::env::var("TELEGRAM_BOT_TOKEN")) {
                let trimmed = env_token.trim().to_string();
                if !trimmed.is_empty() {
                    return Ok(Self {
                        bot_token: trimmed,
                        allowed_user_ids: env_user_ids(),
                        allow_all_users: env_flag_set("AGYGRAM_ALLOW_ALL_USERS"),
                        default_workspace: default_workspace(),
                        default_model: String::new(),
                        default_effort: default_effort(),
                        default_mode: default_mode(),
                        proxy_url: std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("HTTP_PROXY")).ok(),
                        draft_update_interval_ms: default_draft_update_interval_ms(),
                        interrupt_on_new_message: true,
                        executor_timeout_secs: default_executor_timeout_secs(),
                        idle_timeout_secs: default_idle_timeout_secs(),
                        max_execution_secs: default_max_execution_secs(),
                        fish_api_key: std::env::var("FISH_API_KEY").ok(),
                        fish_reference_id: std::env::var("FISH_REFERENCE_ID").ok(),
                        api_id: std::env::var("TG_API_ID").ok().and_then(|s| s.parse().ok()),
                        api_hash: std::env::var("TG_API_HASH").ok(),
                    });
                }
            }

            anyhow::bail!(
                "Config file not found at {}. Please copy config.example.toml to {} and configure your bot_token.",
                config_path.display(),
                config_path.display()
            );
        }

        let content = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config from {}", config_path.display()))?;
        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse TOML from {}", config_path.display()))?;

        // Allow environment variable to override placeholder bot_token
        if config.bot_token == "YOUR_TELEGRAM_BOT_TOKEN" || config.bot_token.trim().is_empty() {
            if let Ok(env_token) = std::env::var("AGYGRAM_BOT_TOKEN").or_else(|_| std::env::var("TELEGRAM_BOT_TOKEN")) {
                let trimmed = env_token.trim().to_string();
                if !trimmed.is_empty() {
                    config.bot_token = trimmed;
                }
            }
        }

        // A unit file may carry the whitelist only in the environment.
        if config.allowed_user_ids.is_empty() {
            let from_env = env_user_ids();
            if !from_env.is_empty() {
                config.allowed_user_ids = from_env;
            }
        }

        if config.bot_token == "YOUR_TELEGRAM_BOT_TOKEN" || config.bot_token.trim().is_empty() {
            anyhow::bail!(
                "bot_token in {} is still set to placeholder 'YOUR_TELEGRAM_BOT_TOKEN'. Please update it with your real Telegram Bot Token from @BotFather.",
                config_path.display()
            );
        }

        Ok(config)
    }

    #[must_use]
    pub fn get_valid_workspace(&self) -> String {
        let trimmed = self.default_workspace.trim();
        let expanded = if let Some(stripped) = trimmed.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(stripped)
            } else {
                PathBuf::from(trimmed)
            }
        } else if trimmed == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
        } else {
            PathBuf::from(trimmed)
        };

        if expanded.exists() && expanded.is_dir() {
            return expanded.to_string_lossy().to_string();
        }
        if std::fs::create_dir_all(&expanded).is_ok() {
            return expanded.to_string_lossy().to_string();
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .to_string_lossy()
            .to_string()
    }

    pub fn get_config_path() -> Result<PathBuf> {
        if let Ok(custom_path) = std::env::var("AGYGRAM_CONFIG_PATH") {
            let pb = PathBuf::from(custom_path.trim());
            if let Some(parent) = pb.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            return Ok(pb);
        }

        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let config_dir = home.join(".config").join("agygram");
        std::fs::create_dir_all(&config_dir)?;
        Ok(config_dir.join("config.toml"))
    }

    /// An explicit whitelist always wins; with none configured, access is denied unless the
    /// operator opted in to serving everyone.
    #[inline]
    #[must_use]
    pub fn is_user_allowed(&self, user_id: u64) -> bool {
        if self.allowed_user_ids.is_empty() {
            return self.allow_all_users;
        }
        self.allowed_user_ids.contains(&user_id)
    }

    #[inline]
    #[must_use]
    pub fn denies_everyone(&self) -> bool {
        self.allowed_user_ids.is_empty() && !self.allow_all_users
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    fn parse(fragment: &str) -> Config {
        toml::from_str(fragment).expect("config fragment should parse")
    }

    #[test]
    fn empty_whitelist_denies_everyone_unless_explicitly_public() {
        let locked = parse("bot_token = \"t\"\n");
        assert!(locked.allowed_user_ids.is_empty());
        assert!(!locked.is_user_allowed(1));
        assert!(!locked.is_user_allowed(0));
        assert!(locked.denies_everyone());

        let public = parse("bot_token = \"t\"\nallow_all_users = true\n");
        assert!(public.is_user_allowed(42));
        assert!(!public.denies_everyone());
    }

    #[test]
    fn explicit_whitelist_wins_over_allow_all() {
        let strict = parse("bot_token = \"t\"\nallowed_user_ids = [7]\nallow_all_users = true\n");
        assert!(strict.is_user_allowed(7));
        assert!(!strict.is_user_allowed(99));
    }

    #[test]
    fn legacy_fields_still_deserialize_with_defaults() {
        // An older config.toml written before these keys existed must keep loading.
        let legacy = parse(
            "bot_token = \"t\"\nallowed_user_ids = [123]\ndefault_workspace = \"/tmp\"\ndefault_model = \"\"\ndefault_effort = \"high\"\ndefault_mode = \"accept-edits\"\ndraft_update_interval_ms = 1000\ninterrupt_on_new_message = true\nexecutor_timeout_secs = 300\n",
        );
        assert_eq!(legacy.allowed_user_ids, vec![123]);
        assert!(!legacy.allow_all_users);
    }
}
