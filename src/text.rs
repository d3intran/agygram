//! Shared text sanitization, HTML escaping/stripping, truncation, and attachment helpers.
//!
//! Decouples Telegram, QQ, quota, TTS, and executor modules from importing
//! `crate::bot::formatter` or `crate::bot::media`.

use std::borrow::Cow;
use std::fmt::Display;

/// Escapes HTML entities `& < > " '` with zero heap allocations on clean strings.
#[must_use]
pub fn escape_html(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return Cow::Borrowed(s);
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
    Cow::Owned(escaped)
}

/// Truncates `s` to at most `max_chars` characters, appending `...` when truncated.
/// Never splits a multi-byte UTF-8 code point.
#[inline]
#[must_use]
pub fn truncate_chars(s: &str, max_chars: usize) -> Cow<'_, str> {
    match s.char_indices().nth(max_chars) {
        Some((cut, _)) => Cow::Owned(format!("{}...", &s[..cut])),
        None => Cow::Borrowed(s),
    }
}

/// Returns a prefix slice of at most `max_chars` characters on a valid UTF-8 boundary.
#[inline]
#[must_use]
pub fn take_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((cut, _)) => &s[..cut],
        None => s,
    }
}

#[inline]
fn is_braille_spinner(c: char) -> bool {
    matches!(c, '\u{2800}'..='\u{280F}')
}

/// Strips ANSI escape sequences (`\x1b[...m`) and Braille spinner frames while preserving
/// indentation on non-spinner lines (such as fenced code blocks).
#[must_use]
pub fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut scratch_buf = String::new();

    for line in input.lines() {
        let has_escape = line.as_bytes().contains(&0x1B);
        let has_spinner = line.chars().any(is_braille_spinner);

        if !has_escape && !has_spinner {
            result.push_str(line);
            result.push('\n');
            continue;
        }

        scratch_buf.clear();
        let mut in_escape = false;

        for c in line.chars() {
            if c == '\x1B' {
                in_escape = true;
            } else if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else if !is_braille_spinner(c) {
                scratch_buf.push(c);
            }
        }

        let kept = if has_spinner {
            scratch_buf.trim_start()
        } else {
            &scratch_buf[..]
        };
        result.push_str(kept);
        result.push('\n');
    }

    let trimmed_len = result.trim_end().len();
    result.truncate(trimmed_len);
    result
}

/// Checks whether the slice immediately following `<` forms a complete HTML tag on the same line,
/// returning the byte index of the closing `>` relative to `rest`.
///
/// Unmatched `<` comparisons such as `x < 5` or `a<y && z>w` return `None` so callers keep the
/// literal `<` instead of dropping the rest of the line.
#[must_use]
pub fn find_html_tag_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let is_closing = bytes.first() == Some(&b'/');
    let mut i = usize::from(is_closing);

    if !bytes.get(i).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }

    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'-' | b'_')) {
        i += 1;
    }

    if i >= bytes.len() {
        return None;
    }

    if is_closing {
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        return (bytes.get(i) == Some(&b'>')).then_some(i);
    }

    if bytes[i] == b'>' {
        return Some(i);
    }
    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'>') {
        return Some(i + 1);
    }
    if !matches!(bytes[i], b' ' | b'\t') {
        return None;
    }

    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                } else if matches!(b, b'\n' | b'\r') {
                    return None;
                }
            }
            None => match b {
                b'>' => return Some(i),
                b'"' | b'\'' => quote = Some(b),
                b'\n' | b'\r' | b'<' | b'&' | b'|' | b';' => return None,
                _ => {}
            },
        }
        i += 1;
    }

    None
}

/// Strips HTML tags and decodes common HTML entities (`&lt;`, `&gt;`, `&amp;`, `&quot;`, `&#39;`,
/// `&apos;`) in a single pass without intermediate allocations.
///
/// Unmatched `<` tokens such as `x < 5` are preserved literally.
#[must_use]
pub fn strip_html_tags(input: &str) -> String {
    if !input.bytes().any(|b| b == b'<' || b == b'&') {
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        let Some(rel) = input[i..].find(['<', '&']) else {
            out.push_str(&input[i..]);
            break;
        };

        if rel > 0 {
            out.push_str(&input[i..i + rel]);
            i += rel;
        }

        if bytes[i] == b'<' {
            if let Some(end) = find_html_tag_end(&input[i + 1..]) {
                i = i + 1 + end + 1;
                continue;
            }
            out.push('<');
            i += 1;
        } else {
            let rest = &input[i..];
            if rest.starts_with("&lt;") {
                out.push('<');
                i += 4;
            } else if rest.starts_with("&gt;") {
                out.push('>');
                i += 4;
            } else if rest.starts_with("&amp;") {
                out.push('&');
                i += 5;
            } else if rest.starts_with("&quot;") {
                out.push('"');
                i += 6;
            } else if rest.starts_with("&#39;") {
                out.push('\'');
                i += 5;
            } else if rest.starts_with("&apos;") {
                out.push('\'');
                i += 6;
            } else {
                out.push('&');
                i += 1;
            }
        }
    }

    out
}

