use anyhow::{Context, Result};
use serde_json::json;
use std::sync::OnceLock;

static TTS_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn get_tts_client() -> &'static reqwest::Client {
    TTS_HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .pool_max_idle_per_host(5)
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

pub async fn generate_speech(
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
) -> Result<Vec<u8>> {
    // Telegram renders a voice bubble only for Ogg/Opus; mp3 arrives as plain audio.
    generate_speech_with_format(text, api_key, reference_id, "opus").await
}

/// QQ accepts mp3/wav/silk for `file_type=3`, so the platform picks the container.
pub async fn generate_speech_with_format(
    text: &str,
    api_key: &str,
    reference_id: Option<&str>,
    format: &str,
) -> Result<Vec<u8>> {
    let client = get_tts_client();
    let url = "https://api.fish.audio/v1/tts";

    let clean_text = clean_text_for_speech(text);
    if clean_text.trim().is_empty() {
        anyhow::bail!("Text for TTS is empty");
    }

    let mut payload = json!({
        "text": clean_text,
        "format": format,
        // Fish Audio treats temperature as the expressiveness dial (0.7 is the default);
        // 0.8 keeps pronunciation stable while giving the read more emotional variance.
        "temperature": 0.8
    });

    if let Some(ref_id) = reference_id {
        if !ref_id.trim().is_empty() {
            payload["reference_id"] = json!(ref_id);
        }
    }

    let res = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .header("model", "s2.1-pro-free")
        .json(&payload)
        .send()
        .await
        .context("Failed to send TTS request to Fish Audio API")?;

    if !res.status().is_success() {
        let err_text = res.text().await.unwrap_or_default();
        anyhow::bail!("Fish Audio API error: {}", err_text);
    }

    let bytes = res.bytes().await.context("Failed to read TTS response bytes")?;
    Ok(bytes.to_vec())
}

/// High-performance text cleaner for TTS:
/// Strips code blocks, tool headers, emojis, markdown prefixes, and HTML tags in a single pass.
#[must_use]
pub fn clean_text_for_speech(text: &str) -> String {
    let mut result = String::with_capacity(text.len().min(512));
    let mut in_code_block = false;
    let mut line_buf = String::with_capacity(128);

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
            || trimmed.starts_with("🛠️")
            || trimmed.starts_with("📖")
            || trimmed.starts_with("📝")
            || trimmed.starts_with("💻")
            || trimmed.starts_with("🔍")
            || trimmed.starts_with("🌐")
            || trimmed.starts_with("🤖")
            || trimmed.starts_with("⏰")
            || trimmed.starts_with("🎨")
            || trimmed.starts_with("🧭")
            || trimmed.starts_with("❓")
            || trimmed.starts_with("🛡️")
            || trimmed.starts_with("⏱")
            || trimmed.starts_with('{')
            || (trimmed.starts_with('[') && !starts_with_emotion_tag(trimmed))
        {
            continue;
        }

        // Strip Markdown headers/bullets
        let clean_prefix = trimmed
            .trim_start_matches(['#', '*', '-', '•', '>'])
            .trim();

        if clean_prefix.is_empty() {
            continue;
        }

        // Strip HTML tags and markdown code backticks/tildes in single pass
        let mut in_tag = false;
        line_buf.clear();
        for ch in clean_prefix.chars() {
            if ch == '<' {
                in_tag = true;
            } else if ch == '>' {
                in_tag = false;
            } else if !in_tag && ch != '`' && ch != '~' {
                line_buf.push(ch);
            }
        }

        let cleaned = line_buf.trim();
        if !cleaned.is_empty() {
            result.push_str(cleaned);
            result.push(' ');
        }
    }

    // Limit to 450 characters (~1.5 minutes of spoken text) for ultra-fast TTS generation
    let mut char_count = 0;
    let mut byte_limit = result.len();
    for (idx, _) in result.char_indices() {
        char_count += 1;
        if char_count > 450 {
            byte_limit = idx;
            break;
        }
    }

    if char_count > 450 {
        result.truncate(byte_limit);
        // Do not hand the engine half an emotion cue such as `[excited`; drop the dangling tag.
        if let Some(open) = result.rfind('[') {
            if !result[open..].contains(']') {
                result.truncate(open);
            }
        }
        let trimmed_len = result.trim_end().len();
        result.truncate(trimmed_len);
        result.push_str("...");
        result
    } else {
        let trimmed_start = result.len() - result.trim_start().len();
        if trimmed_start > 0 {
            result.drain(..trimmed_start);
        }
        let trimmed_len = result.trim_end().len();
        result.truncate(trimmed_len);
        result
    }
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

/// Fish Audio voice ids are opaque tokens; accept the documented shape and avoid handing the
/// TTS request anything that looks like a path or JSON fragment.
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
        let input = "[calm]先说结论。\n[very excited]这个结果太棒了！\n[{\"tool\": \"x\"}]\n[1, 2, 3]";
        let cleaned = clean_text_for_speech(input);
        assert_eq!(cleaned, "[calm]先说结论。 [very excited]这个结果太棒了！");
    }
}
