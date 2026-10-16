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

    /// Strip ANSI escape sequences (\x1b[...m) and braille spinner frames.
    /// Indentation is preserved: progress lines that carried a spinner frame were redrawn in
    /// place, so only those lines lose their left padding. Stripping whitespace globally used
    /// to flatten every fenced code block.
    pub fn strip_ansi_codes(input: &str) -> String {
        let mut result = String::with_capacity(input.len());

        for line in input.lines() {
            let mut in_escape = false;
            let mut had_spinner = false;
            let mut buf = String::with_capacity(line.len());

            for c in line.chars() {
                if c == '\x1B' {
                    in_escape = true;
                } else if in_escape {
                    if c.is_ascii_alphabetic() {
                        in_escape = false;
                    }
                } else if Self::is_braille_spinner(c) {
                    had_spinner = true;
                } else {
                    buf.push(c);
                }
            }

            let kept = if had_spinner { buf.trim_start() } else { &buf[..] };
            result.push_str(kept);
            result.push('\n');
        }

        result.trim_end().to_string()
    }

    #[inline]
    fn is_braille_spinner(c: char) -> bool {
        matches!(c, '\u{2800}'..='\u{280F}')
    }

    /// Monospace cells a glyph occupies. East Asian Wide/Fullwidth characters (and emoji)
    /// render two cells wide in Telegram's monospace font, so padding tables by character count
    /// misaligned every column as soon as a cell held Chinese.
    #[inline]
    fn char_width(c: char) -> usize {
        let u = c as u32;
        if (0x0300..=0x036F).contains(&u) || u == 0x200B {
            return 0;
        }
        let wide = matches!(u,
            0x1100..=0x115F
                | 0x2E80..=0x303E
                | 0x3041..=0x33FF
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xA000..=0xA4CF
                | 0xAC00..=0xD7A3
                | 0xF900..=0xFAFF
                | 0xFE10..=0xFE19
                | 0xFE30..=0xFE6F
                | 0xFF00..=0xFF60
                | 0xFFE0..=0xFFE6
                | 0x1F300..=0x1FAFF
                | 0x20000..=0x2FFFD)
            || (0x1F900..=0x1F9FF).contains(&u);
        if wide {
            2
        } else {
            1
        }
    }

    pub fn display_width(s: &str) -> usize {
        s.chars().map(Self::char_width).sum()
    }

    /// Cut a string to at most `max_cells` monospace cells, never splitting a character.
    pub fn truncate_to_width(s: &str, max_cells: usize) -> String {
        let mut out = String::with_capacity(s.len().min(max_cells * 3));
        let mut used = 0usize;
        for c in s.chars() {
            let w = Self::char_width(c);
            if used + w > max_cells {
                break;
            }
            used += w;
            out.push(c);
        }
        out
    }

    /// Recover a monospace block's full opening markup, including the optional
    /// `class="language-…"` attribute, so continuation chunks keep Telegram's syntax
    /// highlighting instead of degrading to plain text.
    fn pre_open_tag(line: &str) -> String {
        let start = match line.find("<pre") {
            Some(idx) => idx,
            None => return "<pre><code>".to_string(),
        };
        let rest = &line[start..];
        let code_start = rest.find("<code");
        let cut = match code_start {
            Some(idx) => match rest[idx..].find('>') {
                Some(offset) => idx + offset + 1,
                None => rest.find('>').map(|g| g + 1).unwrap_or(rest.len()),
            },
            None => rest.find('>').map(|g| g + 1).unwrap_or(rest.len()),
        };
        rest[..cut].to_string()
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
                                let clipped = Self::truncate_to_width(cell, 25);
                                col_widths[c_idx] =
                                    col_widths[c_idx].max(Self::display_width(&clipped));
                            }
                        }
                    }

                    let mut table_str = String::new();
                    for (r_idx, r) in rows.iter().enumerate() {
                        let mut row_line = String::new();
                        for (c_idx, cell) in r.iter().enumerate() {
                            let width = col_widths.get(c_idx).copied().unwrap_or(0);
                            let cell_chars = Self::truncate_to_width(cell, 25);
                            let pad = width.saturating_sub(Self::display_width(&cell_chars));
                            row_line.push_str(&cell_chars);
                            for _ in 0..pad {
                                row_line.push(' ');
                            }
                            if c_idx + 1 < col_count {
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

    /// Truncate to at most `max_chars` characters, appending an ellipsis when content was dropped.
    /// Never splits a UTF-8 code point (byte-index slicing panicked on CJK titles).
    #[inline]
    pub fn truncate_chars(s: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
        match s.char_indices().nth(max_chars) {
            Some((cut, _)) => std::borrow::Cow::Owned(format!("{}...", &s[..cut])),
            None => std::borrow::Cow::Borrowed(s),
        }
    }

    /// Take at most `max_chars` characters without an ellipsis, on a character boundary.
    #[inline]
    pub fn take_chars(s: &str, max_chars: usize) -> &str {
        match s.char_indices().nth(max_chars) {
            Some((cut, _)) => &s[..cut],
            None => s,
        }
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
        let mut pre_open = String::from("<pre><code>");

        for line in html.lines() {
            let line_len = line.len() + 1;

            if line.contains("<pre") {
                in_pre_tag = true;
                pre_open = Self::pre_open_tag(line);
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
                            current_chunk.push_str(&pre_open);
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
                        current_chunk.push_str(&pre_open);
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

    #[test]
    fn test_truncate_chars_never_splits_utf8() {
        // Titles harvested from real brain transcripts; each used to abort the daemon
        // when sliced at byte 20 by the /history and menu:history renderers.
        let real_titles = [
            "帮我看看可以怎样优化wsl占用的硬盘存储空间",
            "帮我想办法自主更新wsl下的antigravit",
            "你还记得我有一个azure的vps一直开着的吗？",
            "查看这个文件夹下的项目是做什么的，有什么可以优化",
            "为了更新方便我配置了antigravity的自动同步",
        ];
        for title in real_titles {
            let shown = TelegramFormatter::truncate_chars(title, 20);
            assert!(shown.chars().count() <= 23, "over-truncated: {shown}");
            assert!(shown.ends_with("..."), "expected ellipsis for: {shown}");
        }

        assert_eq!(TelegramFormatter::truncate_chars("abc", 10), "abc");
        assert_eq!(TelegramFormatter::truncate_chars("abcd", 2), "ab...");
        assert_eq!(TelegramFormatter::truncate_chars("", 5), "");
    }

    #[test]
    fn test_take_chars_never_splits_utf8() {
        assert_eq!(TelegramFormatter::take_chars("日本語テスト", 3), "日本語");
        assert_eq!(TelegramFormatter::take_chars("ab", 8), "ab");
        assert_eq!(TelegramFormatter::take_chars("", 4), "");
        assert_eq!(TelegramFormatter::take_chars("8de907d6-1c2b", 8), "8de907d6");
    }

    #[test]
    fn test_fenced_code_indentation_is_preserved() {
        let md = "```python\ndef main():\n    return 1\n```";
        let html = TelegramFormatter::to_telegram_html(md);
        assert!(
            html.contains("    return 1"),
            "leading indentation was stripped: {html}"
        );
    }

    #[test]
    fn test_tab_indentation_is_preserved() {
        let raw = "fn main() {\n\tcall();\n}";
        assert!(TelegramFormatter::strip_ansi_codes(raw).contains("\tcall();"));
    }

    #[test]
    fn test_spinner_frames_are_stripped_from_progress_lines() {
        let raw = "  ⠋ Resolving dependencies...\nOK";
        assert_eq!(TelegramFormatter::strip_ansi_codes(raw), "Resolving dependencies...\nOK");
    }

    #[test]
    fn test_ansi_escape_sequences_are_stripped() {
        assert_eq!(TelegramFormatter::strip_ansi_codes("\x1b[31mred\x1b[0m plain"), "red plain");
    }

    #[test]
    fn test_blank_lines_between_paragraphs_survive() {
        assert_eq!(TelegramFormatter::strip_ansi_codes("a\n\nb"), "a\n\nb");
    }

    #[test]
    fn test_display_width_counts_wide_glyphs() {
        assert_eq!(TelegramFormatter::display_width("Note"), 4);
        assert_eq!(TelegramFormatter::display_width("中文"), 4);
        assert_eq!(TelegramFormatter::display_width("中文a"), 5);
        assert_eq!(TelegramFormatter::display_width(""), 0);
    }

    #[test]
    fn test_truncate_to_width_never_splits_a_wide_glyph() {
        assert_eq!(TelegramFormatter::truncate_to_width("中文abc", 4), "中文");
        assert_eq!(TelegramFormatter::truncate_to_width("abc", 25), "abc");
        assert_eq!(TelegramFormatter::truncate_to_width("", 5), "");
    }

    #[test]
    fn test_table_with_cjk_columns_stays_aligned() {
        let md = "| 名称 | 状态 |\n|---|---|\n| 抖音 | 正常 |\n| ab | ok |\n";
        let html = TelegramFormatter::to_telegram_html(md);
        let body = html
            .strip_prefix("<pre><code>")
            .and_then(|s| s.strip_suffix("</code></pre>"))
            .unwrap_or_else(|| panic!("table did not render as a monospace block: {html}"));

        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "expected header, separator and 2 rows: {lines:?}");

        // The invariant is the monospace *cell* position of every column divider, not the
        // character index: a CJK cell is two columns wide but one character long.
        let dividers: Vec<Vec<usize>> = lines
            .iter()
            .map(|line| {
                let mut cells = 0usize;
                let mut hits = Vec::new();
                for c in line.chars() {
                    if c == '│' || c == '┼' {
                        hits.push(cells);
                    }
                    cells += TelegramFormatter::char_width(c);
                }
                hits
            })
            .collect();
        for (idx, d) in dividers.iter().enumerate() {
            assert_eq!(&dividers[0], d, "row {idx} dividers drifted: {lines:?}");
        }

        // Borders must also end at the same column, ignoring the trailing padding that the
        // final line loses to trim_end.
        let widths: Vec<usize> = lines
            .iter()
            .map(|l| TelegramFormatter::display_width(l.trim_end()))
            .collect();
        assert_eq!(
            widths[0], widths[1],
            "separator row length must match the header: {widths:?}"
        );
        assert_eq!(
            widths[0],
            TelegramFormatter::display_width(lines[2].trim_end()),
            "CJK data row must be as wide as the header: {widths:?}"
        );
    }

    #[test]
    fn test_long_code_block_keeps_language_class_across_chunks() {
        let code = "fn main() {\n    let x = 1;\n}\n".repeat(400);
        let md = format!("```rust\n{code}```");
        let html = TelegramFormatter::to_telegram_html(&md);
        let chunks = TelegramFormatter::split_html_chunks(&html, 4000);
        assert!(
            chunks.len() > 1,
            "sample should span several chunks, got {}",
            chunks.len()
        );

        for (idx, chunk) in chunks.iter().enumerate().skip(1) {
            assert!(
                chunk.contains("<pre><code class=\"language-rust\">"),
                "continuation chunk {idx} reopened without the language class: {}",
                &chunk[..chunk.len().min(120)]
            );
        }
    }
}
