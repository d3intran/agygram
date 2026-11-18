//! Shared text sanitization, Unicode display width, HTML escaping/stripping,
//! unified Markdown AST parsing, responsive table layout helpers, and truncation utilities.
//!
//! Decouples Telegram, QQ, quota, TTS, and executor modules from importing
//! `crate::bot::formatter` or `crate::bot::media`.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use std::borrow::Cow;
use std::fmt::Display;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Maximum monospace display columns for rendering a table as a compact box-drawing grid
/// instead of switching to a mobile-friendly vertical card/record layout.
pub(crate) const COMPACT_TABLE_MAX_WIDTH: usize = 38;

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
    push_escaped_html(s, &mut escaped);
    Cow::Owned(escaped)
}

/// Appends HTML-escaped `s` directly into `out`.
#[inline]
pub(crate) fn push_escaped_html(s: &str, out: &mut String) {
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
pub(crate) fn is_braille_spinner(c: char) -> bool {
    matches!(c, '\u{2800}'..='\u{280F}')
}

/// Returns `true` if `s` contains ANSI escape sequences, bare carriage returns, or Braille spinners.
#[inline]
#[must_use]
pub(crate) fn needs_ansi_cleanup(s: &str) -> bool {
    s.as_bytes().contains(&0x1B)
        || s.as_bytes().contains(&b'\r')
        || s.chars().any(is_braille_spinner)
}

/// Strips ANSI escape sequences (CSI/OSC), carriage-return overwrites, and Braille spinner
/// frames while preserving indentation on non-spinner lines (such as fenced code blocks).
#[must_use]
pub fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut scratch_buf = String::new();

    for line in input.lines() {
        // Bare '\r' (carriage-return progress overwrite within a line): keep only final frame.
        let effective_line = match line.rfind('\r') {
            Some(pos) => &line[pos + 1..],
            None => line,
        };
        let has_escape = effective_line.as_bytes().contains(&0x1B);
        let has_spinner = effective_line.chars().any(is_braille_spinner);

        if !has_escape && !has_spinner {
            result.push_str(effective_line);
            result.push('\n');
            continue;
        }

        scratch_buf.clear();
        let bytes = effective_line.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] == 0x1B {
                i += 1;
                if i < bytes.len() && bytes[i] == b'[' {
                    // CSI sequence: ESC [ ... final_byte (@..=~)
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1;
                    }
                } else if i < bytes.len() && bytes[i] == b']' {
                    // OSC sequence: ESC ] ... (BEL or ST)
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1B && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                } else if i < bytes.len() {
                    i += 1;
                }
                continue;
            }

            let Some(ch) = effective_line[i..].chars().next() else {
                break;
            };
            if !is_braille_spinner(ch) {
                scratch_buf.push(ch);
            }
            i += ch.len_utf8();
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

/// Returns monospace display width (in terminal columns) accounting for CJK double-width glyphs,
/// fullwidth punctuation, combining marks, and emoji ZWJ/VS16 sequences.
#[must_use]
pub fn display_width(s: &str) -> usize {
    if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_control()) {
        return s.len();
    }
    if !s.chars().any(char::is_control) {
        return UnicodeWidthStr::width(s);
    }
    let cleaned: String = s.chars().filter(|c| !c.is_control()).collect();
    UnicodeWidthStr::width(cleaned.as_str())
}

#[allow(dead_code)]
#[inline]
fn is_cluster_continuation(c: char) -> bool {
    matches!(c, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}')
        || (!c.is_control() && UnicodeWidthChar::width(c) == Some(0))
}

/// Cuts `s` to at most `max_cells` monospace columns without splitting UTF-8 codepoints,
/// East Asian wide characters, combining marks, or emoji ZWJ/VS16 clusters.
/// Returns `(prefix_slice, used_cells)`.
#[allow(dead_code)]
#[must_use]
pub fn truncate_to_width(s: &str, max_cells: usize) -> (&str, usize) {
    if s.is_ascii() && !s.bytes().any(|b| b.is_ascii_control()) {
        let cut = s.len().min(max_cells);
        return (&s[..cut], cut);
    }

    if !s.contains('\u{200D}') && !s.contains('\u{FE0F}') && !s.contains('\u{FE0E}') {
        let mut used = 0usize;
        for (idx, c) in s.char_indices() {
            let w = if c.is_control() {
                0
            } else {
                UnicodeWidthChar::width(c).unwrap_or(0)
            };
            if w > 0 && used + w > max_cells {
                return (&s[..idx], used);
            }
            used += w;
        }
        return (s, used);
    }

    // Cluster-aware cut for ZWJ / variation selector emoji sequences.
    let mut used = 0usize;
    let mut byte_pos = 0usize;
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let cluster_start = chars[i].0;
        i += 1;
        while i < chars.len() {
            let prev = chars[i - 1].1;
            let cur = chars[i].1;
            if prev == '\u{200D}' || is_cluster_continuation(cur) {
                i += 1;
            } else {
                break;
            }
        }
        let cluster_end = chars.get(i).map_or(s.len(), |(idx, _)| *idx);
        let cluster = &s[cluster_start..cluster_end];
        let w = display_width(cluster);
        if w > 0 && used + w > max_cells {
            return (&s[..byte_pos], used);
        }
        used += w;
        byte_pos = cluster_end;
    }

    (s, used)
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
        } else if let Some((decoded, len)) = decode_html_entity(&input[i..]) {
            out.push(decoded);
            i += len;
        } else {
            out.push('&');
            i += 1;
        }
    }

    out
}

/// Decodes a single HTML entity prefix if recognized, returning `(char, byte_len)`.
#[inline]
#[must_use]
pub(crate) fn decode_html_entity(rest: &str) -> Option<(char, usize)> {
    if rest.starts_with("&lt;") {
        Some(('<', 4))
    } else if rest.starts_with("&gt;") {
        Some(('>', 4))
    } else if rest.starts_with("&amp;") {
        Some(('&', 5))
    } else if rest.starts_with("&quot;") {
        Some(('"', 6))
    } else if rest.starts_with("&#39;") {
        Some(('\'', 5))
    } else if rest.starts_with("&apos;") {
        Some(('\'', 6))
    } else {
        None
    }
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

// ---------------------------------------------------------------------------
// Unified Markdown AST, GFM Table Normalization & Smart Table Layout Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdAlignment {
    Left,
    Center,
    Right,
}

