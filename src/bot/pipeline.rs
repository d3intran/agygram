use crate::artifacts;
use crate::bot::formatter::TelegramFormatter;
use crate::bot::media;
use crate::bot::widget;
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent, will_use_acp};
use crate::session::{ChatSession, SessionManager, TurnGuard, TurnOutcome, hash_system_prompt};
use crate::text;
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardMarkup, MessageId, ParseMode};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const TELEGRAM_TEXT_LIMIT: usize = 4000;
const INTERRUPT_GRACE_SECS: u64 = 6;
const SHUTDOWN_WAIT_SECS: u64 = 3;
const FIRST_TEXT_DEBOUNCE_MS: u64 = 250;
const MAX_ADAPTIVE_INTERVAL_SECS: u64 = 5;
const CLEAN_EDITS_TO_DECAY_BACKOFF: u32 = 4;
const SLOT_WAIT_NOTICE_DELAY_MS: u64 = 300;
const MAX_REPLY_CONTEXT_CHARS: usize = 1200;
const MAX_BOT_REPLY_CONTEXT_CHARS: usize = 200;
const MAX_PRESERVED_REPLY_PATHS: usize = 4;
const MAX_STREAM_THINKING_CHARS: usize = 1500;
const MAX_STREAM_THINKING_BYTES: usize = MAX_STREAM_THINKING_CHARS * 4 + 256;

const TG_PLAIN_LINK_HINT: &str =
    "聊天中提及本地文件用文件名或反引号即可，勿输出不可点击的 `file://` 链接。";

/// Default sensitive paths denied via `agy --settings permissions.deny` so compromised prompts or
/// tool runs cannot read production bot credentials or SSH keys.
fn default_sensitive_deny_paths() -> Vec<String> {
    let mut paths = vec![
        "~/.config/agygram/*".to_string(),
        "~/.config/userbot/*".to_string(),
        "~/.ssh/*".to_string(),
    ];
    if let Some(home) = dirs::home_dir() {
        for rel in [".config/agygram/*", ".config/userbot/*", ".ssh/*"] {
            let abs = home.join(rel).to_string_lossy().into_owned();
            if !paths.contains(&abs) {
                paths.push(abs);
            }
        }
    }
    paths
}

/// Builds the Telegram turn system prompt, injecting the ~1.4 KB `WIDGET_PROMPT_GUIDE` ONLY when
/// the turn actually asks for an interactive card/dashboard/quiz/mini-game.
#[must_use]
pub(crate) fn build_telegram_system_prompt(
    user_system_prompt: &str,
    include_widget_guide: bool,
) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(4);
    let trimmed = user_system_prompt.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    if include_widget_guide {
        parts.push(widget::WIDGET_PROMPT_GUIDE);
    }
    parts.push(artifacts::FILE_DELIVERY_PROMPT_GUIDE);
    parts.push(TG_PLAIN_LINK_HINT);
    parts.join("\n\n")
}

/// Resolves the Telegram system prompt for this turn, deduplicating static base instructions
/// and the on-demand `WIDGET_PROMPT_GUIDE` per compaction epoch on `Persistent` / `Print`
/// backends. Returns `(system_prompt, injected_base_hash, injected_widget_guide)`.
fn resolve_telegram_turn_system_prompt(
    session: &ChatSession,
    wants_widget_guide: bool,
    use_acp: bool,
) -> (String, Option<u64>, bool) {
    let base = build_telegram_system_prompt(&session.system_prompt, false);
    let base_hash = hash_system_prompt(&base);

    if use_acp {
        let full = build_telegram_system_prompt(&session.system_prompt, wants_widget_guide);
        return (full, Some(base_hash), wants_widget_guide);
    }

    let needs_base = session.is_first_turn || session.last_system_prompt_hash != Some(base_hash);
    let needs_widget =
        wants_widget_guide && (session.is_first_turn || !session.last_widget_guide_injected);

    if needs_base {
        let full = build_telegram_system_prompt(&session.system_prompt, needs_widget);
        (full, Some(base_hash), needs_widget)
    } else if needs_widget {
        (widget::WIDGET_PROMPT_GUIDE.to_string(), None, true)
    } else {
        (String::new(), None, false)
    }
}

fn looks_like_local_path_token(raw: &str) -> Option<&str> {
    let trimmed = raw.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '\''
                | '"'
                | '('
                | ')'
                | '['
                | ']'
                | '<'
                | '>'
                | '{'
                | '}'
                | ','
                | ';'
                | ':'
                | '。'
                | '，'
                | '、'
                | '）'
                | '（'
        )
    });
    let cleaned = trimmed.trim_end_matches('.');
    if cleaned.len() < 3 || cleaned.contains("://") {
        return None;
    }
    let starts_like_path =
        cleaned.starts_with('/') || cleaned.starts_with("~/") || cleaned.starts_with("./");
    if !starts_like_path {
        return None;
    }
    let rest = cleaned
        .trim_start_matches('~')
        .trim_start_matches('.')
        .trim_start_matches('/');
    if rest.is_empty() || (!rest.contains('/') && !rest.contains('.')) {
        return None;
    }
    // Avoid mistaking Telegram slash commands like `/status` for filesystem paths.
    if cleaned.starts_with('/') && !cleaned[1..].contains('/') {
        return None;
    }
    Some(cleaned)
}

/// Caps quoted bot-message text to `max_chars` (since bot replies already live in session
/// history) while preserving any local file paths mentioned beyond the truncation boundary.
fn truncate_bot_reply_preserving_paths(body: &str, max_chars: usize) -> Cow<'_, str> {
    let clipped = text::truncate_chars(body, max_chars);
    let Cow::Owned(mut out) = clipped else {
        return Cow::Borrowed(body);
    };

    let mut preserved: Vec<&str> = Vec::new();
    for tok in body.split_whitespace() {
        if let Some(path) = looks_like_local_path_token(tok)
            && !out.contains(path)
            && !preserved.contains(&path)
        {
            preserved.push(path);
            if preserved.len() >= MAX_PRESERVED_REPLY_PATHS {
                break;
            }
        }
    }
    if !preserved.is_empty() {
        let _ = write!(out, "\n[提及路径: {}]", preserved.join(", "));
    }
    Cow::Owned(out)
}

/// Extracts quoted text/caption, sender display name, and resolved or fallback media hint from
/// `msg.reply_to_message()`. Self-replies to bot messages cap prose to 200 chars while
/// preserving attached and mentioned local file paths.
#[must_use]
pub(crate) fn extract_reply_context_with_media(
    msg: &Message,
    resolved_media: Option<(&str, &Path)>,
) -> Option<String> {
    let replied = msg.reply_to_message()?;
    let is_bot_reply = replied.from.as_ref().is_some_and(|u| u.is_bot);
    let sender = replied.from.as_ref().map(|u| {
        if u.is_bot {
            "Bot".to_string()
        } else {
            let full = u.full_name();
            if !full.trim().is_empty() {
                full
            } else if let Some(ref un) = u.username {
                format!("@{un}")
            } else {
                format!("User {}", u.id.0)
            }
        }
    });

    let attachment_hint = if let Some((label, path)) = resolved_media {
        Some(format!("[附件: {label}: {}]", path.display()))
    } else if let Some(doc) = replied.document() {
        Some(format!(
            "[附件: Document {}]",
            doc.file_name.as_deref().unwrap_or("file")
        ))
    } else if replied.photo().is_some() {
        Some("[附件: Image]".to_string())
    } else if replied.voice().is_some() {
        Some("[附件: Voice]".to_string())
    } else if let Some(video) = replied.video() {
        Some(format!(
            "[附件: Video {}]",
            video.file_name.as_deref().unwrap_or("video")
        ))
    } else if let Some(audio) = replied.audio() {
        Some(format!(
            "[附件: Audio {}]",
            audio.file_name.as_deref().unwrap_or("audio")
        ))
    } else if replied.animation().is_some() {
        Some("[附件: Animation]".to_string())
    } else {
        None
    };

    let body = replied
        .text()
        .or_else(|| replied.caption())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if is_bot_reply {
                truncate_bot_reply_preserving_paths(s, MAX_BOT_REPLY_CONTEXT_CHARS)
            } else {
                text::truncate_chars(s, MAX_REPLY_CONTEXT_CHARS)
            }
        });

    if attachment_hint.is_none() && body.is_none() {
        return None;
    }

    let mut out = String::with_capacity(256);
    match sender {
        Some(name) => {
            let _ = writeln!(out, "[引用消息上下文 (发送者: {name})]");
        }
        None => {
            out.push_str("[引用消息上下文]\n");
        }
    }
    if let Some(hint) = attachment_hint {
        let _ = writeln!(out, "{hint}");
    }
    if let Some(text_body) = body {
        let _ = writeln!(out, "{text_body}");
    }
    out.push_str("[/引用消息上下文]");
    Some(out)
}

/// Extracts quoted text/caption, sender display name, and media hint from `msg.reply_to_message()`.
#[cfg(test)]
#[must_use]
pub(crate) fn extract_reply_context(msg: &Message) -> Option<String> {
    extract_reply_context_with_media(msg, None)
}

#[inline]
pub(crate) fn is_message_not_modified_err(err: &teloxide::RequestError) -> bool {
    matches!(
        err,
        teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
    )
}

#[inline]
pub(crate) fn is_html_entity_api_error(err: &teloxide::RequestError) -> bool {
    let teloxide::RequestError::Api(api_err) = err else {
        return false;
    };
    let msg = api_err.to_string();
    msg.contains("parse entities")
        || msg.contains("Unsupported start tag")
        || msg.contains("Can't find end tag")
        || msg.contains("Wrong HTTP URL")
}

#[inline]
#[must_use]
fn compute_next_edit_interval(
    current_interval: Duration,
    visible_utf16_len: usize,
    chunk_count: usize,
) -> Duration {
    if chunk_count > 1 || visible_utf16_len > 2400 {
        current_interval.max(Duration::from_millis(1400))
    } else {
        current_interval
    }
}

#[inline]
pub(crate) fn log_telegram_err(chat_id: i64, action: &'static str, err: teloxide::RequestError) {
    if !is_message_not_modified_err(&err) {
        warn!(chat_id, action, error = ?err, "telegram api call failed");
    }
}

struct ActiveTurnEntry {
    generation: u64,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

/// Shared coordinator runtime managing graceful daemon cancellation and per-chat active turn tasks.
#[derive(Clone)]
pub struct BotRuntime {
    pub cancel: CancellationToken,
    turns: Arc<Mutex<FxHashMap<i64, ActiveTurnEntry>>>,
}

impl BotRuntime {
    #[must_use]
    pub fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            turns: Arc::new(Mutex::new(FxHashMap::default())),
        }
    }

    pub async fn cancel_chat_turn(&self, chat_id: i64) {
        let map = self.turns.lock().await;
        if let Some(entry) = map.get(&chat_id) {
            entry.cancel.cancel();
        }
    }

    async fn clear_turn(&self, chat_id: i64, generation: u64) {
        let mut map = self.turns.lock().await;
        if map
            .get(&chat_id)
            .is_some_and(|e| e.generation == generation)
        {
            map.remove(&chat_id);
        }
    }

    /// Signals cancellation to all active turns and waits briefly for them to tear down cleanly.
    pub async fn shutdown_all(&self) {
        self.cancel.cancel();
        let handles: Vec<tokio::task::JoinHandle<()>> = {
            let mut map = self.turns.lock().await;
            map.drain()
                .map(|(_, entry)| {
                    entry.cancel.cancel();
                    entry.handle
                })
                .collect()
        };
        if handles.is_empty() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(SHUTDOWN_WAIT_SECS), async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
    }
}

/// Parses an optional leading `/voice` (or `/voice@botname`) command prefix, returning
/// `(is_voice_cmd, remaining_text)`. Never triggers on `/voice` appearing mid-sentence.
#[must_use]
pub(crate) fn parse_voice_prefix(input: &str) -> (bool, &str) {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix("/voice") else {
        return (false, input);
    };
    if rest.is_empty() {
        return (true, "");
    }
    if rest.starts_with(char::is_whitespace) {
        return (true, rest.trim());
    }
    if let Some(after_at) = rest.strip_prefix('@') {
        if let Some(ws_idx) = after_at.find(char::is_whitespace) {
            return (true, after_at[ws_idx..].trim());
        }
        return (true, "");
    }
    (false, input)
}

