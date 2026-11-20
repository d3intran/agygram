//! Shared artifact discovery and path-confinement helpers used by Telegram and QQ media delivery.

use crate::text::{contains_ascii_case_insensitive_str, looks_sensitive};
use rustc_hash::FxHashSet;
use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

/// System-prompt rule instructing `agy` never to emit files unsolicited, and to emit explicit
/// `[[send_file:...]]` directives when the user clearly asks to deliver a file to the chat.
pub(crate) const FILE_DELIVERY_PROMPT_GUIDE: &str = "\
文件发送规范（严格遵守）：平时引用文件路径、保存笔记或生成代码时，默认绝对不要向聊天发送文件。\
仅当用户明确要求将某个文件发送/传到当前对话或群里，或要求生成并返回图片/表情包/音视频时（例如「画一张…」「生成表情包」「把文件发过来」「发到群里」「导出发我」），才在回复末尾单独起一行输出 `[[send_file:/完整路径或scratch内文件名]]`（每行一个）。\
快捷规则：生图工具或子代理保存在当前会话 `brain/<conversation_id>/` 根目录下的图片/视频可直接填写原始完整路径发送，无需先 `cp` 到 `scratch/`，也无需再 `read_file` 二次查看；其他隐藏目录（如 `.gemini/` 外部）中的非媒体文件仍须先复制到工作区 `scratch/` 再输出该路径；严禁发送密钥、凭证、`.session` 或生产配置文件。";

/// Keep a candidate only if it resolves to a regular file inside one of `canonical_roots`.
/// Canonicalizing defeats `../` traversal and symlink escapes.
#[cfg(test)]
fn resolve_confined_artifact(candidate: &Path, canonical_roots: &[PathBuf]) -> Option<PathBuf> {
    let resolved = candidate.canonicalize().ok()?;
    if !resolved.is_file() {
        return None;
    }
    canonical_roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

/// Keeps a candidate only if it resolves via `tokio::fs` to a regular file inside one of
/// `canonical_roots`. Canonicalizing defeats `../` traversal and symlink escapes.
async fn resolve_confined_artifact_async(
    candidate: &Path,
    canonical_roots: &[PathBuf],
) -> Option<PathBuf> {
    let resolved = tokio::fs::canonicalize(candidate).await.ok()?;
    if !tokio::fs::metadata(&resolved)
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        return None;
    }
    canonical_roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

/// Returns true if the file extension corresponds to source code, configs, shell scripts,
/// text/docs, or binary executables that should never be auto-delivered as Documents.
#[cfg(test)]
#[must_use]
pub(crate) fn is_blocked_artifact_extension(ext: &str) -> bool {
    matches!(
        ext,
        // Source code
        "rs" | "py" | "js" | "ts" | "go" | "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "hh" | "hxx"
            | "cs" | "java" | "kt" | "kts" | "rb" | "php" | "swift" | "scala" | "r" | "lua"
            | "sql" | "html" | "htm" | "css" | "scss" | "sass" | "less" | "vue" | "svelte"
            | "jsx" | "tsx" | "dart" | "zig"
        // Configs & lockfiles
            | "toml" | "json" | "yaml" | "yml" | "lock" | "ini" | "cfg" | "conf" | "xml" | "properties"
        // Shell scripts
            | "sh" | "bash" | "zsh" | "fish" | "bat" | "cmd" | "ps1"
        // Text / docs
            | "md" | "txt" | "markdown" | "rst" | "log"
        // Binary executables / libraries
            | "bin" | "exe" | "so" | "dylib" | "dll" | "o" | "a" | "out" | "elf"
    )
}

/// Returns true if the extension is a recognized media format.
#[must_use]
pub(crate) fn is_recognized_media_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "mp4" | "mov" | "webm"
    )
}

/// Returns true if the extension is an explicit export deliverable.
#[must_use]
pub(crate) fn is_export_deliverable_extension(ext: &str) -> bool {
    matches!(ext, "pdf" | "zip" | "tar" | "gz" | "csv" | "xlsx" | "docx")
}

/// Returns true if a file located outside `scratch_dir` (in the general workspace) is an allowed deliverable.
#[must_use]
pub(crate) fn is_allowed_workspace_deliverable(ext: &str) -> bool {
    is_recognized_media_extension(ext) || is_export_deliverable_extension(ext)
}

/// Returns `true` if the extension represents a compiled binary, object archive, or shared library
/// that should never be auto-delivered as a chat artifact.
#[must_use]
pub(crate) fn is_binary_executable_extension(ext: &str) -> bool {
    matches!(
        ext,
        "bin" | "exe" | "so" | "dylib" | "dll" | "o" | "a" | "out" | "elf"
    )
}

/// Returns `true` if `resolved` enters any dot-prefixed hidden path component relative to its
/// matching canonical root (e.g. `.env`, `.git`, `.ssh`, `.config`, `.gemini`).
#[must_use]
fn has_hidden_component_under_roots(resolved: &Path, canonical_roots: &[PathBuf]) -> bool {
    if resolved
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'))
    {
        return true;
    }

    let best_root = canonical_roots
        .iter()
        .filter(|root| resolved.starts_with(root))
        .max_by_key(|root| root.components().count());

    let Some(root) = best_root else {
        return true;
    };
    let Ok(rel) = resolved.strip_prefix(root) else {
        return true;
    };
    rel.components().any(|comp| match comp {
        Component::Normal(seg) => seg.to_str().is_some_and(|s| s.starts_with('.')),
        _ => false,
    })
}

/// Evaluates whether a file is safe to deliver as an explicit artifact (confined, non-hidden,
/// non-sensitive, and non-binary-executable).
#[must_use]
fn is_safe_explicit_deliverable(resolved: &Path, canonical_roots: &[PathBuf]) -> bool {
    if has_hidden_component_under_roots(resolved, canonical_roots) {
        return false;
    }
    if looks_sensitive(&resolved.to_string_lossy()) {
        return false;
    }
    let Some(ext) = resolved.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext_lower = ext.to_ascii_lowercase();
    !ext_lower.is_empty() && !is_binary_executable_extension(&ext_lower)
}

/// Extensions allowed inside `scratch/` when the user explicitly asked for file delivery without
/// naming a specific source-code file.
#[must_use]
fn is_scratch_document_extension(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown"
            | "txt"
            | "html"
            | "htm"
            | "svg"
            | "json"
            | "yaml"
            | "yml"
            | "xml"
            | "rst"
            | "log"
            | "tsv"
            | "mp3"
            | "wav"
            | "ogg"
            | "silk"
    )
}

