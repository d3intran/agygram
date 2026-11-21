use crate::text::{decode_html_entity, find_html_tag_end};
use anyhow::{Context, Result};
use bytes::Bytes;
use serde::Serialize;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

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
    #[allow(dead_code)]
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
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(5)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
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
        "🛠️", "📖", "📝", "💻", "🔍", "🌐", "🤖", "⏰", "🎨", "🧭", "❓", "🛡️", "⏱", "🐝", "⚙️",
    ];
    PREFIXES.iter().any(|p| trimmed.starts_with(p))
}

#[inline]
fn is_table_separator_or_box_border(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    let is_box_border = trimmed.chars().all(|c| {
        matches!(
            c,
            '┌' | '─' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘' | ' '
        )
    });
    if is_box_border {
        return true;
    }
    if trimmed.contains('|') && trimmed.contains('-') {
        let is_gfm_sep = trimmed
            .bytes()
            .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t'));
        if is_gfm_sep {
            return true;
        }
    }
    // Horizontal rules (---, ***, ___, ───, ━━━)
    trimmed.len() >= 3
        && (trimmed.bytes().all(|b| b == b'-')
            || trimmed.bytes().all(|b| b == b'*')
            || trimmed.bytes().all(|b| b == b'_')
            || trimmed.chars().all(|c| matches!(c, '─' | '━')))
}

/// Cleans a single non-code line of HTML tags, HTML entities, Markdown `[Title](url)` links,
/// bold/italic/strikethrough/spoiler delimiters, and table cell pipes into natural speech text.
fn clean_speech_line(clean_prefix: &str, out: &mut String) {
    out.clear();
    let trimmed_pipes = clean_prefix.trim_matches(['|', '│']).trim();
    let is_table_row = (clean_prefix.starts_with('|') || clean_prefix.starts_with('│'))
        && (trimmed_pipes.contains('|') || trimmed_pipes.contains('│'));

    let bytes = trimmed_pipes.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Strip Markdown image `!` prefix before `[alt](url)` and turn `[label](url)` into `label`.
        if bytes[i] == b'!' && bytes.get(i + 1) == Some(&b'[') {
            i += 1;
            continue;
        }
        if bytes[i] == b'['
            && let Some(close_bracket_rel) = trimmed_pipes[i + 1..].find(']')
        {
            let label = &trimmed_pipes[i + 1..i + 1 + close_bracket_rel];
            let after_bracket = i + 1 + close_bracket_rel + 1;
            if bytes.get(after_bracket) == Some(&b'(') {
                let mut depth = 1usize;
                let mut end_paren: Option<usize> = None;
                let mut scan = after_bracket + 1;
                while scan < bytes.len() {
                    match bytes[scan] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            if depth == 0 {
                                end_paren = Some(scan);
                                break;
                            }
                        }
                        _ => {}
                    }
                    scan += 1;
                }
                if let Some(close_paren) = end_paren {
                    out.push_str(label.trim());
                    i = close_paren + 1;
                    continue;
                }
            }
        }

        if bytes[i] == b'<' {
            if let Some(end) = find_html_tag_end(&trimmed_pipes[i + 1..]) {
                i = i + 1 + end + 1;
                continue;
            }
            out.push('<');
            i += 1;
            continue;
        }

        if bytes[i] == b'&'
            && let Some((decoded, ent_len)) = decode_html_entity(&trimmed_pipes[i..])
        {
            out.push(decoded);
            i += ent_len;
            continue;
        }

        if matches!(bytes[i], b'`' | b'~') {
            i += 1;
            continue;
        }

        if matches!(bytes[i], b'*' | b'_' | b'|') && bytes.get(i + 1) == Some(&bytes[i]) {
            i += 2;
            continue;
        }
        if bytes[i] == b'*' {
            i += 1;
            continue;
        }

        if is_table_row && (bytes[i] == b'|' || trimmed_pipes[i..].starts_with('│')) {
            let trimmed_out = out.trim_end().len();
            out.truncate(trimmed_out);
            if !out.is_empty() && !out.ends_with(['，', ',', ' ']) {
                out.push('，');
            }
            i += if bytes[i] == b'|' {
                1
            } else {
                '│'.len_utf8()
            };
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
            continue;
        }

        let Some(ch) = trimmed_pipes[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
}

/// High-performance text cleaner for TTS:
/// Strips code blocks, tool headers, tables borders, links' raw URLs, markdown syntax,
/// and valid HTML tags in a single pass, stopping early once the 450-char spoken ceiling is reached.
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

        // Skip URLs, tool logs, table dividers, raw code/JSON lines
        if trimmed.starts_with("http://")
            || trimmed.starts_with("https://")
            || is_tool_log_prefix(trimmed)
            || is_table_separator_or_box_border(trimmed)
            || trimmed.starts_with('{')
            || (trimmed.starts_with('[') && !starts_with_emotion_tag(trimmed))
        {
            continue;
        }

        // Strip Markdown headers/bullets/quote bars
        let clean_prefix = trimmed
            .trim_start_matches(['#', '*', '-', '•', '>', '▎', '▫', '▪'])
            .trim();
        if clean_prefix.is_empty() {
            continue;
        }

        clean_speech_line(clean_prefix, &mut line_buf);

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
        if let Some(open) = result.rfind('[')
            && !result[open..].contains(']')
        {
            result.truncate(open);
        }
    }

    let trimmed_len = result.trim_end().len();
    result.truncate(trimmed_len);
    if total_chars > MAX_SPEECH_CHARS {
        result.push_str("...");
    }
    result
}

/// Returns `true` when `tag` (the inner text of `[...]`) matches a Fish Audio emotion cue
/// such as `calm` or `in a hurry tone` (1..=32 ASCII lowercase/space/hyphen/apostrophe chars).
#[inline]
#[must_use]
pub(crate) fn is_emotion_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, ' ' | '-' | '\''))
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
    is_emotion_tag(&rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_emotion_tag_validation() {
        assert!(is_emotion_tag("calm"));
        assert!(is_emotion_tag("in a hurry tone"));
        assert!(is_emotion_tag("soft-spoken"));
        assert!(is_emotion_tag("don't shout"));
        assert!(!is_emotion_tag(""));
        assert!(!is_emotion_tag("UPPERCASE"));
        assert!(!is_emotion_tag("123"));
        assert!(!is_emotion_tag("{\"tool\":\"x\"}"));
        assert!(!is_emotion_tag(&"a".repeat(33)));
    }

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

    #[test]
    fn test_clean_text_handles_tables_links_and_markdown_styles() {
        let input = "\
## **总结**
这是 **重点**，查看 [文档](https://example.com) &amp; 详情。
| 名称 | 状态 |
|---|---|
| 服务A | 正常 |
---
[calm]一切就绪。";
        let cleaned = clean_text_for_speech(input);
        assert!(cleaned.contains("总结 这是 重点，查看 文档 & 详情。"));
        assert!(cleaned.contains("名称，状态 服务A，正常"));
        assert!(cleaned.contains("[calm]一切就绪。"));
        assert!(!cleaned.contains("https://example.com"));
        assert!(!cleaned.contains("**"));
        assert!(!cleaned.contains("---"));
    }
}
