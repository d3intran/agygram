use crate::bot::formatter::TelegramFormatter;
use crate::config::Config;
use crate::session::SessionManager;
use std::borrow::Cow;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

pub const SUPPORTED_MODELS: &[(&str, &str)] = &[
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("claude-opus-5-5-high", "Claude Opus 5.5 (High)"),
    ("claude-opus-5-5-medium", "Claude Opus 5.5 (Medium)"),
    ("claude-opus-5-5-low", "Claude Opus 5.5 (Low)"),
    ("claude-sonnet-5-5-high", "Claude Sonnet 5.5 (High)"),
    ("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 (Medium)"),
    ("claude-sonnet-5-5-low", "Claude Sonnet 5.5 (Low)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

pub fn is_valid_model(model: &str) -> bool {
    model.is_empty() || SUPPORTED_MODELS.iter().any(|(id, _)| *id == model)
}

pub fn normalize_model_name(input: &str) -> Cow<'static, str> {
    let trimmed = input.trim();
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        // Gemini 4 Argon
        "gemini-4-argon-high" | "argon-high" | "4-high" | "argon4-high" => {
            Cow::Borrowed("gemini-4-argon-high")
        }
        "gemini-4-argon-medium" | "gemini-4-argon" | "argon" | "gemini-4" | "argon-med" | "argon-medium" | "4-med" | "4-medium" => {
            Cow::Borrowed("gemini-4-argon-medium")
        }
        "gemini-4-argon-low" | "argon-low" | "4-low" | "argon4-low" => {
            Cow::Borrowed("gemini-4-argon-low")
        }
        // Gemini 3.8 Flash
        "gemini-3.8-flash-high" | "3.8" | "flash" | "3.8-flash" | "gemini-3.8" | "3.8-high" | "high" => {
            Cow::Borrowed("gemini-3.8-flash-high")
        }
        "gemini-3.8-flash-medium" | "3.8-med" | "3.8-medium" | "medium" | "med" => {
            Cow::Borrowed("gemini-3.8-flash-medium")
        }
        "gemini-3.8-flash-low" | "3.8-low" | "low" => {
            Cow::Borrowed("gemini-3.8-flash-low")
        }
        // Claude Opus 5.5 (with backward compatibility aliases)
        "claude-opus-5-5-high" | "opus" | "claude-opus" | "opus-5.5" | "opus-5" | "opus-high" | "claude-opus-4-6-thinking" => {
            Cow::Borrowed("claude-opus-5-5-high")
        }
        "claude-opus-5-5-medium" | "opus-med" | "opus-medium" => {
            Cow::Borrowed("claude-opus-5-5-medium")
        }
        "claude-opus-5-5-low" | "opus-low" => {
            Cow::Borrowed("claude-opus-5-5-low")
        }
        // Claude Sonnet 5.5 (with backward compatibility aliases)
        "claude-sonnet-5-5-high" | "sonnet-high" => {
            Cow::Borrowed("claude-sonnet-5-5-high")
        }
        "claude-sonnet-5-5-medium" | "sonnet" | "claude" | "claude-sonnet" | "sonnet-5.5" | "sonnet-5" | "sonnet-med" | "claude-sonnet-4-6" => {
            Cow::Borrowed("claude-sonnet-5-5-medium")
        }
        "claude-sonnet-5-5-low" | "sonnet-low" => {
            Cow::Borrowed("claude-sonnet-5-5-low")
        }
        // GPT-OSS 120B
        "gpt-oss-120b-medium" | "oss" | "gpt" | "gpt-oss" | "120b" => {
            Cow::Borrowed("gpt-oss-120b-medium")
        }
        // Default / System Reset
        "default" | "reset" | "system" | "" => {
            Cow::Borrowed("")
        }
        _ => Cow::Owned(trimmed.to_string()),
    }
}

pub fn get_model_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🚀 4 Argon (High)", "set_model:gemini-4-argon-high"),
            InlineKeyboardButton::callback("⚡ 4 Argon (Med)", "set_model:gemini-4-argon-medium"),
            InlineKeyboardButton::callback("🍃 4 Argon (Low)", "set_model:gemini-4-argon-low"),
        ],
        vec![
            InlineKeyboardButton::callback("✨ 3.8 Flash (High)", "set_model:gemini-3.8-flash-high"),
            InlineKeyboardButton::callback("⚡ 3.8 Flash (Med)", "set_model:gemini-3.8-flash-medium"),
            InlineKeyboardButton::callback("🍃 3.8 Flash (Low)", "set_model:gemini-3.8-flash-low"),
        ],
        vec![
            InlineKeyboardButton::callback("👑 Opus 5.5 (High)", "set_model:claude-opus-5-5-high"),
            InlineKeyboardButton::callback("👑 Opus 5.5 (Med)", "set_model:claude-opus-5-5-medium"),
        ],
        vec![
            InlineKeyboardButton::callback("🎭 Sonnet 5.5 (High)", "set_model:claude-sonnet-5-5-high"),
            InlineKeyboardButton::callback("🎭 Sonnet 5.5 (Med)", "set_model:claude-sonnet-5-5-medium"),
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
            InlineKeyboardButton::callback("X-High (超深度)", "set_effort:xhigh"),
            InlineKeyboardButton::callback("Max (极限思考)", "set_effort:max"),
        ],
        vec![
            InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
        ],
    ])
}

