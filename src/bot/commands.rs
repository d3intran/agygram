use crate::bot::callbacks::{
    build_onboarding_card, build_status_card, get_effort_keyboard, get_mode_keyboard,
    get_model_keyboard, normalize_model_name,
};
use crate::bot::formatter::TelegramFormatter;
use crate::bot::pipeline::message_handler;
use crate::config::Config;
use crate::executor::AgyExecutor;
use crate::session::{resolve_user_path, SessionManager};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "agygram 支持的全部指令:")]
pub enum Command {
    #[command(description = "显示欢迎卡片与极速交互指引")]
    Start,
    #[command(description = "显示帮助手册与完整指令一览")]
    Help,
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
    #[command(description = "查看当前会话的完整配置与交互控制板")]
    Status,
    #[command(description = "查看账号 Models & Token 详细额度与遥测面板")]
    Quota,
    #[command(description = "查看账号 Models & Token 详细额度与遥测面板")]
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
    #[command(description = "查看或切换 AI 模型 (/model [<模型名称或别名>])")]
    Model(String),
    #[command(description = "列出系统支持的所有 AI 模型")]
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
    #[command(description = "查看或设置思考力度 (/effort low | medium | high)")]
    Effort(String),
    #[command(description = "查看或切换执行模式 (/mode accept-edits | plan)")]
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
    #[command(description = "申鹤语音回复模式 (/voice on | off | toggle | <Prompt>)")]
    Voice(String),
}

