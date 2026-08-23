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
    ])
}

pub fn get_effort_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Low", "set_effort:low"),
        InlineKeyboardButton::callback("Medium", "set_effort:medium"),
        InlineKeyboardButton::callback("High", "set_effort:high"),
    ]])
}

pub fn get_mode_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("Accept Edits (自动编辑)", "set_mode:accept-edits"),
        InlineKeyboardButton::callback("Plan (仅规划)", "set_mode:plan"),
    ]])
}

pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    if let (Some(data), Some(msg)) = (q.data, q.message) {
        let chat_id = msg.chat().id.0;
        let _ = bot.answer_callback_query(q.id).await;

        if let Some(model_arg) = data.strip_prefix("set_model:") {
            let target_model = normalize_model_name(model_arg);
            session_mgr.set_model(chat_id, target_model.to_string()).await;
            let display_name = if target_model.is_empty() { "系统默认 (Gemini 3.7 Flash)" } else { &target_model };
            let _ = bot
                .send_message(
                    msg.chat().id,
                    format!(
                        "🤖 <b>AI 模型已成功切换为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(display_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
        } else if let Some(effort) = data.strip_prefix("set_effort:") {
            session_mgr.set_effort(chat_id, effort.to_string()).await;
            let _ = bot
                .send_message(
                    msg.chat().id,
                    format!(
                        "⚡ <b>思考力度已成功设置为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(effort)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
        } else if let Some(mode) = data.strip_prefix("set_mode:") {
            session_mgr.set_mode(chat_id, mode.to_string()).await;
            let _ = bot
                .send_message(
                    msg.chat().id,
                    format!(
                        "🎯 <b>执行模式已成功切换为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(mode)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await;
        } else if data == "cmd:new" {
            session_mgr.reset_conversation(chat_id).await;
            let _ = bot
                .send_message(msg.chat().id, "🔄 <b>会话已重置！</b>")
                .parse_mode(ParseMode::Html)
                .await;
        } else if data == "cmd:toggle_voice" {
            let is_on = session_mgr.toggle_voice_mode(chat_id).await;
            let status_str = if is_on { "✅ 申鹤语音模式已开启" } else { "🔇 申鹤语音模式已关闭" };
            let _ = bot
                .send_message(msg.chat().id, format!("<b>{}</b>", status_str))
                .parse_mode(ParseMode::Html)
                .await;
        } else if data == "menu:model" {
            let keyboard = get_model_keyboard();
            let _ = bot
                .send_message(msg.chat().id, "🤖 请选择 AI 模型：")
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:effort" {
            let keyboard = get_effort_keyboard();
            let _ = bot
                .send_message(msg.chat().id, "⚡ 请选择思考力度 (Effort)：")
                .reply_markup(keyboard)
                .await;
        } else if data == "menu:mode" {
            let keyboard = get_mode_keyboard();
            let _ = bot
                .send_message(msg.chat().id, "🎯 请选择执行模式：")
                .reply_markup(keyboard)
                .await;
        }
    }
    Ok(())
}
