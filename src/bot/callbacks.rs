use crate::bot::pipeline::{edit_or_send_html_chunks, log_telegram_err};
use crate::config::Config;
use crate::executor::{AgyExecutor, RecentSessionInfo};
use crate::session::{ChatSession, SessionManager};
use crate::text;
use std::fmt::Write;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tokio::sync::OnceCell;
use tracing::warn;

use crate::models::{
    effort_from_model_suffix, is_safe_cli_value, is_supported_effort, is_supported_mode,
    is_valid_model, normalize_model_name,
};

static AGY_VERSION_CACHE: OnceCell<String> = OnceCell::const_new();

/// Returns the cached `agy --version` string so interactive `/help` and `cmd:help_card`
/// clicks do not spawn a subprocess on every invocation.
pub(crate) async fn get_cached_agy_version() -> &'static str {
    AGY_VERSION_CACHE
        .get_or_init(|| async { AgyExecutor::get_version().await })
        .await
        .as_str()
}

/// Applies `model` to the session and syncs `effort` when the model ID carries an explicit
/// effort suffix (such as `-xhigh`, `-high`, `-medium`, `-low`, `-max`).
pub(crate) async fn apply_model_and_effort(
    session_mgr: &SessionManager,
    chat_id: i64,
    target_model: &str,
) {
    if let Some(effort) = effort_from_model_suffix(target_model) {
        session_mgr.set_effort_enum(chat_id, effort).await;
    }
    session_mgr
        .set_model(chat_id, target_model.to_string())
        .await;
}

/// Shared inline history card builder used by `/history`, `/resume`, and `menu:history`.
#[must_use]
pub(crate) fn build_history_card(
    header: &str,
    recents: &[RecentSessionInfo],
    footer_buttons: Vec<InlineKeyboardButton>,
) -> (String, InlineKeyboardMarkup) {
    let mut rows = Vec::with_capacity(recents.len() + 1);
    let mut text_out = String::with_capacity(header.len() + recents.len() * 128);
    text_out.push_str(header);
    text_out.push_str("\n\n");

    for (idx, r) in recents.iter().enumerate() {
        let flags = match (r.has_plan, r.has_walkthrough) {
            (true, true) => " [Plan+WT]",
            (true, false) => " [Plan]",
            (false, true) => " [WT]",
            (false, false) => "",
        };
        let _ = write!(
            text_out,
            "{}. <b>{}</b>{}\n   <code>{}</code> · {}\n\n",
            idx + 1,
            text::escape_html(&r.title),
            flags,
            text::escape_html(text::take_chars(&r.id, 8)),
            text::escape_html(&r.updated_time)
        );
        rows.push(vec![InlineKeyboardButton::callback(
            format!("{}. {}", idx + 1, text::truncate_chars(&r.title, 20)),
            format!("resume:{}", r.id),
        )]);
    }

    if !footer_buttons.is_empty() {
        rows.push(footer_buttons);
    }

    (text_out, InlineKeyboardMarkup::new(rows))
}

#[must_use]
pub fn get_model_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🚀 4 Argon (High)", "set_model:gemini-4-argon-high"),
            InlineKeyboardButton::callback("⚡ 4 Argon (Med)", "set_model:gemini-4-argon-medium"),
            InlineKeyboardButton::callback("🍃 4 Argon (Low)", "set_model:gemini-4-argon-low"),
        ],
        vec![
            InlineKeyboardButton::callback(
                "✨ 3.8 Flash (High)",
                "set_model:gemini-3.8-flash-high",
            ),
            InlineKeyboardButton::callback(
                "⚡ 3.8 Flash (Med)",
                "set_model:gemini-3.8-flash-medium",
            ),
            InlineKeyboardButton::callback("🍃 3.8 Flash (Low)", "set_model:gemini-3.8-flash-low"),
        ],
        vec![
            InlineKeyboardButton::callback("👑 Opus 5.5 (High)", "set_model:claude-opus-5-5-high"),
            InlineKeyboardButton::callback("👑 Opus 5.5 (Med)", "set_model:claude-opus-5-5-medium"),
        ],
        vec![
            InlineKeyboardButton::callback(
                "🎭 Sonnet 5.5 (High)",
                "set_model:claude-sonnet-5-5-high",
            ),
            InlineKeyboardButton::callback(
                "🎭 Sonnet 5.5 (Med)",
                "set_model:claude-sonnet-5-5-medium",
            ),
        ],
        vec![
            InlineKeyboardButton::callback("🌐 GPT-OSS 120B", "set_model:gpt-oss-120b-medium"),
            InlineKeyboardButton::callback("⚙️ 系统默认", "set_model:default"),
        ],
        vec![InlineKeyboardButton::callback(
            "« 返回控制板",
            "menu:status",
        )],
    ])
}