pub async fn command_handler(
    bot: Bot,
    msg: Message,
    cmd: Command,
    config: Arc<Config>,
    session_mgr: Arc<SessionManager>,
) -> ResponseResult<()> {
    let user_id = msg.from.as_ref().map(|u| u.id.0).unwrap_or(0);
    if !config.is_user_allowed(user_id) {
        bot.send_message(
            msg.chat.id,
            format!("🚫 权限不足：用户 ID {} 未在授权名单中。", user_id),
        )
        .await?;
        return Ok(());
    }

    let chat_id = msg.chat.id.0;
    let session = session_mgr.get_or_create(chat_id).await;

    match cmd {
        Command::Start => {
            let (onboarding_text, keyboard) = build_onboarding_card();
            bot.send_message(msg.chat.id, onboarding_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
        }

        Command::Help => {
            let ver = AgyExecutor::get_version().await;
            let help_text = format!(
                "🤖 <b>agygram 指令手册 (v1.0 / Antigravity CLI v{})</b>\n\n\
                <b>🔄 会话与状态控制：</b>\n\
                • /status : 打开多级原地交互控制卡片\n\
                • /new 或 /reset : 开启全新对话 (重置上下文与统计)\n\
                • /resume <code>[&lt;ID&gt;]</code> : 恢复/接续历史对话\n\
                • /history : 查看 VPS 历史会话列表并一键接续\n\
                • /cancel : 紧急终止正在运行的任务并释放锁\n\
                • /usage 或 /quota : 查看 CLI Token 与额度遥测面板\n\n\
                <b>🧠 Brain 方案与生态：</b>\n\
                • /plan : 查看当前会话的架构方案 (implementation_plan.md)\n\
                • /walkthrough : 查看当前会话的执行报告 (walkthrough.md)\n\
                • /skills : 查看已挂载的 Antigravity 技能库\n\
                • /rules : 查看当前生效的全局与项目规则\n\
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
                • /file <code>&lt;path&gt;</code> : 传输 VPS 上的任何文件到 Telegram\n\n\
                <b>🤖 模型与语音设置：</b>\n\
                • /model <code>[&lt;name&gt;]</code> : 切换 AI 模型 (如 <code>3.7</code>, <code>sonnet</code>, <code>pro</code>)\n\
                • /models : 列出系统支持的全部 AI 模型列表\n\
                • /effort <code>[&lt;level&gt;]</code> : 设置思考力度 (<code>low</code> / <code>medium</code> / <code>high</code>)\n\
                • /mode <code>[&lt;mode&gt;]</code> : 切换模式 (<code>accept-edits</code> / <code>plan</code>)\n\
                • /agent <code>[&lt;name&gt;]</code> : 切换指定的 Agent 智能体\n\
                • /system <code>[&lt;prompt&gt;]</code> : 设置自定义 System Prompt\n\
                • /voice <code>[on|off|toggle|&lt;prompt&gt;]</code> : 申鹤语音回复模式\n\n\
                <i>💡 直接发送文本、代码文件、图片或语音，全自动打字机流式输出！</i>",
                TelegramFormatter::escape_html(&ver)
            );

            bot.send_message(msg.chat.id, help_text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::New | Command::Reset | Command::Clear => {
            session_mgr.reset_conversation(chat_id).await;
            bot.send_message(msg.chat.id, "🔄 <b>会话已重置</b>，上下文与 Token 遥测统计已恢复初始状态！")
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Resume(arg) => {
            let trimmed = arg.trim();
            if trimmed.is_empty() {
                session_mgr.resume_conversation(chat_id, None).await;
                bot.send_message(
                    msg.chat.id,
                    "🔄 <b>已开启历史对话接续模式 (-c/--continue)</b>！下一条消息将自动接续最近一次的 agy 上下文。",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                session_mgr.resume_conversation(chat_id, Some(trimmed.to_string())).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🔄 <b>已成功绑定历史会话 ID:</b> <code>{}</code>\n下一条消息将精确接续该上下文！",
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::History => {
            let recents = AgyExecutor::list_recent_conversations(8).await;
            if recents.is_empty() {
                bot.send_message(msg.chat.id, "📜 <b>暂未发现历史会话。</b>")
                    .parse_mode(ParseMode::Html)
                    .await?;
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
                bot.send_message(msg.chat.id, text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(InlineKeyboardMarkup::new(rows))
                    .await?;
            }
        }

        Command::Plan => {
            if let Some(ref conv_id) = session.conversation_id {
                if let Some(plan_text) = AgyExecutor::read_implementation_plan(conv_id).await {
                    let html = TelegramFormatter::to_telegram_html(&plan_text);
                    let chunks = TelegramFormatter::split_html_chunks(&html, 4000);
                    for chunk in chunks {
                        let _ = bot.send_message(msg.chat.id, chunk).parse_mode(ParseMode::Html).await;
                    }
                } else {
                    bot.send_message(msg.chat.id, "📋 <b>当前会话尚未生成 implementation_plan.md 方案文件。</b>").parse_mode(ParseMode::Html).await?;
                }
            } else {
                bot.send_message(msg.chat.id, "📋 <b>当前为新建会话，尚未关联历史会话 ID。</b>").parse_mode(ParseMode::Html).await?;
            }
        }

        Command::Walkthrough => {
            if let Some(ref conv_id) = session.conversation_id {
                if let Some(wt_text) = AgyExecutor::read_walkthrough(conv_id).await {
                    let html = TelegramFormatter::to_telegram_html(&wt_text);
                    let chunks = TelegramFormatter::split_html_chunks(&html, 4000);
                    for chunk in chunks {
                        let _ = bot.send_message(msg.chat.id, chunk).parse_mode(ParseMode::Html).await;
                    }
                } else {
                    bot.send_message(msg.chat.id, "📝 <b>当前会话尚未生成 walkthrough.md 总结文件。</b>").parse_mode(ParseMode::Html).await?;
                }
            } else {
                bot.send_message(msg.chat.id, "📝 <b>当前为新建会话，尚未关联历史会话 ID。</b>").parse_mode(ParseMode::Html).await?;
            }
        }

        Command::Skills => {
            let skills_str = AgyExecutor::list_skills().await;
            bot.send_message(msg.chat.id, format!("🧩 <b>Antigravity 技能库 (Skills)：</b>\n\n{}", skills_str))
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Rules => {
            let rules_str = AgyExecutor::list_rules(&session.workspace).await;
            bot.send_message(msg.chat.id, format!("🛡️ <b>Antigravity 规则配置 (Rules)：</b>\n\n{}", rules_str))
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Status => {
            let (status_text, keyboard) = build_status_card(chat_id, &session);
            bot.send_message(msg.chat.id, status_text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
        }

        Command::Quota | Command::Usage => {
            let turn_count = session.turn_count;
            let in_tok = session.total_input_tokens;
            let out_tok = session.total_output_tokens;
            let think_tok = session.total_thinking_tokens;
            let cache_tok = session.total_cache_read_tokens;
            let total_tok = in_tok + out_tok;
            let total_dur = session.total_duration_seconds;
            let avg_dur = if turn_count > 0 { total_dur / turn_count as f64 } else { 0.0 };

            match crate::quota::QuotaFetcher::format_models_and_quota().await {
                Ok(quota_html) => {
                    let full_panel = format!(
                        "{}\n\n\
                        ━━━━━━━━━━━━━━━━━━\n\
                        📊 <b>Session Telemetry (当前会话遥测)</b>\n\
                        • <b>对话完成</b>: <code>{} 轮</code> (总耗时: <code>{:.1}s</code>, 平均: <code>{:.1}s/轮</code>)\n\
                        • <b>会话 Token</b>: <code>{} tokens</code> (Prompt: <code>{}</code>, Output: <code>{}</code>, Thinking: <code>{}</code>, Cache: <code>{}</code>)",
                        quota_html,
                        turn_count,
                        total_dur,
                        avg_dur,
                        total_tok,
                        in_tok,
                        out_tok,
                        think_tok,
                        cache_tok
                    );
                    bot.send_message(msg.chat.id, full_panel)
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
                Err(err) => {
                    let input_chars = session.total_input_chars;
                    let output_chars = session.total_output_chars;
                    let elapsed_secs = session.session_start_time.elapsed().as_secs();
                    let duration_str = format!(
                        "{}h {}m {}s",
                        elapsed_secs / 3600,
                        (elapsed_secs % 3600) / 60,
                        elapsed_secs % 60
                    );
                    let current_model = if session.model.is_empty() {
                        "系统默认 (Gemini 3.7 Flash)".to_string()
                    } else {
                        session.model.clone()
                    };
                    let ver = AgyExecutor::get_version().await;

                    let panel = format!(
                        "⚠️ <i>实时 Quota API 获取异常: {}</i>\n\n\
                        🚀 <b>Antigravity CLI (agy) Models & Telemetry Panel</b>\n\n\
                        • <b>CLI 版本</b>: <code>Antigravity CLI v{}</code>\n\
                        • <b>输出流</b>: <code>stream-json</code>\n\
                        • <b>当前 AI 模型</b>: <code>{}</code>\n\
                        • <b>思考力度 (Effort)</b>: <code>{}</code>\n\
                        • <b>执行模式 (Mode)</b>: <code>{}</code>\n\
                        • <b>主工作目录</b>: <code>{}</code>\n\n\
                        📊 <b>实时会话遥测 (Session Telemetry)</b>\n\
                        • <b>对话完成轮数</b>: <code>{} 轮</code>\n\
                        • <b>总消耗 Tokens</b>: <code>{} tokens</code>\n\
                          ├ <b>Prompt 输入</b>: <code>{} tokens</code> (<code>{} chars</code>)\n\
                          ├ <b>Completion 输出</b>: <code>{} tokens</code> (<code>{} chars</code>)\n\
                          ├ <b>Thinking 思考链</b>: <code>{} tokens</code>\n\
                          └ <b>Cache 缓存命中</b>: <code>{} tokens</code>\n\
                        • <b>模型推导总耗时</b>: <code>{:.2}s (平均 {:.2}s/轮)</code>\n\
                        • <b>会话在线时长</b>: <code>{}</code>\n\
                        • <b>上下文续传 (-c)</b>: <code>✅ 自动断点接续中</code>",
                        TelegramFormatter::escape_html(&err.to_string()),
                        TelegramFormatter::escape_html(&ver),
                        TelegramFormatter::escape_html(&current_model),
                        TelegramFormatter::escape_html(&session.effort),
                        TelegramFormatter::escape_html(&session.mode),
                        TelegramFormatter::escape_html(&session.workspace),
                        turn_count,
                        total_tok,
                        in_tok,
                        input_chars,
                        out_tok,
                        output_chars,
                        think_tok,
                        cache_tok,
                        total_dur,
                        avg_dur,
                        duration_str
                    );

                    bot.send_message(msg.chat.id, panel)
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
            }
        }

        Command::Cd(path) => {
            let path_trimmed = path.trim();
            if path_trimmed.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "⚠️ 请提供工作目录路径，例如 <code>/cd /home/jinc</code> 或 <code>/cd ~/agygram</code>",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                let target_path = resolve_user_path(path_trimmed, &session.workspace);
                if target_path.exists() && target_path.is_dir() {
                    let path_str = target_path.to_string_lossy().to_string();
                    session_mgr
                        .set_workspace(chat_id, path_str.clone())
                        .await;
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "📁 <b>主工作目录已成功切换为:</b> <code>{}</code>",
                            TelegramFormatter::escape_html(&path_str)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                } else if std::fs::create_dir_all(&target_path).is_ok() {
                    let path_str = target_path.to_string_lossy().to_string();
                    session_mgr
                        .set_workspace(chat_id, path_str.clone())
                        .await;
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "📁 <b>目标目录不存在，已自动新建并切换至:</b> <code>{}</code>",
                            TelegramFormatter::escape_html(&path_str)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                } else {
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "❌ 无法切换工作目录: 路径 <code>{}</code> 无效或权限不足。",
                            TelegramFormatter::escape_html(path_trimmed)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            }
        }

        Command::AddDir(path) => {
            let path_trimmed = path.trim();
            if path_trimmed.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "⚠️ 请提供项目目录路径，例如 <code>/adddir /home/jinc/project</code> 或 <code>/adddir ~/project</code>",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                let target_path = resolve_user_path(path_trimmed, &session.workspace);
                let path_str = target_path.to_string_lossy().to_string();
                session_mgr
                    .add_extra_dir(chat_id, path_str.clone())
                    .await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "📂 <b>已添加项目工作目录:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(&path_str)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::ClearDirs => {
            session_mgr.clear_extra_dirs(chat_id).await;
            bot.send_message(
                msg.chat.id,
                "🧹 <b>已清空所有添加的额外项目工作目录！</b>",
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }

        Command::Dirs | Command::Pwd => {
            let mut text = format!(
                "📂 <b>当前绑定工作目录</b>\n\n• <b>主目录:</b> <code>{}</code>",
                TelegramFormatter::escape_html(&session.workspace)
            );
            if !session.extra_dirs.is_empty() {
                text.push_str("\n\n<b>附加项目目录:</b>\n");
                for d in &session.extra_dirs {
                    text.push_str(&format!(
                        "• <code>{}</code>\n",
                        TelegramFormatter::escape_html(d)
                    ));
                }
            }
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Model(model_arg) => {
            let trimmed = model_arg.trim();
            if trimmed.is_empty() {
                let keyboard = get_model_keyboard();
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🤖 <b>当前 AI 模型:</b> <code>{}</code>\n请选择需要调用的模型：",
                        TelegramFormatter::escape_html(if session.model.is_empty() {
                            "系统默认 (Gemini 3.7 Flash)"
                        } else {
                            &session.model
                        })
                    ),
                )
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
            } else {
                let target_model = normalize_model_name(trimmed);
                session_mgr.set_model(chat_id, target_model.to_string()).await;
                let display_name = if target_model.is_empty() { "系统默认" } else { &target_model };
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🤖 <b>AI 模型已切换为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(display_name)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::Models => {
            let models_str = AgyExecutor::list_models().await.unwrap_or_default();
            let text = format!(
                "📋 <b>Antigravity CLI 支持的模型列表:</b>\n\n<pre><code>{}</code></pre>\n<i>使用 /model &lt;名称或别名&gt; 进行切换。</i>",
                TelegramFormatter::escape_html(&models_str)
            );
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Mcp(arg) => {
            let trimmed = arg.trim();
            let args: Vec<&str> = if trimmed.is_empty() {
                vec!["list"]
            } else {
                trimmed.split_whitespace().collect()
            };
            let wait_msg = bot
                .send_message(msg.chat.id, "⚙️ <b>正在查询 MCP 服务配置...</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            match AgyExecutor::run_mcp_cmd(&args).await {
                Ok(out) => {
                    let text = if out.trim().is_empty() {
                        "暂无配置的 MCP 服务器。".to_string()
                    } else {
                        out
                    };
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "⚙️ <b>MCP 服务器列表/操作结果:</b>\n\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&text)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
                Err(e) => {
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "❌ <b>MCP 查询失败:</b>\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&e.to_string())
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            }
        }

        Command::Plugin(arg) => {
            let trimmed = arg.trim();
            let args: Vec<&str> = if trimmed.is_empty() {
                vec!["list"]
            } else {
                trimmed.split_whitespace().collect()
            };
            let wait_msg = bot
                .send_message(msg.chat.id, "🧩 <b>正在查询 agy 插件生态...</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            match AgyExecutor::run_plugin_cmd(&args).await {
                Ok(out) => {
                    let text = if out.trim().is_empty() {
                        "暂无安装的插件。".to_string()
                    } else {
                        out
                    };
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "🧩 <b>agy 插件生态列表/操作结果:</b>\n\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&text)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
                Err(e) => {
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "❌ <b>插件查询失败:</b>\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&e.to_string())
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            }
        }

        Command::Plugins => {
            let plugins_str = AgyExecutor::list_plugins().await.unwrap_or_default();
            let text = format!(
                "🧩 <b>已安装的 agy 插件列表:</b>\n\n<pre><code>{}</code></pre>",
                TelegramFormatter::escape_html(&plugins_str)
            );
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Update => {
            let wait_msg = bot
                .send_message(msg.chat.id, "🔄 <b>正在执行 agy update 检查并升级 CLI...</b>")
                .parse_mode(ParseMode::Html)
                .await?;
            match AgyExecutor::run_update().await {
                Ok(out) => {
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "✅ <b>CLI 更新执行完毕:</b>\n\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&out)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
                Err(e) => {
                    bot.edit_message_text(
                        msg.chat.id,
                        wait_msg.id,
                        format!(
                            "❌ <b>CLI 更新失败:</b>\n<pre><code>{}</code></pre>",
                            TelegramFormatter::escape_html(&e.to_string())
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            }
        }

        Command::Sandbox(sandbox_arg) => {
            let trimmed = sandbox_arg.trim().to_lowercase();
            let enabled = match trimmed.as_str() {
                "on" | "true" | "1" | "enable" => {
                    session_mgr.set_sandbox(chat_id, true).await;
                    true
                }
                "off" | "false" | "0" | "disable" => {
                    session_mgr.set_sandbox(chat_id, false).await;
                    false
                }
                _ => session_mgr.toggle_sandbox(chat_id).await,
            };
            let status_str = if enabled {
                "🛡️ <b>沙盒隔离模式已开启</b>\n后续任务将附加 <code>--sandbox</code> 参数运行。"
            } else {
                "⚡ <b>沙盒隔离模式已关闭</b>\n已恢复完整系统与终端权限。"
            };
            bot.send_message(msg.chat.id, status_str)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Project(proj_arg) => {
            let trimmed = proj_arg.trim();
            if trimmed.is_empty() {
                let cur = if session.project.is_empty() {
                    "默认工程 (未指定)"
                } else {
                    &session.project
                };
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "📁 <b>当前绑定的 Project:</b> <code>{}</code>\n\n使用 <code>/project &lt;名称&gt;</code> 可绑定指定工程，或 <code>/project clear</code> 重置。",
                        TelegramFormatter::escape_html(cur)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else if trimmed == "clear" || trimmed == "reset" {
                session_mgr.set_project(chat_id, String::new()).await;
                bot.send_message(
                    msg.chat.id,
                    "🔄 <b>已重置 Project 绑定</b>，恢复默认环境。",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                session_mgr.set_project(chat_id, trimmed.to_string()).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "📁 <b>已成功绑定 Project:</b> <code>{}</code>\n后续任务将附加 <code>--project {}</code> 参数。",
                        TelegramFormatter::escape_html(trimmed),
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::Changelog => {
            let changelog_str = AgyExecutor::show_changelog().await.unwrap_or_default();
            let text = format!(
                "📢 <b>Antigravity CLI 更新日志:</b>\n\n{}",
                TelegramFormatter::to_telegram_html(&changelog_str)
            );
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
        }

        Command::Effort(effort_arg) => {
            let trimmed = effort_arg.trim();
            if trimmed.is_empty() {
                let keyboard = get_effort_keyboard();
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "⚡ <b>当前思考力度 (Effort):</b> <code>{}</code>\n请选择级别：",
                        TelegramFormatter::escape_html(&session.effort)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
            } else {
                session_mgr.set_effort(chat_id, trimmed.to_string()).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "⚡ <b>思考力度已设置为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::Mode(mode_arg) => {
            let trimmed = mode_arg.trim();
            if trimmed.is_empty() {
                let keyboard = get_mode_keyboard();
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🎯 <b>当前执行模式:</b> <code>{}</code>\n请选择模式：",
                        TelegramFormatter::escape_html(&session.mode)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
            } else {
                session_mgr.set_mode(chat_id, trimmed.to_string()).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🎯 <b>执行模式已切换为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::Agent(agent_arg) => {
            let trimmed = agent_arg.trim();
            if trimmed.is_empty() {
                let agents_str = AgyExecutor::list_agents().await.unwrap_or_default();
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🕵️ <b>当前 Agent:</b> <code>{}</code>\n\n系统可用 Agent 列表：\n<pre><code>{}</code></pre>\n<i>发送 /agent &lt;名称&gt; 进行切换。</i>",
                        TelegramFormatter::escape_html(if session.agent.is_empty() { "默认 Agent" } else { &session.agent }),
                        TelegramFormatter::escape_html(&agents_str)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                session_mgr.set_agent(chat_id, trimmed.to_string()).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🕵️ <b>Agent 已切换为:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::Agents => {
            let agents_str = AgyExecutor::list_agents().await.unwrap_or_default();
            bot.send_message(
                msg.chat.id,
                format!(
                    "🕵️ <b>Antigravity CLI 支持的 Agent 列表:</b>\n\n<pre><code>{}</code></pre>",
                    TelegramFormatter::escape_html(&agents_str)
                ),
            )
            .parse_mode(ParseMode::Html)
            .await?;
        }

        Command::System(sys_arg) => {
            let trimmed = sys_arg.trim();
            if trimmed.is_empty() {
                if session.system_prompt.is_empty() {
                    bot.send_message(
                        msg.chat.id,
                        "🧠 <b>当前未设置自定义 System Prompt。</b>\n发送 <code>/system &lt;Prompt内容&gt;</code> 进行设置。",
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                } else {
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "🧠 <b>当前 System Prompt:</b>\n<code>{}</code>\n\n<i>发送 <code>/system clear</code> 可清空提示词。</i>",
                            TelegramFormatter::escape_html(&session.system_prompt)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            } else if trimmed == "clear" || trimmed == "reset" {
                session_mgr.set_system_prompt(chat_id, "".to_string()).await;
                bot.send_message(msg.chat.id, "🧠 <b>System Prompt 已成功清空。</b>")
                    .parse_mode(ParseMode::Html)
                    .await?;
            } else {
                session_mgr
                    .set_system_prompt(chat_id, trimmed.to_string())
                    .await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🧠 <b>System Prompt 已更新:</b>\n<code>{}</code>",
                        TelegramFormatter::escape_html(trimmed)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            }
        }

        Command::File(path) => {
            let path_trimmed = path.trim();
            if path_trimmed.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "⚠️ 请提供 VPS 文件的路径，如 <code>/file config.toml</code> 或 <code>/file ~/.config/agygram/config.toml</code>",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                let file_path = resolve_user_path(path_trimmed, &session.workspace);
                if file_path.exists() && file_path.is_file() {
                    bot.send_document(
                        msg.chat.id,
                        teloxide::types::InputFile::file(&file_path),
                    )
                    .caption(format!(
                        "📄 <b>来自 VPS 的文件:</b> <code>{}</code>",
                        TelegramFormatter::escape_html(&file_path.to_string_lossy())
                    ))
                    .parse_mode(ParseMode::Html)
                    .await?;
                } else {
                    bot.send_message(
                        msg.chat.id,
                        format!(
                            "❌ 发送失败：VPS 上的文件 <code>{}</code> 不存在或无法读取。",
                            TelegramFormatter::escape_html(path_trimmed)
                        ),
                    )
                    .parse_mode(ParseMode::Html)
                    .await?;
                }
            }
        }

        Command::Cancel => {
            let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
            if was_running {
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🛑 <b>已成功强行终止正在运行的任务</b> (PID: {})，会话锁已释放！",
                        pid.unwrap_or(0)
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else {
                bot.send_message(msg.chat.id, "ℹ️ 当前没有正在运行的任务。")
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
        }

        Command::Voice(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("on") || trimmed == "开启" {
                session_mgr.set_voice_mode(chat_id, true).await;
                bot.send_message(
                    msg.chat.id,
                    "🔊 <b>申鹤语音模式已开启！</b>\n后续所有对话均会自动附带申鹤语音回复。",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else if trimmed.eq_ignore_ascii_case("off") || trimmed == "关闭" {
                session_mgr.set_voice_mode(chat_id, false).await;
                bot.send_message(
                    msg.chat.id,
                    "🔇 <b>申鹤语音模式已关闭。</b>\n已恢复为极速纯文本流式输出模式。",
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else if trimmed.eq_ignore_ascii_case("toggle") || trimmed == "切换" {
                let is_on = session_mgr.toggle_voice_mode(chat_id).await;
                let status_str = if is_on { "已开启 (后续自动附带语音)" } else { "已关闭 (极速纯文本模式)" };
                bot.send_message(
                    msg.chat.id,
                    format!("🔊 <b>申鹤语音模式:</b> <code>{}</code>", status_str),
                )
                .parse_mode(ParseMode::Html)
                .await?;
            } else if trimmed.is_empty() {
                let current_state = if session.voice_mode { "✅ 已开启" } else { "❌ 已关闭" };
                let keyboard = InlineKeyboardMarkup::new(vec![vec![
                    InlineKeyboardButton::callback("🔄 切换语音开关", "cmd:toggle_voice"),
                ]]);
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "🔊 <b>申鹤语音回复功能</b>\n\n\
                        • <b>当前常驻状态:</b> <code>{}</code>\n\n\
                        <b>使用方式：</b>\n\
                        1. <code>/voice on</code> / <code>/voice off</code> / <code>/voice toggle</code> : 开启或关闭常驻语音回复\n\
                        2. <code>/voice &lt;问题&gt;</code> : 单次请求获取语音回复 (如 <code>/voice 请介绍一下你自己</code>)\n\n\
                        <i>未开启且未带 /voice 的普通对话将以极速纯文本流式输出。</i>",
                        current_state
                    ),
                )
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .await?;
            } else {
                return message_handler(bot, msg, config, session_mgr).await;
            }
        }
    }

    Ok(())
}