/// Returns `true` if `user_prompt` explicitly names `resolved`'s filename (case-insensitively).
fn prompt_names_file(user_prompt: &str, resolved: &Path) -> bool {
    let Some(name) = resolved.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name.len() < 3 {
        return false;
    }
    let lower_prompt = user_prompt.to_ascii_lowercase();
    let lower_name = name.to_ascii_lowercase();
    lower_prompt.contains(&lower_name)
}

/// Strips `[Attached ... File: ...]` synthetic headers prepended by agygram itself before
/// checking whether the human user actually requested file delivery.
fn strip_injected_attachment_headers(prompt: &str) -> Cow<'_, str> {
    if !prompt.contains("[Attached ") {
        return Cow::Borrowed(prompt);
    }
    let mut out = String::with_capacity(prompt.len());
    for line in prompt.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("[Attached ") && trimmed.ends_with(']') {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    Cow::Owned(out)
}

fn contains_delivery_negation_or_troubleshoot(clause_lower: &str) -> bool {
    const NEGATIONS: &[&str] = &[
        "没发",
        "没有发",
        "未发",
        "未能发",
        "发不了",
        "发不出",
        "不能发",
        "不支持发",
        "禁止发",
        "不要发",
        "别发",
        "不用发",
        "无需发",
        "为什么",
        "为何",
        "怎么没",
        "didn't send",
        "did not send",
        "wasn't sent",
        "was not sent",
        "not sent",
        "failed to send",
        "cannot send",
        "can't send",
        "don't send",
        "do not send",
    ];
    NEGATIONS.iter().any(|n| clause_lower.contains(n))
}

fn clause_explicitly_requests_file_delivery(clause_lower: &str) -> bool {
    if clause_lower.is_empty() || contains_delivery_negation_or_troubleshoot(clause_lower) {
        return false;
    }

    const DIRECT_ZH: &[&str] = &[
        "发过来",
        "发给我",
        "发到群",
        "发群里",
        "发群内",
        "传过来",
        "传给我",
        "传到群",
        "发送过来",
        "发送给我",
        "发送到群",
        "发送文件",
        "发个文件",
        "发份文件",
        "把文件发",
        "把产物发",
        "附件发",
        "下载发我",
    ];
    if DIRECT_ZH.iter().any(|pat| clause_lower.contains(pat)) {
        return true;
    }

    const DIRECT_EN: &[&str] = &[
        "send me the file",
        "send the file",
        "send this file",
        "send that file",
        "send file to",
        "send to group",
        "send to the group",
        "send to chat",
        "send it over",
        "attach the file",
        "deliver the file",
        "send back the file",
    ];
    if DIRECT_EN.iter().any(|pat| clause_lower.contains(pat)) {
        return true;
    }

    let has_zh_verb = ["发我", "发出来", "发送", "发一份", "传到", "导出并发", "把"]
        .iter()
        .any(|v| clause_lower.contains(v))
        && ["发", "传", "递"]
            .iter()
            .any(|ch| clause_lower.contains(ch));

    let has_en_verb = ["send ", "attach ", "upload ", "deliver "]
        .iter()
        .any(|v| clause_lower.contains(v));

    if !(has_zh_verb || has_en_verb) {
        return false;
    }

    const FILE_CUES: &[&str] = &[
        "文件",
        "文档",
        "附件",
        "报告",
        "表格",
        "压缩包",
        "图表",
        "图片",
        "照片",
        "音频",
        "视频",
        "file://",
        ".md",
        ".markdown",
        ".txt",
        ".pdf",
        ".csv",
        ".xlsx",
        ".docx",
        ".zip",
        ".tar",
        ".gz",
        ".html",
        ".htm",
        ".json",
        ".yaml",
        ".yml",
        ".xml",
        ".svg",
        ".png",
        ".jpg",
        ".jpeg",
        ".gif",
        ".webp",
        ".mp4",
        ".mp3",
        ".wav",
        ".py",
        ".rs",
        ".toml",
        ".sh",
    ];
    if FILE_CUES.iter().any(|cue| clause_lower.contains(cue)) {
        return true;
    }

    // Also match English `"send me <something>"` / `"send <target>"` when paired with file nouns.
    has_en_verb
        && [
            "file",
            "document",
            "report",
            "attachment",
            "archive",
            "image",
            "photo",
            "chart",
        ]
        .iter()
        .any(|noun| clause_lower.contains(noun))
}

/// Returns `true` only when the user's message affirmatively asks to send/deliver a file or
/// artifact to the current chat or group.
#[must_use]
pub(crate) fn prompt_explicitly_requests_file_delivery(user_prompt: &str) -> bool {
    let stripped = strip_injected_attachment_headers(user_prompt);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();
    lower
        .split(['\n', '。', '？', '?', '！', '!', '；', ';'])
        .any(|clause| clause_explicitly_requests_file_delivery(clause.trim()))
}

fn clean_send_file_arg(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '`'))
        .trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = strip_file_scheme_or_skip_url(trimmed)?;
    let final_path = without_scheme.trim();
    (!final_path.is_empty()).then(|| final_path.to_string())
}

fn strip_markers_from_line(line: &str, out_paths: &mut Vec<String>) -> (String, bool) {
    if !contains_ascii_case_insensitive_str(line, "[[send_file:") {
        return (line.to_string(), false);
    }
    let lower = line.to_ascii_lowercase();

    let mut rebuilt = String::with_capacity(line.len());
    let mut cursor = 0usize;
    let mut matched = false;

    while let Some(rel_start) = lower[cursor..].find("[[send_file:") {
        let abs_start = cursor + rel_start;
        rebuilt.push_str(&line[cursor..abs_start]);
        let arg_start = abs_start + "[[send_file:".len();
        if let Some(rel_end) = line[arg_start..].find("]]") {
            let arg_end = arg_start + rel_end;
            if let Some(path) = clean_send_file_arg(&line[arg_start..arg_end]) {
                out_paths.push(path);
            }
            cursor = arg_end + 2;
            matched = true;
        } else {
            rebuilt.push_str(&line[abs_start..]);
            cursor = line.len();
            break;
        }
    }
    if cursor < line.len() {
        rebuilt.push_str(&line[cursor..]);
    }
    (rebuilt, matched)
}

fn collapse_excess_newlines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut blank_run = 0usize;
    for seg in input.split_inclusive('\n') {
        if seg.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 2 {
                out.push_str(seg);
            }
        } else {
            blank_run = 0;
            out.push_str(seg);
        }
    }
    out.trim_end_matches(['\r', '\n']).to_string()
}