#[must_use]
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
        vec![InlineKeyboardButton::callback(
            "« 返回控制板",
            "menu:status",
        )],
    ])
}

#[must_use]
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

#[must_use]
pub fn build_status_card(chat_id: i64, session: &ChatSession) -> (String, InlineKeyboardMarkup) {
    let conv_status = session
        .conversation_id
        .as_deref()
        .unwrap_or("新建会话 (尚未生成 ID)");
    let current_model = if session.model.is_empty() {
        "系统默认 (Gemini 3.8 Flash)"
    } else {
        session.model.as_str()
    };
    let current_agent = if session.agent.is_empty() {
        "默认 Agent"
    } else {
        session.agent.as_str()
    };
    let project_str = if session.project.is_empty() {
        "默认工程"
    } else {
        session.project.as_str()
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
        text::escape_html(&session.workspace),
        text::escape_html(project_str),
        text::escape_html(current_model),
        text::escape_html(&session.effort),
        text::escape_html(&session.mode),
        text::escape_html(current_agent),
        sandbox_status_str,
        voice_status_str,
        text::escape_html(conv_status),
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
        vec![InlineKeyboardButton::callback("🔄 重置会话", "cmd:new")],
    ]);

    (status_text, keyboard)
}

#[must_use]
pub fn build_onboarding_card() -> (String, InlineKeyboardMarkup) {
    let text = format!(
        "🚀 <b>欢迎使用 agygram (Antigravity CLI Daemon for Telegram) v{}</b>\n\n\
        这是专为 <b>Google Antigravity CLI (agy)</b> 打造的高性能 Telegram 随身指挥中心。\n\n\
        <b>💡 极速交互指南：</b>\n\
        • 直接发送<b>文本、需求、代码文件或图片</b>，即可触发实时打字机流式推导；\n\
        • 发送 <code>/status</code> 可随时唤起多级原地交互控制面板；\n\
        • 发送 <code>/history</code> 可一键查看并接续 VPS 历史活跃会话；\n\
        • 发送 <code>/plan</code> 与 <code>/walkthrough</code> 可直接调阅方案与总结报告。\n\n\
        <i>轻触下方按钮即可立即开始配置与探索：</i>",
        env!("CARGO_PKG_VERSION")
    );

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

/// Reject callback payloads that are unknown, out of range, or could inject extra CLI flags.
/// Returns a human-readable reason when the payload must not be applied.
#[must_use]
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
        if arg != "continue" && !is_safe_cli_value(arg) {
            return Some("⚠️ 无效的会话 ID。");
        }
    } else if !matches!(
        data,
        "menu:status"
            | "menu:model"
            | "menu:effort"
            | "menu:mode"
            | "menu:history"
            | "cmd:new"
            | "cmd:help_card"
            | "cmd:toggle_voice"
            | "cmd:toggle_sandbox"
            | "cmd:quota"
    ) {
        return Some("⚠️ 未知的按钮操作。");
    }
    None
}

fn back_to_status_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "« 返回控制板",
        "menu:status",
    )]])
}

async fn edit_card_html(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: teloxide::types::MessageId,
    html: String,
    markup: InlineKeyboardMarkup,
    action: &'static str,
) {
    if let Err(err) = bot
        .edit_message_text(target_chat, msg_id, html)
        .parse_mode(ParseMode::Html)
        .reply_markup(markup)
        .await
    {
        log_telegram_err(target_chat.0, action, err);
    }
}