/// Entry point registered on the Telegram dispatcher for non-command / media messages.
pub async fn message_handler(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
) -> ResponseResult<()> {
    Box::pin(dispatch_message_with_session(
        bot,
        msg,
        config,
        session_mgr,
        runtime,
        None,
    ))
    .await
}

/// Options controlling prompt dispatch for normal messages, `/card`, `/voice`, and `/compact`.
#[derive(Default)]
pub(crate) struct DispatchOptions {
    pub prompt_override: Option<String>,
    pub preloaded_session: Option<ChatSession>,
    pub force_card_schema: bool,
    pub raw_slash_command: bool,
}

/// Dispatches an incoming message (optionally reusing an already loaded `ChatSession` from
/// `/voice <prompt>`), returning promptly after spawning the per-chat execution task so
/// subsequent messages can interrupt long-running turns immediately.
#[tracing::instrument(skip_all, fields(chat_id = msg.chat.id.0, user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0)))]
pub(crate) async fn dispatch_message_with_session(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
    preloaded_session: Option<ChatSession>,
) -> ResponseResult<()> {
    Box::pin(dispatch_prompt_with_options(
        bot,
        msg,
        config,
        session_mgr,
        runtime,
        DispatchOptions {
            preloaded_session,
            ..Default::default()
        },
    ))
    .await
}

/// Dispatches an incoming turn with an optional rewritten prompt override.
#[tracing::instrument(skip_all, fields(chat_id = msg.chat.id.0, user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0)))]
#[allow(dead_code)]
pub(crate) async fn dispatch_prompt_with_session(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
    prompt_override: Option<String>,
    preloaded_session: Option<ChatSession>,
) -> ResponseResult<()> {
    Box::pin(dispatch_prompt_with_options(
        bot,
        msg,
        config,
        session_mgr,
        runtime,
        DispatchOptions {
            prompt_override,
            preloaded_session,
            ..Default::default()
        },
    ))
    .await
}

/// Dispatches an incoming turn with full control over prompt override, `--json-schema` card mode,
/// and raw native slash-command passthrough (e.g. `/compact`).
#[tracing::instrument(skip_all, fields(chat_id = msg.chat.id.0, user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0)))]
pub(crate) async fn dispatch_prompt_with_options(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
    opts: DispatchOptions,
) -> ResponseResult<()> {
    let DispatchOptions {
        prompt_override,
        preloaded_session,
        force_card_schema,
        raw_slash_command,
    } = opts;
    let user_id = msg.from.as_ref().map_or(0, |u| u.id.0);
    if !config.is_user_allowed(user_id) {
        warn!(
            user_id,
            chat_id = msg.chat.id.0,
            "ignoring unauthorized message"
        );
        return Ok(());
    }

    let chat_id = msg.chat.id.0;
    let has_media = !raw_slash_command && media::has_supported_media(&msg);
    let (voice_in_text, text_body) = msg.text().map(parse_voice_prefix).unwrap_or((false, ""));
    let (voice_in_caption, caption_body) =
        msg.caption().map(parse_voice_prefix).unwrap_or((false, ""));

    if prompt_override
        .as_deref()
        .is_none_or(|p| p.trim().is_empty())
        && !has_media
        && text_body.trim().is_empty()
        && caption_body.trim().is_empty()
    {
        return Ok(());
    }

    let mut was_interrupted = false;
    let generation = match session_mgr.begin_turn(chat_id).await {
        Some(turn_gen) => turn_gen,
        None => {
            if session_mgr.is_bg_compacting(chat_id).await {
                // Silent background auto-compaction is finishing; wait transparently rather than
                // interrupting it or emitting a busy/interrupted banner.
                match session_mgr
                    .begin_turn_with_timeout(chat_id, Duration::from_secs(90))
                    .await
                {
                    Some(turn_gen) => turn_gen,
                    None => {
                        if let Err(err) = bot
                            .send_message(
                                msg.chat.id,
                                "⏳ <b>上一条任务正在运行中...</b> 发送 /cancel 强行中断任务。",
                            )
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            log_telegram_err(chat_id, "send_busy_notice_bg_compact", err);
                        }
                        return Ok(());
                    }
                }
            } else if !config.interrupt_on_new_message {
                if let Err(err) = bot
                    .send_message(
                        msg.chat.id,
                        "⏳ <b>上一条任务正在运行中...</b> 发送 /cancel 强行中断任务。",
                    )
                    .parse_mode(ParseMode::Html)
                    .await
                {
                    log_telegram_err(chat_id, "send_busy_notice", err);
                }
                return Ok(());
            } else {
                info!(chat_id, "interrupting running turn due to new message");
                runtime.cancel_chat_turn(chat_id).await;
                session_mgr.cancel_task(chat_id).await;
                was_interrupted = true;

                match session_mgr.begin_turn(chat_id).await {
                    Some(turn_gen) => turn_gen,
                    None => {
                        if let Err(err) = bot
                            .send_message(
                                msg.chat.id,
                                "⏳ <b>上一条任务正在运行中...</b> 发送 /cancel 强行中断任务。",
                            )
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            log_telegram_err(chat_id, "send_busy_notice_after_race", err);
                        }
                        return Ok(());
                    }
                }
            }
        }
    };

    let session = match preloaded_session {
        Some(s) => s,
        None => session_mgr.get_or_create(chat_id).await,
    };
    let should_send_voice =
        !raw_slash_command && (session.voice_mode || voice_in_text || voice_in_caption);
    let text_part = if let Some(overridden) = prompt_override
        && !overridden.trim().is_empty()
    {
        overridden.trim().to_string()
    } else if !text_body.trim().is_empty() {
        text_body.trim().to_string()
    } else {
        caption_body.trim().to_string()
    };

    let turn_guard = TurnGuard::new(session_mgr.clone(), chat_id, generation);
    let turn_cancel = runtime.cancel.child_token();

    // Hold `turns` mutex synchronously across spawn + insert so `clear_turn` inside the task can
    // never overtake `insert`.
    let mut turns_map = runtime.turns.lock().await;
    let task_runtime = runtime.clone();
    let task_cancel = turn_cancel.clone();
    let handle = tokio::spawn(async move {
        let ctx = TurnTaskContext {
            bot,
            msg,
            config,
            session_mgr,
            session,
            generation,
            should_send_voice,
            text_part,
            force_card_schema,
            raw_slash_command,
            was_interrupted,
            turn_cancel: task_cancel,
            daemon_cancel: task_runtime.cancel.clone(),
            turn_guard,
        };
        if let Err(err) = Box::pin(run_chat_turn(ctx)).await {
            log_telegram_err(chat_id, "run_chat_turn", err);
        }
        task_runtime.clear_turn(chat_id, generation).await;
    });

    if let Some(prev) = turns_map.insert(
        chat_id,
        ActiveTurnEntry {
            generation,
            cancel: turn_cancel,
            handle,
        },
    ) && prev.generation != generation
    {
        prev.cancel.cancel();
    }

    Ok(())
}

struct TurnTaskContext {
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    session: ChatSession,
    generation: u64,
    should_send_voice: bool,
    text_part: String,
    force_card_schema: bool,
    raw_slash_command: bool,
    was_interrupted: bool,
    turn_cancel: CancellationToken,
    daemon_cancel: CancellationToken,
    turn_guard: TurnGuard,
}

enum PlaceholderState {
    Pending(tokio::task::JoinHandle<Option<MessageId>>),
    Ready(MessageId),
    Failed,
}

impl PlaceholderState {
    async fn resolve(&mut self, cancel: &CancellationToken) -> Option<MessageId> {
        match self {
            Self::Ready(id) => Some(*id),
            Self::Failed => None,
            Self::Pending(_) => {
                let mut prev = std::mem::replace(self, Self::Failed);
                let Self::Pending(handle) = &mut prev else {
                    return None;
                };
                let res = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return None,
                    res = handle => res,
                };
                match res {
                    Ok(Some(id)) => {
                        *self = Self::Ready(id);
                        Some(id)
                    }
                    Ok(None) | Err(_) => {
                        *self = Self::Failed;
                        None
                    }
                }
            }
        }
    }
}

impl Drop for PlaceholderState {
    fn drop(&mut self) {
        if let Self::Pending(handle) = self {
            handle.abort();
        }
    }
}