pub fn get_mode_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("Auto (全自主决策)", "set_mode:auto"),
            InlineKeyboardButton::callback("Accept Edits (自动编辑)", "set_mode:accept-edits"),
        ],
        vec![
            InlineKeyboardButton::callback("Plan (仅规划方案)", "set_mode:plan"),
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
        "系统默认 (Gemini 3.8 Flash)".to_string()
    } else {
        session.model.clone()
    };
    let current_agent = if session.agent.is_empty() {
        "默认 Agent".to_string()
    } else {
        session.agent.clone()
    };

    let voice_status_str = if session.voice_mode {
        "✅ 开启"
    } else {
        "❌ 关闭"
    };

    let sandbox_status_str = if session.sandbox {
        "🛡️ 开启 (Sandbox Restricted)"
    } else {
        "⚡ 关闭 (Full System Access)"
    };

    let project_str = if session.project.is_empty() {
        "默认工程".to_string()
    } else {
        session.project.clone()
    };

    let status_text = format!(
        "📊 <b>当前会话配置控制台</b>\n\n\
        • <b>Chat ID</b>: <code>{}</code>\n\
        • <b>工作目录</b>: <code>{}</code>\n\
        • <b>绑定工程</b>: <code>{}</code>\n\
        • <b>AI 模型</b>: <code>{}</code>\n\
        • <b>思考力度</b>: <code>{}</code>\n\
        • <b>执行模式</b>: <code>{}</code>\n\
        • <b>当前 Agent</b>: <code>{}</code>\n\
        • <b>沙盒隔离</b>: {}\n\
        • <b>语音回复</b>: {}\n\
        • <b>会话 ID</b>: <code>{}</code>\n\
        • <b>运行状态</b>: {}",
        chat_id,
        TelegramFormatter::escape_html(&session.workspace),
        TelegramFormatter::escape_html(&project_str),
        TelegramFormatter::escape_html(&current_model),
        TelegramFormatter::escape_html(&session.effort),
        TelegramFormatter::escape_html(&session.mode),
        TelegramFormatter::escape_html(&current_agent),
        sandbox_status_str,
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

    let sandbox_btn_text = if session.sandbox {
        "🔓 关闭沙盒"
    } else {
        "🛡️ 开启沙盒"
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🤖 切换模型", "menu:model"),
            InlineKeyboardButton::callback("⚡ 思考力度", "menu:effort"),
        ],
        vec![
            InlineKeyboardButton::callback("🎯 执行模式", "menu:mode"),
            InlineKeyboardButton::callback(sandbox_btn_text, "cmd:toggle_sandbox"),
        ],
        vec![
            InlineKeyboardButton::callback("📊 实时额度", "cmd:quota"),
            InlineKeyboardButton::callback("📜 历史会话", "menu:history"),
        ],
        vec![
            InlineKeyboardButton::callback(voice_btn_text, "cmd:toggle_voice"),
            InlineKeyboardButton::callback("❓ 帮助手册", "cmd:help_card"),
        ],
        vec![
            InlineKeyboardButton::callback("🔄 重置会话", "cmd:new"),
        ],
    ]);

    (status_text, keyboard)
}

pub fn build_onboarding_card() -> (String, InlineKeyboardMarkup) {
    let text = "🚀 <b>欢迎使用 agygram (Antigravity CLI Daemon for Telegram) v1.0</b>\n\n\
        这是专为 <b>Google Antigravity CLI (agy)</b> 打造的高性能 Telegram 随身指挥中心。\n\n\
        <b>💡 极速交互指南：</b>\n\
        • 直接发送<b>文本、需求、代码文件或图片</b>，即可触发实时打字机流式推导；\n\
        • 发送 <code>/status</code> 可随时唤起多级原地交互控制面板；\n\
        • 发送 <code>/history</code> 可一键查看并接续 VPS 历史活跃会话；\n\
        • 发送 <code>/plan</code> 与 <code>/walkthrough</code> 可直接调阅方案与总结报告。\n\n\
        <i>轻触下方按钮即可立即开始配置与探索：</i>".to_string();

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🤖 切换模型", "menu:model"),
            InlineKeyboardButton::callback("⚡ 思考力度", "menu:effort"),
        ],
        vec![
            InlineKeyboardButton::callback("📜 历史会话", "menu:history"),
            InlineKeyboardButton::callback("🎯 执行模式", "menu:mode"),
        ],
        vec![
            InlineKeyboardButton::callback("📊 完整控制板", "menu:status"),
            InlineKeyboardButton::callback("❓ 帮助指南", "cmd:help_card"),
        ],
    ]);

    (text, keyboard)
}