impl From<Alignment> for MdAlignment {
    fn from(value: Alignment) -> Self {
        match value {
            Alignment::None | Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MdQuoteKind {
    Standard,
    Expandable,
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

impl MdQuoteKind {
    fn from_cmark(kind: Option<BlockQuoteKind>) -> Self {
        match kind {
            None => Self::Standard,
            Some(BlockQuoteKind::Note) => Self::Note,
            Some(BlockQuoteKind::Tip) => Self::Tip,
            Some(BlockQuoteKind::Important) => Self::Important,
            Some(BlockQuoteKind::Warning) => Self::Warning,
            Some(BlockQuoteKind::Caution) => Self::Caution,
        }
    }

    #[must_use]
    pub(crate) fn alert_badge(self) -> Option<(&'static str, &'static str)> {
        match self {
            Self::Standard | Self::Expandable => None,
            Self::Note => Some(("ℹ️", "Note")),
            Self::Tip => Some(("💡", "Tip")),
            Self::Important => Some(("❗", "Important")),
            Self::Warning => Some(("⚠️", "Warning")),
            Self::Caution => Some(("🚨", "Caution")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MdTable {
    pub alignments: Vec<MdAlignment>,
    pub headers: Vec<Vec<MdInline>>,
    pub rows: Vec<Vec<Vec<MdInline>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MdListItem {
    pub task: Option<bool>,
    pub blocks: Vec<MdBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MdBlock {
    Paragraph(Vec<MdInline>),
    Heading {
        level: u8,
        inlines: Vec<MdInline>,
    },
    BlockQuote {
        kind: MdQuoteKind,
        blocks: Vec<MdBlock>,
    },
    CodeBlock {
        language: String,
        code: String,
    },
    MathBlock(String),
    Table(MdTable),
    List {
        ordered_start: Option<u64>,
        items: Vec<MdListItem>,
    },
    ThematicBreak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MdInline {
    Text(String),
    Code(String),
    Math(String),
    Bold(Vec<MdInline>),
    Italic(Vec<MdInline>),
    Strikethrough(Vec<MdInline>),
    Underline(Vec<MdInline>),
    Spoiler(Vec<MdInline>),
    Link { url: String, inlines: Vec<MdInline> },
    Image { url: String, alt: Vec<MdInline> },
    SoftBreak,
    HardBreak,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct MdDocument {
    pub blocks: Vec<MdBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HtmlInlineKind {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Spoiler,
}

enum InlineKind {
    Bold,
    Italic,
    Strikethrough,
    Link(String),
    Image(String),
    HtmlTag {
        kind: HtmlInlineKind,
        raw_open: String,
    },
}

struct InlineFrame {
    kind: InlineKind,
    children: Vec<MdInline>,
}

enum BlockFrame {
    Document(Vec<MdBlock>),
    Paragraph(Vec<MdInline>),
    Heading {
        level: u8,
        inlines: Vec<MdInline>,
    },
    BlockQuote {
        kind: MdQuoteKind,
        blocks: Vec<MdBlock>,
    },
    CodeBlock {
        language: String,
        code: String,
    },
    List {
        ordered_start: Option<u64>,
        items: Vec<MdListItem>,
    },
    ListItem {
        task: Option<bool>,
        blocks: Vec<MdBlock>,
        tight_inlines: Vec<MdInline>,
    },
    Table {
        alignments: Vec<MdAlignment>,
        headers: Vec<Vec<MdInline>>,
        rows: Vec<Vec<Vec<MdInline>>>,
        in_head: bool,
        current_row: Option<Vec<Vec<MdInline>>>,
    },
    TableCell(Vec<MdInline>),
    FootnoteDef {
        label: String,
        blocks: Vec<MdBlock>,
    },
}

/// Strips optional leading blockquote markers (`> `) from a line for table detection.
fn strip_quote_prefix(line: &str) -> &str {
    let mut s = line.trim_start();
    while let Some(rest) = s.strip_prefix('>') {
        s = rest.trim_start();
    }
    s
}

/// Checks whether a line is a valid GFM table delimiter row (e.g. `| :--- | ---: |` or `--- | ---`).
fn is_gfm_table_delimiter(line: &str) -> bool {
    let stripped = strip_quote_prefix(line).trim();
    if !stripped.contains('|') || !stripped.contains('-') {
        return false;
    }
    let inner = stripped.trim_start_matches('|').trim_end_matches('|');
    if inner.trim().is_empty() {
        return false;
    }
    let mut saw_cell = false;
    for part in inner.split('|') {
        let cell = part.trim();
        if cell.is_empty() {
            return false;
        }
        let core = cell.strip_prefix(':').unwrap_or(cell);
        let core = core.strip_suffix(':').unwrap_or(core);
        if core.is_empty() || !core.bytes().all(|b| b == b'-') {
            return false;
        }
        saw_cell = true;
    }
    saw_cell
}

fn is_escaped_at(bytes: &[u8], pos: usize) -> bool {
    let mut backslashes = 0usize;
    let mut i = pos;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    (backslashes & 1) == 1
}

/// Escapes unescaped `|` characters inside balanced inline backtick spans on a GFM table row
/// so `pulldown-cmark` does not split cells on pipes inside `` `a | b` ``.
fn escape_pipes_in_inline_code(line: &str, out: &mut String) {
    if !line.contains('`') || !line.contains('|') {
        out.push_str(line);
        return;
    }
    let bytes = line.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'`' && !is_escaped_at(bytes, i) {
            let open_start = i;
            let mut ticks = 0usize;
            while i < bytes.len() && bytes[i] == b'`' {
                ticks += 1;
                i += 1;
            }
            let content_start = i;
            let mut close_pos: Option<usize> = None;
            let mut scan = content_start;
            while scan < bytes.len() {
                if bytes[scan] == b'`' {
                    let run_start = scan;
                    let mut run_len = 0usize;
                    while scan < bytes.len() && bytes[scan] == b'`' {
                        run_len += 1;
                        scan += 1;
                    }
                    if run_len == ticks {
                        close_pos = Some(run_start);
                        break;
                    }
                } else {
                    scan += 1;
                }
            }
            if let Some(close_start) = close_pos {
                out.push_str(&line[open_start..content_start]);
                let inner = &line[content_start..close_start];
                let inner_bytes = inner.as_bytes();
                for (idx, ch) in inner.char_indices() {
                    if ch == '|' && !is_escaped_at(inner_bytes, idx) {
                        out.push_str("\\|");
                    } else {
                        out.push(ch);
                    }
                }
                let close_end = close_start + ticks;
                out.push_str(&line[close_start..close_end]);
                i = close_end;
                continue;
            }
            out.push_str(&line[open_start..content_start]);
            continue;
        }
        let Some(ch) = line[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
}

fn parse_fence_opener(line: &str) -> Option<(u8, usize)> {
    let trimmed = strip_quote_prefix(line).trim_start();
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|&b| b == first).count();
    if count >= 3 {
        if first == b'`' && trimmed[count..].contains('`') {
            return None;
        }
        Some((first, count))
    } else {
        None
    }
}

fn is_fence_closer(line: &str, fence_char: u8, fence_len: usize) -> bool {
    let trimmed = strip_quote_prefix(line).trim();
    let count = trimmed.bytes().take_while(|&b| b == fence_char).count();
    count >= fence_len && trimmed[count..].trim().is_empty()
}

/// Preprocesses raw Markdown to escape pipes inside inline code spans within GFM tables.
fn sanitize_table_code_pipes(raw: &str) -> Cow<'_, str> {
    if !raw.contains('|') || !raw.contains('`') {
        return Cow::Borrowed(raw);
    }

    let lines: Vec<&str> = raw.split_inclusive('\n').collect();
    let mut out = String::with_capacity(raw.len() + 32);
    let mut fence: Option<(u8, usize)> = None;
    let mut in_table = false;
    let mut modified = false;

    for idx in 0..lines.len() {
        let line = lines[idx];
        let line_no_nl = line.trim_end_matches(['\r', '\n']);

        if let Some((f_char, f_len)) = fence {
            out.push_str(line);
            if is_fence_closer(line_no_nl, f_char, f_len) {
                fence = None;
            }
            continue;
        }

        if let Some(open) = parse_fence_opener(line_no_nl) {
            in_table = false;
            fence = Some(open);
            out.push_str(line);
            continue;
        }

        if in_table {
            let trimmed = strip_quote_prefix(line_no_nl).trim();
            if trimmed.is_empty()
                || !trimmed.contains('|')
                || trimmed.starts_with('#')
                || trimmed.starts_with("```")
                || trimmed.starts_with("~~~")
            {
                in_table = false;
                out.push_str(line);
            } else {
                let before_len = out.len();
                escape_pipes_in_inline_code(line_no_nl, &mut out);
                if out.len() - before_len != line_no_nl.len() {
                    modified = true;
                }
                out.push_str(&line[line_no_nl.len()..]);
            }
            continue;
        }

        let next_is_delim = lines
            .get(idx + 1)
            .is_some_and(|next| is_gfm_table_delimiter(next.trim_end_matches(['\r', '\n'])));
        if next_is_delim && strip_quote_prefix(line_no_nl).contains('|') {
            in_table = true;
            let before_len = out.len();
            escape_pipes_in_inline_code(line_no_nl, &mut out);
            if out.len() - before_len != line_no_nl.len() {
                modified = true;
            }
            out.push_str(&line[line_no_nl.len()..]);
            continue;
        }

        out.push_str(line);
    }

    if modified {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(raw)
    }
}

fn parse_inline_html_tag(raw: &str) -> Option<(bool, HtmlInlineKind)> {
    let trimmed = raw.trim();
    let inner = trimmed.strip_prefix('<')?.strip_suffix('>')?.trim();
    let is_close = inner.starts_with('/');
    let rest = inner.strip_prefix('/').unwrap_or(inner).trim();
    let tag_name = rest
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    let kind = match tag_name.as_str() {
        "u" | "ins" => HtmlInlineKind::Underline,
        "b" | "strong" => HtmlInlineKind::Bold,
        "i" | "em" => HtmlInlineKind::Italic,
        "s" | "del" | "strike" => HtmlInlineKind::Strikethrough,
        "tg-spoiler" => HtmlInlineKind::Spoiler,
        "span" if is_close || rest.contains("tg-spoiler") => HtmlInlineKind::Spoiler,
        _ => return None,
    };
    Some((is_close, kind))
}

fn is_br_tag(raw: &str) -> bool {
    let t = raw.trim();
    t.eq_ignore_ascii_case("<br>")
        || t.eq_ignore_ascii_case("<br/>")
        || t.eq_ignore_ascii_case("<br />")
}

fn heading_level_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn push_inline_text(target: &mut Vec<MdInline>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(MdInline::Text(prev)) = target.last_mut() {
        // Keep backslash-escaped single "|" separate so "\||" never triggers "||spoiler||".
        if !(prev == "|" && s == "|") {
            prev.push_str(s);
            return;
        }
    }
    target.push(MdInline::Text(s.to_string()));
}

fn current_inline_vec<'a>(
    block_stack: &'a mut [BlockFrame],
    inline_stack: &'a mut [InlineFrame],
) -> Option<&'a mut Vec<MdInline>> {
    if let Some(top) = inline_stack.last_mut() {
        return Some(&mut top.children);
    }
    match block_stack.last_mut()? {
        BlockFrame::Paragraph(inlines)
        | BlockFrame::Heading { inlines, .. }
        | BlockFrame::TableCell(inlines) => Some(inlines),
        BlockFrame::ListItem { tight_inlines, .. } => Some(tight_inlines),
        _ => None,
    }
}

fn flush_unclosed_html_frames(
    block_stack: &mut [BlockFrame],
    inline_stack: &mut Vec<InlineFrame>,
    until_non_html: bool,
) {
    while let Some(top) = inline_stack.last() {
        if until_non_html && !matches!(top.kind, InlineKind::HtmlTag { .. }) {
            break;
        }
        let Some(frame) = inline_stack.pop() else {
            break;
        };
        if let Some(target) = current_inline_vec(block_stack, inline_stack) {
            if let InlineKind::HtmlTag { raw_open, .. } = frame.kind {
                push_inline_text(target, &raw_open);
            }
            for child in frame.children {
                match child {
                    MdInline::Text(t) => push_inline_text(target, &t),
                    other => target.push(other),
                }
            }
        }
    }
}

fn wrap_html_kind(kind: HtmlInlineKind, children: Vec<MdInline>) -> MdInline {
    match kind {
        HtmlInlineKind::Bold => MdInline::Bold(children),
        HtmlInlineKind::Italic => MdInline::Italic(children),
        HtmlInlineKind::Underline => MdInline::Underline(children),
        HtmlInlineKind::Strikethrough => MdInline::Strikethrough(children),
        HtmlInlineKind::Spoiler => MdInline::Spoiler(children),
    }
}

/// Scans `inlines` for paired `||spoiler||` spans outside code/math, recursing into nested styles.
/// Logical OR patterns like `a || b || c` (with whitespace right after `||` or before `||`) stay literal.
fn apply_inline_spoilers(inlines: Vec<MdInline>) -> Vec<MdInline> {
    let mut recursed = Vec::with_capacity(inlines.len());
    for item in inlines {
        let mapped = match item {
            MdInline::Bold(c) => MdInline::Bold(apply_inline_spoilers(c)),
            MdInline::Italic(c) => MdInline::Italic(apply_inline_spoilers(c)),
            MdInline::Strikethrough(c) => MdInline::Strikethrough(apply_inline_spoilers(c)),
            MdInline::Underline(c) => MdInline::Underline(apply_inline_spoilers(c)),
            MdInline::Spoiler(c) => MdInline::Spoiler(apply_inline_spoilers(c)),
            MdInline::Link { url, inlines } => MdInline::Link {
                url,
                inlines: apply_inline_spoilers(inlines),
            },
            other => other,
        };
        recursed.push(mapped);
    }

    if !recursed
        .iter()
        .any(|n| matches!(n, MdInline::Text(t) if t.contains("||")))
    {
        return recursed;
    }

    let mut out = Vec::with_capacity(recursed.len());
    let mut i = 0usize;
    while i < recursed.len() {
        let MdInline::Text(ref text) = recursed[i] else {
            out.push(recursed[i].clone());
            i += 1;
            continue;
        };

        let Some(open_rel) = find_valid_spoiler_open(text, &recursed, i) else {
            push_inline_text(&mut out, text);
            i += 1;
            continue;
        };

        let after_open = &text[open_rel + 2..];
        // Check if closing `||` sits in the very same `Text` node first.
        if let Some(close_rel) = find_valid_spoiler_close_in_str(after_open) {
            push_inline_text(&mut out, &text[..open_rel]);
            let inner = &after_open[..close_rel];
            out.push(MdInline::Spoiler(vec![MdInline::Text(inner.to_string())]));
            let tail = &after_open[close_rel + 2..];
            if !tail.is_empty() {
                recursed[i] = MdInline::Text(tail.to_string());
            } else {
                i += 1;
            }
            continue;
        }

        // Otherwise search forward across sibling nodes in this container.
        if let Some((close_idx, close_rel)) = find_valid_spoiler_close_across(&recursed, i + 1) {
            push_inline_text(&mut out, &text[..open_rel]);
            let mut spoiler_children = Vec::new();
            if !after_open.is_empty() {
                spoiler_children.push(MdInline::Text(after_open.to_string()));
            }
            for node in &recursed[i + 1..close_idx] {
                spoiler_children.push(node.clone());
            }
            let MdInline::Text(ref close_text) = recursed[close_idx] else {
                unreachable!("close_idx always points to MdInline::Text");
            };
            let before_close = &close_text[..close_rel];
            if !before_close.is_empty() {
                push_inline_text(&mut spoiler_children, before_close);
            }
            out.push(MdInline::Spoiler(spoiler_children));

            let after_close = &close_text[close_rel + 2..];
            if !after_close.is_empty() {
                recursed[close_idx] = MdInline::Text(after_close.to_string());
                i = close_idx;
            } else {
                i = close_idx + 1;
            }
            continue;
        }

        push_inline_text(&mut out, text);
        i += 1;
    }

    out
}

fn find_valid_spoiler_open(text: &str, siblings: &[MdInline], idx: usize) -> Option<usize> {
    let mut search = 0usize;
    while let Some(rel) = text[search..].find("||") {
        let pos = search + rel;
        let after = &text[pos + 2..];
        let valid_right = match after.chars().next() {
            Some(c) => !c.is_whitespace(),
            None => siblings
                .get(idx + 1)
                .is_some_and(|next| !starts_with_whitespace(next)),
        };
        if valid_right {
            return Some(pos);
        }
        search = pos + 2;
    }
    None
}

fn find_valid_spoiler_close_in_str(s: &str) -> Option<usize> {
    let mut search = 0usize;
    while let Some(rel) = s[search..].find("||") {
        let pos = search + rel;
        if pos > 0
            && s[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            return Some(pos);
        }
        search = pos + 2;
    }
    None
}

fn find_valid_spoiler_close_across(
    siblings: &[MdInline],
    start_idx: usize,
) -> Option<(usize, usize)> {
    for (idx, node) in siblings.iter().enumerate().skip(start_idx) {
        if matches!(node, MdInline::SoftBreak | MdInline::HardBreak) {
            return None;
        }
        let MdInline::Text(t) = node else {
            continue;
        };
        let mut search = 0usize;
        while let Some(rel) = t[search..].find("||") {
            let pos = search + rel;
            let valid_left = if pos > 0 {
                t[..pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace())
            } else {
                idx > start_idx && !ends_with_whitespace(&siblings[idx - 1])
            };
            if valid_left {
                return Some((idx, pos));
            }
            search = pos + 2;
        }
    }
    None
}

fn starts_with_whitespace(node: &MdInline) -> bool {
    match node {
        MdInline::Text(t) => t.chars().next().is_some_and(char::is_whitespace),
        MdInline::SoftBreak | MdInline::HardBreak => true,
        _ => false,
    }
}

fn ends_with_whitespace(node: &MdInline) -> bool {
    match node {
        MdInline::Text(t) => t.chars().next_back().is_some_and(char::is_whitespace),
        MdInline::SoftBreak | MdInline::HardBreak => true,
        _ => false,
    }
}

/// Strips `>! ` expandable quote markers at the start of a blockquote paragraph.
fn try_strip_expandable_bang_prefix(blocks: &mut [MdBlock]) -> bool {
    let Some(MdBlock::Paragraph(inlines)) = blocks.first_mut() else {
        return false;
    };
    let inlines_len = inlines.len();
    let Some(MdInline::Text(first)) = inlines.first_mut() else {
        return false;
    };
    if let Some(rest) = first.strip_prefix("! ") {
        *first = rest.to_string();
    } else if first == "!" && inlines_len > 1 {
        first.clear();
    } else {
        return false;
    }

    // Also strip leading "! " after breaks inside the same ">! line1\n>! line2" paragraph.
    let mut after_break = false;
    for node in inlines.iter_mut() {
        match node {
            MdInline::SoftBreak | MdInline::HardBreak => after_break = true,
            MdInline::Text(t) if after_break => {
                if let Some(rest) = t.strip_prefix("! ") {
                    *t = rest.to_string();
                } else if t == "!" {
                    t.clear();
                }
                after_break = false;
            }
            _ => after_break = false,
        }
    }
    inlines.retain(|n| !matches!(n, MdInline::Text(t) if t.is_empty()));
    true
}

fn push_block(block_stack: &mut [BlockFrame], block: MdBlock) {
    let Some(parent) = block_stack.last_mut() else {
        return;
    };
    match parent {
        BlockFrame::Document(blocks)
        | BlockFrame::BlockQuote { blocks, .. }
        | BlockFrame::FootnoteDef { blocks, .. } => blocks.push(block),
        BlockFrame::ListItem {
            blocks,
            tight_inlines,
            ..
        } => {
            if !tight_inlines.is_empty() {
                let flushed = apply_inline_spoilers(std::mem::take(tight_inlines));
                blocks.push(MdBlock::Paragraph(flushed));
            }
            blocks.push(block);
        }
        _ => {}
    }
}

/// Parses raw Markdown (cleaning terminal ANSI/spinners and GFM table code-span pipes)
/// into a platform-neutral [`MdDocument`] AST using `pulldown-cmark`.
#[must_use]
pub(crate) fn parse_markdown(raw: &str) -> MdDocument {
    let ansi_cleaned: Cow<'_, str> = if needs_ansi_cleanup(raw) {
        Cow::Owned(strip_ansi_codes(raw))
    } else {
        Cow::Borrowed(raw.trim_end())
    };
    let prepared = sanitize_table_code_pipes(ansi_cleaned.as_ref());

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);
    options.insert(Options::ENABLE_GFM);

    let parser = Parser::new_ext(prepared.as_ref(), options);
    let mut block_stack: Vec<BlockFrame> = vec![BlockFrame::Document(Vec::new())];
    let mut inline_stack: Vec<InlineFrame> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    block_stack.push(BlockFrame::Paragraph(Vec::new()));
                }
                Tag::Heading { level, .. } => {
                    block_stack.push(BlockFrame::Heading {
                        level: heading_level_num(level),
                        inlines: Vec::new(),
                    });
                }
                Tag::BlockQuote(kind) => {
                    block_stack.push(BlockFrame::BlockQuote {
                        kind: MdQuoteKind::from_cmark(kind),
                        blocks: Vec::new(),
                    });
                }
                Tag::CodeBlock(kind) => {
                    let language = match kind {
                        CodeBlockKind::Indented => String::new(),
                        CodeBlockKind::Fenced(info) => info
                            .split(|c: char| c.is_whitespace() || c == ',')
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    };
                    block_stack.push(BlockFrame::CodeBlock {
                        language,
                        code: String::new(),
                    });
                }
                Tag::List(ordered_start) => {
                    block_stack.push(BlockFrame::List {
                        ordered_start,
                        items: Vec::new(),
                    });
                }
                Tag::Item => {
                    block_stack.push(BlockFrame::ListItem {
                        task: None,
                        blocks: Vec::new(),
                        tight_inlines: Vec::new(),
                    });
                }
                Tag::Table(alignments) => {
                    block_stack.push(BlockFrame::Table {
                        alignments: alignments.into_iter().map(MdAlignment::from).collect(),
                        headers: Vec::new(),
                        rows: Vec::new(),
                        in_head: false,
                        current_row: None,
                    });
                }
                Tag::TableHead => {
                    if let Some(BlockFrame::Table {
                        in_head,
                        current_row,
                        ..
                    }) = block_stack.last_mut()
                    {
                        *in_head = true;
                        *current_row = Some(Vec::new());
                    }
                }
                Tag::TableRow => {
                    if let Some(BlockFrame::Table { current_row, .. }) = block_stack.last_mut()
                        && current_row.is_none()
                    {
                        *current_row = Some(Vec::new());
                    }
                }
                Tag::TableCell => {
                    if let Some(BlockFrame::Table { current_row, .. }) = block_stack.last_mut()
                        && current_row.is_none()
                    {
                        *current_row = Some(Vec::new());
                    }
                    block_stack.push(BlockFrame::TableCell(Vec::new()));
                }
                Tag::Emphasis => inline_stack.push(InlineFrame {
                    kind: InlineKind::Italic,
                    children: Vec::new(),
                }),
                Tag::Strong => inline_stack.push(InlineFrame {
                    kind: InlineKind::Bold,
                    children: Vec::new(),
                }),
                Tag::Strikethrough => inline_stack.push(InlineFrame {
                    kind: InlineKind::Strikethrough,
                    children: Vec::new(),
                }),
                Tag::Link { dest_url, .. } => inline_stack.push(InlineFrame {
                    kind: InlineKind::Link(dest_url.into_string()),
                    children: Vec::new(),
                }),
                Tag::Image { dest_url, .. } => inline_stack.push(InlineFrame {
                    kind: InlineKind::Image(dest_url.into_string()),
                    children: Vec::new(),
                }),
                Tag::FootnoteDefinition(label) => {
                    block_stack.push(BlockFrame::FootnoteDef {
                        label: label.into_string(),
                        blocks: Vec::new(),
                    });
                }
                _ => {}
            },

            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);
                    if let Some(BlockFrame::Paragraph(inlines)) = block_stack.pop() {
                        let processed = apply_inline_spoilers(inlines);
                        if !processed.is_empty() {
                            push_block(&mut block_stack, MdBlock::Paragraph(processed));
                        }
                    }
                }
                TagEnd::Heading(_) => {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);
                    if let Some(BlockFrame::Heading { level, inlines }) = block_stack.pop() {
                        let processed = apply_inline_spoilers(inlines);
                        push_block(
                            &mut block_stack,
                            MdBlock::Heading {
                                level,
                                inlines: processed,
                            },
                        );
                    }
                }
                TagEnd::BlockQuote(_) => {
                    if let Some(BlockFrame::BlockQuote {
                        mut kind,
                        mut blocks,
                    }) = block_stack.pop()
                    {
                        if kind == MdQuoteKind::Standard
                            && try_strip_expandable_bang_prefix(&mut blocks)
                        {
                            kind = MdQuoteKind::Expandable;
                        } else if kind == MdQuoteKind::Standard
                            && blocks.len() == 1
                            && matches!(blocks.first(), Some(MdBlock::BlockQuote { .. }))
                            && let Some(MdBlock::BlockQuote {
                                kind: inner_kind,
                                blocks: inner_blocks,
                            }) = blocks.pop()
                        {
                            kind = if inner_kind == MdQuoteKind::Standard {
                                MdQuoteKind::Expandable
                            } else {
                                inner_kind
                            };
                            blocks = inner_blocks;
                        }
                        if !blocks.is_empty() {
                            push_block(&mut block_stack, MdBlock::BlockQuote { kind, blocks });
                        }
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(BlockFrame::CodeBlock { language, code }) = block_stack.pop() {
                        let body = code.trim_end_matches('\n').to_string();
                        push_block(
                            &mut block_stack,
                            MdBlock::CodeBlock {
                                language,
                                code: body,
                            },
                        );
                    }
                }
                TagEnd::List(_) => {
                    if let Some(BlockFrame::List {
                        ordered_start,
                        items,
                    }) = block_stack.pop()
                        && !items.is_empty()
                    {
                        push_block(
                            &mut block_stack,
                            MdBlock::List {
                                ordered_start,
                                items,
                            },
                        );
                    }
                }
                TagEnd::Item => {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);
                    if let Some(BlockFrame::ListItem {
                        task,
                        mut blocks,
                        tight_inlines,
                    }) = block_stack.pop()
                    {
                        if !tight_inlines.is_empty() {
                            let processed = apply_inline_spoilers(tight_inlines);
                            blocks.push(MdBlock::Paragraph(processed));
                        }
                        if let Some(BlockFrame::List { items, .. }) = block_stack.last_mut() {
                            items.push(MdListItem { task, blocks });
                        }
                    }
                }
                TagEnd::TableCell => {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);
                    if let Some(BlockFrame::TableCell(inlines)) = block_stack.pop() {
                        let processed = apply_inline_spoilers(inlines);
                        if let Some(BlockFrame::Table {
                            current_row: Some(row),
                            ..
                        }) = block_stack.last_mut()
                        {
                            row.push(processed);
                        }
                    }
                }
                TagEnd::TableRow => {
                    if let Some(BlockFrame::Table {
                        in_head,
                        headers,
                        rows,
                        current_row,
                        ..
                    }) = block_stack.last_mut()
                        && let Some(row) = current_row.take()
                    {
                        if *in_head && headers.is_empty() {
                            *headers = row;
                        } else {
                            rows.push(row);
                        }
                    }
                }
                TagEnd::TableHead => {
                    if let Some(BlockFrame::Table {
                        in_head,
                        headers,
                        current_row,
                        ..
                    }) = block_stack.last_mut()
                    {
                        if let Some(row) = current_row.take()
                            && headers.is_empty()
                        {
                            *headers = row;
                        }
                        *in_head = false;
                    }
                }
                TagEnd::Table => {
                    if let Some(BlockFrame::Table {
                        mut alignments,
                        mut headers,
                        mut rows,
                        ..
                    }) = block_stack.pop()
                    {
                        let col_count = alignments
                            .len()
                            .max(headers.len())
                            .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                        if col_count > 0 {
                            alignments.resize(col_count, MdAlignment::Left);
                            headers.resize_with(col_count, Vec::new);
                            for row in &mut rows {
                                row.resize_with(col_count, Vec::new);
                            }
                            push_block(
                                &mut block_stack,
                                MdBlock::Table(MdTable {
                                    alignments,
                                    headers,
                                    rows,
                                }),
                            );
                        }
                    }
                }
                TagEnd::Emphasis
                | TagEnd::Strong
                | TagEnd::Strikethrough
                | TagEnd::Link
                | TagEnd::Image => {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, true);
                    if let Some(frame) = inline_stack.pop()
                        && let Some(target) =
                            current_inline_vec(&mut block_stack, &mut inline_stack)
                    {
                        let node = match frame.kind {
                            InlineKind::Bold => MdInline::Bold(frame.children),
                            InlineKind::Italic => MdInline::Italic(frame.children),
                            InlineKind::Strikethrough => MdInline::Strikethrough(frame.children),
                            InlineKind::Link(url) => MdInline::Link {
                                url,
                                inlines: frame.children,
                            },
                            InlineKind::Image(url) => MdInline::Image {
                                url,
                                alt: frame.children,
                            },
                            InlineKind::HtmlTag { kind, .. } => {
                                wrap_html_kind(kind, frame.children)
                            }
                        };
                        target.push(node);
                    }
                }
                TagEnd::FootnoteDefinition => {
                    if let Some(BlockFrame::FootnoteDef { label, mut blocks }) = block_stack.pop() {
                        if let Some(MdBlock::Paragraph(inlines)) = blocks.first_mut() {
                            inlines.insert(0, MdInline::Text(format!("[^{label}]: ")));
                        }
                        for b in blocks {
                            push_block(&mut block_stack, b);
                        }
                    }
                }
                _ => {}
            },