async fn send_initial_placeholder(
    bot: &Bot,
    target_chat: ChatId,
    turn_cancel: &CancellationToken,
) -> Option<MessageId> {
    let chat_id = target_chat.0;
    let mut attempt = 1u8;
    loop {
        if turn_cancel.is_cancelled() {
            return None;
        }
        let send_fut = async {
            bot.send_message(target_chat, "<i>思考中...</i>")
                .parse_mode(ParseMode::Html)
                .await
        };
        let send_res = tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => return None,
            res = send_fut => res,
        };
        match send_res {
            Ok(m) => return Some(m.id),
            Err(teloxide::RequestError::RetryAfter(secs)) if attempt < 2 => {
                attempt += 1;
                let wait = Duration::from_secs(u64::from(secs.seconds()) + 1);
                tokio::select! {
                    biased;
                    _ = turn_cancel.cancelled() => return None,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            Err(err) => {
                warn!(chat_id, attempt, error = ?err, "initial status message send failed");
                if attempt >= 2 {
                    return None;
                }
                attempt += 1;
                tokio::select! {
                    biased;
                    _ = turn_cancel.cancelled() => return None,
                    _ = tokio::time::sleep(Duration::from_millis(600)) => {}
                }
            }
        }
    }
}

#[tracing::instrument(skip_all, fields(chat_id = ctx.msg.chat.id.0, generation = ctx.generation))]
async fn run_chat_turn(mut ctx: TurnTaskContext) -> ResponseResult<()> {
    let target_chat = ctx.msg.chat.id;
    let chat_id = target_chat.0;

    if ctx.was_interrupted
        && let Err(err) = ctx
            .bot
            .send_message(
                target_chat,
                "<b>收到新消息，已打断上一次任务并切换为最新推导...</b>",
            )
            .parse_mode(ParseMode::Html)
            .await
    {
        log_telegram_err(chat_id, "send_interrupt_notice", err);
    }

    if ctx.turn_cancel.is_cancelled() {
        return Ok(());
    }

    let valid_workspace = ctx.session.get_valid_workspace();
    let scratch_dir = PathBuf::from(&valid_workspace).join("scratch");
    if let Err(err) = tokio::fs::create_dir_all(&scratch_dir).await {
        warn!(chat_id, dir = %scratch_dir.display(), error = %err, "failed to create scratch directory");
    }

    let mut prompt_content = String::new();
    let (quoted_media, attachment_opt) = if ctx.raw_slash_command {
        (None, None)
    } else {
        let quoted_fut = async {
            if let Some(replied) = ctx.msg.reply_to_message()
                && media::has_supported_media(replied)
            {
                media::resolve_quoted_media_attachment(
                    &ctx.bot,
                    replied,
                    &scratch_dir,
                    ctx.config.image_max_edge,
                )
                .await
            } else {
                None
            }
        };
        let attachment_fut = media::download_media_attachment(
            &ctx.bot,
            &ctx.msg,
            &scratch_dir,
            ctx.config.image_max_edge,
        );
        tokio::select! {
            biased;
            _ = ctx.turn_cancel.cancelled() => return Ok(()),
            pair = async { tokio::join!(quoted_fut, attachment_fut) } => pair,
        }
    };
    if quoted_media.is_some() || attachment_opt.is_some() {
        crate::qq::media::maybe_spawn_scratch_cleanup(scratch_dir.clone());
    }
    let reply_context = if ctx.raw_slash_command {
        None
    } else {
        extract_reply_context_with_media(
            &ctx.msg,
            quoted_media
                .as_ref()
                .map(|(label, path)| (*label, path.as_path())),
        )
    };

    // Preserve a leading `/` at position 0 so native `agy` commands/skills are not blocked.
    let starts_with_slash = ctx.text_part.trim_start().starts_with('/');
    if starts_with_slash {
        prompt_content.push_str(&ctx.text_part);
        if let Some(ref reply_ctx) = reply_context {
            prompt_content.push_str("\n\n");
            prompt_content.push_str(reply_ctx);
        }
        if let Some(ref attachment_str) = attachment_opt {
            prompt_content.push('\n');
            prompt_content.push_str(attachment_str);
        }
    } else {
        if let Some(ref reply_ctx) = reply_context {
            prompt_content.push_str(reply_ctx);
            prompt_content.push_str("\n\n");
        }
        if let Some(ref attachment_str) = attachment_opt {
            prompt_content.push_str(attachment_str);
        }
        if !ctx.text_part.is_empty() {
            prompt_content.push_str(&ctx.text_part);
        }
    }

    let input_prompt = prompt_content.trim().to_string();
    if input_prompt.is_empty() || ctx.turn_cancel.is_cancelled() {
        return Ok(());
    }

    // Send Typing chat action and initial placeholder concurrently so Telegram HTTPS RTTs never
    // block `acquire_exec_slot` or `agy` spawn (~200-500ms TTFT win).
    {
        let typing_bot = ctx.bot.clone();
        tokio::spawn(async move {
            if let Err(err) = typing_bot
                .send_chat_action(target_chat, teloxide::types::ChatAction::Typing)
                .await
            {
                log_telegram_err(chat_id, "send_initial_typing", err);
            }
        });
    }

    let mut initial_placeholder = {
        let ph_bot = ctx.bot.clone();
        let ph_cancel = ctx.turn_cancel.clone();
        PlaceholderState::Pending(tokio::spawn(async move {
            send_initial_placeholder(&ph_bot, target_chat, &ph_cancel).await
        }))
    };

    // Acquire a global `agy` concurrency slot before spawning to prevent OOM/swap thrashing on small VPS.
    let slot_wait_start = std::time::Instant::now();
    let exec_slot = {
        let slot_fut = ctx.session_mgr.acquire_exec_slot(Some(&ctx.turn_cancel));
        tokio::pin!(slot_fut);
        tokio::select! {
            biased;
            slot = &mut slot_fut => match slot {
                Some(guard) => guard,
                None => return Ok(()),
            },
            _ = tokio::time::sleep(Duration::from_millis(SLOT_WAIT_NOTICE_DELAY_MS)) => {
                if ctx.turn_cancel.is_cancelled() {
                    return Ok(());
                }
                if let Some(ph_id) = initial_placeholder.resolve(&ctx.turn_cancel).await
                    && let Err(err) = ctx
                        .bot
                        .edit_message_text(
                            target_chat,
                            ph_id,
                            "⏳ <i>排队等待算力槽位...</i>",
                        )
                        .parse_mode(ParseMode::Html)
                        .await
                {
                    log_telegram_err(chat_id, "edit_slot_waiting_notice", err);
                }
                match slot_fut.await {
                    Some(guard) => guard,
                    None => return Ok(()),
                }
            }
        }
    };
    let queue_wait_ms = u64::try_from(slot_wait_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    if ctx.turn_cancel.is_cancelled() {
        return Ok(());
    }

    let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>(1000);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    let is_compact_turn = input_prompt
        .split_whitespace()
        .next()
        .is_some_and(|tok| tok.eq_ignore_ascii_case("/compact"));
    let backend = ctx.config.executor_backend_enum();
    let effective_use_acp = will_use_acp(backend, ctx.force_card_schema, ctx.session.remote_agent);
    // Match widget intent ONLY on user text (never `reply_context`), and skip `WIDGET_PROMPT_GUIDE`
    // on `/card` (`force_card_schema == true`) so it never contradicts `--json-schema`.
    let wants_widget_guide = !ctx.raw_slash_command
        && !ctx.force_card_schema
        && widget::prompt_requests_widget_or_card(&ctx.text_part);
    let allow_prose_widget_files = ctx.force_card_schema || wants_widget_guide;
    let (effective_system_prompt, injected_system_prompt_hash, injected_widget_guide) =
        if ctx.raw_slash_command || is_compact_turn {
            (String::new(), None, false)
        } else {
            resolve_telegram_turn_system_prompt(&ctx.session, wants_widget_guide, effective_use_acp)
        };
    let turn_start = std::time::Instant::now();
    let deny_paths = default_sensitive_deny_paths();
    let settings_json = crate::executor::build_session_settings_json(
        ctx.session.show_thinking,
        ctx.session.orchestrator_planner.as_deref(),
        ctx.session.orchestrator_executor.as_deref(),
        &deny_paths,
    );
    let params = ExecutionParams {
        chat_id,
        prompt: &input_prompt,
        conversation_id: ctx.session.conversation_id.as_deref(),
        is_first_turn: ctx.session.is_first_turn,
        workspace: &valid_workspace,
        extra_dirs: &ctx.session.extra_dirs,
        model: &ctx.session.model,
        effort: &ctx.session.effort,
        mode: &ctx.session.mode,
        agent: &ctx.session.agent,
        system_prompt: &effective_system_prompt,
        sandbox: ctx.session.sandbox,
        project: &ctx.session.project,
        settings_json,
        json_schema: ctx.force_card_schema.then_some(widget::TG_CARD_JSON_SCHEMA),
        remote_agent: ctx.session.remote_agent,
        backend,
    };

    let session_mgr_pid = ctx.session_mgr.clone();
    let pid_generation = ctx.generation;
    let pid_task = tokio::spawn(async move {
        if let Ok(pid) = pid_rx.await {
            session_mgr_pid.set_pid(chat_id, pid, pid_generation).await;
        }
    });

    let exec_future = AgyExecutor::run_prompt_streaming(
        params,
        stream_tx,
        Some(pid_tx),
        ctx.config.idle_timeout_secs,
        ctx.config.max_execution_secs,
        Some(ctx.turn_cancel.clone()),
    );
    tokio::pin!(exec_future);

    let turn_cancel_exec = ctx.turn_cancel.clone();
    let daemon_cancel_exec = ctx.daemon_cancel.clone();
    let exec_with_cancel = async {
        tokio::select! {
            biased;
            res = &mut exec_future => Some(res),
            _ = turn_cancel_exec.cancelled() => {
                if daemon_cancel_exec.is_cancelled() {
                    None
                } else {
                    // Allow agy up to 6s after SIGINT from `cancel_task` to flush transcript.jsonl.
                    tokio::select! {
                        biased;
                        res = &mut exec_future => Some(res),
                        _ = daemon_cancel_exec.cancelled() => None,
                        _ = tokio::time::sleep(Duration::from_secs(INTERRUPT_GRACE_SECS)) => None,
                    }
                }
            }
        }
    };

    let render_fut = Box::pin(stream_and_render_turn(
        &ctx.bot,
        target_chat,
        stream_rx,
        StreamRenderOptions {
            initial_placeholder,
            placeholder_html: "<i>思考中...</i>",
            draft_interval_ms: ctx.config.draft_update_interval_ms,
            show_typing: true,
            overwrite_on_result: true,
            break_on_turn_complete: false,
            session_guard: None,
            turn_cancel: &ctx.turn_cancel,
        },
        InitialStreamSeed::default(),
    ));

    let (exec_opt, render_outcome) = tokio::join!(exec_with_cancel, render_fut);
    pid_task.abort();
    // Release the foreground concurrency slot immediately before post-processing / detached waits.
    drop(exec_slot);

    let Some(exec_result) = exec_opt else {
        info!(
            chat_id,
            generation = ctx.generation,
            "turn cancelled before completion"
        );
        return Ok(());
    };

    match exec_result {
        Ok(res) => {
            let is_detached = res.detached;
            let saw_checkpoint = res.saw_checkpoint;
            let output_text = res.output.trim_end().to_string();
            let structured_output = res.structured_output;
            let usage_ref = res.usage.as_ref();
            let turn_duration = turn_start.elapsed().as_secs_f64();
            let resolved_conv_id = res
                .conversation_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| ctx.session.conversation_id.clone());

            if !res.status.eq_ignore_ascii_case("SUCCESS") {
                warn!(
                    chat_id,
                    generation = ctx.generation,
                    status = %res.status,
                    finished_reason = ?res.finished_reason,
                    duration_secs = format_args!("{turn_duration:.2}"),
                    backend = ?backend,
                    "telegram turn finished with non-success status"
                );
            }

            let post_session_opt = ctx
                .session_mgr
                .finish_turn(
                    chat_id,
                    ctx.generation,
                    TurnOutcome {
                        conversation_id: resolved_conv_id.clone(),
                        status: &res.status,
                        input_chars: input_prompt.len(),
                        output_chars: output_text.len(),
                        usage: usage_ref,
                        usage_is_cumulative: res.usage_is_cumulative,
                        context_tokens: res.context_tokens,
                        num_turns: res.num_turns,
                        saw_checkpoint,
                        system_prompt_sent: res.system_prompt_sent,
                        injected_system_prompt_hash,
                        injected_voice_guide: false,
                        injected_widget_guide,
                        duration_seconds: Some(turn_duration),
                        detached: is_detached,
                    },
                )
                .await;
            ctx.turn_guard.disarm();

            let Some(post_session) = post_session_opt else {
                info!(
                    chat_id,
                    generation = ctx.generation,
                    "turn was superseded, discarding output"
                );
                return Ok(());
            };
            if ctx.turn_cancel.is_cancelled() {
                info!(
                    chat_id,
                    generation = ctx.generation,
                    "turn was cancelled before delivery, discarding output"
                );
                return Ok(());
            }

            info!(
                chat_id,
                generation = ctx.generation,
                duration_secs = format_args!("{turn_duration:.2}"),
                context_tokens = post_session.last_context_tokens,
                queue_wait_ms,
                status = %res.status,
                backend = ?backend,
                detached = is_detached,
                "completed telegram turn"
            );

            let mut artifact_roots: Vec<PathBuf> =
                vec![PathBuf::from(&valid_workspace), scratch_dir];
            artifact_roots.extend(ctx.session.extra_dirs.iter().map(PathBuf::from));
            let active_brain_dir =
                artifacts::resolve_conversation_brain_dir_async(resolved_conv_id.as_deref()).await;

            let mut ai_widgets = Vec::new();
            let mut widget_cleaned = output_text.clone();
            let mut structured_card_handled = false;

            if ctx.force_card_schema
                && let Some(ref structured) = structured_output
            {
                match widget::widget_from_structured_card_value(structured) {
                    Ok(w) => {
                        ai_widgets.push(w);
                        structured_card_handled = true;
                        if output_text.trim_start().starts_with('{') {
                            widget_cleaned.clear();
                        }
                    }
                    Err(err) => {
                        warn!(
                            chat_id,
                            error = %err,
                            "failed to build card widget from structured_output; falling back to markdown extraction"
                        );
                    }
                }
            }

            if !structured_card_handled {
                let (cleaned_from_md, mut extracted_widgets) =
                    widget::extract_ai_widgets_with_roots(
                        &output_text,
                        &artifact_roots,
                        allow_prose_widget_files,
                    )
                    .await;
                widget_cleaned = cleaned_from_md;
                ai_widgets.append(&mut extracted_widgets);

                if ctx.force_card_schema
                    && ai_widgets.is_empty()
                    && let Ok(spec) = widget::CustomCardSpec::from_json(&output_text)
                    && let Ok(w) = widget::OutboundWidget::from_custom_card(&spec)
                {
                    ai_widgets.push(w);
                    widget_cleaned.clear();
                }
            }

            let (cleaned_output, explicit_artifacts) =
                artifacts::collect_explicit_deliverables_with_brain_async(
                    &ctx.text_part,
                    &widget_cleaned,
                    &artifact_roots,
                    active_brain_dir.as_deref(),
                )
                .await;
            let cleaned_output = cleaned_output.trim_end();

            let formatted_html = TelegramFormatter::to_telegram_html(cleaned_output);
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, TELEGRAM_TEXT_LIMIT);

            // Spawn TTS without cloning when voice is disabled.
            if ctx.should_send_voice
                && let Some(ref api_key) = ctx.config.fish_api_key
                && !cleaned_output.trim().is_empty()
            {
                let bot_voice = ctx.bot.clone();
                let api_key_cloned = api_key.clone();
                let ref_id = ctx
                    .session
                    .voice_reference_id
                    .clone()
                    .or_else(|| ctx.config.fish_reference_id.clone());
                let voice_text = cleaned_output.to_string();
                tokio::spawn(async move {
                    info!(chat_id = target_chat.0, "generating TTS voice reply");
                    if let Err(err) = bot_voice
                        .send_chat_action(target_chat, teloxide::types::ChatAction::RecordVoice)
                        .await
                    {
                        log_telegram_err(target_chat.0, "send_record_voice_action", err);
                    }
                    match crate::tts::generate_speech_bytes(
                        &voice_text,
                        &api_key_cloned,
                        ref_id.as_deref(),
                        crate::tts::TtsFormat::Opus.as_str(),
                    )
                    .await
                    {
                        Ok(audio_bytes) => {
                            info!(
                                chat_id = target_chat.0,
                                bytes = audio_bytes.len(),
                                "generated TTS voice audio"
                            );
                            let voice_file = teloxide::types::InputFile::memory(audio_bytes)
                                .file_name("voice_reply.ogg");
                            if let Err(err) = bot_voice.send_voice(target_chat, voice_file).await {
                                error!(chat_id = target_chat.0, error = ?err, "failed to send voice reply");
                            }
                        }
                        Err(err) => {
                            error!(chat_id = target_chat.0, error = %err, "TTS generation error");
                        }
                    }
                });
            }

            if chunks.is_empty() {
                info!(
                    chat_id,
                    widget_count = ai_widgets.len(),
                    "rendered text output is empty, reconciling draft messages"
                );
                let placeholder_done = if !ai_widgets.is_empty() {
                    "🎴 <b>已生成交互组件</b>"
                } else if !explicit_artifacts.is_empty() {
                    "📎 <b>已发送请求的文件</b>"
                } else {
                    "✅ <i>任务已完成</i>"
                };
                let done_chunk = [placeholder_done.to_string()];
                deliver_final_chunks(
                    &ctx.bot,
                    target_chat,
                    &render_outcome.msg_ids,
                    &render_outcome.last_rendered_html,
                    &done_chunk,
                    &ctx.turn_cancel,
                )
                .await;
            } else {
                deliver_final_chunks(
                    &ctx.bot,
                    target_chat,
                    &render_outcome.msg_ids,
                    &render_outcome.last_rendered_html,
                    &chunks,
                    &ctx.turn_cancel,
                )
                .await;
            }

            for w in &ai_widgets {
                if ctx.turn_cancel.is_cancelled() {
                    break;
                }
                widget::deliver_widget_with_fallback(
                    &ctx.bot,
                    &ctx.config.bot_token,
                    target_chat,
                    w,
                )
                .await;
            }

            media::deliver_resolved_artifacts(&ctx.bot, target_chat, &explicit_artifacts).await;

            if is_detached && !ctx.turn_cancel.is_cancelled() {
                Box::pin(stream_detached_followups(FollowupStreamParams {
                    bot: ctx.bot,
                    config: ctx.config,
                    chat_id: target_chat,
                    user_prompt: ctx.text_part,
                    stream_rx: render_outcome.stream_rx,
                    artifact_roots,
                    allow_prose_widget_files,
                    conversation_id: resolved_conv_id,
                    session_mgr: ctx.session_mgr,
                    generation: ctx.generation,
                    is_compact_turn,
                    turn_cancel: ctx.turn_cancel,
                }))
                .await;
            } else if !ctx.turn_cancel.is_cancelled() {
                crate::compaction::maybe_schedule_background_compact(
                    ctx.session_mgr.clone(),
                    ctx.config.clone(),
                    chat_id,
                    &post_session,
                    is_compact_turn,
                    is_detached,
                    saw_checkpoint,
                );
            }
        }
        Err(err) => {
            error!(chat_id, error = ?err, "agy execution error");
            let error_html = format!(
                "❌ <b>执行失败:</b>\n<pre><code>{}</code></pre>",
                text::escape_html(&err.to_string())
            );
            let err_chunks = TelegramFormatter::split_html_chunks(&error_html, TELEGRAM_TEXT_LIMIT);
            deliver_final_chunks(
                &ctx.bot,
                target_chat,
                &render_outcome.msg_ids,
                "",
                &err_chunks,
                &ctx.turn_cancel,
            )
            .await;
        }
    }

    Ok(())
}

