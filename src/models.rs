//! Shared domain enums, dynamic model catalog, aliases, and CLI parameter validators.
//!
//! Centralizes model, thinking effort, execution mode, and QQ session mode definitions
//! so Telegram, QQ, config, executor, and session layers validate against one canonical,
//! dynamically updated source.

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

/// Built-in fallback `(id, display_label)` pairs aligned with live Antigravity CLI models.
/// Runtime discovery from `agy models` dynamically merges new models into [`MODEL_REGISTRY`].
pub const SUPPORTED_MODELS: &[(&str, &str)] = &[
    ("gemini-4-neon-xhigh", "Gemini 4 Neon (XHigh)"),
    ("gemini-4-neon-high", "Gemini 4 Neon (High)"),
    ("gemini-4-neon-medium", "Gemini 4 Neon (Medium)"),
    ("gemini-4-neon-low", "Gemini 4 Neon (Low)"),
    ("gemini-4-hydrogen-xhigh", "Gemini 4 Hydrogen (XHigh)"),
    ("gemini-4-hydrogen-high", "Gemini 4 Hydrogen (High)"),
    ("gemini-4-hydrogen-medium", "Gemini 4 Hydrogen (Medium)"),
    ("gemini-4-hydrogen-low", "Gemini 4 Hydrogen (Low)"),
    ("gemini-4-carbon-high", "Gemini 4 Carbon (High)"),
    ("gemini-4-carbon-medium", "Gemini 4 Carbon (Medium)"),
    ("gemini-4-carbon-low", "Gemini 4 Carbon (Low)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("gemini-pro-orchestrator-neon", "Orchestrator (Neon)"),
    ("gemini-pro-orchestrator", "Orchestrator (Carbon)"),
    ("gemini-pro-orchestrator-argon", "Orchestrator (Argon)"),
    ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
    ("claude-sonnet-4-6", "Claude Sonnet 4.6 (Thinking)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

/// Human-readable description of the CLI's default model when `session.model` is empty.
pub const SYSTEM_DEFAULT_MODEL_LABEL: &str = "系统默认 (Gemini 4 Neon)";

const MODEL_REFRESH_TTL: Duration = Duration::from_secs(1800);
const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

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

/// Quota pool that a model draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuotaPool {
    /// Google Gemini / Orchestrator models.
    Gemini,
    /// Third-party models (Claude, GPT-OSS, custom external models).
    ThirdParty,
}

impl QuotaPool {
    /// Human-readable pool label.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini Models",
            Self::ThirdParty => "Claude & GPT Models",
        }
    }
}

/// Normalized metadata for one selectable model entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelEntry {
    /// Canonical CLI `--model` identifier.
    pub id: String,
    /// Human-friendly display label (e.g. `Gemini 4 Carbon (High)`).
    pub label: String,
    /// Short deterministic family slug (`h2`, `carbon`, `argon`, `flash`, `orch`, `claude`, `gpt`, or `f_<hex>`).
    pub family_slug: String,
    /// Display name of the parent family.
    pub family_title: String,
    /// Short button label inside the family variant picker (e.g. `High`, `XHigh`, `Carbon`, `Opus 4.6`).
    pub variant_label: String,
    /// Shared quota pool this model consumes.
    pub quota_pool: QuotaPool,
    /// True when discovered at runtime rather than from the built-in fallback table.
    pub is_dynamic: bool,
}

/// Categorized family of models for multi-page control panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFamilyGroup {
    /// Short deterministic slug suitable for Telegram callback_data (`<= 18` bytes).
    pub slug: String,
    /// Human-readable family title.
    pub title: String,
    /// Display emoji icon.
    pub icon: &'static str,
    /// Shared quota pool.
    pub quota_pool: QuotaPool,
    /// Ordered variants belonging to this family.
    pub models: Vec<ModelEntry>,
}

#[inline]
const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A_OFFSET_BASIS;
    let mut i = 0usize;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV1A_PRIME);
        i += 1;
    }
    hash
}

fn infer_quota_pool(id: &str, label: &str) -> QuotaPool {
    let lower_id = id.to_ascii_lowercase();
    let lower_label = label.to_ascii_lowercase();
    if lower_id.starts_with("gemini")
        || lower_id.contains("orchestrator")
        || lower_label.contains("gemini")
        || lower_label.contains("orchestrator")
    {
        QuotaPool::Gemini
    } else {
        QuotaPool::ThirdParty
    }
}

fn strip_effort_suffix_from_id(id: &str) -> &str {
    for suffix in ["-xhigh", "-high", "-medium", "-med", "-low", "-max"] {
        if let Some(stem) = id.strip_suffix(suffix)
            && !stem.is_empty()
        {
            return stem;
        }
    }
    id
}

