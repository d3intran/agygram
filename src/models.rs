//! Shared domain enums, model aliases, and CLI parameter validators.
//!
//! Centralizes model, thinking effort, execution mode, and QQ session mode definitions
//! so Telegram, QQ, config, and session layers validate against one canonical source.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// Supported built-in model `(id, display_label)` pairs offered in interactive pickers.
pub const SUPPORTED_MODELS: &[(&str, &str)] = &[
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("claude-opus-5-5-high", "Claude Opus 5.5 (High)"),
    ("claude-opus-5-5-medium", "Claude Opus 5.5 (Medium)"),
    ("claude-opus-5-5-low", "Claude Opus 5.5 (Low)"),
    ("claude-sonnet-5-5-high", "Claude Sonnet 5.5 (High)"),
    ("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 (Medium)"),
    ("claude-sonnet-5-5-low", "Claude Sonnet 5.5 (Low)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

/// Error returned when parsing a model option string into a typed enum fails.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid {kind}: {value:?}")]
pub struct ParseOptionError {
    /// Name of the option kind being parsed.
    pub kind: &'static str,
    /// Offending input value.
    pub value: String,
}

/// Thinking effort level passed to `agy --effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Minimal reasoning latency.
    Low,
    /// Balanced reasoning depth.
    Medium,
    /// Deep reasoning (default).
    #[default]
    High,
    /// Extra-deep reasoning.
    XHigh,
    /// Maximum reasoning budget.
    Max,
}

impl Effort {
    /// All canonical effort variants in display order.
    pub const ALL: &'static [Self] = &[Self::Low, Self::Medium, Self::High, Self::XHigh, Self::Max];

    /// Canonical CLI flag string for this effort level.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// Parses an exact canonical lowercase effort token.
    #[inline]
    #[must_use]
    pub fn parse_canonical(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Effort {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for Effort {
    type Err = ParseOptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("low") {
            Ok(Self::Low)
        } else if trimmed.eq_ignore_ascii_case("medium") {
            Ok(Self::Medium)
        } else if trimmed.eq_ignore_ascii_case("high") {
            Ok(Self::High)
        } else if trimmed.eq_ignore_ascii_case("xhigh") {
            Ok(Self::XHigh)
        } else if trimmed.eq_ignore_ascii_case("max") {
            Ok(Self::Max)
        } else {
            Err(ParseOptionError {
                kind: "effort",
                value: trimmed.to_string(),
            })
        }
    }
}

/// Execution mode passed to `agy --mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Fully autonomous decision mode.
    Auto,
    /// Automatically apply edits (default).
    #[default]
    AcceptEdits,
    /// Planning-only mode without making edits.
    Plan,
}

impl Mode {
    /// All canonical execution modes in display order.
    pub const ALL: &'static [Self] = &[Self::Auto, Self::AcceptEdits, Self::Plan];

    /// Canonical CLI flag string for this execution mode.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::AcceptEdits => "accept-edits",
            Self::Plan => "plan",
        }
    }

    /// Parses an exact canonical lowercase execution mode token.
    #[inline]
    #[must_use]
    pub fn parse_canonical(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "accept-edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for Mode {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for Mode {
    type Err = ParseOptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("auto") {
            Ok(Self::Auto)
        } else if trimmed.eq_ignore_ascii_case("accept-edits") {
            Ok(Self::AcceptEdits)
        } else if trimmed.eq_ignore_ascii_case("plan") {
            Ok(Self::Plan)
        } else {
            Err(ParseOptionError {
                kind: "mode",
                value: trimmed.to_string(),
            })
        }
    }
}

/// QQ group session isolation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QqGroupSessionMode {
    /// Share one session across the entire group (default).
    #[default]
    Group,
    /// Isolate sessions per `(group, member)` pair.
    Member,
}

impl QqGroupSessionMode {
    /// Canonical config string for this QQ group session mode.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Member => "member",
        }
    }
}

impl fmt::Display for QqGroupSessionMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for QqGroupSessionMode {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for QqGroupSessionMode {
    type Err = ParseOptionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("group") {
            Ok(Self::Group)
        } else if trimmed.eq_ignore_ascii_case("member") {
            Ok(Self::Member)
        } else {
            Err(ParseOptionError {
                kind: "qq_group_session_mode",
                value: trimmed.to_string(),
            })
        }
    }
}

