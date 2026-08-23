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
    let client = get_tts_client();
    let url = "https://api.fish.audio/v1/tts";

    let clean_text = clean_text_for_speech(text);
    if clean_text.trim().is_empty() {
        anyhow::bail!("Text for TTS is empty");
    }

    let mut payload = json!({
        "text": clean_text,
        "format": "mp3"
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
pub fn clean_text_for_speech(text: &str) -> String {
    let mut result = String::with_capacity(text.len().min(512));
    let mut in_code_block = false;

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
            || trimmed.starts_with('[')
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
        let mut line_buf = String::with_capacity(clean_prefix.len());
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
        let trimmed_end = result.trim_end();
        format!("{}...", trimmed_end)
    } else {
        result.trim().to_string()
    }
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
}