/// Extracts `[[send_file:...]]` markers outside fenced code blocks, returning the cleaned display
/// text alongside the raw path arguments.
#[must_use]
pub(crate) fn extract_explicit_send_files(text: &str) -> (String, Vec<String>) {
    if !contains_ascii_case_insensitive_str(text, "[[send_file:") {
        return (text.to_string(), Vec::new());
    }

    let mut cleaned = String::with_capacity(text.len());
    let mut paths = Vec::new();
    let mut active_fence: Option<(u8, usize)> = None;

    for seg in text.split_inclusive('\n') {
        let line = seg.trim_end_matches(['\r', '\n']);
        let eol = &seg[line.len()..];
        let trimmed_start = line.trim_start();

        if let Some((f_char, f_len)) = active_fence {
            let close_count = trimmed_start.bytes().take_while(|&b| b == f_char).count();
            if close_count >= f_len && trimmed_start[close_count..].trim().is_empty() {
                active_fence = None;
            }
            cleaned.push_str(seg);
            continue;
        }

        if let Some(first_byte) = trimmed_start.as_bytes().first().copied()
            && (first_byte == b'`' || first_byte == b'~')
        {
            let run = trimmed_start
                .bytes()
                .take_while(|&b| b == first_byte)
                .count();
            if run >= 3 && (first_byte == b'~' || !trimmed_start[run..].contains('`')) {
                active_fence = Some((first_byte, run));
                cleaned.push_str(seg);
                continue;
            }
        }

        let (stripped_line, had_marker) = strip_markers_from_line(line, &mut paths);
        if had_marker && stripped_line.trim().is_empty() {
            continue;
        }
        cleaned.push_str(&stripped_line);
        cleaned.push_str(eol);
    }

    (collapse_excess_newlines(&cleaned), paths)
}

/// Masks complete or trailing `[[send_file:...]]` markers during live Telegram draft streaming.
#[must_use]
pub(crate) fn strip_send_file_markers_for_draft(text: &str) -> Cow<'_, str> {
    if !contains_ascii_case_insensitive_str(text, "[[send_file:") {
        return Cow::Borrowed(text);
    }
    let (cleaned, _) = extract_explicit_send_files(text);
    Cow::Owned(cleaned)
}

/// Evaluates whether a candidate file is deliverable as an artifact based on its path and location.
///
/// - Extensionless files (e.g. ELF binaries like `agygram`) are blocked everywhere.
/// - Blocked extensions (source code, configs, scripts, text/doc, binaries) are blocked everywhere.
/// - Outside `scratch_dir` (in the general workspace), strictly only recognized media or export deliverables are allowed.
/// - Inside `scratch_dir`, files are allowed as long as they are not blocked code/binary or sensitive.
#[cfg(test)]
#[must_use]
pub(crate) fn is_deliverable_artifact_path(path: &Path, is_in_scratch: bool) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        // Extensionless files (e.g. ELF binaries like agygram) are blocked everywhere.
        return false;
    };
    let ext_lower = ext.to_ascii_lowercase();

    if is_blocked_artifact_extension(&ext_lower) {
        return false;
    }

    if !is_in_scratch && !is_allowed_workspace_deliverable(&ext_lower) {
        return false;
    }

    true
}

/// Discovers scratch directories from canonical roots.
#[cfg(test)]
pub(crate) fn collect_scratch_roots_sync(canonical_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut scratch_roots: Vec<PathBuf> = Vec::new();
    for root in canonical_roots {
        if root.file_name().and_then(|n| n.to_str()) == Some("scratch") {
            if !scratch_roots.contains(root) {
                scratch_roots.push(root.clone());
            }
        } else {
            let child = root.join("scratch");
            if let Ok(canon_child) = child.canonicalize()
                && canon_child.is_dir()
                && !scratch_roots.contains(&canon_child)
            {
                scratch_roots.push(canon_child);
            }
        }
    }
    scratch_roots
}

/// Asynchronously discovers scratch directories from canonical roots.
pub(crate) async fn collect_scratch_roots_async(canonical_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut scratch_roots: Vec<PathBuf> = Vec::new();
    for root in canonical_roots {
        if root.file_name().and_then(|n| n.to_str()) == Some("scratch") {
            if !scratch_roots.contains(root) {
                scratch_roots.push(root.clone());
            }
        } else {
            let child = root.join("scratch");
            if let Ok(canon_child) = tokio::fs::canonicalize(&child).await
                && tokio::fs::metadata(&canon_child)
                    .await
                    .is_ok_and(|m| m.is_dir())
                && !scratch_roots.contains(&canon_child)
            {
                scratch_roots.push(canon_child);
            }
        }
    }
    scratch_roots
}

struct ScratchSnapshot {
    root: PathBuf,
    entries: FxHashSet<String>,
}

/// Strips a leading `file://` scheme (or label-glued `file://`), while rejecting non-file URLs
/// like `https://...` or `http://...`. Returns `None` when the token is a non-file URL.
fn strip_file_scheme_or_skip_url(token: &str) -> Option<&str> {
    let Some(scheme_pos) = token.find("://") else {
        return Some(token);
    };
    let prefix = &token[..scheme_pos];
    if !prefix.ends_with("file") {
        return None;
    }
    if prefix.len() > 4 {
        let prev_byte = prefix.as_bytes()[prefix.len() - 5];
        if prev_byte.is_ascii_alphanumeric() {
            return None;
        }
    }
    let after = &token[scheme_pos + 3..];
    Some(
        after
            .strip_prefix("localhost")
            .filter(|s| s.starts_with('/'))
            .unwrap_or(after),
    )
}

fn collect_root_strings(allowed_dirs: &[PathBuf], canonical_roots: &[PathBuf]) -> Vec<String> {
    let mut root_strs = Vec::with_capacity(canonical_roots.len() + allowed_dirs.len());
    for root in canonical_roots.iter().chain(allowed_dirs.iter()) {
        if let Some(s) = root.to_str()
            && !s.is_empty()
            && !root_strs.iter().any(|existing| existing == s)
        {
            root_strs.push(s.to_string());
        }
    }
    root_strs
}