async fn render_and_edit_status_card(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: teloxide::types::MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    prefix: Option<&str>,
) {
    let session = session_mgr.get_or_create(chat_id).await;
    let (status_text, keyboard) = build_status_card(chat_id, &session);
    let full_text = match prefix {
        Some(p) => format!("{p}\n\n{status_text}"),
        None => status_text,
    };
    edit_card_html(
        bot,
        target_chat,
        msg_id,
        full_text,
        keyboard,
        "edit_status_card",
    )
    .await;
}

#[tracing::instrument(skip_all, fields(user_id = q.from.id.0))]
pub async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    // Inline buttons are visible to every member of a chat, so callback traffic must be
    // authorized on its own: pressing a button must not mutate another user's session.
    if !config.is_user_allowed(q.from.id.0) {
        warn!(user_id = q.from.id.0, "ignoring unauthorized callback");
        return Ok(());
    }

    let (Some(data), Some(msg)) = (q.data, q.message) else {
        return Ok(());
    };

    let chat_id = msg.chat().id.0;
    let target_chat = msg.chat().id;
    let msg_id = msg.id();

    if let Some(reason) = reject_reason(&data) {
        if let Err(err) = bot.answer_callback_query(q.id).text(reason).await {
            log_telegram_err(chat_id, "answer_rejected_callback", err);
        }
        return Ok(());
    }

    if let Err(err) = bot.answer_callback_query(q.id).await {
        log_telegram_err(chat_id, "answer_callback_query", err);
    }

    if let Some(model_arg) = data.strip_prefix("set_model:") {
        let target_model = normalize_model_name(model_arg);
        apply_model_and_effort(&session_mgr, chat_id, &target_model).await;
        render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None).await;
    } else if let Some(effort) = data.strip_prefix("set_effort:") {
        session_mgr.set_effort(chat_id, effort.to_string()).await;
        render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None).await;
    } else if let Some(mode) = data.strip_prefix("set_mode:") {
        session_mgr.set_mode(chat_id, mode.to_string()).await;
        render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None).await;
    } else if let Some(resume_id) = data.strip_prefix("resume:") {
        if resume_id == "continue" {
            session_mgr.resume_conversation(chat_id, None).await;
            let pfx = "🔄 <b>已开启历史接续模式 (-c/--continue)</b>\n下一条消息将自动接续最近一次的上下文！";
            render_and_edit_status_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(pfx),
            )
            .await;
        } else {
            session_mgr
                .resume_conversation(chat_id, Some(resume_id.to_string()))
                .await;
            let pfx = format!(
                "✅ <b>已成功恢复并接续会话:</b> <code>{}</code>\n下一条消息将精确接续该上下文！",
                text::escape_html(resume_id)
            );
            render_and_edit_status_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(&pfx),
            )
            .await;
        }
    } else {
        match data.as_str() {
            "cmd:new" => {
                session_mgr.reset_conversation(chat_id).await;
                render_and_edit_status_card(
                    &bot,
                    target_chat,
                    msg_id,
                    chat_id,
                    &session_mgr,
                    Some("🔄 <b>会话已重置！</b>"),
                )
                .await;
            }
            "cmd:toggle_voice" => {
                let _ = session_mgr.toggle_voice_mode(chat_id).await;
                render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None)
                    .await;
            }
            "cmd:toggle_sandbox" => {
                let _ = session_mgr.toggle_sandbox(chat_id).await;
                render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None)
                    .await;
            }
            "menu:status" => {
                render_and_edit_status_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None)
                    .await;
            }
            "menu:model" => {
                edit_card_html(
                    &bot,
                    target_chat,
                    msg_id,
                    "🤖 <b>请选择需要切换的 AI 模型：</b>".to_string(),
                    get_model_keyboard(),
                    "menu_model",
                )
                .await;
            }
            "menu:effort" => {
                edit_card_html(
                    &bot,
                    target_chat,
                    msg_id,
                    "⚡ <b>请选择思考力度 (Effort)：</b>".to_string(),
                    get_effort_keyboard(),
                    "menu_effort",
                )
                .await;
            }
            "menu:mode" => {
                edit_card_html(
                    &bot,
                    target_chat,
                    msg_id,
                    "🎯 <b>请选择执行模式：</b>".to_string(),
                    get_mode_keyboard(),
                    "menu_mode",
                )
                .await;
            }
            "cmd:help_card" => {
                let ver = get_cached_agy_version().await;
                let help_text = format!(
                    "📖 <b>agygram 核心指令速查 (v{} / agy v{})</b>\n\n\
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
                    env!("CARGO_PKG_VERSION"),
                    text::escape_html(ver)
                );
                edit_card_html(
                    &bot,
                    target_chat,
                    msg_id,
                    help_text,
                    back_to_status_keyboard(),
                    "cmd_help_card",
                )
                .await;
            }
            "menu:history" => {
                let recents = AgyExecutor::list_recent_conversations(6).await;
                if recents.is_empty() {
                    edit_card_html(
                        &bot,
                        target_chat,
                        msg_id,
                        "📜 <b>暂未发现历史会话。</b>".to_string(),
                        back_to_status_keyboard(),
                        "menu_history_empty",
                    )
                    .await;
                } else {
                    let (card_text, keyboard) = build_history_card(
                        "📜 <b>VPS 最近活跃历史会话 (点击即可接续)：</b>",
                        &recents,
                        vec![InlineKeyboardButton::callback(
                            "« 返回控制板",
                            "menu:status",
                        )],
                    );
                    edit_card_html(
                        &bot,
                        target_chat,
                        msg_id,
                        card_text,
                        keyboard,
                        "menu_history",
                    )
                    .await;
                }
            }
            "cmd:quota" => {
                let quota_html = match crate::quota::QuotaFetcher::format_models_and_quota().await {
                    Ok(html) => html,
                    Err(err) => {
                        warn!(chat_id, error = %err, "callback quota fetch failed");
                        format!(
                            "⚠️ <b>实时 Quota 获取异常:</b> <code>{}</code>\n\n<i>💡 可在 VPS 终端执行 <code>agygram quota</code> 快速验证。</i>",
                            text::escape_html(&err.to_string())
                        )
                    }
                };
                edit_or_send_html_chunks(
                    &bot,
                    target_chat,
                    msg_id,
                    &quota_html,
                    Some(back_to_status_keyboard()),
                )
                .await;
            }
            _ => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_history_card, is_safe_cli_value, is_supported_effort, is_supported_mode,
        reject_reason,
    };
    use crate::executor::RecentSessionInfo;
    use crate::models::is_safe_free_value;
    use teloxide::types::InlineKeyboardButton;

    #[test]
    fn cli_flag_smuggling_is_rejected() {
        for evil in [
            "--sandbox",
            "--dangerously-skip-permissions",
            "-c",
            "x\nrm -rf /",
            "",
        ] {
            assert!(!is_safe_cli_value(evil), "cli check accepted: {evil:?}");
            assert!(!is_safe_free_value(evil), "free check accepted: {evil:?}");
        }
        assert!(
            !is_safe_cli_value(&"a".repeat(65)),
            "cli length cap ignored"
        );
        assert!(
            is_safe_free_value(&"a".repeat(65)),
            "65 chars is a legal free-form name"
        );
        assert!(
            !is_safe_free_value(&"a".repeat(129)),
            "free length cap ignored"
        );
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
        assert!(reject_reason("resume:continue").is_none());
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

    #[test]
    fn history_card_escapes_titles_and_includes_flags() {
        let recents = vec![RecentSessionInfo {
            id: "8de907d6-1c2b-4a5f-9e3d".to_string(),
            title: "Fix <script> & bug".to_string(),
            updated_time: "11-11 10:00".to_string(),
            has_plan: true,
            has_walkthrough: true,
        }];
        let (card, kb) = build_history_card(
            "📜 <b>History</b>",
            &recents,
            vec![InlineKeyboardButton::callback("Back", "menu:status")],
        );
        assert!(card.contains("Fix &lt;script&gt; &amp; bug"));
        assert!(card.contains("[Plan+WT]"));
        assert!(card.contains("<code>8de907d6</code>"));
        assert_eq!(kb.inline_keyboard.len(), 2);
    }
}
