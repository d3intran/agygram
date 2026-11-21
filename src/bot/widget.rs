//! Telegram Bot API 10.4+ QuickJS In-Message Widget, Card & Offline Mini-Game engine.
//!
//! Provides:
//! - Payload optimization (`minify_widget_html`), offline sandbox sanitization (`sanitize_widget_html`),
//!   and strict Bot API 10.4 size validation (`<= 32 KB` raw UTF-8, `<= 4 KB` zlib level-9 compressed).
//! - Multi-template interactive cards (`AgentCardData`, `CustomCardSpec`: metrics/dashboard, quiz,
//!   comparison table, status/timeline) plus 4 built-in zero-dependency QuickJS offline mini-games
//!   (`2048`, `Minesweeper`, `MemoryMatch`, `ReactionTap`), all under 4 KB compressed.
//! - End-to-end AI generation protocol (`WIDGET_PROMPT_GUIDE`, `extract_ai_widgets`,
//!   `extract_ai_widgets_with_roots`) and automatic delivery fallback (`deliver_widget_with_fallback`)
//!   that uses `sendWidget` in eligible private chats and seamlessly degrades to playable `.html`
//!   file attachments or formatted Telegram HTML summaries when `sendWidget` is unavailable.

use crate::bot::formatter::TelegramFormatter;
use crate::text;
use flate2::Compression;
use flate2::write::ZlibEncoder;
use reqwest::Client;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardMarkup, InputFile, ParseMode};
use tracing::{debug, info, warn};

/// Maximum raw HTML byte length allowed by Telegram Bot API 10.4 for in-message widgets (32 KB).
pub const MAX_RAW_HTML_BYTES: usize = 32 * 1024;

/// Maximum zlib-compressed HTML byte length allowed by Telegram Bot API 10.4 servers (4 KB).
pub const MAX_COMPRESSED_HTML_BYTES: usize = 4 * 1024;

/// Maximum byte length of `chat_data` / `public_data` state strings in Bot API 10.4 (1 KB).
pub const MAX_WIDGET_DATA_BYTES: usize = 1024;

/// Maximum file size when loading `.widget.html` artifacts from workspace for fallback `.html` delivery.
pub const MAX_STANDALONE_WIDGET_FILE_BYTES: usize = 512 * 1024;

const TELEGRAM_TEXT_LIMIT: usize = 4000;
const MAX_CAPTION_CHARS: usize = 1024;
const DEFAULT_TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_WIDGET_UNAVAILABLE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_WIDGET_UNAVAILABLE_CACHE_ENTRIES: usize = 64;
const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Strict JSON Schema matching [`CustomCardSpec`] for `agy --json-schema` structured card replies.
pub const TG_CARD_JSON_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["title"],
  "properties": {
    "kind": { "type": "string", "enum": ["card", "metric", "quiz", "table", "status"] },
    "title": { "type": "string", "minLength": 1 },
    "subtitle": { "type": "string" },
    "badge": { "type": "string" },
    "badge_tone": { "type": "string", "enum": ["ok", "warn", "err", "info"] },
    "summary": { "type": "string" },
    "metrics": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["label", "value"],
        "properties": {
          "label": { "type": "string" },
          "value": { "type": "string" },
          "delta": { "type": "string" },
          "tone": { "type": "string", "enum": ["ok", "warn", "err", "info"] }
        }
      }
    },
    "sections": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["title"],
        "properties": {
          "title": { "type": "string" },
          "body": { "type": "string" },
          "items": { "type": "array", "items": { "type": "string" } }
        }
      }
    },
    "table": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "headers": { "type": "array", "items": { "type": "string" } },
        "rows": {
          "type": "array",
          "items": { "type": "array", "items": { "type": "string" } }
        }
      }
    },
    "steps": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["title", "details"],
        "properties": {
          "title": { "type": "string" },
          "details": { "type": "string" },
          "is_expanded": { "type": "boolean" }
        }
      }
    },
    "quiz": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["question", "options", "answer_index"],
        "properties": {
          "question": { "type": "string" },
          "options": { "type": "array", "items": { "type": "string" }, "minItems": 2 },
          "answer_index": { "type": "integer", "minimum": 0 },
          "explanation": { "type": "string" }
        }
      }
    },
    "footer": { "type": "string" }
  }
}"#;

/// Concise system-prompt addendum teaching `agy` how to emit interactive Telegram cards
/// (` ```tg-card `) and standalone QuickJS offline mini-games/apps (` ```tg-widget `) when asked.
pub const WIDGET_PROMPT_GUIDE: &str = r#"Telegram Interactive Widget & Mini-Game Output Protocol:
Only when the user asks for an interactive card, dashboard, quiz, comparison card, or offline mini-game/widget, emit one of these fenced blocks:
1) ```tg-widget (standalone single-file HTML5 + inline <style> + inline <script> for QuickJS Widget Profile 1 & WebView):
   - Strictly offline: NO fetch/XHR/WebSocket/external <script src>/<link>/<img src="http">; NO <input>/<textarea>/<select> (use <button>/clickable <div> grids); NO alert()/confirm()/prompt(); wrap localStorage in try/catch.
   - Mobile-first compact layout (<=340px wide, <=540px tall, no internal scrollbars), use var(--tg-theme-bg-color,#18222d) / var(--tg-theme-text-color,#fff) + @media(prefers-color-scheme:light), touch-friendly buttons/swipes, optional TelegramWidget.HapticFeedback.
   - Keep <10 KB raw (<3.8 KB zlib-compressed) for inline QuickJS widgets; larger HTML auto-delivers as a playable .html attachment.
2) ```tg-card (structured JSON rendered into a compact multi-tab QuickJS card <4 KB compressed):
   {"kind":"metric|quiz|table|status|card","title":"...","subtitle":"...","badge":"...","badge_tone":"ok|warn|err|info","summary":"...","metrics":[{"label":"...","value":"...","delta":"..."}],"sections":[{"title":"...","body":"...","items":["..."]}],"table":{"headers":[...],"rows":[[...]]},"steps":[{"title":"...","details":"...","is_expanded":false}],"quiz":[{"question":"...","options":[...],"answer_index":0,"explanation":"..."}],"footer":"..."}
Do NOT emit tg-widget/tg-card fences for normal conversation or regular code snippets."#;

static WIDGET_HTTP_CLIENT_STATE: OnceLock<RwLock<(Option<String>, Client)>> = OnceLock::new();
/// Caches hashed `bot_token`s for which Telegram returned `400 INPUT_MEDIA_WIDGET_NOT_ALLOWED`,
/// avoiding repeated failing `sendWidget` round-trips and WARN log noise for 24 hours.
static SEND_WIDGET_UNAVAILABLE_CACHE: OnceLock<RwLock<FxHashMap<u64, Instant>>> = OnceLock::new();

#[inline]
const fn hash_bot_token(bot_token: &str) -> u64 {
    let bytes = bot_token.as_bytes();
    let mut hash = FNV1A_OFFSET_BASIS;
    let mut i = 0usize;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV1A_PRIME);
        i += 1;
    }
    hash
}

fn is_send_widget_not_allowed_error(err: &WidgetError) -> bool {
    matches!(
        err,
        WidgetError::ApiError { code: 400, description }
            if description.contains("INPUT_MEDIA_WIDGET_NOT_ALLOWED")
    )
}

fn is_send_widget_cached_unavailable(bot_token: &str) -> bool {
    let Some(lock) = SEND_WIDGET_UNAVAILABLE_CACHE.get() else {
        return false;
    };
    let key = hash_bot_token(bot_token);
    let guard = match lock.read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.get(&key).is_some_and(|recorded| {
        Instant::now().saturating_duration_since(*recorded) < SEND_WIDGET_UNAVAILABLE_TTL
    })
}

fn mark_send_widget_unavailable(bot_token: &str) {
    let lock = SEND_WIDGET_UNAVAILABLE_CACHE.get_or_init(|| RwLock::new(FxHashMap::default()));
    let mut guard = match lock.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    guard.retain(|_, recorded| {
        now.saturating_duration_since(*recorded) < SEND_WIDGET_UNAVAILABLE_TTL
    });
    if guard.len() >= MAX_WIDGET_UNAVAILABLE_CACHE_ENTRIES {
        guard.clear();
    }
    guard.insert(hash_bot_token(bot_token), now);
}

#[cfg(test)]
fn clear_send_widget_unavailable_cache() {
    if let Some(lock) = SEND_WIDGET_UNAVAILABLE_CACHE.get() {
        let mut guard = match lock.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clear();
    }
}

/// Sanitizes a `reqwest::Error` by stripping the full request URL (and redacting `bot_token` as
/// defense-in-depth) so Telegram credentials can never appear in logs or user-facing error text.
fn map_http_error(err: reqwest::Error, bot_token: &str) -> WidgetError {
    let sanitized = err.without_url().to_string();
    if !bot_token.is_empty() && sanitized.contains(bot_token) {
        WidgetError::Http(sanitized.replace(bot_token, "<redacted>"))
    } else {
        WidgetError::Http(sanitized)
    }
}

/// Error types encountered when creating, validating, or sending Telegram 10.4 widgets.
#[derive(Debug, thiserror::Error)]
pub enum WidgetError {
    #[error("HTML payload exceeds {max} bytes raw limit ({size} bytes)")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("HTML payload exceeds {max} bytes compressed limit ({compressed} bytes zlib-9)")]
    CompressedPayloadTooLarge { compressed: usize, max: usize },

    #[error("HTML payload cannot be empty")]
    EmptyPayload,

    #[error("Invalid widget HTML content: {0}")]
    InvalidHtmlContent(String),

    #[error("Invalid card JSON spec: {0}")]
    InvalidCardSpec(String),

    #[error("Telegram 10.4 widgets are only supported in private chats (chat_id: {0})")]
    NonPrivateChat(i64),

    #[error("Telegram Bot API error ({code}): {description}")]
    ApiError { code: i32, description: String },

    #[error("HTTP transport error: {0}")]
    Http(String),

    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid widget parameter: {0}")]
    InvalidParameter(String),
}

/// Response returned by Telegram Bot API on successful `sendWidget`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WidgetMessageResponse {
    pub ok: bool,
    pub message_id: i32,
    pub chat_id: Option<i64>,
    pub date: Option<i64>,
}

/// Outcome returned by [`deliver_widget_with_fallback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetDeliveryOutcome {
    /// Sent natively as an in-message Telegram QuickJS Widget.
    Widget { message_id: i32 },
    /// Delivered as a standalone playable `.html` document attachment.
    HtmlDocumentFallback,
    /// Delivered as a formatted Telegram HTML text message (or chunked plain fallback).
    HtmlTextFallback,
}

/// High-level category governing automatic fallback behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    /// Mini-games, calculators, interactive visualizations -> falls back to `.html` document.
    InteractiveApp,
    /// Information/dashboard/quiz/status cards -> falls back to rich Telegram HTML text.
    Card,
}

/// Compiled widget ready for Telegram `sendWidget` delivery or automatic fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundWidget {
    pub kind: WidgetKind,
    pub title: String,
    pub file_name: String,
    pub html: String,
    pub fallback_html: String,
    pub caption: Option<String>,
    pub raw_bytes: usize,
    pub compressed_bytes: usize,
    pub warnings: Vec<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
}

impl OutboundWidget {
    /// Returns `true` if this widget meets all Telegram Bot API 10.4 `sendWidget` payload limits.
    #[must_use]
    pub fn is_send_widget_eligible(&self) -> bool {
        !self.html.trim().is_empty()
            && !self.html.contains('\0')
            && self.raw_bytes <= MAX_RAW_HTML_BYTES
            && self.compressed_bytes <= MAX_COMPRESSED_HTML_BYTES
    }

    /// Sanitizes, minifies, inspects, and wraps arbitrary standalone HTML into an [`OutboundWidget`].
    pub fn from_raw_html(
        title: impl Into<String>,
        file_name: impl Into<String>,
        raw_html: &str,
        kind: WidgetKind,
    ) -> Result<Self, WidgetError> {
        let trimmed = raw_html.trim();
        if trimmed.is_empty() {
            return Err(WidgetError::EmptyPayload);
        }

        let (sanitized, warnings) = sanitize_widget_html(trimmed);
        let minified = minify_widget_html(&sanitized);
        if minified.len() > MAX_STANDALONE_WIDGET_FILE_BYTES {
            return Err(WidgetError::PayloadTooLarge {
                size: minified.len(),
                max: MAX_STANDALONE_WIDGET_FILE_BYTES,
            });
        }

        let inspection = inspect_widget_html(&minified);
        let title_str = title.into();
        let clean_title = if title_str.trim().is_empty() {
            extract_html_title(&minified)
                .unwrap_or_else(|| "Telegram Interactive Widget".to_string())
        } else {
            title_str
        };

        let raw_name = file_name.into();
        let final_file_name = if raw_name.trim().is_empty() {
            suggest_html_filename(&clean_title)
        } else {
            ensure_html_extension(&raw_name)
        };

        let caption = Some(truncate_caption(&format!("🎮 {}", clean_title)));

        let fallback_html = format!(
            "🎮 <b>{}</b>\n<i>已生成独立交互式 HTML 组件 ({:.1} KB / 压缩 {} B)，可在 Telegram 内置浏览器中直接打开运行。</i>",
            text::escape_html(&clean_title),
            inspection.raw_bytes as f64 / 1024.0,
            inspection.compressed_bytes
        );

        Ok(Self {
            kind,
            title: clean_title,
            file_name: final_file_name,
            html: minified,
            fallback_html,
            caption,
            raw_bytes: inspection.raw_bytes,
            compressed_bytes: inspection.compressed_bytes,
            warnings: [warnings, inspection.warnings].concat(),
            width: Some(0),
            height: Some(0),
        })
    }

    /// Compiles an [`AgentCardData`] into an [`OutboundWidget`].
    pub fn from_agent_card(card: &AgentCardData) -> Result<Self, WidgetError> {
        let html = card.render_html()?;
        let inspection = inspect_widget_html(&html);
        Ok(Self {
            kind: WidgetKind::Card,
            title: card.title.clone(),
            file_name: suggest_html_filename(&card.title),
            html,
            fallback_html: card.render_fallback_html(),
            caption: Some(truncate_caption(&format!(
                "🎴 {} · {}",
                card.title, card.model
            ))),
            raw_bytes: inspection.raw_bytes,
            compressed_bytes: inspection.compressed_bytes,
            warnings: inspection.warnings,
            width: Some(0),
            height: Some(0),
        })
    }

    /// Compiles a [`CustomCardSpec`] into an [`OutboundWidget`].
    pub fn from_custom_card(card: &CustomCardSpec) -> Result<Self, WidgetError> {
        let html = card.render_html()?;
        let inspection = inspect_widget_html(&html);
        Ok(Self {
            kind: WidgetKind::Card,
            title: card.title.clone(),
            file_name: suggest_html_filename(&card.title),
            html,
            fallback_html: card.render_fallback_html(),
            caption: Some(truncate_caption(&format!("🎴 {}", card.title))),
            raw_bytes: inspection.raw_bytes,
            compressed_bytes: inspection.compressed_bytes,
            warnings: inspection.warnings,
            width: Some(0),
            height: Some(0),
        })
    }

    /// Compiles a [`BuiltinGame`] into an [`OutboundWidget`].
    pub fn from_builtin_game(game: BuiltinGame) -> Result<Self, WidgetError> {
        game.to_outbound_widget()
    }
}

/// Diagnostic inspection metrics for a compiled HTML widget payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetInspection {
    pub raw_bytes: usize,
    pub compressed_bytes: usize,
    pub warnings: Vec<String>,
    pub send_widget_eligible: bool,
}

/// Measures exact zlib level-9 compressed byte length (matching Telegram Bot API 10.4 validation).
#[must_use]
pub fn measure_compressed_bytes(html: &str) -> usize {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
    if encoder.write_all(html.as_bytes()).is_err() {
        return usize::MAX;
    }
    encoder.finish().map_or(usize::MAX, |v| v.len())
}

/// Inspects an HTML payload for size limits and offline QuickJS compatibility warnings.
#[must_use]
pub fn inspect_widget_html(html: &str) -> WidgetInspection {
    let raw_bytes = html.len();
    let compressed_bytes = measure_compressed_bytes(html);
    let mut warnings = detect_sandbox_warnings(html);
    if raw_bytes > MAX_RAW_HTML_BYTES {
        warnings.push(format!(
            "raw HTML exceeds 32 KB limit ({raw_bytes} > {MAX_RAW_HTML_BYTES} bytes); will use .html fallback"
        ));
    }
    if compressed_bytes > MAX_COMPRESSED_HTML_BYTES {
        warnings.push(format!(
            "compressed HTML exceeds 4 KB limit ({compressed_bytes} > {MAX_COMPRESSED_HTML_BYTES} bytes); will use .html fallback"
        ));
    }
    let send_widget_eligible = !html.trim().is_empty()
        && !html.contains('\0')
        && raw_bytes <= MAX_RAW_HTML_BYTES
        && compressed_bytes <= MAX_COMPRESSED_HTML_BYTES;

    WidgetInspection {
        raw_bytes,
        compressed_bytes,
        warnings,
        send_widget_eligible,
    }
}

/// Checks raw HTML invariants (non-empty, no NUL bytes, `<= 32 KB`).
pub fn validate_widget_raw(html: &str) -> Result<(), WidgetError> {
    if html.trim().is_empty() {
        return Err(WidgetError::EmptyPayload);
    }
    if html.contains('\0') {
        return Err(WidgetError::InvalidHtmlContent(
            "HTML payload cannot contain NUL bytes".to_string(),
        ));
    }
    if html.len() > MAX_RAW_HTML_BYTES {
        return Err(WidgetError::PayloadTooLarge {
            size: html.len(),
            max: MAX_RAW_HTML_BYTES,
        });
    }
    Ok(())
}

/// Checks that HTML is non-empty, NUL-free, `<= 32 KB` raw, and `<= 4 KB` zlib-9 compressed.
pub fn validate_widget_html(html: &str) -> Result<(), WidgetError> {
    validate_widget_raw(html)?;
    let compressed = measure_compressed_bytes(html);
    if compressed > MAX_COMPRESSED_HTML_BYTES {
        return Err(WidgetError::CompressedPayloadTooLarge {
            compressed,
            max: MAX_COMPRESSED_HTML_BYTES,
        });
    }
    Ok(())
}

