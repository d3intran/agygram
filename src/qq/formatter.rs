use crate::text::{
    self, MdBlock, MdInline, MdTable, inlines_to_plain_text, parse_markdown,
    render_compact_box_table, separate_blocks,
};
use std::borrow::Cow;
use std::fmt::Write as _;

/// Formatter for QQ Bot messages, providing aligned Markdown rendering,
/// smart responsive table layout, and structured plain-text fallback.
pub(crate) struct QqFormatter;

impl QqFormatter {
    /// Formats raw model Markdown output into QQ-optimized Markdown.
    ///
    /// Strips terminal ANSI/spinner artifacts, preserves clean native headings, lists,
    /// blockquotes, links, and renders GFM tables responsively:
    /// - Compact tables (`<= 38` display columns, single-line cells) -> aligned Unicode
    ///   box-drawing tables inside a monospace fence.
    /// - Wide / rich tables -> mobile-friendly structured quote cards.
    #[must_use]
    pub(crate) fn format_markdown(raw: &str) -> String {
        let doc = parse_markdown(raw);
        let mut out = String::with_capacity(raw.len() + 256);
        render_qq_md_blocks(&doc.blocks, 0, &mut out);
        while out.ends_with("\n\n\n") {
            out.pop();
        }
        out
    }

    /// Converts *already-formatted* QQ Markdown (output of [`Self::format_markdown`]) into
    /// clean structured plain text without raw Markdown syntax (`**`, `[title](url)`, ` ``` `).
    #[must_use]
    pub(crate) fn formatted_to_plain_text(formatted_md: &str) -> String {
        let doc = parse_markdown(formatted_md);
        let mut out = String::with_capacity(formatted_md.len() + 64);
        render_qq_plain_blocks(&doc.blocks, 0, &mut out);
        while out.ends_with("\n\n\n") {
            out.pop();
        }
        out
    }

    /// Formats raw model Markdown and converts it directly to clean QQ plain text.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn to_plain_text(raw: &str) -> String {
        let md = Self::format_markdown(raw);
        Self::formatted_to_plain_text(&md)
    }

    /// Splits formatted QQ Markdown into chunks bounded by `max_chars` and `max_bytes`,
    /// closing and reopening fenced code blocks across chunk boundaries.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn split_markdown_chunks(
        formatted_md: &str,
        max_chars: usize,
        max_bytes: usize,
    ) -> Vec<String> {
        text::split_markdown_chunks(formatted_md, max_chars, max_bytes)
    }
}