/// Pure prose scanner that avoids per-word allocations and syscalls by checking bare tokens
/// against a pre-fetched snapshot of `scratch` directory entry names.
fn collect_candidate_paths(
    output_text: &str,
    root_strs: &[String],
    scratch_snapshots: &[ScratchSnapshot],
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen_candidates: FxHashSet<PathBuf> = FxHashSet::default();
    let mut path_buf = PathBuf::new();

    for raw in output_text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '，' | '。'
                    | '、'
                    | '；'
                    | '：'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '「'
                    | '」'
                    | '《'
                    | '》'
            )
    }) {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '"'
                    | '\''
                    | '<'
                    | '>'
                    | '`'
                    | '*'
                    | ','
                    | ';'
                    | '，'
                    | '。'
                    | '、'
                    | '；'
                    | '：'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '「'
                    | '」'
            )
        });
        let token = token.trim_end_matches(['.', ':', '!', '?']);
        let token = token.rsplit_once("](").map_or(token, |(_, target)| {
            target.trim_end_matches([')', '.', ':', '!', '?'])
        });
        if token.is_empty() {
            continue;
        }
        let Some(token) = strip_file_scheme_or_skip_url(token) else {
            continue;
        };
        if token.is_empty() {
            continue;
        }

        if Path::new(token).is_absolute() {
            path_buf.clear();
            path_buf.push(token);
            if seen_candidates.insert(path_buf.clone()) {
                candidates.push(path_buf.clone());
            }
            continue;
        }

        if token.contains('/') || (cfg!(windows) && token.contains('\\')) {
            let mut matched_glued_root = false;
            for root_str in root_strs {
                if let Some(idx) = token.find(root_str.as_str()) {
                    path_buf.clear();
                    path_buf.push(&token[idx..]);
                    if seen_candidates.insert(path_buf.clone()) {
                        candidates.push(path_buf.clone());
                    }
                    matched_glued_root = true;
                }
            }
            if !matched_glued_root {
                let first_comp = token.split(['/', '\\']).next().unwrap_or("");
                if !first_comp.is_empty() {
                    for snap in scratch_snapshots {
                        if snap.entries.contains(first_comp) {
                            path_buf.clear();
                            path_buf.push(&snap.root);
                            path_buf.push(token);
                            if seen_candidates.insert(path_buf.clone()) {
                                candidates.push(path_buf.clone());
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Bare filename: zero syscalls unless it exists in a snapshotted scratch directory.
        for snap in scratch_snapshots {
            if snap.entries.contains(token) {
                path_buf.clear();
                path_buf.push(&snap.root);
                path_buf.push(token);
                if seen_candidates.insert(path_buf.clone()) {
                    candidates.push(path_buf.clone());
                }
            }
        }
    }

    candidates
}

/// Synchronous reference implementation retained under `#[cfg(test)]` to cross-check
/// [`collect_artifact_paths_async`].
#[cfg(test)]
#[must_use]
pub(crate) fn collect_artifact_paths(output_text: &str, allowed_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = dir.canonicalize()
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    if canonical_roots.is_empty() {
        return Vec::new();
    }

    let scratch_roots = collect_scratch_roots_sync(&canonical_roots);
    let mut scratch_snapshots = Vec::new();
    for root in &scratch_roots {
        let mut entries = FxHashSet::default();
        if let Ok(rd) = std::fs::read_dir(root) {
            for entry in rd.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && is_deliverable_artifact_path(Path::new(name), true)
                {
                    entries.insert(name.to_owned());
                }
            }
        }
        if !entries.is_empty() {
            scratch_snapshots.push(ScratchSnapshot {
                root: root.clone(),
                entries,
            });
        }
    }

    let root_strs = collect_root_strings(allowed_dirs, &canonical_roots);
    let candidates = collect_candidate_paths(output_text, &root_strs, &scratch_snapshots);

    let mut found = Vec::new();
    let mut seen = FxHashSet::default();
    for candidate in candidates {
        if let Some(resolved) = resolve_confined_artifact(&candidate, &canonical_roots) {
            let is_in_scratch = scratch_roots.iter().any(|sr| resolved.starts_with(sr));
            if is_deliverable_artifact_path(&resolved, is_in_scratch)
                && seen.insert(resolved.clone())
            {
                found.push(resolved);
            }
        }
    }
    found
}

/// Collects artifact paths mentioned in model output, confined to `allowed_dirs`, using
/// `tokio::fs` directory snapshotting so Tokio worker threads never block on per-word syscalls.
#[cfg(test)]
pub(crate) async fn collect_artifact_paths_async(
    output_text: &str,
    allowed_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = tokio::fs::canonicalize(dir).await
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    if canonical_roots.is_empty() {
        return Vec::new();
    }

    let scratch_roots = collect_scratch_roots_async(&canonical_roots).await;
    let mut scratch_snapshots = Vec::new();
    for root in &scratch_roots {
        let mut entries = FxHashSet::default();
        if let Ok(mut rd) = tokio::fs::read_dir(root).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && is_deliverable_artifact_path(Path::new(name), true)
                {
                    entries.insert(name.to_owned());
                }
            }
        }
        if !entries.is_empty() {
            scratch_snapshots.push(ScratchSnapshot {
                root: root.clone(),
                entries,
            });
        }
    }

    let root_strs = collect_root_strings(allowed_dirs, &canonical_roots);
    let candidates = collect_candidate_paths(output_text, &root_strs, &scratch_snapshots);

    let mut found = Vec::new();
    let mut seen = FxHashSet::default();
    for candidate in candidates {
        if let Some(resolved) = resolve_confined_artifact_async(&candidate, &canonical_roots).await
        {
            let is_in_scratch = scratch_roots.iter().any(|sr| resolved.starts_with(sr));
            if is_deliverable_artifact_path(&resolved, is_in_scratch)
                && seen.insert(resolved.clone())
            {
                found.push(resolved);
            }
        }
    }
    found
}

async fn resolve_explicit_marker_candidate(
    raw_arg: &str,
    canonical_roots: &[PathBuf],
    scratch_roots: &[PathBuf],
) -> Option<PathBuf> {
    let expanded_storage: Option<PathBuf> = if let Some(rest) = raw_arg.strip_prefix("~/") {
        dirs::home_dir().map(|home| home.join(rest))
    } else {
        None
    };
    let path = expanded_storage
        .as_deref()
        .unwrap_or_else(|| Path::new(raw_arg));
    if path.is_absolute() {
        return resolve_confined_artifact_async(path, canonical_roots).await;
    }
    for sr in scratch_roots {
        if let Some(resolved) =
            resolve_confined_artifact_async(&sr.join(path), canonical_roots).await
        {
            return Some(resolved);
        }
    }
    for cr in canonical_roots {
        if let Some(resolved) =
            resolve_confined_artifact_async(&cr.join(path), canonical_roots).await
        {
            return Some(resolved);
        }
    }
    None
}

fn is_allowed_explicit_prose_candidate(
    resolved: &Path,
    is_in_scratch: bool,
    user_prompt: &str,
) -> bool {
    let Some(ext_lower) = resolved
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    if is_allowed_workspace_deliverable(&ext_lower) {
        return true;
    }
    if is_in_scratch && is_scratch_document_extension(&ext_lower) {
        return true;
    }
    prompt_names_file(user_prompt, resolved)
}

/// Resolves the active conversation's Antigravity brain directory
/// (`~/.gemini/antigravity-cli/brain/<conv_id>`), rejecting invalid or traversal-like IDs.
pub(crate) async fn resolve_conversation_brain_dir_async(conv_id: Option<&str>) -> Option<PathBuf> {
    let cid = conv_id?.trim();
    if cid.is_empty()
        || !crate::models::is_safe_cli_value(cid)
        || cid.contains('/')
        || cid.contains('\\')
        || cid == "."
        || cid == ".."
    {
        return None;
    }
    for brain_root in crate::executor::AgyExecutor::get_brain_dirs_async().await {
        let candidate = brain_root.join(cid);
        if let Ok(canon) = tokio::fs::canonicalize(&candidate).await
            && tokio::fs::metadata(&canon)
                .await
                .is_ok_and(|meta| meta.is_dir())
        {
            return Some(canon);
        }
    }
    None
}

/// Returns `true` when `resolved` is a safe direct-child media file (image/video) inside the
/// active conversation's canonical brain directory (where `image-generator` writes outputs).
/// Subdirectories (e.g. `.system_generated/`), dotfiles, sensitive names, and non-media files
/// are strictly rejected.
#[must_use]
fn is_safe_brain_media_deliverable(resolved: &Path, canon_brain_dir: &Path) -> bool {
    if resolved.parent() != Some(canon_brain_dir) {
        return false;
    }
    let Some(file_name) = resolved.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if file_name.starts_with('.') || looks_sensitive(file_name) {
        return false;
    }
    let Some(ext) = resolved.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    is_recognized_media_extension(&ext.to_ascii_lowercase())
}

async fn resolve_brain_media_candidate_async(
    raw_arg: &str,
    canon_brain_dir: &Path,
) -> Option<PathBuf> {
    let expanded_storage: Option<PathBuf> = if let Some(rest) = raw_arg.strip_prefix("~/") {
        dirs::home_dir().map(|home| home.join(rest))
    } else {
        None
    };
    let path = expanded_storage
        .as_deref()
        .unwrap_or_else(|| Path::new(raw_arg));
    let candidate = if path.is_absolute() {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(canon_brain_dir.join(path))
    };
    let resolved = tokio::fs::canonicalize(candidate.as_ref()).await.ok()?;
    if !tokio::fs::metadata(&resolved)
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        return None;
    }
    is_safe_brain_media_deliverable(&resolved, canon_brain_dir).then_some(resolved)
}

