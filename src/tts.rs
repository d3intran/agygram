use crate::text::find_html_tag_end;
use anyhow::{Context, Result};
use bytes::Bytes;
use serde::Serialize;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

pub use crate::models::is_valid_voice_reference;

const FISH_TTS_URL: &str = "https://api.fish.audio/v1/tts";
const FISH_TTS_MODEL: &str = "s2.1-pro-free";
const TTS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SPEECH_CHARS: usize = 450;

static TTS_HTTP_CLIENT_STATE: OnceLock<RwLock<(Option<String>, reqwest::Client)>> = OnceLock::new();

/// Audio container formats supported by Fish Audio TTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsFormat {
    Opus,
    Mp3,
    Wav,
}

impl TtsFormat {
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Opus => "opus",
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
        }
    }
}

#[derive(Serialize)]
struct TtsRequest<'a> {
    text: &'a str,
    format: &'a str,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_id: Option<&'a str>,
}

fn build_tts_client(proxy_url: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(5)
        .tcp_keepalive(Duration::from_secs(60))
        .connect_timeout(TTS_CONNECT_TIMEOUT)
        .timeout(TTS_REQUEST_TIMEOUT);

    if let Some(proxy) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                builder = builder.proxy(p);
            }
            Err(err) => {
                tracing::warn!("Invalid TTS proxy URL {:?}: {err}", proxy);
            }
        }
    }

    builder.build().unwrap_or_else(|err| {
        tracing::warn!("Failed to build TTS HTTP client: {err}; using default client");
        reqwest::Client::new()
    })
}

fn get_tts_client() -> reqwest::Client {
    let state = TTS_HTTP_CLIENT_STATE.get_or_init(|| RwLock::new((None, build_tts_client(None))));
    match state.read() {
        Ok(guard) => guard.1.clone(),
        Err(poisoned) => poisoned.into_inner().1.clone(),
    }
}

/// Configures an optional proxy URL for Fish Audio TTS HTTP requests.
pub fn configure_proxy(proxy_url: Option<&str>) {
    let normalized = proxy_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let state = TTS_HTTP_CLIENT_STATE
        .get_or_init(|| RwLock::new((normalized.clone(), build_tts_client(normalized.as_deref()))));
    let mut guard = match state.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.0 != normalized {
        guard.1 = build_tts_client(normalized.as_deref());
        guard.0 = normalized;
    }
}

/// Synthesizes Telegram Ogg/Opus speech audio as `Vec<u8>`.
///
/// # Errors
/// Returns an error if the cleaned text is empty or the Fish Audio request fails.
pub async fn generate_speech(
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
) -> Result<Vec<u8>> {
    // Telegram renders a voice bubble only for Ogg/Opus; mp3 arrives as plain audio.
    generate_speech_with_format(text, api_key, reference_id, TtsFormat::Opus.as_str()).await
}

/// Synthesizes speech in the requested container format as `Vec<u8>`.
///
/// # Errors
/// Returns an error if the cleaned text is empty or the Fish Audio request fails.
pub async fn generate_speech_with_format(
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
    format: &str,
) -> Result<Vec<u8>> {
    generate_speech_bytes(text, api_key, reference_id, format)
        .await
        .map(|b| b.to_vec())
}

/// Zero-copy Fish Audio TTS helper returning [`bytes::Bytes`].
///
/// # Errors
/// Returns an error if the cleaned text is empty or the Fish Audio request fails.
pub async fn generate_speech_bytes(
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
    format: &str,
) -> Result<Bytes> {
    let client = get_tts_client();
    let clean_text = clean_text_for_speech(text);
    if clean_text.trim().is_empty() {
        anyhow::bail!("Text for TTS is empty");
    }

    let payload = TtsRequest {
        text: &clean_text,
        format,
        // Fish Audio treats temperature as the expressiveness dial (0.7 is the default);
        // 0.8 keeps pronunciation stable while giving the read more emotional variance.
        temperature: 0.8,
        reference_id: reference_id.map(str::trim).filter(|s| !s.is_empty()),
    };

    let res = client
        .post(FISH_TTS_URL)
        .bearer_auth(api_key)
        .header("model", FISH_TTS_MODEL)
        .json(&payload)
        .send()
        .await
        .context("Failed to send TTS request to Fish Audio API")?;

    let status = res.status();
    if !status.is_success() {
        let err_text = res.text().await.unwrap_or_default();
        anyhow::bail!("Fish Audio API error ({}): {}", status, err_text);
    }

    res.bytes()
        .await
        .context("Failed to read TTS response bytes")
}