fn max_backtick_run(s: &str) -> usize {
    let mut best = 0usize;
    let mut cur = 0usize;
    for b in s.bytes() {
        if b == b'`' {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

fn push_qq_inline_code(code: &str, out: &mut String) {
    let run = max_backtick_run(code) + 1;
    for _ in 0..run {
        out.push('`');
    }
    let needs_pad = run > 1 && (code.starts_with('`') || code.ends_with('`'));
    if needs_pad {
        out.push(' ');
    }
    out.push_str(code);
    if needs_pad {
        out.push(' ');
    }
    for _ in 0..run {
        out.push('`');
    }
}

fn push_qq_fenced_block(lang: &str, body: &str, out: &mut String) {
    let fence_len = max_backtick_run(body).max(2) + 1;
    for _ in 0..fence_len {
        out.push('`');
    }
    out.push_str(lang);
    out.push('\n');
    out.push_str(body);
    if !body.is_empty() && !body.ends_with('\n') {
        out.push('\n');
    }
    for _ in 0..fence_len {
        out.push('`');
    }
    out.push('\n');
}

fn render_qq_md_inlines(inlines: &[MdInline], in_bold: bool, out: &mut String) {
    for node in inlines {
        match node {
            MdInline::Text(s) => out.push_str(&convert_inline_latex_in_text(s)),
            MdInline::Code(s) => push_qq_inline_code(s, out),
            MdInline::Math(s) => out.push_str(&latex_to_unicode(s)),
            MdInline::SoftBreak | MdInline::HardBreak => out.push('\n'),
            MdInline::Bold(children) => {
                if in_bold {
                    render_qq_md_inlines(children, true, out);
                } else {
                    let inner_plain = inlines_to_plain_text(children);
                    if inner_plain.trim().is_empty() {
                        render_qq_md_inlines(children, false, out);
                    } else {
                        out.push_str("**");
                        render_qq_md_inlines(children, true, out);
                        out.push_str("**");
                    }
                }
            }
            MdInline::Italic(children) => {
                out.push('*');
                render_qq_md_inlines(children, in_bold, out);
                out.push('*');
            }
            MdInline::Strikethrough(children) => {
                out.push_str("~~");
                render_qq_md_inlines(children, in_bold, out);
                out.push_str("~~");
            }
            MdInline::Underline(children) => {
                render_qq_md_inlines(children, in_bold, out);
            }
            MdInline::Spoiler(children) => {
                out.push_str("||");
                render_qq_md_inlines(children, in_bold, out);
                out.push_str("||");
            }
            MdInline::Link { url, inlines } => {
                if let Some(local_path) = url.strip_prefix("file://") {
                    let label = inlines_to_plain_text(inlines);
                    if inlines.is_empty() || label.trim() == local_path {
                        push_qq_inline_code(local_path, out);
                    } else {
                        render_qq_md_inlines(inlines, in_bold, out);
                        out.push_str(" (");
                        push_qq_inline_code(local_path, out);
                        out.push(')');
                    }
                } else {
                    let label = inlines_to_plain_text(inlines);
                    if inlines.is_empty() || label.trim() == url {
                        out.push_str(url);
                    } else {
                        out.push('[');
                        render_qq_md_inlines(inlines, in_bold, out);
                        out.push_str("](");
                        out.push_str(url);
                        out.push(')');
                    }
                }
            }
            MdInline::Image { url, alt } => {
                out.push_str("🖼 ");
                let alt_text = inlines_to_plain_text(alt);
                if alt_text.trim().is_empty() {
                    out.push_str(url);
                } else {
                    out.push('[');
                    out.push_str(alt_text.trim());
                    out.push_str("](");
                    out.push_str(url);
                    out.push(')');
                }
            }
        }
    }
}

fn push_qq_bold_cell_inlines(inlines: &[MdInline], out: &mut String) {
    let mut rendered = String::new();
    render_qq_md_inlines(inlines, true, &mut rendered);
    let trimmed = rendered.trim();
    if trimmed.is_empty() {
        return;
    }
    // `in_bold = true` suppressed inner `**`, so wrapping once never creates `****`.
    if trimmed.contains("**") {
        out.push_str(trimmed);
    } else {
        out.push_str("**");
        out.push_str(trimmed);
        out.push_str("**");
    }
}

fn render_qq_md_blocks(blocks: &[MdBlock], list_depth: usize, out: &mut String) {
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            separate_blocks(&blocks[idx - 1], block, true, out);
        }

        match block {
            MdBlock::Paragraph(inlines) => {
                render_qq_md_inlines(inlines, false, out);
                out.push('\n');
            }
            MdBlock::Heading { level, inlines } => {
                for _ in 0..(*level).clamp(1, 6) {
                    out.push('#');
                }
                out.push(' ');
                render_qq_md_inlines(inlines, false, out);
                out.push('\n');
            }
            MdBlock::CodeBlock { language, code } => {
                push_qq_fenced_block(language, code, out);
            }
            MdBlock::MathBlock(math) => {
                let readable = latex_to_unicode(math);
                push_qq_fenced_block("", &readable, out);
            }
            MdBlock::ThematicBreak => {
                out.push_str("---\n");
            }
            MdBlock::BlockQuote { kind, blocks } => {
                let mut inner = String::new();
                if let Some((icon, label)) = kind.alert_badge() {
                    let _ = writeln!(inner, "{icon} **{label}**");
                }
                render_qq_md_blocks(blocks, list_depth, &mut inner);
                for line in inner.trim_end_matches('\n').lines() {
                    if line.is_empty() {
                        out.push_str(">\n");
                    } else {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
            MdBlock::List {
                ordered_start,
                items,
            } => {
                for (i, item) in items.iter().enumerate() {
                    for _ in 0..list_depth {
                        out.push_str("  ");
                    }
                    match item.task {
                        Some(true) => out.push_str("- ☑ "),
                        Some(false) => out.push_str("- ☐ "),
                        None => {
                            if let Some(start) = ordered_start {
                                let _ = write!(out, "{}. ", start + i as u64);
                            } else {
                                out.push_str("- ");
                            }
                        }
                    }

                    if item.blocks.is_empty() {
                        out.push('\n');
                        continue;
                    }

                    let mut first_para_inline = false;
                    if let MdBlock::Paragraph(inlines) = &item.blocks[0] {
                        render_qq_md_inlines(inlines, false, out);
                        out.push('\n');
                        first_para_inline = true;
                    } else {
                        out.push('\n');
                    }

                    let rest_start = usize::from(first_para_inline);
                    if rest_start < item.blocks.len() {
                        render_qq_md_blocks(&item.blocks[rest_start..], list_depth + 1, out);
                    }
                }
            }
            MdBlock::Table(table) => {
                if let Some(boxed) = render_compact_box_table(table) {
                    push_qq_fenced_block("", &boxed, out);
                } else {
                    render_qq_card_table(table, out);
                }
            }
        }
    }
}

fn render_qq_card_table(table: &MdTable, out: &mut String) {
    let col_count = table.headers.len();
    if col_count == 0 {
        return;
    }

    if col_count == 2 {
        let h0 = inlines_to_plain_text(&table.headers[0]);
        let h1 = inlines_to_plain_text(&table.headers[1]);
        let _ = writeln!(out, "> 📋 **{} / {}**", h0.trim(), h1.trim());

        for row in &table.rows {
            let k_empty = inlines_to_plain_text(&row[0]).trim().is_empty();
            let v_empty = inlines_to_plain_text(&row[1]).trim().is_empty();
            if k_empty && v_empty {
                continue;
            }
            out.push_str("> • ");
            if !k_empty {
                push_qq_bold_cell_inlines(&row[0], out);
                if !v_empty {
                    out.push_str(": ");
                    render_qq_md_inlines(&row[1], false, out);
                }
            } else {
                render_qq_md_inlines(&row[1], false, out);
            }
            out.push('\n');
        }
        return;
    }

    if table.rows.is_empty() {
        out.push_str("> 📊 **");
        for (idx, h) in table.headers.iter().enumerate() {
            if idx > 0 {
                out.push_str(" / ");
            }
            out.push_str(inlines_to_plain_text(h).trim());
        }
        out.push_str("**\n");
        return;
    }

    let h0 = inlines_to_plain_text(&table.headers[0]);
    let _ = writeln!(out, "> 📊 **{}**", h0.trim());

    for (r_idx, row) in table.rows.iter().enumerate() {
        out.push_str("> • ");
        if inlines_to_plain_text(&row[0]).trim().is_empty() {
            let _ = writeln!(out, "**Row {}**", r_idx + 1);
        } else {
            push_qq_bold_cell_inlines(&row[0], out);
            out.push('\n');
        }

        for (c_idx, cell) in row.iter().enumerate().skip(1) {
            if inlines_to_plain_text(cell).trim().is_empty() {
                continue;
            }
            let raw_hdr = inlines_to_plain_text(&table.headers[c_idx]);
            let hdr = raw_hdr.trim();
            if hdr.is_empty() {
                let _ = write!(out, ">   ▫️ **Col {}**: ", c_idx + 1);
            } else {
                let _ = write!(out, ">   ▫️ **{hdr}**: ");
            }
            render_qq_md_inlines(cell, false, out);
            out.push('\n');
        }
    }
}

// ---------------------------------------------------------------------------
// QQ Clean Structured Plain-Text Renderer (used on msg_type: 0 fallback)
// ---------------------------------------------------------------------------

fn render_qq_plain_inlines(inlines: &[MdInline], out: &mut String) {
    for node in inlines {
        match node {
            MdInline::Text(s) => out.push_str(&convert_inline_latex_in_text(s)),
            MdInline::Code(s) => out.push_str(s),
            MdInline::Math(s) => out.push_str(&latex_to_unicode(s)),
            MdInline::SoftBreak | MdInline::HardBreak => out.push('\n'),
            MdInline::Bold(c)
            | MdInline::Italic(c)
            | MdInline::Strikethrough(c)
            | MdInline::Underline(c) => render_qq_plain_inlines(c, out),
            MdInline::Spoiler(c) => {
                out.push_str("[隐藏: ");
                render_qq_plain_inlines(c, out);
                out.push(']');
            }
            MdInline::Link { url, inlines } => {
                let label = inlines_to_plain_text(inlines);
                let clean_url = url.strip_prefix("file://").unwrap_or(url);
                if label.trim().is_empty() || label.trim() == url || label.trim() == clean_url {
                    out.push_str(clean_url);
                } else {
                    render_qq_plain_inlines(inlines, out);
                    out.push_str(" (");
                    out.push_str(clean_url);
                    out.push(')');
                }
            }
            MdInline::Image { url, alt } => {
                out.push_str("🖼 ");
                let alt_text = inlines_to_plain_text(alt);
                if alt_text.trim().is_empty() {
                    out.push_str(url);
                } else {
                    out.push_str(alt_text.trim());
                    out.push_str(" (");
                    out.push_str(url);
                    out.push(')');
                }
            }
        }
    }
}

fn render_qq_plain_blocks(blocks: &[MdBlock], list_depth: usize, out: &mut String) {
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            separate_blocks(&blocks[idx - 1], block, false, out);
        }

        match block {
            MdBlock::Paragraph(inlines) => {
                render_qq_plain_inlines(inlines, out);
                out.push('\n');
            }
            MdBlock::Heading { level, inlines } => {
                if !out.is_empty() {
                    while !out.ends_with("\n\n") {
                        out.push('\n');
                    }
                }
                match level {
                    1 => {
                        out.push_str("📌 【");
                        render_qq_plain_inlines(inlines, out);
                        out.push_str("】\n━━━━━━━━━━━━━━━\n");
                    }
                    2 => {
                        out.push_str("📌 【");
                        render_qq_plain_inlines(inlines, out);
                        out.push_str("】\n───────────────\n");
                    }
                    3 => {
                        out.push_str("📌 【");
                        render_qq_plain_inlines(inlines, out);
                        out.push_str("】\n");
                    }
                    _ => {
                        out.push_str("▸ 【");
                        render_qq_plain_inlines(inlines, out);
                        out.push_str("】\n");
                    }
                }
            }
            MdBlock::CodeBlock { code, .. } => {
                out.push_str(code);
                out.push('\n');
            }
            MdBlock::MathBlock(math) => {
                out.push_str(&latex_to_unicode(math));
                out.push('\n');
            }
            MdBlock::ThematicBreak => {
                out.push_str("───────────────\n");
            }
            MdBlock::BlockQuote { kind, blocks } => {
                let mut inner = String::new();
                if let Some((icon, label)) = kind.alert_badge() {
                    let _ = writeln!(inner, "{icon} {label}");
                }
                render_qq_plain_blocks(blocks, list_depth, &mut inner);
                for line in inner.trim_end_matches('\n').lines() {
                    out.push_str("│ ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            MdBlock::List {
                ordered_start,
                items,
            } => {
                let bullet = if list_depth == 0 { "• " } else { "◦ " };
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
                        render_qq_plain_inlines(inlines, out);
                        out.push('\n');
                        first_para_inline = true;
                    } else {
                        out.push('\n');
                    }

                    let rest_start = usize::from(first_para_inline);
                    if rest_start < item.blocks.len() {
                        render_qq_plain_blocks(&item.blocks[rest_start..], list_depth + 1, out);
                    }
                }
            }
            MdBlock::Table(table) => {
                if let Some(boxed) = render_compact_box_table(table) {
                    out.push_str(&boxed);
                    out.push('\n');
                } else {
                    let mut card_md = String::new();
                    render_qq_card_table(table, &mut card_md);
                    let parsed = parse_markdown(&card_md);
                    render_qq_plain_blocks(&parsed.blocks, list_depth, out);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lightweight LaTeX -> Readable Unicode Converter for QQ Chat
// ---------------------------------------------------------------------------

fn skip_ascii_spaces(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

/// Reads either a balanced `{...}` group, a `\command` token, or a single UTF-8 char from `s`,
/// returning `(argument_content, remaining_slice)`.
fn take_latex_arg(s: &str) -> Option<(&str, &str)> {
    let trimmed = skip_ascii_spaces(s);
    if trimmed.is_empty() {
        return None;
    }
    if let Some(after_open) = trimmed.strip_prefix('{') {
        let bytes = after_open.as_bytes();
        let mut depth = 1usize;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if bytes[i] == b'{' {
                depth += 1;
            } else if bytes[i] == b'}' {
                depth -= 1;
                if depth == 0 {
                    return Some((&after_open[..i], &after_open[i + 1..]));
                }
            }
            i += 1;
        }
        return Some((after_open, ""));
    }

    if let Some(after_slash) = trimmed.strip_prefix('\\') {
        let cmd_len = after_slash
            .bytes()
            .take_while(u8::is_ascii_alphabetic)
            .count();
        if cmd_len > 0 {
            let end = 1 + cmd_len;
            return Some((&trimmed[..end], &trimmed[end..]));
        }
        if let Some(ch) = after_slash.chars().next() {
            let end = 1 + ch.len_utf8();
            return Some((&trimmed[..end], &trimmed[end..]));
        }
        return None;
    }

    let ch = trimmed.chars().next()?;
    let end = ch.len_utf8();
    Some((&trimmed[..end], &trimmed[end..]))
}

/// Reads an optional `[...]` bracket argument (used by `\sqrt[n]{x}`).
fn take_latex_bracket_arg(s: &str) -> Option<(&str, &str)> {
    let trimmed = skip_ascii_spaces(s);
    let after_open = trimmed.strip_prefix('[')?;
    let bytes = after_open.as_bytes();
    let mut depth = 1usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'[' {
            depth += 1;
        } else if bytes[i] == b']' {
            depth -= 1;
            if depth == 0 {
                return Some((&after_open[..i], &after_open[i + 1..]));
            }
        }
        i += 1;
    }
    None
}

fn to_unicode_superscript(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed == "°" {
        return Some("°".to_string());
    }
    let mut out = String::with_capacity(trimmed.len() * 2);
    for ch in trimmed.chars() {
        let sup = match ch {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            '-' | '−' => '⁻',
            '=' => '⁼',
            '(' => '⁽',
            ')' => '⁾',
            'n' => 'ⁿ',
            'i' => 'ⁱ',
            _ => return None,
        };
        out.push(sup);
    }
    Some(out)
}

fn to_unicode_subscript(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len() * 2);
    for ch in trimmed.chars() {
        let sub = match ch {
            '0' => '₀',
            '1' => '₁',
            '2' => '₂',
            '3' => '₃',
            '4' => '₄',
            '5' => '₅',
            '6' => '₆',
            '7' => '₇',
            '8' => '₈',
            '9' => '₉',
            '+' => '₊',
            '-' | '−' => '₋',
            '=' => '₌',
            '(' => '₍',
            ')' => '₎',
            'a' => 'ₐ',
            'e' => 'ₑ',
            'i' => 'ᵢ',
            'j' => 'ⱼ',
            'k' => 'ₖ',
            'n' => 'ₙ',
            'o' => 'ₒ',
            'x' => 'ₓ',
            _ => return None,
        };
        out.push(sub);
    }
    Some(out)
}

fn latex_symbol_replacement(cmd: &str) -> Option<&'static str> {
    match cmd {
        // Relations & comparisons
        "approx" => Some("≈"),
        "sim" => Some("~"),
        "simeq" => Some("≃"),
        "cong" => Some("≅"),
        "equiv" => Some("≡"),
        "propto" => Some("∝"),
        "le" | "leq" | "leqslant" => Some("≤"),
        "ge" | "geq" | "geqslant" => Some("≥"),
        "ne" | "neq" => Some("≠"),
        "ll" => Some("≪"),
        "gg" => Some("≫"),
        // Arithmetic & dots
        "times" => Some("×"),
        "div" => Some("÷"),
        "pm" => Some("±"),
        "mp" => Some("∓"),
        "cdot" => Some("·"),
        "cdots" => Some("⋯"),
        "ldots" | "dots" => Some("…"),
        "ast" => Some("*"),
        "star" => Some("★"),
        "circ" => Some("°"),
        "bullet" => Some("•"),
        // Arrows
        "rightarrow" | "to" => Some("→"),
        "leftarrow" | "gets" => Some("←"),
        "leftrightarrow" => Some("↔"),
        "Rightarrow" | "implies" => Some("⇒"),
        "Leftarrow" => Some("⇐"),
        "Leftrightarrow" | "iff" => Some("⇔"),
        "mapsto" => Some("↦"),
        "uparrow" => Some("↑"),
        "downarrow" => Some("↓"),
        // Calculus, logic & sets
        "infty" => Some("∞"),
        "partial" => Some("∂"),
        "nabla" => Some("∇"),
        "sum" => Some("∑"),
        "prod" => Some("∏"),
        "int" => Some("∫"),
        "iint" => Some("∬"),
        "oint" => Some("∮"),
        "forall" => Some("∀"),
        "exists" => Some("∃"),
        "emptyset" | "varnothing" => Some("∅"),
        "in" => Some("∈"),
        "notin" => Some("∉"),
        "subset" => Some("⊂"),
        "subseteq" => Some("⊆"),
        "supset" => Some("⊃"),
        "supseteq" => Some("⊇"),
        "cup" => Some("∪"),
        "cap" => Some("∩"),
        "land" | "wedge" => Some("∧"),
        "lor" | "vee" => Some("∨"),
        "neg" | "lnot" => Some("¬"),
        "angle" => Some("∠"),
        "degree" => Some("°"),
        "perp" => Some("⊥"),
        "parallel" => Some("∥"),
        "therefore" => Some("∴"),
        "because" => Some("∵"),
        // Greek lowercase
        "alpha" => Some("α"),
        "beta" => Some("β"),
        "gamma" => Some("γ"),
        "delta" => Some("δ"),
        "epsilon" | "varepsilon" => Some("ε"),
        "zeta" => Some("ζ"),
        "eta" => Some("η"),
        "theta" | "vartheta" => Some("θ"),
        "iota" => Some("ι"),
        "kappa" => Some("κ"),
        "lambda" => Some("λ"),
        "mu" => Some("μ"),
        "nu" => Some("ν"),
        "xi" => Some("ξ"),
        "pi" | "varpi" => Some("π"),
        "rho" | "varrho" => Some("ρ"),
        "sigma" | "varsigma" => Some("σ"),
        "tau" => Some("τ"),
        "upsilon" => Some("υ"),
        "phi" | "varphi" => Some("φ"),
        "chi" => Some("χ"),
        "psi" => Some("ψ"),
        "omega" => Some("ω"),
        // Greek uppercase
        "Gamma" => Some("Γ"),
        "Delta" => Some("Δ"),
        "Theta" => Some("Θ"),
        "Lambda" => Some("Λ"),
        "Xi" => Some("Ξ"),
        "Pi" => Some("Π"),
        "Sigma" => Some("Σ"),
        "Upsilon" => Some("Υ"),
        "Phi" => Some("Φ"),
        "Psi" => Some("Ψ"),
        "Omega" => Some("Ω"),
        // Common math functions
        "sin" => Some("sin"),
        "cos" => Some("cos"),
        "tan" => Some("tan"),
        "cot" => Some("cot"),
        "sec" => Some("sec"),
        "csc" => Some("csc"),
        "arcsin" => Some("arcsin"),
        "arccos" => Some("arccos"),
        "arctan" => Some("arctan"),
        "sinh" => Some("sinh"),
        "cosh" => Some("cosh"),
        "tanh" => Some("tanh"),
        "log" => Some("log"),
        "ln" => Some("ln"),
        "lg" => Some("lg"),
        "exp" => Some("exp"),
        "lim" => Some("lim"),
        "max" => Some("max"),
        "min" => Some("min"),
        "sup" => Some("sup"),
        "inf" => Some("inf"),
        "arg" => Some("arg"),
        "det" => Some("det"),
        "gcd" => Some("gcd"),
        "mod" => Some("mod"),
        // Spacing
        "quad" | "qquad" => Some(" "),
        _ => None,
    }
}

fn render_latex_inner(input: &str, out: &mut String, depth: usize) {
    if depth > 16 {
        out.push_str(input);
        return;
    }
    let mut rest = input;
    while !rest.is_empty() {
        if let Some(after_slash) = rest.strip_prefix('\\') {
            if after_slash.is_empty() {
                break;
            }
            let cmd_len = after_slash
                .bytes()
                .take_while(u8::is_ascii_alphabetic)
                .count();
            if cmd_len == 0 {
                let Some(ch) = after_slash.chars().next() else {
                    break;
                };
                rest = &after_slash[ch.len_utf8()..];
                match ch {
                    '\\' => out.push('\n'),
                    ',' | ';' | ':' | ' ' => out.push(' '),
                    '!' => {}
                    '%' | '$' | '_' | '&' | '#' | '{' | '}' | '|' => out.push(ch),
                    _ => out.push(ch),
                }
                continue;
            }

            let cmd = &after_slash[..cmd_len];
            rest = &after_slash[cmd_len..];

            match cmd {
                "frac" | "dfrac" | "tfrac" => {
                    if let Some((num, after_num)) = take_latex_arg(rest)
                        && let Some((den, after_den)) = take_latex_arg(after_num)
                    {
                        out.push('(');
                        render_latex_inner(num.trim(), out, depth + 1);
                        out.push_str(")/(");
                        render_latex_inner(den.trim(), out, depth + 1);
                        out.push(')');
                        rest = after_den;
                        continue;
                    }
                }
                "sqrt" => {
                    let (root_opt, after_opt) = match take_latex_bracket_arg(rest) {
                        Some((n, r)) => (Some(n), r),
                        None => (None, rest),
                    };
                    if let Some((body, after_body)) = take_latex_arg(after_opt) {
                        if let Some(n_raw) = root_opt {
                            let mut n_conv = String::new();
                            render_latex_inner(n_raw.trim(), &mut n_conv, depth + 1);
                            if let Some(sup) = to_unicode_superscript(&n_conv) {
                                out.push_str(&sup);
                            } else {
                                out.push_str(&n_conv);
                            }
                        }
                        out.push_str("√(");
                        render_latex_inner(body.trim(), out, depth + 1);
                        out.push(')');
                        rest = after_body;
                        continue;
                    }
                    out.push('√');
                    continue;
                }
                "text" | "mathrm" | "mathbf" | "mathit" | "mathsf" | "texttt" | "textbf"
                | "textit" | "mathcal" | "mathbb" | "boldsymbol" | "operatorname" | "hat"
                | "bar" | "vec" | "overline" | "underline" => {
                    if let Some((inner, after_inner)) = take_latex_arg(rest) {
                        render_latex_inner(inner, out, depth + 1);
                        rest = after_inner;
                        continue;
                    }
                }
                "left" | "right" => {
                    let trimmed = skip_ascii_spaces(rest);
                    if let Some(after_dot) = trimmed.strip_prefix('.') {
                        rest = after_dot;
                    } else if let Some(after_lbrace) = trimmed.strip_prefix(r"\{") {
                        out.push('{');
                        rest = after_lbrace;
                    } else if let Some(after_rbrace) = trimmed.strip_prefix(r"\}") {
                        out.push('}');
                        rest = after_rbrace;
                    } else {
                        rest = trimmed;
                    }
                    continue;
                }
                "big" | "Big" | "bigg" | "Bigg" | "bigl" | "bigr" | "Bigl" | "Bigr"
                | "displaystyle" | "textstyle" | "limits" | "nolimits" | "notag" => {
                    continue;
                }
                _ => {
                    if let Some(sym) = latex_symbol_replacement(cmd) {
                        out.push_str(sym);
                    } else {
                        out.push_str(cmd);
                    }
                }
            }
            continue;
        }

        if let Some(after_caret) = rest.strip_prefix('^') {
            if let Some((arg, after_arg)) = take_latex_arg(after_caret) {
                let mut conv = String::new();
                render_latex_inner(arg.trim(), &mut conv, depth + 1);
                if let Some(sup) = to_unicode_superscript(&conv) {
                    out.push_str(&sup);
                } else if conv.chars().count() == 1 {
                    out.push('^');
                    out.push_str(&conv);
                } else {
                    out.push_str("^(");
                    out.push_str(&conv);
                    out.push(')');
                }
                rest = after_arg;
                continue;
            }
            out.push('^');
            rest = after_caret;
            continue;
        }

        if let Some(after_under) = rest.strip_prefix('_') {
            if let Some((arg, after_arg)) = take_latex_arg(after_under) {
                let mut conv = String::new();
                render_latex_inner(arg.trim(), &mut conv, depth + 1);
                if let Some(sub) = to_unicode_subscript(&conv) {
                    out.push_str(&sub);
                } else if conv.chars().count() == 1 {
                    out.push('_');
                    out.push_str(&conv);
                } else {
                    out.push_str("_(");
                    out.push_str(&conv);
                    out.push(')');
                }
                rest = after_arg;
                continue;
            }
            out.push('_');
            rest = after_under;
            continue;
        }

        if rest.starts_with('{')
            && let Some((inner, after_brace)) = take_latex_arg(rest)
        {
            render_latex_inner(inner, out, depth + 1);
            rest = after_brace;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        if ch != '}' {
            out.push(ch);
        }
        rest = &rest[ch.len_utf8()..];
    }
}

/// Converts lightweight LaTeX math expressions into readable Unicode text for QQ chat surfaces.
#[must_use]
pub(crate) fn latex_to_unicode(input: &str) -> String {
    let mut raw_out = String::with_capacity(input.len() + 16);
    render_latex_inner(input.trim(), &mut raw_out, 0);

    // Normalize repeated horizontal spaces per line while preserving deliberate line breaks.
    let mut clean = String::with_capacity(raw_out.len());
    for (idx, line) in raw_out.lines().enumerate() {
        if idx > 0 {
            clean.push('\n');
        }
        let mut prev_space = false;
        for ch in line.trim().chars() {
            if ch == ' ' || ch == '\t' {
                if !prev_space {
                    clean.push(' ');
                    prev_space = true;
                }
            } else {
                clean.push(ch);
                prev_space = false;
            }
        }
    }
    clean
}

fn looks_like_inline_math_span(inner: &str) -> bool {
    let trimmed = inner.trim();
    if trimmed.is_empty() || trimmed.len() > 240 || trimmed.contains('\n') {
        return false;
    }
    if trimmed.contains('\\') {
        return true;
    }
    // Reject currency spans like "$10 and $20" or "$5,000 to $10,000".
    if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
        return false;
    }
    let starts_digit = trimmed.chars().next().is_some_and(|c| c.is_ascii_digit());
    if starts_digit && trimmed.contains(' ') {
        return false;
    }
    trimmed.contains('^')
        || trimmed.contains('_')
        || trimmed.contains('=')
        || trimmed.contains('+')
        || trimmed.contains('×')
        || trimmed.contains('÷')
}

/// Scans plain text nodes for any residual `$$...$$`, `$...$`, `\(...\)`, or `\[...\]` LaTeX
/// spans not already parsed as `MdInline::Math` by `pulldown-cmark`, converting them to Unicode.
fn convert_inline_latex_in_text(text: &str) -> Cow<'_, str> {
    if !text.contains('$') && !text.contains(r"\(") && !text.contains(r"\[") {
        return Cow::Borrowed(text);
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    let mut modified = false;

    while i < bytes.len() {
        // Skip escaped `\$`.
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'$') {
            out.push_str(r"\$");
            i += 2;
            continue;
        }

        // `$$...$$`
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'$') {
            if let Some(rel_end) = text[i + 2..].find("$$") {
                let inner = &text[i + 2..i + 2 + rel_end];
                if !inner.trim().is_empty() {
                    out.push_str(&latex_to_unicode(inner));
                    i = i + 2 + rel_end + 2;
                    modified = true;
                    continue;
                }
            }
            out.push_str("$$");
            i += 2;
            continue;
        }

        // `$...$`
        if bytes[i] == b'$' {
            let mut scan = i + 1;
            let mut close_pos = None;
            while scan < bytes.len() {
                if bytes[scan] == b'\n' {
                    break;
                }
                if bytes[scan] == b'\\' && scan + 1 < bytes.len() {
                    scan += 2;
                    continue;
                }
                if bytes[scan] == b'$' {
                    close_pos = Some(scan);
                    break;
                }
                scan += 1;
            }
            if let Some(end) = close_pos {
                let inner = &text[i + 1..end];
                if looks_like_inline_math_span(inner) {
                    out.push_str(&latex_to_unicode(inner));
                    i = end + 1;
                    modified = true;
                    continue;
                }
            }
            out.push('$');
            i += 1;
            continue;
        }

        // `\(...\)`
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'(')
            && let Some(rel_end) = text[i + 2..].find(r"\)")
        {
            let inner = &text[i + 2..i + 2 + rel_end];
            if !inner.trim().is_empty() {
                out.push_str(&latex_to_unicode(inner));
                i = i + 2 + rel_end + 2;
                modified = true;
                continue;
            }
        }

        // `\[...\]`
        if bytes[i] == b'\\'
            && bytes.get(i + 1) == Some(&b'[')
            && let Some(rel_end) = text[i + 2..].find(r"\]")
        {
            let inner = &text[i + 2..i + 2 + rel_end];
            if !inner.trim().is_empty() {
                out.push_str(&latex_to_unicode(inner));
                i = i + 2 + rel_end + 2;
                modified = true;
                continue;
            }
        }

        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }

    if modified {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_markdown_converts_wide_table_and_avoids_quad_stars() {
        let input = r#"
### 2. 预测市场与公开盘口胜率

从去中心化预测市场来看：

| 平台 / 指标 | BLG | T1 |
| :--- | :--- | :--- |
| **Polymarket 预测市场** | **~56.5% – 57%** | **~43% – 43.5%** |
| **机构赔率 / 指数** | **~1.68 – 1.75** | **~2.14 – 2.27** |

---
"#;

        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("> 📊 **平台 / 指标**"));
        assert!(res.contains("> • **Polymarket 预测市场**"));
        assert!(res.contains("> • **机构赔率 / 指数**"));
        assert!(!res.contains("****"));
        assert!(res.contains(">   ▫️ **BLG**:"));
        assert!(res.contains(">   ▫️ **T1**:"));
        assert!(!res.contains("| :---"));
    }

    #[test]
    fn test_format_markdown_compact_table_renders_cjk_box_and_wide_two_col_uses_cards() {
        let compact = "| **参数** | 值 |\n|---|---|\n| **timeout** | `30s` |\n| retries | 3 |\n";
        let res = QqFormatter::format_markdown(compact);
        assert!(res.contains("```"));
        assert!(res.contains('┌'));
        assert!(res.contains("│ timeout │ 30s"));
        assert!(res.contains("│ retries │ 3"));

        let wide_two = "| **参数** | 详细说明与推荐配置 |\n|---|---|\n| **timeout** | `30s`（超过该时间自动触发安全断开） |\n| retries | 3 |\n";
        let wide_res = QqFormatter::format_markdown(wide_two);
        assert!(wide_res.contains("> 📋 **参数 / 详细说明与推荐配置**"));
        assert!(wide_res.contains("> • **timeout**: `30s`"));
        assert!(wide_res.contains("> • **retries**: 3"));
        assert!(!wide_res.contains("****"));
    }

    #[test]
    fn test_format_markdown_preserves_zero_row_tables() {
        let two_col = "| Key | Value |\n| :--- | :--- |\n\nAfter table";
        let res_two = QqFormatter::format_markdown(two_col);
        assert!(res_two.contains("│ Key │ Value │"));
        assert!(res_two.contains("After table"));

        let wide_three = "| Column Alpha | Column Beta | Column Gamma |\n|---|---|---|\n\nDone";
        let res_three = QqFormatter::format_markdown(wide_three);
        assert!(res_three.contains("> 📊 **Column Alpha / Column Beta / Column Gamma**"));
        assert!(res_three.contains("Done"));
    }

    #[test]
    fn test_code_block_preserved_in_markdown_and_plain_text() {
        let input = "```markdown\n| a | b |\n|---|---|\n| 1 | 2 |\n# comment __init__\n```";
        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("| a | b |"));
        assert!(res.contains("|---|---|"));

        let plain = QqFormatter::formatted_to_plain_text(&res);
        assert!(plain.contains("# comment __init__"));
        assert!(!plain.contains("📌"));
    }

    #[test]
    fn test_format_markdown_strips_ansi_and_spinners() {
        let input = "\x1b[32mok\x1b[0m\n  ⠋ loading\nhello";
        let res = QqFormatter::format_markdown(input);
        assert_eq!(res, "ok\nloading\nhello\n");
    }

    #[test]
    fn test_to_plain_text_hierarchy_links_tasks_and_quotes() {
        let input = "# 一级\n## 二级\n### 三级\n#### 四级 **标题**\n> **引用** `__init__` `code` 文本。\n- [x] Done\n- [ ] Todo\n\n查看 [官方指南](https://example.com) 或 `a | b`。";
        let plain = QqFormatter::to_plain_text(input);
        assert!(plain.contains("📌 【一级】\n━━━━━━━━━━━━━━━"));
        assert!(plain.contains("📌 【二级】\n───────────────"));
        assert!(plain.contains("📌 【三级】"));
        assert!(plain.contains("▸ 【四级 标题】"));
        assert!(plain.contains("│ 引用 __init__ code 文本。"));
        assert!(plain.contains("☑ Done"));
        assert!(plain.contains("☐ Todo"));
        assert!(plain.contains("官方指南 (https://example.com)"));
        assert!(!plain.contains("[官方指南](https://example.com)"));
        assert!(!plain.contains("**"));
        assert!(!plain.contains('`'));
    }

    #[test]
    fn test_table_with_inline_code_pipe_and_multi_bold_key() {
        let input = "| **A** vs **B** | 说明与示例（超长列触发卡片渲染模式） |\n|---|---|\n| `x | y` | bit OR |\n";
        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("> • **`x | y`**: bit OR"));
        assert!(!res.contains("****"));

        let multi_bold = "| **A** vs **B** | 说明与示例（超长列触发卡片渲染模式） |\n|---|---|\n| **A** vs **B** | ok |\n";
        let res_bold = QqFormatter::format_markdown(multi_bold);
        assert!(res_bold.contains("> • **A vs B**: ok"));
        assert!(!res_bold.contains("****"));
    }

    #[test]
    fn test_qq_cjk_compact_table_alignment_and_plain_text_parity() {
        let md = "\
# 性能对比

| 节点 | 延迟 | 状态 |
|---|---:|---|
| 东京 | 12ms | ✅ |
| 伦敦 | 95ms | OK |

> [!TIP]
> 建议开启 `auto` 模式并查看 [Wiki](https://en.wikipedia.org/wiki/Rust_(programming_language))。";

        let qq_md = QqFormatter::format_markdown(md);
        assert!(qq_md.contains("# 性能对比"));
        assert!(qq_md.contains("┌"));
        assert!(qq_md.contains("└"));
        assert!(qq_md.contains("💡 **Tip**"));
        assert!(
            qq_md.contains("[Wiki](https://en.wikipedia.org/wiki/Rust_(programming_language))")
        );

        let qq_plain = QqFormatter::formatted_to_plain_text(&qq_md);
        assert!(qq_plain.contains("📌 【性能对比】"));
        assert!(qq_plain.contains("┌"));
        assert!(qq_plain.contains("│ 东京 │ 12ms │ ✅   │"));
        assert!(qq_plain.contains("│ 💡 Tip"));
        assert!(
            qq_plain.contains("Wiki (https://en.wikipedia.org/wiki/Rust_(programming_language))")
        );
        assert!(!qq_plain.contains("```"));
    }

    #[test]
    fn test_qq_split_markdown_chunks_preserves_compact_table_and_code_fences() {
        let rows: String = (0..20)
            .map(|i| format!("| item_{i:02} | {i} |\n"))
            .collect();
        let md = format!("| Name | Val |\n|---|---|\n{rows}");
        let formatted = QqFormatter::format_markdown(&md);
        let chunks = QqFormatter::split_markdown_chunks(&formatted, 200, 300);
        assert!(chunks.len() > 1);
        for (idx, chunk) in chunks.iter().enumerate() {
            assert!(chunk.chars().count() <= 200);
            assert!(chunk.len() <= 300);
            assert!(
                chunk.starts_with("```"),
                "chunk {idx} should reopen fence: {chunk:?}"
            );
            assert!(
                chunk.trim_end().ends_with("```"),
                "chunk {idx} should close fence: {chunk:?}"
            );
        }
    }

    #[test]
    fn test_latex_to_unicode_converts_fractions_operators_greek_and_superscripts() {
        assert_eq!(latex_to_unicode(r"\frac{a}{b}"), "(a)/(b)");
        assert_eq!(
            latex_to_unicode(r"\frac{\sqrt{a^2 + b^2}}{2}"),
            "(√(a² + b²))/(2)"
        );
        assert_eq!(
            latex_to_unicode(r"x \approx 3.14 \times 10^3 \div 2"),
            "x ≈ 3.14 × 10³ ÷ 2"
        );
        assert_eq!(
            latex_to_unicode(r"\left(a \le b \ge c \ne d \pm e \cdot \infty\right)"),
            "(a ≤ b ≥ c ≠ d ± e · ∞)"
        );
        assert_eq!(
            latex_to_unicode(r"\alpha + \beta \rightarrow \gamma \to \omega"),
            "α + β → γ → ω"
        );
        assert_eq!(
            latex_to_unicode(r"\text{速度} = \frac{\mathrm{距离}}{\mathrm{时间}}"),
            "速度 = (距离)/(时间)"
        );
        assert_eq!(latex_to_unicode(r"\sqrt[3]{27} = 3"), "³√(27) = 3");
    }

    #[test]
    fn test_format_markdown_and_plain_text_render_math_without_raw_latex() {
        let md = "公式 $\\frac{1}{2} \\approx 0.5$，二次方 $x^2 + y^3$，范围 $\\alpha \\le x \\le \\infty$。\n\n$$\n\\frac{\\sqrt{x^2 + 1}}{2} \\to \\pi\n$$\n\n价格在 $10 and $20 之间，且残留 $ \\frac{a}{b} $ 也会转换。";
        let qq_md = QqFormatter::format_markdown(md);
        assert!(qq_md.contains("(1)/(2) ≈ 0.5"));
        assert!(qq_md.contains("x² + y³"));
        assert!(qq_md.contains("α ≤ x ≤ ∞"));
        assert!(qq_md.contains("(√(x² + 1))/(2) → π"));
        assert!(qq_md.contains("$10 and $20"));
        assert!(qq_md.contains("(a)/(b)"));
        assert!(!qq_md.contains(r"\frac"));
        assert!(!qq_md.contains(r"\approx"));
        assert!(!qq_md.contains(r"\sqrt"));

        let qq_plain = QqFormatter::formatted_to_plain_text(&qq_md);
        assert!(qq_plain.contains("(1)/(2) ≈ 0.5"));
        assert!(qq_plain.contains("(√(x² + 1))/(2) → π"));
        assert!(qq_plain.contains("$10 and $20"));
        assert!(!qq_plain.contains(r"\frac"));
    }
}