fn extract_variant_label(id: &str, label: &str) -> String {
    if let Some(eff) = effort_from_model_suffix(id) {
        if id.starts_with("claude-") {
            if !label.is_empty() {
                return label.strip_prefix("Claude ").unwrap_or(label).to_string();
            }
            return id.to_string();
        }
        return match eff {
            Effort::Low => "Low".to_string(),
            Effort::Medium => "Medium".to_string(),
            Effort::High => "High".to_string(),
            Effort::XHigh => "XHigh".to_string(),
            Effort::Max => "Max".to_string(),
        };
    }
    if id == "gemini-pro-orchestrator-neon" {
        return "Neon Planner".to_string();
    }
    if id == "gemini-pro-orchestrator" {
        return "Carbon Planner".to_string();
    }
    if id == "gemini-pro-orchestrator-argon" {
        return "Argon Planner".to_string();
    }
    if id == "claude-opus-4-6-thinking" {
        return "Opus 4.6 (Thinking)".to_string();
    }
    if id == "claude-sonnet-4-6" {
        return "Sonnet 4.6 (Thinking)".to_string();
    }
    if !label.is_empty() {
        return label.to_string();
    }
    id.to_string()
}

fn classify_family(id: &str, label: &str) -> (String, String, &'static str, QuotaPool) {
    let lower = id.to_ascii_lowercase();
    if lower.starts_with("gemini-4-neon") {
        return (
            "neon".to_string(),
            "Gemini 4 Neon".to_string(),
            "🌟",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("gemini-4-hydrogen") {
        return (
            "h2".to_string(),
            "Gemini 4 Hydrogen".to_string(),
            "💎",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("gemini-4-carbon") {
        return (
            "carbon".to_string(),
            "Gemini 4 Carbon".to_string(),
            "🔷",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("gemini-4-argon") {
        return (
            "argon".to_string(),
            "Gemini 4 Argon".to_string(),
            "🔹",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("gemini-3.8-flash") {
        return (
            "flash".to_string(),
            "Gemini 3.8 Flash".to_string(),
            "⚡",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("gemini-pro-orchestrator") {
        return (
            "orch".to_string(),
            "Orchestrator".to_string(),
            "🧠",
            QuotaPool::Gemini,
        );
    }
    if lower.starts_with("claude-") {
        return (
            "claude".to_string(),
            "Claude".to_string(),
            "👑",
            QuotaPool::ThirdParty,
        );
    }
    if lower.starts_with("gpt-") {
        return (
            "gpt".to_string(),
            "GPT-OSS".to_string(),
            "🌐",
            QuotaPool::ThirdParty,
        );
    }

    // Dynamic unknown family: group by stripped ID stem and human-readable label prefix.
    let stem = strip_effort_suffix_from_id(id);
    let pool = infer_quota_pool(id, label);
    let icon = if pool == QuotaPool::Gemini {
        "✨"
    } else {
        "🧩"
    };
    let title = label
        .split('(')
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(stem)
        .to_string();
    let slug = format!("f_{:012x}", fnv1a_64(stem.as_bytes()) & 0xFFFF_FFFF_FFFF);
    (slug, title, icon, pool)
}

fn family_sort_rank(slug: &str) -> (u8, &str) {
    match slug {
        "neon" => (0, slug),
        "h2" => (1, slug),
        "carbon" => (2, slug),
        "argon" => (3, slug),
        "flash" => (4, slug),
        "orch" => (5, slug),
        "claude" => (6, slug),
        "gpt" => (7, slug),
        _ => (8, slug),
    }
}

fn variant_sort_rank(entry: &ModelEntry) -> (u8, &str) {
    if entry.id == "gemini-pro-orchestrator-neon" {
        return (0, entry.id.as_str());
    }
    if entry.id == "gemini-pro-orchestrator" {
        return (1, entry.id.as_str());
    }
    if entry.id == "gemini-pro-orchestrator-argon" {
        return (2, entry.id.as_str());
    }
    if entry.id.starts_with("claude-") {
        let base = if entry.id.contains("5-5") || entry.id.contains("5.5") {
            0
        } else {
            2
        };
        let tier = if entry.id.contains("opus") { 0 } else { 1 };
        return (base + tier, entry.id.as_str());
    }
    if let Some(eff) = effort_from_model_suffix(&entry.id) {
        let r = match eff {
            Effort::Max => 0,
            Effort::XHigh => 1,
            Effort::High => 2,
            Effort::Medium => 3,
            Effort::Low => 4,
        };
        return (r, entry.id.as_str());
    }
    if entry.id.contains("opus") {
        return (0, entry.id.as_str());
    }
    if entry.id.contains("sonnet") {
        return (1, entry.id.as_str());
    }
    (5, entry.id.as_str())
}

fn build_entry(id: String, label: String, is_dynamic: bool) -> ModelEntry {
    let (family_slug, family_title, _icon, quota_pool) = classify_family(&id, &label);
    let variant_label = extract_variant_label(&id, &label);
    ModelEntry {
        id,
        label,
        family_slug,
        family_title,
        variant_label,
        quota_pool,
        is_dynamic,
    }
}

#[derive(Debug)]
struct ModelCatalog {
    entries: Vec<ModelEntry>,
    known_ids: FxHashSet<String>,
    hash_to_id: FxHashMap<String, String>,
    last_refreshed: Option<Instant>,
    unmapped_quota_hint: bool,
}

impl ModelCatalog {
    fn seeded() -> Self {
        let mut entries = Vec::with_capacity(SUPPORTED_MODELS.len());
        let mut known_ids = FxHashSet::default();
        let mut hash_to_id = FxHashMap::default();
        for &(id, label) in SUPPORTED_MODELS {
            let entry = build_entry(id.to_string(), label.to_string(), false);
            known_ids.insert(entry.id.clone());
            hash_to_id.insert(
                format!("{:016x}", fnv1a_64(entry.id.as_bytes())),
                entry.id.clone(),
            );
            entries.push(entry);
        }
        Self {
            entries,
            known_ids,
            hash_to_id,
            last_refreshed: None,
            unmapped_quota_hint: false,
        }
    }

    fn register_id_lookup(&mut self, id: &str) {
        self.known_ids.insert(id.to_string());
        self.hash_to_id
            .insert(format!("{:016x}", fnv1a_64(id.as_bytes())), id.to_string());
    }

    fn merge_live(&mut self, live_pairs: Vec<(String, String)>) -> usize {
        self.last_refreshed = Some(Instant::now());
        if live_pairs.is_empty() {
            return 0;
        }
        self.unmapped_quota_hint = false;

        let builtin_ids: FxHashSet<&str> = SUPPORTED_MODELS.iter().map(|&(id, _)| id).collect();
        let mut newly_added = 0usize;
        let mut next_entries = Vec::with_capacity(live_pairs.len());
        let mut seen = FxHashSet::default();

        for (id, raw_label) in live_pairs {
            if !is_safe_model_id(&id) || !seen.insert(id.clone()) {
                continue;
            }
            let label = if raw_label.trim().is_empty() {
                id.clone()
            } else {
                raw_label.trim().to_string()
            };
            let is_dynamic = !builtin_ids.contains(id.as_str());
            if !self.known_ids.contains(&id) {
                newly_added += 1;
            }
            self.register_id_lookup(&id);
            next_entries.push(build_entry(id, label, is_dynamic));
        }

        if !next_entries.is_empty() {
            // Sort deterministically by family rank -> variant rank so numbered indices and
            // button grids stay consistent even if upstream CLI reorders lines.
            next_entries.sort_by(|a, b| {
                family_sort_rank(&a.family_slug)
                    .cmp(&family_sort_rank(&b.family_slug))
                    .then_with(|| variant_sort_rank(a).cmp(&variant_sort_rank(b)))
            });
            self.entries = next_entries;
        }
        newly_added
    }

    fn needs_refresh(&self) -> bool {
        if self.unmapped_quota_hint {
            return true;
        }
        match self.last_refreshed {
            None => true,
            Some(ts) => ts.elapsed() >= MODEL_REFRESH_TTL,
        }
    }
}

static MODEL_REGISTRY: OnceLock<RwLock<ModelCatalog>> = OnceLock::new();
static MODEL_REFRESH_LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
static MODEL_BG_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

struct AtomicFlagReset<'a>(&'a AtomicBool);

impl Drop for AtomicFlagReset<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn registry() -> &'static RwLock<ModelCatalog> {
    MODEL_REGISTRY.get_or_init(|| RwLock::new(ModelCatalog::seeded()))
}

fn refresh_lock() -> &'static AsyncMutex<()> {
    MODEL_REFRESH_LOCK.get_or_init(|| AsyncMutex::new(()))
}

/// Parses `agy models` stdout (`<id>\t<Display Name>` or `<id>  <Display Name>`) into `(id, label)` pairs.
#[must_use]
pub fn parse_agy_models_output(raw: &str) -> Vec<(String, String)> {
    let clean = crate::text::strip_ansi_codes(raw);
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();

    for line in clean.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (id_part, label_part) = if let Some((id, label)) = trimmed.split_once('\t') {
            (id.trim(), label.trim())
        } else if let Some((id, label)) = trimmed.split_once(char::is_whitespace) {
            (id.trim(), label.trim())
        } else {
            (trimmed, "")
        };

        if !is_safe_model_id(id_part)
            || id_part.eq_ignore_ascii_case("fetching")
            || id_part.eq_ignore_ascii_case("error:")
            || id_part.eq_ignore_ascii_case("warning:")
            || id_part.eq_ignore_ascii_case("info:")
            || id_part.eq_ignore_ascii_case("model")
            || !seen.insert(id_part.to_string())
        {
            continue;
        }
        let label = if label_part.is_empty() {
            id_part.to_string()
        } else {
            label_part.to_string()
        };
        out.push((id_part.to_string(), label));
    }
    out
}

/// Merges a parsed list of `(id, label)` models into the global runtime [`MODEL_REGISTRY`].
/// Returns the count of newly discovered model IDs not previously in the registry.
pub fn merge_live_models(live_pairs: Vec<(String, String)>) -> usize {
    match registry().write() {
        Ok(mut guard) => guard.merge_live(live_pairs),
        Err(poisoned) => poisoned.into_inner().merge_live(live_pairs),
    }
}

/// Observes model family names reported in live `/quota` descriptions (`"Models within this group: ..."`).
/// Marks the model catalog stale for refresh if a name does not match any currently registered model.
pub fn observe_quota_model_names(names: &[String]) {
    if names.is_empty() {
        return;
    }
    let mut guard = match registry().write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    for name in names {
        let needle = name.trim().to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        let known = guard.entries.iter().any(|e| {
            e.label.to_ascii_lowercase().contains(&needle)
                || e.family_title.to_ascii_lowercase().contains(&needle)
                || e.id
                    .to_ascii_lowercase()
                    .contains(&needle.replace(' ', "-"))
        });
        if !known {
            guard.unmapped_quota_hint = true;
            break;
        }
    }
}

/// Triggers a non-blocking background `agy models` sync if the runtime catalog is cold or stale,
/// returning immediately so UI panels and button clicks never wait on a CLI subprocess.
pub fn trigger_background_model_refresh_if_stale() {
    let stale = match registry().read() {
        Ok(guard) => guard.needs_refresh(),
        Err(poisoned) => poisoned.into_inner().needs_refresh(),
    };
    if !stale {
        return;
    }
    if MODEL_BG_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _reset = AtomicFlagReset(&MODEL_BG_IN_FLIGHT);
            let _ = refresh_models_now().await;
        });
    } else {
        MODEL_BG_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Refreshes the runtime model registry from `agy models` when the cache is cold, stale, or
/// when `/quota` revealed previously unseen model names. Double-checks freshness after acquiring
/// the singleflight lock so concurrent waiters do not trigger duplicate CLI spawns.
pub async fn refresh_models_if_stale() -> bool {
    let stale = match registry().read() {
        Ok(guard) => guard.needs_refresh(),
        Err(poisoned) => poisoned.into_inner().needs_refresh(),
    };
    if !stale {
        return false;
    }
    let _guard = refresh_lock().lock().await;
    let still_stale = match registry().read() {
        Ok(guard) => guard.needs_refresh(),
        Err(poisoned) => poisoned.into_inner().needs_refresh(),
    };
    if !still_stale {
        return false;
    }
    let Ok(raw) = crate::executor::AgyExecutor::list_models().await else {
        return false;
    };
    let parsed = parse_agy_models_output(&raw);
    let _ = merge_live_models(parsed);
    true
}

/// Unconditionally queries `agy models` (with singleflight coordination) and updates [`MODEL_REGISTRY`].
///
/// # Errors
/// Returns an error if executing `agy models` fails.
pub async fn refresh_models_now() -> anyhow::Result<usize> {
    let _guard = refresh_lock().lock().await;
    let raw = crate::executor::AgyExecutor::list_models().await?;
    let parsed = parse_agy_models_output(&raw);
    Ok(merge_live_models(parsed))
}

/// Returns all currently active models in deterministic family/variant order.
#[must_use]
pub fn list_all_models() -> Vec<ModelEntry> {
    match registry().read() {
        Ok(guard) => guard.entries.clone(),
        Err(poisoned) => poisoned.into_inner().entries.clone(),
    }
}

/// Returns all models grouped by family in display order.
#[must_use]
pub fn list_model_families() -> Vec<ModelFamilyGroup> {
    let entries = list_all_models();
    let mut groups: Vec<ModelFamilyGroup> = Vec::new();
    let mut slug_to_idx: FxHashMap<String, usize> = FxHashMap::default();

    for entry in entries {
        if let Some(&idx) = slug_to_idx.get(&entry.family_slug) {
            groups[idx].models.push(entry);
        } else {
            let idx = groups.len();
            let (_slug, _title, icon, _pool) = classify_family(&entry.id, &entry.label);
            slug_to_idx.insert(entry.family_slug.clone(), idx);
            groups.push(ModelFamilyGroup {
                slug: entry.family_slug.clone(),
                title: entry.family_title.clone(),
                icon,
                quota_pool: entry.quota_pool,
                models: vec![entry],
            });
        }
    }

    groups.sort_by(|a, b| family_sort_rank(&a.slug).cmp(&family_sort_rank(&b.slug)));
    groups
}

/// Finds a single family group by its deterministic slug.
#[must_use]
pub fn find_model_family(slug: &str) -> Option<ModelFamilyGroup> {
    list_model_families().into_iter().find(|g| g.slug == slug)
}

/// Finds a model entry by ID (case-insensitive) from the active catalog.
#[must_use]
pub fn find_model(id: &str) -> Option<ModelEntry> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let guard = match registry().read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .entries
        .iter()
        .find(|e| e.id.eq_ignore_ascii_case(trimmed))
        .cloned()
}

/// Finds a model entry by 1-based index (for QQ/CLI numbered pickers).
#[must_use]
pub fn find_model_by_index(one_based_index: usize) -> Option<ModelEntry> {
    if one_based_index == 0 {
        return None;
    }
    let guard = match registry().read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.entries.get(one_based_index - 1).cloned()
}

/// Returns a display string for `session.model` (falling back to [`SYSTEM_DEFAULT_MODEL_LABEL`] when empty).
#[must_use]
pub fn model_display_label(model_id: &str) -> String {
    let trimmed = model_id.trim();
    if trimmed.is_empty() {
        return SYSTEM_DEFAULT_MODEL_LABEL.to_string();
    }
    find_model(trimmed).map_or_else(|| trimmed.to_string(), |e| e.label)
}

/// Encodes a model selection into a compact callback_data token strictly `<= 64` bytes (`ms:<id>` or `mh:<hex16>`).
#[must_use]
pub fn encode_model_callback(model_id: &str) -> String {
    let trimmed = model_id.trim();
    if trimmed.is_empty() {
        return "ms:default".to_string();
    }
    if trimmed.len() <= 61 && is_safe_cli_value(trimmed) {
        return format!("ms:{trimmed}");
    }
    let hash_hex = format!("{:016x}", fnv1a_64(trimmed.as_bytes()));
    if let Ok(mut guard) = registry().write() {
        guard.register_id_lookup(trimmed);
    }
    format!("mh:{hash_hex}")
}

/// Decodes a compact callback token (`ms:*`, `mh:*`, or legacy `set_model:*` arg) back to a canonical model ID.
/// Returns `Some("")` for system default.
#[must_use]
pub fn decode_model_callback(token: &str) -> Option<String> {
    if let Some(rest) = token.strip_prefix("ms:") {
        let norm = normalize_model_name(rest);
        if is_valid_model(&norm) {
            return Some(norm.into_owned());
        }
        return None;
    }
    if let Some(hex) = token.strip_prefix("mh:") {
        let guard = match registry().read() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        return guard.hash_to_id.get(hex).cloned();
    }
    let norm = normalize_model_name(token);
    is_valid_model(&norm).then(|| norm.into_owned())
}

/// Resolves user input (1-based index, alias, shorthand, or exact/dynamic ID/label) into a canonical model ID.
/// Returns `Some("")` when the user asks for system default (`"default"`, `"reset"`, `"system"`).
#[must_use]
pub fn resolve_model_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(idx) = trimmed.parse::<usize>() {
        return find_model_by_index(idx).map(|e| e.id);
    }
    let norm = normalize_model_name(trimmed);
    if is_valid_model(&norm) {
        return Some(norm.into_owned());
    }
    if let Some(entry) = find_model(&norm) {
        return Some(entry.id);
    }
    let guard = match registry().read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .entries
        .iter()
        .find(|e| e.label.eq_ignore_ascii_case(trimmed))
        .map(|e| e.id.clone())
}

/// Returns `true` if `model` is empty (system default), in [`SUPPORTED_MODELS`], an upcoming
/// Claude 5.5 canonical ID, or in the live runtime registry.
#[inline]
#[must_use]
pub fn is_valid_model(model: &str) -> bool {
    if model.is_empty()
        || SUPPORTED_MODELS.iter().any(|(id, _)| *id == model)
        || matches!(
            model,
            "claude-opus-5-5-high"
                | "claude-opus-5-5-medium"
                | "claude-opus-5-5-low"
                | "claude-sonnet-5-5-high"
                | "claude-sonnet-5-5-medium"
                | "claude-sonnet-5-5-low"
        )
    {
        return true;
    }
    match registry().read() {
        Ok(guard) => guard.known_ids.contains(model),
        Err(poisoned) => poisoned.into_inner().known_ids.contains(model),
    }
}

fn prefer_live_claude_model_in(
    catalog: &ModelCatalog,
    family: &str,
    preferred_suffix: Option<&str>,
    fallback_4_6: &'static str,
) -> Cow<'static, str> {
    let prefix = format!("claude-{family}-5-5");
    if let Some(suffix) = preferred_suffix {
        let exact = format!("{prefix}-{suffix}");
        if let Some(entry) = catalog
            .entries
            .iter()
            .find(|e| e.id.eq_ignore_ascii_case(&exact))
        {
            return Cow::Owned(entry.id.clone());
        }
    }
    if let Some(entry) = catalog
        .entries
        .iter()
        .find(|e| e.id.to_ascii_lowercase().starts_with(&prefix))
    {
        return Cow::Owned(entry.id.clone());
    }
    Cow::Borrowed(fallback_4_6)
}

fn normalize_model_name_in(input: &str, catalog: &ModelCatalog) -> Cow<'static, str> {
    let trimmed = input.trim();
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        // Gemini 4 Neon
        "gemini-4-neon-xhigh" | "neon-xhigh" => Cow::Borrowed("gemini-4-neon-xhigh"),
        "gemini-4-neon-high" | "gemini-4-neon" | "neon" | "neon-high" => {
            Cow::Borrowed("gemini-4-neon-high")
        }
        "gemini-4-neon-medium" | "neon-med" | "neon-medium" => {
            Cow::Borrowed("gemini-4-neon-medium")
        }
        "gemini-4-neon-low" | "neon-low" => Cow::Borrowed("gemini-4-neon-low"),
        // Gemini 4 Hydrogen
        "gemini-4-hydrogen-xhigh" | "hydrogen-xhigh" | "h2-xhigh" => {
            Cow::Borrowed("gemini-4-hydrogen-xhigh")
        }
        "gemini-4-hydrogen-high"
        | "gemini-4-hydrogen"
        | "hydrogen"
        | "h2"
        | "hydrogen-high"
        | "h2-high" => Cow::Borrowed("gemini-4-hydrogen-high"),
        "gemini-4-hydrogen-medium"
        | "hydrogen-med"
        | "hydrogen-medium"
        | "h2-med"
        | "h2-medium" => Cow::Borrowed("gemini-4-hydrogen-medium"),
        "gemini-4-hydrogen-low" | "hydrogen-low" | "h2-low" => {
            Cow::Borrowed("gemini-4-hydrogen-low")
        }
        // Gemini 4 Carbon
        "gemini-4-carbon-high" | "carbon-high" => Cow::Borrowed("gemini-4-carbon-high"),
        "gemini-4-carbon-medium"
        | "gemini-4-carbon"
        | "carbon"
        | "carbon-med"
        | "carbon-medium" => Cow::Borrowed("gemini-4-carbon-medium"),
        "gemini-4-carbon-low" | "carbon-low" => Cow::Borrowed("gemini-4-carbon-low"),
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
        // Orchestrator
        "gemini-pro-orchestrator-neon" | "orch-neon" | "orchestrator-neon" => {
            Cow::Borrowed("gemini-pro-orchestrator-neon")
        }
        "gemini-pro-orchestrator"
        | "orchestrator"
        | "orch"
        | "orch-carbon"
        | "orchestrator-carbon" => Cow::Borrowed("gemini-pro-orchestrator"),
        "gemini-pro-orchestrator-argon" | "orch-argon" | "orchestrator-argon" => {
            Cow::Borrowed("gemini-pro-orchestrator-argon")
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
        // Claude 5.5 explicit IDs & aliases (preserved intact for 11/30 cutover; never downgraded to 4.6)
        "claude-opus-5-5-high" | "opus-5.5" | "opus-5" => Cow::Borrowed("claude-opus-5-5-high"),
        "claude-opus-5-5-medium" => Cow::Borrowed("claude-opus-5-5-medium"),
        "claude-opus-5-5-low" => Cow::Borrowed("claude-opus-5-5-low"),
        "claude-sonnet-5-5-high" | "sonnet-5.5" | "sonnet-5" => {
            Cow::Borrowed("claude-sonnet-5-5-high")
        }
        "claude-sonnet-5-5-medium" => Cow::Borrowed("claude-sonnet-5-5-medium"),
        "claude-sonnet-5-5-low" => Cow::Borrowed("claude-sonnet-5-5-low"),
        // Claude 4.6 explicit IDs & aliases
        "claude-opus-4-6-thinking" | "claude-opus-4-6" | "opus-4.6" | "opus-4-6" => {
            Cow::Borrowed("claude-opus-4-6-thinking")
        }
        "claude-sonnet-4-6" | "sonnet-4.6" | "sonnet-4-6" => Cow::Borrowed("claude-sonnet-4-6"),
        // Generic Claude shorthands: prefer live 5.5 in catalog when available, otherwise fall back to 4.6
        "opus" | "claude-opus" | "opus-high" => {
            prefer_live_claude_model_in(catalog, "opus", Some("high"), "claude-opus-4-6-thinking")
        }
        "opus-med" | "opus-medium" => {
            prefer_live_claude_model_in(catalog, "opus", Some("medium"), "claude-opus-4-6-thinking")
        }
        "opus-low" => {
            prefer_live_claude_model_in(catalog, "opus", Some("low"), "claude-opus-4-6-thinking")
        }
        "sonnet" | "claude" | "claude-sonnet" | "sonnet-high" => {
            prefer_live_claude_model_in(catalog, "sonnet", Some("high"), "claude-sonnet-4-6")
        }
        "sonnet-med" | "sonnet-medium" => {
            prefer_live_claude_model_in(catalog, "sonnet", Some("medium"), "claude-sonnet-4-6")
        }
        "sonnet-low" => {
            prefer_live_claude_model_in(catalog, "sonnet", Some("low"), "claude-sonnet-4-6")
        }
        // GPT-OSS 120B
        "gpt-oss-120b-medium" | "oss" | "gpt" | "gpt-oss" | "120b" => {
            Cow::Borrowed("gpt-oss-120b-medium")
        }
        // Default / System Reset
        "default" | "reset" | "system" | "" => Cow::Borrowed(""),
        _ => {
            if let Some(entry) = catalog
                .entries
                .iter()
                .find(|e| e.id.eq_ignore_ascii_case(trimmed))
            {
                Cow::Owned(entry.id.clone())
            } else {
                Cow::Owned(trimmed.to_string())
            }
        }
    }
}

/// Normalizes shorthand or legacy model aliases into canonical live Antigravity model IDs.
#[must_use]
pub fn normalize_model_name(input: &str) -> Cow<'static, str> {
    let guard = match registry().read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    normalize_model_name_in(input, &guard)
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

/// Returns `true` if `agy` accepts `--effort` alongside `model`.
///
/// Models that already encode an effort tier (`-low`, `-medium`, `-high`, `-xhigh`, `-max`),
/// Claude models, Orchestrators, and GPT-OSS models reject conflicting or unsupported `--effort`.
#[must_use]
pub fn supports_effort_flag(model: &str) -> bool {
    let m = model.trim();
    if m.is_empty() {
        return true;
    }
    if effort_from_model_suffix(m).is_some() {
        return false;
    }
    let lower = m.to_ascii_lowercase();
    !lower.starts_with("claude")
        && !lower.starts_with("gemini-pro-orchestrator")
        && !lower.starts_with("gpt-oss")
}

/// Computes the exact `--effort` flag value to pass to `agy`, or `None` when `--effort` must be omitted.
///
/// When `model` is empty (CLI default `Gemini 4 Neon`), `low | medium | high | xhigh` are
/// natively supported and only `"max"` is clamped down to `"xhigh"`.
#[must_use]
pub fn effective_cli_effort<'a>(model: &str, effort: &'a str) -> Option<&'a str> {
    let eff = effort.trim();
    if eff.is_empty() || !supports_effort_flag(model) {
        return None;
    }
    if model.trim().is_empty() && eff.eq_ignore_ascii_case("max") {
        return Some("xhigh");
    }
    Some(eff)
}

