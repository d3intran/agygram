use crate::bot::callbacks::{
    apply_model_and_effort, build_card_hub, build_effort_card, build_history_card, build_mode_card,
    build_model_family_card, build_onboarding_card, build_session_card, encode_agent_callback,
    peek_cached_agy_version, render_home_card_data, resolve_builtin_widget,
};
use crate::bot::formatter::TelegramFormatter;
use crate::bot::media::{MAX_UPLOAD_BYTES, format_file_caption};
use crate::bot::pipeline::{
    BotRuntime, dispatch_message_with_session, dispatch_prompt_with_session,
    edit_or_send_html_chunks, send_html_chunks,
};
use crate::bot::widget::{
    build_card_generation_prompt, deliver_widget_with_fallback, send_widget_html_file,
};
use crate::config::Config;
use crate::executor::AgyExecutor;
use crate::models::{
    Effort, Mode, SUPPORTED_MODELS, is_safe_cli_value, is_safe_free_value, list_model_families,
    model_display_label, refresh_models_now, resolve_model_input, supports_effort_flag,
    trigger_background_model_refresh_if_stale,
};
use crate::quota::QuotaFetcher;
use crate::session::{SessionManager, resolve_user_path};
use crate::text;
use std::fmt::Write as _;
use std::str::FromStr;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use teloxide::utils::command::BotCommands;
use tracing::{error, info, warn};

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "agygram 支持的全部指令:")]
pub enum Command {
    #[command(description = "显示欢迎卡片与极速交互指引")]
    Start,
    #[command(description = "显示帮助手册与完整指令一览")]
    Help,
    #[command(description = "打开多级交互控制中心 (/panel)")]
    Panel,
    #[command(description = "打开多级交互控制中心 (别名 /panel)")]
    Settings,
    #[command(description = "打开会话配置与控制中心 (别名 /panel)")]
    Status,
    #[command(description = "打开会话管理中心 (新建/切换/恢复/统计)")]
    Session,
    #[command(description = "开启新会话 (重置上下文与统计)")]
    New,
    #[command(description = "重置当前会话 (别名 /new)")]
    Reset,
    #[command(description = "清空当前会话 (别名 /new)")]
    Clear,
    #[command(description = "恢复/接续历史对话 (/resume [<会话ID>])")]
    Resume(String),
    #[command(description = "查看 VPS 历史会话列表并一键接续")]
    History,
    #[command(description = "查看当前会话的架构方案 (implementation_plan.md)")]
    Plan,
    #[command(description = "查看当前会话的执行总结 (walkthrough.md)")]
    Walkthrough,
    #[command(description = "查看已挂载的 Antigravity 技能库")]
    Skills,
    #[command(description = "查看当前生效的全局与项目规则")]
    Rules,
    #[command(description = "快速查看账号实时模型额度剩余")]
    Quota,
    #[command(description = "快速查看账号实时模型额度剩余")]
    Usage,
    #[command(description = "切换主工作目录 (/cd <绝对路径或~>)")]
    Cd(String),
    #[command(description = "添加额外的项目工作目录 (/adddir <路径>)")]
    AddDir(String),
    #[command(description = "清空所有添加的额外项目工作目录")]
    ClearDirs,
    #[command(description = "查看当前绑定的所有工作目录")]
    Dirs,
    #[command(description = "查看当前工作目录路径")]
    Pwd,
    #[command(description = "打开模型选择器或切换模型 (/model [<名称/序号/别名>])")]
    Model(String),
    #[command(description = "实时同步并列出系统支持的全部 AI 模型")]
    Models,
    #[command(description = "查看或管理 MCP 服务器 (/mcp [<subcommand>])")]
    Mcp(String),
    #[command(description = "管理 agy 插件生态 (/plugin [list | install <name>])")]
    Plugin(String),
    #[command(description = "查看已安装的 agy 插件列表")]
    Plugins,
    #[command(description = "查看 agy CLI 最新更新日志")]
    Changelog,
    #[command(description = "一键执行 agy update 升级 CLI")]
    Update,
    #[command(description = "查看或切换沙盒隔离模式 (/sandbox on | off | toggle)")]
    Sandbox(String),
    #[command(description = "查看或绑定 CLI 工程 (/project [<name> | clear])")]
    Project(String),
    #[command(description = "查看或设置思考力度 (/effort low | medium | high | xhigh | max)")]
    Effort(String),
    #[command(description = "查看或切换执行模式 (/mode auto | accept-edits | plan)")]
    Mode(String),
    #[command(description = "查看或设置 Agent (/agent [<名称>])")]
    Agent(String),
    #[command(description = "列出系统支持的所有 Agent")]
    Agents,
    #[command(description = "查看或设置系统提示词 (/system [<Prompt>])")]
    System(String),
    #[command(description = "发送 VPS 上的文件给用户 (/file <文件路径>)")]
    File(String),
    #[command(description = "取消当前正在运行的任务并释放锁")]
    Cancel,
    #[command(description = "AI 语音回复模式 (/voice on | off | toggle | <Prompt>)")]
    Voice(String),
    #[command(
        description = "QuickJS 交互卡片与离线小游戏 (/card [2048|扫雷|demo|status|html|<需求>])"
    )]
    Card(String),
    #[command(description = "打开 QuickJS 交互卡片与离线小游戏中心 (/widget)")]
    Widget(String),
}