/// Escapes HTML special characters to prevent XSS / broken markup.
#[must_use]
pub fn escape_html(input: &str) -> String {
    text::escape_html(input).into_owned()
}

/// Sanitizes an HTML widget for Telegram's offline QuickJS sandbox and standalone WebView:
/// strips NUL bytes and external `<script src>` / `<link rel="stylesheet">`, wraps bare fragments
/// into a complete responsive HTML5 document with `--tg-theme-*` CSS fallbacks, and reports
/// non-fatal compatibility warnings.
#[must_use]
pub fn sanitize_widget_html(html: &str) -> (String, Vec<String>) {
    let mut warnings = Vec::new();
    let mut cleaned = if html.contains('\0') {
        warnings.push("removed NUL bytes from HTML payload".to_string());
        html.replace('\0', "")
    } else {
        html.to_string()
    };

    let (without_scripts, removed_scripts) = strip_external_tags(&cleaned, "script", "src", true);
    if removed_scripts > 0 {
        cleaned = without_scripts;
        warnings.push(format!(
            "stripped {removed_scripts} external <script src=...> tag(s) unsupported in offline sandbox"
        ));
    }

    let (without_links, removed_links) = strip_external_tags(&cleaned, "link", "href", false);
    if removed_links > 0 {
        cleaned = without_links;
        warnings.push(format!(
            "stripped {removed_links} external <link href=...> tag(s) unsupported in offline sandbox"
        ));
    }

    let lower = cleaned.to_ascii_lowercase();
    let is_full_doc = lower.contains("<html") || lower.contains("<!doctype");

    if !is_full_doc {
        let mut wrapped = String::with_capacity(cleaned.len() + 640);
        wrapped.push_str(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1,user-scalable=no\">\
             <style>:root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);\
             --ht:var(--tg-theme-hint-color,#8b9bb4);--bt:var(--tg-theme-button-color,#2b5278);\
             --bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#131b24)}\
             @media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);\
             --fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);\
             --bt:var(--tg-theme-button-color,#2481cc);--bf:var(--tg-theme-button-text-color,#fff);\
             --cd:var(--tg-theme-secondary-bg-color,#f4f4f5)}}\
             *{box-sizing:border-box}body{margin:0;padding:8px;font-family:-apple-system,BlinkMacSystemFont,\
             Roboto,sans-serif;background:var(--bg);color:var(--fg)}</style></head><body>",
        );
        wrapped.push_str(cleaned.trim());
        wrapped.push_str("</body></html>");
        cleaned = wrapped;
    } else if !lower.contains("name=\"viewport\"")
        && !lower.contains("name='viewport'")
        && let Some(pos) = find_tag_end_ci(&cleaned, "<head")
    {
        let mut with_vp = String::with_capacity(cleaned.len() + 96);
        with_vp.push_str(&cleaned[..=pos]);
        with_vp.push_str(
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1,user-scalable=no\">",
        );
        with_vp.push_str(&cleaned[pos + 1..]);
        cleaned = with_vp;
    }

    warnings.extend(detect_sandbox_warnings(&cleaned));
    (cleaned, warnings)
}

fn detect_sandbox_warnings(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut warnings = Vec::new();

    if lower.contains("fetch(")
        || lower.contains("xmlhttprequest")
        || lower.contains("new websocket")
        || lower.contains("eventsource")
        || lower.contains("sendbeacon")
    {
        warnings.push(
            "detected network API call (blocked in Telegram offline QuickJS sandbox)".to_string(),
        );
    }
    if has_tag_ci(&lower, "<input")
        || has_tag_ci(&lower, "<textarea")
        || has_tag_ci(&lower, "<select")
    {
        warnings.push(
            "detected form control tag (<input>/<textarea>/<select>), which is stripped by Telegram Widget Profile 1".to_string(),
        );
    }
    if lower.contains("alert(") || lower.contains("confirm(") || lower.contains("prompt(") {
        warnings.push(
            "detected browser modal call (alert/confirm/prompt) unsupported in QuickJS".to_string(),
        );
    }
    if lower.contains("eval(") || lower.contains("new function(") {
        warnings.push("detected dynamic eval/Function call (discouraged in QuickJS)".to_string());
    }
    if lower.contains("src=\"http") || lower.contains("src='http") || lower.contains("url(http") {
        warnings.push(
            "detected external HTTP asset reference (blocked in offline sandbox)".to_string(),
        );
    }
    warnings
}

fn has_tag_ci(lower_html: &str, prefix: &str) -> bool {
    let mut start = 0usize;
    while let Some(rel) = lower_html[start..].find(prefix) {
        let after = start + rel + prefix.len();
        if lower_html
            .as_bytes()
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            return true;
        }
        start = after;
    }
    false
}

fn find_tag_end_ci(html: &str, open_prefix: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut start = 0usize;
    while let Some(rel) = lower[start..].find(open_prefix) {
        let pos = start + rel;
        let after = pos + open_prefix.len();
        if lower
            .as_bytes()
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            if let Some(gt_rel) = html[after..].find('>') {
                return Some(after + gt_rel);
            }
            return None;
        }
        start = after;
    }
    None
}

fn strip_external_tags(
    html: &str,
    tag_name: &str,
    attr_name: &str,
    has_closing_tag: bool,
) -> (String, usize) {
    let lower = html.to_ascii_lowercase();
    let open_pat = format!("<{tag_name}");
    let close_pat = format!("</{tag_name}>");

    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut removed = 0usize;

    while let Some(rel) = lower[cursor..].find(&open_pat) {
        let tag_start = cursor + rel;
        let after_name = tag_start + open_pat.len();
        if !lower
            .as_bytes()
            .get(after_name)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
        {
            out.push_str(&html[cursor..after_name]);
            cursor = after_name;
            continue;
        }

        let Some(gt_rel) = html[after_name..].find('>') else {
            break;
        };
        let tag_end = after_name + gt_rel + 1;
        let open_tag_lower = &lower[tag_start..tag_end];
        let is_external = is_external_attr(open_tag_lower, attr_name);

        if is_external {
            out.push_str(&html[cursor..tag_start]);
            if has_closing_tag {
                if let Some(close_rel) = lower[tag_end..].find(&close_pat) {
                    cursor = tag_end + close_rel + close_pat.len();
                } else {
                    cursor = tag_end;
                }
            } else {
                cursor = tag_end;
            }
            removed += 1;
        } else {
            out.push_str(&html[cursor..tag_end]);
            cursor = tag_end;
        }
    }

    out.push_str(&html[cursor..]);
    (out, removed)
}

fn is_external_attr(tag_lower: &str, attr_name: &str) -> bool {
    let patterns = [
        format!("{attr_name}=\"http://"),
        format!("{attr_name}=\"https://"),
        format!("{attr_name}=\"//"),
        format!("{attr_name}='http://"),
        format!("{attr_name}='https://"),
        format!("{attr_name}='//"),
    ];
    patterns.iter().any(|p| tag_lower.contains(p))
}

/// Safely minifies HTML/CSS/JS whitespace and comments without altering string literals,
/// template literals, `<pre>` / `<textarea>` blocks, or JS automatic semicolon insertion (ASI).
#[must_use]
pub fn minify_widget_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    // `to_ascii_lowercase` only maps single-byte ASCII `A-Z` -> `a-z`, preserving exact UTF-8
    // byte offsets across the entire string so we compute it once (O(N)) outside the loop.
    let lower = html.to_ascii_lowercase();
    debug_assert_eq!(lower.len(), html.len());
    let mut cursor = 0usize;

    while cursor < html.len() {
        let rest = &html[cursor..];
        let lower_rest = &lower[cursor..];

        let next_special = [
            ("<style", RawBlockKind::Style),
            ("<script", RawBlockKind::Script),
            ("<pre", RawBlockKind::Pre),
            ("<textarea", RawBlockKind::Textarea),
        ]
        .into_iter()
        .filter_map(|(pat, kind)| find_next_open_tag(lower_rest, pat).map(|idx| (idx, kind)))
        .min_by_key(|(idx, _)| *idx);

        let Some((rel_idx, kind)) = next_special else {
            minify_html_fragment(rest, &mut out);
            break;
        };

        if rel_idx > 0 {
            minify_html_fragment(&rest[..rel_idx], &mut out);
        }

        let tag_abs = cursor + rel_idx;
        let Some(gt_rel) = html[tag_abs..].find('>') else {
            minify_html_fragment(&html[tag_abs..], &mut out);
            break;
        };
        let open_end = tag_abs + gt_rel + 1;
        out.push_str(&html[tag_abs..open_end]);

        let close_tag = kind.close_tag();
        let Some(close_rel) = lower[open_end..].find(close_tag) else {
            out.push_str(&html[open_end..]);
            break;
        };

        let body_end = open_end + close_rel;
        let body = &html[open_end..body_end];
        match kind {
            RawBlockKind::Style => minify_css_into(body, &mut out),
            RawBlockKind::Script => minify_js_into(body, &mut out),
            RawBlockKind::Pre | RawBlockKind::Textarea => out.push_str(body),
        }

        let close_end = body_end + close_tag.len();
        out.push_str(&html[body_end..close_end]);
        cursor = close_end;
    }

    out.trim().to_string()
}

#[derive(Clone, Copy)]
enum RawBlockKind {
    Style,
    Script,
    Pre,
    Textarea,
}

impl RawBlockKind {
    const fn close_tag(self) -> &'static str {
        match self {
            Self::Style => "</style>",
            Self::Script => "</script>",
            Self::Pre => "</pre>",
            Self::Textarea => "</textarea>",
        }
    }
}

fn find_next_open_tag(lower_html: &str, open_prefix: &str) -> Option<usize> {
    let mut start = 0usize;
    while let Some(rel) = lower_html[start..].find(open_prefix) {
        let pos = start + rel;
        let after = pos + open_prefix.len();
        if lower_html
            .as_bytes()
            .get(after)
            .is_some_and(|b| b.is_ascii_whitespace() || *b == b'>')
        {
            return Some(pos);
        }
        start = after;
    }
    None
}

fn minify_html_fragment(fragment: &str, out: &mut String) {
    let bytes = fragment.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Strip standard HTML comments (preserving <!DOCTYPE ...>)
        if bytes[i..].starts_with(b"<!--")
            && let Some(end_rel) = fragment[i + 4..].find("-->")
        {
            i += 4 + end_rel + 3;
            continue;
        }

        if bytes[i].is_ascii_whitespace() {
            let ws_start = i;
            let mut has_newline = false;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                if bytes[i] == b'\n' || bytes[i] == b'\r' {
                    has_newline = true;
                }
                i += 1;
            }
            let prev_is_gt = out.ends_with('>');
            let next_is_lt = bytes.get(i) == Some(&b'<');
            if has_newline && (prev_is_gt || next_is_lt) {
                continue;
            }
            if prev_is_gt && next_is_lt && ws_start == 0 {
                continue;
            }
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }

        let Some(ch) = fragment[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn minify_css_into(css: &str, out: &mut String) {
    let bytes = css.as_bytes();
    let css_start = out.len();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        if let Some(q) = quote {
            if bytes[i] == b'\\' {
                out.push('\\');
                i += 1;
                if let Some(escaped) = css[i..].chars().next() {
                    out.push(escaped);
                    i += escaped.len_utf8();
                }
                continue;
            }
            let Some(ch) = css[i..].chars().next() else {
                break;
            };
            out.push(ch);
            if ch as u32 == u32::from(q) {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }

        if bytes[i..].starts_with(b"/*")
            && let Some(end_rel) = css[i + 2..].find("*/")
        {
            i += 2 + end_rel + 2;
            continue;
        }

        if bytes[i] == b'"' || bytes[i] == b'\'' {
            quote = Some(bytes[i]);
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        if bytes[i].is_ascii_whitespace() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let prev = out.as_bytes().last().copied();
            let next = bytes.get(i).copied();
            let skip_around = matches!(prev, Some(b'{' | b'}' | b';' | b',' | b':'))
                || matches!(next, Some(b'{' | b'}' | b';' | b','));
            if !skip_around && out.len() > css_start && next.is_some() {
                out.push(' ');
            }
            continue;
        }

        if bytes[i] == b'}' && out.ends_with(';') {
            out.pop();
        }

        let Some(ch) = css[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
}

/// Line-oriented JS minifier that trims indentation, drops empty/comment-only lines,
/// and preserves `\n` and template literals so ASI and multi-line strings never break.
fn minify_js_into(js: &str, out: &mut String) {
    let mut in_template = false;
    let mut in_block_comment = false;
    let mut wrote_any = false;

    for raw_line in js.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if in_template {
            if wrote_any {
                out.push('\n');
            }
            out.push_str(line);
            wrote_any = true;
            update_js_template_state(line, &mut in_template);
            continue;
        }

        let trimmed = line.trim();
        if in_block_comment {
            if let Some(end_idx) = trimmed.find("*/") {
                in_block_comment = false;
                let rest = trimmed[end_idx + 2..].trim();
                if !rest.is_empty() && !rest.starts_with("//") {
                    if wrote_any {
                        out.push('\n');
                    }
                    out.push_str(rest);
                    wrote_any = true;
                    update_js_template_state(rest, &mut in_template);
                }
            }
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if let Some(after_open) = trimmed.strip_prefix("/*") {
            if let Some(end_idx) = after_open.find("*/") {
                let rest = after_open[end_idx + 2..].trim();
                if rest.is_empty() || rest.starts_with("//") {
                    continue;
                }
                if wrote_any {
                    out.push('\n');
                }
                out.push_str(rest);
                wrote_any = true;
                update_js_template_state(rest, &mut in_template);
            } else {
                in_block_comment = true;
            }
            continue;
        }

        if wrote_any {
            out.push('\n');
        }
        out.push_str(trimmed);
        wrote_any = true;
        update_js_template_state(trimmed, &mut in_template);
    }
}

fn update_js_template_state(line: &str, in_template: &mut bool) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;

    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            i += 2;
            continue;
        }
        if *in_template {
            if b == b'`' {
                *in_template = false;
            }
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        if b == b'`' {
            *in_template = true;
        } else if b == b'\'' || b == b'"' {
            quote = Some(b);
        }
        i += 1;
    }
}

// ============================================================================
// AgentCardData (Original Card API, Updated & Preserved)
// ============================================================================

/// Dynamic execution status of an Agent represented on the widget card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Thinking,
    Executing,
    Finished,
    Error,
}

impl AgentStatus {
    #[must_use]
    pub const fn badge_class(&self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Executing => "executing",
            Self::Finished => "finished",
            Self::Error => "error",
        }
    }

    #[must_use]
    pub const fn display_label(&self) -> &'static str {
        match self {
            Self::Thinking => "思考中",
            Self::Executing => "工具执行中",
            Self::Finished => "已完成",
            Self::Error => "执行异常",
        }
    }

    #[must_use]
    pub const fn indicator_html(&self) -> &'static str {
        match self {
            Self::Thinking => r#"<span class="pulse-dot"></span>"#,
            Self::Executing => r#"<span class="spinner-icon"></span>"#,
            Self::Finished => r#"<span class="check-icon">✓</span>"#,
            Self::Error => r#"<span class="alert-icon">!</span>"#,
        }
    }
}

/// A collapsible trace/timeline step on an interactive card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardStep {
    pub title: String,
    pub details: String,
    #[serde(default)]
    pub is_expanded: bool,
}

impl AgentCardStep {
    #[must_use]
    pub fn new(title: impl Into<String>, details: impl Into<String>, is_expanded: bool) -> Self {
        Self {
            title: title.into(),
            details: details.into(),
            is_expanded,
        }
    }
}

/// Data model representing an interactive Agent Card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardData {
    pub title: String,
    pub subtitle: String,
    pub model: String,
    pub status: AgentStatus,
    pub result: String,
    #[serde(default)]
    pub steps: Vec<AgentCardStep>,
}

