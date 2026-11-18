use crate::text::{
    self, MdBlock, MdDocument, MdInline, MdQuoteKind, MdTable, inlines_to_plain_text,
    parse_markdown, push_escaped_html, render_compact_box_table, separate_blocks,
};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum TgStyle<'a> {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Spoiler,
    Link(&'a str),
}

impl TgStyle<'_> {
    fn push_open(self, out: &mut String) {
        match self {
            Self::Bold => out.push_str("<b>"),
            Self::Italic => out.push_str("<i>"),
            Self::Underline => out.push_str("<u>"),
            Self::Strikethrough => out.push_str("<s>"),
            Self::Spoiler => out.push_str("<tg-spoiler>"),
            Self::Link(url) => {
                out.push_str("<a href=\"");
                push_escaped_html(url, out);
                out.push_str("\">");
            }
        }
    }

    fn push_close(self, out: &mut String) {
        match self {
            Self::Bold => out.push_str("</b>"),
            Self::Italic => out.push_str("</i>"),
            Self::Underline => out.push_str("</u>"),
            Self::Strikethrough => out.push_str("</s>"),
            Self::Spoiler => out.push_str("</tg-spoiler>"),
            Self::Link(_) => out.push_str("</a>"),
        }
    }
}

/// Telegram HTML inline writer enforcing strict Bot API entity nesting rules:
/// - Lazy opening eliminates empty tags such as `<b></b>`.
/// - `<code>` is never nested inside `<b>`/`<i>`/`<u>`/`<s>`/`<tg-spoiler>`/`<a>`
///   (wrapping styles temporarily close around `<code>` and lazily reopen afterward;
///   inside `<a href="...">`, inline code preserves the clickable link text).
struct TgInlineWriter<'a> {
    out: &'a mut String,
    desired: Vec<TgStyle<'a>>,
    emitted: usize,
}

impl<'a> TgInlineWriter<'a> {
    fn new(out: &'a mut String) -> Self {
        Self {
            out,
            desired: Vec::new(),
            emitted: 0,
        }
    }

    fn ensure_open(&mut self) {
        while self.emitted < self.desired.len() {
            self.desired[self.emitted].push_open(self.out);
            self.emitted += 1;
        }
    }

    fn close_all_emitted(&mut self) {
        while self.emitted > 0 {
            self.emitted -= 1;
            self.desired[self.emitted].push_close(self.out);
        }
    }

    fn push_style(&mut self, style: TgStyle<'a>) {
        self.desired.push(style);
    }

    fn pop_style(&mut self) {
        if self.emitted == self.desired.len() {
            self.emitted -= 1;
            if let Some(style) = self.desired.pop() {
                style.push_close(self.out);
            }
        } else {
            self.desired.pop();
        }
    }

