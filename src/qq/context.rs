//! Group speaker attribution, persistent member nicknames, and bounded quote/reply context cache
//! for QQ Bot Open Platform v2 (`GROUP_AT_MESSAGE_CREATE`, `GROUP_MESSAGE_CREATE`, `C2C_MESSAGE_CREATE`).

use super::types::{MessageAttachment, MessageReference, MessageScene, MsgElement};
use crate::text::truncate_chars;
use rustc_hash::FxHashMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock as StdRwLock};
use tracing::warn;

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

const MAX_CACHED_SCOPES: usize = 64;
const MAX_MESSAGES_PER_SCOPE: usize = 200;
const MAX_CACHED_TEXT_CHARS: usize = 1200;
const MAX_NICKNAME_CHARS: usize = 24;
const MAX_STORED_NICKNAMES: usize = 2048;

/// Message type `103` in QQ Open Platform v2 denotes a Reply/Quoted composite message.
pub(crate) const QQ_QUOTE_MESSAGE_TYPE: u32 = 103;

type NicknameMap = FxHashMap<String, String>;

#[derive(Default)]
struct ScopeRingStore {
    scope_order: VecDeque<String>,
    scopes: FxHashMap<String, VecDeque<CachedQqMessage>>,
}

/// A recent inbound or outbound QQ message indexed by `msg_id` and `ref_idx` (`msg_idx` / `ext_info.ref_idx`)
/// so subsequent quote-replies can resolve both text and local/remote media attachments.
#[derive(Debug, Clone)]
pub(crate) struct CachedQqMessage {
    pub msg_id: Option<String>,
    pub ref_idx: Option<String>,
    pub sender_label: String,
    pub text: String,
    pub local_files: Vec<PathBuf>,
    pub remote_attachments: Vec<MessageAttachment>,
}

/// Extracted quote information merged from inline `msg_elements` and `QqContextStore`.
#[derive(Debug, Default, Clone)]
pub(crate) struct ExtractedQuote {
    pub sender_label: Option<String>,
    pub text: String,
    pub local_files: Vec<PathBuf>,
    pub remote_attachments: Vec<MessageAttachment>,
    pub cache_key: Option<String>,
    pub had_quote_indicator: bool,
}

static NICKNAMES_CACHE: OnceLock<StdRwLock<NicknameMap>> = OnceLock::new();
static MESSAGE_CONTEXT_CACHE: OnceLock<StdRwLock<ScopeRingStore>> = OnceLock::new();

#[inline]
fn fnv1a_64_ascii_upper(s: &str) -> u64 {
    let mut hash = FNV1A_OFFSET_BASIS;
    for b in s.trim().bytes() {
        hash ^= u64::from(b.to_ascii_uppercase());
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    hash
}

/// Computes a deterministic 4-hex-digit tag (e.g. `#A3F2`) from a QQ `member_openid` / `user_openid`
/// without exposing the raw OpenID.
#[must_use]
pub(crate) fn member_short_tag(openid: &str) -> String {
    let trimmed = openid.trim();
    if trimmed.is_empty() {
        return "#0000".to_string();
    }
    let h = fnv1a_64_ascii_upper(trimmed);
    let folded = ((h >> 48) ^ (h >> 32) ^ (h >> 16) ^ h) as u16;
    format!("#{:04X}", folded)
}

fn default_nicknames_path() -> PathBuf {
    let base = dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("agygram").join("qq_nicknames.json")
}

fn load_nicknames_from_disk(path: &Path) -> NicknameMap {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return FxHashMap::default();
    };
    serde_json::from_str::<FxHashMap<String, String>>(&raw).unwrap_or_default()
}

fn nicknames_lock() -> &'static StdRwLock<NicknameMap> {
    NICKNAMES_CACHE
        .get_or_init(|| StdRwLock::new(load_nicknames_from_disk(&default_nicknames_path())))
}

fn persist_nicknames_snapshot(snapshot: &NicknameMap, path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        warn!(dir = %parent.display(), error = %err, "Failed to create qq_nicknames directory");
        return;
    }
    let Ok(json) = serde_json::to_string_pretty(snapshot) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = write_private_file(&tmp, json.as_bytes()) {
        warn!(path = %tmp.display(), error = %err, "Failed to write qq_nicknames temp file");
        return;
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        warn!(path = %path.display(), error = %err, "Failed to atomically replace qq_nicknames.json");
        let _ = std::fs::remove_file(&tmp);
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.sync_all()
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

/// Validates and normalizes a user-chosen display nickname (`1..=24` chars, single-line, no brackets).
pub(crate) fn sanitize_nickname(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("称呼不能为空。");
    }
    let count = trimmed.chars().count();
    if count > MAX_NICKNAME_CHARS {
        return Err("称呼长度不能超过 24 个字符。");
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '[' | ']' | '<' | '>' | '`' | '\n' | '\r'))
    {
        return Err("称呼不能包含控制字符或括号/反引号符号。");
    }
    Ok(trimmed.to_string())
}