impl AgentCardData {
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        model: impl Into<String>,
        status: AgentStatus,
        result: impl Into<String>,
        steps: Vec<AgentCardStep>,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            model: model.into(),
            status,
            result: result.into(),
            steps,
        }
    }

    /// Creates a rich, interactive demo card demonstrating Thinking, Tool Execution, and Results.
    #[must_use]
    pub fn demo() -> Self {
        Self {
            title: "agygram Agent".to_string(),
            subtitle: "Antigravity Swarm Coordinator".to_string(),
            model: "Gemini 4 Hydrogen (XHigh)".to_string(),
            status: AgentStatus::Finished,
            result: "✅ **Telegram 10.4+ QuickJS Widget 引擎就绪**\n\n已升级支持：\n• 4 款内置零依赖离线小游戏 (2048 / 扫雷 / 记忆翻牌 / 极速反应)\n• AI 自定义卡片与离线小游戏直出 (`tg-widget` / `tg-card`)\n• 自动暗色/亮色主题匹配、严格 <=4 KB 压缩校验与 `.html` 无缝降级"
                .to_string(),
            steps: vec![
                AgentCardStep::new(
                    "Step 1: QuickJS Widget Profile 1 沙盒校验",
                    "• 运行环境: QuickJS Offline Sandbox (无网络/无表单控件)\n• 压缩限制: raw <= 32 KB, zlib-9 <= 4 KB\n• 主题适配: --tg-theme-* + prefers-color-scheme",
                    false,
                ),
                AgentCardStep::new(
                    "Step 2: 工具链与内置小游戏预检",
                    "• 2048 / 扫雷 / 记忆翻牌 / 极速反应 全部压缩至 ~1.7–2.3 KB\n• 自动过滤外链脚本并检测 fetch / <input> 兼容风险",
                    true,
                ),
                AgentCardStep::new(
                    "Step 3: 智能发送与降级通道",
                    "• 私聊且 <=4 KB: 原生 sendWidget\n• 群聊 / 超限 / 客户端限制: 自动发送可玩 .html 附件或 HTML 卡片",
                    false,
                ),
            ],
        }
    }

    /// Renders this card into a standalone, zero-dependency HTML document (raw `<= 32 KB`;
    /// checked for `<= 4 KB` compressed eligibility by [`OutboundWidget::from_agent_card`]).
    pub fn render_html(&self) -> Result<String, WidgetError> {
        let mut html = String::with_capacity(4096);

        let safe_title = escape_html(&self.title);
        let safe_subtitle = escape_html(&self.subtitle);
        let safe_model = escape_html(&self.model);
        let safe_result = escape_html(&self.result);
        let status_badge_class = self.status.badge_class();
        let status_label = self.status.display_label();
        let status_indicator = self.status.indicator_html();

        html.push_str(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1,user-scalable=no\">\
             <style>",
        );
        html.push_str(CARD_INLINE_CSS);
        html.push_str("</style></head><body><div class=\"widget-card\">");

        let _ = write!(
            html,
            "<div class=\"card-header\"><div class=\"agent-info\"><div class=\"agent-avatar\">🤖</div>\
             <div class=\"agent-meta\"><div class=\"agent-title\">{safe_title}</div>\
             <div class=\"agent-subtitle\">{safe_subtitle} · {safe_model}</div></div></div>\
             <div id=\"statusBadge\" class=\"status-badge {status_badge_class}\">{status_indicator} \
             <span>{status_label}</span></div></div>"
        );

        html.push_str(
            "<div class=\"tabs-nav\">\
             <button class=\"tab-btn active\" data-tab=\"result\" onclick=\"switchTab('result')\">📋 结果</button>\
             <button class=\"tab-btn\" data-tab=\"steps\" onclick=\"switchTab('steps')\">⏱️ 步骤</button>\
             </div><div class=\"tab-content\">",
        );

        let _ = write!(
            html,
            "<div id=\"panel-result\" class=\"tab-panel active\"><div id=\"resultBody\" class=\"result-body\">{safe_result}</div></div>"
        );

        html.push_str("<div id=\"panel-steps\" class=\"tab-panel\"><div class=\"accordion\">");
        for (idx, step) in self.steps.iter().enumerate() {
            let open_class = if step.is_expanded { " open" } else { "" };
            let safe_step_title = escape_html(&step.title);
            let safe_step_details = escape_html(&step.details);
            let _ = write!(
                html,
                "<div class=\"accordion-item{open_class}\"><div class=\"accordion-header\" onclick=\"toggleAccordion(this)\">\
                 <span class=\"step-title-text\">{safe_step_title}</span><span class=\"accordion-chevron\">▶</span></div>\
                 <div class=\"accordion-body\"><pre>{safe_step_details}</pre></div></div>"
            );
            debug!(step_index = idx, "rendered step");
        }
        html.push_str("</div></div></div>");

        html.push_str(
            "<div class=\"card-footer\"><span class=\"footer-brand\">⚡ Telegram QuickJS Widget</span>\
             <span class=\"footer-time\">agygram</span></div></div><script>",
        );
        html.push_str(CARD_INLINE_JS);
        html.push_str("</script></body></html>");

        validate_widget_raw(&html)?;
        Ok(html)
    }

    /// Renders a graceful fallback Telegram HTML message when widget rendering is unavailable.
    #[must_use]
    pub fn render_fallback_html(&self) -> String {
        let mut msg = String::with_capacity(1024);
        let status_emoji = match self.status {
            AgentStatus::Thinking => "🤔",
            AgentStatus::Executing => "⚙️",
            AgentStatus::Finished => "✅",
            AgentStatus::Error => "❌",
        };

        let _ = write!(
            msg,
            "🎴 <b>{}</b>\n<b>模型:</b> <code>{}</code>\n<b>状态:</b> {} {}\n\n",
            text::escape_html(&self.title),
            text::escape_html(&self.model),
            status_emoji,
            self.status.display_label()
        );

        let _ = write!(
            msg,
            "<b>📝 执行结果:</b>\n{}\n\n",
            text::escape_html(&self.result)
        );

        if !self.steps.is_empty() {
            msg.push_str("<b>🔍 执行步骤与时间线:</b>\n");
            for step in &self.steps {
                let _ = write!(
                    msg,
                    "• <b>{}</b>\n<blockquote expandable>{}</blockquote>\n",
                    text::escape_html(&step.title),
                    text::escape_html(&step.details)
                );
            }
        }

        msg.push_str(
            "\n<i>(💡 提示: 当前环境不支持 Telegram In-Message HTML Widget，已无缝降级为标准 HTML 消息)</i>",
        );
        msg
    }
}

// ============================================================================
// Multi-Template Dynamic Card System (CustomCardSpec for ```tg-card)
// ============================================================================

/// Semantic template style for [`CustomCardSpec`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardKind {
    #[default]
    Card,
    #[serde(alias = "dashboard", alias = "metrics")]
    Metric,
    #[serde(alias = "flashcard", alias = "trivia")]
    Quiz,
    #[serde(alias = "comparison", alias = "grid")]
    Table,
    #[serde(alias = "timeline", alias = "progress")]
    Status,
    #[serde(other)]
    Other,
}

impl CardKind {
    const fn icon(self) -> &'static str {
        match self {
            Self::Metric => "📊",
            Self::Quiz => "🧠",
            Self::Table => "📋",
            Self::Status => "⏱️",
            Self::Card | Self::Other => "🎴",
        }
    }
}

/// A single KPI metric tile inside a [`CustomCardSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardMetric {
    pub label: String,
    pub value: String,
    #[serde(default, alias = "sub", alias = "change")]
    pub delta: Option<String>,
    #[serde(default)]
    pub tone: Option<String>,
}

/// An interactive section/tab inside a [`CustomCardSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSection {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub items: Vec<String>,
}

/// Compact table rendered inside a [`CustomCardSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardTable {
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default)]
    pub rows: Vec<Vec<String>>,
}

/// Interactive multiple-choice quiz item rendered natively in QuickJS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuizQuestion {
    pub question: String,
    pub options: Vec<String>,
    #[serde(default, alias = "answer", alias = "correct")]
    pub answer_index: usize,
    #[serde(default)]
    pub explanation: Option<String>,
}

/// Flexible JSON-driven card schema used by AI replies (` ```tg-card `) and `/card` demos.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomCardSpec {
    #[serde(default)]
    pub kind: CardKind,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub badge: Option<String>,
    #[serde(default)]
    pub badge_tone: Option<String>,
    #[serde(default, alias = "body", alias = "description")]
    pub summary: Option<String>,
    #[serde(default)]
    pub metrics: Vec<CardMetric>,
    #[serde(default, alias = "tabs")]
    pub sections: Vec<CardSection>,
    #[serde(default)]
    pub table: Option<CardTable>,
    #[serde(default, alias = "timeline")]
    pub steps: Vec<AgentCardStep>,
    #[serde(default)]
    pub quiz: Vec<QuizQuestion>,
    #[serde(default)]
    pub footer: Option<String>,
}

impl CustomCardSpec {
    /// Parses a JSON string (optionally stripping leading/trailing whitespace) into [`CustomCardSpec`].
    pub fn from_json(raw: &str) -> Result<Self, WidgetError> {
        let spec: Self = serde_json::from_str(raw.trim())
            .map_err(|e| WidgetError::InvalidCardSpec(e.to_string()))?;
        if spec.title.trim().is_empty() {
            return Err(WidgetError::InvalidCardSpec(
                "card title cannot be empty".to_string(),
            ));
        }
        Ok(spec)
    }

    /// Demo KPI / Metric Dashboard card.
    #[must_use]
    pub fn demo_dashboard() -> Self {
        Self {
            kind: CardKind::Metric,
            title: format!("agygram v{} 运行看板", env!("CARGO_PKG_VERSION")),
            subtitle: Some("Master VPS · InstaBox Hybrid Swarm".to_string()),
            badge: Some("Healthy".to_string()),
            badge_tone: Some("ok".to_string()),
            summary: Some(
                "极简 Rust 守护进程稳定运行中，已接入 Telegram QuickJS 卡片与离线小游戏引擎。"
                    .to_string(),
            ),
            metrics: vec![
                CardMetric {
                    label: "内存占用".to_string(),
                    value: "~10 MB".to_string(),
                    delta: Some("892 MB VPS 安全".to_string()),
                    tone: Some("ok".to_string()),
                },
                CardMetric {
                    label: "主力模型".to_string(),
                    value: "Gemini 4".to_string(),
                    delta: Some("Hydrogen XHigh".to_string()),
                    tone: Some("info".to_string()),
                },
                CardMetric {
                    label: "Widget 压缩上限".to_string(),
                    value: "4,096 B".to_string(),
                    delta: Some("zlib-9 严格校验".to_string()),
                    tone: Some("ok".to_string()),
                },
                CardMetric {
                    label: "内置小游戏".to_string(),
                    value: "4 款".to_string(),
                    delta: Some("纯离线可玩".to_string()),
                    tone: Some("ok".to_string()),
                },
            ],
            sections: vec![
                CardSection {
                    title: "核心通道".to_string(),
                    body: None,
                    items: vec![
                        "Telegram Bot + Inline Control Panel".to_string(),
                        "QQ Official Bot OpenAPI v2 + Base64 上传".to_string(),
                    ],
                },
                CardSection {
                    title: "降级策略".to_string(),
                    body: Some(
                        "私聊 <=4KB 原生 sendWidget；群聊或超限自动发送可玩 .html 或富文本卡片。"
                            .to_string(),
                    ),
                    items: Vec::new(),
                },
            ],
            table: None,
            steps: Vec::new(),
            quiz: Vec::new(),
            footer: Some("agygram · QuickJS Dashboard".to_string()),
        }
    }

    /// Demo interactive QuickJS Quiz / Flashcard card.
    #[must_use]
    pub fn demo_quiz() -> Self {
        Self {
            kind: CardKind::Quiz,
            title: "Telegram QuickJS Widget 速测".to_string(),
            subtitle: Some("点击选项即时判分 + 触觉反馈".to_string()),
            badge: Some("2 题".to_string()),
            badge_tone: Some("info".to_string()),
            summary: None,
            metrics: Vec::new(),
            sections: Vec::new(),
            table: None,
            steps: Vec::new(),
            quiz: vec![
                QuizQuestion {
                    question: "Telegram Bot API 10.4 对 Widget 压缩后大小上限是多少？".to_string(),
                    options: vec![
                        "4 KB (4,096 字节)".to_string(),
                        "32 KB (32,768 字节)".to_string(),
                        "1 MB".to_string(),
                    ],
                    answer_index: 0,
                    explanation: Some(
                        "原始 HTML 上限 32 KB，服务端 zlib-9 压缩后必须 <= 4 KB。".to_string(),
                    ),
                },
                QuizQuestion {
                    question: "Widget 运行在客户端哪个无网沙盒引擎中？".to_string(),
                    options: vec![
                        "Full Chromium + Node.js".to_string(),
                        "QuickJS (Widget Profile 1)".to_string(),
                        "WebAssembly WASI".to_string(),
                    ],
                    answer_index: 1,
                    explanation: Some(
                        "Widget 运行在离线 QuickJS 沙盒，禁用 fetch/XHR 与 <input> 表单。"
                            .to_string(),
                    ),
                },
            ],
            footer: Some("点击选项作答 · 无需联网".to_string()),
        }
    }

    /// Demo comparison / table card.
    #[must_use]
    pub fn demo_comparison() -> Self {
        Self {
            kind: CardKind::Table,
            title: "交付通道对比矩阵".to_string(),
            subtitle: Some("Inline Widget vs Standalone .html vs HTML Text".to_string()),
            badge: Some("Matrix".to_string()),
            badge_tone: Some("info".to_string()),
            summary: Some("agygram 根据聊天类型与压缩体积自动选择最优呈现形态。".to_string()),
            metrics: Vec::new(),
            sections: Vec::new(),
            table: Some(CardTable {
                headers: vec![
                    "形态".to_string(),
                    "触发条件".to_string(),
                    "互动性".to_string(),
                ],
                rows: vec![
                    vec![
                        "QuickJS Widget".to_string(),
                        "私聊 & <=4KB".to_string(),
                        "气泡内即点".to_string(),
                    ],
                    vec![
                        ".html 附件".to_string(),
                        "游戏降级/超4KB".to_string(),
                        "内置浏览器".to_string(),
                    ],
                    vec![
                        "HTML 摘要".to_string(),
                        "群聊信息卡".to_string(),
                        "折叠/链接".to_string(),
                    ],
                ],
            }),
            steps: Vec::new(),
            quiz: Vec::new(),
            footer: Some("agygram · Comparison Card".to_string()),
        }
    }

    /// Renders this [`CustomCardSpec`] into a compact HTML+QuickJS widget (`<= 4 KB` compressed for typical cards).
    pub fn render_html(&self) -> Result<String, WidgetError> {
        let mut html = String::with_capacity(4096);
        html.push_str(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width,initial-scale=1,user-scalable=no\">\
             <style>",
        );
        html.push_str(CUSTOM_CARD_BASE_CSS);
        if !self.quiz.is_empty() {
            html.push_str(CUSTOM_CARD_QUIZ_CSS);
        }
        if self.table.is_some() {
            html.push_str(CUSTOM_CARD_TABLE_CSS);
        }
        html.push_str("</style></head><body><div class=\"card\">");

        self.render_html_header(&mut html);
        self.render_html_metrics_and_summary(&mut html);
        if let Some(ref tbl) = self.table {
            render_html_table(tbl, &mut html);
        }
        self.render_html_sections(&mut html);
        self.render_html_steps(&mut html);
        self.render_html_quiz(&mut html);
        self.render_html_footer(&mut html);

        html.push_str("</div><script>");
        html.push_str(CUSTOM_CARD_BASE_JS);
        if !self.quiz.is_empty() {
            html.push_str(CUSTOM_CARD_QUIZ_JS);
        }
        html.push_str("</script></body></html>");

        validate_widget_raw(&html)?;
        Ok(html)
    }

    fn render_html_header(&self, html: &mut String) {
        let badge_tone = normalize_tone(self.badge_tone.as_deref());
        let _ = write!(
            html,
            "<div class=\"hdr\"><div><div class=\"ttl\">{}</div>",
            escape_html(&self.title)
        );
        if let Some(ref sub) = self.subtitle {
            let _ = write!(html, "<div class=\"sub\">{}</div>", escape_html(sub));
        }
        html.push_str("</div>");
        if let Some(ref badge) = self.badge {
            let _ = write!(
                html,
                "<span id=\"cbg\" class=\"bg {badge_tone}\">{}</span>",
                escape_html(badge)
            );
        }
        html.push_str("</div>");
    }

    fn render_html_metrics_and_summary(&self, html: &mut String) {
        if self.metrics.is_empty() && self.summary.is_none() {
            return;
        }
        html.push_str("<div class=\"sec\">");
        if !self.metrics.is_empty() {
            html.push_str("<div class=\"kpis\">");
            for m in &self.metrics {
                let tone = normalize_tone(m.tone.as_deref());
                let _ = write!(
                    html,
                    "<div class=\"kpi\"><div class=\"kl\">{}</div><div class=\"kv\">{}</div>",
                    escape_html(&m.label),
                    escape_html(&m.value)
                );
                if let Some(ref delta) = m.delta {
                    let _ = write!(
                        html,
                        "<div class=\"kd {tone}\">{}</div>",
                        escape_html(delta)
                    );
                }
                html.push_str("</div>");
            }
            html.push_str("</div>");
        }
        if let Some(ref summary) = self.summary {
            let _ = write!(
                html,
                "<div id=\"csum\" class=\"sum\">{}</div>",
                escape_html(summary)
            );
        }
        html.push_str("</div>");
    }

    fn render_html_sections(&self, html: &mut String) {
        if self.sections.len() >= 2 {
            html.push_str("<div class=\"tabs\">");
            for (idx, sec) in self.sections.iter().enumerate() {
                let on = if idx == 0 { " on" } else { "" };
                let _ = write!(
                    html,
                    "<button class=\"tb{on}\" data-t=\"{idx}\" onclick=\"st('{idx}')\">{}</button>",
                    escape_html(&sec.title)
                );
            }
            html.push_str("</div><div class=\"sec\">");
            for (idx, sec) in self.sections.iter().enumerate() {
                let on = if idx == 0 { " on" } else { "" };
                let _ = write!(html, "<div id=\"tp-{idx}\" class=\"tp{on}\">");
                render_section_body_into(sec, html);
                html.push_str("</div>");
            }
            html.push_str("</div>");
        } else if let Some(sec) = self.sections.first() {
            let _ = write!(
                html,
                "<div class=\"sec\"><div class=\"sttl\">{}</div>",
                escape_html(&sec.title)
            );
            render_section_body_into(sec, html);
            html.push_str("</div>");
        }
    }

    fn render_html_steps(&self, html: &mut String) {
        if self.steps.is_empty() {
            return;
        }
        html.push_str("<div class=\"sec\"><div class=\"acc\">");
        for step in &self.steps {
            let on = if step.is_expanded { " on" } else { "" };
            let _ = write!(
                html,
                "<div class=\"ai{on}\"><div class=\"ah\" onclick=\"ta(this)\">\
                 <span>{}</span><span>▶</span></div><div class=\"ab\">{}</div></div>",
                escape_html(&step.title),
                escape_html(&step.details)
            );
        }
        html.push_str("</div></div>");
    }

    fn render_html_quiz(&self, html: &mut String) {
        if self.quiz.is_empty() {
            return;
        }
        html.push_str("<div class=\"sec\"><div class=\"qz\">");
        for (qi, q) in self.quiz.iter().enumerate() {
            let _ = write!(
                html,
                "<div id=\"qq-{qi}\" class=\"qq\"><div class=\"qt\">{}. {}</div><div class=\"qo\">",
                qi + 1,
                escape_html(&q.question)
            );
            let ans = q.answer_index.min(q.options.len().saturating_sub(1));
            for (oi, opt) in q.options.iter().enumerate() {
                let _ = write!(
                    html,
                    "<button class=\"qb\" onclick=\"qa(this,{qi},{oi},{ans})\">{}</button>",
                    escape_html(opt)
                );
            }
            html.push_str("</div>");
            if let Some(ref exp) = q.explanation {
                let _ = write!(html, "<div class=\"qe\">💡 {}</div>", escape_html(exp));
            }
            html.push_str("</div>");
        }
        html.push_str("</div></div>");
    }

