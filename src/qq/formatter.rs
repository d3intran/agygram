use std::borrow::Cow;
use std::fmt::Write;

/// Formatter for QQ Bot messages, providing optimized Markdown rendering
/// and graceful fallback for QQ client limitations (such as table formatting).
pub(crate) struct QqFormatter;

impl QqFormatter {
    /// Formats raw model markdown output into QQ-optimized markdown.
    ///
    /// Strips terminal ANSI/spinner artifacts and converts Markdown tables into
    /// structured quote cards, because official QQ Markdown does not natively
    /// render `| ... |` table syntax.
    #[must_use]
    pub(crate) fn format_markdown(raw: &str) -> String {
        let cleaned: Cow<'_, str> = if needs_ansi_strip(raw) {
            Cow::Owned(crate::text::strip_ansi_codes(raw))
        } else {
            Cow::Borrowed(raw)
        };

        let mut out = String::with_capacity(cleaned.len() + 256);
        let mut lines = cleaned.lines().peekable();
        let mut in_code_block = false;
        let mut headers_buf: Vec<&str> = Vec::with_capacity(8);
        let mut row_buf: Vec<&str> = Vec::with_capacity(8);

        while let Some(line) = lines.next() {
            let trimmed = line.trim();

            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                out.push_str(line);
                out.push('\n');
                continue;
            }

            if in_code_block {
                out.push_str(line);
                out.push('\n');
                continue;
            }

            // A Markdown table starts with a pipe-containing header followed by a separator row.
            if Self::is_table_header(trimmed, lines.peek().copied()) {
                let _separator = lines.next();
                Self::parse_row_into(trimmed, &mut headers_buf);

                if !headers_buf.is_empty() {
                    let has_rows = lines
                        .peek()
                        .is_some_and(|next| Self::is_table_data_row(next.trim()));

                    Self::render_table_header(&mut out, &headers_buf, has_rows);

                    while let Some(next_line) = lines.peek() {
                        let next_trimmed = next_line.trim();
                        if !Self::is_table_data_row(next_trimmed) {
                            break;
                        }
                        lines.next();
                        Self::parse_row_into(next_trimmed, &mut row_buf);
                        Self::render_table_row(&mut out, &headers_buf, &row_buf);
                    }
                    out.push('\n');
                }
                continue;
            }

            out.push_str(line);
            out.push('\n');
        }

        while out.ends_with("\n\n\n") {
            out.pop();
        }

        out
    }

    /// Converts *already-formatted* QQ markdown (output of [`Self::format_markdown`]) into
    /// readable plain text without re-parsing tables or re-running `format_markdown`.
    #[must_use]
    pub(crate) fn formatted_to_plain_text(formatted_md: &str) -> String {
        let mut out = String::with_capacity(formatted_md.len());
        let mut in_code_block = false;

        for line in formatted_md.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }

            if in_code_block {
                out.push_str(line);
                out.push('\n');
                continue;
            }

            if let Some(title) = Self::parse_heading(trimmed) {
                out.push_str("\n📌 【");
                Self::push_stripped_inline_markdown(&mut out, title);
                out.push_str("】\n");
            } else if let Some(content) = trimmed
                .strip_prefix("> ")
                .or_else(|| (trimmed == ">").then_some(""))
            {
                Self::push_stripped_inline_markdown(&mut out, content);
                out.push('\n');
            } else {
                Self::push_stripped_inline_markdown(&mut out, line);
                out.push('\n');
            }
        }

        out
    }

    /// Formats raw model markdown and converts it to clean plain text.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn to_plain_text(raw: &str) -> String {
        let md = Self::format_markdown(raw);
        Self::formatted_to_plain_text(&md)
    }

    #[inline]
    fn is_table_header(line: &str, next_line: Option<&str>) -> bool {
        if !line.contains('|') {
            return false;
        }
        let Some(next) = next_line else {
            return false;
        };
        let next_trim = next.trim();
        next_trim.contains('|')
            && next_trim.contains('-')
            && next_trim
                .bytes()
                .all(|b| matches!(b, b'|' | b'-' | b':' | b' ' | b'\t'))
    }

    #[inline]
    fn is_table_data_row(trimmed: &str) -> bool {
        !trimmed.starts_with("```") && !trimmed.is_empty() && trimmed.contains('|')
    }

    #[inline]
    fn parse_row_into<'a>(line: &'a str, buf: &mut Vec<&'a str>) {
        buf.clear();
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        if trimmed.is_empty() {
            return;
        }
        buf.extend(trimmed.split('|').map(str::trim));
    }

    fn render_table_header(out: &mut String, headers: &[&str], has_rows: bool) {
        if headers.len() == 2 {
            let h0 = Self::strip_surrounding_formatting(headers[0]);
            let h1 = Self::strip_surrounding_formatting(headers[1]);
            let _ = writeln!(out, "> 📋 **{h0} / {h1}**");
            return;
        }

        if !has_rows && headers.len() > 1 {
            out.push_str("> 📊 **");
            for (idx, h) in headers.iter().enumerate() {
                if idx > 0 {
                    out.push_str(" / ");
                }
                out.push_str(Self::strip_surrounding_formatting(h));
            }
            out.push_str("**\n");
            return;
        }

        let h0 = Self::strip_surrounding_formatting(headers[0]);
        let _ = writeln!(out, "> 📊 **{h0}**");
    }

    fn render_table_row(out: &mut String, headers: &[&str], row: &[&str]) {
        if row.is_empty() {
            return;
        }

        if headers.len() == 2 {
            let k = Self::strip_surrounding_formatting(row.first().copied().unwrap_or_default());
            let v = row.get(1).copied().unwrap_or_default();
            if !k.is_empty() || !v.is_empty() {
                out.push_str("> • ");
                Self::push_bold_cell(out, k);
                out.push_str(": ");
                out.push_str(v);
                out.push('\n');
            }
            return;
        }

        let first = Self::strip_surrounding_formatting(row.first().copied().unwrap_or_default());
        out.push_str("> • ");
        Self::push_bold_cell(out, first);
        out.push('\n');

        for (idx, raw_header) in headers.iter().enumerate().skip(1) {
            if let Some(val) = row.get(idx)
                && !val.is_empty()
            {
                let header = Self::strip_surrounding_formatting(raw_header);
                let _ = writeln!(out, ">   ▫️ **{header}**: {val}");
            }
        }
    }

    #[inline]
    fn push_bold_cell(out: &mut String, cell: &str) {
        if cell.is_empty() || cell.contains("**") {
            out.push_str(cell);
        } else {
            out.push_str("**");
            out.push_str(cell);
            out.push_str("**");
        }
    }

    #[inline]
    fn strip_surrounding_formatting(s: &str) -> &str {
        let trimmed = s.trim();
        if let Some(inner) = trimmed
            .strip_prefix("**")
            .and_then(|t| t.strip_suffix("**"))
            && !inner.contains("**")
        {
            return inner.trim();
        }
        if let Some(inner) = trimmed
            .strip_prefix("__")
            .and_then(|t| t.strip_suffix("__"))
            && !inner.contains("__")
        {
            return inner.trim();
        }
        trimmed
    }

    #[inline]
    fn parse_heading(trimmed: &str) -> Option<&str> {
        let hashes = trimmed
            .as_bytes()
            .iter()
            .take_while(|&&b| b == b'#')
            .count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            Some(trimmed[hashes + 1..].trim())
        } else {
            None
        }
    }

    fn push_stripped_inline_markdown(out: &mut String, s: &str) {
        let bytes = s.as_bytes();
        let mut start = 0usize;
        let mut i = 0usize;

        while i < bytes.len() {
            match bytes[i] {
                b'`' => {
                    out.push_str(&s[start..i]);
                    i += 1;
                    start = i;
                }
                b'*' if bytes.get(i + 1) == Some(&b'*') => {
                    out.push_str(&s[start..i]);
                    i += 2;
                    start = i;
                }
                b'_' if bytes.get(i + 1) == Some(&b'_') => {
                    out.push_str(&s[start..i]);
                    i += 2;
                    start = i;
                }
                _ => {
                    i += 1;
                }
            }
        }

        out.push_str(&s[start..]);
    }
}

