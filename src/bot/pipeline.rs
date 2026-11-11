use crate::bot::formatter::TelegramFormatter;
use crate::bot::media;
use crate::config::Config;
use crate::executor::{AgyExecutor, ExecutionParams, StreamEvent};
use crate::session::{ChatSession, SessionManager, TurnGuard};
use crate::text;
use rustc_hash::FxHashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{ChatId, InlineKeyboardMarkup, MessageId, ParseMode};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const TELEGRAM_TEXT_LIMIT: usize = 4000;
const INTERRUPT_GRACE_SECS: u64 = 6;
const SHUTDOWN_WAIT_SECS: u64 = 3;

#[inline]
pub(crate) fn is_message_not_modified_err(err: &teloxide::RequestError) -> bool {
    matches!(
        err,
        teloxide::RequestError::Api(teloxide::ApiError::MessageNotModified)
    )
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
    dispatch_message_with_session(bot, msg, config, session_mgr, runtime, None).await
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
    let has_media = media::has_supported_media(&msg);
    let (voice_in_text, text_body) = msg.text().map(parse_voice_prefix).unwrap_or((false, ""));
    let (voice_in_caption, caption_body) =
        msg.caption().map(parse_voice_prefix).unwrap_or((false, ""));

    if !has_media && text_body.trim().is_empty() && caption_body.trim().is_empty() {
        return Ok(());
    }

    let mut was_interrupted = false;
    let generation = match session_mgr.begin_turn(chat_id).await {
        Some(gen) => gen,
        None => {
            if !config.interrupt_on_new_message {
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
            }

            info!(chat_id, "interrupting running turn due to new message");
            runtime.cancel_chat_turn(chat_id).await;
            session_mgr.cancel_task(chat_id).await;
            was_interrupted = true;

            match session_mgr.begin_turn(chat_id).await {
                Some(gen) => gen,
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
    };

    let session = match preloaded_session {
        Some(s) => s,
        None => session_mgr.get_or_create(chat_id).await,
    };
    let should_send_voice = session.voice_mode || voice_in_text || voice_in_caption;
    let text_part = if !text_body.trim().is_empty() {
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
    ) {
        if prev.generation != generation {
            prev.cancel.cancel();
        }
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
    was_interrupted: bool,
    turn_cancel: CancellationToken,
    daemon_cancel: CancellationToken,
    turn_guard: TurnGuard,
}

#[tracing::instrument(skip_all, fields(chat_id = ctx.msg.chat.id.0, generation = ctx.generation))]
async fn run_chat_turn(mut ctx: TurnTaskContext) -> ResponseResult<()> {
    let target_chat = ctx.msg.chat.id;
    let chat_id = target_chat.0;

    if ctx.was_interrupted {
        if let Err(err) = ctx
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
    let attachment_fut = media::download_media_attachment(&ctx.bot, &ctx.msg, &scratch_dir);
    let attachment_opt = tokio::select! {
        biased;
        _ = ctx.turn_cancel.cancelled() => return Ok(()),
        res = attachment_fut => res,
    };
    if let Some(attachment_str) = attachment_opt {
        prompt_content.push_str(&attachment_str);
    }
    if !ctx.text_part.is_empty() {
        prompt_content.push_str(&ctx.text_part);
    }

    let input_prompt = prompt_content.trim().to_string();
    if input_prompt.is_empty() || ctx.turn_cancel.is_cancelled() {
        return Ok(());
    }

    if let Err(err) = ctx
        .bot
        .send_chat_action(target_chat, teloxide::types::ChatAction::Typing)
        .await
    {
        log_telegram_err(chat_id, "send_initial_typing", err);
    }

    let initial_status_msg = {
        let mut attempt = 1u8;
        loop {
            if ctx.turn_cancel.is_cancelled() {
                return Ok(());
            }
            match ctx
                .bot
                .send_message(target_chat, "<i>思考中...</i>")
                .parse_mode(ParseMode::Html)
                .await
            {
                Ok(m) => break m,
                Err(teloxide::RequestError::RetryAfter(secs)) if attempt < 3 => {
                    attempt += 1;
                    let wait = Duration::from_secs(u64::from(secs.seconds()) + 1);
                    tokio::select! {
                        biased;
                        _ = ctx.turn_cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                Err(err) => {
                    warn!(chat_id, attempt, error = ?err, "initial status message send failed");
                    if attempt >= 3 {
                        return Err(err);
                    }
                    attempt += 1;
                    tokio::select! {
                        biased;
                        _ = ctx.turn_cancel.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(600)) => {}
                    }
                }
            }
        }
    };

    let (stream_tx, stream_rx) = mpsc::channel::<StreamEvent>(1000);
    let (pid_tx, pid_rx) = tokio::sync::oneshot::channel::<u32>();

    let params = ExecutionParams {
        prompt: &input_prompt,
        conversation_id: ctx.session.conversation_id.as_deref(),
        is_first_turn: ctx.session.is_first_turn,
        workspace: &valid_workspace,
        extra_dirs: &ctx.session.extra_dirs,
        model: &ctx.session.model,
        effort: &ctx.session.effort,
        mode: &ctx.session.mode,
        agent: &ctx.session.agent,
        system_prompt: &ctx.session.system_prompt,
        sandbox: ctx.session.sandbox,
        project: &ctx.session.project,
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

    let render_fut = stream_and_render_turn(
        &ctx.bot,
        target_chat,
        stream_rx,
        StreamRenderOptions {
            initial_msg_id: initial_status_msg.id,
            placeholder_html: "<i>思考中...</i>",
            draft_interval_ms: ctx.config.draft_update_interval_ms,
            show_typing: true,
            overwrite_on_result: true,
            break_on_turn_complete: false,
            session_guard: None,
            turn_cancel: &ctx.turn_cancel,
        },
        InitialStreamSeed::default(),
    );

    let (exec_opt, render_outcome) = tokio::join!(exec_with_cancel, render_fut);
    pid_task.abort();

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
            let output_text = res.output;
            let usage_ref = res.usage.as_ref();
            let dur_val = res.duration_seconds;

            let owns_turn = ctx
                .session_mgr
                .finish_turn(
                    chat_id,
                    ctx.generation,
                    crate::session::TurnOutcome {
                        conversation_id: res.conversation_id,
                        input_chars: input_prompt.len(),
                        output_chars: output_text.len(),
                        usage: usage_ref,
                        duration_seconds: dur_val,
                    },
                )
                .await;
            ctx.turn_guard.disarm();

            if !owns_turn || ctx.turn_cancel.is_cancelled() {
                info!(
                    chat_id,
                    generation = ctx.generation,
                    "turn was superseded, discarding output"
                );
                return Ok(());
            }

            let mut artifact_roots: Vec<PathBuf> =
                vec![PathBuf::from(&valid_workspace), scratch_dir];
            artifact_roots.extend(ctx.session.extra_dirs.iter().map(PathBuf::from));
            media::auto_deliver_detected_artifacts(
                &ctx.bot,
                target_chat,
                &output_text,
                &artifact_roots,
            )
            .await;

            let formatted_html = TelegramFormatter::to_telegram_html(&output_text);
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, TELEGRAM_TEXT_LIMIT);

            // Spawn TTS without cloning `output_text`: HTML formatting above only borrowed it.
            if ctx.should_send_voice {
                if let Some(ref api_key) = ctx.config.fish_api_key {
                    let bot_voice = ctx.bot.clone();
                    let api_key_cloned = api_key.clone();
                    let ref_id = ctx
                        .session
                        .voice_reference_id
                        .clone()
                        .or_else(|| ctx.config.fish_reference_id.clone());
                    tokio::spawn(async move {
                        info!(chat_id = target_chat.0, "generating TTS voice reply");
                        if let Err(err) = bot_voice
                            .send_chat_action(target_chat, teloxide::types::ChatAction::RecordVoice)
                            .await
                        {
                            log_telegram_err(target_chat.0, "send_record_voice_action", err);
                        }
                        match crate::tts::generate_speech_bytes(
                            &output_text,
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
                                if let Err(err) =
                                    bot_voice.send_voice(target_chat, voice_file).await
                                {
                                    error!(chat_id = target_chat.0, error = ?err, "failed to send voice reply");
                                }
                            }
                            Err(err) => {
                                error!(chat_id = target_chat.0, error = %err, "TTS generation error");
                            }
                        }
                    });
                }
            }

            if chunks.is_empty() {
                info!(
                    chat_id,
                    "rendered output is empty, reconciling draft messages"
                );
                cleanup_stale_rollover_messages(
                    &ctx.bot,
                    target_chat,
                    &render_outcome.msg_ids,
                    1,
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

            if is_detached && !ctx.turn_cancel.is_cancelled() {
                stream_detached_followups(FollowupStreamParams {
                    bot: ctx.bot,
                    chat_id: target_chat,
                    stream_rx: render_outcome.stream_rx,
                    draft_interval_ms: ctx.config.draft_update_interval_ms,
                    artifact_roots,
                    session_mgr: ctx.session_mgr,
                    generation: ctx.generation,
                    turn_cancel: ctx.turn_cancel,
                })
                .await;
            }
        }
        Err(err) => {
            error!(chat_id, error = ?err, "agy execution error");
            let error_html = format!(
                "❌ <b>执行失败:</b>\n<pre><code>{}</code></pre>",
                text::escape_html(&err.to_string())
            );
            let target_id = render_outcome
                .msg_ids
                .first()
                .copied()
                .unwrap_or(initial_status_msg.id);
            let err_chunks = TelegramFormatter::split_html_chunks(&error_html, TELEGRAM_TEXT_LIMIT);
            deliver_final_chunks(
                &ctx.bot,
                target_chat,
                &[target_id],
                "",
                &err_chunks,
                &ctx.turn_cancel,
            )
            .await;
            cleanup_stale_rollover_messages(
                &ctx.bot,
                target_chat,
                &render_outcome.msg_ids,
                1,
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
    error_msg: Option<String>,
    is_dirty: bool,
}

struct StreamRenderOptions<'a> {
    initial_msg_id: MessageId,
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

        if !state.last_tool_call.is_empty() {
            if self.tool_dirty {
                self.cached_tool_html = TelegramFormatter::format_tool_call(
                    &state.last_tool_call,
                    &state.last_tool_summary,
                );
                self.tool_dirty = false;
            }
            draft_buf.push_str(&self.cached_tool_html);
        }

        if !state.last_thinking.is_empty() {
            if self.thinking_dirty && !self.thinking_capped {
                self.cached_thinking_html =
                    TelegramFormatter::format_thinking(&state.last_thinking);
                self.thinking_dirty = false;
                if state.last_thinking.trim().chars().nth(1500).is_some() {
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
                self.cached_text_html =
                    TelegramFormatter::to_telegram_html(&state.accumulated_text);
                self.text_dirty = false;
            }
            if !self.cached_text_html.is_empty() {
                draft_buf.push_str(&self.cached_text_html);
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
    last_thinking: String,
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
    let mut state = StreamBuffers {
        accumulated_text: seed.accumulated_text,
        last_tool_call: seed.last_tool_call,
        last_tool_summary: seed.last_tool_summary,
        last_thinking: seed.last_thinking,
        error_msg: seed.error_msg,
    };
    let mut is_dirty = seed.is_dirty;
    let mut cache = DraftSectionCache::new(
        !state.last_tool_call.is_empty(),
        !state.last_thinking.is_empty(),
        !state.accumulated_text.is_empty(),
    );

    let mut msg_ids = vec![opts.initial_msg_id];
    let mut last_rendered_html = String::new();
    let mut draft_buf = String::with_capacity(2048);
    let mut channel_closed = false;

    let base_interval = Duration::from_millis(opts.draft_interval_ms.max(500));
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
                match maybe_ev {
                    Some(StreamEvent::Init { .. }) => {}
                    Some(StreamEvent::TextDelta(delta)) => {
                        state.accumulated_text.push_str(&delta);
                        cache.text_dirty = true;
                        is_dirty = true;
                    }
                    Some(StreamEvent::ThinkingDelta(thinking)) => {
                        if !cache.thinking_capped {
                            state.last_thinking.push_str(&thinking);
                            cache.thinking_dirty = true;
                        }
                        is_dirty = true;
                    }
                    Some(StreamEvent::ToolUse { tool, summary }) => {
                        if state.last_tool_call != tool || state.last_tool_summary != summary {
                            state.last_tool_call = tool;
                            state.last_tool_summary = summary;
                            cache.tool_dirty = true;
                        }
                        is_dirty = true;
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
                    Some(StreamEvent::Result { response, .. }) => {
                        if !response.is_empty()
                            && (opts.overwrite_on_result || state.accumulated_text.trim().is_empty())
                        {
                            state.accumulated_text = response;
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
                    if let Some(until) = backoff_until {
                        if until > now {
                            render_sleep.as_mut().reset(until);
                        }
                    }
                }
            }
            _ = &mut render_sleep, if is_dirty => {
                if let Some((mgr, gen)) = opts.session_guard {
                    if !mgr.is_generation_current(chat_id.0, gen).await {
                        break;
                    }
                }

                cache.render_draft(&state, opts.placeholder_html, &mut draft_buf);

                let balanced_draft = TelegramFormatter::balance_streaming_html(&draft_buf);
                let chunks = TelegramFormatter::split_html_chunks(&balanced_draft, TELEGRAM_TEXT_LIMIT);

                // Multi-message rollover: seal earlier chunks and allocate rollover message IDs.
                let mut rollover_retry_after = None;
                while chunks.len() > msg_ids.len() {
                    let sealed_idx = msg_ids.len() - 1;
                    let completed_chunk = &chunks[sealed_idx];
                    let active_id = msg_ids[sealed_idx];
                    if let Err(err) = bot
                        .edit_message_text(chat_id, active_id, completed_chunk)
                        .parse_mode(ParseMode::Html)
                        .await
                    {
                        log_telegram_err(chat_id.0, "rollover_seal_edit", err);
                    }

                    match bot
                        .send_message(chat_id, "<i>(续)...</i>")
                        .parse_mode(ParseMode::Html)
                        .await
                    {
                        Ok(new_msg) => {
                            msg_ids.push(new_msg.id);
                            last_rendered_html.clear();
                        }
                        Err(teloxide::RequestError::RetryAfter(secs)) => {
                            rollover_retry_after =
                                Some(Duration::from_secs(u64::from(secs.seconds()) + 1).max(base_interval));
                            break;
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

                let active_idx = msg_ids.len() - 1;
                let active_msg_id = msg_ids[active_idx];
                if let Some(active_chunk) = chunks.get(active_idx) {
                    if active_chunk != &last_rendered_html {
                        match bot
                            .edit_message_text(chat_id, active_msg_id, active_chunk)
                            .parse_mode(ParseMode::Html)
                            .await
                        {
                            Ok(_) => {
                                last_rendered_html.clone_from(active_chunk);
                                backoff_until = None;
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + base_interval);
                            }
                            Err(teloxide::RequestError::RetryAfter(retry_secs)) => {
                                let wait = Duration::from_secs(u64::from(retry_secs.seconds()) + 1)
                                    .max(base_interval);
                                let until = tokio::time::Instant::now() + wait;
                                backoff_until = Some(until);
                                is_dirty = true;
                                render_sleep.as_mut().reset(until);
                            }
                            Err(err) if is_message_not_modified_err(&err) => {
                                last_rendered_html.clone_from(active_chunk);
                                backoff_until = None;
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + base_interval);
                            }
                            Err(err) => {
                                warn!(chat_id = chat_id.0, error = ?err, "draft edit rejected");
                                backoff_until = None;
                                is_dirty = false;
                                render_sleep
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + base_interval);
                            }
                        }
                    } else {
                        is_dirty = false;
                        render_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + base_interval);
                    }
                } else {
                    is_dirty = false;
                    render_sleep
                        .as_mut()
                        .reset(tokio::time::Instant::now() + base_interval);
                }
            }
            _ = &mut typing_sleep, if opts.show_typing => {
                let action = if state.last_tool_call == "generate_image" {
                    teloxide::types::ChatAction::UploadPhoto
                } else if state.last_tool_call.contains("file")
                    || state.last_tool_call.contains("resource")
                {
                    teloxide::types::ChatAction::UploadDocument
                } else {
                    teloxide::types::ChatAction::Typing
                };
                if let Err(err) = bot.send_chat_action(chat_id, action).await {
                    log_telegram_err(chat_id.0, "send_chat_action", err);
                }
                typing_sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + Duration::from_secs(4));
            }
        }
    }

    StreamRenderOutcome {
        stream_rx,
        msg_ids,
        last_rendered_html,
        accumulated_text: state.accumulated_text,
        error_msg: state.error_msg,
        channel_closed,
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

/// Splits plain text safely on UTF-8 character boundaries without HTML tags.
#[must_use]
pub(crate) fn split_plain_chunks(plain: &str, max_bytes: usize) -> Vec<String> {
    if plain.len() <= max_bytes {
        return vec![plain.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < plain.len() {
        let mut end = (start + max_bytes).min(plain.len());
        while end > start && !plain.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = (start + 1..=plain.len())
                .find(|&i| plain.is_char_boundary(i))
                .unwrap_or(plain.len());
        }
        // Prefer breaking on a newline if one sits in the second half of the window.
        if end < plain.len() {
            if let Some(nl) = plain[start..end].rfind('\n') {
                if nl > (end - start) / 2 {
                    end = start + nl + 1;
                }
            }
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
    cancel: &CancellationToken,
) {
    for (idx, plain) in plain_chunks.iter().enumerate() {
        if cancel.is_cancelled() {
            return;
        }
        let mut retried = false;
        loop {
            let res = if idx == 0 {
                if let Some(mid) = edit_msg_id {
                    bot.edit_message_text(chat_id, mid, plain).await.map(|_| ())
                } else {
                    bot.send_message(chat_id, plain).await.map(|_| ())
                }
            } else {
                bot.send_message(chat_id, plain).await.map(|_| ())
            };
            match res {
                Ok(()) => break,
                Err(teloxide::RequestError::RetryAfter(secs)) if !retried => {
                    retried = true;
                    if !sleep_retry_after(secs.seconds(), cancel).await {
                        return;
                    }
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
    loop {
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
                error!(chat_id = chat_id.0, msg_id = msg_id.0, error = ?err, "final HTML edit rejected");
                break;
            }
        }
    }

    // Try sending as a fresh HTML message first; only strip HTML tags if HTML itself is rejected.
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

    let plain_chunks = html_chunk_to_plain_fallback(html_chunk);
    send_plain_chunks(bot, chat_id, Some(msg_id), &plain_chunks, cancel).await;
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
    send_plain_chunks(bot, chat_id, None, &plain_chunks, cancel).await;
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

/// Sends multi-chunk HTML replies for commands (splitting at 4000 chars, respecting `RetryAfter`,
/// and falling back to tag-stripped plain text if HTML is rejected).
pub(crate) async fn send_html_chunks(bot: &Bot, chat_id: ChatId, html: &str) {
    let chunks = TelegramFormatter::split_html_chunks(html, TELEGRAM_TEXT_LIMIT);
    let cancel = CancellationToken::new();
    for chunk in chunks {
        send_html_chunk_with_fallback(bot, chat_id, &chunk, None, &cancel).await;
    }
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
    chat_id: ChatId,
    stream_rx: mpsc::Receiver<StreamEvent>,
    draft_interval_ms: u64,
    artifact_roots: Vec<PathBuf>,
    session_mgr: Arc<SessionManager>,
    generation: u64,
    turn_cancel: CancellationToken,
}

async fn stream_detached_followups(params: FollowupStreamParams) {
    let FollowupStreamParams {
        bot,
        chat_id,
        mut stream_rx,
        draft_interval_ms,
        artifact_roots,
        session_mgr,
        generation,
        turn_cancel,
    } = params;
    loop {
        let mut seed = InitialStreamSeed {
            is_dirty: true,
            ..Default::default()
        };
        let mut turn_already_done = false;
        let mut channel_closed = false;

        // Wait for the first meaningful event of a follow-up turn.
        loop {
            let ev = tokio::select! {
                biased;
                _ = turn_cancel.cancelled() => return,
                ev = stream_rx.recv() => ev,
            };
            match ev {
                Some(StreamEvent::Init { .. }) => continue,
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
                Some(StreamEvent::TurnComplete { text }) => {
                    if text.trim().is_empty() {
                        continue;
                    }
                    seed.accumulated_text = text;
                    turn_already_done = true;
                    break;
                }
                Some(StreamEvent::Result { response, .. }) => {
                    if !response.trim().is_empty() {
                        seed.accumulated_text = response;
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
            && seed.error_msg.is_none()
        {
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
                let outcome = stream_and_render_turn(
                    &bot,
                    chat_id,
                    stream_rx,
                    StreamRenderOptions {
                        initial_msg_id: status_msg.id,
                        placeholder_html: "<i>⏳ 后台任务已完成，正在生成后续回复...</i>",
                        draft_interval_ms,
                        show_typing: false,
                        overwrite_on_result: false,
                        break_on_turn_complete: true,
                        session_guard: Some((&session_mgr, generation)),
                        turn_cancel: &turn_cancel,
                    },
                    seed,
                )
                .await;
                stream_rx = outcome.stream_rx;
                (
                    outcome.msg_ids,
                    outcome.last_rendered_html,
                    outcome.accumulated_text,
                    outcome.error_msg,
                    outcome.channel_closed,
                )
            };

        if !session_mgr
            .is_generation_current(chat_id.0, generation)
            .await
            || turn_cancel.is_cancelled()
        {
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
                cleanup_stale_rollover_messages(&bot, chat_id, &msg_ids, 1, &turn_cancel).await;
            }
        } else {
            media::auto_deliver_detected_artifacts(&bot, chat_id, output_text, &artifact_roots)
                .await;

            let formatted_html = TelegramFormatter::to_telegram_html(output_text);
            let chunks = TelegramFormatter::split_html_chunks(&formatted_html, TELEGRAM_TEXT_LIMIT);
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

        if outcome_closed {
            break;
        }
    }

    session_mgr.clear_detached_pid(chat_id.0, generation).await;
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
}