    fn render_html_footer(&self, html: &mut String) {
        let footer_left = self
            .footer
            .as_deref()
            .unwrap_or("⚡ Telegram QuickJS Widget");
        let footer_right = if self.quiz.is_empty() {
            "<span id=\"qsc\">agygram</span>".to_string()
        } else {
            format!(
                "<span id=\"qsc\" data-n=\"0\" data-ok=\"0\">0/{}</span>",
                self.quiz.len()
            )
        };
        let _ = write!(
            html,
            "<div class=\"ftr\"><span>{}</span>{footer_right}</div>",
            escape_html(footer_left)
        );
    }

    /// Renders a clean, readable Telegram HTML fallback message for group chats or non-widget clients.
    #[must_use]
    pub fn render_fallback_html(&self) -> String {
        let mut msg = String::with_capacity(1024);
        let icon = self.kind.icon();
        let _ = write!(msg, "{icon} <b>{}</b>", text::escape_html(&self.title));
        if let Some(ref badge) = self.badge {
            let _ = write!(msg, " [<code>{}</code>]", text::escape_html(badge));
        }
        msg.push('\n');
        if let Some(ref sub) = self.subtitle {
            let _ = writeln!(msg, "<i>{}</i>", text::escape_html(sub));
        }
        if let Some(ref sum) = self.summary {
            let _ = writeln!(msg, "\n{}", text::escape_html(sum));
        }
        self.render_fallback_metrics(&mut msg);
        if let Some(ref tbl) = self.table {
            render_fallback_table(tbl, &mut msg);
        }
        self.render_fallback_sections_and_steps(&mut msg);
        self.render_fallback_quiz(&mut msg);
        if let Some(ref ftr) = self.footer {
            let _ = write!(msg, "\n<i>{}</i>", text::escape_html(ftr));
        }
        msg
    }

    fn render_fallback_metrics(&self, msg: &mut String) {
        if self.metrics.is_empty() {
            return;
        }
        msg.push('\n');
        for m in &self.metrics {
            let _ = write!(
                msg,
                "• <b>{}:</b> <code>{}</code>",
                text::escape_html(&m.label),
                text::escape_html(&m.value)
            );
            if let Some(ref d) = m.delta {
                let _ = write!(msg, " (<i>{}</i>)", text::escape_html(d));
            }
            msg.push('\n');
        }
    }

    fn render_fallback_sections_and_steps(&self, msg: &mut String) {
        for sec in &self.sections {
            let _ = writeln!(msg, "\n<b>📌 {}</b>", text::escape_html(&sec.title));
            if let Some(ref body) = sec.body {
                let _ = writeln!(msg, "{}", text::escape_html(body));
            }
            for item in &sec.items {
                let _ = writeln!(msg, "• {}", text::escape_html(item));
            }
        }
        if !self.steps.is_empty() {
            msg.push_str("\n<b>🔍 详情步骤:</b>\n");
            for step in &self.steps {
                let _ = write!(
                    msg,
                    "• <b>{}</b>\n<blockquote expandable>{}</blockquote>\n",
                    text::escape_html(&step.title),
                    text::escape_html(&step.details)
                );
            }
        }
    }

    fn render_fallback_quiz(&self, msg: &mut String) {
        if self.quiz.is_empty() {
            return;
        }
        msg.push_str("\n<b>🧠 互动问答:</b>\n");
        for (qi, q) in self.quiz.iter().enumerate() {
            let _ = writeln!(msg, "{}. <b>{}</b>", qi + 1, text::escape_html(&q.question));
            for (oi, opt) in q.options.iter().enumerate() {
                let letter = (b'A' + (oi as u8).min(25)) as char;
                let _ = writeln!(msg, "   {letter}) {}", text::escape_html(opt));
            }
            let ans_text = q.options.get(q.answer_index).map_or("?", String::as_str);
            let exp_text = q.explanation.as_deref().unwrap_or("");
            let _ = writeln!(
                msg,
                "   <tg-spoiler>答案: {} {}</tg-spoiler>",
                text::escape_html(ans_text),
                text::escape_html(exp_text)
            );
        }
    }
}

fn render_fallback_table(tbl: &CardTable, msg: &mut String) {
    msg.push('\n');
    for (r_idx, row) in tbl.rows.iter().enumerate() {
        if let Some(first) = row.first() {
            let _ = writeln!(msg, "<b>{}. {}</b>", r_idx + 1, text::escape_html(first));
        }
        for (c_idx, val) in row.iter().enumerate().skip(1) {
            let hdr = tbl.headers.get(c_idx).map_or("Col", String::as_str);
            let _ = writeln!(
                msg,
                "  • {}: {}",
                text::escape_html(hdr),
                text::escape_html(val)
            );
        }
    }
}

fn render_html_table(tbl: &CardTable, html: &mut String) {
    html.push_str("<div class=\"sec\"><table>");
    if !tbl.headers.is_empty() {
        html.push_str("<thead><tr>");
        for h in &tbl.headers {
            let _ = write!(html, "<th>{}</th>", escape_html(h));
        }
        html.push_str("</tr></thead>");
    }
    if !tbl.rows.is_empty() {
        html.push_str("<tbody>");
        for row in &tbl.rows {
            html.push_str("<tr>");
            for cell in row {
                let _ = write!(html, "<td>{}</td>", escape_html(cell));
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody>");
    }
    html.push_str("</table></div>");
}

fn render_section_body_into(sec: &CardSection, html: &mut String) {
    if let Some(ref body) = sec.body {
        let _ = write!(html, "<div class=\"sum\">{}</div>", escape_html(body));
    }
    if !sec.items.is_empty() {
        html.push_str("<ul class=\"lst\">");
        for item in &sec.items {
            let _ = write!(html, "<li>{}</li>", escape_html(item));
        }
        html.push_str("</ul>");
    }
}

fn normalize_tone(tone: Option<&str>) -> &'static str {
    match tone.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("ok" | "success" | "green") => "ok",
        Some("warn" | "warning" | "yellow" | "orange") => "warn",
        Some("err" | "error" | "danger" | "red") => "err",
        _ => "info",
    }
}

// ============================================================================
// Built-in Playable Offline Mini-Games (< 4 KB zlib-9 compressed each!)
// ============================================================================

/// Built-in zero-dependency QuickJS offline mini-games ready for `/card` and `/card <game>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinGame {
    Game2048,
    Minesweeper,
    MemoryMatch,
    ReactionTap,
}

const ALL_BUILTIN_GAMES: [BuiltinGame; 4] = [
    BuiltinGame::Game2048,
    BuiltinGame::Minesweeper,
    BuiltinGame::MemoryMatch,
    BuiltinGame::ReactionTap,
];

impl BuiltinGame {
    /// Returns all built-in mini-games in menu display order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &ALL_BUILTIN_GAMES
    }

    /// Short deterministic identifier used in commands and `<= 64`-byte callbacks.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Game2048 => "2048",
            Self::Minesweeper => "minesweeper",
            Self::MemoryMatch => "memory",
            Self::ReactionTap => "reaction",
        }
    }

    /// Human-readable title shown in messages and HTML captions.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Game2048 => "2048 数字合成",
            Self::Minesweeper => "经典扫雷 8×8",
            Self::MemoryMatch => "记忆翻牌配对",
            Self::ReactionTap => "极速反应测试",
        }
    }

    /// Compact InlineKeyboard button label.
    #[must_use]
    pub const fn button_label(self) -> &'static str {
        match self {
            Self::Game2048 => "🔢 2048",
            Self::Minesweeper => "💣 扫雷",
            Self::MemoryMatch => "🃏 记忆翻牌",
            Self::ReactionTap => "⚡ 极速反应",
        }
    }

    /// Short description of the game mechanics.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Game2048 => "滑动或方向键合并方块冲刺 2048",
            Self::Minesweeper => "8×8 经典扫雷，支持挖雷/插旗切换",
            Self::MemoryMatch => "16 宫格 Emoji 翻牌配对挑战最少步数",
            Self::ReactionTap => "毫秒级神经反应测速，记录历史最快成绩",
        }
    }

    /// Filename used when delivering as a standalone playable `.html` file.
    #[must_use]
    pub fn filename(self) -> String {
        format!("agygram_{}.html", self.id())
    }

    /// Parses user command arguments or aliases (Chinese/English) into a built-in game.
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_ascii_lowercase().as_str() {
            "2048" | "game2048" => Some(Self::Game2048),
            "minesweeper" | "mine" | "mines" | "扫雷" => Some(Self::Minesweeper),
            "memory" | "match" | "mem" | "记忆" | "翻牌" | "记忆翻牌" => {
                Some(Self::MemoryMatch)
            }
            "reaction" | "reflex" | "tap" | "反应" | "手速" | "极速反应" => {
                Some(Self::ReactionTap)
            }
            _ => None,
        }
    }

    /// Returns the validated standalone HTML source (strictly `<= 4 KB` zlib-9 compressed).
    pub fn render_html(self) -> Result<String, WidgetError> {
        let raw = match self {
            Self::Game2048 => GAME_2048_HTML,
            Self::Minesweeper => GAME_MINESWEEPER_HTML,
            Self::MemoryMatch => GAME_MEMORY_HTML,
            Self::ReactionTap => GAME_REACTION_HTML,
        };
        let html = raw.to_string();
        validate_widget_html(&html)?;
        Ok(html)
    }

    /// Builds an [`OutboundWidget`] ready for [`deliver_widget_with_fallback`].
    pub fn to_outbound_widget(self) -> Result<OutboundWidget, WidgetError> {
        let html = self.render_html()?;
        let inspection = inspect_widget_html(&html);
        Ok(OutboundWidget {
            kind: WidgetKind::InteractiveApp,
            title: self.title().to_string(),
            file_name: self.filename(),
            html,
            fallback_html: format!(
                "🎮 <b>{}</b>\n{}\n<i>(请在 Telegram 内置浏览器中打开附件 .html 文件畅玩)</i>",
                text::escape_html(self.title()),
                text::escape_html(self.description())
            ),
            caption: Some(truncate_caption(&format!(
                "🎮 {} — {}",
                self.title(),
                self.description()
            ))),
            raw_bytes: inspection.raw_bytes,
            compressed_bytes: inspection.compressed_bytes,
            warnings: inspection.warnings,
            width: Some(0),
            height: Some(0),
        })
    }
}

// ============================================================================
// AI Response Extraction & Prompt Builders
// ============================================================================