            Event::Text(text) => {
                if let Some(BlockFrame::CodeBlock { code, .. }) = block_stack.last_mut() {
                    code.push_str(&text);
                } else if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack)
                {
                    push_inline_text(target, &text);
                }
            }

            Event::Code(code) => {
                if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                    target.push(MdInline::Code(code.into_string()));
                }
            }

            Event::InlineMath(math) => {
                if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                    target.push(MdInline::Math(math.into_string()));
                }
            }

            Event::DisplayMath(math) => {
                let body = math.trim().to_string();
                if matches!(block_stack.last(), Some(BlockFrame::Paragraph(_))) {
                    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);
                    if let Some(BlockFrame::Paragraph(prev_inlines)) = block_stack.pop() {
                        let processed = apply_inline_spoilers(prev_inlines);
                        if !processed.is_empty() {
                            push_block(&mut block_stack, MdBlock::Paragraph(processed));
                        }
                    }
                    push_block(&mut block_stack, MdBlock::MathBlock(body));
                    block_stack.push(BlockFrame::Paragraph(Vec::new()));
                } else {
                    push_block(&mut block_stack, MdBlock::MathBlock(body));
                }
            }

            Event::SoftBreak => {
                if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                    target.push(MdInline::SoftBreak);
                }
            }

            Event::HardBreak => {
                if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                    target.push(MdInline::HardBreak);
                }
            }

            Event::Rule => {
                push_block(&mut block_stack, MdBlock::ThematicBreak);
            }

            Event::TaskListMarker(checked) => {
                if let Some(BlockFrame::ListItem { task, .. }) = block_stack.last_mut() {
                    *task = Some(checked);
                }
            }

            Event::FootnoteReference(label) => {
                if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                    push_inline_text(target, &format!("[^{label}]"));
                }
            }

            Event::InlineHtml(html) => {
                let raw_html = html.as_ref();
                if is_br_tag(raw_html) {
                    if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack) {
                        target.push(MdInline::HardBreak);
                    }
                } else if let Some((is_close, kind)) = parse_inline_html_tag(raw_html) {
                    if is_close {
                        if let Some(pos) = inline_stack.iter().rposition(
                            |f| matches!(f.kind, InlineKind::HtmlTag { kind: k, .. } if k == kind),
                        ) {
                            while inline_stack.len() > pos + 1 {
                                let Some(extra) = inline_stack.pop() else {
                                    break;
                                };
                                if let Some(target) =
                                    current_inline_vec(&mut block_stack, &mut inline_stack)
                                {
                                    if let InlineKind::HtmlTag { raw_open, .. } = extra.kind {
                                        push_inline_text(target, &raw_open);
                                    }
                                    target.extend(extra.children);
                                }
                            }
                            if let Some(frame) = inline_stack.pop()
                                && let Some(target) =
                                    current_inline_vec(&mut block_stack, &mut inline_stack)
                            {
                                target.push(wrap_html_kind(kind, frame.children));
                            }
                        } else if let Some(target) =
                            current_inline_vec(&mut block_stack, &mut inline_stack)
                        {
                            push_inline_text(target, raw_html);
                        }
                    } else {
                        inline_stack.push(InlineFrame {
                            kind: InlineKind::HtmlTag {
                                kind,
                                raw_open: raw_html.to_string(),
                            },
                            children: Vec::new(),
                        });
                    }
                } else if let Some(target) = current_inline_vec(&mut block_stack, &mut inline_stack)
                {
                    push_inline_text(target, raw_html);
                }
            }

            Event::Html(html) => {
                let trimmed = html.trim();
                if trimmed.is_empty() || (trimmed.starts_with("<!--") && trimmed.ends_with("-->")) {
                    continue;
                }
                if trimmed.starts_with('<') && parse_inline_html_tag(trimmed).is_some() {
                    // Handle standalone inline HTML blocks like `<u>text</u>` or `<tg-spoiler>...</tg-spoiler>`.
                    let synthetic = format!("x {trimmed}");
                    let mut inner_doc = parse_markdown(&synthetic);
                    if let Some(MdBlock::Paragraph(mut inlines)) = inner_doc.blocks.pop() {
                        if let Some(MdInline::Text(first)) = inlines.first_mut()
                            && let Some(stripped) = first.strip_prefix("x ")
                        {
                            *first = stripped.to_string();
                            if first.is_empty() {
                                inlines.remove(0);
                            }
                        }
                        if !inlines.is_empty() {
                            push_block(&mut block_stack, MdBlock::Paragraph(inlines));
                        }
                    }
                    continue;
                }
                push_block(
                    &mut block_stack,
                    MdBlock::Paragraph(vec![MdInline::Text(
                        html.trim_end_matches('\n').to_string(),
                    )]),
                );
            }
        }
    }

    flush_unclosed_html_frames(&mut block_stack, &mut inline_stack, false);

    // Finalize root document cleanly even if streaming input ended mid-block.
    while block_stack.len() > 1 {
        let Some(top) = block_stack.pop() else {
            break;
        };
        match top {
            BlockFrame::Paragraph(inlines) => {
                let processed = apply_inline_spoilers(inlines);
                if !processed.is_empty() {
                    push_block(&mut block_stack, MdBlock::Paragraph(processed));
                }
            }
            BlockFrame::CodeBlock { language, code } => {
                push_block(
                    &mut block_stack,
                    MdBlock::CodeBlock {
                        language,
                        code: code.trim_end_matches('\n').to_string(),
                    },
                );
            }
            BlockFrame::BlockQuote { kind, blocks } if !blocks.is_empty() => {
                push_block(&mut block_stack, MdBlock::BlockQuote { kind, blocks });
            }
            _ => {}
        }
    }

    match block_stack.pop() {
        Some(BlockFrame::Document(blocks)) => MdDocument { blocks },
        _ => MdDocument::default(),
    }
}