#[must_use]
pub(crate) fn get_member_nickname(openid: &str) -> Option<String> {
    let key = openid.trim().to_ascii_uppercase();
    if key.is_empty() {
        return None;
    }
    match nicknames_lock().read() {
        Ok(guard) => guard.get(&key).cloned(),
        Err(poisoned) => poisoned.into_inner().get(&key).cloned(),
    }
}

pub(crate) fn set_member_nickname(openid: &str, nickname: Option<String>) {
    set_member_nickname_with_path(openid, nickname, &default_nicknames_path());
}

pub(crate) fn set_member_nickname_with_path(
    openid: &str,
    nickname: Option<String>,
    persist_path: &Path,
) {
    let key = openid.trim().to_ascii_uppercase();
    if key.is_empty() {
        return;
    }
    let snapshot = {
        let mut guard = match nicknames_lock().write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        match nickname {
            Some(nick) => {
                if guard.len() >= MAX_STORED_NICKNAMES && !guard.contains_key(&key) {
                    guard.clear();
                }
                guard.insert(key, nick);
            }
            None => {
                guard.remove(&key);
            }
        }
        guard.clone()
    };
    persist_nicknames_snapshot(&snapshot, persist_path);
}

/// Formats a stable human-readable speaker label for a QQ group member, combining custom nickname
/// (when bound via `/nick`) and deterministic `#XXXX` short tag.
#[must_use]
pub(crate) fn format_group_speaker(member_openid: &str) -> String {
    let tag = member_short_tag(member_openid);
    match get_member_nickname(member_openid) {
        Some(nick) => format!("群成员「{nick}」({tag})"),
        None => format!("群成员 {tag}"),
    }
}

fn context_store_lock() -> &'static StdRwLock<ScopeRingStore> {
    MESSAGE_CONTEXT_CACHE.get_or_init(|| StdRwLock::new(ScopeRingStore::default()))
}