#[inline]
fn needs_ansi_strip(raw: &str) -> bool {
    raw.as_bytes().contains(&0x1B) || raw.chars().any(|c| matches!(c, '\u{2800}'..='\u{280F}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_markdown_converts_table_and_avoids_quad_stars() {
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
    fn test_format_markdown_two_column_table_and_bold_keys() {
        let input = "| **参数** | 值 |\n|---|---|\n| **timeout** | `30s` |\n| retries | 3 |\n";
        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("> 📋 **参数 / 值**"));
        assert!(res.contains("> • **timeout**: `30s`"));
        assert!(res.contains("> • **retries**: 3"));
        assert!(!res.contains("****"));
    }

    #[test]
    fn test_format_markdown_preserves_zero_row_tables() {
        let two_col = "| Key | Value |\n| :--- | :--- |\n\nAfter table";
        let res_two = QqFormatter::format_markdown(two_col);
        assert!(res_two.contains("> 📋 **Key / Value**"));
        assert!(res_two.contains("After table"));

        let three_col = "| A | B | C |\n|---|---|---|\n\nDone";
        let res_three = QqFormatter::format_markdown(three_col);
        assert!(res_three.contains("> 📊 **A / B / C**"));
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
    fn test_to_plain_text_supports_h1_through_h6_and_inline_cleanup() {
        let input = "# 一级\n### 三级\n#### 四级 **标题**\n##### 五级\n###### **六级**\n> **引用** `__code__` 文本。";
        let plain = QqFormatter::to_plain_text(input);
        assert!(plain.contains("📌 【一级】"));
        assert!(plain.contains("📌 【三级】"));
        assert!(plain.contains("📌 【四级 标题】"));
        assert!(plain.contains("📌 【五级】"));
        assert!(plain.contains("📌 【六级】"));
        assert!(plain.contains("引用 code 文本。"));
        assert!(!plain.contains("**"));
        assert!(!plain.contains("__"));
        assert!(!plain.contains('`'));
    }

    #[test]
    fn test_strip_surrounding_formatting_keeps_multi_bold_intact() {
        assert_eq!(
            QqFormatter::strip_surrounding_formatting(" **bold** "),
            "bold"
        );
        assert_eq!(
            QqFormatter::strip_surrounding_formatting("**A** vs **B**"),
            "**A** vs **B**"
        );
        let input = "| Col | Desc |\n|---|---|\n| **A** vs **B** | ok |";
        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("> • **A** vs **B**: ok"));
        assert!(!res.contains("****"));
    }
}
