use crate::text::{
    self, MdBlock, MdInline, MdTable, inlines_to_plain_text, parse_markdown,
    render_compact_box_table, separate_blocks,
};
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
            MdInline::Text(s) => out.push_str(s),
            MdInline::Code(s) | MdInline::Math(s) => push_qq_inline_code(s, out),
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
                push_qq_fenced_block("", math, out);
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
            MdInline::Text(s) | MdInline::Code(s) | MdInline::Math(s) => out.push_str(s),
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
            MdBlock::CodeBlock { code, .. } | MdBlock::MathBlock(code) => {
                out.push_str(code);
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
}