#[derive(Default)]
struct InitialStreamSeed {
    accumulated_text: String,
    last_tool_call: String,
    last_tool_summary: String,
    last_thinking: String,
    checkpoint_noted: bool,
    last_subagent_update: String,
    last_tool_error: Option<(String, String)>,
    error_msg: Option<String>,
    is_dirty: bool,
}

struct StreamRenderOptions<'a> {
    initial_placeholder: PlaceholderState,
    placeholder_html: &'static str,
    draft_interval_ms: u64,
    show_typing: bool,
    overwrite_on_result: bool,
    break_on_turn_complete: bool,
    session_guard: Option<(&'a SessionManager, u64)>,
    turn_cancel: &'a CancellationToken,
}

struct StreamRenderOutcome {
    stream_rx: mpsc::Receiver<StreamEvent>,
    msg_ids: Vec<MessageId>,
    last_rendered_html: String,
    accumulated_text: String,
    error_msg: Option<String>,
    channel_closed: bool,
    saw_checkpoint: bool,
    result_context_tokens: Option<u64>,
    result_status: Option<String>,
}

struct DraftSectionCache {
    cached_tool_html: String,
    tool_dirty: bool,
    cached_thinking_html: String,
    thinking_dirty: bool,
    thinking_capped: bool,
    cached_text_html: String,
    text_dirty: bool,
}

impl DraftSectionCache {
    fn new(tool_dirty: bool, thinking_dirty: bool, text_dirty: bool) -> Self {
        Self {
            cached_tool_html: String::new(),
            tool_dirty,
            cached_thinking_html: String::new(),
            thinking_dirty,
            thinking_capped: false,
            cached_text_html: String::new(),
            text_dirty,
        }
    }

    fn render_draft(
        &mut self,
        state: &StreamBuffers,
        placeholder_html: &str,
        draft_buf: &mut String,
    ) {
        draft_buf.clear();

        if state.checkpoint_noted {
            draft_buf.push_str("🗜️ <i>已自动压缩上下文</i>\n\n");
        }

        if !state.last_subagent_update.is_empty() {
            let preview = text::truncate_chars(&state.last_subagent_update, 180);
            let _ = write!(
                draft_buf,
                "🤖 <i>子代理进度: {}</i>\n\n",
                text::escape_html(&preview)
            );
        }

        if !state.last_tool_call.is_empty() {
            if self.tool_dirty {
                let mut tool_card = String::new();
                if state.tool_count > 1 {
                    let _ = writeln!(tool_card, "🔧 <i>工具调用进度 #{}</i>", state.tool_count);
                }
                tool_card.push_str(&TelegramFormatter::format_tool_call(
                    &state.last_tool_call,
                    &state.last_tool_summary,
                ));
                self.cached_tool_html = tool_card;
                self.tool_dirty = false;
            }
            draft_buf.push_str(&self.cached_tool_html);
        }

        if let Some((ref tool, ref msg)) = state.last_tool_error {
            let msg_preview = text::truncate_chars(msg, 240);
            let _ = write!(
                draft_buf,
                "⚠️ <b>工具异常</b> (<code>{}</code>): <code>{}</code>\n\n",
                text::escape_html(tool),
                text::escape_html(&msg_preview)
            );
        }

        if !state.last_thinking.is_empty() {
            if self.thinking_dirty {
                self.cached_thinking_html =
                    TelegramFormatter::format_thinking(&state.last_thinking);
                self.thinking_dirty = false;
                if state
                    .last_thinking
                    .trim()
                    .chars()
                    .nth(MAX_STREAM_THINKING_CHARS)
                    .is_some()
                {
                    self.thinking_capped = true;
                }
            }
            draft_buf.push_str(&self.cached_thinking_html);
        }

        if let Some(ref err) = state.error_msg {
            let _ = write!(
                draft_buf,
                "\n❌ <b>Error:</b> <code>{}</code>\n",
                text::escape_html(err)
            );
        }

        if !state.accumulated_text.is_empty() {
            if self.text_dirty {
                let draft_md = strip_widget_fences_for_streaming_draft(&state.accumulated_text);
                self.cached_text_html = TelegramFormatter::to_telegram_html(draft_md.as_ref());
                self.text_dirty = false;
            }
            if !self.cached_text_html.is_empty() {
                draft_buf.push_str(&self.cached_text_html);
            }
        } else if !draft_buf.is_empty() && state.error_msg.is_none() {
            if !state.last_thinking.is_empty() {
                draft_buf.push_str("⏳ <i>正在推理并组织回复...</i>");
            } else {
                draft_buf.push_str("⏳ <i>正在执行工具...</i>");
            }
        }

        if draft_buf.is_empty() {
            draft_buf.push_str(placeholder_html);
        }
    }
}

#[derive(Default)]
struct StreamBuffers {
    accumulated_text: String,
    last_tool_call: String,
    last_tool_summary: String,
    tool_count: usize,
    last_thinking: String,
    checkpoint_noted: bool,
    last_subagent_update: String,
    last_tool_error: Option<(String, String)>,
    error_msg: Option<String>,
}