fn normalize_opt_id(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn ids_match(entry: &CachedQqMessage, needle: &str) -> bool {
    let needle = needle.trim();
    if needle.is_empty() {
        return false;
    }
    entry.ref_idx.as_deref() == Some(needle) || entry.msg_id.as_deref() == Some(needle)
}

/// Inserts or updates a recent inbound/outbound message in the bounded per-scope ring buffer.
pub(crate) fn record_message(
    scope: &str,
    msg_id: Option<&str>,
    ref_idx: Option<&str>,
    sender_label: impl Into<String>,
    text: &str,
    local_files: Vec<PathBuf>,
    remote_attachments: Vec<MessageAttachment>,
) {
    let scope_trimmed = scope.trim();
    if scope_trimmed.is_empty() {
        return;
    }
    let norm_id = normalize_opt_id(msg_id);
    let norm_ref = normalize_opt_id(ref_idx);
    if norm_id.is_none() && norm_ref.is_none() {
        return;
    }

    let clipped_text = truncate_chars(text.trim(), MAX_CACHED_TEXT_CHARS).into_owned();
    let sender = sender_label.into();

    let mut store = match context_store_lock().write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    if !store.scopes.contains_key(scope_trimmed) {
        while store.scope_order.len() >= MAX_CACHED_SCOPES {
            if let Some(evicted) = store.scope_order.pop_front() {
                store.scopes.remove(&evicted);
            }
        }
        store.scope_order.push_back(scope_trimmed.to_string());
        store
            .scopes
            .insert(scope_trimmed.to_string(), VecDeque::new());
    }

    let Some(ring) = store.scopes.get_mut(scope_trimmed) else {
        return;
    };

    // Merge in-place if passive GROUP_MESSAGE_CREATE or an earlier phase already recorded this ID/ref_idx.
    for existing in ring.iter_mut().rev() {
        let same_id = norm_id.is_some() && existing.msg_id == norm_id;
        let same_ref = norm_ref.is_some() && existing.ref_idx == norm_ref;
        if same_id || same_ref {
            if existing.msg_id.is_none() {
                existing.msg_id = norm_id;
            }
            if existing.ref_idx.is_none() {
                existing.ref_idx = norm_ref;
            }
            if !clipped_text.is_empty() {
                existing.text = clipped_text;
            }
            if !sender.is_empty() {
                existing.sender_label = sender;
            }
            for p in local_files {
                if !existing.local_files.contains(&p) {
                    existing.local_files.push(p);
                }
            }
            if existing.remote_attachments.is_empty() && !remote_attachments.is_empty() {
                existing.remote_attachments = remote_attachments;
            }
            return;
        }
    }

    while ring.len() >= MAX_MESSAGES_PER_SCOPE {
        ring.pop_front();
    }
    ring.push_back(CachedQqMessage {
        msg_id: norm_id,
        ref_idx: norm_ref,
        sender_label: sender,
        text: clipped_text,
        local_files,
        remote_attachments,
    });
}

/// Attaches freshly downloaded local file paths to an existing cached message so repeat quotes
/// reuse the local `scratch/` files without re-downloading.
pub(crate) fn attach_downloaded_files_to_cached(scope: &str, lookup_key: &str, paths: &[PathBuf]) {
    if paths.is_empty() || scope.trim().is_empty() || lookup_key.trim().is_empty() {
        return;
    }
    let mut store = match context_store_lock().write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let Some(ring) = store.scopes.get_mut(scope.trim()) else {
        return;
    };
    for entry in ring.iter_mut().rev() {
        if ids_match(entry, lookup_key) {
            for p in paths {
                if !entry.local_files.contains(p) {
                    entry.local_files.push(p.clone());
                }
            }
            break;
        }
    }
}

#[must_use]
pub(crate) fn lookup_cached_message(scope: &str, key: &str) -> Option<CachedQqMessage> {
    let key = key.trim();
    if scope.trim().is_empty() || key.is_empty() {
        return None;
    }
    let store = match context_store_lock().read() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let ring = store.scopes.get(scope.trim())?;
    ring.iter()
        .rev()
        .find(|entry| ids_match(entry, key))
        .cloned()
}

/// Extracts fallback user text when `event.content` is empty on composite (`message_type = 103`) events.
#[must_use]
pub(crate) fn extract_effective_content<'a>(
    top_content: &'a str,
    scene: Option<&MessageScene>,
    elements: &'a [MsgElement],
) -> &'a str {
    let trimmed = top_content.trim();
    if !trimmed.is_empty() || elements.is_empty() {
        return trimmed;
    }
    let cur_idx = scene.and_then(MessageScene::msg_idx);
    let ref_idx = scene.and_then(MessageScene::ref_msg_idx);

    for el in elements {
        let el_idx = el
            .msg_idx
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let is_current = match (cur_idx, ref_idx, el_idx) {
            (Some(cur), _, Some(eid)) => eid == cur,
            (None, Some(r), Some(eid)) => eid != r,
            _ => false,
        };
        if is_current
            && let Some(c) = el
                .content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            return c;
        }
    }
    trimmed
}

#[allow(clippy::too_many_arguments)]
fn collect_quoted_elements_recursive(
    elements: &[MsgElement],
    cur_idx: Option<&str>,
    ref_idx: Option<&str>,
    top_content: &str,
    is_group: bool,
    out_sender: &mut Option<String>,
    out_texts: &mut Vec<String>,
    out_atts: &mut Vec<MessageAttachment>,
) {
    for el in elements {
        let el_idx = el
            .msg_idx
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // Skip an element explicitly tagged as the current reply itself rather than the quoted target.
        if let (Some(cur), Some(eid)) = (cur_idx, el_idx)
            && cur == eid
            && ref_idx != Some(eid)
        {
            continue;
        }

        let matched_ref = match (ref_idx, el_idx) {
            (Some(r), Some(eid)) => r == eid,
            // When QQ omits per-element `msg_idx` inside a `103` reply container, any element whose
            // text differs from `top_content` or carries attachments belongs to the quoted message.
            (Some(_), None) | (None, _) => true,
        };

        if matched_ref {
            if out_sender.is_none()
                && let Some(ref auth) = el.author
                && let Some(oid) = auth.best_openid()
            {
                *out_sender = Some(if is_group {
                    format_group_speaker(oid)
                } else {
                    format!("用户 {}", member_short_tag(oid))
                });
            }

            if let Some(c) = el
                .content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                && (el_idx.is_some() || c != top_content.trim())
                && !out_texts.iter().any(|existing| existing == c)
            {
                out_texts.push(c.to_string());
            }

            for att in &el.attachments {
                if !att.url.trim().is_empty()
                    && !out_atts.iter().any(|existing| existing.url == att.url)
                {
                    out_atts.push(att.clone());
                }
            }
        }

        if !el.msg_elements.is_empty() {
            collect_quoted_elements_recursive(
                &el.msg_elements,
                cur_idx,
                ref_idx,
                top_content,
                is_group,
                out_sender,
                out_texts,
                out_atts,
            );
        }
    }
}