/// Checks whether `haystack` contains ASCII `needle` on non-identifier/non-path boundaries
/// (so `"src/bot/widget.rs"` or `"discard"` will not false-trigger, while `"做一个widget"` or
/// `"build a quiz"` will).
fn contains_ascii_word_ci(lower_haystack: &str, needle: &str) -> bool {
    let bytes = lower_haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = lower_haystack[start..].find(needle) {
        let pos = start + rel;
        let end = pos + needle.len();
        let prev_ok = pos == 0
            || (!bytes[pos - 1].is_ascii_alphanumeric()
                && !matches!(bytes[pos - 1], b'_' | b'.' | b'/' | b'\\'));
        let next_ok = end >= bytes.len()
            || (!bytes[end].is_ascii_alphanumeric()
                && !matches!(bytes[end], b'_' | b'.' | b'/' | b'\\'));
        if prev_ok && next_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

/// Checks whether `lower_haystack` contains a standalone slash command such as `/card` or `/widget`
/// (not inside a filesystem path like `src/bot/widget.rs`).
fn contains_slash_command_ci(lower_haystack: &str, cmd: &str) -> bool {
    let bytes = lower_haystack.as_bytes();
    let mut start = 0usize;
    while let Some(rel) = lower_haystack[start..].find(cmd) {
        let pos = start + rel;
        let end = pos + cmd.len();
        let prev_ok = pos == 0 || bytes[pos - 1].is_ascii_whitespace();
        let next_ok = end >= bytes.len() || bytes[end].is_ascii_whitespace() || bytes[end] == b'@';
        if prev_ok && next_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

/// Returns `true` only when a user prompt explicitly requests an interactive card, dashboard,
/// quiz, or offline mini-game/widget, allowing normal turns to skip injecting the 1.4 KB
/// [`WIDGET_PROMPT_GUIDE`] system prompt boilerplate.
#[must_use]
pub fn prompt_requests_widget_or_card(prompt: &str) -> bool {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return false;
    }

    const CJK_TRIGGERS: &[&str] = &[
        "交互卡片",
        "互动卡片",
        "交互组件",
        "互动组件",
        "交互小组件",
        "小组件",
        "卡片",
        "仪表盘",
        "看板",
        "小游戏",
        "互动游戏",
        "离线游戏",
        "扫雷",
        "记忆翻牌",
        "极速反应",
        "五子棋",
        "贪吃蛇",
        "俄罗斯方块",
        "井字棋",
        "数独",
        "计算器",
        "速测",
        "测验",
        "问答卡",
        "对比卡",
        "进度卡",
    ];
    if CJK_TRIGGERS.iter().any(|kw| trimmed.contains(kw)) {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    if contains_slash_command_ci(&lower, "/card") || contains_slash_command_ci(&lower, "/widget") {
        return true;
    }
    const EXPLICIT_TOKENS: &[&str] = &[
        "tg-widget",
        "tg-card",
        "telegram-widget",
        "telegram-card",
        "quickjs",
    ];
    if EXPLICIT_TOKENS.iter().any(|kw| lower.contains(kw)) {
        return true;
    }

    const ASCII_WORDS: &[&str] = &[
        "widget",
        "dashboard",
        "quiz",
        "flashcard",
        "minigame",
        "mini-game",
        "mini game",
        "minesweeper",
        "2048",
        "interactive card",
        "status card",
        "metric card",
        "comparison card",
        "summary card",
    ];
    ASCII_WORDS
        .iter()
        .any(|kw| contains_ascii_word_ci(&lower, kw))
}

/// Builds an [`OutboundWidget`] directly from `agy --json-schema` `structured_output`.
pub fn widget_from_structured_card_value(
    val: &serde_json::Value,
) -> Result<OutboundWidget, WidgetError> {
    let spec = match val {
        serde_json::Value::String(raw) => CustomCardSpec::from_json(raw)?,
        other => {
            let parsed: CustomCardSpec = serde_json::from_value(other.clone())
                .map_err(|e| WidgetError::InvalidCardSpec(e.to_string()))?;
            if parsed.title.trim().is_empty() {
                return Err(WidgetError::InvalidCardSpec(
                    "card title cannot be empty".to_string(),
                ));
            }
            parsed
        }
    };
    OutboundWidget::from_custom_card(&spec)
}

/// Merges a session's user `system_prompt` with [`WIDGET_PROMPT_GUIDE`] for Telegram turns.
#[must_use]
#[allow(dead_code)]
pub fn build_effective_system_prompt(user_system_prompt: &str) -> String {
    let trimmed = user_system_prompt.trim();
    if trimmed.is_empty() {
        WIDGET_PROMPT_GUIDE.to_string()
    } else {
        format!("{trimmed}\n\n{WIDGET_PROMPT_GUIDE}")
    }
}

/// Wraps a `/card <prompt>` user request into an actionable prompt for `agy` (paired with
/// `--json-schema TG_CARD_JSON_SCHEMA` so `result.structured_output` renders directly into a card).
#[must_use]
pub fn build_card_generation_prompt(user_request: &str) -> String {
    format!(
        "请直接为 Telegram 用户制作一张结构化交互卡片来满足以下需求：\n{}\n\n\
         输出要求：\n\
         - 严格遵循提供的 JSON Schema（CustomCardSpec），填写清晰的 title 与适合场景的 kind（card / metric / quiz / table / status）。\n\
         - 合理组织 metrics、sections、table、steps 或 quiz 内容，语言简洁直观，便于手机端阅读与点按互动。",
        user_request.trim()
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FenceKind {
    Normal,
    Widget,
    Card,
}

struct ActiveFence {
    kind: FenceKind,
    fence_char: u8,
    fence_len: usize,
    title_hint: String,
    body: String,
}

/// Extracts all ```tg-widget and ```tg-card fenced blocks from an AI markdown reply,
/// returning `(cleaned_markdown, compiled_widgets)`.
#[must_use]
pub fn extract_ai_widgets(markdown: &str) -> (String, Vec<OutboundWidget>) {
    if !text::contains_ascii_case_insensitive_str(markdown, "tg-widget")
        && !text::contains_ascii_case_insensitive_str(markdown, "tg-card")
        && !text::contains_ascii_case_insensitive_str(markdown, "telegram-widget")
        && !text::contains_ascii_case_insensitive_str(markdown, "telegram-card")
    {
        return (markdown.to_string(), Vec::new());
    }

    let mut cleaned = String::with_capacity(markdown.len());
    let mut widgets = Vec::new();
    let mut active: Option<ActiveFence> = None;
    let mut raw_block_buffer = String::new();
    let mut widget_seq = 0usize;

    for segment in markdown.split_inclusive('\n') {
        let line_no_eol = segment.trim_end_matches(['\r', '\n']);

        if let Some(ref mut fence) = active {
            if is_closing_fence(line_no_eol, fence.fence_char, fence.fence_len) {
                if let Some(finished) = active.take() {
                    handle_closed_fence(
                        finished,
                        &raw_block_buffer,
                        segment,
                        &mut cleaned,
                        &mut widgets,
                        &mut widget_seq,
                    );
                    raw_block_buffer.clear();
                }
            } else {
                raw_block_buffer.push_str(segment);
                if fence.kind != FenceKind::Normal {
                    fence.body.push_str(segment);
                }
            }
            continue;
        }

        if let Some((fence_char, fence_len, info)) = parse_opening_fence(line_no_eol) {
            let (kind, title_hint) = classify_fence_info(info);
            raw_block_buffer.clear();
            raw_block_buffer.push_str(segment);
            active = Some(ActiveFence {
                kind,
                fence_char,
                fence_len,
                title_hint,
                body: String::new(),
            });
            continue;
        }

        cleaned.push_str(segment);
    }

    // Unclosed trailing fence: preserve text rather than dropping partial output.
    if active.is_some() {
        cleaned.push_str(&raw_block_buffer);
    }

    (collapse_excess_blank_lines(&cleaned), widgets)
}

fn handle_closed_fence(
    finished: ActiveFence,
    raw_block_buffer: &str,
    closing_segment: &str,
    cleaned: &mut String,
    widgets: &mut Vec<OutboundWidget>,
    widget_seq: &mut usize,
) {
    match finished.kind {
        FenceKind::Normal => {
            cleaned.push_str(raw_block_buffer);
            cleaned.push_str(closing_segment);
        }
        FenceKind::Widget => {
            *widget_seq += 1;
            let title = if finished.title_hint.is_empty() {
                extract_html_title(&finished.body)
                    .unwrap_or_else(|| format!("Interactive Widget #{widget_seq}"))
            } else {
                finished.title_hint
            };
            let file_name = suggest_html_filename(&title);
            match OutboundWidget::from_raw_html(
                title,
                file_name,
                &finished.body,
                WidgetKind::InteractiveApp,
            ) {
                Ok(w) => widgets.push(w),
                Err(err) => {
                    warn!(error = %err, "failed to compile tg-widget block; keeping raw markdown");
                    cleaned.push_str(raw_block_buffer);
                    cleaned.push_str(closing_segment);
                }
            }
        }
        FenceKind::Card => {
            match CustomCardSpec::from_json(&finished.body)
                .and_then(|spec| OutboundWidget::from_custom_card(&spec))
            {
                Ok(w) => widgets.push(w),
                Err(err) => {
                    warn!(error = %err, "failed to compile tg-card JSON; keeping raw markdown");
                    cleaned.push_str(raw_block_buffer);
                    cleaned.push_str(closing_segment);
                }
            }
        }
    }
}

/// Extracts inline `tg-widget` / `tg-card` fenced blocks, and when `allow_prose_widget_files` is
/// `true` also safely loads referenced `.widget.html` files confined within `allowed_dirs`.
pub async fn extract_ai_widgets_with_roots(
    markdown: &str,
    allowed_dirs: &[PathBuf],
    allow_prose_widget_files: bool,
) -> (String, Vec<OutboundWidget>) {
    let (cleaned, mut widgets) = extract_ai_widgets(markdown);
    if !allow_prose_widget_files
        || allowed_dirs.is_empty()
        || !text::contains_ascii_case_insensitive_str(markdown, ".widget.html")
    {
        return (cleaned, widgets);
    }

    let mut canonical_roots = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = tokio::fs::canonicalize(dir).await
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    if canonical_roots.is_empty() {
        return (cleaned, widgets);
    }

    let mut seen_paths: Vec<PathBuf> = Vec::new();
    for token in markdown.split_whitespace() {
        let Some(candidate_str) = extract_widget_file_candidate(token) else {
            continue;
        };
        let resolved = match resolve_confined_widget_file(candidate_str, &canonical_roots).await {
            Some(p) if !seen_paths.contains(&p) => p,
            _ => continue,
        };
        seen_paths.push(resolved.clone());

        let file_name = resolved
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if text::looks_sensitive(&file_name) || text::looks_sensitive(&resolved.to_string_lossy()) {
            warn!(path = %resolved.display(), "refusing sensitive .widget.html file");
            continue;
        }

        let Ok(meta) = tokio::fs::metadata(&resolved).await else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_STANDALONE_WIDGET_FILE_BYTES as u64 {
            continue;
        }

        let Ok(html_content) = tokio::fs::read_to_string(&resolved).await else {
            continue;
        };
        let title = extract_html_title(&html_content)
            .unwrap_or_else(|| file_name.trim_end_matches(".widget.html").to_string());
        if let Ok(w) = OutboundWidget::from_raw_html(
            title,
            ensure_html_extension(&file_name),
            &html_content,
            WidgetKind::InteractiveApp,
        ) {
            widgets.push(w);
        }
    }

    (cleaned, widgets)
}

fn parse_opening_fence(line: &str) -> Option<(u8, usize, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    let ch = *bytes.get(i)?;
    if ch != b'`' && ch != b'~' {
        return None;
    }
    let start = i;
    while i < bytes.len() && bytes[i] == ch {
        i += 1;
    }
    let len = i - start;
    if len < 3 {
        return None;
    }
    let info = line[i..].trim();
    // Backtick fences cannot contain backticks in the info string (CommonMark rule).
    if ch == b'`' && info.contains('`') {
        return None;
    }
    Some((ch, len, info))
}

fn is_closing_fence(line: &str, fence_char: u8, min_len: usize) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && i < 3 && bytes[i] == b' ' {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i] == fence_char {
        i += 1;
    }
    i - start >= min_len && line[i..].trim().is_empty()
}

fn classify_fence_info(info: &str) -> (FenceKind, String) {
    let mut parts = info.splitn(2, char::is_whitespace);
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.next().unwrap_or("").trim();
    let clean_hint = rest
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string();

    match lang.as_str() {
        "tg-widget" | "telegram-widget" => (FenceKind::Widget, clean_hint),
        "tg-card" | "telegram-card" => (FenceKind::Card, clean_hint),
        _ => (FenceKind::Normal, String::new()),
    }
}

fn collapse_excess_blank_lines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut empty_run = 0usize;
    for line in input.lines() {
        if line.trim().is_empty() {
            empty_run += 1;
            if empty_run <= 2 {
                out.push('\n');
            }
        } else {
            empty_run = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

fn extract_widget_file_candidate(raw_token: &str) -> Option<&str> {
    let trimmed = raw_token.trim_matches(|c: char| {
        matches!(
            c,
            '(' | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '"'
                | '\''
                | '<'
                | '>'
                | '`'
                | '*'
                | ','
                | ';'
                | '，'
                | '。'
                | '、'
                | '；'
                | '：'
                | '（'
                | '）'
                | '【'
                | '】'
        )
    });
    let path_str = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    if path_str.contains("://") {
        return None;
    }
    path_str
        .to_ascii_lowercase()
        .ends_with(".widget.html")
        .then_some(path_str)
}

async fn resolve_confined_widget_file(
    candidate: &str,
    canonical_roots: &[PathBuf],
) -> Option<PathBuf> {
    let p = Path::new(candidate);
    if p.is_absolute() {
        let resolved = tokio::fs::canonicalize(p).await.ok()?;
        return canonical_roots
            .iter()
            .any(|root| resolved.starts_with(root))
            .then_some(resolved);
    }
    for root in canonical_roots {
        let joined = root.join(p);
        if let Ok(resolved) = tokio::fs::canonicalize(&joined).await
            && resolved.starts_with(root)
        {
            return Some(resolved);
        }
    }
    None
}

fn extract_html_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title>")? + "<title>".len();
    let end = lower[start..].find("</title>")?;
    let raw = html[start..start + end].trim();
    (!raw.is_empty()).then(|| raw.to_string())
}

fn suggest_html_filename(title: &str) -> String {
    let mut slug = String::with_capacity(32);
    let mut last_underscore = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore && !slug.is_empty() && slug.len() < 28 {
            slug.push('_');
            last_underscore = true;
        }
        if slug.len() >= 28 {
            break;
        }
    }
    let cleaned = slug.trim_matches('_');
    if cleaned.is_empty() {
        "agygram_widget.html".to_string()
    } else {
        format!("{cleaned}.html")
    }
}

fn ensure_html_extension(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("agygram_widget.html")
        .trim();
    if base.to_ascii_lowercase().ends_with(".html") || base.to_ascii_lowercase().ends_with(".htm") {
        base.to_string()
    } else {
        format!("{base}.html")
    }
}

fn truncate_caption(input: &str) -> String {
    text::truncate_chars(input, MAX_CAPTION_CHARS).into_owned()
}

// ============================================================================
// HTTP Client & Telegram Bot API 10.4 Transport + Automatic Delivery Fallback
// ============================================================================

fn build_widget_http_client(proxy_url: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true);

    if let Some(proxy) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                builder = builder.proxy(p);
            }
            Err(err) => {
                warn!("Invalid proxy URL for widget client {:?}: {err}", proxy);
            }
        }
    }

    builder.build().unwrap_or_else(|err| {
        warn!("Failed to build widget HTTP client: {err}; fallback to default");
        Client::new()
    })
}

/// Configures global HTTP proxy for Telegram widget calls.
pub fn configure_proxy(proxy_url: Option<&str>) {
    let normalized = proxy_url.map(str::trim).filter(|s| !s.is_empty());
    let state = WIDGET_HTTP_CLIENT_STATE
        .get_or_init(|| RwLock::new((None, build_widget_http_client(None))));

    let mut guard = match state.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    if guard.0.as_deref() != normalized {
        guard.0 = normalized.map(ToString::to_string);
        guard.1 = build_widget_http_client(normalized);
    }
}

/// Retrieves the cached reqwest client configured for widget requests.
#[must_use]
pub fn get_widget_client() -> Client {
    let state = WIDGET_HTTP_CLIENT_STATE
        .get_or_init(|| RwLock::new((None, build_widget_http_client(None))));
    match state.read() {
        Ok(g) => g.1.clone(),
        Err(poisoned) => poisoned.into_inner().1.clone(),
    }
}

/// Optional Bot API 10.4 parameters for `sendWidget`.
#[derive(Debug, Clone, Default)]
pub struct SendWidgetOptions<'a> {
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub caption: Option<&'a str>,
    pub parse_mode: Option<&'a str>,
    pub chat_data: Option<&'a str>,
    pub public_data: Option<&'a str>,
    pub protect_content: Option<bool>,
    pub allow_queries: Option<bool>,
    pub reply_parameters: Option<&'a serde_json::Value>,
    pub reply_markup: Option<&'a InlineKeyboardMarkup>,
}

#[derive(Serialize)]
struct SendWidgetPayload<'a> {
    chat_id: i64,
    html: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    caption: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_data: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_data: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protect_content: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    allow_queries: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_parameters: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_markup: Option<&'a InlineKeyboardMarkup>,
}

#[derive(Serialize)]
struct EditMessageWidgetDataPayload {
    chat_id: i64,
    message_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_data: Option<String>,
}

#[derive(Deserialize)]
struct RawTelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i32>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawMessageResult {
    message_id: i32,
    chat: Option<RawChatResult>,
    date: Option<i64>,
}

#[derive(Deserialize)]
struct RawChatResult {
    id: i64,
}

/// Sends an in-message HTML widget to a private Telegram chat using Bot API 10.4+.
#[allow(dead_code)]
pub async fn send_widget(
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
) -> Result<WidgetMessageResponse, WidgetError> {
    send_widget_with_options(
        bot_token,
        chat_id,
        html_content,
        SendWidgetOptions::default(),
    )
    .await
}

/// Sends an in-message HTML widget with optional dimensions, caption, state data, and reply markup.
pub async fn send_widget_with_options(
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
    options: SendWidgetOptions<'_>,
) -> Result<WidgetMessageResponse, WidgetError> {
    let client = get_widget_client();
    send_widget_with_client_and_options(
        &client,
        DEFAULT_TELEGRAM_API_BASE,
        bot_token,
        chat_id,
        html_content,
        options,
    )
    .await
}

/// Sends an in-message HTML widget with an explicit reqwest client and API base URL.
#[allow(dead_code)]
pub async fn send_widget_with_client(
    client: &Client,
    api_base: &str,
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
) -> Result<WidgetMessageResponse, WidgetError> {
    send_widget_with_client_and_options(
        client,
        api_base,
        bot_token,
        chat_id,
        html_content,
        SendWidgetOptions::default(),
    )
    .await
}

/// Sends an in-message HTML widget with explicit HTTP client, base URL, and Bot API options.
pub async fn send_widget_with_client_and_options(
    client: &Client,
    api_base: &str,
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
    options: SendWidgetOptions<'_>,
) -> Result<WidgetMessageResponse, WidgetError> {
    validate_widget_html(html_content)?;

    if chat_id < 0 {
        return Err(WidgetError::NonPrivateChat(chat_id));
    }

    if let Some(cd) = options.chat_data
        && cd.len() > MAX_WIDGET_DATA_BYTES
    {
        return Err(WidgetError::InvalidParameter(format!(
            "chat_data exceeds {MAX_WIDGET_DATA_BYTES} bytes ({})",
            cd.len()
        )));
    }
    if let Some(pd) = options.public_data
        && pd.len() > MAX_WIDGET_DATA_BYTES
    {
        return Err(WidgetError::InvalidParameter(format!(
            "public_data exceeds {MAX_WIDGET_DATA_BYTES} bytes ({})",
            pd.len()
        )));
    }

    let url = format!("{api_base}/bot{bot_token}/sendWidget");
    let payload = SendWidgetPayload {
        chat_id,
        html: html_content,
        width: options.width,
        height: options.height,
        caption: options.caption,
        parse_mode: options.parse_mode,
        chat_data: options.chat_data,
        public_data: options.public_data,
        protect_content: options.protect_content,
        allow_queries: options.allow_queries,
        reply_parameters: options.reply_parameters,
        reply_markup: options.reply_markup,
    };

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| map_http_error(e, bot_token))?;
    let status = response.status();
    let body_text = response
        .text()
        .await
        .map_err(|e| map_http_error(e, bot_token))?;

    let parsed: RawTelegramEnvelope<RawMessageResult> =
        serde_json::from_str(&body_text).map_err(|e| {
            debug!(body = %body_text, "Failed to parse Telegram API response JSON");
            WidgetError::Serialization(e)
        })?;

    if parsed.ok
        && let Some(msg) = parsed.result
    {
        return Ok(WidgetMessageResponse {
            ok: true,
            message_id: msg.message_id,
            chat_id: msg.chat.map(|c| c.id),
            date: msg.date,
        });
    }

    Err(WidgetError::ApiError {
        code: parsed.error_code.unwrap_or(i32::from(status.as_u16())),
        description: parsed
            .description
            .unwrap_or_else(|| "Unknown Telegram API error".to_string()),
    })
}

/// Updates `chat_data` or `public_data` of an existing HTML widget via Bot API 10.4+.
#[allow(dead_code)]
pub async fn edit_message_widget_data(
    bot_token: &str,
    chat_id: i64,
    message_id: i32,
    chat_data: Option<&serde_json::Value>,
    public_data: Option<&serde_json::Value>,
) -> Result<(), WidgetError> {
    let client = get_widget_client();
    edit_message_widget_data_with_client(
        &client,
        DEFAULT_TELEGRAM_API_BASE,
        bot_token,
        chat_id,
        message_id,
        chat_data,
        public_data,
    )
    .await
}

/// Updates widget state data with an explicit reqwest client and API base URL.
#[allow(dead_code)]
pub async fn edit_message_widget_data_with_client(
    client: &Client,
    api_base: &str,
    bot_token: &str,
    chat_id: i64,
    message_id: i32,
    chat_data: Option<&serde_json::Value>,
    public_data: Option<&serde_json::Value>,
) -> Result<(), WidgetError> {
    if chat_data.is_none() && public_data.is_none() {
        return Err(WidgetError::InvalidParameter(
            "Either chat_data or public_data must be provided".to_string(),
        ));
    }

    let chat_data_str = serialize_widget_data_value("chat_data", chat_data)?;
    let public_data_str = serialize_widget_data_value("public_data", public_data)?;

    let url = format!("{api_base}/bot{bot_token}/editMessageWidgetData");
    let payload = EditMessageWidgetDataPayload {
        chat_id,
        message_id,
        chat_data: chat_data_str,
        public_data: public_data_str,
    };

    let response = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| map_http_error(e, bot_token))?;
    let status = response.status();
    let body_text = response
        .text()
        .await
        .map_err(|e| map_http_error(e, bot_token))?;

    let parsed: RawTelegramEnvelope<serde_json::Value> =
        serde_json::from_str(&body_text).map_err(|e| {
            debug!(body = %body_text, "Failed to parse Telegram API response JSON");
            WidgetError::Serialization(e)
        })?;

    if parsed.ok {
        return Ok(());
    }

    Err(WidgetError::ApiError {
        code: parsed.error_code.unwrap_or(i32::from(status.as_u16())),
        description: parsed
            .description
            .unwrap_or_else(|| "Unknown Telegram API error".to_string()),
    })
}

fn serialize_widget_data_value(
    field_name: &str,
    value: Option<&serde_json::Value>,
) -> Result<Option<String>, WidgetError> {
    let Some(val) = value else {
        return Ok(None);
    };
    let encoded = match val {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other)?,
    };
    if encoded.len() > MAX_WIDGET_DATA_BYTES {
        return Err(WidgetError::InvalidParameter(format!(
            "{field_name} exceeds {MAX_WIDGET_DATA_BYTES} bytes ({})",
            encoded.len()
        )));
    }
    Ok(Some(encoded))
}

fn format_widget_html_file_caption(widget: &OutboundWidget) -> String {
    let prefix = "🎮 <b>";
    let suffix = format!(
        "</b>\n<i>点击附件即可在 Telegram 内置浏览器直接运行/游玩 · 纯离线 ({:.1} KB / gz {} B)</i>",
        widget.raw_bytes as f64 / 1024.0,
        widget.compressed_bytes
    );
    // Reserve char budget for prefix, suffix, and worst-case HTML entity expansion on title
    // before assembling HTML tags so closing `</b>` / `</i>` tags can never be truncated.
    let fixed_chars = prefix.chars().count() + suffix.chars().count();
    let title_budget = MAX_CAPTION_CHARS.saturating_sub(fixed_chars);
    let mut escaped_title = String::with_capacity(widget.title.len().min(title_budget * 2));
    let mut used_chars = 0usize;
    let limit_with_ellipsis = title_budget.saturating_sub(3);
    let mut checkpoint_len = 0usize;
    let mut truncated = false;

    for ch in widget.title.chars() {
        let ent = match ch {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '"' => Some("&quot;"),
            '\'' => Some("&#39;"),
            _ => None,
        };
        let step = ent.map_or(1, str::len);
        if used_chars + step > title_budget {
            truncated = true;
            break;
        }
        if let Some(e) = ent {
            escaped_title.push_str(e);
        } else {
            escaped_title.push(ch);
        }
        used_chars += step;
        if used_chars <= limit_with_ellipsis {
            checkpoint_len = escaped_title.len();
        }
    }
    if truncated {
        escaped_title.truncate(checkpoint_len);
        if title_budget >= 3 {
            escaped_title.push_str("...");
        }
    }

    format!("{prefix}{escaped_title}{suffix}")
}