/// Shared single `tokio::select!` (`biased;`) stream ingestion and Telegram draft renderer used
/// by both initial prompt turns and detached background follow-ups.
async fn stream_and_render_turn(
    bot: &Bot,
    chat_id: ChatId,
    mut stream_rx: mpsc::Receiver<StreamEvent>,
    opts: StreamRenderOptions<'_>,
    seed: InitialStreamSeed,
) -> StreamRenderOutcome {
    let tool_count = usize::from(!seed.last_tool_call.is_empty());
    let mut state = StreamBuffers {
        accumulated_text: seed.accumulated_text,
        last_tool_call: seed.last_tool_call,
        last_tool_summary: seed.last_tool_summary,
        tool_count,
        last_thinking: seed.last_thinking,
        checkpoint_noted: seed.checkpoint_noted,
        last_subagent_update: seed.last_subagent_update,
        last_tool_error: seed.last_tool_error,
        error_msg: seed.error_msg,
    };
    let mut is_dirty = seed.is_dirty;
    let mut cache = DraftSectionCache::new(
        !state.last_tool_call.is_empty(),
        !state.last_thinking.is_empty(),
        !state.accumulated_text.is_empty(),
    );

    let mut placeholder = opts.initial_placeholder;
    let mut msg_ids = Vec::new();
    if let PlaceholderState::Ready(id) = &placeholder {
        msg_ids.push(*id);
    }
    let mut last_rendered_idx: Option<usize> = None;
    let mut last_rendered_html = String::new();
    let mut draft_buf = String::with_capacity(2048);
    let mut channel_closed = false;
    let mut saw_checkpoint = state.checkpoint_noted;
    let mut result_context_tokens: Option<u64> = None;
    let mut result_status: Option<String> = None;

    let base_interval = Duration::from_millis(opts.draft_interval_ms.max(500));
    let max_adaptive_interval = Duration::from_secs(MAX_ADAPTIVE_INTERVAL_SECS).max(base_interval);
    let mut current_interval = base_interval;
    let mut consecutive_ok_edits = 0u32;
    let mut has_rendered_once = false;
    let mut last_edit_at: Option<tokio::time::Instant> = None;
    let mut backoff_until: Option<tokio::time::Instant> = None;
    let initial_deadline = if is_dirty {
        tokio::time::Instant::now()
    } else {
        tokio::time::Instant::now() + base_interval
    };
    let render_sleep = tokio::time::sleep_until(initial_deadline);
    tokio::pin!(render_sleep);
    let typing_sleep = tokio::time::sleep(Duration::from_secs(4));
    tokio::pin!(typing_sleep);

    loop {
        tokio::select! {
            biased;
            _ = opts.turn_cancel.cancelled() => {
                break;
            }
            maybe_ev = stream_rx.recv() => {
                let was_dirty = is_dirty;
                let mut is_tool_event = false;
                match maybe_ev {
                    Some(StreamEvent::Init { .. }) => {}
                    Some(StreamEvent::TextDelta(delta)) => {
                        state.accumulated_text.push_str(&delta);
                        cache.text_dirty = true;
                        is_dirty = true;
                    }
                    Some(StreamEvent::ThinkingDelta(thinking)) => {
                        if !cache.thinking_capped {
                            let remaining = MAX_STREAM_THINKING_BYTES
                                .saturating_sub(state.last_thinking.len());
                            if remaining > 0 {
                                if thinking.len() <= remaining {
                                    state.last_thinking.push_str(&thinking);
                                } else {
                                    let mut cut = remaining;
                                    while cut > 0 && !thinking.is_char_boundary(cut) {
                                        cut -= 1;
                                    }
                                    state.last_thinking.push_str(&thinking[..cut]);
                                }
                                cache.thinking_dirty = true;
                                is_dirty = true;
                            }
                            if state.last_thinking.len() >= MAX_STREAM_THINKING_BYTES
                                || state
                                    .last_thinking
                                    .trim()
                                    .chars()
                                    .nth(MAX_STREAM_THINKING_CHARS)
                                    .is_some()
                            {
                                cache.thinking_capped = true;
                            }
                        }
                    }
                    Some(StreamEvent::ToolUse { tool, summary }) => {
                        if state.last_tool_call != tool || state.last_tool_summary != summary {
                            state.last_tool_call = tool;
                            state.last_tool_summary = summary;
                            state.tool_count += 1;
                            cache.tool_dirty = true;
                            is_tool_event = true;
                            is_dirty = true;
                        }
                    }
                    Some(StreamEvent::Checkpoint) => {
                        saw_checkpoint = true;
                        if !state.checkpoint_noted {
                            state.checkpoint_noted = true;
                            is_tool_event = true;
                            is_dirty = true;
                        }
                    }
                    Some(StreamEvent::SubagentUpdate { summary }) => {
                        if !summary.trim().is_empty() && state.last_subagent_update != summary {
                            state.last_subagent_update = summary;
                            is_tool_event = true;
                            is_dirty = true;
                        }
                    }
                    Some(StreamEvent::ToolError { tool, message }) => {
                        let changed = state.last_tool_error.as_ref().is_none_or(
                            |(prev_tool, prev_msg)| *prev_tool != tool || *prev_msg != message,
                        );
                        if changed {
                            state.last_tool_error = Some((tool, message));
                            is_tool_event = true;
                            is_dirty = true;
                        }
                    }
                    Some(StreamEvent::TurnComplete { text }) => {
                        if !text.is_empty() {
                            state.accumulated_text = text;
                            cache.text_dirty = true;
                        }
                        if opts.break_on_turn_complete {
                            break;
                        }
                        is_dirty = true;
                    }
                    Some(StreamEvent::Result(res)) => {
                        let res = *res;
                        if res.saw_checkpoint {
                            saw_checkpoint = true;
                        }
                        if res.context_tokens.is_some() {
                            result_context_tokens = res.context_tokens;
                        }
                        if !res.status.trim().is_empty() {
                            result_status = Some(res.status);
                        }
                        if !res.response.is_empty()
                            && (opts.overwrite_on_result || state.accumulated_text.trim().is_empty())
                        {
                            state.accumulated_text = res.response;
                            cache.text_dirty = true;
                        }
                        channel_closed = true;
                        break;
                    }
                    Some(StreamEvent::Error(err)) => {
                        state.error_msg = Some(err);
                        channel_closed = true;
                        break;
                    }
                    None => {
                        channel_closed = true;
                        break;
                    }
                }
                if is_dirty && !was_dirty {
                    let now = tokio::time::Instant::now();
                    let min_next_edit = last_edit_at.map_or(now, |t| t + base_interval);
                    if let Some(until) = backoff_until
                        && until > now
                    {
                        render_sleep.as_mut().reset(until);
                    } else if is_tool_event {
                        render_sleep.as_mut().reset(now.max(min_next_edit));
                    } else if !has_rendered_once {
                        render_sleep.as_mut().reset(
                            (now + Duration::from_millis(FIRST_TEXT_DEBOUNCE_MS))
                                .max(min_next_edit),
                        );
                    } else if render_sleep.deadline() < min_next_edit {
                        render_sleep.as_mut().reset(min_next_edit);
                    }
                }
            }
            _ = &mut render_sleep, if is_dirty => {
                if let Some((mgr, turn_gen)) = opts.session_guard
                    && !mgr.is_generation_current(chat_id.0, turn_gen).await {
                        break;
                    }

                if msg_ids.is_empty()
                    && let Some(ph_id) = placeholder.resolve(opts.turn_cancel).await
                {
                    msg_ids.push(ph_id);
                }
                if opts.turn_cancel.is_cancelled() {
                    break;
                }

                cache.render_draft(&state, opts.placeholder_html, &mut draft_buf);

                let balanced_draft = TelegramFormatter::balance_streaming_html(&draft_buf);
                let chunks = TelegramFormatter::split_html_chunks(&balanced_draft, TELEGRAM_TEXT_LIMIT);
                let visible_utf16 = TelegramFormatter::visible_utf16_len(&balanced_draft);
                let paced_interval =
                    compute_next_edit_interval(current_interval, visible_utf16, chunks.len());

                // Multi-message rollover: seal earlier chunks and send each new chunk directly
                // (eliminating the `<i>(续)...</i>` send + immediate edit double-call flicker).
                let mut rollover_retry_after = None;
                while chunks.len() > msg_ids.len() {
                    if let Some(&active_id) = msg_ids.last() {
                        let sealed_idx = msg_ids.len() - 1;
                        let completed_chunk = &chunks[sealed_idx];
                        if last_rendered_idx != Some(sealed_idx)
                            || completed_chunk != &last_rendered_html
                        {
                            match bot
                                .edit_message_text(chat_id, active_id, completed_chunk)
                                .parse_mode(ParseMode::Html)
                                .await
                            {
                                Ok(_) => {
                                    last_rendered_idx = Some(sealed_idx);
                                    last_rendered_html.clone_from(completed_chunk);
                                    last_edit_at = Some(tokio::time::Instant::now());
                                }
                                Err(err) if is_message_not_modified_err(&err) => {
                                    last_rendered_idx = Some(sealed_idx);
                                    last_rendered_html.clone_from(completed_chunk);
                                }
                                Err(teloxide::RequestError::RetryAfter(secs)) => {
                                    consecutive_ok_edits = 0;
                                    current_interval =
                                        (current_interval * 2).min(max_adaptive_interval);
                                    rollover_retry_after = Some(
                                        Duration::from_secs(u64::from(secs.seconds()) + 1)
                                            .max(current_interval),
                                    );
                                    break;
                                }
                                Err(err) if is_html_entity_api_error(&err) => {
                                    log_telegram_err(chat_id.0, "rollover_seal_edit_html", err);
                                    let plain_parts = html_chunk_to_plain_fallback(completed_chunk);
                                    if let Some(first_plain) = plain_parts.first() {
                                        let _ = bot
                                            .edit_message_text(chat_id, active_id, first_plain)
                                            .await;
                                    }
                                    last_rendered_idx = Some(sealed_idx);
                                    last_rendered_html.clone_from(completed_chunk);
                                    last_edit_at = Some(tokio::time::Instant::now());
                                }
                                Err(err) => {
                                    log_telegram_err(chat_id.0, "rollover_seal_edit", err);
                                }
                            }
                        }
                    }

                    let next_idx = msg_ids.len();
                    let next_chunk = &chunks[next_idx];
                    match bot
                        .send_message(chat_id, next_chunk)
                        .parse_mode(ParseMode::Html)
                        .await
                    {
                        Ok(new_msg) => {
                            msg_ids.push(new_msg.id);
                            last_rendered_idx = Some(next_idx);
                            last_rendered_html.clone_from(next_chunk);
                            backoff_until = None;
                            has_rendered_once = true;
                            last_edit_at = Some(tokio::time::Instant::now());
                            consecutive_ok_edits += 1;
                            if consecutive_ok_edits >= CLEAN_EDITS_TO_DECAY_BACKOFF
                                && current_interval > base_interval
                            {
                                current_interval = (current_interval / 2).max(base_interval);
                                consecutive_ok_edits = 0;
                            }
                        }
                        Err(teloxide::RequestError::RetryAfter(secs)) => {
                            consecutive_ok_edits = 0;
                            current_interval = (current_interval * 2).min(max_adaptive_interval);
                            rollover_retry_after = Some(
                                Duration::from_secs(u64::from(secs.seconds()) + 1)
                                    .max(current_interval),
                            );
                            break;
                        }
                        Err(err) if is_html_entity_api_error(&err) => {
                            log_telegram_err(chat_id.0, "rollover_send_html", err);
                            let plain_parts = html_chunk_to_plain_fallback(next_chunk);
                            let fallback_text =
                                plain_parts.first().map_or("...", String::as_str);
                            match bot.send_message(chat_id, fallback_text).await {
                                Ok(new_msg) => {
                                    msg_ids.push(new_msg.id);
                                    last_rendered_idx = Some(next_idx);
                                    last_rendered_html.clone_from(next_chunk);
                                    backoff_until = None;
                                    has_rendered_once = true;
                                    last_edit_at = Some(tokio::time::Instant::now());
                                }
                                Err(plain_err) => {
                                    warn!(chat_id = chat_id.0, error = ?plain_err, "failed to send plain rollover message");
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            warn!(chat_id = chat_id.0, error = ?err, "failed to send rollover message");
                            break;
                        }
                    }
                }

                if let Some(wait) = rollover_retry_after {
                    let until = tokio::time::Instant::now() + wait;
                    backoff_until = Some(until);
                    render_sleep.as_mut().reset(until);
                    continue;
                }

                if !chunks.is_empty() && !msg_ids.is_empty() {
                    // Clamp to `chunks.len() - 1` so shrinking drafts (e.g. closed widget fences)
                    // continue updating smoothly instead of stalling until final delivery.
                    let active_idx = (chunks.len() - 1).min(msg_ids.len() - 1);
                    let active_msg_id = msg_ids[active_idx];
                    let active_chunk = &chunks[active_idx];
                    if last_rendered_idx != Some(active_idx) || active_chunk != &last_rendered_html {
                        match bot
                            .edit_message_text(chat_id, active_msg_id, active_chunk)
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            Ok(_) => {
                                last_rendered_idx = Some(active_idx);
                                last_rendered_html.clone_from(active_chunk);
                                backoff_until = None;
                                has_rendered_once = true;
                                last_edit_at = Some(tokio::time::Instant::now());
                                consecutive_ok_edits += 1;
                                if consecutive_ok_edits >= CLEAN_EDITS_TO_DECAY_BACKOFF
                                    && current_interval > base_interval
                                {
                                    current_interval = (current_interval / 2).max(base_interval);
                                    consecutive_ok_edits = 0;
                                }
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + paced_interval);
                            }
                            Err(teloxide::RequestError::RetryAfter(retry_secs)) => {
                                consecutive_ok_edits = 0;
                                current_interval = (current_interval * 2).min(max_adaptive_interval);
                                let wait = Duration::from_secs(u64::from(retry_secs.seconds()) + 1)
                                    .max(current_interval);
                                let until = tokio::time::Instant::now() + wait;
                                backoff_until = Some(until);
                                is_dirty = true;
                                render_sleep.as_mut().reset(until);
                            }
                            Err(err) if is_message_not_modified_err(&err) => {
                                last_rendered_idx = Some(active_idx);
                                last_rendered_html.clone_from(active_chunk);
                                backoff_until = None;
                                has_rendered_once = true;
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + paced_interval);
                            }
                            Err(err) if is_html_entity_api_error(&err) => {
                                warn!(chat_id = chat_id.0, error = ?err, "draft HTML edit rejected; falling back to plain draft");
                                let plain_parts = html_chunk_to_plain_fallback(active_chunk);
                                if let Some(first_plain) = plain_parts.first() {
                                    let _ = bot.edit_message_text(chat_id, active_msg_id, first_plain).await;
                                }
                                last_rendered_idx = Some(active_idx);
                                last_rendered_html.clone_from(active_chunk);
                                backoff_until = None;
                                has_rendered_once = true;
                                last_edit_at = Some(tokio::time::Instant::now());
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + paced_interval);
                            }
                            Err(err) => {
                                warn!(chat_id = chat_id.0, error = ?err, "draft edit rejected");
                                backoff_until = None;
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + paced_interval);
                            }
                        }
                    } else {
                        is_dirty = false;
                        render_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + paced_interval);
                    }
                } else {
                    is_dirty = false;
                    render_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + paced_interval);
                }
            }
            _ = &mut typing_sleep, if opts.show_typing => {
                let now = tokio::time::Instant::now();
                if let Some(until) = backoff_until
                    && until > now
                {
                    typing_sleep.as_mut().reset(until);
                    continue;
                }
                let action = if state.last_tool_call == "generate_image" {
                    teloxide::types::ChatAction::UploadPhoto
                } else if state.last_tool_call.contains("file")
                    || state.last_tool_call.contains("resource")
                {
                    teloxide::types::ChatAction::UploadDocument
                } else {
                    teloxide::types::ChatAction::Typing
                };
                let typing_bot = bot.clone();
                tokio::spawn(async move {
                    if let Err(err) = typing_bot.send_chat_action(chat_id, action).await {
                        log_telegram_err(chat_id.0, "send_chat_action", err);
                    }
                });
                typing_sleep
                    .as_mut()
                    .reset(now + Duration::from_secs(4));
            }
        }
    }

    if msg_ids.is_empty()
        && !opts.turn_cancel.is_cancelled()
        && let Some(ph_id) = placeholder.resolve(opts.turn_cancel).await
    {
        msg_ids.push(ph_id);
    }

    StreamRenderOutcome {
        stream_rx,
        msg_ids,
        last_rendered_html,
        accumulated_text: state.accumulated_text,
        error_msg: state.error_msg,
        channel_closed,
        saw_checkpoint,
        result_context_tokens,
        result_status,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FinalReconcilePlan {
    pub edits: Vec<(usize, MessageId)>,
    pub stale_to_delete: Vec<MessageId>,
    pub sends: Range<usize>,
}

/// Computes how streamed draft `msg_ids` map onto `final_chunks_len` chunks so a shorter
/// final answer never leaves a trailing `<i>(续)...</i>` rollover message frozen.
#[must_use]
pub(crate) fn plan_final_reconciliation(
    msg_ids: &[MessageId],
    final_chunks_len: usize,
) -> FinalReconcilePlan {
    let edit_count = final_chunks_len.min(msg_ids.len());
    let edits = (0..edit_count).map(|idx| (idx, msg_ids[idx])).collect();
    let stale_to_delete = if final_chunks_len < msg_ids.len() {
        msg_ids[final_chunks_len..].to_vec()
    } else {
        Vec::new()
    };
    let sends = msg_ids.len()..final_chunks_len;
    FinalReconcilePlan {
        edits,
        stale_to_delete,
        sends,
    }
}

/// Splits plain text safely on UTF-8 character boundaries without exceeding `max_utf16`
/// UTF-16 code units per chunk (matching Telegram Bot API's 4096 UTF-16 unit limit).
#[must_use]
pub(crate) fn split_plain_chunks(plain: &str, max_utf16: usize) -> Vec<String> {
    let limit = max_utf16.max(1);
    // Every valid UTF-8 char uses at least as many UTF-8 bytes as UTF-16 code units.
    if plain.len() <= limit {
        return vec![plain.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    let half_limit = limit / 2;
    while start < plain.len() {
        let rest = &plain[start..];
        let mut utf16_used = 0usize;
        let mut end = start;
        let mut last_nl_after_half: Option<usize> = None;

        for (rel_idx, ch) in rest.char_indices() {
            let ch_u16 = ch.len_utf16();
            if utf16_used + ch_u16 > limit && end > start {
                break;
            }
            utf16_used += ch_u16;
            end = start + rel_idx + ch.len_utf8();
            if ch == '\n' && utf16_used > half_limit {
                last_nl_after_half = Some(end);
            }
        }

        if end < plain.len()
            && let Some(nl_end) = last_nl_after_half
        {
            end = nl_end;
        }

        chunks.push(plain[start..end].to_string());
        start = end;
    }
    chunks
}

/// Converts an HTML chunk into Telegram-safe plain text with zero raw HTML tags.
#[must_use]
pub(crate) fn html_chunk_to_plain_fallback(html_chunk: &str) -> Vec<String> {
    let plain = text::strip_html_tags(html_chunk);
    let trimmed = plain.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    split_plain_chunks(trimmed, TELEGRAM_TEXT_LIMIT)
}

async fn sleep_retry_after(secs: u32, cancel: &CancellationToken) -> bool {
    let wait = Duration::from_secs(u64::from(secs) + 1);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(wait) => true,
    }
}

async fn send_plain_chunks(
    bot: &Bot,
    chat_id: ChatId,
    edit_msg_id: Option<MessageId>,
    plain_chunks: &[String],
    reply_markup: Option<InlineKeyboardMarkup>,
    cancel: &CancellationToken,
) {
    let total = plain_chunks.len();
    for (idx, plain) in plain_chunks.iter().enumerate() {
        if cancel.is_cancelled() {
            return;
        }
        let is_last = idx + 1 == total;
        let mut retried = false;
        let mut edit_failed = false;
        loop {
            let res = if idx == 0
                && !edit_failed
                && let Some(mid) = edit_msg_id
            {
                let mut req = bot.edit_message_text(chat_id, mid, plain);
                if is_last && let Some(ref kb) = reply_markup {
                    req = req.reply_markup(kb.clone());
                }
                req.await.map(|_| ())
            } else {
                let mut req = bot.send_message(chat_id, plain);
                if is_last && let Some(ref kb) = reply_markup {
                    req = req.reply_markup(kb.clone());
                }
                req.await.map(|_| ())
            };
            match res {
                Ok(()) => break,
                Err(err) if is_message_not_modified_err(&err) => break,
                Err(teloxide::RequestError::RetryAfter(secs)) if !retried => {
                    retried = true;
                    if !sleep_retry_after(secs.seconds(), cancel).await {
                        return;
                    }
                }
                Err(err) if idx == 0 && edit_msg_id.is_some() && !edit_failed => {
                    log_telegram_err(chat_id.0, "edit_plain_fallback", err);
                    edit_failed = true;
                }
                Err(err) => {
                    log_telegram_err(chat_id.0, "send_plain_fallback", err);
                    break;
                }
            }
        }
    }
}

async fn edit_html_chunk_with_fallback(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
    html_chunk: &str,
    reply_markup: Option<InlineKeyboardMarkup>,
    cancel: &CancellationToken,
) {
    let mut retried = false;
    let html_rejected = loop {
        if cancel.is_cancelled() {
            return;
        }
        let mut req = bot
            .edit_message_text(chat_id, msg_id, html_chunk)
            .parse_mode(ParseMode::Html);
        if let Some(ref kb) = reply_markup {
            req = req.reply_markup(kb.clone());
        }
        match req.await {
            Ok(_) => return,
            Err(err) if is_message_not_modified_err(&err) => return,
            Err(teloxide::RequestError::RetryAfter(secs)) if !retried => {
                retried = true;
                if !sleep_retry_after(secs.seconds(), cancel).await {
                    return;
                }
            }
            Err(err) => {
                let rejected = is_html_entity_api_error(&err);
                error!(
                    chat_id = chat_id.0,
                    msg_id = msg_id.0,
                    html_rejected = rejected,
                    error = ?err,
                    "final HTML edit rejected"
                );
                break rejected;
            }
        }
    };

    // Try sending as a fresh HTML message only when the edit failed for a non-HTML reason
    // (such as deleted original message); skip redundant HTML send if Telegram rejected entities.
    if !html_rejected {
        let mut send_retried = false;
        loop {
            if cancel.is_cancelled() {
                return;
            }
            let mut req = bot
                .send_message(chat_id, html_chunk)
                .parse_mode(ParseMode::Html);
            if let Some(ref kb) = reply_markup {
                req = req.reply_markup(kb.clone());
            }
            match req.await {
                Ok(_) => return,
                Err(teloxide::RequestError::RetryAfter(secs)) if !send_retried => {
                    send_retried = true;
                    if !sleep_retry_after(secs.seconds(), cancel).await {
                        return;
                    }
                }
                Err(err) => {
                    warn!(chat_id = chat_id.0, error = ?err, "fresh HTML fallback rejected; stripping tags");
                    break;
                }
            }
        }
    }

    let plain_chunks = html_chunk_to_plain_fallback(html_chunk);
    send_plain_chunks(
        bot,
        chat_id,
        Some(msg_id),
        &plain_chunks,
        reply_markup,
        cancel,
    )
    .await;
}

async fn send_html_chunk_with_fallback(
    bot: &Bot,
    chat_id: ChatId,
    html_chunk: &str,
    reply_markup: Option<InlineKeyboardMarkup>,
    cancel: &CancellationToken,
) {
    let mut retried = false;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let mut req = bot
            .send_message(chat_id, html_chunk)
            .parse_mode(ParseMode::Html);
        if let Some(ref kb) = reply_markup {
            req = req.reply_markup(kb.clone());
        }
        match req.await {
            Ok(_) => return,
            Err(teloxide::RequestError::RetryAfter(secs)) if !retried => {
                retried = true;
                if !sleep_retry_after(secs.seconds(), cancel).await {
                    return;
                }
            }
            Err(err) => {
                warn!(chat_id = chat_id.0, error = ?err, "continuation HTML send rejected; stripping tags");
                break;
            }
        }
    }

    let plain_chunks = html_chunk_to_plain_fallback(html_chunk);
    send_plain_chunks(bot, chat_id, None, &plain_chunks, reply_markup, cancel).await;
}

async fn cleanup_stale_rollover_messages(
    bot: &Bot,
    chat_id: ChatId,
    msg_ids: &[MessageId],
    keep_count: usize,
    cancel: &CancellationToken,
) {
    for &stale_id in msg_ids.iter().skip(keep_count) {
        if cancel.is_cancelled() {
            return;
        }
        if let Err(err) = bot.delete_message(chat_id, stale_id).await {
            warn!(
                chat_id = chat_id.0,
                msg_id = stale_id.0,
                error = ?err,
                "failed to delete stale rollover message; closing placeholder"
            );
            if let Err(edit_err) = bot
                .edit_message_text(chat_id, stale_id, "<i>(以上为完整回复)</i>")
                .parse_mode(ParseMode::Html)
                .await
            {
                log_telegram_err(chat_id.0, "close_stale_rollover_placeholder", edit_err);
            }
        }
    }
}

async fn deliver_final_chunks(
    bot: &Bot,
    chat_id: ChatId,
    msg_ids: &[MessageId],
    last_rendered_html: &str,
    chunks: &[String],
    cancel: &CancellationToken,
) {
    let plan = plan_final_reconciliation(msg_ids, chunks.len());
    let no_rollover_unchanged =
        msg_ids.len() == 1 && chunks.len() == 1 && chunks[0] == last_rendered_html;

    for (idx, msg_id) in plan.edits {
        if cancel.is_cancelled() {
            return;
        }
        if idx == 0 && no_rollover_unchanged {
            continue;
        }
        edit_html_chunk_with_fallback(bot, chat_id, msg_id, &chunks[idx], None, cancel).await;
    }

    if !plan.stale_to_delete.is_empty() {
        cleanup_stale_rollover_messages(bot, chat_id, msg_ids, chunks.len(), cancel).await;
    }

    for idx in plan.sends {
        if cancel.is_cancelled() {
            return;
        }
        send_html_chunk_with_fallback(bot, chat_id, &chunks[idx], None, cancel).await;
    }
}

/// Sends multi-chunk HTML replies for commands (splitting at 4000 UTF-16 units, respecting
/// `RetryAfter`, and falling back to tag-stripped plain text if HTML is rejected), optionally
/// attaching `final_markup` to the last chunk.
pub(crate) async fn send_html_chunks_with_markup(
    bot: &Bot,
    chat_id: ChatId,
    html: &str,
    final_markup: Option<InlineKeyboardMarkup>,
) {
    let chunks = TelegramFormatter::split_html_chunks(html, TELEGRAM_TEXT_LIMIT);
    let total = chunks.len();
    let cancel = CancellationToken::new();
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let markup = if idx + 1 == total {
            final_markup.clone()
        } else {
            None
        };
        send_html_chunk_with_fallback(bot, chat_id, &chunk, markup, &cancel).await;
    }
}

