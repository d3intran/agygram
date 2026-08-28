use crate::bot::formatter::TelegramFormatter;
use crate::session::SessionManager;
use std::borrow::Cow;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

pub fn normalize_model_name(input: &str) -> Cow<'static, str> {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "3.7" | "flash-3.7" | "3.7-flash" | "gemini-3.7" | "3.7-high" => Cow::Borrowed("gemini-3.7-flash-high"),
        "3.7-med" | "3.7-medium" | "gemini-3.7-flash-medium" => Cow::Borrowed("gemini-3.7-flash-medium"),
        "3.7-low" | "gemini-3.7-flash-low" => Cow::Borrowed("gemini-3.7-flash-low"),
        "3.6" | "flash" | "3.6-flash" | "gemini-3.6" | "3.6-high" => Cow::Borrowed("gemini-3.6-flash-high"),
        "3.6-med" | "3.6-medium" | "gemini-3.6-flash-medium" => Cow::Borrowed("gemini-3.6-flash-medium"),
        "3.6-low" | "gemini-3.6-flash-low" => Cow::Borrowed("gemini-3.6-flash-low"),
        "3.5" | "3.5-flash" | "gemini-3.5" => Cow::Borrowed("gemini-3.5-flash-high"),
        "pro" | "3.1" | "3.1-pro" | "gemini-3.1" => Cow::Borrowed("gemini-3.1-pro-high"),
        "sonnet" | "claude" | "sonnet-4.6" | "claude-sonnet" => Cow::Borrowed("claude-sonnet-4-6"),
        "opus" | "opus-4.6" | "claude-opus" => Cow::Borrowed("claude-opus-4-6-thinking"),
        "oss" | "gpt" | "gpt-oss" | "120b" => Cow::Borrowed("gpt-oss-120b-medium"),
        "default" | "reset" | "system" => Cow::Borrowed(""),
        _ => Cow::Owned(trimmed.to_string()),
    }
}

pub fn get_model_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✨ Gemini 3.7 Flash (High)", "set_model:gemini-3.7-flash-high"),
            InlineKeyboardButton::callback("⚡ Gemini 3.7 Flash (Med)", "set_model:gemini-3.7-flash-medium"),
        ],
        vec![
            InlineKeyboardButton::callback("⚡ Gemini 3.6 Flash", "set_model:gemini-3.6-flash-high"),
            InlineKeyboardButton::callback("🧠 Gemini 3.1 Pro", "set_model:gemini-3.1-pro-high"),
        ],
        vec![
            InlineKeyboardButton::callback("🎭 Claude Sonnet 4.6", "set_model:claude-sonnet-4-6"),
            InlineKeyboardButton::callback("👑 Claude Opus 4.6", "set_model:claude-opus-4-6-thinking"),
        ],
        vec![
            InlineKeyboardButton::callback("🌐 GPT-OSS 120B", "set_model:gpt-oss-120b-medium"),
            InlineKeyboardButton::callback("⚙️ 系统默认", "set_model:default"),
        ],
        vec![
            InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
        ],
    ])
}

pub fn get_effort_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Low (极速)", "set_effort:low"),
            InlineKeyboardButton::callback("Medium (均衡)", "set_effort:medium"),
            InlineKeyboardButton::callback("High (深度思考)", "set_effort:high"),
        ],
        vec![
            InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
        ],
    ])
}

pub fn get_mode_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Accept Edits (自动编辑)", "set_mode:accept-edits"),
            InlineKeyboardButton::callback("Plan (仅规划)", "set_mode:plan"),
        ],
        vec![
            InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
        ],
    ])
}