/// Sends a widget directly as a standalone playable `.html` document attachment.
pub async fn send_widget_html_file(
    bot: &Bot,
    chat_id: ChatId,
    widget: &OutboundWidget,
) -> ResponseResult<()> {
    let file =
        InputFile::memory(widget.html.as_bytes().to_vec()).file_name(widget.file_name.clone());
    let caption = format_widget_html_file_caption(widget);
    bot.send_document(chat_id, file)
        .caption(caption)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Delivers an [`OutboundWidget`] with automatic fallback:
/// 1. Private chats (`chat_id > 0`) & compressed `<= 4 KB` (unless cached unavailable for this bot
///    token): attempts Bot API `sendWidget`.
/// 2. On group chat, `> 4 KB` compressed payload, cached unavailability, or Telegram API error
///    (e.g. `INPUT_MEDIA_WIDGET_NOT_ALLOWED`, cached for 24h):
///    - `WidgetKind::InteractiveApp` -> sends standalone playable `.html` document attachment (or HTML text fallback if upload fails).
///    - `WidgetKind::Card` -> sends formatted chunked Telegram HTML card summary.
pub async fn deliver_widget_with_fallback(
    bot: &Bot,
    bot_token: &str,
    chat_id: ChatId,
    widget: &OutboundWidget,
) -> WidgetDeliveryOutcome {
    if chat_id.0 > 0 && widget.is_send_widget_eligible() {
        if is_send_widget_cached_unavailable(bot_token) {
            debug!(
                chat_id = chat_id.0,
                title = %widget.title,
                "skipping sendWidget (cached unavailable for 24h); using fallback directly"
            );
        } else {
            let opts = SendWidgetOptions {
                width: widget.width,
                height: widget.height,
                caption: widget.caption.as_deref(),
                ..Default::default()
            };
            match send_widget_with_options(bot_token, chat_id.0, &widget.html, opts).await {
                Ok(resp) => {
                    info!(
                        chat_id = chat_id.0,
                        message_id = resp.message_id,
                        title = %widget.title,
                        compressed_bytes = widget.compressed_bytes,
                        "sent Telegram QuickJS widget"
                    );
                    return WidgetDeliveryOutcome::Widget {
                        message_id: resp.message_id,
                    };
                }
                Err(err) if is_send_widget_not_allowed_error(&err) => {
                    mark_send_widget_unavailable(bot_token);
                    info!(
                        chat_id = chat_id.0,
                        title = %widget.title,
                        "sendWidget rejected with INPUT_MEDIA_WIDGET_NOT_ALLOWED; caching fallback for 24h"
                    );
                }
                Err(err) => {
                    warn!(
                        chat_id = chat_id.0,
                        title = %widget.title,
                        error = ?err,
                        "sendWidget unavailable; falling back cleanly"
                    );
                }
            }
        }
    }

    if widget.kind == WidgetKind::InteractiveApp {
        match send_widget_html_file(bot, chat_id, widget).await {
            Ok(()) => {
                info!(
                    chat_id = chat_id.0,
                    file = %widget.file_name,
                    "delivered interactive widget as standalone .html attachment"
                );
                return WidgetDeliveryOutcome::HtmlDocumentFallback;
            }
            Err(err) => {
                warn!(
                    chat_id = chat_id.0,
                    file = %widget.file_name,
                    error = ?err,
                    "failed to send .html attachment; falling back to HTML text"
                );
            }
        }
    }

    send_html_fallback_chunks(bot, chat_id, &widget.fallback_html).await;
    WidgetDeliveryOutcome::HtmlTextFallback
}

async fn send_html_fallback_chunks(bot: &Bot, chat_id: ChatId, html: &str) {
    let chunks = TelegramFormatter::split_html_chunks(html, TELEGRAM_TEXT_LIMIT);
    for chunk in chunks {
        if let Err(err) = bot
            .send_message(chat_id, &chunk)
            .parse_mode(ParseMode::Html)
            .await
        {
            warn!(chat_id = chat_id.0, error = ?err, "HTML card fallback rejected; stripping tags");
            let plain = text::strip_html_tags(&chunk);
            if !plain.trim().is_empty()
                && let Err(plain_err) = bot.send_message(chat_id, plain).await
            {
                warn!(chat_id = chat_id.0, error = ?plain_err, "plain fallback send failed");
            }
        }
    }
}

// ============================================================================
// Embedded CSS/JS Templates & Built-in Offline QuickJS Mini-Games
// ============================================================================

const CARD_INLINE_CSS: &str = r#":root{--bg:var(--tg-theme-bg-color,#18222d);--text:var(--tg-theme-text-color,#fff);--hint:var(--tg-theme-hint-color,#7d8b99);--sec-bg:var(--tg-theme-secondary-bg-color,#131b24);--hdr-bg:var(--tg-theme-header-bg-color,#242f3d);--accent:var(--tg-theme-accent-text-color,#64b5f6);--border:rgba(255,255,255,0.08);--ok:#4caf50;--warn:#ff9800;--err:#f44336}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--text:var(--tg-theme-text-color,#222);--hint:var(--tg-theme-hint-color,#8e8e93);--sec-bg:var(--tg-theme-secondary-bg-color,#f4f4f5);--hdr-bg:var(--tg-theme-header-bg-color,#f7f7f8);--accent:var(--tg-theme-accent-text-color,#2481cc);--border:rgba(0,0,0,0.08)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,sans-serif;background:var(--bg);color:var(--text);padding:10px;font-size:13.5px;line-height:1.45}.widget-card{background:var(--sec-bg);border:1px solid var(--border);border-radius:12px;overflow:hidden}.card-header{padding:10px 12px;display:flex;align-items:center;justify-content:space-between;background:var(--hdr-bg);border-bottom:1px solid var(--border)}.agent-info{display:flex;align-items:center;gap:8px}.agent-avatar{font-size:18px}.agent-meta{display:flex;flex-direction:column}.agent-title{font-weight:600;font-size:13px}.agent-subtitle{font-size:11px;color:var(--hint)}.status-badge{display:inline-flex;align-items:center;gap:5px;padding:3px 8px;border-radius:12px;font-size:11.5px;font-weight:500}.status-badge.thinking{background:rgba(100,181,246,0.15);color:var(--accent)}.status-badge.executing{background:rgba(255,152,0,0.15);color:var(--warn)}.status-badge.finished{background:rgba(76,175,80,0.15);color:var(--ok)}.status-badge.error{background:rgba(244,67,54,0.15);color:var(--err)}.pulse-dot{width:7px;height:7px;border-radius:50%;background:currentColor;display:inline-block}.spinner-icon{width:10px;height:10px;border:2px solid currentColor;border-top-color:transparent;border-radius:50%;display:inline-block}.tabs-nav{display:flex;background:var(--hdr-bg);border-bottom:1px solid var(--border)}.tab-btn{flex:1;padding:8px 0;border:none;background:transparent;color:var(--hint);font-size:12.5px;font-weight:500;cursor:pointer;border-bottom:2px solid transparent}.tab-btn.active{color:var(--accent);border-bottom:2px solid var(--accent)}.tab-content{padding:12px}.tab-panel{display:none}.tab-panel.active{display:block}.result-body{font-size:13px;line-height:1.55;white-space:pre-wrap;word-break:break-word}.accordion{display:flex;flex-direction:column;gap:6px}.accordion-item{border:1px solid var(--border);border-radius:8px;background:var(--bg);overflow:hidden}.accordion-header{padding:8px 10px;font-size:12px;font-weight:500;display:flex;justify-content:space-between;align-items:center;cursor:pointer;user-select:none}.accordion-chevron{font-size:9px;color:var(--hint)}.accordion-body{display:none;padding:8px 10px;font-size:11.5px;color:var(--hint);border-top:1px solid var(--border)}.accordion-item.open .accordion-body{display:block}.accordion-body pre{white-space:pre-wrap;word-break:break-all;font-family:ui-monospace,Consolas,monospace}.card-footer{padding:6px 12px;font-size:10.5px;color:var(--hint);border-top:1px solid var(--border);display:flex;justify-content:space-between;align-items:center}"#;

const CARD_INLINE_JS: &str = r#"var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function switchTab(t){document.querySelectorAll('.tab-btn').forEach(function(b){b.classList.toggle('active',b.dataset.tab===t)});document.querySelectorAll('.tab-panel').forEach(function(p){p.classList.toggle('active',p.id==='panel-'+t)});try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred('light')}catch(e){}}
function toggleAccordion(h){h.parentElement.classList.toggle('open')}
function applyWidgetState(d){if(!d)return;if(typeof d==='string'){try{d=JSON.parse(d)}catch(e){return}}if(d.status){var b=document.getElementById('statusBadge');if(b){var s=String(d.status).toLowerCase();b.className='status-badge '+s;var ic=s==='thinking'?'<span class="pulse-dot"></span>':s==='executing'?'<span class="spinner-icon"></span>':s==='finished'?'<span>✓</span>':'<span>!</span>';var tx=s==='thinking'?'思考中':s==='executing'?'工具执行中':s==='finished'?'已完成':'执行异常';b.innerHTML=ic+' <span>'+tx+'</span>'}}if(d.result){var r=document.getElementById('resultBody');if(r)r.textContent=d.result}}
if(W){if(W.chatData)applyWidgetState(W.chatData);if(W.publicData)applyWidgetState(W.publicData);if(typeof W.onEvent==='function'){W.onEvent('chatDataChanged',applyWidgetState);W.onEvent('publicDataChanged',applyWidgetState);W.onEvent('data_updated',applyWidgetState)}}"#;

const CUSTOM_CARD_BASE_CSS: &str = r#":root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);--ht:var(--tg-theme-hint-color,#8b9bb4);--cd:var(--tg-theme-secondary-bg-color,#131b24);--hd:var(--tg-theme-header-bg-color,#242f3d);--ac:var(--tg-theme-accent-text-color,#64b5f6);--bd:rgba(128,128,128,.2);--ok:#4caf50;--wn:#ff9800;--er:#f44336}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);--cd:var(--tg-theme-secondary-bg-color,#f4f4f5);--hd:var(--tg-theme-header-bg-color,#f7f7f8);--ac:var(--tg-theme-accent-text-color,#2481cc)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,Roboto,sans-serif;background:var(--bg);color:var(--fg);padding:8px;font-size:13px;line-height:1.45}.card{background:var(--cd);border:1px solid var(--bd);border-radius:12px;overflow:hidden}.hdr{padding:10px 12px;background:var(--hd);border-bottom:1px solid var(--bd);display:flex;justify-content:space-between;align-items:center;gap:8px}.ttl{font-weight:700;font-size:14px}.sub{font-size:11px;color:var(--ht);margin-top:2px}.bg{padding:2px 8px;border-radius:10px;font-size:11px;font-weight:600;white-space:nowrap}.bg.ok{background:rgba(76,175,80,.18);color:var(--ok)}.bg.warn{background:rgba(255,152,0,.18);color:var(--wn)}.bg.err{background:rgba(244,67,54,.18);color:var(--er)}.bg.info{background:rgba(100,181,246,.18);color:var(--ac)}.sec{padding:10px 12px}.sttl{font-weight:600;font-size:12.5px;margin-bottom:4px}.sum{font-size:13px;white-space:pre-wrap;word-break:break-word}.kpis{display:grid;grid-template-columns:repeat(2,1fr);gap:6px;margin-bottom:8px}.kpi{background:var(--bg);border:1px solid var(--bd);border-radius:8px;padding:7px 9px}.kl{font-size:10.5px;color:var(--ht)}.kv{font-size:15px;font-weight:700;margin-top:2px}.kd{font-size:10.5px;margin-top:1px;color:var(--ht)}.kd.ok{color:var(--ok)}.kd.warn{color:var(--wn)}.kd.err{color:var(--er)}.tabs{display:flex;background:var(--hd);border-top:1px solid var(--bd);border-bottom:1px solid var(--bd)}.tb{flex:1;padding:7px 4px;border:none;background:transparent;color:var(--ht);font-size:12px;font-weight:600;cursor:pointer;border-bottom:2px solid transparent}.tb.on{color:var(--ac);border-bottom-color:var(--ac)}.tp{display:none}.tp.on{display:block}.lst{padding-left:16px;font-size:12.5px}.lst li{margin:3px 0}.acc{display:flex;flex-direction:column;gap:5px}.ai{border:1px solid var(--bd);border-radius:8px;background:var(--bg);overflow:hidden}.ah{padding:7px 10px;font-size:12px;font-weight:600;display:flex;justify-content:space-between;cursor:pointer}.ab{display:none;padding:7px 10px;font-size:11.5px;color:var(--ht);border-top:1px solid var(--bd);white-space:pre-wrap;word-break:break-word}.ai.on .ab{display:block}.ftr{padding:6px 12px;font-size:10.5px;color:var(--ht);border-top:1px solid var(--bd);display:flex;justify-content:space-between}"#;

const CUSTOM_CARD_TABLE_CSS: &str = r#"table{width:100%;border-collapse:collapse;font-size:12px}th,td{text-align:left;padding:5px 6px;border-bottom:1px solid var(--bd)}th{color:var(--ht);font-weight:600;font-size:11px}"#;

const CUSTOM_CARD_QUIZ_CSS: &str = r#".qz{display:flex;flex-direction:column;gap:8px}.qq{background:var(--bg);border:1px solid var(--bd);border-radius:8px;padding:8px 10px}.qt{font-weight:600;font-size:12.5px;margin-bottom:6px}.qo{display:grid;gap:4px}.qb{text-align:left;padding:6px 8px;border:1px solid var(--bd);border-radius:6px;background:var(--cd);color:var(--fg);font-size:12px;cursor:pointer}.qb.ok{background:rgba(76,175,80,.2);border-color:var(--ok)}.qb.no{background:rgba(244,67,54,.2);border-color:var(--er)}.qe{font-size:11px;color:var(--ht);margin-top:5px;display:none}.qe.on{display:block}"#;

const CUSTOM_CARD_BASE_JS: &str = r#"var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function hap(k){try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred(k||'light')}catch(e){}}
function st(i){document.querySelectorAll('.tb').forEach(function(b){b.classList.toggle('on',b.dataset.t===i)});document.querySelectorAll('.tp').forEach(function(p){p.classList.toggle('on',p.id==='tp-'+i)});hap('light')}
function ta(h){h.parentElement.classList.toggle('on');hap('light')}
function ap(d){if(!d)return;if(typeof d==='string'){try{d=JSON.parse(d)}catch(e){return}}var b=document.getElementById('cbg');if(b&&d.badge)b.textContent=d.badge;var s=document.getElementById('csum');if(s&&d.summary)s.textContent=d.summary}
if(W){if(W.chatData)ap(W.chatData);if(W.publicData)ap(W.publicData);if(typeof W.onEvent==='function'){W.onEvent('chatDataChanged',ap);W.onEvent('publicDataChanged',ap);W.onEvent('data_updated',ap)}}"#;

const CUSTOM_CARD_QUIZ_JS: &str = r#"
function qa(btn,qi,oi,ans){var box=document.getElementById('qq-'+qi);if(!box||box.dataset.done)return;box.dataset.done='1';var bs=box.querySelectorAll('.qb');bs.forEach(function(b,idx){if(idx===ans)b.classList.add('ok');else if(idx===oi)b.classList.add('no')});var ex=box.querySelector('.qe');if(ex)ex.classList.add('on');var sc=document.getElementById('qsc');if(sc){var p=sc.dataset;p.n=(+p.n||0)+1;if(oi===ans)p.ok=(+p.ok||0)+1;sc.textContent='得分 '+p.ok+'/'+p.n}hap(oi===ans?'medium':'heavy')}"#;

const GAME_2048_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no"><title>2048</title><style>:root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);--ht:var(--tg-theme-hint-color,#8b9bb4);--bt:var(--tg-theme-button-color,#2b5278);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#131b24);--bd:rgba(128,128,128,.2)}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);--bt:var(--tg-theme-button-color,#2481cc);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#f2f3f5)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,Roboto,sans-serif;background:var(--bg);color:var(--fg);padding:8px;user-select:none;max-width:340px;margin:0 auto}.top{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;gap:6px}.ttl{font-weight:700;font-size:17px}.scs{display:flex;gap:5px;align-items:center}.bx{background:var(--cd);border:1px solid var(--bd);border-radius:8px;padding:3px 8px;text-align:center;min-width:52px}.bx small{display:block;font-size:9px;color:var(--ht);text-transform:uppercase}.bx b{font-size:13px}.btn{background:var(--bt);color:var(--bf);border:none;border-radius:8px;padding:7px 10px;font-size:12px;font-weight:600;cursor:pointer}.grid{display:grid;grid-template-columns:repeat(4,1fr);gap:6px;background:var(--cd);padding:6px;border-radius:10px;border:1px solid var(--bd);touch-action:none}.c{height:58px;border-radius:7px;display:flex;align-items:center;justify-content:center;font-weight:700;font-size:18px;background:rgba(128,128,128,.12);color:var(--fg)}.t2{background:#eee4da;color:#776e65}.t4{background:#ede0c8;color:#776e65}.t8{background:#f2b179;color:#fff}.t16{background:#f59563;color:#fff}.t32{background:#f67c5f;color:#fff}.t64{background:#f65e3b;color:#fff}.t128{background:#edcf72;color:#fff;font-size:15px}.t256{background:#edcc61;color:#fff;font-size:15px}.t512{background:#edc850;color:#fff;font-size:15px}.t1024{background:#edc53f;color:#fff;font-size:13px}.t2048{background:#edc22e;color:#fff;font-size:13px}.pad{display:grid;grid-template-columns:repeat(3,48px);gap:5px;justify-content:center;margin-top:8px}.pad button{height:34px;border-radius:8px;border:1px solid var(--bd);background:var(--cd);color:var(--fg);font-size:15px;cursor:pointer}.pad .sp{visibility:hidden}#st{text-align:center;font-size:12px;color:var(--ht);margin-top:6px;min-height:16px}</style></head><body><div class="top"><span class="ttl">2048</span><div class="scs"><div class="bx"><small>Score</small><b id="sc">0</b></div><div class="bx"><small>Best</small><b id="bs">0</b></div><button class="btn" id="nw">New</button></div></div><div class="grid" id="bd"></div><div class="pad"><span class="sp"></span><button data-d="0">▲</button><span class="sp"></span><button data-d="3">◀</button><button data-d="2">▼</button><button data-d="1">▶</button></div><div id="st">滑动棋盘或点击方向键</div><script>
var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function hap(k){try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred(k||'light')}catch(e){}}
var g=[],sc=0,bs=0,won=0,bd=document.getElementById('bd'),scE=document.getElementById('sc'),bsE=document.getElementById('bs'),st=document.getElementById('st');
try{bs=+(localStorage.getItem('tg2048b')||0)}catch(e){}
function add(){var z=[];for(var i=0;i<16;i++)if(!g[i])z.push(i);if(z.length)g[z[Math.random()*z.length|0]]=Math.random()<.9?2:4}
function draw(){var h='';for(var i=0;i<16;i++){var v=g[i];h+='<div class="c'+(v?' t'+Math.min(v,2048):'')+'">'+(v||'')+'</div>'}bd.innerHTML=h;scE.textContent=sc;bsE.textContent=bs}
function init(){g=Array(16).fill(0);sc=0;won=0;st.textContent='滑动棋盘或点击方向键';add();add();draw()}
function slide(r){var a=r.filter(Boolean),pts=0;for(var i=0;i<a.length-1;i++)if(a[i]===a[i+1]){a[i]*=2;pts+=a[i];if(a[i]===2048&&!won){won=1;st.textContent='🎉 合成 2048！可继续挑战高分'}a.splice(i+1,1)}while(a.length<4)a.push(0);return[a,pts]}
function over(){for(var i=0;i<16;i++){if(!g[i])return 0;if(i%4<3&&g[i]===g[i+1])return 0;if(i<12&&g[i]===g[i+4])return 0}return 1}
function mv(d){var ch=0,pts=0;for(var i=0;i<4;i++){var ln=[];for(var j=0;j<4;j++){var idx=d===0?j*4+i:d===1?i*4+(3-j):d===2?(3-j)*4+i:i*4+j;ln.push(g[idx])}var res=slide(ln),nl=res[0];pts+=res[1];for(var j=0;j<4;j++){var idx=d===0?j*4+i:d===1?i*4+(3-j):d===2?(3-j)*4+i:i*4+j;if(g[idx]!==nl[j])ch=1;g[idx]=nl[j]}}if(!ch)return;sc+=pts;if(sc>bs){bs=sc;try{localStorage.setItem('tg2048b',bs)}catch(e){}}add();draw();hap(pts?'medium':'light');if(over())st.textContent='游戏结束 — 点击 New 重开'}
document.getElementById('nw').onclick=function(){hap('medium');init()};
document.querySelectorAll('.pad button').forEach(function(b){b.onclick=function(){mv(+b.dataset.d)}});
var sx=0,sy=0;
bd.addEventListener('touchstart',function(e){if(!e.touches||!e.touches[0])return;sx=e.touches[0].clientX;sy=e.touches[0].clientY},{passive:true});
bd.addEventListener('touchend',function(e){if(!e.changedTouches||!e.changedTouches[0])return;var dx=e.changedTouches[0].clientX-sx,dy=e.changedTouches[0].clientY-sy;if(Math.max(Math.abs(dx),Math.abs(dy))>22)mv(Math.abs(dx)>Math.abs(dy)?(dx>0?1:3):(dy>0?2:0))},{passive:true});
document.addEventListener('keydown',function(e){var m={ArrowUp:0,ArrowRight:1,ArrowDown:2,ArrowLeft:3,w:0,d:1,s:2,a:3};if(m[e.key]!==undefined){e.preventDefault();mv(m[e.key])}});
init();
</script></body></html>"#;