#[tracing::instrument(skip_all, fields(user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0), chat_id = msg.chat.id.0))]
pub async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Command,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map_or(0, |u| u.id.0);
    let chat_id = msg.chat.id.0;
    if !config.is_user_allowed(user_id) {
        warn!(user_id, chat_id, "ignoring unauthorized command");
        return Ok(());
    }

    info!(user_id, chat_id, "received telegram command");

    match cmd {
        Command::Start => handle_start(&bot, msg.chat.id).await?,
        Command::Help => handle_help(&bot, msg.chat.id).await?,
        Command::Panel | Command::Settings | Command::Status => {
            handle_panel(&bot, msg.chat.id, &session_mgr).await?;
        }
        Command::Session => handle_session_panel(&bot, msg.chat.id, &session_mgr).await?,
        Command::New | Command::Reset | Command::Clear => {
            session_mgr.reset_conversation(chat_id).await;
            send_html(
                &bot,
                msg.chat.id,
                "🔄 <b>会话已重置</b>，上下文与统计已恢复初始状态！",
            )
            .await?;
        }
        Command::Resume(arg) => handle_resume(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::History => handle_history(&bot, msg.chat.id).await?,
        Command::Plan => handle_brain_doc(&bot, msg.chat.id, &session_mgr, true).await?,
        Command::Walkthrough => handle_brain_doc(&bot, msg.chat.id, &session_mgr, false).await?,
        Command::Skills => handle_skills(&bot, msg.chat.id).await,
        Command::Rules => handle_rules(&bot, msg.chat.id, &session_mgr).await,
        Command::Quota | Command::Usage => handle_quota(&bot, msg.chat.id).await?,
        Command::Cd(path) => handle_cd(&bot, msg.chat.id, &session_mgr, &path).await?,
        Command::AddDir(path) => handle_add_dir(&bot, msg.chat.id, &session_mgr, &path).await?,
        Command::ClearDirs => {
            session_mgr.clear_extra_dirs(chat_id).await;
            send_html(
                &bot,
                msg.chat.id,
                "🧹 <b>已清空所有添加的额外项目工作目录！</b>",
            )
            .await?;
        }
        Command::Dirs | Command::Pwd => handle_dirs(&bot, msg.chat.id, &session_mgr).await?,
        Command::Model(arg) => handle_model(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Models => handle_models_list(&bot, msg.chat.id).await,
        Command::Mcp(arg) => {
            handle_cli_subcommand(&bot, msg.chat.id, CliSubcmdKind::Mcp, &arg).await?;
        }
        Command::Plugin(arg) => {
            handle_cli_subcommand(&bot, msg.chat.id, CliSubcmdKind::Plugin, &arg).await?;
        }
        Command::Plugins => handle_plugins_list(&bot, msg.chat.id).await,
        Command::Changelog => handle_changelog(&bot, msg.chat.id).await,
        Command::Update => handle_update(&bot, msg.chat.id).await?,
        Command::Sandbox(arg) => handle_sandbox(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Project(arg) => handle_project(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Effort(arg) => handle_effort(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Mode(arg) => handle_mode(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Agent(arg) => handle_agent(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::Agents => handle_agents_list(&bot, msg.chat.id).await,
        Command::System(arg) => handle_system(&bot, msg.chat.id, &session_mgr, &arg).await?,
        Command::File(path) => handle_file(&bot, msg.chat.id, &session_mgr, &path).await?,
        Command::Cancel => handle_cancel(&bot, msg.chat.id, &session_mgr, &runtime).await?,
        Command::Voice(text_arg) => {
            return handle_voice(bot, msg, config, session_mgr, runtime, text_arg).await;
        }
        Command::Card(arg) | Command::Widget(arg) => {
            return Box::pin(handle_card_command(
                bot,
                msg,
                config,
                session_mgr,
                runtime,
                &arg,
            ))
            .await;
        }
    }

    Ok(())
}

async fn send_html(bot: &Bot, chat_id: ChatId, html: impl Into<String>) -> ResponseResult<()> {
    bot.send_message(chat_id, html)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

async fn handle_start(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let (onboarding_text, keyboard) = build_onboarding_card();
    bot.send_message(chat_id, onboarding_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn handle_help(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let ver = peek_cached_agy_version();
    let help_text = format!(
        "🤖 <b>agygram 指令手册 (v{} / Antigravity CLI v{})</b>\n\n\
        <b>🎛 控制中心与会话管理：</b>\n\
        • /panel / /settings / /status : 打开多级交互式控制中心\n\
        • /session : 打开会话管理中心（查看统计、一键切换/恢复）\n\
        • /card 或 /widget : QuickJS 交互卡片与离线小游戏中心\n\
        • /new 或 /reset : 开启全新对话 (重置上下文与统计)\n\
        • /resume <code>[&lt;ID&gt;]</code> : 恢复/接续历史对话\n\
        • /history : 查看 VPS 历史会话列表并一键接续\n\
        • /cancel : 紧急终止正在运行的任务并释放锁\n\
        • /usage 或 /quota : 快速查看实时模型额度剩余\n\n\
        <b>🧠 Brain 方案与定制生态：</b>\n\
        • /plan : 查看当前会话的架构方案 (implementation_plan.md)\n\
        • /walkthrough : 查看当前会话的执行报告 (walkthrough.md)\n\
        • /skills : 查看已挂载的 Antigravity 技能库\n\
        • /rules : 查看当前生效的全局与项目规则 (GEMINI.md / AGENTS.md / .agents/rules)\n\
        • /mcp <code>[&lt;subcommand&gt;]</code> : 管理 MCP 服务器\n\
        • /plugin <code>[&lt;subcommand&gt;]</code> : 管理插件生态\n\
        • /update : 一键升级 agy CLI\n\n\
        <b>📂 目录与工程：</b>\n\
        • /cd <code>&lt;path&gt;</code> : 切换主工作目录 (支持 <code>~/...</code>)\n\
        • /adddir <code>&lt;path&gt;</code> : 添加附加项目工作目录\n\
        • /cleardirs : 清空附加项目工作目录\n\
        • /dirs 或 /pwd : 查看当前绑定的所有工作目录\n\
        • /project <code>[&lt;name&gt;]</code> : 绑定或重置 Project\n\
        • /sandbox <code>[on|off|toggle]</code> : 沙盒隔离开关\n\
        • /file <code>&lt;path&gt;</code> : 传输 VPS 上的文件到 Telegram\n\n\
        <b>🤖 模型与运行参数：</b>\n\
        • /model <code>[&lt;名称/序号/别名&gt;]</code> : 打开模型家族选择器或直接切换 (如 <code>h2</code>, <code>carbon</code>, <code>argon</code>, <code>flash</code>, <code>orch</code>, <code>opus</code>, <code>sonnet</code>, <code>oss</code>)\n\
        • /models : 实时拉取 <code>agy models</code> 并按家族分组展示\n\
        • /effort <code>[&lt;level&gt;]</code> : 设置思考力度 (<code>low</code> / <code>medium</code> / <code>high</code> / <code>xhigh</code> / <code>max</code>)\n\
        • /mode <code>[&lt;mode&gt;]</code> : 切换执行模式 (<code>auto</code> / <code>accept-edits</code> / <code>plan</code>)\n\
        • /agent <code>[&lt;name&gt;]</code> : 切换指定的 Agent 智能体\n\
        • /system <code>[&lt;prompt&gt;]</code> : 设置自定义 System Prompt\n\
        • /voice <code>[on|off|toggle|id|&lt;prompt&gt;]</code> : AI 语音回复模式 (Fish Audio TTS)",
        env!("CARGO_PKG_VERSION"),
        text::escape_html(ver)
    );
    send_html_chunks(bot, chat_id, &help_text).await;
    Ok(())
}

async fn handle_panel(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
) -> ResponseResult<()> {
    let (panel_text, keyboard) = render_home_card_data(chat_id.0, session_mgr, false, None).await;
    bot.send_message(chat_id, panel_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn handle_session_panel(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
) -> ResponseResult<()> {
    let (session, recents) = tokio::join!(
        session_mgr.get_or_create(chat_id.0),
        AgyExecutor::list_recent_conversations(6),
    );
    let (card_text, keyboard) = build_session_card(&session, &recents, None);
    bot.send_message(chat_id, card_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn handle_resume(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    arg: &str,
) -> ResponseResult<()> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        let recents = AgyExecutor::list_recent_conversations(8).await;
        if recents.is_empty() {
            session_mgr.resume_conversation(chat_id.0, None).await;
            send_html(
                bot,
                chat_id,
                "🔄 <b>暂未扫描到历史会话，已自动开启接续模式 (-c/--continue)</b>！下一条消息将自动接续最近一次的 agy 上下文。",
            )
            .await?;
        } else {
            let (card_text, keyboard) = build_history_card(
                "🔄 <b>请选择需要恢复接续的历史会话：</b>",
                &recents,
                vec![
                    InlineKeyboardButton::callback("🔄 接续最近一次 (-c)", "rs:continue"),
                    InlineKeyboardButton::callback("« 返回控制中心", "cp:home"),
                ],
            );
            bot.send_message(chat_id, card_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
        }
    } else if !is_safe_cli_value(trimmed) {
        bot.send_message(
            chat_id,
            "⚠️ 无效的会话 ID：不能以 - 开头，且只允许字母、数字与 _ - . +",
        )
        .await?;
    } else {
        session_mgr
            .resume_conversation(chat_id.0, Some(trimmed.to_string()))
            .await;
        send_html(
            bot,
            chat_id,
            format!(
                "🔄 <b>已成功绑定历史会话 ID:</b> <code>{}</code>\n下一条消息将精确接续该上下文！",
                text::escape_html(trimmed)
            ),
        )
        .await?;
    }
    Ok(())
}

async fn handle_history(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let recents = AgyExecutor::list_recent_conversations(8).await;
    if recents.is_empty() {
        send_html(bot, chat_id, "📜 <b>暂未发现历史会话。</b>").await?;
    } else {
        let (card_text, keyboard) = build_history_card(
            "📜 <b>VPS 最近活跃历史会话 (点击即可接续)：</b>",
            &recents,
            vec![InlineKeyboardButton::callback("« 返回控制中心", "cp:home")],
        );
        bot.send_message(chat_id, card_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
    }
    Ok(())
}

async fn handle_brain_doc(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    is_plan: bool,
) -> ResponseResult<()> {
    let session = session_mgr.get_or_create(chat_id.0).await;
    let Some(ref conv_id) = session.conversation_id else {
        let msg = if is_plan {
            "📋 <b>当前为新建会话，尚未关联历史会话 ID。</b>"
        } else {
            "📝 <b>当前为新建会话，尚未关联历史会话 ID。</b>"
        };
        return send_html(bot, chat_id, msg).await;
    };

    let content_opt = if is_plan {
        AgyExecutor::read_implementation_plan(conv_id).await
    } else {
        AgyExecutor::read_walkthrough(conv_id).await
    };

    if let Some(doc_text) = content_opt {
        let html = TelegramFormatter::to_telegram_html(&doc_text);
        send_html_chunks(bot, chat_id, &html).await;
    } else {
        let missing = if is_plan {
            "📋 <b>当前会话尚未生成 implementation_plan.md 方案文件。</b>"
        } else {
            "📝 <b>当前会话尚未生成 walkthrough.md 总结文件。</b>"
        };
        send_html(bot, chat_id, missing).await?;
    }
    Ok(())
}

async fn handle_skills(bot: &Bot, chat_id: ChatId) {
    let skills_str = match AgyExecutor::list_skill_sections().await {
        Ok(sections) => {
            let mut out = String::new();
            for (section, section_skills) in sections {
                let _ = writeln!(out, "📦 <b>{section}</b>:");
                for (name, desc) in section_skills {
                    if desc.is_empty() {
                        let _ = writeln!(
                            out,
                            "• <code>{}</code>",
                            text::escape_html(&format!("/{name}"))
                        );
                    } else {
                        let short_desc = text::truncate_chars(&desc, 50);
                        let _ = writeln!(
                            out,
                            "• <code>{}</code> : <i>{}</i>",
                            text::escape_html(&format!("/{name}")),
                            text::escape_html(&short_desc)
                        );
                    }
                }
                out.push('\n');
            }
            if out.is_empty() {
                "未发现已安装的 Antigravity 技能。".to_string()
            } else {
                out.trim_end().to_string()
            }
        }
        Err(msg) => msg.to_string(),
    };
    let html = format!("🧩 <b>Antigravity 技能库 (Skills)：</b>\n\n{skills_str}");
    send_html_chunks(bot, chat_id, &html).await;
}

async fn handle_rules(bot: &Bot, chat_id: ChatId, session_mgr: &SessionManager) {
    let session = session_mgr.get_or_create(chat_id.0).await;
    let rules_str = match AgyExecutor::list_rule_sections(&session.workspace).await {
        Ok(sections) => {
            let mut out = String::new();
            for sec in sections {
                let _ = writeln!(
                    out,
                    "{} <b>{}</b>:",
                    sec.icon,
                    text::escape_html(&sec.title)
                );
                let _ = writeln!(
                    out,
                    "<pre><code>{}</code></pre>\n",
                    text::escape_html(&sec.body)
                );
            }
            if out.is_empty() {
                "📋 <b>当前暂无活跃的自定义规则文件。</b>\n可在项目根目录下创建 <code>GEMINI.md</code> 或 <code>AGENTS.md</code> 添加项目规则。".to_string()
            } else {
                out.trim_end().to_string()
            }
        }
        Err(msg) => msg.to_string(),
    };
    let html = format!("🛡️ <b>Antigravity 规则配置 (Rules)：</b>\n\n{rules_str}");
    send_html_chunks(bot, chat_id, &html).await;
}

async fn handle_card_command(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
    arg: &str,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let trimmed = arg.trim();

    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("hub")
        || trimmed.eq_ignore_ascii_case("menu")
        || trimmed.eq_ignore_ascii_case("list")
    {
        let (hub_text, keyboard) = build_card_hub();
        bot.send_message(chat_id, hub_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
        return Ok(());
    }

    let first_word = trimmed.split_whitespace().next().unwrap_or_default();
    if first_word.eq_ignore_ascii_case("html") {
        let html_target = trimmed
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim());
        if html_target.is_empty() {
            let (hub_text, keyboard) = build_card_hub();
            bot.send_message(
                chat_id,
                format!(
                    "📄 <b>用法：</b><code>/card html &lt;目标&gt;</code> (如 <code>/card html 2048</code>、<code>minesweeper</code>、<code>dash</code>)\n\n{hub_text}"
                ),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
            return Ok(());
        }

        match resolve_builtin_widget(html_target, &session_mgr, chat_id.0).await {
            Ok(Some(widget)) => {
                send_widget_html_file(&bot, chat_id, &widget).await?;
            }
            Ok(None) => {
                send_html(
                    &bot,
                    chat_id,
                    "⚠️ 未识别的导出目标。支持：<code>2048</code>, <code>minesweeper</code>, <code>memory</code>, <code>reaction</code>, <code>dash</code>, <code>quiz</code>, <code>table</code>, <code>agent</code>, <code>status</code>",
                )
                .await?;
            }
            Err(err) => {
                send_html(
                    &bot,
                    chat_id,
                    format!(
                        "❌ <b>构建 HTML 附件失败</b>: {}",
                        text::escape_html(&err.to_string())
                    ),
                )
                .await?;
            }
        }
        return Ok(());
    }

    match resolve_builtin_widget(trimmed, &session_mgr, chat_id.0).await {
        Ok(Some(widget)) => {
            let outcome = Box::pin(deliver_widget_with_fallback(
                &bot,
                &config.bot_token,
                chat_id,
                &widget,
            ))
            .await;
            info!(
                chat_id = chat_id.0,
                title = %widget.title,
                outcome = ?outcome,
                "Delivered built-in card/game widget"
            );
            Ok(())
        }
        Err(err) => {
            warn!(error = ?err, "Failed to build widget payload");
            send_html(
                &bot,
                chat_id,
                format!(
                    "❌ <b>渲染交互卡片失败</b>: {}",
                    text::escape_html(&err.to_string())
                ),
            )
            .await
        }
        Ok(None) => {
            let ai_prompt = build_card_generation_prompt(trimmed);
            Box::pin(dispatch_prompt_with_session(
                bot,
                msg,
                config,
                session_mgr,
                runtime,
                Some(ai_prompt),
                None,
            ))
            .await
        }
    }
}

async fn handle_quota(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let placeholder = bot
        .send_message(chat_id, "⏳ <i>正在查询实时额度...</i>")
        .parse_mode(ParseMode::Html)
        .await?;

    let html = match QuotaFetcher::format_models_and_quota().await {
        Ok(quota_html) => quota_html,
        Err(err) => {
            error!(chat_id = chat_id.0, error = %err, "failed to fetch live quota");
            format!(
                "⚠️ <b>实时额度获取失败:</b> <code>{}</code>\n\n\
                <i>💡 已尝试自动刷新 OAuth 凭证。若持续异常，可在 VPS 执行 <code>agygram quota</code> 排查。</i>",
                text::escape_html(&err.to_string())
            )
        }
    };

    let kb = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("🔄 刷新额度", "cp:quota:r"),
            InlineKeyboardButton::callback("🤖 切换模型", "cp:mf"),
        ],
        vec![InlineKeyboardButton::callback("« 返回控制中心", "cp:home")],
    ]);
    edit_or_send_html_chunks(bot, chat_id, placeholder.id, &html, Some(kb)).await;
    Ok(())
}

async fn handle_cd(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    path: &str,
) -> ResponseResult<()> {
    let path_trimmed = path.trim();
    if path_trimmed.is_empty() {
        return send_html(
            bot,
            chat_id,
            "⚠️ 请提供工作目录路径，例如 <code>/cd /path/to/project</code> 或 <code>/cd ~/agygram</code>",
        )
        .await;
    }

    let session = session_mgr.get_or_create(chat_id.0).await;
    let target_path = resolve_user_path(path_trimmed, &session.workspace);

    match tokio::fs::metadata(&target_path).await {
        Ok(meta) if meta.is_dir() => {
            let path_str = target_path.to_string_lossy().into_owned();
            session_mgr.set_workspace(chat_id.0, path_str.clone()).await;
            send_html(
                bot,
                chat_id,
                format!(
                    "📁 <b>主工作目录已成功切换为:</b> <code>{}</code>",
                    text::escape_html(&path_str)
                ),
            )
            .await
        }
        Ok(_) => {
            send_html(
                bot,
                chat_id,
                format!(
                    "❌ 无法切换工作目录: 路径 <code>{}</code> 不是目录。",
                    text::escape_html(path_trimmed)
                ),
            )
            .await
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            match tokio::fs::create_dir_all(&target_path).await {
                Ok(()) => {
                    let path_str = target_path.to_string_lossy().into_owned();
                    session_mgr.set_workspace(chat_id.0, path_str.clone()).await;
                    send_html(
                        bot,
                        chat_id,
                        format!(
                            "📁 <b>目标目录不存在，已自动新建并切换至:</b> <code>{}</code>",
                            text::escape_html(&path_str)
                        ),
                    )
                    .await
                }
                Err(create_err) => {
                    warn!(
                        chat_id = chat_id.0,
                        path = %target_path.display(),
                        error = %create_err,
                        "failed to create workspace directory"
                    );
                    send_html(
                        bot,
                        chat_id,
                        format!(
                            "❌ 无法切换工作目录: 路径 <code>{}</code> 无效或权限不足。",
                            text::escape_html(path_trimmed)
                        ),
                    )
                    .await
                }
            }
        }
        Err(err) => {
            warn!(
                chat_id = chat_id.0,
                path = %target_path.display(),
                error = %err,
                "failed to stat workspace directory"
            );
            send_html(
                bot,
                chat_id,
                format!(
                    "❌ 无法切换工作目录: 路径 <code>{}</code> 无效或权限不足。",
                    text::escape_html(path_trimmed)
                ),
            )
            .await
        }
    }
}

async fn handle_add_dir(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    path: &str,
) -> ResponseResult<()> {
    let path_trimmed = path.trim();
    if path_trimmed.is_empty() {
        return send_html(
            bot,
            chat_id,
            "⚠️ 请提供项目目录路径，例如 <code>/adddir /path/to/project</code> 或 <code>/adddir ~/project</code>",
        )
        .await;
    }

    let session = session_mgr.get_or_create(chat_id.0).await;
    let target_path = resolve_user_path(path_trimmed, &session.workspace);
    let path_str = target_path.to_string_lossy().into_owned();
    session_mgr.add_extra_dir(chat_id.0, path_str.clone()).await;
    send_html(
        bot,
        chat_id,
        format!(
            "📂 <b>已添加项目工作目录:</b> <code>{}</code>",
            text::escape_html(&path_str)
        ),
    )
    .await
}

async fn handle_dirs(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
) -> ResponseResult<()> {
    let session = session_mgr.get_or_create(chat_id.0).await;
    let mut text_out = format!(
        "📂 <b>当前绑定工作目录</b>\n\n• <b>主目录:</b> <code>{}</code>",
        text::escape_html(&session.workspace)
    );
    if !session.extra_dirs.is_empty() {
        text_out.push_str("\n\n<b>附加项目目录:</b>\n");
        for d in &session.extra_dirs {
            let _ = writeln!(text_out, "• <code>{}</code>", text::escape_html(d));
        }
    }
    send_html(bot, chat_id, text_out).await
}

async fn handle_model(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    model_arg: &str,
) -> ResponseResult<()> {
    trigger_background_model_refresh_if_stale();
    let trimmed = model_arg.trim();
    let mut banner: Option<String> = None;

    if !trimmed.is_empty() {
        let mut resolved = resolve_model_input(trimmed);
        if resolved.is_none() {
            // Try a forced CLI refresh in case a brand-new model was just deployed upstream.
            let _ = refresh_models_now().await;
            resolved = resolve_model_input(trimmed);
        }

        let Some(target_model) = resolved else {
            let ((overview, _), session) = tokio::join!(
                QuotaFetcher::peek_overview_and_revalidate(),
                session_mgr.get_or_create(chat_id.0),
            );
            let mut supported_list = String::new();
            for (idx, (id, name)) in SUPPORTED_MODELS.iter().enumerate() {
                if idx > 0 {
                    supported_list.push('\n');
                }
                let _ = write!(supported_list, "• <code>{id}</code> (<i>{name}</i>)");
            }
            let (_, keyboard) = build_model_family_card(&session, overview.as_ref(), None);
            bot.send_message(
                chat_id,
                format!(
                    "⚠️ <b>无效的模型名称:</b> <code>{}</code>\n\n\
                    <b>当前参考模型（可使用 /models 查看完整实时列表）：</b>\n{}\n\n\
                    <i>也可直接点击下方模型家族快速切换：</i>",
                    text::escape_html(trimmed),
                    supported_list
                ),
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
            return Ok(());
        };

        let inferred = apply_model_and_effort(session_mgr, chat_id.0, &target_model).await;
        let label = model_display_label(&target_model);
        banner = Some(match inferred {
            Some(eff) => format!(
                "✅ <b>AI 模型已切换为:</b> <code>{}</code> (<code>{}</code> · Effort 同步为 <code>{}</code>)",
                text::escape_html(&label),
                text::escape_html(&target_model),
                eff.as_str()
            ),
            None if target_model.is_empty() => {
                format!(
                    "✅ <b>已恢复系统默认模型:</b> <code>{}</code>",
                    text::escape_html(&label)
                )
            }
            None => format!(
                "✅ <b>AI 模型已切换为:</b> <code>{}</code> (<code>{}</code>)",
                text::escape_html(&label),
                text::escape_html(&target_model)
            ),
        });
    }

    let ((overview, _), session) = tokio::join!(
        QuotaFetcher::peek_overview_and_revalidate(),
        session_mgr.get_or_create(chat_id.0),
    );
    let (card_text, keyboard) =
        build_model_family_card(&session, overview.as_ref(), banner.as_deref());
    bot.send_message(chat_id, card_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn handle_models_list(bot: &Bot, chat_id: ChatId) {
    let (_, (overview, _)) = tokio::join!(
        refresh_models_now(),
        QuotaFetcher::peek_overview_and_revalidate(),
    );
    let families = list_model_families();

    let mut out = String::from("📋 <b>Antigravity CLI 实时模型目录</b>\n\n");
    let mut index = 1usize;
    for fam in &families {
        let pool_badge = overview
            .as_ref()
            .and_then(|ov| ov.button_pct(fam.quota_pool))
            .map(|p| format!(" · 剩余 {p}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{} <b>{}</b>{}:",
            fam.icon,
            text::escape_html(&fam.title),
            text::escape_html(&pool_badge)
        );
        for m in &fam.models {
            let dyn_tag = if m.is_dynamic { " 🆕" } else { "" };
            let _ = writeln!(
                out,
                "  {}. <code>{}</code> — <i>{}</i>{dyn_tag}",
                index,
                text::escape_html(&m.id),
                text::escape_html(&m.label)
            );
            index += 1;
        }
        out.push('\n');
    }
    out.push_str(
        "<i>💡 使用 <code>/model &lt;序号|名称|别名&gt;</code> 切换，或点击下方选择器：</i>",
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("🤖 打开模型选择器", "cp:mf"),
        InlineKeyboardButton::callback("« 返回控制中心", "cp:home"),
    ]]);
    let chunks = TelegramFormatter::split_html_chunks(&out, 4000);
    let total = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut req = bot
            .send_message(chat_id, &chunk)
            .parse_mode(ParseMode::Html);
        if i + 1 == total {
            req = req.reply_markup(kb.clone());
        }
        if let Err(err) = req.await {
            warn!(chat_id = chat_id.0, error = ?err, "failed to send /models list");
        }
    }
}

#[derive(Clone, Copy)]
enum CliSubcmdKind {
    Mcp,
    Plugin,
}

async fn handle_cli_subcommand(
    bot: &Bot,
    chat_id: ChatId,
    kind: CliSubcmdKind,
    arg: &str,
) -> ResponseResult<()> {
    let trimmed = arg.trim();
    let args: Vec<&str> = if trimmed.is_empty() {
        vec!["list"]
    } else {
        trimmed.split_whitespace().collect()
    };

    let (wait_label, ok_prefix, empty_msg, err_prefix) = match kind {
        CliSubcmdKind::Mcp => (
            "⚙️ <b>正在查询 MCP 服务配置...</b>",
            "⚙️ <b>MCP 服务器列表/操作结果:</b>",
            "暂无配置的 MCP 服务器。",
            "❌ <b>MCP 查询失败:</b>",
        ),
        CliSubcmdKind::Plugin => (
            "🧩 <b>正在查询 agy 插件生态...</b>",
            "🧩 <b>agy 插件生态列表/操作结果:</b>",
            "暂无安装的插件。",
            "❌ <b>插件查询失败:</b>",
        ),
    };

    let wait_msg = bot
        .send_message(chat_id, wait_label)
        .parse_mode(ParseMode::Html)
        .await?;

    let result = match kind {
        CliSubcmdKind::Mcp => AgyExecutor::run_mcp_cmd(&args).await,
        CliSubcmdKind::Plugin => AgyExecutor::run_plugin_cmd(&args).await,
    };

    let html = match result {
        Ok(out) => {
            let body = if out.trim().is_empty() {
                empty_msg
            } else {
                out.as_str()
            };
            format!(
                "{ok_prefix}\n\n<pre><code>{}</code></pre>",
                text::escape_html(body)
            )
        }
        Err(err) => {
            format!(
                "{err_prefix}\n<pre><code>{}</code></pre>",
                text::escape_html(&err.to_string())
            )
        }
    };

    edit_or_send_html_chunks(bot, chat_id, wait_msg.id, &html, None).await;
    Ok(())
}

async fn handle_plugins_list(bot: &Bot, chat_id: ChatId) {
    let plugins_str = AgyExecutor::list_plugins().await.unwrap_or_default();
    let html = format!(
        "🧩 <b>已安装的 agy 插件列表:</b>\n\n<pre><code>{}</code></pre>",
        text::escape_html(&plugins_str)
    );
    send_html_chunks(bot, chat_id, &html).await;
}

async fn handle_changelog(bot: &Bot, chat_id: ChatId) {
    let changelog_str = AgyExecutor::show_changelog().await.unwrap_or_default();
    let html = format!(
        "📢 <b>Antigravity CLI 更新日志:</b>\n\n{}",
        TelegramFormatter::to_telegram_html(&changelog_str)
    );
    send_html_chunks(bot, chat_id, &html).await;
}

async fn handle_update(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let wait_msg = bot
        .send_message(chat_id, "🔄 <b>正在执行 agy update 检查并升级 CLI...</b>")
        .parse_mode(ParseMode::Html)
        .await?;

    let html = match AgyExecutor::run_update().await {
        Ok(out) => format!(
            "✅ <b>CLI 更新执行完毕:</b>\n\n<pre><code>{}</code></pre>",
            text::escape_html(&out)
        ),
        Err(err) => format!(
            "❌ <b>CLI 更新失败:</b>\n<pre><code>{}</code></pre>",
            text::escape_html(&err.to_string())
        ),
    };

    edit_or_send_html_chunks(bot, chat_id, wait_msg.id, &html, None).await;
    Ok(())
}

async fn handle_sandbox(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    sandbox_arg: &str,
) -> ResponseResult<()> {
    let trimmed = sandbox_arg.trim().to_ascii_lowercase();
    let enabled = match trimmed.as_str() {
        "on" | "true" | "1" | "enable" => {
            session_mgr.set_sandbox(chat_id.0, true).await;
            true
        }
        "off" | "false" | "0" | "disable" => {
            session_mgr.set_sandbox(chat_id.0, false).await;
            false
        }
        _ => session_mgr.toggle_sandbox(chat_id.0).await,
    };
    let status_str = if enabled {
        "🛡️ <b>沙盒隔离模式已开启</b>\n后续任务将附加 <code>--sandbox</code> 参数运行。"
    } else {
        "⚡ <b>沙盒隔离模式已关闭</b>\n已恢复完整系统与终端权限。"
    };
    send_html(bot, chat_id, status_str).await
}

async fn handle_project(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    proj_arg: &str,
) -> ResponseResult<()> {
    let trimmed = proj_arg.trim();
    if trimmed.is_empty() {
        let session = session_mgr.get_or_create(chat_id.0).await;
        let cur = if session.project.is_empty() {
            "默认工程 (未指定)"
        } else {
            &session.project
        };
        send_html(
            bot,
            chat_id,
            format!(
                "📁 <b>当前绑定的 Project:</b> <code>{}</code>\n\n使用 <code>/project &lt;名称&gt;</code> 可绑定指定工程，或 <code>/project clear</code> 重置。",
                text::escape_html(cur)
            ),
        )
        .await
    } else if trimmed == "clear" || trimmed == "reset" {
        session_mgr.set_project(chat_id.0, String::new()).await;
        send_html(
            bot,
            chat_id,
            "🔄 <b>已重置 Project 绑定</b>，恢复默认环境。",
        )
        .await
    } else if !is_safe_free_value(trimmed) {
        bot.send_message(
            chat_id,
            "⚠️ 无效的 Project 名称：不能以 - 开头、不能含控制字符，长度上限 128。",
        )
        .await?;
        Ok(())
    } else {
        session_mgr
            .set_project(chat_id.0, trimmed.to_string())
            .await;
        let escaped = text::escape_html(trimmed);
        send_html(
            bot,
            chat_id,
            format!(
                "📁 <b>已成功绑定 Project:</b> <code>{escaped}</code>\n后续任务将附加 <code>--project {escaped}</code> 参数。"
            ),
        )
        .await
    }
}

async fn handle_effort(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    effort_arg: &str,
) -> ResponseResult<()> {
    let trimmed = effort_arg.trim();
    if trimmed.is_empty() {
        let session = session_mgr.get_or_create(chat_id.0).await;
        let (card_text, keyboard) = build_effort_card(&session, None);
        bot.send_message(chat_id, card_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
        return Ok(());
    }

    match Effort::from_str(trimmed) {
        Ok(effort) => {
            let (synced_model, changed) = session_mgr
                .set_effort_and_sync_model(chat_id.0, effort)
                .await;
            let msg = if changed {
                format!(
                    "⚡ <b>思考力度已设置为:</b> <code>{}</code>\n🤖 <b>已同步切换同家族模型:</b> <code>{}</code>",
                    effort.as_str(),
                    text::escape_html(&synced_model)
                )
            } else if !supports_effort_flag(&synced_model) && !synced_model.is_empty() {
                format!(
                    "⚡ <b>默认思考力度已记录为:</b> <code>{}</code>\n<i>(当前模型 <code>{}</code> 内置固定推理预算)</i>",
                    effort.as_str(),
                    text::escape_html(&synced_model)
                )
            } else {
                format!("⚡ <b>思考力度已设置为:</b> <code>{}</code>", effort.as_str())
            };
            send_html(bot, chat_id, msg).await
        }
        Err(_) => {
            send_html(
                bot,
                chat_id,
                "⚠️ 无效的思考力度。支持的值为: <code>low</code>, <code>medium</code>, <code>high</code>, <code>xhigh</code>, <code>max</code>",
            )
            .await
        }
    }
}

async fn handle_mode(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    mode_arg: &str,
) -> ResponseResult<()> {
    let trimmed = mode_arg.trim();
    if trimmed.is_empty() {
        let session = session_mgr.get_or_create(chat_id.0).await;
        let (card_text, keyboard) = build_mode_card(&session, None);
        bot.send_message(chat_id, card_text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await?;
        return Ok(());
    }

    match Mode::from_str(trimmed) {
        Ok(mode) => {
            session_mgr.set_mode_enum(chat_id.0, mode).await;
            send_html(
                bot,
                chat_id,
                format!("🎯 <b>执行模式已切换为:</b> <code>{}</code>", mode.as_str()),
            )
            .await
        }
        Err(_) => {
            send_html(
                bot,
                chat_id,
                "⚠️ 无效的执行模式。支持的值为: <code>auto</code>, <code>accept-edits</code>, <code>plan</code>",
            )
            .await
        }
    }
}

async fn handle_agent(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    agent_arg: &str,
) -> ResponseResult<()> {
    let trimmed = agent_arg.trim();
    if trimmed.is_empty() {
        let (session, agents_str) = tokio::join!(session_mgr.get_or_create(chat_id.0), async {
            AgyExecutor::list_agents().await.unwrap_or_default()
        },);
        let parsed_agents = AgyExecutor::list_agents_parsed_cached();
        let current_agent = if session.agent.is_empty() {
            "默认 Agent"
        } else {
            &session.agent
        };
        let html = format!(
            "🕵️ <b>当前 Agent:</b> <code>{}</code>\n\n系统可用 Agent 列表：\n<pre><code>{}</code></pre>\n<i>发送 /agent &lt;名称&gt; 进行切换，或 /agent clear 重置。</i>",
            text::escape_html(current_agent),
            text::escape_html(&agents_str)
        );
        let mut rows = Vec::new();
        let mut row = Vec::with_capacity(2);
        for a in parsed_agents.iter().take(6) {
            row.push(InlineKeyboardButton::callback(
                format!("🕵️ {}", text::truncate_chars(a, 14)),
                encode_agent_callback(a),
            ));
            if row.len() == 2 {
                rows.push(std::mem::take(&mut row));
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
        rows.push(vec![
            InlineKeyboardButton::callback("♻️ 重置默认 Agent", "cp:clagent"),
            InlineKeyboardButton::callback("« 返回控制中心", "cp:home"),
        ]);
        bot.send_message(chat_id, html)
            .parse_mode(ParseMode::Html)
            .reply_markup(InlineKeyboardMarkup::new(rows))
            .await?;
        Ok(())
    } else if trimmed.eq_ignore_ascii_case("clear") || trimmed.eq_ignore_ascii_case("reset") {
        session_mgr.set_agent(chat_id.0, String::new()).await;
        send_html(bot, chat_id, "🕵️ <b>已重置为默认 Agent</b>").await
    } else if !is_safe_free_value(trimmed) {
        bot.send_message(
            chat_id,
            "⚠️ 无效的 Agent 名称：不能以 - 开头、不能含控制字符，长度上限 128。",
        )
        .await?;
        Ok(())
    } else {
        session_mgr.set_agent(chat_id.0, trimmed.to_string()).await;
        send_html(
            bot,
            chat_id,
            format!(
                "🕵️ <b>Agent 已切换为:</b> <code>{}</code>",
                text::escape_html(trimmed)
            ),
        )
        .await
    }
}

async fn handle_agents_list(bot: &Bot, chat_id: ChatId) {
    let agents_str = AgyExecutor::list_agents().await.unwrap_or_default();
    let html = format!(
        "🕵️ <b>Antigravity CLI 支持的 Agent 列表:</b>\n\n<pre><code>{}</code></pre>",
        text::escape_html(&agents_str)
    );
    send_html_chunks(bot, chat_id, &html).await;
}

async fn handle_system(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    sys_arg: &str,
) -> ResponseResult<()> {
    let trimmed = sys_arg.trim();
    if trimmed.is_empty() {
        let session = session_mgr.get_or_create(chat_id.0).await;
        if session.system_prompt.is_empty() {
            send_html(
                bot,
                chat_id,
                "🧠 <b>当前未设置自定义 System Prompt。</b>\n发送 <code>/system &lt;Prompt内容&gt;</code> 进行设置。",
            )
            .await
        } else {
            let html = format!(
                "🧠 <b>当前 System Prompt:</b>\n<code>{}</code>\n\n<i>发送 <code>/system clear</code> 可清空提示词。</i>",
                text::escape_html(&session.system_prompt)
            );
            send_html_chunks(bot, chat_id, &html).await;
            Ok(())
        }
    } else if trimmed == "clear" || trimmed == "reset" {
        session_mgr
            .set_system_prompt(chat_id.0, String::new())
            .await;
        send_html(bot, chat_id, "🧠 <b>System Prompt 已成功清空。</b>").await
    } else {
        session_mgr
            .set_system_prompt(chat_id.0, trimmed.to_string())
            .await;
        let html = format!(
            "🧠 <b>System Prompt 已更新:</b>\n<code>{}</code>",
            text::escape_html(trimmed)
        );
        send_html_chunks(bot, chat_id, &html).await;
        Ok(())
    }
}

async fn handle_file(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    path: &str,
) -> ResponseResult<()> {
    let path_trimmed = path.trim();
    if path_trimmed.is_empty() {
        return send_html(
            bot,
            chat_id,
            "⚠️ 请提供 VPS 文件的路径，如 <code>/file config.toml</code> 或 <code>/file ~/.config/agygram/config.toml</code>",
        )
        .await;
    }

    let session = session_mgr.get_or_create(chat_id.0).await;
    let file_path = resolve_user_path(path_trimmed, &session.workspace);

    match tokio::fs::metadata(&file_path).await {
        Ok(meta) if meta.is_file() => {
            if meta.len() > MAX_UPLOAD_BYTES {
                let mb = meta.len() as f64 / 1_048_576.0;
                return send_html(
                    bot,
                    chat_id,
                    format!(
                        "⚠️ <b>文件过大无法发送</b>\n<code>{}</code> 约 <code>{mb:.1} MB</code>，超过 Bot API 的 50 MB 上传上限。",
                        text::escape_html(&file_path.to_string_lossy())
                    ),
                )
                .await;
            }
            let caption =
                format_file_caption("📄", "来自 VPS 的文件", &file_path.to_string_lossy());
            bot.send_document(chat_id, teloxide::types::InputFile::file(&file_path))
                .caption(caption)
                .parse_mode(ParseMode::Html)
                .await?;
            Ok(())
        }
        Ok(_) | Err(_) => {
            send_html(
                bot,
                chat_id,
                format!(
                    "❌ 发送失败：VPS 上的文件 <code>{}</code> 不存在或无法读取。",
                    text::escape_html(path_trimmed)
                ),
            )
            .await
        }
    }
}

async fn handle_cancel(
    bot: &Bot,
    chat_id: ChatId,
    session_mgr: &SessionManager,
    runtime: &BotRuntime,
) -> ResponseResult<()> {
    runtime.cancel_chat_turn(chat_id.0).await;
    let (pid, was_running) = session_mgr.cancel_task(chat_id.0).await;
    if was_running {
        send_html(
            bot,
            chat_id,
            format!(
                "🛑 <b>已成功强行终止正在运行的任务</b> (PID: {})，会话锁已释放！",
                pid.unwrap_or(0)
            ),
        )
        .await
    } else {
        send_html(bot, chat_id, "ℹ️ 当前没有正在运行的任务。").await
    }
}

#[must_use]
pub(crate) fn effective_voice_reference<'a>(
    session_ref: Option<&'a str>,
    config_ref: Option<&'a str>,
) -> &'a str {
    session_ref.or(config_ref).unwrap_or("Fish Audio 默认音色")
}

#[must_use]
pub(crate) fn format_voice_id_reset_reply(config_ref: Option<&str>) -> String {
    let restored = effective_voice_reference(None, config_ref);
    format!(
        "✅ <b>已恢复默认音色:</b> <code>{}</code>",
        text::escape_html(restored)
    )
}

async fn handle_voice(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
    runtime: BotRuntime,
    text_arg: String,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;
    let trimmed = text_arg.trim();

    if trimmed.eq_ignore_ascii_case("on") || trimmed == "开启" {
        session_mgr.set_voice_mode(chat_id.0, true).await;
        send_html(
            &bot,
            chat_id,
            "🔊 <b>AI 语音回复模式已开启！</b>\n后续所有对话均会自动附带 TTS 语音回复。",
        )
        .await
    } else if trimmed.eq_ignore_ascii_case("off") || trimmed == "关闭" {
        session_mgr.set_voice_mode(chat_id.0, false).await;
        send_html(
            &bot,
            chat_id,
            "🔇 <b>AI 语音回复模式已关闭。</b>\n已恢复为极速纯文本流式输出模式。",
        )
        .await
    } else if trimmed.eq_ignore_ascii_case("toggle") || trimmed == "切换" {
        let is_on = session_mgr.toggle_voice_mode(chat_id.0).await;
        let status_str = if is_on {
            "已开启 (后续自动附带语音)"
        } else {
            "已关闭 (极速纯文本模式)"
        };
        send_html(
            &bot,
            chat_id,
            format!("🔊 <b>AI 语音回复模式:</b> <code>{status_str}</code>"),
        )
        .await
    } else if trimmed
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("id"))
    {
        let id_arg = trimmed
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim());
        if id_arg.is_empty() {
            let session = session_mgr.get_or_create(chat_id.0).await;
            let current_ref = effective_voice_reference(
                session.voice_reference_id.as_deref(),
                config.fish_reference_id.as_deref(),
            );
            send_html(
                &bot,
                chat_id,
                format!(
                    "🎙 <b>当前音色ID:</b> <code>{}</code>\n用法: <code>/voice id &lt;音色ID&gt;</code> 更换；<code>/voice id reset</code> 恢复默认",
                    text::escape_html(current_ref)
                ),
            )
            .await
        } else if id_arg.eq_ignore_ascii_case("reset")
            || id_arg.eq_ignore_ascii_case("default")
            || id_arg == "重置"
        {
            session_mgr.set_voice_reference(chat_id.0, None).await;
            send_html(
                &bot,
                chat_id,
                format_voice_id_reset_reply(config.fish_reference_id.as_deref()),
            )
            .await
        } else if crate::models::is_valid_voice_reference(id_arg) {
            session_mgr
                .set_voice_reference(chat_id.0, Some(id_arg.to_string()))
                .await;
            send_html(
                &bot,
                chat_id,
                format!(
                    "✅ <b>音色已切换:</b> <code>{}</code>\n下一轮语音回复生效。",
                    text::escape_html(id_arg)
                ),
            )
            .await
        } else {
            send_html(
                &bot,
                chat_id,
                "⚠️ 音色ID格式不正确：只允许字母、数字、- 和 _，长度 8-128。",
            )
            .await
        }
    } else if trimmed.is_empty() {
        let session = session_mgr.get_or_create(chat_id.0).await;
        let current_state = if session.voice_mode {
            "✅ 已开启"
        } else {
            "❌ 已关闭"
        };
        let current_ref = effective_voice_reference(
            session.voice_reference_id.as_deref(),
            config.fish_reference_id.as_deref(),
        );
        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("🔄 切换语音开关", "cp:tvoice"),
            InlineKeyboardButton::callback("« 返回控制中心", "cp:home"),
        ]]);
        bot.send_message(
            chat_id,
            format!(
                "🔊 <b>AI 语音回复功能 (Fish Audio TTS)</b>\n\n\
                • <b>当前常驻状态:</b> <code>{}</code>\n\
                • <b>当前音色ID:</b> <code>{}</code>\n\n\
                <b>使用方式：</b>\n\
                1. <code>/voice on</code> / <code>/voice off</code> / <code>/voice toggle</code> : 开启或关闭常驻语音回复\n\
                2. <code>/voice &lt;问题&gt;</code> : 单次请求获取语音回复 (如 <code>/voice 请介绍一下你自己</code>)\n\
                3. <code>/voice id &lt;音色ID&gt;</code> : 更换 Fish Audio 音色；<code>/voice id reset</code> 恢复配置默认",
                current_state,
                text::escape_html(current_ref)
            ),
        )
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
        Ok(())
    } else {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::utils::command::BotCommands;

    #[test]
    fn bot_commands_stay_within_telegram_limits() {
        let cmds = Command::bot_commands();
        assert!(cmds.len() <= 100);
        assert!(cmds.iter().any(|c| c.command == "/panel"));
        assert!(cmds.iter().any(|c| c.command == "/settings"));
        assert!(cmds.iter().any(|c| c.command == "/session"));
        assert!(cmds.iter().any(|c| c.command == "/model"));
        assert!(cmds.iter().any(|c| c.command == "/quota"));
        assert!(cmds.iter().any(|c| c.command == "/status"));
        for c in cmds {
            assert!(!c.description.is_empty() && c.description.chars().count() <= 256);
        }
    }

    #[test]
    fn voice_id_reset_reply_reflects_cleared_override_not_stale_session_value() {
        let with_cfg = format_voice_id_reset_reply(Some("cfg_voice_123"));
        assert!(with_cfg.contains("cfg_voice_123"));
        assert!(!with_cfg.contains("old_custom_voice"));

        let without_cfg = format_voice_id_reset_reply(None);
        assert!(without_cfg.contains("Fish Audio 默认音色"));

        assert_eq!(
            effective_voice_reference(Some("custom_id"), Some("cfg_voice_123")),
            "custom_id"
        );
        assert_eq!(
            effective_voice_reference(None, Some("cfg_voice_123")),
            "cfg_voice_123"
        );
        assert_eq!(effective_voice_reference(None, None), "Fish Audio 默认音色");
    }

    #[test]
    fn voice_id_reset_escapes_html_in_config_ref() {
        let reply = format_voice_id_reset_reply(Some("ref_<prod>&1"));
        assert!(reply.contains("ref_&lt;prod&gt;&amp;1"));
    }

    #[test]
    fn file_caption_respects_limit_for_long_paths() {
        let long_path = format!("/home/user/{}/report.txt", "sub/".repeat(400));
        let cap = format_file_caption("📄", "来自 VPS 的文件", &long_path);
        assert!(cap.chars().count() <= 1024);
        assert!(cap.ends_with("...</code>"));
    }
}