#[inline]
fn is_tool_log_prefix(trimmed: &str) -> bool {
    if trimmed.is_ascii() {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "🛠️", "📖", "📝", "💻", "🔍", "🌐", "🤖", "⏰", "🎨", "🧭", "❓", "🛡️", "⏱",
    ];
    PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

/// High-performance text cleaner for TTS:
/// Strips code blocks, tool headers, emojis, markdown prefixes, and valid HTML tags in a single
/// pass, stopping early once the 450-char spoken ceiling is reached.
#[must_use]
pub fn clean_text_for_speech(text: &str) -> String {
    let mut result = String::with_capacity(text.len().min(512));
    let mut in_code_block = false;
    let mut line_buf = String::with_capacity(128);
    let mut total_chars = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }

        // Skip URLs, tool logs, raw code lines, blockquote headers
        if trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || is_tool_log_prefix(trimmed)
            || trimmed.starts_with('{')
            || (trimmed.starts_with('[') && !starts_with_emotion_tag(trimmed))
        {
            continue;
        }

        // Strip Markdown headers/bullets
        let clean_prefix = trimmed.trim_start_matches(['#', '*', '-', '•', '>']).trim();
        if clean_prefix.is_empty() {
            continue;
        }

        // Strip HTML tags (preserving unmatched `<`) and markdown backticks/tildes in a single pass
        line_buf.clear();
        let bytes = clean_prefix.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let Some(rel) = clean_prefix[i..].find(['<', '`', '~']) else {
                line_buf.push_str(&clean_prefix[i..]);
                break;
            };
            if rel > 0 {
                line_buf.push_str(&clean_prefix[i..i + rel]);
                i += rel;
            }
            match bytes[i] {
                b'<' => {
                    if let Some(end) = find_html_tag_end(&clean_prefix[i + 1..]) {
                        i = i + 1 + end + 1;
                    } else {
                        line_buf.push('<');
                        i += 1;
                    }
                }
                b'`' | b'~' => {
                    i += 1;
                }
                _ => break,
            }
        }

        let cleaned = line_buf.trim();
        if !cleaned.is_empty() {
            if !result.is_empty() {
                result.push(' ');
                total_chars += 1;
            }
            result.push_str(cleaned);
            total_chars += cleaned.chars().count();
            if total_chars > MAX_SPEECH_CHARS {
                break;
            }
        }
    }

    if total_chars > MAX_SPEECH_CHARS {
        if let Some((cut, _)) = result.char_indices().nth(MAX_SPEECH_CHARS) {
            result.truncate(cut);
        }
        // Do not hand the engine half an emotion cue such as `[excited`; drop the dangling tag.
        if let Some(open) = result.rfind('[') {
            if !result[open..].contains(']') {
                result.truncate(open);
            }
        }
    }

    let trimmed_len = result.trim_end().len();
    result.truncate(trimmed_len);
    if total_chars > MAX_SPEECH_CHARS {
        result.push_str("...");
    }
    result
}

/// Fish Audio emotion cues (`[calm]`, `[in a hurry tone]`) start lines with `[` just like raw
/// tool JSON does, so only treat a short lowercase bracket as a cue instead of dropping the line.
fn starts_with_emotion_tag(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('[') else {
        return false;
    };
    let Some(end) = rest.find(']') else {
        return false;
    };
    let tag = &rest[..end];
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, ' ' | '-' | '\''))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_text_for_speech_basic() {
        let input = "# 标题\n这是一个 <b>加粗</b> 测试，以及一段 `代码`。\n```rust\nfn main() {}\n```\n结束。";
        let cleaned = clean_text_for_speech(input);
        assert_eq!(cleaned, "标题 这是一个 加粗 测试，以及一段 代码。 结束。");
    }

    #[test]
    fn test_clean_text_filters_tool_and_urls() {
        let input = "🛠️ Executing: view_file\nhttps://example.com\n正常回复内容。";
        let cleaned = clean_text_for_speech(input);
        assert_eq!(cleaned, "正常回复内容。");
    }

    #[test]
    fn test_clean_text_keeps_emotion_cues_but_drops_json() {
        let input =
            "[calm]先说结论。\n[very excited]这个结果太棒了！\n[{\"tool\": \"x\"}]\n[1, 2, 3]";
        let cleaned = clean_text_for_speech(input);
        assert_eq!(cleaned, "[calm]先说结论。 [very excited]这个结果太棒了！");
    }

    #[test]
    fn test_clean_text_preserves_unmatched_less_than() {
        let input = "当 x < 5 时返回错误。\n如果 a<b && c>d 也继续朗读。";
        let cleaned = clean_text_for_speech(input);
        assert_eq!(
            cleaned,
            "当 x < 5 时返回错误。 如果 a<b && c>d 也继续朗读。"
        );
    }

    #[test]
    fn test_clean_text_truncates_at_450_chars_and_drops_dangling_cue() {
        let line = "一二三四五".repeat(89); // 445 chars; `[excited]` crosses char 450
        let input = format!("{line}\n[excited] 接下来的内容将被截断。\n后面还有更多行。");
        let cleaned = clean_text_for_speech(&input);
        assert!(cleaned.ends_with("..."));
        assert!(!cleaned.contains("[excited"));
        assert!(!cleaned.contains("[exc"));
        assert!(cleaned.chars().count() <= 453);
    }

    #[test]
    fn test_tts_request_serialization_skips_empty_reference() {
        let req = TtsRequest {
            text: "hello",
            format: "opus",
            temperature: 0.8,
            reference_id: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("reference_id"));

        let req_with_ref = TtsRequest {
            text: "hello",
            format: "mp3",
            temperature: 0.8,
            reference_id: Some("voice-id-123"),
        };
        let json_ref = serde_json::to_string(&req_with_ref).unwrap();
        assert!(json_ref.contains("\"reference_id\":\"voice-id-123\""));
    }
}