const GAME_MINESWEEPER_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no"><title>扫雷 8x8</title><style>:root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);--ht:var(--tg-theme-hint-color,#8b9bb4);--bt:var(--tg-theme-button-color,#2b5278);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#131b24);--bd:rgba(128,128,128,.25)}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);--bt:var(--tg-theme-button-color,#2481cc);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#f2f3f5)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,Roboto,sans-serif;background:var(--bg);color:var(--fg);padding:8px;user-select:none;max-width:340px;margin:0 auto}.top{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;gap:4px}.ttl{font-weight:700;font-size:14px}.bar{display:flex;gap:4px;align-items:center}.pill{background:var(--cd);border:1px solid var(--bd);border-radius:8px;padding:4px 7px;font-size:11.5px;font-weight:600}.btn{background:var(--bt);color:var(--bf);border:none;border-radius:8px;padding:5px 9px;font-size:11.5px;font-weight:600;cursor:pointer}.btn.alt{background:var(--cd);color:var(--fg);border:1px solid var(--bd)}.g{display:grid;grid-template-columns:repeat(8,1fr);gap:3px;background:var(--cd);padding:5px;border-radius:10px;border:1px solid var(--bd)}.cell{height:34px;border:none;border-radius:5px;background:rgba(128,128,128,.22);color:var(--fg);font-weight:700;font-size:13px;cursor:pointer;display:flex;align-items:center;justify-content:center}.cell.open{background:var(--bg);border:1px solid var(--bd);cursor:default}.cell.mine{background:#e53935;color:#fff}.c1{color:#4fc3f7}.c2{color:#66bb6a}.c3{color:#ef5350}.c4{color:#ab47bc}.c5{color:#ffa726}.c6{color:#26c6da}.c7{color:#ec407a}.c8{color:var(--ht)}#st{text-align:center;font-size:12px;color:var(--ht);margin-top:7px;min-height:16px}</style></head><body><div class="top"><span class="ttl">💣 扫雷</span><div class="bar"><span class="pill" id="fl">🚩 10</span><span class="pill" id="tm">⏱ 0s</span><button class="btn alt" id="md">⛏ 挖雷</button><button class="btn" id="rs">🔄</button></div></div><div class="g" id="bd"></div><div id="st">点击格子开始，切换 🚩 可插旗标记</div><script>
var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function hap(k){try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred(k||'light')}catch(e){}}
var N=8,M=10,b=[],op=[],fg=[],dead=0,won=0,first=1,flagMode=0,sec=0,ti=0;
var bd=document.getElementById('bd'),flE=document.getElementById('fl'),tmE=document.getElementById('tm'),mdE=document.getElementById('md'),st=document.getElementById('st');
function stopT(){if(ti){clearInterval(ti);ti=0}}
function nb(i){var r=i/N|0,c=i%N,o=[];for(var dr=-1;dr<=1;dr++)for(var dc=-1;dc<=1;dc++){if(!dr&&!dc)continue;var nr=r+dr,nc=c+dc;if(nr>=0&&nr<N&&nc>=0&&nc<N)o.push(nr*N+nc)}return o}
function place(safe){var p=[];for(var i=0;i<N*N;i++)if(i!==safe)p.push(i);for(var i=p.length-1;i>0;i--){var j=Math.random()*(i+1)|0,t=p[i];p[i]=p[j];p[j]=t}for(var k=0;k<M;k++)b[p[k]]=-1;for(var i=0;i<N*N;i++)if(b[i]!==-1){var cnt=0;nb(i).forEach(function(u){if(b[u]===-1)cnt++});b[i]=cnt}}
function reveal(idx){if(op[idx]||fg[idx])return;op[idx]=1;if(b[idx]===0){var q=[idx];while(q.length){var cur=q.shift();nb(cur).forEach(function(u){if(!op[u]&&!fg[u]){op[u]=1;if(b[u]===0)q.push(u)}})}}}
function checkWin(){var c=0;for(var i=0;i<N*N;i++)if(op[i])c++;if(c===N*N-M){won=1;stopT();st.textContent='🎉 通关成功！用时 '+sec+' 秒';hap('heavy')}}
function draw(){var fc=0;for(var i=0;i<N*N;i++)if(fg[i])fc++;flE.textContent='🚩 '+(M-fc);var h='';for(var i=0;i<N*N;i++){var cls='cell',txt='';if(op[i]){if(b[i]===-1){cls+=' open mine';txt='💣'}else{cls+=' open';if(b[i]>0){cls+=' c'+b[i];txt=b[i]}}}else if(fg[i]){txt='🚩'}h+='<button class="'+cls+'" data-i="'+i+'">'+txt+'</button>'}bd.innerHTML=h}
function hit(i,right){if(dead||won)return;if(right||flagMode){if(op[i])return;fg[i]=!fg[i];hap('light');draw();return}if(fg[i])return;if(first){first=0;place(i);ti=setInterval(function(){sec++;tmE.textContent='⏱ '+sec+'s'},1000)}if(b[i]===-1){dead=1;stopT();for(var k=0;k<N*N;k++)if(b[k]===-1)op[k]=1;st.textContent='💥 踩雷了！点击 🔄 重开';hap('heavy');draw();return}reveal(i);checkWin();hap('light');draw()}
function init(){stopT();b=Array(N*N).fill(0);op=Array(N*N).fill(0);fg=Array(N*N).fill(0);dead=0;won=0;first=1;sec=0;tmE.textContent='⏱ 0s';st.textContent='点击格子开始，切换 🚩 可插旗标记';draw()}
bd.onclick=function(e){var t=e.target;while(t&&t!==bd&&(!t.dataset||t.dataset.i===undefined))t=t.parentElement;if(t&&t.dataset&&t.dataset.i!==undefined)hit(+t.dataset.i,0)};
bd.oncontextmenu=function(e){e.preventDefault();var t=e.target;while(t&&t!==bd&&(!t.dataset||t.dataset.i===undefined))t=t.parentElement;if(t&&t.dataset&&t.dataset.i!==undefined)hit(+t.dataset.i,1)};
mdE.onclick=function(){flagMode=!flagMode;mdE.textContent=flagMode?'🚩 标旗':'⛏ 挖雷';hap('light')};
document.getElementById('rs').onclick=function(){hap('medium');init()};
init();
</script></body></html>"#;

const GAME_MEMORY_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no"><title>记忆翻牌</title><style>:root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);--ht:var(--tg-theme-hint-color,#8b9bb4);--bt:var(--tg-theme-button-color,#2b5278);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#131b24);--bd:rgba(128,128,128,.25)}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);--bt:var(--tg-theme-button-color,#2481cc);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#f2f3f5)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,Roboto,sans-serif;background:var(--bg);color:var(--fg);padding:8px;user-select:none;max-width:340px;margin:0 auto}.top{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;gap:6px}.ttl{font-weight:700;font-size:15px}.bar{display:flex;gap:6px;align-items:center}.pill{background:var(--cd);border:1px solid var(--bd);border-radius:8px;padding:4px 8px;font-size:12px;font-weight:600}.btn{background:var(--bt);color:var(--bf);border:none;border-radius:8px;padding:5px 10px;font-size:12px;font-weight:600;cursor:pointer}.g{display:grid;grid-template-columns:repeat(4,1fr);gap:7px;background:var(--cd);padding:8px;border-radius:10px;border:1px solid var(--bd)}.card{height:60px;border:1px solid var(--bd);border-radius:9px;background:var(--bt);color:var(--bf);font-size:24px;cursor:pointer;display:flex;align-items:center;justify-content:center}.card.open{background:var(--bg);color:var(--fg)}.card.ok{background:rgba(76,175,80,.2);border-color:#4caf50;cursor:default}#st{text-align:center;font-size:12px;color:var(--ht);margin-top:7px;min-height:16px}</style></head><body><div class="top"><span class="ttl">🃏 记忆翻牌</span><div class="bar"><span class="pill" id="mv">步数 0</span><span class="pill" id="bs">最佳 -</span><button class="btn" id="rs">🔄</button></div></div><div class="g" id="bd"></div><div id="st">翻开两张相同的图标即可配对</div><script>
var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function hap(k){try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred(k||'light')}catch(e){}}
var icons=['🚀','🤖','⚡','🎯','🔥','💎','🧩','🎮'],deck=[],open=[],done=[],moves=0,best=0,lock=0;
var bd=document.getElementById('bd'),mvE=document.getElementById('mv'),bsE=document.getElementById('bs'),st=document.getElementById('st');
try{best=+(localStorage.getItem('tgmemb')||0)}catch(e){}
function draw(){mvE.textContent='步数 '+moves;bsE.textContent='最佳 '+(best||'-');var h='';for(var i=0;i<16;i++){var isO=open.indexOf(i)>=0,isD=done[i],cls='card'+(isD?' ok':isO?' open':'');h+='<button class="'+cls+'" data-i="'+i+'">'+(isO||isD?deck[i]:'?')+'</button>'}bd.innerHTML=h}
function init(){deck=icons.concat(icons);for(var i=deck.length-1;i>0;i--){var j=Math.random()*(i+1)|0,t=deck[i];deck[i]=deck[j];deck[j]=t}open=[];done=Array(16).fill(0);moves=0;lock=0;st.textContent='翻开两张相同的图标即可配对';draw()}
bd.onclick=function(e){var t=e.target;while(t&&t!==bd&&(!t.dataset||t.dataset.i===undefined))t=t.parentElement;if(!t||!t.dataset||t.dataset.i===undefined||lock)return;var i=+t.dataset.i;if(done[i]||open.indexOf(i)>=0)return;open.push(i);hap('light');draw();if(open.length===2){moves++;var a=open[0],b=open[1];if(deck[a]===deck[b]){done[a]=done[b]=1;open=[];hap('medium');draw();if(done.every(Boolean)){if(!best||moves<best){best=moves;try{localStorage.setItem('tgmemb',best)}catch(e){}}st.textContent='🎉 全部配对成功！共 '+moves+' 步';hap('heavy');draw()}}else{lock=1;setTimeout(function(){open=[];lock=0;draw()},520)}}};
document.getElementById('rs').onclick=function(){hap('medium');init()};
init();
</script></body></html>"#;