/// Returns `true` if `model` is empty (system default) or in [`SUPPORTED_MODELS`].
#[inline]
#[must_use]
pub fn is_valid_model(model: &str) -> bool {
    model.is_empty() || SUPPORTED_MODELS.iter().any(|(id, _)| *id == model)
}

/// Normalizes shorthand or legacy model aliases into canonical model IDs.
#[must_use]
pub fn normalize_model_name(input: &str) -> Cow<'static, str> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        // Gemini 4 Argon
        "gemini-4-argon-high" | "argon-high" | "4-high" | "argon4-high" => {
            Cow::Borrowed("gemini-4-argon-high")
        }
        "gemini-4-argon-medium"
        | "gemini-4-argon"
        | "argon"
        | "gemini-4"
        | "argon-med"
        | "argon-medium"
        | "4-med"
        | "4-medium" => Cow::Borrowed("gemini-4-argon-medium"),
        "gemini-4-argon-low" | "argon-low" | "4-low" | "argon4-low" => {
            Cow::Borrowed("gemini-4-argon-low")
        }
        // Gemini 3.8 Flash
        "gemini-3.8-flash-high"
        | "3.8"
        | "flash"
        | "3.8-flash"
        | "gemini-3.8"
        | "3.8-high"
        | "high" => Cow::Borrowed("gemini-3.8-flash-high"),
        "gemini-3.8-flash-medium" | "3.8-med" | "3.8-medium" | "medium" | "med" => {
            Cow::Borrowed("gemini-3.8-flash-medium")
        }
        "gemini-3.8-flash-low" | "3.8-low" | "low" => Cow::Borrowed("gemini-3.8-flash-low"),
        // Claude Opus 5.5 (with backward compatibility aliases)
        "claude-opus-5-5-high"
        | "opus"
        | "claude-opus"
        | "opus-5.5"
        | "opus-5"
        | "opus-high"
        | "claude-opus-4-6-thinking" => Cow::Borrowed("claude-opus-5-5-high"),
        "claude-opus-5-5-medium" | "opus-med" | "opus-medium" => {
            Cow::Borrowed("claude-opus-5-5-medium")
        }
        "claude-opus-5-5-low" | "opus-low" => Cow::Borrowed("claude-opus-5-5-low"),
        // Claude Sonnet 5.5 (with backward compatibility aliases)
        "claude-sonnet-5-5-high" | "sonnet-high" => Cow::Borrowed("claude-sonnet-5-5-high"),
        "claude-sonnet-5-5-medium"
        | "sonnet"
        | "claude"
        | "claude-sonnet"
        | "sonnet-5.5"
        | "sonnet-5"
        | "sonnet-med"
        | "claude-sonnet-4-6" => Cow::Borrowed("claude-sonnet-5-5-medium"),
        "claude-sonnet-5-5-low" | "sonnet-low" => Cow::Borrowed("claude-sonnet-5-5-low"),
        // GPT-OSS 120B
        "gpt-oss-120b-medium" | "oss" | "gpt" | "gpt-oss" | "120b" => {
            Cow::Borrowed("gpt-oss-120b-medium")
        }
        // Default / System Reset
        "default" | "reset" | "system" | "" => Cow::Borrowed(""),
        _ => Cow::Owned(trimmed.to_string()),
    }
}

/// Infers the implied [`Effort`] from a model suffix (checking `-xhigh` before `-high`).
#[inline]
#[must_use]
pub fn effort_from_model_suffix(model: &str) -> Option<Effort> {
    if model.ends_with("-xhigh") {
        Some(Effort::XHigh)
    } else if model.ends_with("-max") {
        Some(Effort::Max)
    } else if model.ends_with("-high") {
        Some(Effort::High)
    } else if model.ends_with("-medium") || model.ends_with("-med") {
        Some(Effort::Medium)
    } else if model.ends_with("-low") {
        Some(Effort::Low)
    } else {
        None
    }
}