/// Sends multi-chunk HTML replies for commands (splitting at 4000 chars, respecting `RetryAfter`,
/// and falling back to tag-stripped plain text if HTML is rejected).
pub(crate) async fn send_html_chunks(bot: &Bot, chat_id: ChatId, html: &str) {
    send_html_chunks_with_markup(bot, chat_id, html, None).await;
}

/// Replaces an existing placeholder/card message with chunk 0 and sends overflow chunks when a
/// command or callback output exceeds 4000 characters, attaching `final_markup` to the last chunk.
pub(crate) async fn edit_or_send_html_chunks(
    bot: &Bot,
    chat_id: ChatId,
    msg_id: MessageId,
    html: &str,
    final_markup: Option<InlineKeyboardMarkup>,
) {
    let chunks = TelegramFormatter::split_html_chunks(html, TELEGRAM_TEXT_LIMIT);
    let total = chunks.len();
    let cancel = CancellationToken::new();

    for (idx, chunk) in chunks.into_iter().enumerate() {
        let markup = if idx + 1 == total {
            final_markup.clone()
        } else {
            None
        };
        if idx == 0 {
            edit_html_chunk_with_fallback(bot, chat_id, msg_id, &chunk, markup, &cancel).await;
        } else {
            send_html_chunk_with_fallback(bot, chat_id, &chunk, markup, &cancel).await;
        }
    }
}

