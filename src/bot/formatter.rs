use std::borrow::Cow;
use std::fmt::Write;

pub struct TelegramFormatter;

const TELEGRAM_CONTINUED_PREFIX: &str = "<i>(continued)</i>\n\n";
const TELEGRAM_CONTINUES_SUFFIX: &str = "\n\n<i>(continues...)</i>";
const TELEGRAM_MARKER_BUDGET: usize =
    TELEGRAM_CONTINUED_PREFIX.len() + TELEGRAM_CONTINUES_SUFFIX.len();
const MIN_MARKER_CHUNK_SIZE: usize = 120;

#[derive(Clone, Debug)]
struct OpenTag {
    name: String,
    open_markup: String,
}

impl TelegramFormatter {
    /// Format raw agy output into valid Telegram HTML format.
    #[must_use]
    pub fn to_telegram_html(raw: &str) -> String {
        if !raw.as_bytes().contains(&0x1B)
            && !raw.as_bytes().contains(&b'\r')
            && !raw.chars().any(Self::is_braille_spinner)
        {
            return Self::markdown_to_telegram_html(raw.trim_end());
        }
        let clean = Self::strip_ansi_codes(raw);
        Self::markdown_to_telegram_html(&clean)
    }

    /// Format tool usage event into Telegram HTML status card with specialized icons & parameters.
    #[must_use]
    pub fn format_tool_call(tool_name: &str, summary: &str) -> String {
        let (icon, display_title) = match tool_name {
            "view_file" | "read_resource" => ("📖", "View File"),
            "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "sed_file" => ("📝", "Edit File"),
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
        let mut out = String::with_capacity(128 + clean_param.len().min(320));
        if clean_param.is_empty() {
            out.push_str(icon);
            out.push_str(" <b>Executing:</b> <code>");
            Self::push_escaped_html(tool_name, &mut out);
            out.push_str("</code>\n\n");
            return out;
        }

        out.push_str(icon);
        out.push_str(" <b>");
        Self::push_escaped_html(display_title, &mut out);
        if clean_param.chars().nth(80).is_some() || clean_param.contains('\n') {
            let preview = Self::truncate_chars(&clean_param, 300);
            out.push_str(":</b>\n<blockquote expandable><code>");
            Self::push_escaped_html(&preview, &mut out);
            out.push_str("</code></blockquote>\n\n");
        } else {
            out.push_str(":</b> <code>");
            Self::push_escaped_html(&clean_param, &mut out);
            out.push_str("</code>\n\n");
        }
        out
    }

    /// Extract readable parameter summary from tool input JSON.
    fn extract_tool_param_summary(raw: &str) -> Cow<'_, str> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return Cow::Borrowed("");
        }

        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if let Some(cmd) = v.get("CommandLine").and_then(|x| x.as_str()) {
                return Cow::Owned(cmd.to_string());
            }
            if let Some(target) = v
                .get("TargetFile")
                .or_else(|| v.get("AbsolutePath"))
                .and_then(|x| x.as_str())
            {
                return Cow::Owned(target.to_string());
            }
            if let Some(subagents) = v.get("Subagents").and_then(|x| x.as_array())
                && let Some(first) = subagents.first()
            {
                let role = first
                    .get("Role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("Subagent");
                let prompt = first.get("Prompt").and_then(|p| p.as_str()).unwrap_or("");
                return Cow::Owned(format!("[{role}] {prompt}"));
            }
            if let Some(role) = v.get("Role").and_then(|x| x.as_str()) {
                let prompt = v.get("Prompt").and_then(|p| p.as_str()).unwrap_or("");
                return Cow::Owned(format!("[{role}] {prompt}"));
            }
            if let Some(q) = v
                .get("Query")
                .or_else(|| v.get("query"))
                .or_else(|| v.get("Pattern"))
                .and_then(|x| x.as_str())
            {
                return Cow::Owned(q.to_string());
            }
            if let Some(url) = v
                .get("Url")
                .or_else(|| v.get("url"))
                .and_then(|x| x.as_str())
            {
                return Cow::Owned(url.to_string());
            }
            if let Some(prompt) = v
                .get("Prompt")
                .or_else(|| v.get("prompt"))
                .and_then(|x| x.as_str())
            {
                return Cow::Owned(prompt.to_string());
            }
        }