/// Resolves quote metadata from both inline `msg_elements` and the local `QqContextStore`.
#[must_use]
pub(crate) fn extract_quote_context(
    scope: &str,
    is_group: bool,
    top_content: &str,
    message_type: Option<u32>,
    scene: Option<&MessageScene>,
    reference: Option<&MessageReference>,
    elements: &[MsgElement],
) -> ExtractedQuote {
    let cur_idx = scene.and_then(MessageScene::msg_idx);
    let ref_idx = scene.and_then(MessageScene::ref_msg_idx);
    let ref_msg_id = reference
        .map(|r| r.message_id.trim())
        .filter(|s| !s.is_empty());

    let had_quote_indicator =
        ref_idx.is_some() || ref_msg_id.is_some() || message_type == Some(QQ_QUOTE_MESSAGE_TYPE);

    if !had_quote_indicator && elements.is_empty() {
        return ExtractedQuote::default();
    }

    let mut sender_label: Option<String> = None;
    let mut text_parts: Vec<String> = Vec::new();
    let mut local_files: Vec<PathBuf> = Vec::new();
    let mut remote_attachments: Vec<MessageAttachment> = Vec::new();
    let mut matched_cache_key: Option<String> = None;

    // 1. Check local ring-buffer cache first (covers Bot's own replies/images + prior @Bot messages + passive group msgs).
    let cached = ref_idx
        .and_then(|k| {
            let hit = lookup_cached_message(scope, k);
            if hit.is_some() {
                matched_cache_key = Some(k.to_string());
            }
            hit
        })
        .or_else(|| {
            ref_msg_id.and_then(|k| {
                let hit = lookup_cached_message(scope, k);
                if hit.is_some() {
                    matched_cache_key = Some(k.to_string());
                }
                hit
            })
        });

    if let Some(hit) = cached {
        if !hit.sender_label.is_empty() {
            sender_label = Some(hit.sender_label);
        }
        if !hit.text.is_empty() {
            text_parts.push(hit.text);
        }
        local_files = hit.local_files;
        remote_attachments = hit.remote_attachments;
    }

    // 2. Merge any inline `msg_elements` supplied directly by QQ Open Platform.
    if !elements.is_empty() {
        collect_quoted_elements_recursive(
            elements,
            cur_idx,
            ref_idx,
            top_content,
            is_group,
            &mut sender_label,
            &mut text_parts,
            &mut remote_attachments,
        );
    }

    let combined_text = truncate_chars(&text_parts.join("\n"), MAX_CACHED_TEXT_CHARS).into_owned();

    ExtractedQuote {
        sender_label,
        text: combined_text,
        local_files,
        remote_attachments,
        cache_key: matched_cache_key,
        had_quote_indicator,
    }
}

