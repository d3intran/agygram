use std::fmt::Write;

pub struct TelegramFormatter;

const TELEGRAM_CONTINUED_PREFIX: &str = "<i>(continued)</i>\n\n";
const TELEGRAM_CONTINUES_SUFFIX: &str = "\n\n<i>(continues...)</i>";

impl TelegramFormatter {
    /// Format raw agy output into valid Telegram HTML format.
    pub fn to_telegram_html(raw: &str) -> String {
        let clean = Self::strip_ansi_codes(raw);
        Self::markdown_to_telegram_html(&clean)
    }

    /// Format tool usage event into Telegram HTML status card with specialized icons & parameters.
    pub fn format_tool_call(tool_name: &str, summary: &str) -> String {
        let (icon, display_title) = match tool_name {
            "view_file" | "read_resource" => ("📖", "View File"),
            "write_to_file" | "replace_file_content" | "multi_replace_file_content" | "sed_file" => ("📝", "Edit File"),
            "run_command" | "send_command_input" | "command_status" => ("💻", "Run Command"),
            "grep_search" | "find_by_name" => ("🔍", "Search Code"),
            "search_web" | "read_url_content" => ("🌐", "Web Search"),
            "invoke_subagent" | "define_subagent" | "manage_subagents" => ("🤖", "Subagent"),
            "schedule" | "manage_task" => ("⏰", "Task Manager"),
            "generate_image" => ("🎨", "Generate Image"),
            "read_browser_page" | "open_browser_url" | "click_browser_pixel" => ("🧭", "Browser"),
            "ask_question" => ("❓", "Question"),
            "ask_permission" | "ask_custom_permission" => ("🛡️", "Permission"),
            _ => ("🛠️", tool_name),
        };

        let clean_param = Self::extract_tool_param_summary(summary);
        if !clean_param.is_empty() {
            let preview = if clean_param.chars().count() > 90 {
                let mut cut = clean_param.len();
                let mut count = 0;
                for (idx, _) in clean_param.char_indices() {
                    count += 1;
                    if count > 90 {
                        cut = idx;
                        break;
                    }
                }
                format!("{}...", clean_param[..cut].trim_end())
            } else {
                clean_param
            };
            format!(
                "{} <b>{}:</b> <code>{}</code>\n\n",
                icon,
                Self::escape_html(display_title),
                Self::escape_html(&preview)
            )
        } else {
            format!(
                "{} <b>Executing:</b> <code>{}</code>\n\n",
                icon,
                Self::escape_html(tool_name)
            )
        }
    }