/// Ensures clean vertical separation between consecutive rendered AST blocks.
pub(crate) fn separate_blocks(
    prev: &MdBlock,
    next: &MdBlock,
    single_after_heading: bool,
    out: &mut String,
) {
    if single_after_heading && matches!(prev, MdBlock::Heading { .. }) {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return;
    }

    let wants_blank_line = matches!(
        (prev, next),
        (_, MdBlock::Heading { .. })
            | (MdBlock::Paragraph(_), MdBlock::Paragraph(_))
            | (
                MdBlock::CodeBlock { .. }
                    | MdBlock::MathBlock(_)
                    | MdBlock::Table(_)
                    | MdBlock::BlockQuote { .. },
                _
            )
    );
    if wants_blank_line {
        while !out.ends_with("\n\n") {
            out.push('\n');
        }
    } else if !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Converts inline AST nodes into plain display text (stripping formatting markers).
#[must_use]
pub(crate) fn inlines_to_plain_text(inlines: &[MdInline]) -> String {
    let mut out = String::new();
    push_inlines_plain_text(inlines, &mut out);
    out
}

pub(crate) fn push_inlines_plain_text(inlines: &[MdInline], out: &mut String) {
    for node in inlines {
        match node {
            MdInline::Text(s) | MdInline::Code(s) | MdInline::Math(s) => out.push_str(s),
            MdInline::Bold(c)
            | MdInline::Italic(c)
            | MdInline::Strikethrough(c)
            | MdInline::Underline(c)
            | MdInline::Spoiler(c) => push_inlines_plain_text(c, out),
            MdInline::Link { url, inlines } => {
                let before = out.len();
                push_inlines_plain_text(inlines, out);
                if out.len() == before {
                    out.push_str(url);
                }
            }
            MdInline::Image { url, alt } => {
                let before = out.len();
                push_inlines_plain_text(alt, out);
                if out.len() == before {
                    out.push_str(url);
                }
            }
            MdInline::SoftBreak | MdInline::HardBreak => out.push('\n'),
        }
    }
}

fn has_rich_only_table_cell(inlines: &[MdInline]) -> bool {
    inlines.iter().any(|node| match node {
        MdInline::SoftBreak
        | MdInline::HardBreak
        | MdInline::Link { .. }
        | MdInline::Image { .. }
        | MdInline::Spoiler(_) => true,
        MdInline::Text(s) => s.contains('\n'),
        MdInline::Bold(c)
        | MdInline::Italic(c)
        | MdInline::Strikethrough(c)
        | MdInline::Underline(c) => has_rich_only_table_cell(c),
        MdInline::Code(_) | MdInline::Math(_) => false,
    })
}

/// Pads `s` (whose precomputed display width is `w`) to `target_w` spaces according to `align`.
fn push_aligned_cell(out: &mut String, s: &str, w: usize, target_w: usize, align: MdAlignment) {
    let pad = target_w.saturating_sub(w);
    match align {
        MdAlignment::Left => {
            out.push_str(s);
            for _ in 0..pad {
                out.push(' ');
            }
        }
        MdAlignment::Right => {
            for _ in 0..pad {
                out.push(' ');
            }
            out.push_str(s);
        }
        MdAlignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            for _ in 0..left {
                out.push(' ');
            }
            out.push_str(s);
            for _ in 0..right {
                out.push(' ');
            }
        }
    }
}