/// Collects deliverable artifact files ONLY when explicitly requested:
/// 1. Always extracts and strips `[[send_file:...]]` markers from `ai_output_text`. If present,
///    delivers strictly those confined, non-hidden, non-sensitive, non-binary files (plus
///    direct-child media outputs inside `active_brain_dir` when provided).
/// 2. If no `[[send_file:...]]` markers were emitted, delivers NOTHING unless `user_prompt`
///    affirmatively requests sending a file — in which case prose/prompt paths are scanned safely
///    (allowing `.md`/`.txt`/`.html`/media/exports in `scratch/`, direct-child media in
///    `active_brain_dir`, and any filename explicitly named by the user).
pub(crate) async fn collect_explicit_deliverables_with_brain_async(
    user_prompt: &str,
    ai_output_text: &str,
    allowed_dirs: &[PathBuf],
    active_brain_dir: Option<&Path>,
) -> (String, Vec<PathBuf>) {
    let (cleaned_text, raw_markers) = extract_explicit_send_files(ai_output_text);
    if allowed_dirs.is_empty() && active_brain_dir.is_none() {
        return (cleaned_text, Vec::new());
    }

    let has_markers = !raw_markers.is_empty();
    let user_requested = prompt_explicitly_requests_file_delivery(user_prompt);
    if !has_markers && !user_requested {
        return (cleaned_text, Vec::new());
    }

    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = tokio::fs::canonicalize(dir).await
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    let canon_brain_dir = match active_brain_dir {
        Some(dir) => match tokio::fs::canonicalize(dir).await {
            Ok(canon)
                if tokio::fs::metadata(&canon)
                    .await
                    .is_ok_and(|meta| meta.is_dir()) =>
            {
                Some(canon)
            }
            _ => None,
        },
        None => None,
    };
    if canonical_roots.is_empty() && canon_brain_dir.is_none() {
        return (cleaned_text, Vec::new());
    }

    let scratch_roots = collect_scratch_roots_async(&canonical_roots).await;
    let mut found = Vec::new();
    let mut seen = FxHashSet::default();

    if has_markers {
        for raw_arg in raw_markers {
            if let Some(resolved) =
                resolve_explicit_marker_candidate(&raw_arg, &canonical_roots, &scratch_roots).await
                && is_safe_explicit_deliverable(&resolved, &canonical_roots)
            {
                if seen.insert(resolved.clone()) {
                    found.push(resolved);
                }
                continue;
            }
            if let Some(ref brain_dir) = canon_brain_dir
                && let Some(resolved) =
                    resolve_brain_media_candidate_async(&raw_arg, brain_dir).await
                && seen.insert(resolved.clone())
            {
                found.push(resolved);
            }
        }
        return (cleaned_text, found);
    }

    let mut scratch_snapshots = Vec::new();
    for root in &scratch_roots {
        let mut entries = FxHashSet::default();
        if let Ok(mut rd) = tokio::fs::read_dir(root).await {
            while let Ok(Some(entry)) = rd.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && !name.starts_with('.')
                    && !looks_sensitive(name)
                    && let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str())
                {
                    let ext_lower = ext.to_ascii_lowercase();
                    if !is_binary_executable_extension(&ext_lower)
                        && (is_allowed_workspace_deliverable(&ext_lower)
                            || is_scratch_document_extension(&ext_lower)
                            || prompt_names_file(user_prompt, Path::new(name)))
                    {
                        entries.insert(name.to_owned());
                    }
                }
            }
        }
        if !entries.is_empty() {
            scratch_snapshots.push(ScratchSnapshot {
                root: root.clone(),
                entries,
            });
        }
    }

    let mut root_strs = collect_root_strings(allowed_dirs, &canonical_roots);
    if let Some(ref brain_dir) = canon_brain_dir
        && let Some(s) = brain_dir.to_str()
        && !s.is_empty()
        && !root_strs.iter().any(|existing| existing == s)
    {
        root_strs.push(s.to_string());
    }
    let user_prompt_clean = strip_injected_attachment_headers(user_prompt);
    let mut candidates = collect_candidate_paths(&cleaned_text, &root_strs, &scratch_snapshots);
    candidates.extend(collect_candidate_paths(
        &user_prompt_clean,
        &root_strs,
        &scratch_snapshots,
    ));

    for candidate in candidates {
        if let Some(resolved) = resolve_confined_artifact_async(&candidate, &canonical_roots).await
            && is_safe_explicit_deliverable(&resolved, &canonical_roots)
        {
            let is_in_scratch = scratch_roots.iter().any(|sr| resolved.starts_with(sr));
            if is_allowed_explicit_prose_candidate(&resolved, is_in_scratch, user_prompt)
                && seen.insert(resolved.clone())
            {
                found.push(resolved);
            }
            continue;
        }
        if let Some(ref brain_dir) = canon_brain_dir
            && let Ok(resolved) = tokio::fs::canonicalize(&candidate).await
            && tokio::fs::metadata(&resolved)
                .await
                .is_ok_and(|meta| meta.is_file())
            && is_safe_brain_media_deliverable(&resolved, brain_dir)
            && seen.insert(resolved.clone())
        {
            found.push(resolved);
        }
    }

    (cleaned_text, found)
}

