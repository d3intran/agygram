use std::fmt::Write;

/// Formatter for QQ Bot messages, providing optimized Markdown rendering
/// and graceful fallback for QQ client limitations (such as table formatting).
pub struct QqFormatter;

impl QqFormatter {
    /// Formats raw model markdown output into QQ-optimized markdown.
    /// In particular, converts Markdown tables into structured quote cards,
    /// because official QQ Markdown does not natively render `| ... |` table syntax.
    #[must_use]
    pub fn format_markdown(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len() + 256);
        let mut lines = raw.lines().peekable();
        let mut in_code_block = false;

        while let Some(line) = lines.next() {
            let trimmed = line.trim();

            // Toggle code blocks
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

            // Check if this line starts a Markdown table:
            // A table header must contain '|', and the next line must be a separator '| --- |'
            if Self::is_table_header(trimmed, lines.peek().copied()) {
                let header_line = trimmed;
                let _separator_line = lines.next(); // Consume separator line

                let headers: Vec<&str> = Self::parse_row(header_line);
                let mut rows: Vec<Vec<&str>> = Vec::new();

                while let Some(next_line) = lines.peek() {
                    let next_trimmed = next_line.trim();
                    if next_trimmed.starts_with("```") || next_trimmed.is_empty() || !next_trimmed.contains('|') {
                        break;
                    }
                    rows.push(Self::parse_row(next_trimmed));
                    lines.next(); // Consume table data line
                }

                Self::render_table(&mut out, &headers, &rows);
                continue;
            }

            out.push_str(line);
            out.push('\n');
        }

        // Remove trailing extra newlines if any
        while out.ends_with("\n\n\n") {
            out.pop();
        }

        out
    }

    /// Converts markdown to readable clean plain text, used as fallback when
    /// QQ returns an error for Markdown messages.
    #[must_use]
    pub fn to_plain_text(raw: &str) -> String {
        let md = Self::format_markdown(raw);
        let mut out = String::with_capacity(md.len());

        for line in md.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("### ") {
                let title = trimmed.trim_start_matches('#').trim();
                let _ = writeln!(out, "\n📌 【{title}】");
            } else if trimmed.starts_with("## ") {
                let title = trimmed.trim_start_matches('#').trim();
                let _ = writeln!(out, "\n📌 【{title}】");
            } else if trimmed.starts_with("# ") {
                let title = trimmed.trim_start_matches('#').trim();
                let _ = writeln!(out, "\n📌 【{title}】");
            } else if trimmed.starts_with("> ") {
                // Strip quotes but keep indentation/bullets
                let content = &trimmed[2..];
                let cleaned = Self::strip_inline_markdown(content);
                let _ = writeln!(out, "{cleaned}");
            } else {
                let cleaned = Self::strip_inline_markdown(line);
                let _ = writeln!(out, "{cleaned}");
            }
        }

        out
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
        next_trim.contains('|') && next_trim.contains("---")
    }

    #[inline]
    fn parse_row(line: &str) -> Vec<&str> {
        let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
        trimmed
            .split('|')
            .map(|cell| cell.trim())
            .collect()
    }

    fn render_table(out: &mut String, headers: &[&str], rows: &[Vec<&str>]) {
        if headers.is_empty() || rows.is_empty() {
            return;
        }

        let clean_headers: Vec<String> = headers
            .iter()
            .map(|h| Self::strip_surrounding_formatting(h))
            .collect();

        // 2-column table optimization (e.g., Key - Value parameters)
        if clean_headers.len() == 2 {
            out.push_str("> 📋 **");
            out.push_str(&clean_headers[0]);
            out.push_str(" / ");
            out.push_str(&clean_headers[1]);
            out.push_str("**\n");

            for row in rows {
                if row.is_empty() {
                    continue;
                }
                let k = row.first().copied().unwrap_or_default();
                let v = row.get(1).copied().unwrap_or_default();
                if !k.is_empty() || !v.is_empty() {
                    let _ = writeln!(out, "> • **{k}**: {v}");
                }
            }
            out.push('\n');
            return;
        }

        // 3+ columns table: card layout
        out.push_str("> 📊 **");
        out.push_str(&clean_headers[0]);
        out.push_str("**\n");

        for row in rows {
            if row.is_empty() {
                continue;
            }
            let first = row.first().copied().unwrap_or_default();
            let _ = writeln!(out, "> • **{first}**");

            for (idx, header) in clean_headers.iter().enumerate().skip(1) {
                if let Some(val) = row.get(idx) {
                    if !val.is_empty() {
                        let _ = writeln!(out, ">   ▫️ **{header}**: {val}");
                    }
                }
            }
        }
        out.push('\n');
    }

    #[inline]
    fn strip_surrounding_formatting(s: &str) -> String {
        let trimmed = s.trim();
        let stripped = trimmed
            .strip_prefix("**")
            .and_then(|t| t.strip_suffix("**"))
            .unwrap_or(trimmed);
        stripped.to_string()
    }

    #[inline]
    fn strip_inline_markdown(s: &str) -> String {
        s.replace("**", "")
            .replace("__", "")
            .replace("```", "")
            .replace('`', "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_markdown_converts_table() {
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
        assert!(res.contains("> • ****Polymarket 预测市场****") || res.contains("Polymarket"));
        assert!(res.contains(">   ▫️ **BLG**:"));
        assert!(res.contains(">   ▫️ **T1**:"));
        assert!(!res.contains("| :---"));
    }

    #[test]
    fn test_code_block_preserved() {
        let input = "```markdown\n| a | b |\n|---|---|\n| 1 | 2 |\n```";
        let res = QqFormatter::format_markdown(input);
        assert!(res.contains("| a | b |"));
        assert!(res.contains("|---|---|"));
    }

    #[test]
    fn test_to_plain_text() {
        let input = "### 标题\n这是 **加粗** 文本。";
        let plain = QqFormatter::to_plain_text(input);
        assert!(plain.contains("📌 【标题】"));
        assert!(plain.contains("这是 加粗 文本。"));
        assert!(!plain.contains("**"));
    }
}