struct FollowupStreamParams {
    bot: Bot,
    config: Arc<Config>,
    chat_id: ChatId,
    user_prompt: String,
    stream_rx: mpsc::Receiver<StreamEvent>,
    artifact_roots: Vec<PathBuf>,
    allow_prose_widget_files: bool,
    conversation_id: Option<String>,
    session_mgr: Arc<SessionManager>,
    generation: u64,
    is_compact_turn: bool,
    turn_cancel: CancellationToken,
}

async fn stream_detached_followups(params: FollowupStreamParams) {
    let FollowupStreamParams {
        bot,
        config,
        chat_id,
        user_prompt,
        mut stream_rx,
        artifact_roots,
        allow_prose_widget_files,
        mut conversation_id,
        session_mgr,
        generation,
        is_compact_turn,
        turn_cancel,
    } = params;
    let mut any_followup_checkpoint = false;

    loop {
        let mut seed = InitialStreamSeed {
            is_dirty: true,
            ..Default::default()
        };
        let mut turn_already_done = false;
        let mut channel_closed = false;
        let mut turn_saw_checkpoint = false;
        let mut turn_context_tokens: Option<u64> = None;

        // Wait for the first meaningful event of a follow-up turn.
        loop {
            let ev = tokio::select! {
                biased;
                _ = turn_cancel.cancelled() => {
                    session_mgr.clear_detached_pid(chat_id.0, generation).await;
                    return;
                }
                ev = stream_rx.recv() => ev,
            };
            match ev {
                Some(StreamEvent::Init {
                    conversation_id: cid,
                }) => {
                    if !cid.trim().is_empty() {
                        conversation_id = Some(cid);
                    }
                }
                Some(StreamEvent::TextDelta(delta)) => {
                    seed.accumulated_text.push_str(&delta);
                    break;
                }
                Some(StreamEvent::ThinkingDelta(thinking)) => {
                    seed.last_thinking.push_str(&thinking);
                    break;
                }
                Some(StreamEvent::ToolUse { tool, summary }) => {
                    seed.last_tool_call = tool;
                    seed.last_tool_summary = summary;
                    break;
                }
                Some(StreamEvent::Checkpoint) => {
                    seed.checkpoint_noted = true;
                    turn_saw_checkpoint = true;
                    any_followup_checkpoint = true;
                }
                Some(StreamEvent::SubagentUpdate { summary }) => {
                    if !summary.trim().is_empty() {
                        seed.last_subagent_update = summary;
                        break;
                    }
                }
                Some(StreamEvent::ToolError { tool, message }) => {
                    seed.last_tool_error = Some((tool, message));
                    break;
                }
                Some(StreamEvent::TurnComplete { text }) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    seed.accumulated_text = text;
                    turn_already_done = true;
                    break;
                }
                Some(StreamEvent::Result(res)) => {
                    let res = *res;
                    if !res.status.eq_ignore_ascii_case("SUCCESS") {
                        warn!(
                            chat_id = chat_id.0,
                            generation,
                            status = %res.status,
                            "detached telegram follow-up finished with non-success status"
                        );
                    }
                    if let Some(cid) = res
                        .conversation_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        conversation_id = Some(cid.to_string());
                    }
                    if res.saw_checkpoint {
                        turn_saw_checkpoint = true;
                        any_followup_checkpoint = true;
                    }
                    if res.context_tokens.is_some() {
                        turn_context_tokens = res.context_tokens;
                    }
                    if !res.response.trim().is_empty() {
                        seed.accumulated_text = res.response;
                        turn_already_done = true;
                    }
                    channel_closed = true;
                    break;
                }
                Some(StreamEvent::Error(err)) => {
                    seed.error_msg = Some(err);
                    turn_already_done = true;
                    channel_closed = true;
                    break;
                }
                None => {
                    channel_closed = true;
                    break;
                }
            }
        }

        if channel_closed
            && seed.accumulated_text.trim().is_empty()
            && seed.last_tool_call.is_empty()
            && seed.last_thinking.is_empty()
            && seed.last_subagent_update.is_empty()
            && seed.last_tool_error.is_none()
            && seed.error_msg.is_none()
        {
            if turn_context_tokens.is_some() || turn_saw_checkpoint {
                session_mgr
                    .record_detached_followup_result(
                        chat_id.0,
                        generation,
                        turn_context_tokens,
                        turn_saw_checkpoint,
                    )
                    .await;
            }
            break;
        }

        if !session_mgr
            .is_generation_current(chat_id.0, generation)
            .await
            || turn_cancel.is_cancelled()
        {
            info!(
                chat_id = chat_id.0,
                generation, "detached turn superseded, stopping follow-up stream"
            );
            session_mgr.clear_detached_pid(chat_id.0, generation).await;
            return;
        }

        let status_msg = match bot
            .send_message(chat_id, "<i>⏳ 后台任务已完成，正在生成后续回复...</i>")
            .parse_mode(ParseMode::Html)
            .await
        {
            Ok(m) => m,
            Err(err) => {
                error!(
                    chat_id = chat_id.0,
                    error = ?err,
                    "failed to send detached follow-up placeholder"
                );
                if turn_context_tokens.is_some() || turn_saw_checkpoint {
                    session_mgr
                        .record_detached_followup_result(
                            chat_id.0,
                            generation,
                            turn_context_tokens,
                            turn_saw_checkpoint,
                        )
                        .await;
                }
                break;
            }
        };

        let (msg_ids, last_rendered_html, accumulated_text, error_msg, outcome_closed) =
            if turn_already_done {
                (
                    vec![status_msg.id],
                    String::new(),
                    seed.accumulated_text,
                    seed.error_msg,
                    channel_closed,
                )
            } else {
                let outcome = Box::pin(stream_and_render_turn(
                    &bot,
                    chat_id,
                    stream_rx,
                    StreamRenderOptions {
                        initial_placeholder: PlaceholderState::Ready(status_msg.id),
                        placeholder_html: "<i>⏳ 后台任务已完成，正在生成后续回复...</i>",
                        draft_interval_ms: config.draft_update_interval_ms,
                        show_typing: false,
                        overwrite_on_result: false,
                        break_on_turn_complete: true,
                        session_guard: Some((&session_mgr, generation)),
                        turn_cancel: &turn_cancel,
                    },
                    seed,
                ))
                .await;
                stream_rx = outcome.stream_rx;
                if outcome.saw_checkpoint {
                    turn_saw_checkpoint = true;
                    any_followup_checkpoint = true;
                }
                if outcome.result_context_tokens.is_some() {
                    turn_context_tokens = outcome.result_context_tokens;
                }
                if let Some(ref status) = outcome.result_status
                    && !status.eq_ignore_ascii_case("SUCCESS")
                {
                    warn!(
                        chat_id = chat_id.0,
                        generation,
                        status = %status,
                        "detached telegram follow-up finished with non-success status"
                    );
                }
                (
                    outcome.msg_ids,
                    outcome.last_rendered_html,
                    outcome.accumulated_text,
                    outcome.error_msg,
                    outcome.channel_closed,
                )
            };

        if turn_context_tokens.is_some() || turn_saw_checkpoint {
            session_mgr
                .record_detached_followup_result(
                    chat_id.0,
                    generation,
                    turn_context_tokens,
                    turn_saw_checkpoint,
                )
                .await;
        }

        if !session_mgr
            .is_generation_current(chat_id.0, generation)
            .await
            || turn_cancel.is_cancelled()
        {
            session_mgr.clear_detached_pid(chat_id.0, generation).await;
            return;
        }

        let output_text = accumulated_text.trim();
        if output_text.is_empty() {
            if let Some(ref err) = error_msg {
                let err_html = format!("❌ <b>Error:</b> <code>{}</code>", text::escape_html(err));
                let err_chunks =
                    TelegramFormatter::split_html_chunks(&err_html, TELEGRAM_TEXT_LIMIT);
                deliver_final_chunks(
                    &bot,
                    chat_id,
                    &msg_ids,
                    &last_rendered_html,
                    &err_chunks,
                    &turn_cancel,
                )
                .await;
            } else {
                let done_chunk = ["✅ <i>后台任务已完成</i>".to_string()];
                deliver_final_chunks(
                    &bot,
                    chat_id,
                    &msg_ids,
                    &last_rendered_html,
                    &done_chunk,
                    &turn_cancel,
                )
                .await;
            }
        } else {
            let (widget_cleaned, ai_widgets) = widget::extract_ai_widgets_with_roots(
                output_text,
                &artifact_roots,
                allow_prose_widget_files,
            )
            .await;
            let active_brain_dir =
                artifacts::resolve_conversation_brain_dir_async(conversation_id.as_deref()).await;
            let (cleaned_output, explicit_artifacts) =
                artifacts::collect_explicit_deliverables_with_brain_async(
                    &user_prompt,
                    &widget_cleaned,
                    &artifact_roots,
                    active_brain_dir.as_deref(),
                )
                .await;
            let cleaned_output = cleaned_output.trim_end();
            let formatted_html = TelegramFormatter::to_telegram_html(cleaned_output);
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, TELEGRAM_TEXT_LIMIT);
            if chunks.is_empty() {
                let done_text = if !ai_widgets.is_empty() {
                    "🎴 <b>已生成交互组件</b>"
                } else if !explicit_artifacts.is_empty() {
                    "📎 <b>已发送请求的文件</b>"
                } else {
                    "✅ <i>任务已完成</i>"
                };
                let done_chunk = [done_text.to_string()];
                deliver_final_chunks(
                    &bot,
                    chat_id,
                    &msg_ids,
                    &last_rendered_html,
                    &done_chunk,
                    &turn_cancel,
                )
                .await;
            } else {
                deliver_final_chunks(
                    &bot,
                    chat_id,
                    &msg_ids,
                    &last_rendered_html,
                    &chunks,
                    &turn_cancel,
                )
                .await;
            }
            for w in &ai_widgets {
                if turn_cancel.is_cancelled() {
                    break;
                }
                widget::deliver_widget_with_fallback(&bot, &config.bot_token, chat_id, w).await;
            }
            media::deliver_resolved_artifacts(&bot, chat_id, &explicit_artifacts).await;
        }

        if outcome_closed {
            break;
        }
    }

    session_mgr.clear_detached_pid(chat_id.0, generation).await;
    if !turn_cancel.is_cancelled()
        && session_mgr
            .is_generation_current(chat_id.0, generation)
            .await
    {
        let post_session = session_mgr.get_or_create(chat_id.0).await;
        crate::compaction::maybe_schedule_background_compact(
            session_mgr,
            config,
            chat_id.0,
            &post_session,
            is_compact_turn,
            false,
            any_followup_checkpoint,
        );
    }
}