/// Finds a sibling model variant matching `target_effort` in the same family as `current_model`
/// (for example switching `gemini-4-carbon-high` + `low` -> `gemini-4-carbon-low`).
#[must_use]
pub fn sibling_model_for_effort(current_model: &str, target_effort: Effort) -> Option<String> {
    let trimmed = current_model.trim();
    if trimmed.is_empty() || effort_from_model_suffix(trimmed).is_none() {
        return None;
    }
    let stem = strip_effort_suffix_from_id(trimmed);
    let candidate = format!("{stem}-{}", target_effort.as_str());
    if is_valid_model(&candidate) {
        Some(candidate)
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

/// Validates a dynamic model identifier (allows up to 128 bytes and `/` or `:` for gateway models,
/// while rejecting leading `-`, whitespace, and control characters).
#[must_use]
pub fn is_safe_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/' | ':'))
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
        for e in [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ] {
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
        for m in [Mode::Auto, Mode::AcceptEdits, Mode::Plan] {
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
        // Without live 5.5 entries in catalog, generic opus/sonnet fall back to 4.6.
        assert_eq!(normalize_model_name("opus"), "claude-opus-4-6-thinking");
        assert_eq!(normalize_model_name("sonnet"), "claude-sonnet-4-6");
        // Explicit Claude 5.5 IDs & aliases must pass through intact (never rewritten to 4.6).
        assert_eq!(
            normalize_model_name("claude-opus-5-5-high"),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            normalize_model_name("claude-opus-5-5-medium"),
            "claude-opus-5-5-medium"
        );
        assert_eq!(
            normalize_model_name("claude-opus-5-5-low"),
            "claude-opus-5-5-low"
        );
        assert_eq!(normalize_model_name("opus-5.5"), "claude-opus-5-5-high");
        assert_eq!(normalize_model_name("opus-5"), "claude-opus-5-5-high");
        assert_eq!(
            normalize_model_name("claude-sonnet-5-5-medium"),
            "claude-sonnet-5-5-medium"
        );
        assert_eq!(normalize_model_name("sonnet-5.5"), "claude-sonnet-5-5-high");
        assert_eq!(normalize_model_name("sonnet-5"), "claude-sonnet-5-5-high");
        assert_eq!(
            normalize_model_name("claude-opus-5-5-thinking"),
            "claude-opus-5-5-thinking"
        );

        // When live catalog includes Claude 5.5, generic opus/sonnet prefer 5.5 over 4.6.
        let mut live_catalog = ModelCatalog::seeded();
        live_catalog.merge_live(vec![
            (
                "claude-opus-5-5-high".to_string(),
                "Claude Opus 5.5 (High)".to_string(),
            ),
            (
                "claude-opus-5-5-medium".to_string(),
                "Claude Opus 5.5 (Medium)".to_string(),
            ),
            (
                "claude-sonnet-5-5-high".to_string(),
                "Claude Sonnet 5.5 (High)".to_string(),
            ),
            (
                "claude-sonnet-5-5-medium".to_string(),
                "Claude Sonnet 5.5 (Medium)".to_string(),
            ),
        ]);
        assert_eq!(
            normalize_model_name_in("opus", &live_catalog),
            "claude-opus-5-5-high"
        );
        assert_eq!(
            normalize_model_name_in("opus-med", &live_catalog),
            "claude-opus-5-5-medium"
        );
        assert_eq!(
            normalize_model_name_in("sonnet", &live_catalog),
            "claude-sonnet-5-5-high"
        );
        assert_eq!(
            normalize_model_name_in("sonnet-medium", &live_catalog),
            "claude-sonnet-5-5-medium"
        );
        // Explicit 4.6 aliases still map to 4.6.
        assert_eq!(
            normalize_model_name_in("opus-4.6", &live_catalog),
            "claude-opus-4-6-thinking"
        );

        // Gemini 4 Neon & Orchestrator Neon aliases.
        assert_eq!(normalize_model_name("neon"), "gemini-4-neon-high");
        assert_eq!(normalize_model_name("neon-xhigh"), "gemini-4-neon-xhigh");
        assert_eq!(normalize_model_name("neon-high"), "gemini-4-neon-high");
        assert_eq!(normalize_model_name("neon-med"), "gemini-4-neon-medium");
        assert_eq!(normalize_model_name("neon-medium"), "gemini-4-neon-medium");
        assert_eq!(normalize_model_name("neon-low"), "gemini-4-neon-low");
        assert_eq!(
            normalize_model_name("orch-neon"),
            "gemini-pro-orchestrator-neon"
        );
        assert_eq!(
            normalize_model_name("orchestrator-neon"),
            "gemini-pro-orchestrator-neon"
        );

        assert_eq!(normalize_model_name("carbon"), "gemini-4-carbon-medium");
        assert_eq!(normalize_model_name("h2-xhigh"), "gemini-4-hydrogen-xhigh");
        assert_eq!(normalize_model_name("orch"), "gemini-pro-orchestrator");
        assert_eq!(
            normalize_model_name("orch-argon"),
            "gemini-pro-orchestrator-argon"
        );
        assert_eq!(normalize_model_name("flash"), "gemini-3.8-flash-high");
        assert_eq!(normalize_model_name("default"), "");
        assert!(is_valid_model(""));
        assert!(is_valid_model("gemini-4-neon-xhigh"));
        assert!(is_valid_model("gemini-pro-orchestrator-neon"));
        assert!(is_valid_model(&normalize_model_name("argon")));
        assert!(is_valid_model("gemini-pro-orchestrator-argon"));
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
    fn effort_compatibility_prevents_cli_rejection() {
        assert!(!supports_effort_flag("claude-opus-4-6-thinking"));
        assert!(!supports_effort_flag("claude-opus-5-5-high"));
        assert!(!supports_effort_flag("claude-sonnet-4-6"));
        assert!(!supports_effort_flag("gemini-pro-orchestrator"));
        assert!(!supports_effort_flag("gemini-pro-orchestrator-neon"));
        assert!(!supports_effort_flag("gemini-pro-orchestrator-argon"));
        assert!(!supports_effort_flag("gemini-4-neon-xhigh"));
        assert!(!supports_effort_flag("gemini-4-carbon-high"));
        assert!(!supports_effort_flag("gpt-oss-120b-medium"));
        assert!(supports_effort_flag(""));
        assert!(supports_effort_flag("gemini-4-carbon"));

        assert_eq!(
            effective_cli_effort("gemini-pro-orchestrator", "high"),
            None
        );
        assert_eq!(effective_cli_effort("claude-sonnet-4-6", "high"), None);
        // Default model (Gemini 4 Neon) natively supports xhigh; only max is clamped to xhigh.
        assert_eq!(effective_cli_effort("", "xhigh"), Some("xhigh"));
        assert_eq!(effective_cli_effort("", "max"), Some("xhigh"));
        assert_eq!(effective_cli_effort("", "high"), Some("high"));
        assert_eq!(effective_cli_effort("", "low"), Some("low"));
        assert_eq!(
            sibling_model_for_effort("gemini-4-neon-high", Effort::XHigh).as_deref(),
            Some("gemini-4-neon-xhigh")
        );
        assert_eq!(
            sibling_model_for_effort("gemini-4-carbon-high", Effort::Low).as_deref(),
            Some("gemini-4-carbon-low")
        );
        assert_eq!(
            sibling_model_for_effort("gemini-4-carbon-high", Effort::XHigh),
            None
        );
        assert_eq!(
            sibling_model_for_effort("gemini-4-hydrogen-high", Effort::XHigh).as_deref(),
            Some("gemini-4-hydrogen-xhigh")
        );
    }

    #[test]
    fn parse_and_merge_dynamic_models_and_compact_callbacks() {
        let raw = "\u{280b} Fetching available models...\n\
                   gemini-4-carbon-high\tGemini 4 Carbon (High)\n\
                   gemini-4.5-neon-xhigh\tGemini 4.5 Neon (XHigh)\n\
                   enterprise/very-long-gateway-provider/models/gemini-custom-reasoning-ultra-v2-high\tCustom Gateway Ultra\n";
        let parsed = parse_agy_models_output(raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, "gemini-4-carbon-high");
        assert_eq!(parsed[1].0, "gemini-4.5-neon-xhigh");

        let mut isolated = ModelCatalog::seeded();
        let added = isolated.merge_live(parsed);
        assert_eq!(added, 2);
        assert!(isolated.known_ids.contains("gemini-4.5-neon-xhigh"));

        let long_id =
            "enterprise/very-long-gateway-provider/models/gemini-custom-reasoning-ultra-v2-high";
        assert!(isolated.known_ids.contains(long_id));

        // encode_model_callback registers long IDs into hash lookup without mutating the active list.
        let tok_long = encode_model_callback(long_id);
        assert!(tok_long.len() <= 64, "token too long: {tok_long}");
        assert!(tok_long.starts_with("mh:"));
        assert_eq!(decode_model_callback(&tok_long).as_deref(), Some(long_id));

        let tok_orch = encode_model_callback("gemini-pro-orchestrator-argon");
        assert!(tok_orch.len() <= 64);
        assert_eq!(tok_orch, "ms:gemini-pro-orchestrator-argon");
        assert_eq!(
            decode_model_callback(&tok_orch).as_deref(),
            Some("gemini-pro-orchestrator-argon")
        );

        let tok_def = encode_model_callback("");
        assert_eq!(tok_def, "ms:default");
        assert_eq!(decode_model_callback(&tok_def).as_deref(), Some(""));
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
            assert!(!is_safe_model_id(evil));
            assert!(!is_safe_free_value(evil));
        }
        assert!(is_safe_cli_value("gemini-3.8-flash-high"));
        assert!(!is_safe_cli_value(&"a".repeat(65)));
        assert!(is_safe_model_id(&"a".repeat(100)));
        assert!(!is_safe_model_id(&"a".repeat(129)));
        assert!(is_safe_free_value(&"a".repeat(65)));
        assert!(!is_safe_free_value(&"a".repeat(129)));
    }
}