/// Returns `true` if `effort` is one of the canonical CLI effort strings.
#[inline]
#[must_use]
pub fn is_supported_effort(effort: &str) -> bool {
    Effort::parse_canonical(effort).is_some()
}

/// Returns `true` if `mode` is one of the canonical CLI execution mode strings.
#[inline]
#[must_use]
pub fn is_supported_mode(mode: &str) -> bool {
    Mode::parse_canonical(mode).is_some()
}

/// Validates a compact CLI argument token (rejects leading `-`, spaces, control chars, or >64 bytes).
#[must_use]
pub fn is_safe_cli_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

/// Validates a free-form CLI argument such as a project or agent name (allows spaces, up to 128 bytes).
#[must_use]
pub fn is_safe_free_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && !value.chars().any(char::is_control)
}

/// Validates a Fish Audio voice reference ID (`8..=128` ASCII alphanumeric, `-`, `_`).
#[inline]
#[must_use]
pub fn is_valid_voice_reference(id: &str) -> bool {
    (8..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_roundtrip_and_canonical_check() {
        for &e in Effort::ALL {
            assert_eq!(Effort::from_str(e.as_str()).unwrap(), e);
            assert!(is_supported_effort(e.as_str()));
            assert_eq!(e.to_string(), e.as_str());
        }
        assert_eq!(Effort::from_str("  XHigh ").unwrap(), Effort::XHigh);
        assert!(Effort::from_str("ultra").is_err());
        assert!(!is_supported_effort("off"));
        assert!(!is_supported_effort(""));
    }

    #[test]
    fn mode_roundtrip_and_canonical_check() {
        for &m in Mode::ALL {
            assert_eq!(Mode::from_str(m.as_str()).unwrap(), m);
            assert!(is_supported_mode(m.as_str()));
            assert_eq!(m.to_string(), m.as_str());
        }
        assert_eq!(Mode::from_str(" Accept-Edits ").unwrap(), Mode::AcceptEdits);
        assert!(Mode::from_str("danger").is_err());
        assert!(!is_supported_mode(""));
    }

    #[test]
    fn qq_group_session_mode_parses_case_insensitively() {
        assert_eq!(
            QqGroupSessionMode::from_str("MEMBER").unwrap(),
            QqGroupSessionMode::Member
        );
        assert_eq!(
            QqGroupSessionMode::from_str("group").unwrap(),
            QqGroupSessionMode::Group
        );
        assert!(QqGroupSessionMode::from_str("other").is_err());
    }

    #[test]
    fn normalize_model_aliases_and_suffix_effort() {
        assert_eq!(normalize_model_name("opus"), "claude-opus-5-5-high");
        assert_eq!(normalize_model_name("flash"), "gemini-3.8-flash-high");
        assert_eq!(normalize_model_name("default"), "");
        assert!(is_valid_model(""));
        assert!(is_valid_model(&normalize_model_name("argon")));
        assert!(!is_valid_model("gpt-4o"));

        // Crucial ordering: -xhigh must match XHigh, not High!
        assert_eq!(effort_from_model_suffix("foo-xhigh"), Some(Effort::XHigh));
        assert_eq!(effort_from_model_suffix("foo-high"), Some(Effort::High));
        assert_eq!(effort_from_model_suffix("foo-med"), Some(Effort::Medium));
        assert_eq!(effort_from_model_suffix("foo-medium"), Some(Effort::Medium));
        assert_eq!(effort_from_model_suffix("foo-low"), Some(Effort::Low));
        assert_eq!(effort_from_model_suffix("foo-max"), Some(Effort::Max));
        assert_eq!(effort_from_model_suffix("foo"), None);
    }

    #[test]
    fn cli_and_free_value_validators_reject_flag_smuggling() {
        for evil in [
            "--sandbox",
            "--dangerously-skip-permissions",
            "-c",
            "x\nrm -rf /",
            "",
        ] {
            assert!(!is_safe_cli_value(evil));
            assert!(!is_safe_free_value(evil));
        }
        assert!(is_safe_cli_value("gemini-3.8-flash-high"));
        assert!(!is_safe_cli_value(&"a".repeat(65)));
        assert!(is_safe_free_value(&"a".repeat(65)));
        assert!(!is_safe_free_value(&"a".repeat(129)));
    }
}