/// Callback data is client-supplied and lands straight on the `agy` command line, so reject
/// values that could smuggle in an extra flag (`--sandbox`, `-c`) or exceed message limits.
pub fn is_safe_cli_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+'))
}

pub fn is_supported_effort(effort: &str) -> bool {
    matches!(effort, "low" | "medium" | "high" | "xhigh" | "max")
}

pub fn is_supported_mode(mode: &str) -> bool {
    matches!(mode, "accept-edits" | "auto" | "plan")
}

/// Free-form values that still reach the command line must not start with a dash (the CLI
/// parser would read them as another flag) or embed control characters. Spaces stay legal
/// because project and agent names may contain them.
pub fn is_safe_free_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('-')
        && !value.chars().any(|c| c.is_control())
}

/// Reject callback payloads that are unknown, out of range, or could inject extra CLI flags.
/// Returns a human-readable reason when the payload must not be applied.
pub fn reject_reason(data: &str) -> Option<&'static str> {
    if let Some(arg) = data.strip_prefix("set_model:") {
        if !is_valid_model(&normalize_model_name(arg)) {
            return Some("⚠️ 该模型不在受支持的列表内。");
        }
    } else if let Some(arg) = data.strip_prefix("set_effort:") {
        if !is_supported_effort(arg) {
            return Some("⚠️ 无效的 thinking effort。");
        }
    } else if let Some(arg) = data.strip_prefix("set_mode:") {
        if !is_supported_mode(arg) {
            return Some("⚠️ 无效的执行模式。支持: auto, accept-edits, plan");
        }
    } else if let Some(arg) = data.strip_prefix("resume:") {
        if !is_safe_cli_value(arg) {
            return Some("⚠️ 无效的会话 ID。");
        }
    } else if !matches!(data, "menu:status" | "menu:model" | "menu:effort" | "menu:mode" | "menu:history" | "cmd:new" | "cmd:help_card" | "cmd:toggle_voice" | "cmd:toggle_sandbox") {
        return Some("⚠️ 未知的按钮操作。");
    }
    None
}

pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    // Inline buttons are visible to every member of a chat, so callback traffic must be
    // authorized on its own: pressing a button must not mutate another user's session.
    if !config.is_user_allowed(q.from.id.0) {
        let _ = bot
            .answer_callback_query(q.id)
            .text("🚫 未授权的操作。")
            .await;
        return Ok(());
    }

    if let (Some(data), Some(msg)) = (q.data, q.message) {
        if let Some(reason) = reject_reason(&data) {
            let _ = bot.answer_callback_query(q.id).text(reason).await;
            return Ok(());
        }

        let chat_id = msg.chat().id.0;
        let msg_id = msg.id();
        let _ = bot.answer_callback_query(q.id).await;

        if let Some(model_arg) = data.strip_prefix("set_model:") {
            let target_model = normalize_model_name(model_arg);
            if target_model.ends_with("-high") {
                session_mgr.set_effort(chat_id, "high".to_string()).await;
            } else if target_model.ends_with("-medium") || target_model.ends_with("-med") {
                session_mgr.set_effort(chat_id, "medium".to_string()).await;
            } else if target_model.ends_with("-low") {
                session_mgr.set_effort(chat_id, "low".to_string()).await;
            } else if target_model.ends_with("-xhigh") {
                session_mgr.set_effort(chat_id, "xhigh".to_string()).await;
            } else if target_model.ends_with("-max") {
                session_mgr.set_effort(chat_id, "max".to_string()).await;
            }
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
        } else if data == "cmd:toggle_sandbox" {
            let _ = session_mgr.toggle_sandbox(chat_id).await;
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
        } else if data == "cmd:help_card" {
            let ver = crate::executor::AgyExecutor::get_version().await;
            let help_text = format!(
                "📖 <b>agygram 核心指令速查 (v1.0 / agy v{})</b>\n\n\
                • /status : 交互式控制台\n\
                • /history : 查看历史会话并一键接续\n\
                • /plan & /walkthrough : 架构方案与执行报告\n\
                • /skills & /rules : 技能库与规则巡检\n\
                • /sandbox : 沙盒隔离开关\n\
                • /project <code>[&lt;name&gt;]</code> : 绑定或切换工程\n\
                • /mcp & /plugins : MCP 工具与插件列表\n\
                • /update : 一键升级 agy CLI\n\
                • /model, /effort, /mode, /voice : 快捷参数设置\n\
                • /cd, /adddir, /dirs, /file : 路径与文件直传",
                TelegramFormatter::escape_html(&ver)
            );
            let back_kb = InlineKeyboardMarkup::new(vec![vec![
                InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
            ]]);
            let _ = bot
                .edit_message_text(msg.chat().id, msg_id, help_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(back_kb)
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
                    text.push_str(&format!(
                        "{}. <b>{}</b>{}\n   <code>{}</code> · {}\n\n",
                        idx + 1,
                        TelegramFormatter::escape_html(&r.title),
                        flags,
                        TelegramFormatter::escape_html(TelegramFormatter::take_chars(&r.id, 8)),
                        r.updated_time
                    ));
                    rows.push(vec![InlineKeyboardButton::callback(
                        format!("{}. {}", idx + 1, TelegramFormatter::truncate_chars(&r.title, 20)),
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
        } else if data == "cmd:quota" {
            match crate::quota::QuotaFetcher::format_models_and_quota().await {
                Ok(quota_html) => {
                    let back_kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
                    ]]);
                    let _ = bot
                        .edit_message_text(msg.chat().id, msg_id, quota_html)
                        .parse_mode(ParseMode::Html)
                        .reply_markup(back_kb)
                        .await;
                }
                Err(err) => {
                    let back_kb = InlineKeyboardMarkup::new(vec![vec![
                        InlineKeyboardButton::callback("« 返回控制板", "menu:status"),
                    ]]);
                    let _ = bot
                        .edit_message_text(
                            msg.chat().id,
                            msg_id,
                            format!(
                                "⚠️ <b>实时 Quota 获取异常:</b> <code>{}</code>\n\n<i>💡 可在 VPS 终端执行 <code>agygram quota</code> 快速验证。</i>",
                                TelegramFormatter::escape_html(&err.to_string())
                            ),
                        )
                        .parse_mode(ParseMode::Html)
                        .reply_markup(back_kb)
                        .await;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_safe_cli_value, is_safe_free_value, is_supported_effort, is_supported_mode, reject_reason};

    #[test]
    fn cli_flag_smuggling_is_rejected() {
        for evil in ["--sandbox", "--dangerously-skip-permissions", "-c", "x\nrm -rf /", ""] {
            assert!(!is_safe_cli_value(evil), "cli check accepted: {evil:?}");
            assert!(!is_safe_free_value(evil), "free check accepted: {evil:?}");
        }
        assert!(!is_safe_cli_value(&"a".repeat(65)), "cli length cap ignored");
        assert!(is_safe_free_value(&"a".repeat(65)), "65 chars is a legal free-form name");
        assert!(!is_safe_free_value(&"a".repeat(129)), "free length cap ignored");
    }

    #[test]
    fn legitimate_values_pass() {
        assert!(is_safe_cli_value("accept-edits"));
        assert!(is_safe_cli_value("gemini-3.8-flash-high"));
        assert!(is_safe_cli_value("8de907d6-1c2b-4a5f-9e3d-77f0a1b2c3d4"));
        assert!(is_safe_free_value("my project name"));
    }

    #[test]
    fn callback_payloads_are_validated_before_mutation() {
        assert!(reject_reason("set_model:gemini-3.8-flash-high").is_none());
        assert!(reject_reason("set_model:gemini-4-argon-medium").is_none());
        assert!(reject_reason("set_model:claude-opus-5-5-high").is_none());
        assert!(reject_reason("set_model:default").is_none());
        assert!(reject_reason("set_model:gpt-4o").is_some());
        assert!(reject_reason("set_effort:max").is_none());
        assert!(reject_reason("set_effort:ultra").is_some());
        assert!(reject_reason("set_mode:auto").is_none());
        assert!(reject_reason("set_mode:accept-edits").is_none());
        assert!(reject_reason("set_mode:plan").is_none());
        assert!(reject_reason("set_mode:--sandbox").is_some());
        assert!(reject_reason("resume:8de907d6-1c2b").is_none());
        assert!(reject_reason("resume:--conversation evil").is_some());
        assert!(reject_reason("menu:status").is_none());
        assert!(reject_reason("cmd:new").is_none());
        assert!(reject_reason("cmd:rm_rf").is_some());
        assert!(reject_reason("").is_some());
    }

    #[test]
    fn mode_whitelist_matches_the_command_surface() {
        for m in ["auto", "accept-edits", "plan"] {
            assert!(is_supported_mode(m), "{m} should be supported");
        }
        assert!(!is_supported_mode("danger"));
        assert!(!is_supported_mode(""));
    }

    #[test]
    fn effort_whitelist_matches_the_command_surface() {
        for e in ["low", "medium", "high", "xhigh", "max"] {
            assert!(is_supported_effort(e), "{e} should be supported");
        }
        assert!(!is_supported_effort("off"));
        assert!(!is_supported_effort(""));
    }
}
