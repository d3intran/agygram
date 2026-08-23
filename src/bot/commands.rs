use crate::bot::callbacks::{get_effort_keyboard, get_mode_keyboard, get_model_keyboard, normalize_model_name};
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
    #[command(description = "显示帮助信息与完整指令菜单")]
    Start,
    #[command(description = "显示帮助信息与完整指令菜单")]
    Help,
    #[command(description = "开启新会话 (重置上下文与统计)")]
    New,
    #[command(description = "重置当前会话 (别名 /new)")]
    Reset,
    #[command(description = "清空当前会话 (别名 /new)")]
    Clear,
    #[command(description = "恢复/接续历史对话 (/resume [<会话ID>])")]
    Resume(String),
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
    #[command(description = "查看已配置的 MCP 服务器列表")]
    Mcp(String),
    #[command(description = "查看已安装的 agy 插件列表")]
    Plugins,
    #[command(description = "查看 agy CLI 最新更新日志")]
    Changelog,
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
        Command::Start | Command::Help => {
            let ver = AgyExecutor::get_version().await;
            let help_text = format!(
                "🤖 <b>agygram (Antigravity CLI Daemon for Telegram)</b>\n\n\
                基于 <b>Antigravity CLI v{}</b> 的高性能 Telegram 交互守护进程。\n\n\
                <b>核心指令一览：</b>\n\
                • /new 或 /reset : 开启全新对话 (重置上下文与统计)\n\
                • /status : 查看工作目录、模型、Token遥测与快捷控制板\n\
                • /usage 或 /quota : 查看 CLI 原生 Token 与会话遥测面板\n\
                • /cd <code>&lt;path&gt;</code> : 切换主工作目录 (支持 <code>~/...</code>)\n\
                • /adddir <code>&lt;path&gt;</code> : 添加附加项目工作目录\n\
                • /cleardirs : 清空附加项目工作目录\n\
                • /dirs 或 /pwd : 查看当前绑定的所有工作目录\n\
                • /model <code>[&lt;name&gt;]</code> : 切换 AI 模型 (如 <code>3.7</code>, <code>sonnet</code>, <code>pro</code>)\n\
                • /models : 列出系统支持的全部 AI 模型列表\n\
                • /mcp : 查看已配置的 MCP 服务器列表\n\
                • /plugins : 查看已安装的 agy 插件列表\n\
                • /effort <code>[&lt;level&gt;]</code> : 设置思考力度 (<code>low</code> / <code>medium</code> / <code>high</code>)\n\
                • /mode <code>[&lt;mode&gt;]</code> : 切换模式 (<code>accept-edits</code> / <code>plan</code>)\n\
                • /agent <code>[&lt;name&gt;]</code> : 切换指定的 Agent 智能体\n\
                • /system <code>[&lt;prompt&gt;]</code> : 设置或清空自定义 System Prompt\n\
                • /file <code>&lt;path&gt;</code> : 传输 VPS 上的任何文件到 Telegram\n\
                • /voice <code>[on|off|toggle|&lt;prompt&gt;]</code> : 申鹤语音回复模式\n\
                • /cancel : 强行终止正在运行的任务并释放锁\n\n\
                <i>💡 支持直接发送<b>文本、代码文件、图片、语音</b>，全自动打字机流式输出！</i>",
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

        Command::Status => {
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
                "❌ 关闭 (支持 /voice <内容> 单次触发)"
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
                "🔇 关闭语音模式"
            } else {
                "🔊 开启语音模式"
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
                    InlineKeyboardButton::callback("🔄 重置会话", "cmd:new"),
                ],
            ]);

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
            let _ = arg;
            let mcp_str = AgyExecutor::list_mcp().await.unwrap_or_default();
            let text = format!(
                "🔌 <b>Antigravity CLI (MCP) 服务器配置:</b>\n\n<pre><code>{}</code></pre>\n<i>可在终端中使用 <code>agy mcp add/remove/list</code> 管理 MCP 服务器。</i>",
                TelegramFormatter::escape_html(&mcp_str)
            );
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::Html)
                .await?;
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