/// Convenience wrapper without an active conversation brain directory (used by tests and callers
/// without a conversation context).
#[cfg(test)]
pub(crate) async fn collect_explicit_deliverables_async(
    user_prompt: &str,
    ai_output_text: &str,
    allowed_dirs: &[PathBuf],
) -> (String, Vec<PathBuf>) {
    collect_explicit_deliverables_with_brain_async(user_prompt, ai_output_text, allowed_dirs, None)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp fixture should be creatable");
        base
    }

    #[test]
    fn files_outside_the_allowed_roots_are_refused() {
        let base = fixture("agygram_confine_plain");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let inside = workspace.join("chart.png");
        std::fs::write(&inside, b"png-bytes").unwrap();
        let secret = outside.join("credentials.json");
        std::fs::write(&secret, b"secret").unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&inside, &roots),
            Some(inside.canonicalize().unwrap())
        );
        assert_eq!(resolve_confined_artifact(&secret, &roots), None);
        assert_eq!(
            resolve_confined_artifact(Path::new("/etc/hostname"), &roots),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn directories_are_never_delivered() {
        let base = fixture("agygram_confine_dir");
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("nested.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("nested.json"), &roots),
            None,
            "a directory must not be treated as an artifact"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifact_paths_are_found_in_prose_and_file_urls() {
        let base = fixture("agygram_collect_paths");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("karina_event.png"), b"png").unwrap();
        std::fs::write(scratch.join("chart.jpg"), b"jpg").unwrap();
        std::fs::write(scratch.join("diagram.svg"), b"svg").unwrap();
        std::fs::write(workspace.join("report.pdf"), b"pdf").unwrap();
        std::fs::write(workspace.join("README.md"), b"readme").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "1. 活动照\n   文件路径：{}/karina_event.png）\n2. `karina_event.png`\n3. chart.jpg.\n4. https://example.com/a.png\n5. README.md\n6. file://{}/diagram.svg\n7. [报告](file://{}/report.pdf)\n8. ![图](chart.jpg)",
            scratch.display(),
            scratch.display(),
            workspace.display()
        );

        let found = collect_artifact_paths(&text, &roots);
        let mut names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["chart.jpg", "diagram.svg", "karina_event.png", "report.pdf"]
        );
        assert!(
            !names.contains(&"README.md".to_string()),
            "a bare project file in the workspace root must not be auto-sent"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn artifact_paths_async_matches_sync_collection() {
        let base = fixture("agygram_collect_paths_async");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("plot.png"), b"png").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "Generated plot.png and file://{}/plot.png plus https://example.com/plot.png",
            scratch.display()
        );

        let sync_paths = collect_artifact_paths(&text, &roots);
        let async_paths = collect_artifact_paths_async(&text, &roots).await;
        assert_eq!(sync_paths, async_paths);
        assert_eq!(async_paths.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifact_paths_cannot_escape_roots() {
        let base = fixture("agygram_collect_escape");
        let workspace = base.join("ws");
        let outside = base.join("out");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.png"), b"x").unwrap();

        let roots = vec![workspace];
        let text = format!("见 {}", outside.join("secret.png").display());
        assert!(
            collect_artifact_paths(&text, &roots).is_empty(),
            "a path outside the allowed roots must not be collected"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn traversal_and_symlink_escapes_are_refused() {
        let base = fixture("agygram_confine_escape");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let secret = outside.join("wallet.json");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, workspace.join("art.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("art.json"), &roots),
            None,
            "symlink escape must not deliver the target"
        );
        assert_eq!(
            resolve_confined_artifact(&workspace.join("../../outside/wallet.json"), &roots),
            None,
            "dot-dot traversal must not leave the workspace"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn extension_blocking_and_classification_rules() {
        for blocked in [
            "rs", "py", "js", "ts", "go", "c", "cpp", "h", "html", "css", "toml", "json", "yaml",
            "yml", "lock", "sh", "bash", "zsh", "md", "txt", "bin", "exe", "so",
        ] {
            assert!(
                is_blocked_artifact_extension(blocked),
                "{blocked} must be marked as blocked"
            );
        }

        for allowed_media in ["png", "jpg", "jpeg", "webp", "gif", "mp4", "mov", "webm"] {
            assert!(
                is_recognized_media_extension(allowed_media),
                "{allowed_media} must be recognized media"
            );
            assert!(
                is_allowed_workspace_deliverable(allowed_media),
                "{allowed_media} must be allowed workspace deliverable"
            );
        }

        for export in ["pdf", "zip", "tar", "gz", "csv", "xlsx", "docx"] {
            assert!(
                is_export_deliverable_extension(export),
                "{export} must be export deliverable"
            );
            assert!(
                is_allowed_workspace_deliverable(export),
                "{export} must be allowed workspace deliverable"
            );
        }

        // General workspace paths (outside scratch_dir)
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/agygram-src/Cargo.toml"),
            false
        ));
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/agygram"),
            false
        ));
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/update-agygram.sh"),
            false
        ));
        assert!(is_deliverable_artifact_path(
            Path::new("/home/azureuser/report.pdf"),
            false
        ));
        assert!(is_deliverable_artifact_path(
            Path::new("/home/azureuser/plot.png"),
            false
        ));

        // Scratch dir paths
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/scratch/Cargo.toml"),
            true
        ));
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/scratch/agygram"),
            true
        ));
        assert!(!is_deliverable_artifact_path(
            Path::new("/home/azureuser/scratch/update-agygram.sh"),
            true
        ));
        assert!(is_deliverable_artifact_path(
            Path::new("/home/azureuser/scratch/report.pdf"),
            true
        ));
        assert!(is_deliverable_artifact_path(
            Path::new("/home/azureuser/scratch/plot.png"),
            true
        ));
    }

    #[test]
    fn mentioning_cargo_toml_agygram_script_is_not_collected_while_media_and_exports_are() {
        let base = fixture("agygram_code_binary_blocked");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();

        // 1. Blocked files in general workspace:
        // Cargo.toml (config), agygram (ELF binary, extensionless), update-agygram.sh (shell script),
        // main.rs (source code), README.md (markdown doc).
        let toml_file = workspace.join("Cargo.toml");
        std::fs::write(&toml_file, b"[package]\nname = \"agygram\"\n").unwrap();
        let binary_file = workspace.join("agygram");
        std::fs::write(&binary_file, b"\x7fELFbinary").unwrap();
        let script_file = workspace.join("update-agygram.sh");
        std::fs::write(&script_file, b"#!/bin/bash\necho update\n").unwrap();
        let rs_file = workspace.join("main.rs");
        std::fs::write(&rs_file, b"fn main() {}\n").unwrap();
        let md_file = workspace.join("README.md");
        std::fs::write(&md_file, b"# Readme\n").unwrap();

        // 2. Allowed files in general workspace:
        // report.pdf (export deliverable), plot.png (recognized media), data.csv (export deliverable)
        let pdf_file = workspace.join("report.pdf");
        std::fs::write(&pdf_file, b"%PDF-1.4\n").unwrap();
        let png_file = workspace.join("plot.png");
        std::fs::write(&png_file, b"\x89PNG\r\n").unwrap();
        let csv_file = workspace.join("data.csv");
        std::fs::write(&csv_file, b"a,b,c\n").unwrap();

        // 3. Blocked files in scratch_dir:
        // temp.py (source code), notes.txt (doc), scratch_bin (extensionless)
        std::fs::write(scratch.join("temp.py"), b"print('hi')\n").unwrap();
        std::fs::write(scratch.join("notes.txt"), b"some notes\n").unwrap();
        std::fs::write(scratch.join("scratch_bin"), b"binary").unwrap();

        // 4. Allowed files in scratch_dir:
        // scratch_chart.jpg (media)
        std::fs::write(scratch.join("scratch_chart.jpg"), b"\xff\xd8\xff").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "Please check file://{cargo} and run {bin} or file://{script}.\n\
             Also look at {rs} and {md}.\n\
             Scratch temp: temp.py and notes.txt and scratch_bin.\n\
             Deliverables generated: file://{pdf}, [图片](file://{png}), {csv}, and scratch_chart.jpg",
            cargo = toml_file.display(),
            bin = binary_file.display(),
            script = script_file.display(),
            rs = rs_file.display(),
            md = md_file.display(),
            pdf = pdf_file.display(),
            png = png_file.display(),
            csv = csv_file.display(),
        );

        let found = collect_artifact_paths(&text, &roots);
        let mut names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();

        assert_eq!(
            names,
            vec!["data.csv", "plot.png", "report.pdf", "scratch_chart.jpg"]
        );
        assert!(!names.contains(&"Cargo.toml".to_string()));
        assert!(!names.contains(&"agygram".to_string()));
        assert!(!names.contains(&"update-agygram.sh".to_string()));
        assert!(!names.contains(&"main.rs".to_string()));
        assert!(!names.contains(&"README.md".to_string()));
        assert!(!names.contains(&"temp.py".to_string()));
        assert!(!names.contains(&"notes.txt".to_string()));
        assert!(!names.contains(&"scratch_bin".to_string()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn async_mentioning_cargo_toml_agygram_script_is_not_collected() {
        let base = fixture("agygram_async_code_blocked");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();

        let toml_file = workspace.join("Cargo.toml");
        std::fs::write(&toml_file, b"[package]\n").unwrap();
        let binary_file = workspace.join("agygram");
        std::fs::write(&binary_file, b"ELF").unwrap();
        let script_file = workspace.join("update-agygram.sh");
        std::fs::write(&script_file, b"#!/bin/sh").unwrap();
        let pdf_file = workspace.join("report.pdf");
        std::fs::write(&pdf_file, b"PDF").unwrap();
        let png_file = workspace.join("plot.png");
        std::fs::write(&png_file, b"PNG").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "file://{cargo} {bin} {script} {pdf} {png}",
            cargo = toml_file.display(),
            bin = binary_file.display(),
            script = script_file.display(),
            pdf = pdf_file.display(),
            png = png_file.display(),
        );

        let found = collect_artifact_paths_async(&text, &roots).await;
        let mut names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();

        assert_eq!(names, vec!["plot.png", "report.pdf"]);
        assert!(!names.contains(&"Cargo.toml".to_string()));
        assert!(!names.contains(&"agygram".to_string()));
        assert!(!names.contains(&"update-agygram.sh".to_string()));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn explicit_delivery_intent_matches_only_affirmative_send_requests() {
        assert!(prompt_explicitly_requests_file_delivery(
            "我把agygram进程升级了，试试把上次的👉 han_dynasty_50_questions.md发过来"
        ));
        assert!(prompt_explicitly_requests_file_delivery(
            "帮我生成一份汉代历史报告并把文件发到群里"
        ));
        assert!(prompt_explicitly_requests_file_delivery(
            "Please send me the report.pdf file"
        ));
        assert!(prompt_explicitly_requests_file_delivery(
            "[Attached Image File: /home/azureuser/scratch/m1-photo.jpg]\n把整理好的 notes.md 发给我"
        ));

        // Must NOT trigger on troubleshooting questions, plain file reads, or save-only instructions
        assert!(!prompt_explicitly_requests_file_delivery(
            "检查qq bot刚刚的情况，为什么有个文件没发到群聊里"
        ));
        assert!(!prompt_explicitly_requests_file_delivery(
            "检查我的vps运维情况，垃圾清理，网络、性能优化等"
        ));
        assert!(!prompt_explicitly_requests_file_delivery(
            "帮我看看 README.md 和 Cargo.toml 写了什么"
        ));
        assert!(!prompt_explicitly_requests_file_delivery(
            "写一个图表脚本并保存到 scratch/chart.png"
        ));
        assert!(!prompt_explicitly_requests_file_delivery(
            "[Attached Image File: /home/azureuser/scratch/m1-photo.jpg]\n解释一下这张图"
        ));
        assert!(!prompt_explicitly_requests_file_delivery(
            "Write a python script to send emails"
        ));
    }

    #[test]
    fn extract_explicit_send_files_strips_markers_outside_code_fences() {
        let input = "已完成整理。\n\n```markdown\n[[send_file:/tmp/example.md]]\n```\n\n[[send_file:/home/azureuser/scratch/han_dynasty_50_questions.md]]\n[[send_file:\"file:///home/azureuser/scratch/chart.png\"]]";
        let (cleaned, markers) = extract_explicit_send_files(input);
        assert_eq!(
            markers,
            vec![
                "/home/azureuser/scratch/han_dynasty_50_questions.md",
                "/home/azureuser/scratch/chart.png"
            ]
        );
        assert!(cleaned.contains("```markdown\n[[send_file:/tmp/example.md]]\n```"));
        assert!(!cleaned.contains("han_dynasty_50_questions.md]]"));
        assert!(!cleaned.contains("chart.png]]"));
    }

    #[tokio::test]
    async fn explicit_deliverables_suppress_unrequested_and_deliver_requested_md() {
        let base = fixture("agygram_explicit_deliverables");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        let hidden_gemini = workspace.join(".gemini");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(&hidden_gemini).unwrap();

        let md_file = scratch.join("han_dynasty_50_questions.md");
        let png_file = scratch.join("chart.png");
        let secret_file = hidden_gemini.join("oauth-token");
        let env_file = workspace.join(".env");
        let bin_file = workspace.join("agygram");
        let readme_file = workspace.join("README.md");
        std::fs::write(&md_file, b"# Han Dynasty\n50 questions").unwrap();
        std::fs::write(&png_file, b"PNG").unwrap();
        std::fs::write(&secret_file, b"secret").unwrap();
        std::fs::write(&env_file, b"KEY=1").unwrap();
        std::fs::write(&bin_file, b"ELF").unwrap();
        std::fs::write(&readme_file, b"# README").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];

        // 1. Unrequested turn mentioning chart.png and han_dynasty_50_questions.md -> sends NOTHING
        let reply_plain = format!(
            "图表已保存到 {}，文档在 {}，项目说明见 {}。",
            png_file.display(),
            md_file.display(),
            readme_file.display()
        );
        let (_, paths_unrequested) =
            collect_explicit_deliverables_async("帮我画个图表保存一下", &reply_plain, &roots).await;
        assert!(
            paths_unrequested.is_empty(),
            "unrequested turn must not auto-deliver artifacts"
        );

        // 2. Explicit user request WITHOUT [[send_file:]] marker -> delivers scratch .md & .png, NOT workspace README.md
        let (_, paths_prompt_requested) = collect_explicit_deliverables_async(
            "试试把上次的 han_dynasty_50_questions.md 发过来",
            &reply_plain,
            &roots,
        )
        .await;
        assert_eq!(
            paths_prompt_requested,
            vec![
                png_file.canonicalize().unwrap(),
                md_file.canonicalize().unwrap()
            ]
        );

        // 3. AI emits [[send_file:...]] markers -> delivers ONLY safe marked files, blocks hidden/.env/bin
        let marked_reply = format!(
            "已准备好文件。\n[[send_file:han_dynasty_50_questions.md]]\n[[send_file:{}]]\n[[send_file:{}]]\n[[send_file:{}]]",
            secret_file.display(),
            env_file.display(),
            bin_file.display()
        );
        let (cleaned, paths_marked) =
            collect_explicit_deliverables_async("把刚才那份题目发给我", &marked_reply, &roots)
                .await;
        assert_eq!(cleaned, "已准备好文件。");
        assert_eq!(paths_marked, vec![md_file.canonicalize().unwrap()]);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn active_brain_dir_allows_direct_media_only_and_blocks_internals() {
        let base = fixture("agygram_brain_media_whitelist");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        let gemini_root = workspace.join(".gemini").join("antigravity-cli");
        let brain_active = gemini_root.join("brain").join("conv-active-1");
        let brain_other = gemini_root.join("brain").join("conv-other-2");
        let sys_logs = brain_active.join(".system_generated").join("logs");

        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(&sys_logs).unwrap();
        std::fs::create_dir_all(&brain_other).unwrap();

        let generated_jpg = brain_active.join("gwen_chibi_1795176573774.jpg");
        let generated_mp4 = brain_active.join("demo_clip.mp4");
        let brain_json = brain_active.join("state.json");
        let brain_dot_img = brain_active.join(".hidden.jpg");
        let brain_secret_img = brain_active.join("credentials_dump.png");
        let nested_log_img = sys_logs.join("debug.jpg");
        let other_conv_img = brain_other.join("other.jpg");
        let oauth_token = gemini_root.join("antigravity-oauth-token");

        std::fs::write(&generated_jpg, b"\xff\xd8\xff").unwrap();
        std::fs::write(&generated_mp4, b"mp4").unwrap();
        std::fs::write(&brain_json, b"{}").unwrap();
        std::fs::write(&brain_dot_img, b"\xff\xd8\xff").unwrap();
        std::fs::write(&brain_secret_img, b"\x89PNG").unwrap();
        std::fs::write(&nested_log_img, b"\xff\xd8\xff").unwrap();
        std::fs::write(&other_conv_img, b"\xff\xd8\xff").unwrap();
        std::fs::write(&oauth_token, b"secret-token").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let reply = format!(
            "格温 Q 版来啦！\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]\n\
             [[send_file:{}]]",
            generated_jpg.display(),
            generated_mp4.display(),
            brain_json.display(),
            brain_dot_img.display(),
            brain_secret_img.display(),
            nested_log_img.display(),
            other_conv_img.display(),
            oauth_token.display(),
        );

        let (cleaned, delivered) = collect_explicit_deliverables_with_brain_async(
            "我要一个格温的",
            &reply,
            &roots,
            Some(&brain_active),
        )
        .await;
        assert_eq!(cleaned, "格温 Q 版来啦！");
        assert_eq!(
            delivered,
            vec![
                generated_jpg.canonicalize().unwrap(),
                generated_mp4.canonicalize().unwrap(),
            ]
        );

        // Without active_brain_dir, hidden .gemini paths are still strictly refused.
        let (_, delivered_without_brain) =
            collect_explicit_deliverables_with_brain_async("我要一个格温的", &reply, &roots, None)
                .await;
        assert!(delivered_without_brain.is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }
}