/// Formats the resolved quote block (or an explicit uncached-quote guard note) for prepending
/// to the `agy` prompt.
#[must_use]
pub(crate) fn format_quote_prompt_block(
    quote: &ExtractedQuote,
    existing_local_files: &[PathBuf],
    downloaded_quote_note: &str,
) -> String {
    let has_resolved_files = !existing_local_files.is_empty() || !downloaded_quote_note.is_empty();
    let has_resolved_text = !quote.text.trim().is_empty();

    if has_resolved_text || has_resolved_files {
        let mut out = String::with_capacity(256);
        let who = quote.sender_label.as_deref().unwrap_or("历史消息");
        let _ = writeln!(out, "[引用消息上下文 — 原发送者: {who}]");
        if has_resolved_text {
            let _ = writeln!(out, "被引用内容: {}", quote.text.trim());
        }
        for path in existing_local_files {
            let _ = writeln!(out, "[Quoted File: {}]", path.display());
        }
        if !downloaded_quote_note.is_empty() {
            out.push_str(downloaded_quote_note);
        }
        out.push('\n');
        return out;
    }

    if quote.had_quote_indicator {
        return String::from(
            "[系统引用提示: 当前发言人在客户端「引用/回复」了一条历史消息，但 QQ 官方接口未推送该被引用消息的原文或原图（可能是较早之前的消息，或群友之间未 @ 机器人的普通消息）。若当前提问依赖被引用内容或图片（如“图中是谁”“这个怎么这么眼熟”“这句话什么意思”），请直接礼貌说明因 QQ 官方机器人权限限制未能读取被引用的原消息/原图，请对方将原图或原文连同 @ 一起发出来，切勿盲目猜测或张冠李戴。]\n\n",
        );
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_short_tag_and_nickname_formatting() {
        let tag_a = member_short_tag("8A3F91C200112233");
        let tag_b = member_short_tag("9911AABBCCDDEEFF");
        assert!(tag_a.starts_with('#') && tag_a.len() == 5);
        assert_ne!(tag_a, tag_b);
        assert_eq!(tag_a, member_short_tag("  8a3f91c200112233 "));

        let tmp_dir =
            std::env::temp_dir().join(format!("agygram_qq_nick_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp_dir);
        let nick_file = tmp_dir.join("qq_nicknames.json");

        assert!(sanitize_nickname("").is_err());
        assert!(sanitize_nickname("bad[name]").is_err());
        let clean = sanitize_nickname("  阿杰  ").unwrap();
        assert_eq!(clean, "阿杰");

        set_member_nickname_with_path("TEST_MEMBER_OPENID_1", Some(clean), &nick_file);
        let label = format_group_speaker("TEST_MEMBER_OPENID_1");
        assert!(label.starts_with("群成员「阿杰」(#"));
        assert!(nick_file.exists());

        set_member_nickname_with_path("TEST_MEMBER_OPENID_1", None, &nick_file);
        assert_eq!(get_member_nickname("TEST_MEMBER_OPENID_1"), None);
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn quote_cache_resolves_bot_ref_idx_and_inline_elements() {
        let scope = "grp:TEST_GROUP_QUOTE_1";
        let img_path = PathBuf::from("/tmp/x_mikasa_3.jpg");

        // 1. Bot sends an image whose response returned `ext_info.ref_idx = "REFIDX_mikasa=="`
        record_message(
            scope,
            Some("ROBOT1.0_sent_img"),
            Some("REFIDX_mikasa=="),
            "AI 助手",
            "[AI 发送的 image: x_mikasa_3.jpg]",
            vec![img_path.clone()],
            Vec::new(),
        );

        // 2. Group member quotes that image (`ref_msg_idx=REFIDX_mikasa==`) and asks who is in it
        let scene = MessageScene {
            source: Some("default".into()),
            ext: vec![
                "msg_idx=REFIDX_user_turn==".into(),
                "ref_msg_idx=REFIDX_mikasa==".into(),
                "auth_token=secret_xyz".into(),
            ],
        };
        assert!(!format!("{scene:?}").contains("secret_xyz"));
        assert_eq!(scene.ref_msg_idx(), Some("REFIDX_mikasa=="));

        let extracted = extract_quote_context(
            scope,
            true,
            "图中人物的名字是什么",
            Some(0),
            Some(&scene),
            None,
            &[],
        );
        assert!(extracted.had_quote_indicator);
        assert_eq!(extracted.sender_label.as_deref(), Some("AI 助手"));
        assert_eq!(extracted.local_files, vec![img_path.clone()]);

        let block = format_quote_prompt_block(&extracted, &extracted.local_files, "");
        assert!(block.contains("原发送者: AI 助手"));
        assert!(block.contains("[Quoted File: /tmp/x_mikasa_3.jpg]"));

        // 3. Uncached quote emits explicit anti-hallucination guard note
        let unknown_scene = MessageScene {
            source: Some("default".into()),
            ext: vec!["ref_msg_idx=REFIDX_unseen_999==".into()],
        };
        let miss = extract_quote_context(
            scope,
            true,
            "这个怎么这么眼熟",
            Some(QQ_QUOTE_MESSAGE_TYPE),
            Some(&unknown_scene),
            None,
            &[],
        );
        assert!(miss.had_quote_indicator);
        assert!(miss.text.is_empty() && miss.local_files.is_empty());
        let miss_block = format_quote_prompt_block(&miss, &[], "");
        assert!(miss_block.contains("系统引用提示"));
        assert!(miss_block.contains("切勿盲目猜测"));
    }
}