const GAME_REACTION_HTML: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1,user-scalable=no"><title>极速反应测试</title><style>:root{--bg:var(--tg-theme-bg-color,#18222d);--fg:var(--tg-theme-text-color,#fff);--ht:var(--tg-theme-hint-color,#8b9bb4);--bt:var(--tg-theme-button-color,#2b5278);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#131b24);--bd:rgba(128,128,128,.25)}@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--fg:var(--tg-theme-text-color,#222);--ht:var(--tg-theme-hint-color,#777);--bt:var(--tg-theme-button-color,#2481cc);--bf:var(--tg-theme-button-text-color,#fff);--cd:var(--tg-theme-secondary-bg-color,#f2f3f5)}}*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,Roboto,sans-serif;background:var(--bg);color:var(--fg);padding:8px;user-select:none;max-width:340px;margin:0 auto}.top{display:flex;justify-content:space-between;align-items:center;margin-bottom:8px;gap:6px}.ttl{font-weight:700;font-size:15px}.bar{display:flex;gap:6px}.pill{background:var(--cd);border:1px solid var(--bd);border-radius:8px;padding:4px 8px;font-size:12px;font-weight:600}.pad{width:100%;height:170px;border:none;border-radius:14px;font-size:20px;font-weight:700;color:#fff;cursor:pointer;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:6px}.pad small{font-size:12px;font-weight:400;opacity:.9}.idle{background:var(--bt);color:var(--bf)}.wait{background:#d97706}.go{background:#16a34a}.early{background:#dc2626}.hist{display:flex;gap:5px;justify-content:center;margin-top:8px;flex-wrap:wrap}.tag{background:var(--cd);border:1px solid var(--bd);border-radius:6px;padding:3px 7px;font-size:11px;color:var(--ht)}</style></head><body><div class="top"><span class="ttl">⚡ 极速反应</span><div class="bar"><span class="pill" id="av">均速 -</span><span class="pill" id="bs">最快 -</span></div></div><button class="pad idle" id="pd"><span>点击开始测试</span><small>变成绿色时立刻点击</small></button><div class="hist" id="hs"></div><script>
var W=typeof TelegramWidget!=='undefined'?TelegramWidget:null;
function hap(k){try{if(W&&W.HapticFeedback)W.HapticFeedback.impactOccurred(k||'light')}catch(e){}}
var st=0,tm=0,t0=0,log=[],best=0;
var pd=document.getElementById('pd'),avE=document.getElementById('av'),bsE=document.getElementById('bs'),hs=document.getElementById('hs');
try{best=+(localStorage.getItem('tgreactb')||0)}catch(e){}
function stats(){bsE.textContent='最快 '+(best?best+'ms':'-');if(log.length){var sum=log.reduce(function(a,b){return a+b},0);avE.textContent='均速 '+Math.round(sum/log.length)+'ms'}hs.innerHTML=log.map(function(v,i){return'<span class="tag">#'+(i+1)+' '+v+'ms</span>'}).join('')}
function setPad(cls,a,b){pd.className='pad '+cls;pd.innerHTML='<span>'+a+'</span><small>'+b+'</small>'}
pd.onclick=function(){if(st===0||st===3||st===4){st=1;setPad('wait','等待变绿...','千万别抢跑');hap('light');clearTimeout(tm);tm=setTimeout(function(){st=2;t0=Date.now();setPad('go','⚡ 立即点击!','GO GO GO');hap('medium')},1100+Math.random()*2400)}else if(st===1){clearTimeout(tm);st=3;setPad('early','太早了! 抢跑犯规','点击重试');hap('heavy')}else if(st===2){var ms=Math.max(1,Date.now()-t0);st=4;log.push(ms);if(log.length>5)log.shift();if(!best||ms<best){best=ms;try{localStorage.setItem('tgreactb',best)}catch(e){}}setPad('idle',ms+' ms','点击再测一次');hap('heavy');stats()}};
stats();
</script></body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_raw_and_compressed_sizes_within_limits() {
        let demo = AgentCardData::demo();
        let html = demo.render_html().expect("AgentCard HTML render failed");
        let compressed = measure_compressed_bytes(&html);
        assert!(html.len() <= MAX_RAW_HTML_BYTES);
        assert!(
            compressed <= MAX_COMPRESSED_HTML_BYTES,
            "AgentCard compressed {} B > {} B",
            compressed,
            MAX_COMPRESSED_HTML_BYTES
        );
        validate_widget_html(&html).expect("AgentCard demo should pass full widget validation");
    }

    #[test]
    fn test_all_builtin_games_fit_under_4kb_compressed() {
        for game in BuiltinGame::all() {
            let widget = game
                .to_outbound_widget()
                .unwrap_or_else(|e| panic!("{} failed: {e}", game.id()));
            assert!(
                widget.raw_bytes <= MAX_RAW_HTML_BYTES,
                "{} raw {} > 32KB",
                game.id(),
                widget.raw_bytes
            );
            assert!(
                widget.compressed_bytes <= MAX_COMPRESSED_HTML_BYTES,
                "{} compressed {} > 4KB",
                game.id(),
                widget.compressed_bytes
            );
            assert!(
                widget.is_send_widget_eligible(),
                "{} should be sendWidget eligible",
                game.id()
            );
            assert!(
                widget.warnings.is_empty(),
                "{} had unexpected sandbox warnings: {:?}",
                game.id(),
                widget.warnings
            );
        }
        assert_eq!(BuiltinGame::parse("扫雷"), Some(BuiltinGame::Minesweeper));
        assert_eq!(BuiltinGame::parse("2048"), Some(BuiltinGame::Game2048));
        assert_eq!(
            BuiltinGame::parse("记忆翻牌"),
            Some(BuiltinGame::MemoryMatch)
        );
        assert_eq!(
            BuiltinGame::parse("reaction"),
            Some(BuiltinGame::ReactionTap)
        );
    }

    #[test]
    fn test_all_custom_card_demos_fit_under_4kb_compressed() {
        for card in [
            CustomCardSpec::demo_dashboard(),
            CustomCardSpec::demo_quiz(),
            CustomCardSpec::demo_comparison(),
        ] {
            let widget = OutboundWidget::from_custom_card(&card).expect("custom card compile");
            assert!(
                widget.compressed_bytes <= MAX_COMPRESSED_HTML_BYTES,
                "card '{}' compressed {} B > 4KB",
                card.title,
                widget.compressed_bytes
            );
            assert!(widget.is_send_widget_eligible());
            let fb = card.render_fallback_html();
            assert!(fb.contains(text::escape_html(&card.title).as_ref()));
        }
    }

    #[test]
    fn test_custom_card_json_aliases_and_escaping() {
        let json = r#"{
            "kind": "dashboard",
            "title": "Build <v1.6> & Status",
            "badge": "OK",
            "tabs": [
                {"title": "Tab <1>", "items": ["Item & A"]}
            ],
            "timeline": [
                {"title": "Step 1", "details": "<script>xss()</script>", "is_expanded": true}
            ]
        }"#;
        let spec = CustomCardSpec::from_json(json).expect("should parse aliases");
        assert_eq!(spec.kind, CardKind::Metric);
        assert_eq!(spec.sections.len(), 1);
        assert_eq!(spec.steps.len(), 1);

        let html = spec.render_html().expect("render html");
        assert!(!html.contains("<script>xss()</script>"));
        assert!(html.contains("&lt;script&gt;xss()&lt;/script&gt;"));
        assert!(html.contains("Build &lt;v1.6&gt; &amp; Status"));
    }

    #[test]
    fn test_sanitizer_strips_external_scripts_and_wraps_fragments() {
        let dirty = r#"<!-- comment --><script src="https://evil.example/a.js"></script>
        <link rel="stylesheet" href="//cdn.example/s.css">
        <div id="box">Hello</div>
        <input type="text">
        <script>
        // inline comment
        var x = `keep   spaces`;
        fetch('/api');
        </script>"#;

        let (sanitized, warnings) = sanitize_widget_html(dirty);
        assert!(!sanitized.contains("evil.example"));
        assert!(!sanitized.contains("cdn.example"));
        assert!(sanitized.contains("<!DOCTYPE html>"));
        assert!(sanitized.contains("name=\"viewport\""));
        assert!(sanitized.contains("--tg-theme-bg-color"));
        assert!(warnings.iter().any(|w| w.contains("external <script")));
        assert!(warnings.iter().any(|w| w.contains("network API")));
        assert!(warnings.iter().any(|w| w.contains("form control")));

        let minified = minify_widget_html(&sanitized);
        assert!(minified.len() < sanitized.len());
        assert!(minified.contains("`keep   spaces`"));
        assert!(!minified.contains("// inline comment"));
        assert!(!minified.contains("<!-- comment -->"));
    }

    #[test]
    fn test_extract_ai_widgets_parses_tg_widget_and_tg_card() {
        let reply = r#"这是为你做的测试卡片与小游戏：

````tg-widget
<!DOCTYPE html><html><head><title>Mini Clicker</title></head><body><button id="b" onclick="c()">0</button><script>var n=0;function c(){document.getElementById('b').textContent=++n}</script></body></html>
````

```tg-card
{"kind":"quiz","title":"Rust Quiz","quiz":[{"question":"Edition?","options":["2021","2024"],"answer_index":1}]}
```

```rust
fn regular_code() {}
```

玩得开心！"#;

        let (cleaned, widgets) = extract_ai_widgets(reply);
        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets[0].kind, WidgetKind::InteractiveApp);
        assert_eq!(widgets[0].title, "Mini Clicker");
        assert!(widgets[0].is_send_widget_eligible());
        assert_eq!(widgets[1].kind, WidgetKind::Card);
        assert_eq!(widgets[1].title, "Rust Quiz");
        assert!(cleaned.contains("```rust\nfn regular_code() {}\n```"));
        assert!(!cleaned.contains("tg-widget"));
        assert!(!cleaned.contains("tg-card"));
        assert!(cleaned.contains("玩得开心！"));
    }

    #[test]
    fn test_extract_ai_widgets_preserves_unclosed_or_invalid_card() {
        let broken = "Here:\n```tg-card\n{bad json\n```\nEnd.";
        let (cleaned, widgets) = extract_ai_widgets(broken);
        assert!(widgets.is_empty());
        assert!(cleaned.contains("{bad json"));
    }

    #[test]
    fn test_edit_message_widget_data_validation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let res = edit_message_widget_data("dummy_token", 123456, 1, None, None).await;
            match res {
                Err(WidgetError::InvalidParameter(msg)) => {
                    assert!(msg.contains("Either chat_data or public_data"));
                }
                other => panic!("Expected InvalidParameter error, got {other:?}"),
            }

            let huge = serde_json::json!({ "blob": "x".repeat(MAX_WIDGET_DATA_BYTES + 10) });
            let res2 = edit_message_widget_data("dummy_token", 123456, 1, Some(&huge), None).await;
            match res2 {
                Err(WidgetError::InvalidParameter(msg)) => assert!(msg.contains("exceeds")),
                other => panic!("Expected InvalidParameter for oversized chat_data, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_template_escaping() {
        assert_eq!(
            escape_html("<script>alert(\"XSS & 'bad'\")</script>"),
            "&lt;script&gt;alert(&quot;XSS &amp; &#39;bad&#39;&quot;)&lt;/script&gt;"
        );

        let custom = AgentCardData::new(
            "<Special & Title>",
            "\"Subtitle's test\"",
            "Model <v2>",
            AgentStatus::Thinking,
            "Result with <tags> & \"quotes\"",
            vec![AgentCardStep::new(
                "Step <1>",
                "Details with & special > chars",
                false,
            )],
        );

        let html = custom.render_html().unwrap();
        assert!(!html.contains("<Special & Title>"));
        assert!(html.contains("&lt;Special &amp; Title&gt;"));
        assert!(!html.contains("<tags>"));
        assert!(html.contains("&lt;tags&gt;"));
    }

    #[test]
    fn test_send_widget_payload_construction() {
        let payload = SendWidgetPayload {
            chat_id: 123456789,
            html: "<b>Hello Widget</b>",
            width: Some(0),
            height: None,
            caption: Some("Summary"),
            parse_mode: None,
            chat_data: Some("{\"a\":1}"),
            public_data: None,
            protect_content: None,
            allow_queries: None,
            reply_parameters: None,
            reply_markup: None,
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        assert!(json_str.contains("\"chat_id\":123456789"));
        assert!(json_str.contains("\"html\":\"<b>Hello Widget</b>\""));
        assert!(json_str.contains("\"width\":0"));
        assert!(json_str.contains("\"caption\":\"Summary\""));
        assert!(!json_str.contains("public_data"));
    }

    #[test]
    fn test_edit_widget_data_payload_construction() {
        let chat_val = serde_json::json!({ "status": "executing", "progress": 50 });
        let chat_encoded = serialize_widget_data_value("chat_data", Some(&chat_val)).unwrap();
        let payload = EditMessageWidgetDataPayload {
            chat_id: 987654321,
            message_id: 42,
            chat_data: chat_encoded,
            public_data: None,
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        assert!(json_str.contains("\"chat_id\":987654321"));
        assert!(json_str.contains("\"message_id\":42"));
        assert!(
            json_str.contains(
                "\"chat_data\":\"{\\\"progress\\\":50,\\\"status\\\":\\\"executing\\\"}\""
            )
        );
        assert!(!json_str.contains("public_data"));
    }

    #[test]
    fn test_error_types() {
        let err = validate_widget_html("   ").unwrap_err();
        assert!(matches!(err, WidgetError::EmptyPayload));

        let large_html = "x".repeat(MAX_RAW_HTML_BYTES + 1);
        let err = validate_widget_html(&large_html).unwrap_err();
        match err {
            WidgetError::PayloadTooLarge { size, max } => {
                assert_eq!(size, MAX_RAW_HTML_BYTES + 1);
                assert_eq!(max, MAX_RAW_HTML_BYTES);
            }
            _ => panic!("Expected PayloadTooLarge error, got {err:?}"),
        }

        // High-entropy payload under 32 KB raw but > 4 KB zlib-9 compressed
        let mut noisy = String::with_capacity(9000);
        let mut seed: u32 = 0x1234_5678;
        for _ in 0..9000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let ch = (b'A' + ((seed >> 24) % 26) as u8) as char;
            noisy.push(ch);
        }
        let err = validate_widget_html(&noisy).unwrap_err();
        assert!(
            matches!(err, WidgetError::CompressedPayloadTooLarge { .. }),
            "expected CompressedPayloadTooLarge, got {err:?}"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let res = send_widget("dummy_token", -1001234567, "<div>Valid</div>").await;
            match res {
                Err(WidgetError::NonPrivateChat(id)) => assert_eq!(id, -1001234567),
                other => panic!("Expected NonPrivateChat error, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_agent_card_data_serialization_and_fallback() {
        let demo = AgentCardData::demo();
        let serialized = serde_json::to_string(&demo).expect("Serialize AgentCardData failed");
        let deserialized: AgentCardData =
            serde_json::from_str(&serialized).expect("Deserialize AgentCardData failed");
        assert_eq!(demo, deserialized);

        let fallback = demo.render_fallback_html();
        assert!(fallback.contains("<b>agygram Agent</b>"));
        assert!(fallback.contains("Gemini 4 Hydrogen (XHigh)"));
        assert!(fallback.contains("已完成"));
        assert!(fallback.contains("Telegram In-Message HTML Widget"));
    }

    #[test]
    fn test_effective_system_prompt_merges_guide() {
        assert_eq!(build_effective_system_prompt("  "), WIDGET_PROMPT_GUIDE);
        let merged = build_effective_system_prompt("Always respond in Chinese.");
        assert!(merged.starts_with("Always respond in Chinese."));
        assert!(merged.contains("tg-widget"));
        assert!(merged.contains("tg-card"));
    }

    #[test]
    fn test_minifier_handles_escaped_css_quotes_and_js_trailing_comments() {
        let html = r#"<!DOCTYPE html><html><head><style>
        .q::after { content: "a\"b\\"; color: red; }
        </style></head><body><script>
        var a = 1; // comment with unmatched backtick `
        var b = `multi
          line`;
        </script></body></html>"#;

        let min = minify_widget_html(html);
        assert!(min.contains(r#"content:"a\"b\\";color:red"#));
        assert!(min.contains("  line`;"));
    }

    #[test]
    fn test_extract_ai_widgets_with_roots_confines_widget_html_files() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let unique = format!(
                "agygram_widget_test_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let root = std::env::temp_dir().join(&unique);
            let outside = std::env::temp_dir().join(format!("{unique}_outside"));
            tokio::fs::create_dir_all(&root).await.unwrap();
            tokio::fs::create_dir_all(&outside).await.unwrap();

            let good_file = root.join("snake.widget.html");
            let evil_file = outside.join("escape.widget.html");
            tokio::fs::write(&good_file, "<title>Snake</title><button>Play</button>")
                .await
                .unwrap();
            tokio::fs::write(&evil_file, "<title>Evil</title><button>X</button>")
                .await
                .unwrap();

            let md = format!(
                "Saved {} and tried {}",
                good_file.display(),
                evil_file.display()
            );

            // When `allow_prose_widget_files == false`, casual `.widget.html` mentions are ignored.
            let (_cleaned_no_prose, widgets_no_prose) =
                extract_ai_widgets_with_roots(&md, std::slice::from_ref(&root), false).await;
            assert!(widgets_no_prose.is_empty());

            // When `allow_prose_widget_files == true`, confined files load and escapes are blocked.
            let (_cleaned, widgets) =
                extract_ai_widgets_with_roots(&md, std::slice::from_ref(&root), true).await;
            assert_eq!(widgets.len(), 1);
            assert_eq!(widgets[0].title, "Snake");
            assert_eq!(widgets[0].file_name, "snake.widget.html");

            let _ = tokio::fs::remove_dir_all(&root).await;
            let _ = tokio::fs::remove_dir_all(&outside).await;
        });
    }

    #[test]
    fn test_http_transport_errors_never_leak_bot_token() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // Bind an ephemeral loopback port and drop the listener so connection is refused fast.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);

            let secret_token = "987654321:SUPER_SECRET_BOT_TOKEN_LEAK_GUARD";
            let client = Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let api_base = format!("http://127.0.0.1:{port}");

            let err =
                send_widget_with_client(&client, &api_base, secret_token, 123456, "<div>ok</div>")
                    .await
                    .unwrap_err();
            assert!(matches!(err, WidgetError::Http(_)), "got {err:?}");
            assert!(!err.to_string().contains(secret_token));
            assert!(!format!("{err:?}").contains(secret_token));

            let chat_data = serde_json::json!({"ok": true});
            let edit_err = edit_message_widget_data_with_client(
                &client,
                &api_base,
                secret_token,
                123456,
                1,
                Some(&chat_data),
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(edit_err, WidgetError::Http(_)), "got {edit_err:?}");
            assert!(!edit_err.to_string().contains(secret_token));
            assert!(!format!("{edit_err:?}").contains(secret_token));
        });
    }

    #[test]
    fn test_send_widget_unavailable_cache_and_error_classification() {
        clear_send_widget_unavailable_cache();
        let not_allowed = WidgetError::ApiError {
            code: 400,
            description: "Bad Request: INPUT_MEDIA_WIDGET_NOT_ALLOWED".to_string(),
        };
        let other_err = WidgetError::ApiError {
            code: 400,
            description: "Bad Request: chat not found".to_string(),
        };
        assert!(is_send_widget_not_allowed_error(&not_allowed));
        assert!(!is_send_widget_not_allowed_error(&other_err));

        let tok_a = "111:TOKEN_A_CACHE_TEST";
        let tok_b = "222:TOKEN_B_CACHE_TEST";
        assert!(!is_send_widget_cached_unavailable(tok_a));
        mark_send_widget_unavailable(tok_a);
        assert!(is_send_widget_cached_unavailable(tok_a));
        assert!(!is_send_widget_cached_unavailable(tok_b));
        clear_send_widget_unavailable_cache();
    }

    #[test]
    fn test_prompt_requests_widget_or_card_detection() {
        for yes in [
            "给我做一个仪表盘",
            "画一个数据看板",
            "写个扫雷小游戏",
            "来一把 2048",
            "生成一张交互卡片对比 Rust 和 Go",
            "Create a quiz about Linux",
            "做一个复利计算器",
            "帮我做一个widget演示",
            "/card 进度总结",
        ] {
            assert!(
                prompt_requests_widget_or_card(yes),
                "expected widget trigger for: {yes}"
            );
        }
        for no in [
            "帮我修复 src/bot/widget.rs 的一个 bug",
            "请解释一下 Rust 的生命周期",
            "git checkout -- . to discard changes",
            "查询今天上海天气",
            "cargo test 报错了怎么修",
        ] {
            assert!(
                !prompt_requests_widget_or_card(no),
                "unexpected widget trigger for ordinary prompt: {no}"
            );
        }
    }

    #[test]
    fn test_tg_card_json_schema_and_structured_output_builder() {
        let schema: serde_json::Value = serde_json::from_str(TG_CARD_JSON_SCHEMA)
            .expect("TG_CARD_JSON_SCHEMA must be valid JSON");
        assert_eq!(schema["type"], "object");

        let structured = serde_json::json!({
            "kind": "metric",
            "title": "VPS 状态",
            "badge": "OK",
            "badge_tone": "ok",
            "metrics": [
                {"label": "RAM", "value": "892 MB", "delta": "正常", "tone": "ok"}
            ],
            "footer": "agygram"
        });
        let widget = widget_from_structured_card_value(&structured).expect("build structured card");
        assert_eq!(widget.kind, WidgetKind::Card);
        assert_eq!(widget.title, "VPS 状态");
        assert!(widget.is_send_widget_eligible());

        let empty_title = serde_json::json!({ "title": "   " });
        assert!(matches!(
            widget_from_structured_card_value(&empty_title),
            Err(WidgetError::InvalidCardSpec(_))
        ));
    }

    #[test]
    fn test_minifier_preserves_textarea_and_non_ascii_offsets() {
        let html = "<div>  中文标题  </div><textarea>  keep   spaces  </textarea><style> body { color: red; } </style><pre>  code  </pre>";
        let min = minify_widget_html(html);
        assert!(min.contains("<textarea>  keep   spaces  </textarea>"));
        assert!(min.contains("<style>body{color:red}</style>"));
        assert!(min.contains("<pre>  code  </pre>"));
        assert!(min.contains("中文标题"));
    }

    #[test]
    fn test_widget_html_file_caption_preserves_closing_tags_for_long_titles() {
        let mut w = BuiltinGame::Game2048.to_outbound_widget().unwrap();
        w.title = format!("超长标题 <测试> & {}", "字".repeat(1200));
        let cap = format_widget_html_file_caption(&w);
        assert!(cap.chars().count() <= MAX_CAPTION_CHARS);
        assert!(cap.contains("</b>\n<i>"));
        assert!(cap.ends_with("</i>"));
        assert!(!cap.contains("<测试>"));
    }
}