    fn push_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.ensure_open();
        push_escaped_html(s, self.out);
    }

    fn push_code(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.desired.iter().any(|st| matches!(st, TgStyle::Link(_))) {
            // Keep clickable `<a href="...">` intact without illegal `<code>` inside `<a>`.
            self.ensure_open();
            push_escaped_html(s, self.out);
            return;
        }
        self.close_all_emitted();
        self.out.push_str("<code>");
        push_escaped_html(s, self.out);
        self.out.push_str("</code>");
    }

    fn write_inlines(&mut self, inlines: &'a [MdInline]) {
        for node in inlines {
            match node {
                MdInline::Text(s) => self.push_text(s),
                MdInline::Code(s) | MdInline::Math(s) => self.push_code(s),
                MdInline::SoftBreak | MdInline::HardBreak => self.push_text("\n"),
                MdInline::Bold(children) => {
                    let dup = self.desired.contains(&TgStyle::Bold);
                    if !dup {
                        self.push_style(TgStyle::Bold);
                    }
                    self.write_inlines(children);
                    if !dup {
                        self.pop_style();
                    }
                }
                MdInline::Italic(children) => {
                    let dup = self.desired.contains(&TgStyle::Italic);
                    if !dup {
                        self.push_style(TgStyle::Italic);
                    }
                    self.write_inlines(children);
                    if !dup {
                        self.pop_style();
                    }
                }
                MdInline::Underline(children) => {
                    let dup = self.desired.contains(&TgStyle::Underline);
                    if !dup {
                        self.push_style(TgStyle::Underline);
                    }
                    self.write_inlines(children);
                    if !dup {
                        self.pop_style();
                    }
                }
                MdInline::Strikethrough(children) => {
                    let dup = self.desired.contains(&TgStyle::Strikethrough);
                    if !dup {
                        self.push_style(TgStyle::Strikethrough);
                    }
                    self.write_inlines(children);
                    if !dup {
                        self.pop_style();
                    }
                }
                MdInline::Spoiler(children) => {
                    let dup = self.desired.contains(&TgStyle::Spoiler);
                    if !dup {
                        self.push_style(TgStyle::Spoiler);
                    }
                    self.write_inlines(children);
                    if !dup {
                        self.pop_style();
                    }
                }
                MdInline::Link { url, inlines } => {
                    if is_telegram_clickable_url(url) {
                        self.push_style(TgStyle::Link(url.as_str()));
                        if inlines.is_empty() {
                            self.push_text(url);
                        } else {
                            self.write_inlines(inlines);
                        }
                        self.pop_style();
                    } else if let Some(local_path) = url.strip_prefix("file://") {
                        let label = inlines_to_plain_text(inlines);
                        if inlines.is_empty() || label.trim() == local_path {
                            self.push_code(local_path);
                        } else {
                            self.write_inlines(inlines);
                            self.push_text(" (");
                            self.push_code(local_path);
                            self.push_text(")");
                        }
                    } else {
                        let label = inlines_to_plain_text(inlines);
                        if inlines.is_empty() || label.trim() == url {
                            self.push_text(url);
                        } else {
                            self.write_inlines(inlines);
                            self.push_text(" (");
                            self.push_text(url);
                            self.push_text(")");
                        }
                    }
                }
                MdInline::Image { url, alt } => {
                    self.push_text("🖼 ");
                    if is_telegram_clickable_url(url) {
                        self.push_style(TgStyle::Link(url.as_str()));
                        if alt.is_empty() {
                            self.push_text(url);
                        } else {
                            self.write_inlines(alt);
                        }
                        self.pop_style();
                    } else if !alt.is_empty() {
                        self.write_inlines(alt);
                    } else {
                        self.push_text(url);
                    }
                }
            }
        }
    }

    fn finish(mut self) {
        self.close_all_emitted();
    }
}

#[inline]
fn is_telegram_clickable_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("tg://")
}

impl TelegramFormatter {
    /// Format raw agy Markdown/terminal output into valid Telegram HTML.
    #[must_use]
    pub fn to_telegram_html(raw: &str) -> String {
        let doc = parse_markdown(raw);
        let mut out = String::with_capacity(raw.len() + 128);
        render_tg_document(&doc, false, &mut out);
        let trimmed_len = out.trim_end_matches('\n').len();
        out.truncate(trimmed_len);
        out
    }

    /// Formats raw Markdown for embedding inside an outer `<blockquote expandable>` container
    /// (such as proactive notifications), flattening any inner blockquotes to prevent illegal
    /// nested `<blockquote>` tags rejected by Telegram Bot API.
    #[must_use]
    pub fn to_telegram_html_for_blockquote(raw: &str) -> String {
        let doc = parse_markdown(raw);
        let mut out = String::with_capacity(raw.len() + 128);
        render_tg_document(&doc, true, &mut out);
        let trimmed_len = out.trim_end_matches('\n').len();
        out.truncate(trimmed_len);
        out
    }