/// Sanitizes an untrusted attachment filename so joining it onto a scratch directory stays a
/// single relative path component prefixed by `message_id`.
#[must_use]
pub fn safe_attachment_name(
    message_id: impl Display,
    hint: Option<&str>,
    fallback: &str,
) -> String {
    let Some(hint_str) = hint else {
        return format!("m{message_id}-{fallback}");
    };
    let flattened: String = hint_str
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = flattened.trim().trim_matches(['.', ' ']);
    let base = if cleaned.is_empty() {
        fallback
    } else {
        take_chars(cleaned, 60)
    };
    format!("m{message_id}-{base}")
}

#[inline]
fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Returns `true` if a file name or path looks like a dotfile, key, or credential store that
/// should never be auto-delivered without an explicit `/file` request.
#[must_use]
pub fn looks_sensitive(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    const NEEDLES: &[&[u8]] = &[
        b"id_rsa",
        b"id_ed25519",
        b"id_ecdsa",
        b".pem",
        b".p12",
        b".pfx",
        b".key",
        b"netrc",
        b"credentials",
        b"secret",
        b"passwd",
        b"shadow",
        b"api_key",
        b"apikey",
        b"/.git/",
    ];
    let bytes = name.as_bytes();
    NEEDLES
        .iter()
        .any(|needle| contains_ascii_case_insensitive(bytes, needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn escape_html_zero_alloc_when_clean() {
        let clean = "plain text 123";
        assert!(matches!(escape_html(clean), Cow::Borrowed(_)));
        assert_eq!(
            escape_html("a < b & c > \"d\" 'e'"),
            "a &lt; b &amp; c &gt; &quot;d&quot; &#39;e&#39;"
        );
    }

    #[test]
    fn strip_html_tags_handles_entities_and_unmatched_lt() {
        assert_eq!(
            strip_html_tags("<b>Hello</b> <code>world &amp; test</code>"),
            "Hello world & test"
        );
        assert_eq!(
            strip_html_tags("&lt;code&gt;let x = 1;&lt;/code&gt;"),
            "<code>let x = 1;</code>"
        );
        // Must NOT drop the rest of the string after `<`!
        assert_eq!(
            strip_html_tags("if x < 5 and y > 2 then <b>ok</b>"),
            "if x < 5 and y > 2 then ok"
        );
        assert_eq!(
            strip_html_tags("compare a<y && z>w with <a href=\"https://x?a=1&b=2\">link</a>"),
            "compare a<y && z>w with link"
        );
        assert_eq!(strip_html_tags("trailing <unclosed"), "trailing <unclosed");
        assert_eq!(strip_html_tags("&amp;lt;"), "&lt;");
    }

    #[test]
    fn truncate_and_take_chars_respect_utf8() {
        assert_eq!(truncate_chars("中文测试内容", 2), "中文...");
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(take_chars("日本語テスト", 3), "日本語");
    }

    #[test]
    fn strip_ansi_codes_preserves_indentation_and_cleans_spinners() {
        assert_eq!(
            strip_ansi_codes("\x1b[31mred\x1b[0m\n  ⠋ working\n    indented"),
            "red\nworking\n    indented"
        );
    }

    #[test]
    fn safe_attachment_name_prevents_traversal_and_clips_utf8() {
        for hostile in ["../../etc/passwd", "/home/u/.bashrc", "\0.env", "..."] {
            let name = safe_attachment_name(7, Some(hostile), "fallback.bin");
            let p = Path::new(&name);
            assert!(p.is_relative() && p.components().count() == 1);
            assert!(!name.starts_with("m7-."));
        }
        let cjk = safe_attachment_name(1, Some(&"中".repeat(100)), "f.bin");
        assert!(cjk.chars().count() <= 63);
    }

    #[test]
    fn looks_sensitive_matches_case_insensitively() {
        assert!(looks_sensitive(".env"));
        assert!(looks_sensitive("ID_RSA"));
        assert!(looks_sensitive("server.PEM"));
        assert!(looks_sensitive("/repo/.GIT/config"));
        assert!(!looks_sensitive("chart.png"));
    }
}
