use crate::models::{Effort, Mode, QqGroupSessionMode};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

const KNOWN_CONFIG_KEYS: &[&str] = &[
    "bot_token",
    "allowed_user_ids",
    "allow_all_users",
    "default_workspace",
    "default_model",
    "default_effort",
    "default_mode",
    "proxy_url",
    "draft_update_interval_ms",
    "interrupt_on_new_message",
    "executor_timeout_secs",
    "idle_timeout_secs",
    "max_execution_secs",
    "fish_api_key",
    "fish_reference_id",
    "api_id",
    "api_hash",
    "webhook_url",
    "webhook_port",
    "webhook_secret",
    "qq_app_id",
    "qq_app_secret",
    "qq_allowed_openids",
    "qq_allowed_groups",
    "qq_allow_all_groups",
    "qq_group_session_mode",
];

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
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
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default = "default_webhook_port")]
    pub webhook_port: u16,
    #[serde(default)]
    pub webhook_secret: Option<String>,

    // QQ Bot configuration (Open Platform)
    #[serde(default)]
    pub qq_app_id: Option<String>,
    #[serde(default)]
    pub qq_app_secret: Option<String>,
    #[serde(default)]
    pub qq_allowed_openids: Vec<String>,
    /// Group OpenIDs allowed to talk to the bot. A sender whitelist entry does not grant access
    /// to an unlisted group.
    #[serde(default)]
    pub qq_allowed_groups: Vec<String>,
    #[serde(default)]
    pub qq_allow_all_groups: bool,
    #[serde(default = "default_qq_group_session_mode")]
    pub qq_group_session_mode: String,
}

fn redacted_str(secret: &str) -> &'static str {
    if secret.trim().is_empty() {
        ""
    } else {
        "[REDACTED]"
    }
}

fn redacted_opt(secret: &Option<String>) -> Option<&'static str> {
    secret.as_deref().map(redacted_str)
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bot_token", &redacted_str(&self.bot_token))
            .field("allowed_user_ids", &self.allowed_user_ids)
            .field("allow_all_users", &self.allow_all_users)
            .field("default_workspace", &self.default_workspace)
            .field("default_model", &self.default_model)
            .field("default_effort", &self.default_effort)
            .field("default_mode", &self.default_mode)
            .field("proxy_url", &self.proxy_url)
            .field("draft_update_interval_ms", &self.draft_update_interval_ms)
            .field("interrupt_on_new_message", &self.interrupt_on_new_message)
            .field("executor_timeout_secs", &self.executor_timeout_secs)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("max_execution_secs", &self.max_execution_secs)
            .field("fish_api_key", &redacted_opt(&self.fish_api_key))
            .field("fish_reference_id", &self.fish_reference_id)
            .field("api_id", &self.api_id)
            .field("api_hash", &redacted_opt(&self.api_hash))
            .field("webhook_url", &self.webhook_url)
            .field("webhook_port", &self.webhook_port)
            .field("webhook_secret", &redacted_opt(&self.webhook_secret))
            .field("qq_app_id", &self.qq_app_id)
            .field("qq_app_secret", &redacted_opt(&self.qq_app_secret))
            .field("qq_allowed_openids", &self.qq_allowed_openids)
            .field("qq_allowed_groups", &self.qq_allowed_groups)
            .field("qq_allow_all_groups", &self.qq_allow_all_groups)
            .field("qq_group_session_mode", &self.qq_group_session_mode)
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bot_token: String::new(),
            allowed_user_ids: Vec::new(),
            allow_all_users: false,
            default_workspace: default_workspace(),
            default_model: String::new(),
            default_effort: default_effort(),
            default_mode: default_mode(),
            proxy_url: None,
            draft_update_interval_ms: default_draft_update_interval_ms(),
            interrupt_on_new_message: true,
            executor_timeout_secs: default_executor_timeout_secs(),
            idle_timeout_secs: default_idle_timeout_secs(),
            max_execution_secs: default_max_execution_secs(),
            fish_api_key: None,
            fish_reference_id: None,
            api_id: None,
            api_hash: None,
            webhook_url: None,
            webhook_port: default_webhook_port(),
            webhook_secret: None,
            qq_app_id: None,
            qq_app_secret: None,
            qq_allowed_openids: Vec::new(),
            qq_allowed_groups: Vec::new(),
            qq_allow_all_groups: false,
            qq_group_session_mode: default_qq_group_session_mode(),
        }
    }
}

fn default_webhook_port() -> u16 {
    8443
}

fn default_qq_group_session_mode() -> String {
    QqGroupSessionMode::Group.as_str().to_string()
}