        Cow::Borrowed(trimmed)
    }

    /// Format telemetry badge for the footer of completed responses.
    #[cfg(test)]
    #[must_use]
    pub fn format_telemetry_badge(
        usage: Option<&crate::executor::TokenUsage>,
        duration_secs: Option<f64>,
    ) -> String {
        let mut parts = Vec::new();
        if let Some(d) = duration_secs {
            parts.push(format!("⏱ <i>{d:.1}s</i>"));
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

    /// Balances unclosed HTML tags during real-time streaming drafts to prevent 400 bad entity
    /// errors. Returns `Cow::Borrowed` with zero allocations when all tags are already balanced.
    #[must_use]
    pub fn balance_streaming_html(html: &str) -> Cow<'_, str> {
        let mut stack: Vec<&str> = Vec::new();
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            rest = &rest[open..];
            let after_lt = &rest[1..];
            let Some(rel_close) = crate::text::find_html_tag_end(after_lt) else {
                // Incomplete tag at the very end of the stream or literal `<`: stop at trailing partial tag.
                if !after_lt.contains('>') {
                    break;
                }
                rest = after_lt;
                continue;
            };
            let tag = &after_lt[..rel_close];
            rest = &after_lt[rel_close + 1..];

            if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                if let Some(pos) = stack.iter().rposition(|open_name| *open_name == name) {
                    stack.truncate(pos);
                }
            } else {
                let name = tag.split_whitespace().next().unwrap_or("");
                if !name.is_empty() && !tag.ends_with('/') {
                    stack.push(name);
                }
            }
        }

        if stack.is_empty() {
            return Cow::Borrowed(html);
        }
        let mut balanced = String::with_capacity(html.len() + stack.len() * 8);
        balanced.push_str(html);
        for name in stack.iter().rev() {
            balanced.push_str("</");
            balanced.push_str(name);
            balanced.push('>');
        }
        Cow::Owned(balanced)
    }

    /// Format thinking step into Telegram HTML snippet with expandable blockquote.
    #[must_use]
    pub fn format_thinking(thinking_text: &str) -> String {
        let trimmed = thinking_text.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let preview = Self::truncate_chars(trimmed, 1500);
        let mut out = String::with_capacity(preview.len() + 64);
        out.push_str("<blockquote expandable>🧠 <b>Thinking:</b>\n<i>");
        Self::push_escaped_html(&preview, &mut out);
        out.push_str("</i></blockquote>\n\n");
        out
    }

    /// Strip ANSI escape sequences (`\x1b[...m`) and braille spinner frames while preserving
    /// indentation on non-spinner lines.
    #[inline]
    #[must_use]
    pub fn strip_ansi_codes(input: &str) -> String {
        crate::text::strip_ansi_codes(input)
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
        let wide = matches!(
            u,
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
                | 0x20000..=0x2FFFD
        ) || (0x1F900..=0x1F9FF).contains(&u);
        if wide { 2 } else { 1 }
    }

    #[cfg(test)]
    #[must_use]
    pub fn display_width(s: &str) -> usize {
        s.chars().map(Self::char_width).sum()
    }

    /// Cut a string slice to at most `max_cells` monospace cells without allocating, returning
    /// `(prefix_slice, used_cells)`.
    #[must_use]
    pub fn truncate_to_width(s: &str, max_cells: usize) -> (&str, usize) {
        let mut used = 0usize;
        for (idx, c) in s.char_indices() {
            let w = Self::char_width(c);
            if used + w > max_cells {
                return (&s[..idx], used);
            }
            used += w;
        }
        (s, used)
    }

    /// Markdown to Telegram HTML converter:
    /// Supports code blocks with language highlighting, headers, blockquotes, bullets, tables,
    /// horizontal rules, and snake_case-safe inline markdown.
    fn markdown_to_telegram_html(input: &str) -> String {
        let mut output = String::with_capacity(input.len() + 128);
        let mut in_code_block = false;
        let mut code_lang = String::new();
        let mut code_buf = String::new();
        let mut table_buf: Vec<&str> = Vec::new();

        let flush_table = |buf: &mut Vec<&str>, out: &mut String| {
            if buf.is_empty() {
                return;
            }
            if buf.len() >= 2 && buf[1].contains("---") {
                // Render as clean monospace ASCII table borrowing cell slices directly.
                let mut rows: Vec<Vec<&str>> = Vec::with_capacity(buf.len());
                for (idx, line) in buf.iter().enumerate() {
                    if idx == 1 {
                        continue; // skip separator row
                    }
                    let cells: Vec<&str> =
                        line.trim_matches('|').split('|').map(str::trim).collect();
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                }

                if !rows.is_empty() {
                    let col_count = rows.iter().map(Vec::len).max().unwrap_or(0);
                    let mut col_widths = vec![0usize; col_count];
                    for r in &rows {
                        for (c_idx, cell) in r.iter().enumerate() {
                            if c_idx < col_count {
                                let (_, cell_w) = Self::truncate_to_width(cell, 25);
                                col_widths[c_idx] = col_widths[c_idx].max(cell_w);
                            }
                        }
                    }

                    let mut table_str = String::with_capacity(rows.len() * col_count * 12);
                    for (r_idx, r) in rows.iter().enumerate() {
                        for (c_idx, cell) in r.iter().enumerate() {
                            let width = col_widths.get(c_idx).copied().unwrap_or(0);
                            let (cell_slice, cell_w) = Self::truncate_to_width(cell, 25);
                            let pad = width.saturating_sub(cell_w);
                            table_str.push_str(cell_slice);
                            for _ in 0..pad {
                                table_str.push(' ');
                            }
                            if c_idx + 1 < col_count {
                                table_str.push_str(" │ ");
                            }
                        }
                        table_str.push('\n');
                        if r_idx == 0 {
                            for (c_idx, w) in col_widths.iter().enumerate() {
                                if c_idx > 0 {
                                    table_str.push_str("─┼─");
                                }
                                for _ in 0..*w {
                                    table_str.push('─');
                                }
                            }
                            table_str.push('\n');
                        }
                    }
                    out.push_str("<pre><code>");
                    Self::push_escaped_html(table_str.trim_end(), out);
                    out.push_str("</code></pre>\n");
                }
            } else {
                for line in buf.iter() {
                    Self::push_inline_markdown(line, out);
                    out.push('\n');
                }
            }
            buf.clear();
        };

        for line in input.lines() {
            let trimmed = line.trim();

            // Table line detection
            if !in_code_block
                && trimmed.starts_with('|')
                && trimmed.ends_with('|')
                && trimmed.len() > 2
            {
                table_buf.push(trimmed);
                continue;
            } else if !table_buf.is_empty() {
                flush_table(&mut table_buf, &mut output);
            }

            if trimmed.starts_with("```") {
                if in_code_block {
                    in_code_block = false;
                    Self::push_code_block(&code_lang, code_buf.trim_end_matches('\n'), &mut output);
                    code_buf.clear();
                    code_lang.clear();
                } else {
                    in_code_block = true;
                    code_lang.clear();
                    code_lang.push_str(trimmed.trim_start_matches('`').trim());
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
                    output.push_str("<b>");
                    Self::push_inline_markdown(header_text, &mut output);
                    output.push_str("</b>\n");
                    continue;
                }

                // Check block math ($$ math $$)
                if trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() > 4 {
                    let math_content = trimmed[2..trimmed.len() - 2].trim();
                    output.push_str("<pre><code>");
                    Self::push_escaped_html(math_content, &mut output);
                    output.push_str("</code></pre>\n");
                    continue;
                }

                // Check expandable blockquote (>! text or >> text)
                if let Some(expandable_content) = trimmed
                    .strip_prefix(">! ")
                    .or_else(|| trimmed.strip_prefix(">> "))
                {
                    output.push_str("<blockquote expandable>");
                    Self::push_inline_markdown(expandable_content, &mut output);
                    output.push_str("</blockquote>\n");
                    continue;
                }

                // Check standard blockquote (> text)
                if let Some(quote_content) = trimmed.strip_prefix("> ") {
                    output.push_str("<blockquote>");
                    Self::push_inline_markdown(quote_content, &mut output);
                    output.push_str("</blockquote>\n");
                    continue;
                }

                // Check horizontal rule (--- or *** or ___)
                if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                    output.push_str("───────────────\n");
                    continue;
                }

                // Process line with bullet point check (* item or - item or + item)
                if let Some(bullet_content) = trimmed
                    .strip_prefix("* ")
                    .or_else(|| trimmed.strip_prefix("- "))
                    .or_else(|| trimmed.strip_prefix("+ "))
                {
                    output.push_str("• ");
                    Self::push_inline_markdown(bullet_content, &mut output);
                } else {
                    Self::push_inline_markdown(line, &mut output);
                }
                output.push('\n');
            }
        }

        if !table_buf.is_empty() {
            flush_table(&mut table_buf, &mut output);
        }

        if in_code_block && !code_buf.is_empty() {
            Self::push_code_block(&code_lang, code_buf.trim_end_matches('\n'), &mut output);
        }

        let trimmed_len = output.trim_end_matches('\n').len();
        output.truncate(trimmed_len);
        output
    }

    fn push_code_block(code_lang: &str, body: &str, out: &mut String) {
        if code_lang.is_empty() {
            out.push_str("<pre><code>");
        } else {
            out.push_str("<pre><code class=\"language-");
            Self::push_escaped_html(code_lang, out);
            out.push_str("\">");
        }
        Self::push_escaped_html(body, out);
        out.push_str("</code></pre>\n");
    }

    #[inline]
    fn push_escaped_html(s: &str, out: &mut String) {
        if !s
            .bytes()
            .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
        {
            out.push_str(s);
            return;
        }
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(c),
            }
        }
    }

    /// Format inline bold (**text** or __text__), italic (*text* or _text_), inline code (`code`),
    /// links ([text](url)), strikethrough (~~text~~) directly into `out`.
    fn push_inline_markdown(line: &str, out: &mut String) {
        let bytes = line.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            // Bold with **: **text**
            if i + 1 < len
                && bytes[i] == b'*'
                && bytes[i + 1] == b'*'
                && let Some(end) = line[i + 2..].find("**")
            {
                out.push_str("<b>");
                Self::push_escaped_html(&line[i + 2..i + 2 + end], out);
                out.push_str("</b>");
                i += 4 + end;
                continue;
            }

            // Bold with __: __text__
            if i + 1 < len
                && bytes[i] == b'_'
                && bytes[i + 1] == b'_'
                && let Some(end) = line[i + 2..].find("__")
            {
                out.push_str("<b>");
                Self::push_escaped_html(&line[i + 2..i + 2 + end], out);
                out.push_str("</b>");
                i += 4 + end;
                continue;
            }

            // Italic with *: *text*
            if bytes[i] == b'*'
                && (i == 0 || bytes[i - 1] != b'*')
                && i + 1 < len
                && bytes[i + 1] != b'*'
                && bytes[i + 1] != b' '
                && let Some(end) = line[i + 1..].find('*')
                && end > 0
                && (i + 1 + end + 1 >= len || bytes[i + 1 + end + 1] != b'*')
            {
                out.push_str("<i>");
                Self::push_escaped_html(&line[i + 1..i + 1 + end], out);
                out.push_str("</i>");
                i += 2 + end;
                continue;
            }

            // Italic with _: _text_ (strictly check word boundaries)
            if bytes[i] == b'_'
                && (i == 0 || !Self::is_identifier_char(bytes[i - 1]))
                && i + 1 < len
                && bytes[i + 1] != b'_'
                && bytes[i + 1] != b' '
                && let Some(end) = line[i + 1..].find('_')
                && end > 0
            {
                let after_idx = i + 1 + end + 1;
                if after_idx >= len || !Self::is_identifier_char(bytes[after_idx]) {
                    out.push_str("<i>");
                    Self::push_escaped_html(&line[i + 1..i + 1 + end], out);
                    out.push_str("</i>");
                    i += 2 + end;
                    continue;
                }
            }

            // Inline code: `code`
            if bytes[i] == b'`'
                && (i == 0 || bytes[i - 1] != b'`')
                && let Some(end) = line[i + 1..].find('`')
            {
                out.push_str("<code>");
                Self::push_escaped_html(&line[i + 1..i + 1 + end], out);
                out.push_str("</code>");
                i += 2 + end;
                continue;
            }

            // Inline math: $formula$ (must not be $$)
            if bytes[i] == b'$'
                && (i == 0 || bytes[i - 1] != b'$')
                && (i + 1 < len && bytes[i + 1] != b'$' && bytes[i + 1] != b' ')
                && let Some(end) = line[i + 1..].find('$')
            {
                let inner = &line[i + 1..i + 1 + end];
                if end > 0 && !inner.contains('$') && !inner.contains('\n') {
                    out.push_str("<code>");
                    Self::push_escaped_html(inner, out);
                    out.push_str("</code>");
                    i += 2 + end;
                    continue;
                }
            }

            // Markdown link: [text](url)
            if bytes[i] == b'['
                && let Some(bracket_end) = line[i + 1..].find(']')
            {
                let text_part = &line[i + 1..i + 1 + bracket_end];
                let after_bracket = i + 1 + bracket_end + 1;
                if after_bracket < len
                    && bytes[after_bracket] == b'('
                    && let Some(paren_end) = line[after_bracket + 1..].find(')')
                {
                    let url = &line[after_bracket + 1..after_bracket + 1 + paren_end];
                    if url.starts_with("http://")
                        || url.starts_with("https://")
                        || url.starts_with("tg://")
                        || url.starts_with("file://")
                    {
                        out.push_str("<a href=\"");
                        Self::push_escaped_html(url, out);
                        out.push_str("\">");
                        Self::push_escaped_html(text_part, out);
                        out.push_str("</a>");
                        i = after_bracket + 1 + paren_end + 1;
                        continue;
                    }
                }
            }

            // Strikethrough: ~~text~~
            if i + 1 < len
                && bytes[i] == b'~'
                && bytes[i + 1] == b'~'
                && let Some(end) = line[i + 2..].find("~~")
            {
                out.push_str("<s>");
                Self::push_escaped_html(&line[i + 2..i + 2 + end], out);
                out.push_str("</s>");
                i += 4 + end;
                continue;
            }

            // Default HTML escaping
            let Some(ch) = line[i..].chars().next() else {
                break;
            };
            match ch {
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '&' => out.push_str("&amp;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(ch),
            }
            i += ch.len_utf8();
        }
    }

    #[inline]
    fn is_identifier_char(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    /// Truncate to at most `max_chars` characters, appending an ellipsis when content was dropped.
    /// Never splits a UTF-8 code point.
    #[inline]
    #[must_use]
    pub fn truncate_chars(s: &str, max_chars: usize) -> Cow<'_, str> {
        crate::text::truncate_chars(s, max_chars)
    }

    /// Take at most `max_chars` characters without an ellipsis, on a character boundary.
    #[cfg(test)]
    #[inline]
    #[must_use]
    pub fn take_chars(s: &str, max_chars: usize) -> &str {
        crate::text::take_chars(s, max_chars)
    }

    /// Drop `</tag>` closers that no longer have an opener, returning `Cow::Borrowed` when
    /// everything already matches.
    #[must_use]
    pub fn drop_unmatched_closing_tags(html: &str) -> Cow<'_, str> {
        let mut out: Option<String> = None;
        let mut copied_up_to = 0usize;
        let mut stack: Vec<&str> = Vec::new();
        let mut idx = 0usize;

        while let Some(rel_open) = html[idx..].find('<') {
            let tag_start = idx + rel_open;
            let after_lt = &html[tag_start + 1..];
            let Some(rel_close) = crate::text::find_html_tag_end(after_lt) else {
                idx = tag_start + 1;
                continue;
            };
            let tag = &after_lt[..rel_close];
            let tag_end = tag_start + 1 + rel_close + 1;
            idx = tag_end;

            if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                if let Some(pos) = stack.iter().rposition(|opened| *opened == name) {
                    stack.truncate(pos);
                } else {
                    let buf = out.get_or_insert_with(|| String::with_capacity(html.len()));
                    buf.push_str(&html[copied_up_to..tag_start]);
                    copied_up_to = tag_end;
                }
            } else {
                let name = tag.split_whitespace().next().unwrap_or("");
                if !name.is_empty() && !tag.ends_with('/') {
                    stack.push(name);
                }
            }
        }

        match out {
            Some(mut buf) => {
                buf.push_str(&html[copied_up_to..]);
                Cow::Owned(buf)
            }
            None => Cow::Borrowed(html),
        }
    }

    /// If `end` lands inside an HTML tag, move the cut back to that tag's `<`; if the tag starts
    /// at `start`, extend past its `>` instead so progress is never zero.
    fn cut_outside_tag(line: &str, start: usize, end: usize) -> usize {
        let window = &line[start..end];
        if let Some(open) = window.rfind('<')
            && !window[open..].contains('>')
        {
            let cut = start + open;
            if cut > start {
                return cut;
            }
            if let Some(gt) = line[end..].find('>') {
                return end + gt + 1;
            }
            return line.len();
        }
        end
    }

    #[inline]
    fn closing_bytes(stack: &[OpenTag]) -> usize {
        stack.iter().map(|t| t.name.len() + 3).sum()
    }

    #[inline]
    fn append_closing_tags(out: &mut String, stack: &[OpenTag]) {
        for tag in stack.iter().rev() {
            let _ = write!(out, "</{}>", tag.name);
        }
    }

    #[inline]
    fn append_opening_tags(out: &mut String, stack: &[OpenTag]) {
        for tag in stack {
            out.push_str(&tag.open_markup);
        }
    }

    /// Updates `stack` across `fragment` and returns `true` if `fragment` contained any
    /// non-whitespace byte outside HTML tags.
    fn scan_fragment_tags(fragment: &str, stack: &mut Vec<OpenTag>) -> bool {
        let mut has_non_ws_body = false;
        let mut idx = 0usize;

        while idx < fragment.len() {
            let Some(rel_open) = fragment[idx..].find('<') else {
                if fragment[idx..].bytes().any(|b| !b.is_ascii_whitespace()) {
                    has_non_ws_body = true;
                }
                break;
            };
            let tag_start = idx + rel_open;
            if fragment[idx..tag_start]
                .bytes()
                .any(|b| !b.is_ascii_whitespace())
            {
                has_non_ws_body = true;
            }

            let after_lt = &fragment[tag_start + 1..];
            let Some(rel_close) = crate::text::find_html_tag_end(after_lt) else {
                // Literal `<` (not a valid tag) counts as non-whitespace text.
                has_non_ws_body = true;
                idx = tag_start + 1;
                continue;
            };

            let tag = &after_lt[..rel_close];
            let tag_end = tag_start + 1 + rel_close + 1;
            idx = tag_end;

            if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                if let Some(pos) = stack.iter().rposition(|t| t.name == name) {
                    stack.truncate(pos);
                }
            } else {
                let name = tag.split_whitespace().next().unwrap_or("");
                if !name.is_empty() && !tag.ends_with('/') {
                    stack.push(OpenTag {
                        name: name.to_string(),
                        open_markup: fragment[tag_start..tag_end].to_string(),
                    });
                }
            }
        }

        has_non_ws_body
    }

    /// Computes the closing-tag byte overhead that would remain after appending `fragment`.
    fn closing_bytes_after(stack: &[OpenTag], fragment: &str) -> usize {
        let mut sim_len = stack.len();
        let mut extra_bytes = 0usize;
        let mut extra_names: Vec<&str> = Vec::new();
        let mut idx = 0usize;

        while let Some(rel_open) = fragment[idx..].find('<') {
            let tag_start = idx + rel_open;
            let after_lt = &fragment[tag_start + 1..];
            let Some(rel_close) = crate::text::find_html_tag_end(after_lt) else {
                idx = tag_start + 1;
                continue;
            };
            let tag = &after_lt[..rel_close];
            idx = tag_start + 1 + rel_close + 1;

            if let Some(name) = tag.strip_prefix('/') {
                let name = name.trim();
                if let Some(pos) = extra_names.iter().rposition(|n| *n == name) {
                    for removed in extra_names.drain(pos..) {
                        extra_bytes = extra_bytes.saturating_sub(removed.len() + 3);
                    }
                } else if let Some(pos) = stack[..sim_len].iter().rposition(|t| t.name == name) {
                    sim_len = pos;
                }
            } else {
                let name = tag.split_whitespace().next().unwrap_or("");
                if !name.is_empty() && !tag.ends_with('/') {
                    extra_names.push(name);
                    extra_bytes += name.len() + 3;
                }
            }
        }

        Self::closing_bytes(&stack[..sim_len]) + extra_bytes
    }

    /// HTML message chunker:
    /// Splits at line boundaries (or character/tag boundaries on oversized lines) while preserving
    /// stacked block/inline tags (including `<blockquote expandable>` wrapping `<pre><code class="...">`)
    /// and adding `(continues...)` & `(continued)` when space permits.
    /// Guarantees every chunk is independently valid HTML and respects `max_chunk_size` whenever
    /// individual unbreakable tags/tokens allow.
    #[must_use]
    pub fn split_html_chunks(html: &str, max_chunk_size: usize) -> Vec<String> {
        if html.len() <= max_chunk_size {
            return vec![html.to_string()];
        }

        let use_markers = max_chunk_size >= MIN_MARKER_CHUNK_SIZE;
        let marker_budget = if use_markers {
            TELEGRAM_MARKER_BUDGET
        } else {
            0
        };
        let chunk_limit = max_chunk_size.saturating_sub(marker_budget).max(1);

        let mut raw_chunks: Vec<String> = Vec::new();
        let mut current_chunk = String::with_capacity(max_chunk_size);
        let mut open_tags: Vec<OpenTag> = Vec::new();
        let mut has_body_content = false;

        let flush_current = |raw_chunks: &mut Vec<String>,
                             current_chunk: &mut String,
                             open_tags: &[OpenTag],
                             has_body_content: &mut bool| {
            if *has_body_content {
                Self::append_closing_tags(current_chunk, open_tags);
                raw_chunks.push(std::mem::take(current_chunk));
                Self::append_opening_tags(current_chunk, open_tags);
                *has_body_content = false;
            }
        };

        for line in html.lines() {
            let line_closing_after = Self::closing_bytes_after(&open_tags, line);
            let fits_with_nl =
                current_chunk.len() + line.len() + 1 + line_closing_after <= chunk_limit;

            if fits_with_nl {
                current_chunk.push_str(line);
                current_chunk.push('\n');
                has_body_content |= Self::scan_fragment_tags(line, &mut open_tags);
                continue;
            }

            // If this line closes open tags and fits without a trailing newline, finish closing
            // in the current chunk instead of creating a tag-only tail chunk.
            let fits_closing_without_nl = line_closing_after < Self::closing_bytes(&open_tags)
                && current_chunk.len() + line.len() + line_closing_after <= chunk_limit;
            if fits_closing_without_nl {
                current_chunk.push_str(line);
                has_body_content |= Self::scan_fragment_tags(line, &mut open_tags);
                continue;
            }

            // Flush existing body before checking whether `line` fits on a fresh chunk.
            if has_body_content {
                flush_current(
                    &mut raw_chunks,
                    &mut current_chunk,
                    &open_tags,
                    &mut has_body_content,
                );
            }

            let fresh_closing_after = Self::closing_bytes_after(&open_tags, line);
            if current_chunk.len() + line.len() + 1 + fresh_closing_after <= chunk_limit {
                current_chunk.push_str(line);
                current_chunk.push('\n');
                has_body_content |= Self::scan_fragment_tags(line, &mut open_tags);
                continue;
            }

            // Single line exceeds a chunk budget: slice safely outside HTML tags and UTF-8 chars.
            let mut start_idx = 0usize;
            while start_idx < line.len() {
                let mut avail = chunk_limit
                    .saturating_sub(current_chunk.len())
                    .saturating_sub(Self::closing_bytes(&open_tags));

                if (avail == 0 || avail < 16) && has_body_content {
                    flush_current(
                        &mut raw_chunks,
                        &mut current_chunk,
                        &open_tags,
                        &mut has_body_content,
                    );
                    avail = chunk_limit
                        .saturating_sub(current_chunk.len())
                        .saturating_sub(Self::closing_bytes(&open_tags));
                }
                let avail = avail.max(1);

                let mut end_idx = (start_idx + avail).min(line.len());
                while end_idx > start_idx && !line.is_char_boundary(end_idx) {
                    end_idx -= 1;
                }
                if end_idx == start_idx {
                    end_idx = (start_idx + 1..=line.len())
                        .find(|&i| line.is_char_boundary(i))
                        .unwrap_or(line.len());
                }
                end_idx = Self::cut_outside_tag(line, start_idx, end_idx);

                let sub_str = &line[start_idx..end_idx];
                let sub_closing_after = Self::closing_bytes_after(&open_tags, sub_str);
                if has_body_content
                    && current_chunk.len() + sub_str.len() + sub_closing_after > chunk_limit
                {
                    flush_current(
                        &mut raw_chunks,
                        &mut current_chunk,
                        &open_tags,
                        &mut has_body_content,
                    );
                    continue;
                }

                current_chunk.push_str(sub_str);
                has_body_content |= Self::scan_fragment_tags(sub_str, &mut open_tags);
                start_idx = end_idx;
            }

            if current_chunk.len() + 1 + Self::closing_bytes(&open_tags) <= chunk_limit {
                current_chunk.push('\n');
            } else if has_body_content {
                flush_current(
                    &mut raw_chunks,
                    &mut current_chunk,
                    &open_tags,
                    &mut has_body_content,
                );
            }
        }

        if has_body_content || (raw_chunks.is_empty() && !current_chunk.is_empty()) {
            Self::append_closing_tags(&mut current_chunk, &open_tags);
            raw_chunks.push(current_chunk);
        }

        let total = raw_chunks.len();
        if total <= 1 {
            return raw_chunks;
        }

        let mut final_chunks = Vec::with_capacity(total);
        for (i, chunk) in raw_chunks.into_iter().enumerate() {
            let mut formatted = String::with_capacity(chunk.len() + marker_budget);
            if use_markers && i > 0 {
                formatted.push_str(TELEGRAM_CONTINUED_PREFIX);
            }
            formatted.push_str(&chunk);
            if use_markers && i < total - 1 {
                formatted.push_str(TELEGRAM_CONTINUES_SUFFIX);
            }
            let sanitized = Self::drop_unmatched_closing_tags(&formatted);
            let balanced = Self::balance_streaming_html(&sanitized);
            final_chunks.push(balanced.into_owned());
        }

        final_chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_markdown_to_telegram_html() {
        let md =
            "## 标题\n这是 **加粗** 和 *斜体* 以及 `代码` 与 ~~删除线~~。\n> 引用内容\n* 列表项";
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

        let already_balanced = "<b>ok</b>";
        assert!(matches!(
            TelegramFormatter::balance_streaming_html(already_balanced),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn balance_closes_nested_inline_elements_in_order() {
        let html = "<b>bold <i>italic <code>code";
        let balanced = TelegramFormatter::balance_streaming_html(html);
        assert!(balanced.ends_with("</code></i></b>"));
    }

    #[test]
    fn unmatched_closers_are_dropped() {
        let html = "tail</code> text</b>";
        assert_eq!(
            TelegramFormatter::drop_unmatched_closing_tags(html),
            "tail text"
        );
        let balanced = "<b>ok</b>";
        assert!(matches!(
            TelegramFormatter::drop_unmatched_closing_tags(balanced),
            Cow::Borrowed(_)
        ));
    }

    fn assert_html_balanced(chunk: &str) {
        let mut stack: Vec<String> = Vec::new();
        let mut rest = chunk;
        while let Some(open) = rest.find('<') {
            rest = &rest[open..];
            let Some(close) = rest.find('>') else { break };
            let tag = &rest[1..close];
            rest = &rest[close + 1..];
            if let Some(name) = tag.strip_prefix('/') {
                let expected = stack.pop();
                assert_eq!(
                    expected.as_deref(),
                    Some(name.trim()),
                    "unbalanced chunk: {chunk}"
                );
            } else {
                let name = tag.split_whitespace().next().unwrap_or("");
                if !name.is_empty() && !tag.ends_with('/') {
                    stack.push(name.to_string());
                }
            }
        }
        assert!(stack.is_empty(), "unclosed tags in chunk: {chunk}");
    }

    #[test]
    fn long_line_cuts_never_break_tags() {
        let html = format!("before <code>{}</code> after", "x".repeat(9000));
        let chunks = TelegramFormatter::split_html_chunks(&html, 4000);
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert_html_balanced(&chunk);
            assert!(chunk.len() <= 4000, "chunk exceeded limit: {}", chunk.len());
        }
    }

    #[test]
    fn nested_pre_inside_expandable_blockquote_preserves_nesting_and_attrs() {
        let code_lines = "let value = 42;\n".repeat(40);
        let html = format!(
            "🔔 <b>【通知】</b>\n\n<blockquote expandable>头部说明\n<pre><code class=\"language-rust\">{code_lines}</code></pre>\n尾部说明</blockquote>"
        );
        let chunks = TelegramFormatter::split_html_chunks(&html, 250);
        assert!(
            chunks.len() > 2,
            "expected multiple chunks, got {}",
            chunks.len()
        );
        for (idx, chunk) in chunks.iter().enumerate() {
            assert_html_balanced(chunk);
            assert!(
                chunk.len() <= 250,
                "chunk {idx} exceeded max_chunk_size=250: len={}",
                chunk.len()
            );
            assert!(
                !crate::text::strip_html_tags(chunk)
                    .replace("(continued)", "")
                    .replace("(continues...)", "")
                    .trim()
                    .is_empty(),
                "chunk {idx} had no body content: {chunk}"
            );
        }
        assert!(
            chunks[1].contains("<blockquote expandable>")
                && chunks[1].contains("<pre><code class=\"language-rust\">"),
            "continuation chunk lost expandable blockquote or language class: {}",
            chunks[1]
        );
    }

    #[test]
    fn small_max_chunk_size_150_stays_bounded_and_non_empty() {
        let long_pre = format!(
            "<pre><code class=\"language-python\">{}</code></pre>",
            "x = 1\n".repeat(40)
        );
        let chunks = TelegramFormatter::split_html_chunks(&long_pre, 150);
        assert!(chunks.len() > 1);
        for (idx, chunk) in chunks.iter().enumerate() {
            assert_html_balanced(chunk);
            assert!(
                chunk.len() <= 150,
                "chunk {idx} exceeded max_chunk_size=150: len={} ({chunk:?})",
                chunk.len()
            );
            assert!(
                chunk.contains("x = 1"),
                "chunk {idx} emitted tag-only shell without body: {chunk:?}"
            );
        }
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
        assert_eq!(
            TelegramFormatter::take_chars("8de907d6-1c2b", 8),
            "8de907d6"
        );
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
        assert_eq!(
            TelegramFormatter::strip_ansi_codes(raw),
            "Resolving dependencies...\nOK"
        );
    }

    #[test]
    fn test_ansi_escape_sequences_are_stripped() {
        assert_eq!(
            TelegramFormatter::strip_ansi_codes("\x1b[31mred\x1b[0m plain"),
            "red plain"
        );
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
        assert_eq!(
            TelegramFormatter::truncate_to_width("中文abc", 4),
            ("中文", 4)
        );
        assert_eq!(TelegramFormatter::truncate_to_width("abc", 25), ("abc", 3));
        assert_eq!(TelegramFormatter::truncate_to_width("", 5), ("", 0));
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
        assert_eq!(
            lines.len(),
            4,
            "expected header, separator and 2 rows: {lines:?}"
        );

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

    #[test]
    fn test_markdown_timeout_diagnostics_render_and_escape_telegram_html() {
        let frozen_md = "⏱ **任务卡死终止**\n超过 180s 无任何输出。\n关联进程: `agy<main>&1`\n\n请发送 /new 重置会话后重试。";
        let frozen_html = TelegramFormatter::to_telegram_html(frozen_md);
        assert!(frozen_html.contains("<b>任务卡死终止</b>"));
        assert!(frozen_html.contains("<code>agy&lt;main&gt;&amp;1</code>"));

        let stdin_md = "⚠️ **任务死锁终止**\n```\nssh user@host < /tmp/key && echo ok\n```";
        let stdin_html = TelegramFormatter::to_telegram_html(stdin_md);
        assert!(stdin_html.contains("<b>任务死锁终止</b>"));
        assert!(
            stdin_html
                .contains("<pre><code>ssh user@host &lt; /tmp/key &amp;&amp; echo ok</code></pre>")
        );
    }
}