pub fn build_status_card(chat_id: i64, session: &crate::session::ChatSession) -> (String, InlineKeyboardMarkup) {
    let conv_status = session
        .conversation_id
        .as_deref()
        .unwrap_or("新建会话 (尚未生成 ID)");
    let current_model = if session.model.is_empty() {
        "系统默认 (Gemini 3.7 Flash)".to_string()
    } else {
        session.model.clone()
    };
    let current_agent = if session.agent.is_empty() {
        "默认 Agent".to_string()
    } else {
        session.agent.clone()
    };

    let voice_status_str = if session.voice_mode {
        "✅ 开启 (全量语音回复)"
    } else {
        "❌ 关闭 (支持 /voice 单次触发)"
    };

    let status_text = format!(
        "📊 <b>当前会话配置状态</b>\n\n\
        • <b>Chat ID</b>: <code>{}</code>\n\
        • <b>主工作目录</b>: <code>{}</code>\n\
        • <b>AI 模型</b>: <code>{}</code>\n\
        • <b>思考力度 (Effort)</b>: <code>{}</code>\n\
        • <b>执行模式</b>: <code>{}</code>\n\
        • <b>当前 Agent</b>: <code>{}</code>\n\
        • <b>语音回复模式</b>: <code>{}</code>\n\
        • <b>当前会话 ID</b>: <code>{}</code>\n\
        • <b>运行状态</b>: {}",
        chat_id,
        TelegramFormatter::escape_html(&session.workspace),
        TelegramFormatter::escape_html(&current_model),
        TelegramFormatter::escape_html(&session.effort),
        TelegramFormatter::escape_html(&session.mode),
        TelegramFormatter::escape_html(&current_agent),
        voice_status_str,
        TelegramFormatter::escape_html(conv_status),
        if session.is_running {
            "⏳ 正在运行中"
        } else {
            "✅ 空闲"
        }
    );

    let voice_btn_text = if session.voice_mode {
        "🔇 关闭语音"
    } else {
        "🔊 开启语音"
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🤖 切换模型", "menu:model"),
            InlineKeyboardButton::callback("⚡ 思考力度", "menu:effort"),
        ],
        vec![
            InlineKeyboardButton::callback("🎯 执行模式", "menu:mode"),
            InlineKeyboardButton::callback(voice_btn_text, "cmd:toggle_voice"),
        ],
        vec![
            InlineKeyboardButton::callback("📜 历史会话", "menu:history"),
            InlineKeyboardButton::callback("🔄 重置会话", "cmd:new"),
        ],
    ]);

    (status_text, keyboard)
}

pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat().id.0;
        let msg_id = msg.id();
        let _ = bot.answer_callback_query(q.id).await;

        if let Some(model_arg) = data.strip_prefix("set_model:") {
            let target_model = normalize_model_name(model_arg);
            session_mgr.set_model(chat_id, target_model.to_string()).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if let Some(effort) = data.strip_prefix("set_effort:") {
            session_mgr.set_effort(chat_id, effort.to_string()).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if let Some(mode) = data.strip_prefix("set_mode:") {
            session_mgr.set_mode(chat_id, mode.to_string()).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if let Some(resume_id) = data.strip_prefix("resume:") {
            session_mgr.resume_conversation(chat_id, Some(resume_id.to_string())).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, format!("✅ <b>已成功恢复并接续会话:</b> <code>{}</code>\n\n{}", resume_id, status_text))
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "cmd:new" {
            session_mgr.reset_conversation(chat_id).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, format!("🔄 <b>会话已重置！</b>\n\n{}", status_text))
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "cmd:toggle_voice" {
            let _ = session_mgr.toggle_voice_mode(chat_id).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:status" {
            let session = session_mgr.get_or_create(chat_id).await;
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:model" {
            let keyboard = get_model_keyboard();
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, "🤖 <b>请选择需要切换的 AI 模型：</b>")
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:effort" {
            let keyboard = get_effort_keyboard();
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, "⚡ <b>请选择思考力度 (Effort)：</b>")
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:mode" {
            let keyboard = get_mode_keyboard();
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, "🎯 <b>请选择执行模式：</b>")
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:history" {
            let recents = crate::executor::AgyExecutor::list_recent_conversations(6).await;
            if recents.is_empty() {
                let back_kb = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
                ]]);
                let _ = bot
                    .edit_message_text(msg.chat().id, msg_id, "📜 <b>暂未发现历史会话。</b>")
                    .parse_mode(ParseMode::Html)
                    .reply_markup(back_kb)
                    .await;
            } else {
                let mut rows = Vec::new();
                let mut text = String::from("📜 <b>VPS 最近活跃历史会话 (点击即可接续)：</b>\n\n");
                for (idx, r) in recents.iter().enumerate() {
                    let flags = match (r.has_plan, r.has_walkthrough) {
                        (true, true) => " [Plan+WT]",
                        (true, false) => " [Plan]",
                        (false, true) => " [WT]",
                        (false, false) => "",
                    };
                    text.push_str(&format!("{}. <b>{}</b>{}\n   <code>{}</code> · {}\n\n", idx + 1, TelegramFormatter::escape_html(&r.title), flags, &r.id[..8.min(r.id.len())], r.updated_time));
                    rows.push(vec![InlineKeyboardButton::callback(
                        format!("{}. {}", idx + 1, &r.title[..20.min(r.title.len())]),
                        format!("resume:{}", r.id),
                    )]);
                }
                rows.push(vec![InlineKeyboardButton::callback("« 返回控制板", "menu:status")]);
                let _ = bot
                    .edit_message_text(msg.chat().id, msg_id, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(InlineKeyboardMarkup::new(rows))
                    .await;
            }
        }
    }
    Ok(())
}