    /// Extract readable parameter summary from tool input JSON
    fn extract_tool_param_summary(raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return String::new();
        }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(cmd) = v.get("CommandLine").and_then(|x| x.as_str()) {
                return cmd.to_string();
            }
            if let Some(target) = v.get("TargetFile").or_else(|| v.get("AbsolutePath")).and_then(|x| x.as_str()) {
                return target.to_string();
            }
            if let Some(subagents) = v.get("Subagents").and_then(|x| x.as_array()) {
                if let Some(first) = subagents.first() {
                    let role = first.get("Role").and_then(|r| r.as_str()).unwrap_or("Subagent");
                    let prompt = first.get("Prompt").and_then(|p| p.as_str()).unwrap_or("");
                    return format!("[{}] {}", role, prompt);
                }
            }
            if let Some(role) = v.get("Role").and_then(|x| x.as_str()) {
                let prompt = v.get("Prompt").and_then(|p| p.as_str()).unwrap_or("");
                return format!("[{}] {}", role, prompt);
            }
            if let Some(q) = v.get("Query").or_else(|| v.get("query")).or_else(|| v.get("Pattern")).and_then(|x| x.as_str()) {
                return q.to_string();
            }
            if let Some(url) = v.get("Url").or_else(|| v.get("url")).and_then(|x| x.as_str()) {
                return url.to_string();
            }
            if let Some(prompt) = v.get("Prompt").or_else(|| v.get("prompt")).and_then(|x| x.as_str()) {
                return prompt.to_string();
            }
        }

        trimmed.to_string()
    }

    /// Format telemetry badge for the footer of completed responses
    pub fn format_telemetry_badge(usage: Option<&crate::executor::TokenUsage>, duration_secs: Option<f64>) -> String {
        let mut parts = Vec::new();
        if let Some(d) = duration_secs {
            parts.push(format!("⏱ <i>{:.1}s</i>", d));
        }
        if let Some(u) = usage {
            let total = u.total_tokens.max(u.input_tokens + u.output_tokens);
            if total > 0 {
                if u.thinking_tokens > 0 {
                    parts.push(format!(
                        "🧠 <i>{} tokens (Prompt: {}, Thinking: {}, Output: {})</i>",
                        total, u.input_tokens, u.thinking_tokens, u.output_tokens
                    ));
                } else {
                    parts.push(format!(
                        "🧠 <i>{} tokens (Prompt: {}, Output: {})</i>",
                        total, u.input_tokens, u.output_tokens
                    ));
                }
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("\n\n───────────────\n{}", parts.join(" • "))
        }
    }

    /// Balances unclosed HTML tags during real-time streaming drafts to prevent 400 bad entity errors
    pub fn balance_streaming_html(html: &str) -> String {
        let mut balanced = html.to_string();
        let open_pre = balanced.matches("<pre>").count() + balanced.matches("<pre ").count();
        let close_pre = balanced.matches("</pre>").count();
        if open_pre > close_pre {
            balanced.push_str("</code></pre>");
        }

        let open_quote = balanced.matches("<blockquote>").count() + balanced.matches("<blockquote ").count();
        let close_quote = balanced.matches("</blockquote>").count();
        if open_quote > close_quote {
            balanced.push_str("</blockquote>");
        }

        let open_b = balanced.matches("<b>").count();
        let close_b = balanced.matches("</b>").count();
        if open_b > close_b {
            balanced.push_str("</b>");
        }

        let open_i = balanced.matches("<i>").count();
        let close_i = balanced.matches("</i>").count();
        if open_i > close_i {
            balanced.push_str("</i>");
        }

        balanced
    }

    /// Format thinking step into Telegram HTML snippet with expandable blockquote.
    pub fn format_thinking(thinking_text: &str) -> String {
        let trimmed = thinking_text.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let preview = if trimmed.chars().count() > 350 {
            let mut cut = trimmed.len();
            let mut count = 0;
            for (idx, _) in trimmed.char_indices() {
                count += 1;
                if count > 350 {
                    cut = idx;
                    break;
                }
            }
            format!("{}...", trimmed[..cut].trim_end())
        } else {
            trimmed.to_string()
        };
        format!(
            "<blockquote expandable>🧠 <b>Thinking:</b>\n<i>{}</i></blockquote>\n\n",
            Self::escape_html(&preview)
        )
    }

    /// Strip ANSI escape sequences (\x1b[...m)
    pub fn strip_ansi_codes(input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut in_escape = false;

        for line in input.lines() {
            let trimmed_line = line.trim_start_matches(|ch: char| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(ch) || ch.is_whitespace());
            for c in trimmed_line.chars() {
                if c == '\x1B' {
                    in_escape = true;
                } else if in_escape {
                    if c.is_ascii_alphabetic() {
                        in_escape = false;
                    }
                } else {
                    result.push(c);
                }
            }
            result.push('\n');
        }
        result.trim_end().to_string()
    }

    /// Markdown to Telegram HTML converter:
    /// Supports code blocks with language highlighting, headers, blockquotes, bullets, tables,
    /// horizontal rules, and snake_case-safe inline markdown.
    fn markdown_to_telegram_html(input: &str) -> String {
        let mut output = String::with_capacity(input.len() + 128);
        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_buf = String::new();
        let mut table_buf: Vec<String> = Vec::new();

        let flush_table = |buf: &mut Vec<String>, out: &mut String| {
            if buf.is_empty() {
                return;
            }
            if buf.len() >= 2 && buf[1].contains("---") {
                // Render as clean monospace ASCII table
                let mut rows: Vec<Vec<String>> = Vec::new();
                for (idx, line) in buf.iter().enumerate() {
                    if idx == 1 {
                        continue; // skip separator row
                    }
                    let cells: Vec<String> = line
                        .trim_matches('|')
                        .split('|')
                        .map(|c| c.trim().to_string())
                        .collect();
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                }

                if !rows.is_empty() {
                    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                    let mut col_widths = vec![0; col_count];
                    for r in &rows {
                        for (c_idx, cell) in r.iter().enumerate() {
                            if c_idx < col_count {
                                col_widths[c_idx] = col_widths[c_idx].max(cell.chars().count().min(25));
                            }
                        }
                    }

                    let mut table_str = String::new();
                    for (r_idx, r) in rows.iter().enumerate() {
                        let mut row_line = String::new();
                        for (c_idx, cell) in r.iter().enumerate() {
                            let width = col_widths.get(c_idx).copied().unwrap_or(0);
                            let cell_chars: String = cell.chars().take(25).collect();
                            let char_len = cell_chars.chars().count();
                            let pad = width.saturating_sub(char_len);
                            row_line.push_str(&cell_chars);
                            for _ in 0..pad {
                                row_line.push(' ');
                            }
                            if c_idx < col_count - 1 {
                                row_line.push_str(" │ ");
                            }
                        }
                        table_str.push_str(&row_line);
                        table_str.push('\n');
                        if r_idx == 0 {
                            let sep: String = col_widths
                                .iter()
                                .map(|w| "─".repeat(*w))
                                .collect::<Vec<_>>()
                                .join("─┼─");
                            table_str.push_str(&sep);
                            table_str.push('\n');
                        }
                    }
                    let escaped = Self::escape_html(table_str.trim_end());
                    let _ = writeln!(out, "<pre><code>{escaped}</code></pre>");
                }
            } else {
                for line in buf.iter() {
                    out.push_str(&Self::format_inline_markdown(line));
                    out.push('\n');
                }
            }
            buf.clear();
        };

        for line in input.lines() {
            let trimmed = line.trim();

            // Table line detection
            if !in_code_block && trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 2 {
                table_buf.push(trimmed.to_string());
                continue;
            } else if !table_buf.is_empty() {
                flush_table(&mut table_buf, &mut output);
            }

            if trimmed.starts_with("```") {
                if in_code_block {
                    // Close code block
                    in_code_block = false;
                    let escaped = Self::escape_html(code_buf.trim_end_matches('\n'));
                    if code_lang.is_empty() {
                        let _ = writeln!(output, "<pre><code>{escaped}</code></pre>");
                    } else {
                        let escaped_lang = Self::escape_html(&code_lang);
                        let _ = writeln!(output, "<pre><code class=\"language-{escaped_lang}\">{escaped}</code></pre>");
                    }
                    code_buf.clear();
                    code_lang.clear();
                } else {
                    // Open code block
                    in_code_block = true;
                    code_lang = trimmed.trim_start_matches('`').trim().to_string();
                    code_buf.clear();
                }
            } else if in_code_block {
                code_buf.push_str(line);
                code_buf.push('\n');
            } else {
                // Check headers (# Header ... ###### Header)
                let hash_count = trimmed.chars().take_while(|c| *c == '#').count();
                if (1..=6).contains(&hash_count) && trimmed[hash_count..].starts_with(' ') {
                    let header_text = trimmed[hash_count..].trim();
                    let formatted_header = Self::format_inline_markdown(header_text);
                    let _ = writeln!(output, "<b>{}</b>", formatted_header);
                    continue;
                }

                // Check blockquote (> text)
                if let Some(quote_content) = trimmed.strip_prefix("> ") {
                    let formatted_quote = Self::format_inline_markdown(quote_content);
                    let _ = writeln!(output, "<blockquote>{}</blockquote>", formatted_quote);
                    continue;
                }

                // Check horizontal rule (--- or *** or ___)
                if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                    output.push_str("───────────────\n");
                    continue;
                }

                // Process line with bullet point check (* item or - item or + item)
                let processed_line = if let Some(bullet_content) = trimmed.strip_prefix("* ").or_else(|| trimmed.strip_prefix("- ")).or_else(|| trimmed.strip_prefix("+ ")) {
                    format!("• {}", Self::format_inline_markdown(bullet_content))
                } else {
                    Self::format_inline_markdown(line)
                };

                output.push_str(&processed_line);
                output.push('\n');
            }
        }

        if !table_buf.is_empty() {
            flush_table(&mut table_buf, &mut output);
        }

        if in_code_block && !code_buf.is_empty() {
            let escaped = Self::escape_html(code_buf.trim_end_matches('\n'));
            if code_lang.is_empty() {
                let _ = writeln!(output, "<pre><code>{escaped}</code></pre>");
            } else {
                let escaped_lang = Self::escape_html(&code_lang);
                let _ = writeln!(output, "<pre><code class=\"language-{escaped_lang}\">{escaped}</code></pre>");
            }
        }

        output.trim_end_matches('\n').to_string()
    }

    /// Format inline bold (**text** or __text__), italic (*text* or _text_), inline code (`code`), links ([text](url)), strikethrough (~~text~~)
    fn format_inline_markdown(line: &str) -> String {
        let mut line_out = String::with_capacity(line.len() + 16);
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            // Bold with **: **text**
            if i + 1 < len && bytes[i] == b'*' && bytes[i + 1] == b'*' {
                if let Some(end) = line[i + 2..].find("**") {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<b>{inner}</b>");
                    i += 4 + end;
                    continue;
                }
            }

            // Bold with __: __text__
            if i + 1 < len && bytes[i] == b'_' && bytes[i + 1] == b'_' {
                if let Some(end) = line[i + 2..].find("__") {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<b>{inner}</b>");
                    i += 4 + end;
                    continue;
                }
            }

            // Italic with *: *text*
            if bytes[i] == b'*'
                && (i == 0 || bytes[i - 1] != b'*')
                && i + 1 < len
                && bytes[i + 1] != b'*'
                && bytes[i + 1] != b' '
            {
                if let Some(end) = line[i + 1..].find('*') {
                    if end > 0 && (i + 1 + end + 1 >= len || bytes[i + 1 + end + 1] != b'*') {
                        let inner = Self::escape_html(&line[i + 1..i + 1 + end]);
                        let _ = write!(line_out, "<i>{inner}</i>");
                        i += 2 + end;
                        continue;
                    }
                }
            }

            // Italic with _: _text_ (strictly check word boundaries)
            if bytes[i] == b'_'
                && (i == 0 || !Self::is_identifier_char(bytes[i - 1]))
                && i + 1 < len
                && bytes[i + 1] != b'_'
                && bytes[i + 1] != b' '
            {
                if let Some(end) = line[i + 1..].find('_') {
                    if end > 0 {
                        let after_idx = i + 1 + end + 1;
                        if after_idx >= len || !Self::is_identifier_char(bytes[after_idx]) {
                            let inner = Self::escape_html(&line[i + 1..i + 1 + end]);
                            let _ = write!(line_out, "<i>{inner}</i>");
                            i += 2 + end;
                            continue;
                        }
                    }
                }
            }

            // Inline code: `code`
            if bytes[i] == b'`' && (i == 0 || bytes[i - 1] != b'`') {
                if let Some(end) = line[i + 1..].find('`') {
                    let inner = Self::escape_html(&line[i + 1..i + 1 + end]);
                    let _ = write!(line_out, "<code>{inner}</code>");
                    i += 2 + end;
                    continue;
                }
            }

            // Markdown link: [text](url)
            if bytes[i] == b'[' {
                if let Some(bracket_end) = line[i + 1..].find(']') {
                    let text_part = &line[i + 1..i + 1 + bracket_end];
                    let after_bracket = i + 1 + bracket_end + 1;
                    if after_bracket < len && bytes[after_bracket] == b'(' {
                        if let Some(paren_end) = line[after_bracket + 1..].find(')') {
                            let url = &line[after_bracket + 1..after_bracket + 1 + paren_end];
                            if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("tg://") || url.starts_with("file://") {
                                let text_html = Self::escape_html(text_part);
                                let url_html = Self::escape_html(url);
                                let _ = write!(line_out, "<a href=\"{url_html}\">{text_html}</a>");
                                i = after_bracket + 1 + paren_end + 1;
                                continue;
                            }
                        }
                    }
                }
            }

            // Strikethrough: ~~text~~
            if i + 1 < len && bytes[i] == b'~' && bytes[i + 1] == b'~' {
                if let Some(end) = line[i + 2..].find("~~") {
                    let inner = Self::escape_html(&line[i + 2..i + 2 + end]);
                    let _ = write!(line_out, "<s>{inner}</s>");
                    i += 4 + end;
                    continue;
                }
            }

            // Default HTML escaping
            let ch = line[i..].chars().next().unwrap();
            match ch {
                '<' => line_out.push_str("&lt;"),
                '>' => line_out.push_str("&gt;"),
                '&' => line_out.push_str("&amp;"),
                '"' => line_out.push_str("&quot;"),
                '\'' => line_out.push_str("&#39;"),
                _ => line_out.push(ch),
            }
            i += ch.len_utf8();
        }

        line_out
    }

    #[inline]
    fn is_identifier_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Escape HTML entities & < > " ' with zero allocations on clean strings
    pub fn escape_html(s: &str) -> std::borrow::Cow<'_, str> {
        if !s.bytes().any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\'')) {
            return std::borrow::Cow::Borrowed(s);
        }
        let mut escaped = String::with_capacity(s.len() + 16);
        for c in s.chars() {
            match c {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                '"' => escaped.push_str("&quot;"),
                '\'' => escaped.push_str("&#39;"),
                _ => escaped.push(c),
            }
        }
        std::borrow::Cow::Owned(escaped)
    }

    /// HTML message chunker:
    /// Splitting at line boundaries while closing/reopening `<pre><code>` and `<blockquote>`
    /// and adding `(continues...)` & `(continued)`.
    /// Guarantees no single chunk exceeds max_chunk_size with zero redundant character vector allocations.
    pub fn split_html_chunks(html: &str, max_chunk_size: usize) -> Vec<String> {
        if html.len() <= max_chunk_size {
            return vec![html.to_string()];
        }

        let target_size = if max_chunk_size > 150 { max_chunk_size - 100 } else { max_chunk_size };
        let mut raw_chunks = Vec::new();
        let mut current_chunk = String::with_capacity(max_chunk_size);
        let mut in_pre_tag = false;
        let mut in_quote_tag = false;

        for line in html.lines() {
            let line_len = line.len() + 1;

            if line.contains("<pre") {
                in_pre_tag = true;
            }
            if line.contains("<blockquote") {
                in_quote_tag = true;
            }

            if line_len > target_size {
                let mut start_idx = 0;
                while start_idx < line.len() {
                    let mut end_idx = (start_idx + target_size).min(line.len());
                    // Align to UTF-8 character boundary
                    while end_idx > start_idx && !line.is_char_boundary(end_idx) {
                        end_idx -= 1;
                    }
                    if end_idx == start_idx {
                        end_idx = (start_idx + 1..=line.len())
                            .find(|&i| line.is_char_boundary(i))
                            .unwrap_or(line.len());
                    }

                    let sub_str = &line[start_idx..end_idx];
                    if current_chunk.len() + sub_str.len() > target_size {
                        if in_pre_tag {
                            current_chunk.push_str("</code></pre>");
                            raw_chunks.push(std::mem::take(&mut current_chunk));
                            current_chunk.push_str("<pre><code>");
                        } else if in_quote_tag {
                            current_chunk.push_str("</blockquote>");
                            raw_chunks.push(std::mem::take(&mut current_chunk));
                            current_chunk.push_str("<blockquote>");
                        } else {
                            raw_chunks.push(std::mem::take(&mut current_chunk));
                        }
                    }
                    current_chunk.push_str(sub_str);
                    start_idx = end_idx;
                }
                current_chunk.push('\n');
            } else {
                if current_chunk.len() + line_len > target_size {
                    if in_pre_tag {
                        current_chunk.push_str("</code></pre>");
                        raw_chunks.push(std::mem::take(&mut current_chunk));
                        current_chunk.push_str("<pre><code>");
                    } else if in_quote_tag {
                        current_chunk.push_str("</blockquote>");
                        raw_chunks.push(std::mem::take(&mut current_chunk));
                        current_chunk.push_str("<blockquote>");
                    } else {
                        raw_chunks.push(std::mem::take(&mut current_chunk));
                    }
                }
                current_chunk.push_str(line);
                current_chunk.push('\n');
            }

            if line.contains("</pre>") {
                in_pre_tag = false;
            }
            if line.contains("</blockquote>") {
                in_quote_tag = false;
            }
        }

        if !current_chunk.trim().is_empty() {
            raw_chunks.push(current_chunk);
        }

        let total = raw_chunks.len();
        let mut final_chunks = Vec::with_capacity(total);

        for (i, chunk) in raw_chunks.into_iter().enumerate() {
            let mut formatted = String::new();
            if i > 0 {
                formatted.push_str(TELEGRAM_CONTINUED_PREFIX);
            }
            formatted.push_str(&chunk);
            if i < total - 1 {
                formatted.push_str(TELEGRAM_CONTINUES_SUFFIX);
            }
            final_chunks.push(formatted);
        }

        final_chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_to_telegram_html() {
        let md = "## 标题\n这是 **加粗** 和 *斜体* 以及 `代码` 与 ~~删除线~~。\n> 引用内容\n* 列表项";
        let html = TelegramFormatter::to_telegram_html(md);
        assert!(html.contains("<b>标题</b>"));
        assert!(html.contains("<b>加粗</b>"));
        assert!(html.contains("<i>斜体</i>"));
        assert!(html.contains("<code>代码</code>"));
        assert!(html.contains("<s>删除线</s>"));
        assert!(html.contains("<blockquote>引用内容</blockquote>"));
        assert!(html.contains("• 列表项"));
    }

    #[test]
    fn test_split_html_chunks_small() {
        let html = "<b>简短消息</b>";
        let chunks = TelegramFormatter::split_html_chunks(html, 1000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], html);
    }

    #[test]
    fn test_balance_streaming_html() {
        let open_pre = "<pre><code>fn main() {";
        let balanced = TelegramFormatter::balance_streaming_html(open_pre);
        assert!(balanced.ends_with("</code></pre>"));

        let open_bold = "<b>正在生成中...";
        let balanced_bold = TelegramFormatter::balance_streaming_html(open_bold);
        assert!(balanced_bold.ends_with("</b>"));
    }

    #[test]
    fn test_table_formatting() {
        let table_md = "| Model | Speed |\n|---|---|\n| Flash | Fast |\n| Pro | Deep |";
        let html = TelegramFormatter::to_telegram_html(table_md);
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("Flash"));
        assert!(html.contains("Fast"));
    }

    #[test]
    fn test_telemetry_badge() {
        let usage = crate::executor::TokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            thinking_tokens: 50,
            cache_read_tokens: 0,
            total_tokens: 350,
        };
        let badge = TelegramFormatter::format_telemetry_badge(Some(&usage), Some(2.5));
        assert!(badge.contains("2.5s"));
        assert!(badge.contains("350 tokens"));
    }
}