    /// Format tool usage event into Telegram HTML status card with specialized icons & parameters.
    #[must_use]
    pub fn format_tool_call(tool_name: &str, summary: &str) -> String {
        let (icon, display_title) = match tool_name {
            "view_file" | "read_file" | "read_resource" | "list_dir" => ("📖", "Read File"),
            "write_to_file"
            | "write_file"
            | "edit_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "sed_file" => ("📝", "Edit File"),
            "run_command" | "send_command_input" | "command_status" => ("💻", "Run Command"),
            "grep_search" | "find_by_name" => ("🔍", "Search Code"),
            "search_web" | "read_url_content" => ("🌐", "Web Search"),
            "invoke_subagent" | "define_subagent" | "manage_subagents" => ("🤖", "Subagent"),
            "run_workflow" => ("⚙️", "Workflow"),
            "swarm" | "post_message" | "read_board" | "swarm_wait" => ("🐝", "Swarm"),
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
            push_escaped_html(tool_name, &mut out);
            out.push_str("</code>\n\n");
            return out;
        }

        out.push_str(icon);
        out.push_str(" <b>");
        push_escaped_html(display_title, &mut out);
        if clean_param.chars().nth(80).is_some() || clean_param.contains('\n') {
            let preview = Self::truncate_chars(&clean_param, 300);
            out.push_str(":</b>\n<blockquote expandable><code>");
            push_escaped_html(&preview, &mut out);
            out.push_str("</code></blockquote>\n\n");
        } else {
            out.push_str(":</b> <code>");
            push_escaped_html(&clean_param, &mut out);
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
                .or_else(|| v.get("Path"))
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
            if let Some(desc) = v
                .get("Description")
                .or_else(|| v.get("Prompt"))
                .or_else(|| v.get("prompt"))
                .and_then(|x| x.as_str())
            {
                return Cow::Owned(desc.to_string());
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
            let Some(rel_close) = text::find_html_tag_end(after_lt) else {
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
        push_escaped_html(&preview, &mut out);
        out.push_str("</i></blockquote>\n\n");
        out
    }

    /// Strip ANSI escape sequences (`\x1b[...m`) and braille spinner frames while preserving
    /// indentation on non-spinner lines.
    #[allow(dead_code)]
    #[inline]
    #[must_use]
    pub fn strip_ansi_codes(input: &str) -> String {
        text::strip_ansi_codes(input)
    }

    /// Returns monospace column display width (CJK/emoji double-width aware).
    #[allow(dead_code)]
    #[inline]
    #[must_use]
    pub fn display_width(s: &str) -> usize {
        text::display_width(s)
    }

    /// Cut a string slice to at most `max_cells` monospace cells without allocating, returning
    /// `(prefix_slice, used_cells)`.
    #[allow(dead_code)]
    #[inline]
    #[must_use]
    pub fn truncate_to_width(s: &str, max_cells: usize) -> (&str, usize) {
        text::truncate_to_width(s, max_cells)
    }

    /// Truncate to at most `max_chars` characters, appending an ellipsis when content was dropped.
    /// Never splits a UTF-8 code point.
    #[inline]
    #[must_use]
    pub fn truncate_chars(s: &str, max_chars: usize) -> Cow<'_, str> {
        text::truncate_chars(s, max_chars)
    }

    /// Take at most `max_chars` characters without an ellipsis, on a character boundary.
    #[cfg(test)]
    #[inline]
    #[must_use]
    pub fn take_chars(s: &str, max_chars: usize) -> &str {
        text::take_chars(s, max_chars)
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
            let Some(rel_close) = text::find_html_tag_end(after_lt) else {
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

    /// Ensures `end` does not slice an HTML `<...>` tag OR an HTML `&...;` entity.
    /// Moves `cut` back to the start of the interrupted token, or extends past its terminator
    /// if the token starts right at `start`.
    fn cut_outside_tag_or_entity(line: &str, start: usize, end: usize) -> usize {
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

        // Guard HTML character entities (`&amp;`, `&lt;`, `&gt;`, `&quot;`, `&#39;`, etc.).
        let tail_start = end.saturating_sub(10).max(start);
        let tail = &line[tail_start..end];
        if let Some(rel_amp) = tail.rfind('&')
            && !tail[rel_amp..].contains(';')
        {
            let amp_pos = tail_start + rel_amp;
            if let Some(semi_rel) = line[amp_pos..].find(';')
                && semi_rel <= 10
            {
                let candidate = &line[amp_pos + 1..amp_pos + semi_rel];
                let looks_like_entity = !candidate.is_empty()
                    && (candidate.strip_prefix('#').is_some_and(|digits| {
                        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                    }) || candidate.bytes().all(|b| b.is_ascii_alphanumeric()));
                if looks_like_entity {
                    if amp_pos > start {
                        return amp_pos;
                    }
                    return amp_pos + semi_rel + 1;
                }
            }
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
            let Some(rel_close) = text::find_html_tag_end(after_lt) else {
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
            let Some(rel_close) = text::find_html_tag_end(after_lt) else {
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

    /// Checks whether keeping a `<pre>...</pre>` block (such as a compact box table) intact
    /// by flushing before it would allow the entire block to live in a single fresh chunk.
    fn should_flush_before_atomic_pre(
        lines: &[&str],
        line_idx: usize,
        open_tags: &[OpenTag],
        current_chunk_len: usize,
        chunk_limit: usize,
    ) -> bool {
        if open_tags.iter().any(|t| t.name == "pre") {
            return false;
        }
        let first = lines[line_idx];
        if !first.contains("<pre>") {
            return false;
        }
        let outer_overhead: usize = open_tags
            .iter()
            .map(|t| t.open_markup.len() + t.name.len() + 3)
            .sum();
        let mut block_bytes = 0usize;
        let mut found_end = false;
        for &l in lines.iter().skip(line_idx) {
            block_bytes += l.len() + 1;
            if outer_overhead + block_bytes > chunk_limit {
                return false;
            }
            if l.contains("</pre>") {
                found_end = true;
                break;
            }
        }
        found_end && current_chunk_len + block_bytes + Self::closing_bytes(open_tags) > chunk_limit
    }

    /// HTML message chunker:
    /// Splits at line boundaries (or character/tag/entity boundaries on oversized lines) while
    /// preserving stacked block/inline tags (including `<blockquote expandable>` wrapping
    /// `<pre><code class="...">`) and adding `(continues...)` & `(continued)` when space permits.
    /// Guarantees every chunk is independently valid HTML, never splits UTF-8 codepoints or
    /// `&amp;` HTML entities, and keeps compact `<pre>` blocks intact when they fit in one chunk.
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

        let lines: Vec<&str> = html.lines().collect();
        for (line_idx, &line) in lines.iter().enumerate() {
            if has_body_content
                && Self::should_flush_before_atomic_pre(
                    &lines,
                    line_idx,
                    &open_tags,
                    current_chunk.len(),
                    chunk_limit,
                )
            {
                flush_current(
                    &mut raw_chunks,
                    &mut current_chunk,
                    &open_tags,
                    &mut has_body_content,
                );
            }

            let line_closing_after = Self::closing_bytes_after(&open_tags, line);
            let fits_with_nl =
                current_chunk.len() + line.len() + 1 + line_closing_after <= chunk_limit;

            if fits_with_nl {
                current_chunk.push_str(line);
                current_chunk.push('\n');
                has_body_content |= Self::scan_fragment_tags(line, &mut open_tags);
                continue;
            }

            let fits_closing_without_nl = line_closing_after < Self::closing_bytes(&open_tags)
                && current_chunk.len() + line.len() + line_closing_after <= chunk_limit;
            if fits_closing_without_nl {
                current_chunk.push_str(line);
                has_body_content |= Self::scan_fragment_tags(line, &mut open_tags);
                continue;
            }

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

            // Single line exceeds a chunk budget: slice safely outside HTML tags, entities, and UTF-8 chars.
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
                end_idx = Self::cut_outside_tag_or_entity(line, start_idx, end_idx);

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

fn render_tg_inlines(inlines: &[MdInline], out: &mut String) {
    let mut writer = TgInlineWriter::new(out);
    writer.write_inlines(inlines);
    writer.finish();
}

fn render_tg_bold_inlines(inlines: &[MdInline], out: &mut String) {
    let mut writer = TgInlineWriter::new(out);
    writer.push_style(TgStyle::Bold);
    writer.write_inlines(inlines);
    writer.pop_style();
    writer.finish();
}

fn render_tg_italic_bold_inlines(inlines: &[MdInline], out: &mut String) {
    let mut writer = TgInlineWriter::new(out);
    writer.push_style(TgStyle::Italic);
    writer.push_style(TgStyle::Bold);
    writer.write_inlines(inlines);
    writer.pop_style();
    writer.pop_style();
    writer.finish();
}

fn estimate_blocks_metrics(blocks: &[MdBlock]) -> (usize, usize, bool) {
    let mut chars = 0usize;
    let mut lines = 0usize;
    let mut has_heavy = false;
    for block in blocks {
        match block {
            MdBlock::Paragraph(inlines) | MdBlock::Heading { inlines, .. } => {
                let p = inlines_to_plain_text(inlines);
                chars += p.chars().count();
                lines += p.lines().count().max(1);
            }
            MdBlock::CodeBlock { code, .. } | MdBlock::MathBlock(code) => {
                chars += code.chars().count();
                lines += code.lines().count().max(1);
                has_heavy = true;
            }
            MdBlock::Table(t) => {
                chars += t.rows.len() * t.headers.len() * 16;
                lines += t.rows.len() + 2;
                has_heavy = true;
            }
            MdBlock::BlockQuote { blocks, .. } => {
                let (c, l, h) = estimate_blocks_metrics(blocks);
                chars += c;
                lines += l;
                has_heavy |= h;
            }
            MdBlock::List { items, .. } => {
                for item in items {
                    let (c, l, h) = estimate_blocks_metrics(&item.blocks);
                    chars += c;
                    lines += l;
                    has_heavy |= h;
                }
            }
            MdBlock::ThematicBreak => lines += 1,
        }
    }
    (chars, lines, has_heavy)
}

fn render_tg_document(doc: &MdDocument, in_blockquote: bool, out: &mut String) {
    render_tg_blocks(&doc.blocks, in_blockquote, 0, out);
}

fn render_tg_blocks(blocks: &[MdBlock], in_blockquote: bool, list_depth: usize, out: &mut String) {
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            separate_blocks(&blocks[idx - 1], block, true, out);
        }

        match block {
            MdBlock::Paragraph(inlines) => {
                render_tg_inlines(inlines, out);
                out.push('\n');
            }
            MdBlock::Heading { level, inlines } => {
                match level {
                    1 => {
                        out.push_str("📌 ");
                        render_tg_bold_inlines(inlines, out);
                    }
                    2 => {
                        out.push_str("▌ ");
                        render_tg_bold_inlines(inlines, out);
                    }
                    3 => {
                        out.push_str("▸ ");
                        render_tg_bold_inlines(inlines, out);
                    }
                    _ => render_tg_italic_bold_inlines(inlines, out),
                }
                out.push('\n');
            }
            MdBlock::CodeBlock { language, code } => {
                if language.is_empty() {
                    out.push_str("<pre><code>");
                } else {
                    out.push_str("<pre><code class=\"language-");
                    push_escaped_html(language, out);
                    out.push_str("\">");
                }
                push_escaped_html(code, out);
                out.push_str("</code></pre>\n");
            }
            MdBlock::MathBlock(math) => {
                out.push_str("<pre><code>");
                push_escaped_html(math, out);
                out.push_str("</code></pre>\n");
            }
            MdBlock::ThematicBreak => {
                out.push_str("───────────────\n");
            }
            MdBlock::BlockQuote { kind, blocks } => {
                if in_blockquote {
                    // Already inside a `<blockquote>`: flatten with left accent bars so Telegram
                    // never receives illegal nested `<blockquote>` tags.
                    let mut inner = String::new();
                    if let Some((icon, label)) = kind.alert_badge() {
                        let _ = writeln!(inner, "{icon} <b>{label}</b>");
                    }
                    render_tg_blocks(blocks, true, list_depth, &mut inner);
                    for line in inner.trim_end_matches('\n').lines() {
                        out.push_str("▎ ");
                        out.push_str(line);
                        out.push('\n');
                    }
                } else {
                    let (chars, lines, has_heavy) = estimate_blocks_metrics(blocks);
                    let expandable = *kind == MdQuoteKind::Expandable
                        || chars >= 220
                        || lines >= 5
                        || (has_heavy && lines >= 3);
                    if expandable {
                        out.push_str("<blockquote expandable>");
                    } else {
                        out.push_str("<blockquote>");
                    }
                    if let Some((icon, label)) = kind.alert_badge() {
                        let _ = writeln!(out, "{icon} <b>{label}</b>");
                    }
                    let mut inner = String::new();
                    render_tg_blocks(blocks, true, list_depth, &mut inner);
                    out.push_str(inner.trim_end_matches('\n'));
                    out.push_str("</blockquote>\n");
                }
            }
            MdBlock::List {
                ordered_start,
                items,
            } => {
                let bullet = match list_depth {
                    0 => "• ",
                    1 => "◦ ",
                    _ => "▪ ",
                };
                for (i, item) in items.iter().enumerate() {
                    for _ in 0..list_depth {
                        out.push_str("  ");
                    }
                    match item.task {
                        Some(true) => out.push_str("☑ "),
                        Some(false) => out.push_str("☐ "),
                        None => {
                            if let Some(start) = ordered_start {
                                let _ = write!(out, "{}. ", start + i as u64);
                            } else {
                                out.push_str(bullet);
                            }
                        }
                    }

                    if item.blocks.is_empty() {
                        out.push('\n');
                        continue;
                    }

                    let mut first_para_inline = false;
                    if let MdBlock::Paragraph(inlines) = &item.blocks[0] {
                        render_tg_inlines(inlines, out);
                        out.push('\n');
                        first_para_inline = true;
                    } else {
                        out.push('\n');
                    }

                    let rest_start = usize::from(first_para_inline);
                    if rest_start < item.blocks.len() {
                        render_tg_blocks(
                            &item.blocks[rest_start..],
                            in_blockquote,
                            list_depth + 1,
                            out,
                        );
                    }
                }
            }
            MdBlock::Table(table) => {
                if let Some(boxed) = render_compact_box_table(table) {
                    out.push_str("<pre><code>");
                    push_escaped_html(&boxed, out);
                    out.push_str("</code></pre>\n");
                } else {
                    render_tg_card_table(table, in_blockquote, out);
                }
            }
        }
    }
}

fn render_tg_card_table(table: &MdTable, in_blockquote: bool, out: &mut String) {
    let col_count = table.headers.len();
    if col_count == 0 {
        return;
    }

    let mut card_body = String::with_capacity(table.rows.len() * col_count * 32 + 64);

    if col_count == 2 {
        card_body.push_str("📋 <b>");
        let h0_plain = inlines_to_plain_text(&table.headers[0]);
        let h1_plain = inlines_to_plain_text(&table.headers[1]);
        push_escaped_html(h0_plain.trim(), &mut card_body);
        card_body.push_str(" / ");
        push_escaped_html(h1_plain.trim(), &mut card_body);
        card_body.push_str("</b>\n");

        for row in &table.rows {
            let k_empty = inlines_to_plain_text(&row[0]).trim().is_empty();
            let v_empty = inlines_to_plain_text(&row[1]).trim().is_empty();
            if k_empty && v_empty {
                continue;
            }
            card_body.push_str("• ");
            if !k_empty {
                render_tg_bold_inlines(&row[0], &mut card_body);
                if !v_empty {
                    card_body.push_str(": ");
                    render_tg_inlines(&row[1], &mut card_body);
                }
            } else {
                render_tg_inlines(&row[1], &mut card_body);
            }
            card_body.push('\n');
        }
    } else if table.rows.is_empty() {
        card_body.push_str("📊 <b>");
        for (idx, header) in table.headers.iter().enumerate() {
            if idx > 0 {
                card_body.push_str(" / ");
            }
            push_escaped_html(inlines_to_plain_text(header).trim(), &mut card_body);
        }
        card_body.push_str("</b>\n");
    } else {
        let h0 = inlines_to_plain_text(&table.headers[0]);
        card_body.push_str("📊 <b>");
        push_escaped_html(h0.trim(), &mut card_body);
        card_body.push_str("</b>\n");

        for (r_idx, row) in table.rows.iter().enumerate() {
            if r_idx > 0 && table.rows.len() > 2 {
                card_body.push('\n');
            }
            let first_plain = inlines_to_plain_text(&row[0]);
            if first_plain.trim().is_empty() {
                let _ = writeln!(card_body, "• <b>Row {}</b>", r_idx + 1);
            } else {
                let _ = write!(card_body, "• <b>{}. </b>", r_idx + 1);
                render_tg_bold_inlines(&row[0], &mut card_body);
                card_body.push('\n');
            }

            for (c_idx, cell) in row.iter().enumerate().skip(1) {
                if inlines_to_plain_text(cell).trim().is_empty() {
                    continue;
                }
                let raw_hdr = inlines_to_plain_text(&table.headers[c_idx]);
                let hdr = raw_hdr.trim();
                card_body.push_str("  ▫️ <b>");
                if hdr.is_empty() {
                    let _ = write!(card_body, "Col {}", c_idx + 1);
                } else {
                    push_escaped_html(hdr, &mut card_body);
                }
                card_body.push_str(":</b> ");
                render_tg_inlines(cell, &mut card_body);
                card_body.push('\n');
            }
        }
    }

    let trimmed = card_body.trim_end_matches('\n');
    if in_blockquote {
        out.push_str(trimmed);
        out.push('\n');
    } else if table.rows.len() >= 8 || trimmed.chars().count() >= 500 {
        out.push_str("<blockquote expandable>");
        out.push_str(trimmed);
        out.push_str("</blockquote>\n");
    } else {
        out.push_str("<blockquote>");
        out.push_str(trimmed);
        out.push_str("</blockquote>\n");
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
    fn nested_markdown_and_telegram_entities_stay_valid() {
        let md = "# **主标题 `cmd`**\n\n这是 **粗体含 *斜体* 和 `code` 与 ~~删除~~**，以及 <u>下划线</u> 和 ||剧透内容||。\n- [x] 已完成\n- [ ] 待办\n\n[文档](https://example.com?a=1&b=2) 和 [源码](file:///home/user/main.rs)\n\n逻辑或: `if a || b` 或 x || y || z\n\n泛型: Vec<String>";
        let html = TelegramFormatter::to_telegram_html(md);
        assert_html_balanced(&html);
        assert!(!html.contains("<b><b>"));
        assert!(!html.contains("<a href=\"file://"));
        assert!(html.contains("<code>/home/user/main.rs</code>"));
        assert!(html.contains("<a href=\"https://example.com?a=1&amp;b=2\">文档</a>"));
        assert!(html.contains("<u>下划线</u>"));
        assert!(html.contains("<tg-spoiler>剧透内容</tg-spoiler>"));
        assert!(html.contains("☑ 已完成"));
        assert!(html.contains("☐ 待办"));
        assert!(html.contains("x || y || z"));
        assert!(html.contains("Vec&lt;String&gt;"));
        // Telegram forbids `<code>` nested inside `<b>`.
        assert!(!html.contains("<b>主标题 <code>"));
        assert!(!html.contains("<code>cmd</code></b>"));
    }

    #[test]
    fn long_quotes_and_bang_quotes_become_expandable_without_nesting() {
        let short = TelegramFormatter::to_telegram_html("> 短引用");
        assert_eq!(short, "<blockquote>短引用</blockquote>");

        let bang = TelegramFormatter::to_telegram_html(">! 折叠提示\n>! 第二行");
        assert!(bang.starts_with("<blockquote expandable>"));
        assert!(!bang.contains("! "));

        let long_lines = "> 这是一段很长的引用内容用于测试自动折叠效果。\n".repeat(6);
        let long_html = TelegramFormatter::to_telegram_html(&long_lines);
        assert!(long_html.starts_with("<blockquote expandable>"));

        let nested = TelegramFormatter::to_telegram_html("> 外层\n> > 内层");
        assert_eq!(nested.matches("<blockquote>").count(), 1);
        assert_eq!(nested.matches("</blockquote>").count(), 1);

        let for_notif = TelegramFormatter::to_telegram_html_for_blockquote("> 引用\n\n正文");
        assert!(!for_notif.contains("<blockquote>"));
        assert!(for_notif.contains("▎ 引用"));
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
    fn long_line_cuts_never_break_tags_or_html_entities() {
        let html = format!("before <code>{}</code> after", "x&amp;y&lt;z ".repeat(700));
        let chunks = TelegramFormatter::split_html_chunks(&html, 400);
        assert!(chunks.len() > 1);
        for (idx, chunk) in chunks.iter().enumerate() {
            assert_html_balanced(chunk);
            assert!(
                chunk.len() <= 400,
                "chunk {idx} exceeded limit: {}",
                chunk.len()
            );
            assert!(!chunk.ends_with("&a") && !chunk.ends_with("&am") && !chunk.ends_with("&amp"));
            assert!(!chunk.ends_with("&l") && !chunk.ends_with("&lt"));
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
                !text::strip_html_tags(chunk)
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
    fn test_table_formatting_compact_box_and_wide_cards() {
        let compact_md = "| Model | Speed |\n|---|---|\n| Flash | Fast |\n| Pro | Deep |";
        let html = TelegramFormatter::to_telegram_html(compact_md);
        assert!(html.contains("<pre><code>"));
        assert!(html.contains('┌'));
        assert!(html.contains('└'));
        assert!(html.contains("Flash"));
        assert!(html.contains("Fast"));

        let wide_md = "| Model | Context Window | Key Capability |\n|---|---|---|\n| **Gemini 4 Argon** | `1M` | Complex reasoning & multi-file edits |\n| **Claude Opus** | `200K` | Architecture & [Docs](https://example.com) |";
        let wide_html = TelegramFormatter::to_telegram_html(wide_md);
        assert!(
            !wide_html.contains('┌'),
            "wide table should render as cards: {wide_html}"
        );
        assert!(wide_html.contains("<blockquote>"));
        assert!(wide_html.contains("📊 <b>Model</b>"));
        assert!(
            wide_html.contains("<b>1. </b><b>Gemini 4 Argon</b>")
                || wide_html.contains("<b>Gemini 4 Argon</b>")
        );
        assert!(wide_html.contains("<code>1M</code>"));
        assert!(wide_html.contains("<a href=\"https://example.com\">Docs</a>"));
        assert_html_balanced(&wide_html);
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
            6,
            "expected top border, header, separator, 2 rows and bottom border: {lines:?}"
        );

        let dividers: Vec<Vec<usize>> = lines
            .iter()
            .map(|line| {
                let mut cells = 0usize;
                let mut hits = Vec::new();
                for c in line.chars() {
                    if matches!(c, '┌' | '├' | '│' | '└' | '┬' | '┼' | '┴' | '┐' | '┤' | '┘')
                    {
                        hits.push(cells);
                    }
                    cells += TelegramFormatter::display_width(&c.to_string());
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
        for (idx, w) in widths.iter().enumerate() {
            assert_eq!(widths[0], *w, "row {idx} width mismatch: {widths:?}");
        }
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

    #[test]
    fn gfm_tables_handle_pipes_in_code_missing_outer_pipes_uneven_cols_and_alignment() {
        let md = "\
Expr | Align | Note
--- | :---: | ---:
`a | b` | **ok** | 12
x \\| y | mid
`m || n` | | 999";
        let html = TelegramFormatter::to_telegram_html(md);
        assert_html_balanced(&html);
        assert!(html.contains("<pre><code>"));
        assert!(html.contains("a | b"));
        assert!(html.contains("x | y"));
        assert!(html.contains("m || n"));
        assert!(html.contains("┌"));
        assert!(html.contains("└"));

        let body = html
            .strip_prefix("<pre><code>")
            .and_then(|s| s.strip_suffix("</code></pre>"))
            .unwrap();
        let widths: Vec<usize> = body
            .lines()
            .map(|l| TelegramFormatter::display_width(l.trim_end()))
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "misaligned: {body}"
        );
    }

    #[test]
    fn link_with_parens_math_nested_lists_and_gfm_alerts_render_cleanly() {
        let md = "\
> [!WARNING]
> Check `config.toml` before restart.

1. First step with [Rust](https://en.wikipedia.org/wiki/Rust_(programming_language))
   - Nested bullet
   - [x] Subtask done
2. Math: $E = mc^2$

$$
\\sum_{i=1}^n i
$$";
        let html = TelegramFormatter::to_telegram_html(md);
        assert_html_balanced(&html);
        assert!(html.contains("⚠️ <b>Warning</b>"));
        assert!(html.contains(
            "<a href=\"https://en.wikipedia.org/wiki/Rust_(programming_language)\">Rust</a>"
        ));
        assert!(html.contains("1. First step"));
        assert!(html.contains("  ◦ Nested bullet"));
        assert!(html.contains("  ☑ Subtask done"));
        assert!(html.contains("<code>E = mc^2</code>"));
        assert!(html.contains("<pre><code>\\sum_{i=1}^n i</code></pre>"));
    }

    #[test]
    fn compact_table_is_flushed_intact_to_next_chunk_when_it_fits() {
        let intro = format!("{}\n\n", "Preface text line.\n".repeat(12));
        let table = "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |";
        let html = TelegramFormatter::to_telegram_html(&format!("{intro}{table}"));
        let chunks = TelegramFormatter::split_html_chunks(&html, 240);
        assert!(chunks.len() >= 2);
        let table_chunk = chunks.last().unwrap();
        assert!(table_chunk.contains('┌'));
        assert!(table_chunk.contains('└'));
        assert_html_balanced(table_chunk);
    }
}