fn default_workspace() -> String {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .to_string_lossy()
        .into_owned()
}

fn default_effort() -> String {
    Effort::High.as_str().to_string()
}

fn default_mode() -> String {
    Mode::AcceptEdits.as_str().to_string()
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

/// Expands leading `~` or `~/` against `dirs::home_dir()`.
#[must_use]
pub fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if let Some(stripped) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    }
    PathBuf::from(trimmed)
}

/// Resolves and ensures a workspace directory exists, falling back to `$HOME` or `/tmp`.
#[must_use]
pub fn ensure_valid_workspace(raw_workspace: &str) -> String {
    let expanded = expand_tilde(raw_workspace);
    if (expanded.exists() && expanded.is_dir()) || std::fs::create_dir_all(&expanded).is_ok() {
        return expanded.to_string_lossy().into_owned();
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .to_string_lossy()
        .into_owned()
}

fn env_non_empty(
    env_get: &impl Fn(&str) -> Option<String>,
    primary: &str,
    fallback: Option<&str>,
) -> Option<String> {
    env_get(primary)
        .or_else(|| fallback.and_then(env_get))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn env_csv_with(
    env_get: &impl Fn(&str) -> Option<String>,
    primary: &str,
    fallback: &str,
) -> Vec<String> {
    env_get(primary)
        .or_else(|| env_get(fallback))
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn env_user_ids_with(env_get: &impl Fn(&str) -> Option<String>) -> Vec<u64> {
    env_get("AGYGRAM_ALLOWED_USER_IDS")
        .or_else(|| env_get("ALLOWED_USER_IDS"))
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse::<u64>().ok())
        .collect()
}

fn env_flag_set_with(env_get: &impl Fn(&str) -> Option<String>, key: &str) -> bool {
    env_get(key).is_some_and(|value| {
        let v = value.trim();
        v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("on")
    })
}

fn trim_opt(opt: &mut Option<String>) {
    if let Some(val) = opt {
        let trimmed = val.trim();
        if trimmed.is_empty() {
            *opt = None;
        } else if trimmed.len() != val.len() {
            *val = trimmed.to_string();
        }
    }
}

fn trim_vec(items: &mut Vec<String>) {
    items.retain_mut(|item| {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            false
        } else {
            if trimmed.len() != item.len() {
                *item = trimmed.to_string();
            }
            true
        }
    });
}

impl Config {
    /// Loads configuration from [`Config::get_config_path`] and environment overrides.
    ///
    /// # Errors
    /// Returns an error if the config file is unreadable/invalid TOML, or if neither a config file
    /// nor bot credentials in environment variables are present.
    pub fn load() -> Result<Self> {
        let (cfg, _) = Self::load_with_path()?;
        Ok(cfg)
    }

    /// Loads configuration together with the resolved config file path (without creating directories).
    ///
    /// # Errors
    /// Returns an error under the same conditions as [`Config::load`].
    pub fn load_with_path() -> Result<(Self, PathBuf)> {
        let config_path = Self::get_config_path()?;
        let cfg = Self::load_from_path_with_env(&config_path, |key| std::env::var(key).ok())?;
        Ok((cfg, config_path))
    }

    fn load_from_path_with_env(
        config_path: &Path,
        env_get: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let (mut config, file_existed, explicit_webhook_port) = if config_path.exists() {
            let content = std::fs::read_to_string(config_path)
                .with_context(|| format!("Failed to read config from {}", config_path.display()))?;
            if let Ok(table) = toml::from_str::<toml::Table>(&content) {
                for key in table.keys() {
                    if !KNOWN_CONFIG_KEYS.contains(&key.as_str()) {
                        tracing::warn!(
                            "Unknown configuration key `{}` in {}; ignoring",
                            key,
                            config_path.display()
                        );
                    }
                }
                let has_webhook_port = table.contains_key("webhook_port");
                let cfg: Config = toml::from_str(&content).with_context(|| {
                    format!("Failed to parse TOML from {}", config_path.display())
                })?;
                (cfg, true, has_webhook_port)
            } else {
                let cfg: Config = toml::from_str(&content).with_context(|| {
                    format!("Failed to parse TOML from {}", config_path.display())
                })?;
                (cfg, true, false)
            }
        } else {
            (Config::default(), false, false)
        };

        config.normalize();
        config.apply_env_overrides(&env_get, explicit_webhook_port);
        config.normalize();

        if !config.has_telegram() && !config.has_qq() {
            if file_existed {
                anyhow::bail!(
                    "Neither Telegram bot_token nor QQ qq_app_id/qq_app_secret is configured in {}. Please configure at least one bot service.",
                    config_path.display()
                );
            }
            anyhow::bail!(
                "Config file not found at {}. Please copy config.example.toml to {} and configure your bot_token or QQ app credentials.",
                config_path.display(),
                config_path.display()
            );
        }

        Ok(config)
    }

    fn apply_env_overrides(
        &mut self,
        env_get: &impl Fn(&str) -> Option<String>,
        explicit_webhook_port: bool,
    ) {
        if self.bot_token == "YOUR_TELEGRAM_BOT_TOKEN" || self.bot_token.trim().is_empty() {
            if let Some(env_token) =
                env_non_empty(env_get, "AGYGRAM_BOT_TOKEN", Some("TELEGRAM_BOT_TOKEN"))
            {
                self.bot_token = env_token;
            }
        }

        if self.allowed_user_ids.is_empty() {
            let from_env = env_user_ids_with(env_get);
            if !from_env.is_empty() {
                self.allowed_user_ids = from_env;
            }
        }

        if !self.allow_all_users && env_flag_set_with(env_get, "AGYGRAM_ALLOW_ALL_USERS") {
            self.allow_all_users = true;
        }

        if self.proxy_url.is_none() {
            self.proxy_url = env_non_empty(env_get, "HTTPS_PROXY", Some("HTTP_PROXY"));
        }
        if self.fish_api_key.is_none() {
            self.fish_api_key = env_non_empty(env_get, "FISH_API_KEY", None);
        }
        if self.fish_reference_id.is_none() {
            self.fish_reference_id = env_non_empty(env_get, "FISH_REFERENCE_ID", None);
        }
        if self.api_id.is_none() {
            self.api_id = env_non_empty(env_get, "TG_API_ID", None).and_then(|s| s.parse().ok());
        }
        if self.api_hash.is_none() {
            self.api_hash = env_non_empty(env_get, "TG_API_HASH", None);
        }
        if self.webhook_url.is_none() {
            self.webhook_url = env_non_empty(env_get, "AGYGRAM_WEBHOOK_URL", None);
        }
        if !explicit_webhook_port {
            if let Some(port) = env_non_empty(env_get, "AGYGRAM_WEBHOOK_PORT", None)
                .and_then(|s| s.parse::<u16>().ok())
            {
                self.webhook_port = port;
            }
        }
        if self.webhook_secret.is_none() {
            self.webhook_secret = env_non_empty(env_get, "AGYGRAM_WEBHOOK_SECRET", None);
        }

        if self.qq_app_id.is_none() {
            self.qq_app_id = env_non_empty(env_get, "AGYGRAM_QQ_APP_ID", Some("QQ_APP_ID"));
        }
        if self.qq_app_secret.is_none() {
            self.qq_app_secret =
                env_non_empty(env_get, "AGYGRAM_QQ_APP_SECRET", Some("QQ_APP_SECRET"));
        }
        if self.qq_allowed_openids.is_empty() {
            let env_openids =
                env_csv_with(env_get, "AGYGRAM_QQ_ALLOWED_OPENIDS", "QQ_ALLOWED_OPENIDS");
            if !env_openids.is_empty() {
                self.qq_allowed_openids = env_openids;
            }
        }
        if self.qq_allowed_groups.is_empty() {
            let env_groups =
                env_csv_with(env_get, "AGYGRAM_QQ_ALLOWED_GROUPS", "QQ_ALLOWED_GROUPS");
            if !env_groups.is_empty() {
                self.qq_allowed_groups = env_groups;
            }
        }
        if !self.qq_allow_all_groups && env_flag_set_with(env_get, "AGYGRAM_QQ_ALLOW_ALL_GROUPS") {
            self.qq_allow_all_groups = true;
        }
        if let Some(mode) = env_non_empty(env_get, "AGYGRAM_QQ_GROUP_SESSION_MODE", None) {
            self.qq_group_session_mode = mode;
        }
    }

    fn normalize(&mut self) {
        let trimmed_tok = self.bot_token.trim();
        if trimmed_tok.len() != self.bot_token.len() {
            self.bot_token = trimmed_tok.to_string();
        }
        trim_opt(&mut self.proxy_url);
        trim_opt(&mut self.fish_api_key);
        trim_opt(&mut self.fish_reference_id);
        trim_opt(&mut self.api_hash);
        trim_opt(&mut self.webhook_url);
        trim_opt(&mut self.webhook_secret);
        trim_opt(&mut self.qq_app_id);
        trim_opt(&mut self.qq_app_secret);
        trim_vec(&mut self.qq_allowed_openids);
        trim_vec(&mut self.qq_allowed_groups);

        if let Ok(parsed) = self.default_effort.parse::<Effort>() {
            self.default_effort = parsed.as_str().to_string();
        } else if !self.default_effort.trim().is_empty() {
            tracing::warn!(
                "Invalid default_effort {:?}; falling back to {:?}",
                self.default_effort,
                Effort::default().as_str()
            );
            self.default_effort = Effort::default().as_str().to_string();
        } else {
            self.default_effort = Effort::default().as_str().to_string();
        }

        if let Ok(parsed) = self.default_mode.parse::<Mode>() {
            self.default_mode = parsed.as_str().to_string();
        } else if !self.default_mode.trim().is_empty() {
            tracing::warn!(
                "Invalid default_mode {:?}; falling back to {:?}",
                self.default_mode,
                Mode::default().as_str()
            );
            self.default_mode = Mode::default().as_str().to_string();
        } else {
            self.default_mode = Mode::default().as_str().to_string();
        }

        if let Ok(parsed) = self.qq_group_session_mode.parse::<QqGroupSessionMode>() {
            self.qq_group_session_mode = parsed.as_str().to_string();
        } else if !self.qq_group_session_mode.trim().is_empty() {
            tracing::warn!(
                "Invalid qq_group_session_mode {:?}; falling back to {:?}",
                self.qq_group_session_mode,
                QqGroupSessionMode::default().as_str()
            );
            self.qq_group_session_mode = QqGroupSessionMode::default().as_str().to_string();
        } else {
            self.qq_group_session_mode = QqGroupSessionMode::default().as_str().to_string();
        }
    }

    /// Typed default thinking effort parsed from `default_effort`.
    #[inline]
    #[must_use]
    pub fn effort(&self) -> Effort {
        self.default_effort.parse().unwrap_or_default()
    }

    /// Typed default execution mode parsed from `default_mode`.
    #[inline]
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.default_mode.parse().unwrap_or_default()
    }

    /// Typed QQ group session isolation mode parsed from `qq_group_session_mode`.
    #[inline]
    #[must_use]
    pub fn qq_group_session_mode_enum(&self) -> QqGroupSessionMode {
        self.qq_group_session_mode.parse().unwrap_or_default()
    }

    #[inline]
    #[must_use]
    pub fn has_telegram(&self) -> bool {
        !self.bot_token.trim().is_empty() && self.bot_token != "YOUR_TELEGRAM_BOT_TOKEN"
    }

    #[inline]
    #[must_use]
    pub fn has_qq(&self) -> bool {
        let id_ok = self
            .qq_app_id
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let sec_ok = self
            .qq_app_secret
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        id_ok && sec_ok
    }

    #[must_use]
    pub fn get_valid_workspace(&self) -> String {
        ensure_valid_workspace(&self.default_workspace)
    }

    /// Resolves the path to `config.toml` without creating parent directories.
    pub fn get_config_path() -> Result<PathBuf> {
        if let Ok(custom_path) = std::env::var("AGYGRAM_CONFIG_PATH") {
            let trimmed = custom_path.trim();
            if !trimmed.is_empty() {
                return Ok(PathBuf::from(trimmed));
            }
        }

        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Ok(home.join(".config").join("agygram").join("config.toml"))
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

    #[inline]
    #[must_use]
    pub fn is_qq_user_allowed(&self, openid: &str) -> bool {
        if self.allow_all_users {
            return true;
        }
        if self.qq_allowed_openids.is_empty() {
            return false;
        }
        let target = openid.trim();
        self.qq_allowed_openids
            .iter()
            .any(|allowed| allowed.trim() == target)
    }

    /// Group access is governed by the group whitelist (or the explicit allow-all switch).
    /// A user whitelist entry only grants that user's private chat; it must not open every group
    /// the bot happens to be in.
    #[inline]
    #[must_use]
    pub fn is_qq_group_allowed(&self, group_openid: &str, _sender_openid: &str) -> bool {
        if self.allow_all_users || self.qq_allow_all_groups {
            return true;
        }
        let target = group_openid.trim();
        let listed = |ids: &[String]| ids.iter().any(|id| id.trim() == target);
        // `qq_allowed_openids` historically carried group ids too; keep those configs working.
        listed(&self.qq_allowed_groups) || listed(&self.qq_allowed_openids)
    }

    #[inline]
    #[must_use]
    pub fn denies_qq_everyone(&self) -> bool {
        self.qq_allowed_openids.is_empty()
            && self.qq_allowed_groups.is_empty()
            && !self.allow_all_users
            && !self.qq_allow_all_groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
    fn qq_group_access_requires_the_group_to_be_listed() {
        let cfg = parse(
            "bot_token = \"t\"\nqq_allowed_openids = [\"USER\"]\nqq_allowed_groups = [\"GROUP\"]\n",
        );
        assert!(cfg.is_qq_group_allowed("GROUP", "ANYONE"));
        assert!(
            !cfg.is_qq_group_allowed("OTHER", "USER"),
            "a whitelisted sender must not open an unlisted group"
        );
        assert!(!cfg.is_qq_group_allowed("OTHER", "ANYONE"));

        let legacy = parse("bot_token = \"t\"\nqq_allowed_openids = [\"GROUP\"]\n");
        assert!(
            legacy.is_qq_group_allowed("GROUP", "ANYONE"),
            "group ids kept in qq_allowed_openids must stay valid"
        );

        let open = parse("bot_token = \"t\"\nqq_allow_all_groups = true\n");
        assert!(open.is_qq_group_allowed("ANY", "ANYONE"));
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

    #[test]
    fn debug_formatting_redacts_all_secrets() {
        let cfg = Config {
            bot_token: "123456:SECRET_TG_TOKEN".into(),
            fish_api_key: Some("fish-secret-key".into()),
            api_hash: Some("tg-api-hash-secret".into()),
            webhook_secret: Some("wh-secret".into()),
            qq_app_secret: Some("qq-secret".into()),
            ..Config::default()
        };
        let debug_str = format!("{cfg:?}");
        assert!(!debug_str.contains("SECRET_TG_TOKEN"));
        assert!(!debug_str.contains("fish-secret-key"));
        assert!(!debug_str.contains("tg-api-hash-secret"));
        assert!(!debug_str.contains("wh-secret"));
        assert!(!debug_str.contains("qq-secret"));
        assert!(debug_str.contains("[REDACTED]"));
    }

    #[test]
    fn env_overrides_apply_when_config_file_exists_and_trim_whitelists() {
        let dir = std::env::temp_dir().join(format!("agygram_cfg_env_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
bot_token = "YOUR_TELEGRAM_BOT_TOKEN"
qq_allowed_openids = ["  OPENID_1  ", "  "]
qq_allowed_groups = [" GROUP_1 "]
"#,
        )
        .unwrap();

        let env_map: HashMap<&str, &str> = HashMap::from([
            ("AGYGRAM_BOT_TOKEN", "  tg-env-token  "),
            ("AGYGRAM_ALLOW_ALL_USERS", "YES"),
            ("HTTPS_PROXY", "socks5://127.0.0.1:1080"),
            ("FISH_API_KEY", "fish-from-env"),
            ("AGYGRAM_WEBHOOK_PORT", "9443"),
            ("AGYGRAM_QQ_GROUP_SESSION_MODE", "MEMBER"),
        ]);

        let cfg =
            Config::load_from_path_with_env(&path, |k| env_map.get(k).map(|v| (*v).to_string()))
                .expect("config should load with env overrides");

        assert_eq!(cfg.bot_token, "tg-env-token");
        assert!(cfg.allow_all_users);
        assert_eq!(cfg.proxy_url.as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(cfg.fish_api_key.as_deref(), Some("fish-from-env"));
        assert_eq!(cfg.webhook_port, 9443);
        assert_eq!(cfg.qq_allowed_openids, vec!["OPENID_1"]);
        assert_eq!(cfg.qq_allowed_groups, vec!["GROUP_1"]);
        assert_eq!(cfg.qq_group_session_mode, "member");
        assert_eq!(cfg.qq_group_session_mode_enum(), QqGroupSessionMode::Member);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_only_load_works_without_config_file() {
        let missing = std::env::temp_dir().join("agygram_nonexistent_dir_check/config.toml");
        let env_map: HashMap<&str, &str> = HashMap::from([
            ("QQ_APP_ID", "102857208"),
            ("QQ_APP_SECRET", "qq-sec"),
            ("AGYGRAM_QQ_ALLOW_ALL_GROUPS", "true"),
        ]);

        let cfg =
            Config::load_from_path_with_env(&missing, |k| env_map.get(k).map(|v| (*v).to_string()))
                .expect("QQ env-only config should load");

        assert!(cfg.has_qq());
        assert!(!cfg.has_telegram());
        assert!(cfg.qq_allow_all_groups);
        assert!(
            !missing.parent().unwrap().exists(),
            "loading missing config must not create dirs"
        );
    }
}