/// Replaces in-flight or completed top-level `tg-widget`, `tg-card`, `telegram-widget`, and
/// `telegram-card` fenced blocks during live Telegram draft streaming with a clean status line
/// instead of flashing raw HTML/CSS/JS or JSON.
#[must_use]
pub(crate) fn strip_widget_fences_for_streaming_draft(md: &str) -> Cow<'_, str> {
    let md_no_markers = artifacts::strip_send_file_markers_for_draft(md);
    if !widget::contains_widget_fence_marker(md_no_markers.as_ref()) {
        return md_no_markers;
    }
    match md_no_markers {
        Cow::Borrowed(s) => widget::strip_widget_fences_for_draft(s),
        Cow::Owned(s) => match widget::strip_widget_fences_for_draft(&s) {
            Cow::Borrowed(_) => Cow::Owned(s),
            Cow::Owned(stripped) => Cow::Owned(stripped),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_not_modified_is_recognised_as_no_op() {
        let not_mod = teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified);
        assert!(is_message_not_modified_err(&not_mod));

        let other = teloxide::RequestError::Api(teloxide::ApiError::ChatNotFound);
        assert!(!is_message_not_modified_err(&other));
    }

    #[test]
    fn voice_prefix_only_matches_leading_command() {
        assert_eq!(parse_voice_prefix("/voice hello"), (true, "hello"));
        assert_eq!(parse_voice_prefix("  /voice@my_bot   你好"), (true, "你好"));
        assert_eq!(parse_voice_prefix("/voice"), (true, ""));
        assert_eq!(parse_voice_prefix("/voice@my_bot"), (true, ""));
        assert_eq!(
            parse_voice_prefix("Please fix the /voice handler bug"),
            (false, "Please fix the /voice handler bug")
        );
        assert_eq!(parse_voice_prefix("/voice_mode"), (false, "/voice_mode"));
    }

    #[test]
    fn rollover_reconciliation_handles_shorter_equal_and_longer_finals() {
        let msgs = vec![MessageId(10), MessageId(11), MessageId(12)];

        // Draft rolled over to 3 messages, final answer fits in 1 chunk: edit M10, delete M11 & M12!
        let shorter = plan_final_reconciliation(&msgs, 1);
        assert_eq!(shorter.edits, vec![(0, MessageId(10))]);
        assert_eq!(shorter.stale_to_delete, vec![MessageId(11), MessageId(12)]);
        assert!(shorter.sends.is_empty());

        // Draft rolled over to 3 messages, final answer fits in 2 chunks: edit M10 & M11, delete M12!
        let two = plan_final_reconciliation(&msgs, 2);
        assert_eq!(two.edits, vec![(0, MessageId(10)), (1, MessageId(11))]);
        assert_eq!(two.stale_to_delete, vec![MessageId(12)]);
        assert!(two.sends.is_empty());

        // Equal lengths: edit all 3, delete none, send none.
        let equal = plan_final_reconciliation(&msgs, 3);
        assert_eq!(equal.edits.len(), 3);
        assert!(equal.stale_to_delete.is_empty());
        assert!(equal.sends.is_empty());

        // Final answer longer than draft: edit 3, send indices 3..5.
        let longer = plan_final_reconciliation(&msgs, 5);
        assert_eq!(longer.edits.len(), 3);
        assert!(longer.stale_to_delete.is_empty());
        assert_eq!(longer.sends, 3..5);
    }

    #[test]
    fn plain_text_fallback_strips_html_tags_and_decodes_entities() {
        let html = "<i>(continued)</i>\n\n<b>Result:</b> <code>x &lt; 5 &amp;&amp; y &gt; 2</code>";
        let plain = html_chunk_to_plain_fallback(html);
        assert_eq!(plain, vec!["(continued)\n\nResult: x < 5 && y > 2"]);
        assert!(!plain[0].contains("<b>"));
        assert!(!plain[0].contains("<code>"));
        assert!(!plain[0].contains("&lt;"));
    }

    #[test]
    fn adaptive_edit_interval_scales_with_size_and_chunks() {
        let base = Duration::from_millis(800);
        assert_eq!(compute_next_edit_interval(base, 500, 1), base);
        assert_eq!(
            compute_next_edit_interval(base, 2500, 1),
            Duration::from_millis(1400)
        );
        assert_eq!(
            compute_next_edit_interval(base, 500, 2),
            Duration::from_millis(1400)
        );
        let slow = Duration::from_secs(3);
        assert_eq!(compute_next_edit_interval(slow, 3000, 2), slow);
    }

    #[test]
    fn streaming_draft_masks_partial_and_complete_widget_fences_only() {
        let plain = "Normal markdown without widgets";
        assert!(matches!(
            strip_widget_fences_for_streaming_draft(plain),
            Cow::Borrowed(_)
        ));

        for lang in ["tg-widget", "tg-card", "telegram-widget", "telegram-card"] {
            let streaming = format!(
                "给你写好了小游戏：\n```{lang}\n<!DOCTYPE html><html><style>body{{color:red}}"
            );
            let masked = strip_widget_fences_for_streaming_draft(&streaming);
            assert!(
                masked.contains("正在构建交互式卡片"),
                "expected {lang} fence to be masked"
            );
            assert!(
                !masked.contains("<!DOCTYPE html>"),
                "expected {lang} raw body to be hidden"
            );
        }

        let quoted_tutorial = "```markdown\n```tg-widget\nexample\n```\n```";
        let kept = strip_widget_fences_for_streaming_draft(quoted_tutorial);
        assert!(kept.contains("example"));
    }

    #[test]
    fn split_plain_chunks_uses_utf16_units_for_cjk_and_emoji() {
        let cjk_3500 = "汉".repeat(3500);
        assert_eq!(cjk_3500.len(), 10_500);
        let single = split_plain_chunks(&cjk_3500, TELEGRAM_TEXT_LIMIT);
        assert_eq!(
            single.len(),
            1,
            "3500 CJK chars fit in 1 Telegram 4000-UTF16 chunk"
        );

        let cjk_4500 = "汉".repeat(4500);
        let two = split_plain_chunks(&cjk_4500, TELEGRAM_TEXT_LIMIT);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].encode_utf16().count(), 4000);
        assert_eq!(two[1].encode_utf16().count(), 500);

        let emojis_2500 = "😀".repeat(2500); // 5000 UTF-16 code units
        let emoji_chunks = split_plain_chunks(&emojis_2500, TELEGRAM_TEXT_LIMIT);
        assert_eq!(emoji_chunks.len(), 2);
        assert_eq!(emoji_chunks[0].encode_utf16().count(), 4000);
        assert_eq!(emoji_chunks[1].encode_utf16().count(), 1000);
    }

    #[test]
    fn telegram_system_prompt_injects_widget_guide_only_on_demand() {
        let normal = build_telegram_system_prompt("Be concise.", false);
        assert!(normal.contains("Be concise."));
        assert!(normal.contains("[[send_file:"));
        assert!(normal.contains("file://"));
        assert!(!normal.contains("Telegram Interactive Widget"));

        let with_widget = build_telegram_system_prompt("Be concise.", true);
        assert!(with_widget.contains("Telegram Interactive Widget"));
        assert!(with_widget.contains("tg-card"));
    }

    #[test]
    fn extract_reply_context_formats_text_caption_sender_and_media() {
        let no_reply: Message = serde_json::from_str(
            r#"{
                "message_id": 1,
                "date": 1700000000,
                "chat": { "id": 100, "type": "private" },
                "text": "hi"
            }"#,
        )
        .unwrap();
        assert!(extract_reply_context(&no_reply).is_none());

        let with_reply: Message = serde_json::from_str(
            r#"{
                "message_id": 2,
                "date": 1700000001,
                "chat": { "id": 100, "type": "private" },
                "text": "解释一下这个",
                "reply_to_message": {
                    "message_id": 1,
                    "date": 1700000000,
                    "from": { "id": 42, "is_bot": false, "first_name": "Alice", "last_name": "Chen" },
                    "chat": { "id": 100, "type": "private" },
                    "document": {
                        "file_id": "doc-1",
                        "file_unique_id": "u-1",
                        "file_name": "report.pdf",
                        "file_size": 1024
                    },
                    "caption": "Q3 季度财务摘要"
                }
            }"#,
        )
        .unwrap();
        let ctx = extract_reply_context(&with_reply).expect("reply context");
        assert!(ctx.contains("引用消息上下文"));
        assert!(ctx.contains("Alice Chen"));
        assert!(ctx.contains("[附件: Document report.pdf]"));
        assert!(ctx.contains("Q3 季度财务摘要"));

        let resolved_ctx = extract_reply_context_with_media(
            &with_reply,
            Some(("Document", Path::new("/ws/scratch/m1-report.pdf"))),
        )
        .expect("resolved media reply context");
        assert!(resolved_ctx.contains("[附件: Document: /ws/scratch/m1-report.pdf]"));
    }

    #[test]
    fn extract_reply_context_caps_bot_self_reply_while_preserving_paths() {
        let long_prose = format!(
            "{} 已生成报告 `/home/azureuser/ws/scratch/report.pdf` 请查收。",
            "这是一段很长的机器人回复内容。".repeat(25)
        );
        let msg: Message = serde_json::from_value(serde_json::json!({
            "message_id": 10,
            "date": 1700000010,
            "chat": { "id": 100, "type": "private" },
            "text": "帮我改一下表头",
            "reply_to_message": {
                "message_id": 9,
                "date": 1700000009,
                "from": { "id": 999, "is_bot": true, "first_name": "AgyBot" },
                "chat": { "id": 100, "type": "private" },
                "text": long_prose
            }
        }))
        .unwrap();

        let ctx = extract_reply_context_with_media(
            &msg,
            Some((
                "Image",
                Path::new("/home/azureuser/ws/scratch/m9-photo.jpg"),
            )),
        )
        .expect("bot reply context");
        assert!(ctx.contains("发送者: Bot"));
        assert!(ctx.contains("[附件: Image: /home/azureuser/ws/scratch/m9-photo.jpg]"));
        assert!(ctx.contains("/home/azureuser/ws/scratch/report.pdf"));
        assert!(
            ctx.chars().count() < 400,
            "bot self-reply context should stay compact: {} chars",
            ctx.chars().count()
        );
    }

    #[test]
    fn draft_renderer_displays_checkpoint_subagent_and_tool_error() {
        let state = StreamBuffers {
            checkpoint_noted: true,
            last_subagent_update: "worker-1 scanning <src>".to_string(),
            last_tool_error: Some((
                "run_command".to_string(),
                "exit status 1 & stderr".to_string(),
            )),
            ..Default::default()
        };
        let mut cache = DraftSectionCache::new(false, false, false);
        let mut draft = String::new();
        cache.render_draft(&state, "<i>思考中...</i>", &mut draft);
        assert!(draft.contains("🗜️ <i>已自动压缩上下文</i>"));
        assert!(draft.contains("worker-1 scanning &lt;src&gt;"));
        assert!(draft.contains("⚠️ <b>工具异常</b> (<code>run_command</code>)"));
        assert!(draft.contains("exit status 1 &amp; stderr"));
    }

    #[test]
    fn telegram_turn_system_prompt_deduplicates_base_and_widget_per_epoch() {
        let mut session =
            ChatSession::new("/tmp".into(), String::new(), "auto".into(), "high".into());
        session.system_prompt = "Be concise.".to_string();

        let (p1, h1, w1) = resolve_telegram_turn_system_prompt(&session, false, false);
        assert!(p1.contains("Be concise."));
        assert!(p1.contains("[[send_file:"));
        assert!(!w1);
        let h1 = h1.expect("first turn records base prompt hash");

        session.is_first_turn = false;
        session.last_system_prompt_hash = Some(h1);
        let (p2, h2, w2) = resolve_telegram_turn_system_prompt(&session, false, false);
        assert!(p2.is_empty());
        assert_eq!(h2, None);
        assert!(!w2);

        // First widget turn in this epoch injects ONLY the widget guide.
        let (p_widget, h_widget, w_widget) =
            resolve_telegram_turn_system_prompt(&session, true, false);
        assert_eq!(p_widget, widget::WIDGET_PROMPT_GUIDE);
        assert_eq!(h_widget, None);
        assert!(w_widget);

        // Subsequent widget turn in the same epoch skips repeating the 1.5 KB guide.
        session.last_widget_guide_injected = true;
        let (p_widget_dup, h_widget_dup, w_widget_dup) =
            resolve_telegram_turn_system_prompt(&session, true, false);
        assert!(p_widget_dup.is_empty());
        assert_eq!(h_widget_dup, None);
        assert!(!w_widget_dup);

        // ACP backend still injects the widget guide when requested.
        let (p_acp, h_acp, w_acp) = resolve_telegram_turn_system_prompt(&session, true, true);
        assert!(p_acp.contains("Telegram Interactive Widget"));
        assert_eq!(h_acp, Some(h1));
        assert!(w_acp);

        // Changing custom system prompt invalidates hash and re-injects base (without re-sending widget).
        session.system_prompt = "Answer in English.".to_string();
        let (p3, h3, w3) = resolve_telegram_turn_system_prompt(&session, false, false);
        assert!(p3.contains("Answer in English."));
        assert_ne!(h3, Some(h1));
        assert!(!w3);
    }
}