fn push_box_border(out: &mut String, widths: &[usize], left: char, mid: char, right: char) {
    out.push(left);
    for (idx, &w) in widths.iter().enumerate() {
        if idx > 0 {
            out.push(mid);
        }
        for _ in 0..(w + 2) {
            out.push('─');
        }
    }
    out.push(right);
}

fn push_box_row(
    out: &mut String,
    cells: &[(String, usize)],
    widths: &[usize],
    alignments: &[MdAlignment],
) {
    out.push('│');
    for (idx, &target_w) in widths.iter().enumerate() {
        out.push(' ');
        if let Some((text, w)) = cells.get(idx) {
            let align = alignments.get(idx).copied().unwrap_or(MdAlignment::Left);
            push_aligned_cell(out, text, *w, target_w, align);
        } else {
            for _ in 0..target_w {
                out.push(' ');
            }
        }
        out.push_str(" │");
    }
}

/// Renders a GFM [`MdTable`] as a crisp Unicode box-drawing table (`┌─┬─┐` / `│ │ │` / `├─┼─┤` / `└─┴─┘`)
/// when all cells are single-line without links/spoilers and total display width fits within
/// [`COMPACT_TABLE_MAX_WIDTH`] (38 columns). Returns `None` for wider or rich tables so callers
/// render the mobile-friendly vertical card/record layout instead.
#[must_use]
pub(crate) fn render_compact_box_table(table: &MdTable) -> Option<String> {
    let col_count = table.headers.len();
    if col_count == 0 {
        return None;
    }

    if table.headers.iter().any(|c| has_rich_only_table_cell(c))
        || table
            .rows
            .iter()
            .any(|row| row.iter().any(|c| has_rich_only_table_cell(c)))
    {
        return None;
    }

    let mut header_cells = Vec::with_capacity(col_count);
    let mut col_widths = vec![1usize; col_count];
    for (idx, cell) in table.headers.iter().enumerate() {
        let plain = inlines_to_plain_text(cell).trim().to_string();
        let w = display_width(&plain);
        col_widths[idx] = col_widths[idx].max(w);
        header_cells.push((plain, w));
    }

    let mut body_rows = Vec::with_capacity(table.rows.len());
    for row in &table.rows {
        let mut cells = Vec::with_capacity(col_count);
        for (idx, cell) in row.iter().enumerate() {
            let plain = inlines_to_plain_text(cell).trim().to_string();
            let w = display_width(&plain);
            if idx < col_count {
                col_widths[idx] = col_widths[idx].max(w);
            }
            cells.push((plain, w));
        }
        body_rows.push(cells);
    }

    let total_width = (col_count + 1) + col_widths.iter().sum::<usize>() + (2 * col_count);
    if total_width > COMPACT_TABLE_MAX_WIDTH {
        return None;
    }

    let mut out = String::with_capacity((body_rows.len() + 4) * (total_width + 4));
    push_box_border(&mut out, &col_widths, '┌', '┬', '┐');
    out.push('\n');
    push_box_row(&mut out, &header_cells, &col_widths, &table.alignments);
    out.push('\n');

    if !body_rows.is_empty() {
        push_box_border(&mut out, &col_widths, '├', '┼', '┤');
        out.push('\n');
        for row in &body_rows {
            push_box_row(&mut out, row, &col_widths, &table.alignments);
            out.push('\n');
        }
    }
    push_box_border(&mut out, &col_widths, '└', '┴', '┘');
    Some(out)
}

/// Fence-aware and UTF-8-safe Markdown chunker enforcing both `max_chars` and `max_bytes`.
/// Automatically closes and reopens fenced code blocks (```` ``` ```` / `~~~`) across chunks.
#[must_use]
pub(crate) fn split_markdown_chunks(md: &str, max_chars: usize, max_bytes: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let max_bytes = max_bytes.max(1);

    if md.is_empty() {
        return vec![String::new()];
    }
    if md.len() <= max_bytes && (md.len() <= max_chars || md.chars().count() <= max_chars) {
        return vec![md.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::with_capacity(max_bytes.min(md.len()));
    let mut current_chars = 0usize;
    let mut open_fence: Option<(char, usize, String)> = None;

    let close_reserve = |fence: &Option<(char, usize, String)>| -> usize {
        fence.as_ref().map_or(0, |(_, len, _)| 1 + len)
    };

    let reopen_fence =
        |buf: &mut String, chars: &mut usize, fence: &Option<(char, usize, String)>| {
            if let Some((ch, len, info)) = fence {
                for _ in 0..*len {
                    buf.push(*ch);
                }
                buf.push_str(info);
                buf.push('\n');
                *chars += len + info.chars().count() + 1;
            }
        };

    let flush_current = |chunks: &mut Vec<String>,
                         current: &mut String,
                         current_chars: &mut usize,
                         fence: &Option<(char, usize, String)>| {
        if current.is_empty() {
            return;
        }
        if let Some((ch, len, _)) = fence {
            if !current.ends_with('\n') {
                current.push('\n');
            }
            for _ in 0..*len {
                current.push(*ch);
            }
            current.push('\n');
        }
        chunks.push(std::mem::take(current));
        *current_chars = 0;
        reopen_fence(current, current_chars, fence);
    };

    for line in md.split_inclusive('\n') {
        let line_no_nl = line.trim_end_matches(['\r', '\n']);
        let line_chars = line.chars().count();
        let line_bytes = line.len();

        let next_fence = if let Some((f_char, f_len, _)) = &open_fence {
            if is_fence_closer(line_no_nl, *f_char as u8, *f_len) {
                None
            } else {
                open_fence.clone()
            }
        } else if let Some((f_byte, f_len)) = parse_fence_opener(line_no_nl) {
            let trimmed = line_no_nl.trim_start();
            let info = trimmed[f_len..].trim().to_string();
            Some((f_byte as char, f_len, info))
        } else {
            None
        };

        let reserve = close_reserve(&next_fence);
        if current.len() + line_bytes + reserve <= max_bytes
            && current_chars + line_chars + reserve <= max_chars
        {
            current.push_str(line);
            current_chars += line_chars;
            open_fence = next_fence;
            continue;
        }

        if !current.is_empty() {
            flush_current(&mut chunks, &mut current, &mut current_chars, &open_fence);
        }

        let fresh_reserve = close_reserve(&next_fence);
        if current.len() + line_bytes + fresh_reserve <= max_bytes
            && current_chars + line_chars + fresh_reserve <= max_chars
        {
            current.push_str(line);
            current_chars += line_chars;
            open_fence = next_fence;
            continue;
        }

        // Single oversized line: split safely on UTF-8 boundaries accounting for fence closing.
        let mut start = 0usize;
        while start < line.len() {
            let close_cost = close_reserve(&open_fence);
            let avail_bytes = max_bytes.saturating_sub(current.len() + close_cost).max(1);
            let avail_chars = max_chars.saturating_sub(current_chars + close_cost).max(1);

            let mut end = start;
            let mut taken_chars = 0usize;
            for (rel_idx, ch) in line[start..].char_indices() {
                let next_end = start + rel_idx + ch.len_utf8();
                if taken_chars > 0
                    && (next_end - start > avail_bytes || taken_chars + 1 > avail_chars)
                {
                    break;
                }
                end = next_end;
                taken_chars += 1;
                if next_end - start >= avail_bytes || taken_chars >= avail_chars {
                    break;
                }
            }
            if end == start {
                end = line[start..]
                    .char_indices()
                    .nth(1)
                    .map_or(line.len(), |(rel, _)| start + rel);
                taken_chars = 1;
            }

            current.push_str(&line[start..end]);
            current_chars += taken_chars;
            start = end;
            if start < line.len() {
                flush_current(&mut chunks, &mut current, &mut current_chars, &open_fence);
            }
        }
        open_fence = next_fence;
    }

    if !current.is_empty() {
        if let Some((ch, len, _)) = &open_fence {
            if !current.ends_with('\n') {
                current.push('\n');
            }
            for _ in 0..*len {
                current.push(*ch);
            }
        }
        chunks.push(current);
    }

    chunks
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
        assert_eq!(
            strip_ansi_codes("\x1b]0;window title\x07progress 10%\rprogress 100%"),
            "progress 100%"
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

    #[test]
    fn display_width_and_truncate_handle_cjk_and_emoji() {
        assert_eq!(display_width("Note"), 4);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("中文a"), 5);
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(display_width(""), 0);
        assert_eq!(truncate_to_width("中文abc", 4), ("中文", 4));
        assert_eq!(truncate_to_width("中文abc", 3), ("中", 2));
        assert_eq!(truncate_to_width("👩‍🔬ok", 2), ("👩‍🔬", 2));
        assert_eq!(truncate_to_width("👩‍🔬ok", 1), ("", 0));
    }

    #[test]
    fn parse_markdown_handles_table_with_inline_code_pipes_and_uneven_cols() {
        let md = "Expr | Desc\n---|---\n`a | b` | bitwise OR\n`x \\| y`\n";
        let doc = parse_markdown(md);
        assert_eq!(doc.blocks.len(), 1);
        let MdBlock::Table(ref table) = doc.blocks[0] else {
            panic!("expected table, got {:?}", doc.blocks[0]);
        };
        assert_eq!(table.headers.len(), 2);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(inlines_to_plain_text(&table.rows[0][0]), "a | b");
        assert_eq!(inlines_to_plain_text(&table.rows[0][1]), "bitwise OR");
        assert_eq!(inlines_to_plain_text(&table.rows[1][0]), "x | y");
        assert_eq!(inlines_to_plain_text(&table.rows[1][1]), "");
    }

    #[test]
    fn compact_box_table_aligns_cjk_and_rejects_wide_tables() {
        let compact_md = "| 名称 | 数值 |\n|---|---:|\n| 抖音 | 99% |\n| ab | 5% |";
        let MdBlock::Table(ref t) = parse_markdown(compact_md).blocks[0] else {
            panic!("expected table");
        };
        let boxed = render_compact_box_table(t).expect("should fit as compact box table");
        let lines: Vec<&str> = boxed.lines().collect();
        assert_eq!(lines.len(), 6);
        let widths: Vec<usize> = lines.iter().map(|l| display_width(l)).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "widths={widths:?}");
        assert!(boxed.contains("│ 抖音 │  99% │"));

        let wide_md = "| Model | Context Window | Primary Strengths |\n|---|---|---|\n| Gemini 4 Argon | 1,000,000 tokens | Deep reasoning and multi-file refactoring |";
        let MdBlock::Table(ref wt) = parse_markdown(wide_md).blocks[0] else {
            panic!("expected wide table");
        };
        assert!(render_compact_box_table(wt).is_none());
    }

    #[test]
    fn parse_markdown_handles_spoilers_underline_tasks_and_expandable_quotes() {
        let md = "- [x] Done\n- [ ] Todo\n\nSecret ||hidden **data**|| and x || y || z and <u>under</u>\n\n>! Collapsible line 1\n>! Collapsible line 2";
        let doc = parse_markdown(md);
        assert!(matches!(doc.blocks[0], MdBlock::List { .. }));
        let MdBlock::Paragraph(ref p) = doc.blocks[1] else {
            panic!("expected paragraph");
        };
        assert!(p.iter().any(|n| matches!(n, MdInline::Spoiler(_))));
        assert!(p.iter().any(|n| matches!(n, MdInline::Underline(_))));
        assert!(
            p.iter()
                .any(|n| matches!(n, MdInline::Text(t) if t.contains("x || y || z")))
        );
        let MdBlock::BlockQuote { kind, .. } = &doc.blocks[2] else {
            panic!("expected blockquote");
        };
        assert_eq!(*kind, MdQuoteKind::Expandable);
    }

    #[test]
    fn split_markdown_chunks_reopens_fences_and_respects_cjk_limits() {
        let code = "println!(\"中文\");\n".repeat(20);
        let md = format!("```rust\n{code}```");
        let chunks = split_markdown_chunks(&md, 80, 140);
        assert!(chunks.len() > 2);
        for (idx, chunk) in chunks.iter().enumerate() {
            assert!(chunk.len() <= 140, "chunk {idx} bytes={}", chunk.len());
            assert!(
                chunk.chars().count() <= 80,
                "chunk {idx} chars={}",
                chunk.chars().count()
            );
            assert!(
                chunk.starts_with("```rust\n"),
                "chunk {idx} missing fence opener: {chunk:?}"
            );
            assert!(
                chunk.trim_end().ends_with("```"),
                "chunk {idx} missing fence closer: {chunk:?}"
            );
        }
    }
}
