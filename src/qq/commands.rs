//! QQ Bot slash command handlers, shared-group RBAC enforcement, native `/compact`,
//! strict `/file` path validation, and unknown-command filtering.

use super::QqRuntime;
use super::client::{QqClient, QqTarget};
use super::context;
use super::delivery::reply;
use super::handler::IncomingMeta;
use super::media::deliver_single_file;
use crate::artifacts::validate_explicit_file_path;
use crate::config::Config;
use crate::executor::AgyExecutor;
use crate::models::{
    Effort, Mode, QuotaPool, SYSTEM_DEFAULT_MODEL_LABEL, effective_cli_effort, is_safe_cli_value,
    is_safe_free_value, is_valid_voice_reference, list_model_families, model_display_label,
    refresh_models_now, resolve_model_input, trigger_background_model_refresh_if_stale,
};
use crate::quota::{QuotaFetcher, QuotaOverview};
use crate::session::{ChatSession, SessionManager, resolve_user_path};
use crate::text::{looks_sensitive, truncate_chars};
use anyhow::Result;
use std::borrow::Cow;
use std::fmt::Write;
use std::path::PathBuf;
use tracing::{info, warn};

/// Built-in Antigravity CLI native slash commands allowed to pass straight through to `agy`.
const NATIVE_AGY_SLASH_COMMANDS: &[&str] = &[
    "/swarm",
    "/goal",
    "/plan",
    "/grill-me",
    "/schedule",
    "/learn",
    "/boost",
    "/compact",
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandOutcome<'a> {
    Handled,
    ExecutePrompt {
        prompt_text: Cow<'a, str>,
        voice_requested: bool,
    },
}

/// Returns `true` if `cmd` (with normalized lowercase leading `/`) requires QQ admin privileges
/// when invoked inside a shared group session (`qq_group_session_mode = "group"`).
#[must_use]
pub(crate) fn requires_shared_group_admin(cmd: &str, arg: &str) -> bool {
    match cmd {
        "/file" | "/history" | "/resume" | "/rules" | "/cd" | "/adddir" | "/cleardirs"
        | "/sandbox" | "/project" | "/remote" => true,
        "/workspace" | "/system" => !arg.trim().is_empty(),
        _ => false,
    }
}

pub(crate) async fn handle_sender_identity_command(
    client: &QqClient,
    target: QqTarget<'_>,
    chat_id: i64,
    sender_openid: &str,
    content: &str,
    msg_id: &str,
) -> Result<bool> {
    if !content.starts_with('/') {
        return Ok(false);
    }
    let cmd_raw = content.split_whitespace().next().unwrap_or_default();
    if !cmd_raw.eq_ignore_ascii_case("/nick")
        && !cmd_raw.eq_ignore_ascii_case("/whoami")
        && !cmd_raw.eq_ignore_ascii_case("/name")
    {
        return Ok(false);
    }

    let arg = content
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim());
    let tag = context::member_short_tag(sender_openid);
    info!(
        chat_id,
        cmd = %cmd_raw.to_ascii_lowercase(),
        speaker = %tag,
        has_arg = !arg.is_empty(),
        "Handled QQ sender identity command"
    );

    if arg.is_empty() || cmd_raw.eq_ignore_ascii_case("/whoami") {
        let current = context::get_member_nickname(sender_openid);
        let status = match current {
            Some(nick) => format!("当前已绑定称呼: **「{nick}」**"),
            None => "当前未绑定自定义称呼".to_string(),
        };
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🪪 **你的身份标识**: `{tag}`（{status}）\n\
                • 发送 `/nick <称呼>` 绑定固定称呼（例如 `/nick 阿杰`，上限 16 字，括号内指令与控制字符会被自动过滤），群聊中 AI 将按名字区分你与其他群友\n\
                • 发送 `/nick clear` 清除自定义称呼"
            ),
        )
        .await?;
        return Ok(true);
    }

    if arg.eq_ignore_ascii_case("clear")
        || arg.eq_ignore_ascii_case("reset")
        || arg.eq_ignore_ascii_case("off")
    {
        context::set_member_nickname_async(sender_openid, None).await;
        reply(
            client,
            target,
            msg_id,
            &format!("🧹 已清除自定义称呼，恢复默认群聊标识 `{tag}`。"),
        )
        .await?;
        return Ok(true);
    }

    match context::sanitize_nickname(arg) {
        Ok(clean) => {
            context::set_member_nickname_async(sender_openid, Some(clean.clone())).await;
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "✅ 已将你的身份称呼绑定为 **「{clean}」** (`{tag}`)！后续群聊中 AI 将自动区分你的身份。"
                ),
            )
            .await?;
        }
        Err(reason) => {
            reply(client, target, msg_id, &format!("⚠️ {reason}")).await?;
        }
    }
    Ok(true)
}

fn build_qq_help_text() -> String {
    format!(
        "🚀 **agygram QQ 机器人已就绪** (v{})\n\n\
        **🎛️ 控制板与会话：**\n\
        • `/panel` 或 `/status` - 查看控制板、实时额度与快捷序号\n\
        • `/new` / `/reset` / `/clear` - 中断任务并重置会话上下文\n\
        • `/compact [<侧重点>]` - 原生压缩上下文检查点 (CHECKPOINT)\n\
        • `/resume [<ID>]` / `/history` - 查看或恢复历史会话\n\
        • `/nick [<称呼>|clear]` - 绑定群聊个人称呼（让 AI 在群里精准认识你）\n\
        • `/quota` 或 `/usage` - 查询 Google Cloud 实时模型额度\n\
        • `/cancel` - 中断当前排队或正在执行的任务\n\n\
        **🤖 模型与执行参数：**\n\
        • `/model [<序号|名称|别名>]` - 切换模型 (如 `1`, `neon`, `h2`, `carbon`, `orch`, `opus`, `reset`)\n\
        • `/models` - 查看完整分组模型与序号列表\n\
        • `/effort [<级别>]` - 思考力度 (`low` / `medium` / `high` / `xhigh` / `max`)\n\
        • `/mode [<模式>]` - 执行模式 (`auto` / `accept-edits` / `plan`)\n\
        • `/thinking [on|off|default]` - 控制思考过程展示\n\
        • `/remote [on|off|toggle]` - 开关云端远程代理 (`--remote-agent`)\n\
        • `/agent [<名称>]` / `/agents` - 切换或查看 Agent\n\
        • `/sandbox [on|off|toggle]` - 开关沙盒隔离\n\
        • `/project [<名称>|clear]` - 绑定或清空 Project\n\
        • `/system [<Prompt>|clear]` - 设置或清空自定义提示词\n\n\
        **📂 工作区与文件：**\n\
        • `/workspace [<路径>]` - 查看或切换主目录 (兼容 `/cd`, `/pwd`, `/dirs`)\n\
        • `/adddir <路径>` / `/cleardirs` - 添加或清空附加项目目录\n\
        • `/file <路径>` - 发送工作区内图片/音视频/文件或文本预览\n\n\
        **🧩 生态与语音：**\n\
        • `/skills` / `/rules` - 查看 Antigravity 技能库与规则\n\
        • `/voice [on|off|toggle|id <ID>|<问题>]` - 语音开关或单次语音回复\n\n\
        直接发送任何开发需求、问题或图片/文件，即可唤起 Antigravity CLI 执行。",
        env!("CARGO_PKG_VERSION")
    )
}

fn build_unknown_command_guide(cmd_raw: &str) -> String {
    format!(
        "❓ 未识别的指令：`{cmd_raw}`\n\n\
        常用指令速查：\n\
        • `/panel` `/status` `/quota` — 控制板、状态与额度\n\
        • `/new` `/compact` `/cancel` — 新建、压缩或中断任务\n\
        • `/model` `/effort` `/mode` `/thinking` `/remote` — 模型与参数\n\
        • `/nick` `/voice` `/skills` `/help` — 群聊称呼、语音、技能与完整帮助"
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_qq_command<'a>(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    runtime: &QqRuntime,
    target: QqTarget<'_>,
    chat_id: i64,
    content: &'a str,
    msg_id: &str,
    meta: IncomingMeta<'_>,
) -> Result<CommandOutcome<'a>> {
    if !content.starts_with('/') {
        return Ok(CommandOutcome::ExecutePrompt {
            prompt_text: Cow::Borrowed(content),
            voice_requested: false,
        });
    }

    let cmd_raw = content.split_whitespace().next().unwrap_or_default();
    let cmd = cmd_raw.to_ascii_lowercase();
    // Split on the original text so `/MODEL opus` preserves argument casing.
    let arg = content
        .split_once(char::is_whitespace)
        .map_or("", |(_, rest)| rest.trim());

    let speaker_tag = context::member_short_tag(meta.sender_openid);

    // P0: Shared-group RBAC guard for sensitive/mutating commands.
    if meta.is_shared_group
        && requires_shared_group_admin(&cmd, arg)
        && !config.is_qq_admin(meta.sender_openid)
    {
        warn!(
            chat_id,
            cmd = %cmd,
            speaker = %speaker_tag,
            "Blocked non-admin QQ shared-group command"
        );
        reply(
            client,
            target,
            msg_id,
            &format!("🔒 权限不足：当前为共享群聊，指令 `{cmd}` 仅限管理员操作。"),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    match cmd.as_str() {
        "/start" | "/help" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ command");
            reply(client, target, msg_id, &build_qq_help_text()).await?;
            Ok(CommandOutcome::Handled)
        }
        "/panel" | "/status" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ command");
            handle_panel_command(client, config, session_mgr, target, chat_id, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/new" | "/reset" | "/clear" => {
            let cancelled_turns = runtime.cancel_chat(chat_id);
            let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
            session_mgr.reset_conversation(chat_id).await;
            info!(
                chat_id,
                cmd = %cmd,
                speaker = %speaker_tag,
                cancelled_turns,
                ?pid,
                was_running,
                "Handled QQ reset command"
            );
            reply(
                client,
                target,
                msg_id,
                "🔄 会话上下文已重置，已开启全新对话轮次。",
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }
        "/compact" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_focus = !arg.is_empty(), "Handled QQ /compact command");
            handle_compact_command(client, config, session_mgr, target, chat_id, arg, msg_id).await
        }
        "/resume" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /resume command");
            handle_resume_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/history" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /history command");
            handle_history_command(client, target, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/usage" | "/quota" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /quota command");
            match QuotaFetcher::format_models_and_quota_plain().await {
                Ok(plain_text) => {
                    reply(client, target, msg_id, &plain_text).await?;
                }
                Err(err) => {
                    let err_msg = format!("⚠️ 实时额度获取失败: {err:#}");
                    reply(client, target, msg_id, &err_msg).await?;
                }
            }
            Ok(CommandOutcome::Handled)
        }
        "/models" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /models command");
            handle_models_list_command(client, session_mgr, target, chat_id, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/model" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /model command");
            handle_model_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/skills" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /skills command");
            let skills = AgyExecutor::list_skills_plain().await;
            reply(
                client,
                target,
                msg_id,
                &format!("🧩 Antigravity 技能库 (Skills)：\n\n{skills}"),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }
        "/rules" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /rules command");
            let session = session_mgr.get_or_create(chat_id).await;
            let rules = AgyExecutor::list_rules_plain(&session.workspace).await;
            reply(
                client,
                target,
                msg_id,
                &format!("🛡️ Antigravity 规则配置 (Rules)：\n\n{rules}"),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }
        "/effort" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /effort command");
            handle_effort_command(client, config, session_mgr, target, chat_id, arg, msg_id)
                .await?;
            Ok(CommandOutcome::Handled)
        }
        "/mode" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /mode command");
            handle_mode_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/thinking" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /thinking command");
            handle_thinking_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/remote" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /remote command");
            handle_remote_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/agent" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /agent command");
            handle_agent_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/agents" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /agents command");
            let agents = AgyExecutor::list_agents().await.unwrap_or_default();
            let body = if agents.trim().is_empty() {
                "暂无可用 Agent。".to_string()
            } else {
                format!("🕵️ **可用 Agent 列表：**\n```\n{}\n```", agents.trim())
            };
            reply(client, target, msg_id, &body).await?;
            Ok(CommandOutcome::Handled)
        }
        "/sandbox" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /sandbox command");
            handle_sandbox_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/project" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, arg, "Handled QQ /project command");
            handle_project_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/system" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /system command");
            handle_system_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/workspace" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /workspace command");
            handle_workspace_command(client, session_mgr, target, chat_id, arg, msg_id, false)
                .await?;
            Ok(CommandOutcome::Handled)
        }
        "/cd" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /cd command");
            handle_workspace_command(client, session_mgr, target, chat_id, arg, msg_id, true)
                .await?;
            Ok(CommandOutcome::Handled)
        }
        "/pwd" | "/dirs" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /dirs command");
            let session = session_mgr.get_or_create(chat_id).await;
            reply(client, target, msg_id, &format_dirs_text(&session)).await?;
            Ok(CommandOutcome::Handled)
        }
        "/adddir" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /adddir command");
            if arg.is_empty() {
                reply(
                    client,
                    target,
                    msg_id,
                    "⚠️ 请提供附加目录路径，例如 `/adddir ~/repo`",
                )
                .await?;
            } else {
                let session = session_mgr.get_or_create(chat_id).await;
                let resolved = resolve_user_path(arg, &session.workspace);
                let path_str = resolved.to_string_lossy().into_owned();
                session_mgr.add_extra_dir(chat_id, path_str.clone()).await;
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("📂 已添加附加工作目录：\n`{path_str}`"),
                )
                .await?;
            }
            Ok(CommandOutcome::Handled)
        }
        "/cleardirs" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, "Handled QQ /cleardirs command");
            session_mgr.clear_extra_dirs(chat_id).await;
            reply(client, target, msg_id, "🧹 已清空所有附加工作目录。").await?;
            Ok(CommandOutcome::Handled)
        }
        "/file" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /file command");
            handle_file_command(client, session_mgr, target, chat_id, arg, msg_id).await?;
            Ok(CommandOutcome::Handled)
        }
        "/cancel" => {
            let cancelled_turns = runtime.cancel_chat(chat_id);
            let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
            let hit = was_running || cancelled_turns > 0;
            info!(
                chat_id,
                cmd = "/cancel",
                speaker = %speaker_tag,
                hit,
                cancelled_turns,
                ?pid,
                was_running,
                "Handled QQ /cancel command"
            );
            if hit {
                let pid_str = pid.map_or_else(|| "-".to_string(), |p| p.to_string());
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("🛑 任务已中断 (PID: {pid_str})，工作区锁与等待队列已释放。"),
                )
                .await?;
            } else {
                reply(client, target, msg_id, "ℹ️ 当前没有正在执行的任务。").await?;
            }
            Ok(CommandOutcome::Handled)
        }
        "/voice" => {
            info!(chat_id, cmd = %cmd, speaker = %speaker_tag, has_arg = !arg.is_empty(), "Handled QQ /voice command");
            handle_voice_command(client, config, session_mgr, target, chat_id, arg, msg_id).await
        }
        _ => {
            if NATIVE_AGY_SLASH_COMMANDS
                .iter()
                .any(|native| native.eq_ignore_ascii_case(&cmd))
            {
                info!(
                    chat_id,
                    cmd = %cmd,
                    speaker = %speaker_tag,
                    "Forwarding native agy slash command"
                );
                return Ok(CommandOutcome::ExecutePrompt {
                    prompt_text: Cow::Borrowed(content),
                    voice_requested: false,
                });
            }

            let skill_candidate = cmd.trim_start_matches('/');
            if !skill_candidate.is_empty() {
                let known_skills = AgyExecutor::list_known_skill_names().await;
                if known_skills
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(skill_candidate))
                {
                    info!(
                        chat_id,
                        cmd = %cmd,
                        speaker = %speaker_tag,
                        "Forwarding known skill slash command"
                    );
                    return Ok(CommandOutcome::ExecutePrompt {
                        prompt_text: Cow::Borrowed(content),
                        voice_requested: false,
                    });
                }
            }

            info!(
                chat_id,
                cmd = %cmd,
                speaker = %speaker_tag,
                "Rejected unknown QQ slash command with help guide"
            );
            reply(
                client,
                target,
                msg_id,
                &build_unknown_command_guide(cmd_raw),
            )
            .await?;
            Ok(CommandOutcome::Handled)
        }
    }
}

fn format_dirs_text(session: &ChatSession) -> String {
    let mut out = format!("📂 **当前工作目录：**\n• 主目录: `{}`", session.workspace);
    if !session.extra_dirs.is_empty() {
        out.push_str("\n• 附加目录:");
        for dir in &session.extra_dirs {
            let _ = write!(out, "\n  - `{dir}`");
        }
    }
    out
}

fn build_qq_panel_text(
    session: &ChatSession,
    config: &Config,
    overview: Option<&QuotaOverview>,
) -> String {
    let conv_id = session.conversation_id.as_deref().unwrap_or("新建会话");
    let model_disp = model_display_label(&session.model);
    let effort_note = match effective_cli_effort(&session.model, &session.effort) {
        Some(eff) if eff != session.effort => format!("{} (实际: {eff})", session.effort),
        Some(eff) => eff.to_string(),
        None => format!("{} (由模型固定/忽略 --effort)", session.effort),
    };
    let backend_disp = config.executor_backend_enum().as_str();
    let agent_disp = if session.agent.is_empty() {
        "默认"
    } else {
        session.agent.as_str()
    };
    let project_disp = if session.project.is_empty() {
        "默认"
    } else {
        session.project.as_str()
    };
    let thinking_disp = match session.show_thinking {
        Some(true) => "开启",
        Some(false) => "关闭",
        None => "默认",
    };

    let ctx_disp = if session.last_context_tokens > 0 {
        format!("~{}K", session.last_context_tokens.div_ceil(1000))
    } else {
        "初始".to_string()
    };
    let compact_suffix = match session.last_auto_compact_turn {
        Some(turn) => format!(" (上次自动压缩: 第 {turn} 轮)"),
        None => String::new(),
    };

    let mut out = format!(
        "🎛️ **agygram 控制面板**\n\n\
        • 会话 ID: `{conv_id}`\n\
        • 工作目录: `{}` (附加 {} 个)\n\
        • 当前模型: **{model_disp}**\n\
        • 思考力度: `{effort_note}` | 模式: `{}` | 思考显示: `{thinking_disp}`\n\
        • 执行后端: `{backend_disp}` | 沙盒: {} | 远程代理: {} | Agent: `{agent_disp}` | 工程: `{project_disp}`\n\
        • 语音模式: {} | 轮次: {} | 累计耗时: {:.1}s | 上下文: `{ctx_disp}`{compact_suffix}\n\
        • Token: {} in / {} out\n",
        session.workspace,
        session.extra_dirs.len(),
        session.mode,
        if session.sandbox {
            "已开启"
        } else {
            "已关闭"
        },
        if session.remote_agent {
            "已开启"
        } else {
            "已关闭"
        },
        if session.voice_mode {
            "已开启"
        } else {
            "已关闭"
        },
        session.turn_count,
        session.total_duration_seconds,
        session.total_input_tokens,
        session.total_output_tokens
    );

    if let Some(ov) = overview {
        out.push_str("\n📊 **实时额度概览：**");
        if let Some(g) = ov.compact_badge(QuotaPool::Gemini) {
            let _ = write!(out, "\n• Gemini: `{g}`");
        }
        if let Some(tp) = ov.compact_badge(QuotaPool::ThirdParty) {
            let _ = write!(out, "\n• Claude & GPT: `{tp}`");
        }
        out.push('\n');
    }

    out.push_str("\n🔢 **快捷模型编号 (`/model <序号>`):**\n");
    append_compact_numbered_models(&mut out, &session.model);
    out.push_str(
        "\n💡 `/model reset` 恢复默认 · `/workspace <路径>` 切目录 · `/compact` 原生压缩 · `/new` 新建",
    );
    out
}

fn append_compact_numbered_models(out: &mut String, active_model: &str) {
    let families = list_model_families();
    let mut idx = 1usize;
    for family in families {
        let _ = write!(out, "{} **{}**: ", family.icon, family.title);
        for (i, entry) in family.models.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            let active = !active_model.is_empty() && entry.id.eq_ignore_ascii_case(active_model);
            let mark = if active { "✅" } else { "" };
            let dyn_tag = if entry.is_dynamic { "*" } else { "" };
            let _ = write!(out, "`{idx}`.{}{dyn_tag}{mark}", entry.variant_label);
            idx += 1;
        }
        out.push('\n');
    }
}

fn build_qq_models_list_text(
    active_model: &str,
    overview: Option<&QuotaOverview>,
    compact: bool,
) -> String {
    let current_disp = model_display_label(active_model);
    let mut out = format!("📋 **可用 AI 模型列表** (当前: **{current_disp}**)\n\n");
    let families = list_model_families();
    let mut idx = 1usize;

    for family in families {
        let badge = overview
            .and_then(|ov| ov.compact_badge(family.quota_pool))
            .map(|b| format!(" [{b}]"))
            .unwrap_or_default();
        let _ = writeln!(out, "{} **{}**{badge}", family.icon, family.title);
        for entry in family.models {
            let active = !active_model.is_empty() && entry.id.eq_ignore_ascii_case(active_model);
            let check = if active { " ✅" } else { "" };
            let dyn_badge = if entry.is_dynamic { " [动态]" } else { "" };
            if compact {
                let _ = writeln!(
                    out,
                    "  `{idx}`. {} (`{}`){dyn_badge}{check}",
                    entry.variant_label, entry.id
                );
            } else {
                let _ = writeln!(
                    out,
                    "  `{idx}`. **{}** — `{}`{dyn_badge}{check}",
                    entry.label, entry.id
                );
            }
            idx += 1;
        }
    }

    let _ = write!(
        out,
        "\n💡 发送 `/model <序号|ID|别名>` 切换（如 `/model 1` 或 `/model neon`），`/model reset` 恢复默认 ({SYSTEM_DEFAULT_MODEL_LABEL})。"
    );
    out
}

async fn handle_panel_command(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
) -> Result<()> {
    trigger_background_model_refresh_if_stale();
    let ((overview, _), session) = tokio::join!(
        QuotaFetcher::peek_overview_and_revalidate(),
        session_mgr.get_or_create(chat_id),
    );
    let panel = build_qq_panel_text(&session, config, overview.as_ref());
    reply(client, target, msg_id, &panel).await?;
    Ok(())
}

async fn handle_models_list_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    msg_id: &str,
) -> Result<()> {
    let (_, (overview, _), session) = tokio::join!(
        refresh_models_now(),
        QuotaFetcher::peek_overview_and_revalidate(),
        session_mgr.get_or_create(chat_id),
    );
    let text = build_qq_models_list_text(&session.model, overview.as_ref(), false);
    reply(client, target, msg_id, &text).await?;
    Ok(())
}

/// Builds the true native `/compact` prompt passed directly to `agy` (without prepending
/// `[System Instructions]`) so `agy` triggers an authentic `CHECKPOINT` compaction.
fn build_native_compact_prompt(focus: &str) -> String {
    let focus = focus.trim();
    if focus.is_empty() {
        "/compact".to_string()
    } else {
        format!("/compact {focus}")
    }
}

/// Returns `true` when the argument after `/compact` looks like a question *about* compaction
/// (e.g. `/compact 为什么频繁触发...`) rather than a compaction focus, preventing accidental
/// 60-second compactions when users ask about the command.
#[must_use]
fn looks_like_compact_question(arg: &str) -> bool {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.contains('?')
        || trimmed.contains('？')
        || trimmed.contains("为什么")
        || trimmed.contains("怎么")
        || trimmed.contains("啥意思")
        || trimmed.contains("什么意思")
        || trimmed.contains("如何触发")
        || trimmed.contains("是不是")
}

async fn handle_compact_command<'a>(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &'a str,
    msg_id: &str,
) -> Result<CommandOutcome<'a>> {
    let session = session_mgr.get_or_create(chat_id).await;
    if session.conversation_id.is_none() && session.is_first_turn {
        reply(
            client,
            target,
            msg_id,
            "ℹ️ 当前为全新会话，暂无上下文可压缩。直接发送问题或使用 `/new` 重置即可。",
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    if looks_like_compact_question(arg) {
        let compact_suffix = match session.last_auto_compact_turn {
            Some(turn) => format!("（上次自动压缩: 第 {turn} 轮）"),
            None => String::new(),
        };
        let ctx_disp = if session.last_context_tokens > 0 {
            format!(
                "约 {}K tokens{compact_suffix}",
                session.last_context_tokens.div_ceil(1000)
            )
        } else {
            format!("刚重置/压缩{compact_suffix}")
        };
        let auto_desc = if config.auto_compact_threshold_tokens > 0 {
            let mut desc = format!(
                "已开启空闲无感知自动压缩（单步上下文 ≥ {}K 且静默 {}s 后后台自动压缩",
                config.auto_compact_threshold_tokens / 1000,
                config.auto_compact_idle_secs
            );
            if config.auto_compact_idle_decay_secs > 0
                && config.auto_compact_idle_threshold_tokens > 0
            {
                let _ = write!(
                    desc,
                    "；或 ≥ {}K 且静默 {}m",
                    config.auto_compact_idle_threshold_tokens / 1000,
                    config.auto_compact_idle_decay_secs.div_ceil(60)
                );
            }
            desc.push('）');
            desc
        } else {
            "后台自动压缩已关闭".to_string()
        };
        reply(
            client,
            target,
            msg_id,
            &format!(
                "ℹ️ 检测到你像是在询问 `/compact` 用法，已避免误触发压缩。\n\
                • 当前真实上下文：{ctx_disp}\n\
                • 自动策略：{auto_desc}\n\
                • 如需立即手动压缩，请直接发送 `/compact`（或 `/compact 保留重点`）；如想就这个话题提问，直接发文字即可（不要以 `/compact` 开头）。"
            ),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    Ok(CommandOutcome::ExecutePrompt {
        prompt_text: Cow::Owned(build_native_compact_prompt(arg)),
        voice_requested: false,
    })
}

async fn handle_resume_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let recents = AgyExecutor::list_recent_conversations(8).await;
        if recents.is_empty() {
            session_mgr.resume_conversation(chat_id, None).await;
            reply(
                client,
                target,
                msg_id,
                "🔄 未扫描到历史会话文件，已开启接续模式 (`-c`)；下一条消息将接续最近一次 agy 上下文。",
            )
            .await?;
        } else {
            let mut out = String::from("🔄 **最近历史会话：**\n");
            for (i, item) in recents.iter().enumerate() {
                let title = truncate_chars(&item.title, 42);
                let _ = writeln!(
                    out,
                    "{}. `{}` — {} ({})",
                    i + 1,
                    item.id,
                    title,
                    item.updated_time
                );
            }
            out.push_str("\n发送 `/resume <会话ID>` 绑定并继续。");
            reply(client, target, msg_id, &out).await?;
        }
        return Ok(());
    }

    if !is_safe_cli_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的会话 ID：不能以 `-` 开头，仅允许字母、数字及 `_ - . +`。",
        )
        .await?;
        return Ok(());
    }

    session_mgr
        .resume_conversation(chat_id, Some(arg.to_string()))
        .await;
    reply(
        client,
        target,
        msg_id,
        &format!("🔄 已绑定历史会话 ID: `{arg}`，下一条消息将接续该上下文。"),
    )
    .await?;
    Ok(())
}

async fn handle_history_command(
    client: &QqClient,
    target: QqTarget<'_>,
    msg_id: &str,
) -> Result<()> {
    let recents = AgyExecutor::list_recent_conversations(8).await;
    if recents.is_empty() {
        reply(client, target, msg_id, "📜 暂未发现历史会话。").await?;
        return Ok(());
    }
    let mut out = String::from("📜 **VPS 最近活跃会话：**\n");
    for (i, item) in recents.iter().enumerate() {
        let title = truncate_chars(&item.title, 42);
        let _ = writeln!(
            out,
            "{}. `{}` — {} ({})",
            i + 1,
            item.id,
            title,
            item.updated_time
        );
    }
    out.push_str("\n发送 `/resume <会话ID>` 即可接续。");
    reply(client, target, msg_id, &out).await?;
    Ok(())
}

async fn handle_model_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    trigger_background_model_refresh_if_stale();

    if arg.is_empty() {
        let (session, (overview, _)) = tokio::join!(
            session_mgr.get_or_create(chat_id),
            QuotaFetcher::peek_overview_and_revalidate(),
        );
        let text = build_qq_models_list_text(&session.model, overview.as_ref(), true);
        reply(client, target, msg_id, &text).await?;
        return Ok(());
    }

    let resolved = if let Some(found) = resolve_model_input(arg) {
        Some(found)
    } else {
        let _ = refresh_models_now().await;
        if let Some(found) = resolve_model_input(arg) {
            Some(found)
        } else if is_safe_cli_value(arg) {
            AgyExecutor::list_models().await.ok().and_then(|raw| {
                raw.lines()
                    .filter_map(|line| line.split_whitespace().next())
                    .find(|id| id.eq_ignore_ascii_case(arg))
                    .map(str::to_string)
            })
        } else {
            None
        }
    };

    let Some(target_model) = resolved else {
        reply(
            client,
            target,
            msg_id,
            &format!(
                "⚠️ 无效的模型名称或序号: `{arg}`\n发送 `/models` 或 `/model` 查看完整序号列表。"
            ),
        )
        .await?;
        return Ok(());
    };

    let _synced_effort = session_mgr
        .set_model_and_sync_effort(chat_id, target_model.clone())
        .await;

    let updated = session_mgr.get_or_create(chat_id).await;
    let display = model_display_label(&target_model);
    let effort_status = match effective_cli_effort(&target_model, &updated.effort) {
        Some(eff) => format!("思考力度: `{eff}`"),
        None => format!("思考力度: `{}` (由该模型固定)", updated.effort),
    };
    let id_suffix = if target_model.is_empty() {
        String::new()
    } else {
        format!(" (`{target_model}`)")
    };

    reply(
        client,
        target,
        msg_id,
        &format!("✅ AI 模型已切换为: **{display}**{id_suffix}\n⚡ {effort_status}"),
    )
    .await?;
    Ok(())
}

async fn handle_effort_command(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!(
                "⚡ 当前思考力度: `{}`（QQ 默认: `{}`）\n可选: `low`, `medium`, `high`, `xhigh`, `max`",
                session.effort,
                config.effective_qq_effort()
            ),
        )
        .await?;
        return Ok(());
    }

    let Ok(effort) = arg.parse::<Effort>() else {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的思考力度。可选: `low`, `medium`, `high`, `xhigh`, `max`",
        )
        .await?;
        return Ok(());
    };

    let (effective_model, model_synced) =
        session_mgr.set_effort_and_sync_model(chat_id, effort).await;

    let note = if model_synced {
        format!(
            "\n🔄 已同步切换模型变体为: **{}** (`{effective_model}`)",
            model_display_label(&effective_model)
        )
    } else if effective_cli_effort(&effective_model, effort.as_str()).is_none() {
        format!(
            "\n💡 注意: 当前模型 `{effective_model}` 不接受 `--effort` 参数，将使用模型固定思考级别。"
        )
    } else {
        String::new()
    };

    reply(
        client,
        target,
        msg_id,
        &format!("✅ 思考力度已设置为: `{effort}`{note}"),
    )
    .await?;
    Ok(())
}

async fn handle_mode_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🛠️ 当前执行模式: `{}`\n可选: `auto`, `accept-edits`, `plan`",
                session.mode
            ),
        )
        .await?;
    } else if let Ok(mode) = arg.parse::<Mode>() {
        session_mgr.set_mode_enum(chat_id, mode).await;
        reply(
            client,
            target,
            msg_id,
            &format!("✅ 执行模式已设置为: `{mode}`"),
        )
        .await?;
    } else {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的执行模式。可选: `auto`, `accept-edits`, `plan`",
        )
        .await?;
    }
    Ok(())
}

async fn handle_thinking_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let state = match session.show_thinking {
            Some(true) => "已开启 (`on`)",
            Some(false) => "已关闭 (`off`)",
            None => "跟随默认 (`default`)",
        };
        reply(
            client,
            target,
            msg_id,
            &format!("💭 当前思考过程展示: {state}\n用法: `/thinking on|off|default`"),
        )
        .await?;
        return Ok(());
    }

    let lower = arg.to_ascii_lowercase();
    let msg = match lower.as_str() {
        "on" | "true" | "1" | "enable" | "开启" => {
            session_mgr.set_show_thinking(chat_id, Some(true)).await;
            "💭 已开启思考过程 (`showThinking: true`)"
        }
        "off" | "false" | "0" | "disable" | "关闭" => {
            session_mgr.set_show_thinking(chat_id, Some(false)).await;
            "💭 已关闭思考过程 (`showThinking: false`)"
        }
        "default" | "reset" | "clear" | "auto" | "默认" => {
            session_mgr.set_show_thinking(chat_id, None).await;
            "💭 思考过程展示已恢复默认设置"
        }
        _ => "⚠️ 无效参数。用法: `/thinking on|off|default`",
    };
    reply(client, target, msg_id, msg).await?;
    Ok(())
}

async fn handle_remote_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let state = if session.remote_agent {
            "已开启 (`--remote-agent`)"
        } else {
            "已关闭"
        };
        reply(
            client,
            target,
            msg_id,
            &format!("☁️ 当前云端远程代理: {state}\n用法: `/remote on|off|toggle`"),
        )
        .await?;
        return Ok(());
    }

    let lower = arg.to_ascii_lowercase();
    let enabled = match lower.as_str() {
        "on" | "true" | "1" | "enable" | "开启" => {
            session_mgr.set_remote_agent(chat_id, true).await;
            true
        }
        "off" | "false" | "0" | "disable" | "关闭" => {
            session_mgr.set_remote_agent(chat_id, false).await;
            false
        }
        "toggle" | "切换" => session_mgr.toggle_remote_agent(chat_id).await,
        _ => {
            reply(
                client,
                target,
                msg_id,
                "⚠️ 无效参数。用法: `/remote on|off|toggle`",
            )
            .await?;
            return Ok(());
        }
    };

    let msg = if enabled {
        "☁️ 云端远程代理已开启 (`--remote-agent`)"
    } else {
        "🖥️ 云端远程代理已关闭（恢复本机执行）"
    };
    reply(client, target, msg_id, msg).await?;
    Ok(())
}

async fn handle_agent_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let (session, agents) = tokio::join!(session_mgr.get_or_create(chat_id), async {
            AgyExecutor::list_agents().await.unwrap_or_default()
        },);
        let current = if session.agent.is_empty() {
            "默认 Agent"
        } else {
            &session.agent
        };
        let list_section = if agents.trim().is_empty() {
            String::new()
        } else {
            format!("\n\n可用列表：\n```\n{}\n```", agents.trim())
        };
        reply(
            client,
            target,
            msg_id,
            &format!("🕵️ 当前 Agent: `{current}`{list_section}\n使用 `/agent <名称>` 切换，`/agent reset` 恢复默认。"),
        )
        .await?;
    } else if arg.eq_ignore_ascii_case("default")
        || arg.eq_ignore_ascii_case("reset")
        || arg.eq_ignore_ascii_case("clear")
    {
        session_mgr.set_agent(chat_id, String::new()).await;
        reply(client, target, msg_id, "🕵️ 已恢复默认 Agent。").await?;
    } else if !is_safe_free_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的 Agent 名称：不能以 `-` 开头或含控制字符，上限 128 字符。",
        )
        .await?;
    } else {
        session_mgr.set_agent(chat_id, arg.to_string()).await;
        reply(
            client,
            target,
            msg_id,
            &format!("🕵️ Agent 已切换为: `{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_sandbox_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    let lower = arg.to_ascii_lowercase();
    let enabled = match lower.as_str() {
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
    let msg = if enabled {
        "🛡️ 沙盒隔离模式已开启 (`--sandbox`)"
    } else {
        "⚡ 沙盒隔离模式已关闭"
    };
    reply(client, target, msg_id, msg).await?;
    Ok(())
}

async fn handle_project_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let cur = if session.project.is_empty() {
            "默认工程 (未指定)"
        } else {
            &session.project
        };
        reply(
            client,
            target,
            msg_id,
            &format!("📁 当前工程: `{cur}`\n使用 `/project <名称>` 绑定，`/project clear` 清空。"),
        )
        .await?;
    } else if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("reset") {
        session_mgr.set_project(chat_id, String::new()).await;
        reply(client, target, msg_id, "🔄 已重置 Project 绑定。").await?;
    } else if !is_safe_free_value(arg) {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 无效的 Project 名称：不能以 `-` 开头或含控制字符，上限 128 字符。",
        )
        .await?;
    } else {
        session_mgr.set_project(chat_id, arg.to_string()).await;
        reply(
            client,
            target,
            msg_id,
            &format!("📁 已绑定 Project: `{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_system_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        if session.system_prompt.is_empty() {
            reply(
                client,
                target,
                msg_id,
                "🧠 当前未设置自定义 System Prompt。\n发送 `/system <内容>` 设置，`/system clear` 清空。",
            )
            .await?;
        } else {
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "🧠 当前 System Prompt:\n```\n{}\n```\n发送 `/system clear` 可清空。",
                    session.system_prompt
                ),
            )
            .await?;
        }
    } else if arg.eq_ignore_ascii_case("clear") || arg.eq_ignore_ascii_case("reset") {
        session_mgr.set_system_prompt(chat_id, String::new()).await;
        reply(client, target, msg_id, "🧠 System Prompt 已清空。").await?;
    } else {
        session_mgr
            .set_system_prompt(chat_id, arg.to_string())
            .await;
        reply(
            client,
            target,
            msg_id,
            &format!("🧠 System Prompt 已更新：\n`{arg}`"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_workspace_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
    require_arg: bool,
) -> Result<()> {
    if arg.is_empty() {
        if require_arg {
            reply(
                client,
                target,
                msg_id,
                "⚠️ 请提供目标目录路径，例如 `/cd ~/project` 或 `/workspace ~/project`",
            )
            .await?;
        } else {
            let session = session_mgr.get_or_create(chat_id).await;
            let mut text = format_dirs_text(&session);
            text.push_str("\n\n用法: `/workspace <路径>` 切换主目录；`/adddir <路径>` 添加附加目录；`/cleardirs` 清空附加目录");
            reply(client, target, msg_id, &text).await?;
        }
        return Ok(());
    }

    let session = session_mgr.get_or_create(chat_id).await;
    let target_path = resolve_user_path(arg, &session.workspace);
    let is_existing_dir = tokio::fs::metadata(&target_path)
        .await
        .is_ok_and(|meta| meta.is_dir());
    if is_existing_dir || tokio::fs::create_dir_all(&target_path).await.is_ok() {
        let path_str = target_path.to_string_lossy().to_string();
        session_mgr.set_workspace(chat_id, path_str.clone()).await;
        let prefix = if is_existing_dir {
            "📂 工作主目录已切换至"
        } else {
            "📂 目标目录不存在，已新建并切换至"
        };
        reply(client, target, msg_id, &format!("{prefix}:\n`{path_str}`")).await?;
    } else {
        reply(
            client,
            target,
            msg_id,
            &format!("❌ 无法切换工作目录: 路径 `{arg}` 无效或权限不足。"),
        )
        .await?;
    }
    Ok(())
}

async fn handle_file_command(
    client: &QqClient,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &str,
    msg_id: &str,
) -> Result<()> {
    if arg.is_empty() {
        reply(
            client,
            target,
            msg_id,
            "⚠️ 请提供 VPS 文件路径，例如 `/file scratch/report.pdf` 或 `/file Cargo.toml`",
        )
        .await?;
        return Ok(());
    }

    let session = session_mgr.get_or_create(chat_id).await;
    let workspace = session.get_valid_workspace();
    let file_path = resolve_user_path(arg, &workspace);

    let mut allowed_roots = vec![
        PathBuf::from(&workspace),
        PathBuf::from(&workspace).join("scratch"),
    ];
    allowed_roots.extend(session.extra_dirs.iter().map(PathBuf::from));

    match tokio::fs::metadata(&file_path).await {
        Ok(meta) if !meta.is_file() => {
            reply(
                client,
                target,
                msg_id,
                &format!("❌ `{}` 不是普通文件，无法发送。", file_path.display()),
            )
            .await?;
        }
        Err(_) => {
            if looks_sensitive(&file_path.to_string_lossy()) || looks_sensitive(arg) {
                warn!(
                    chat_id,
                    arg, "Blocked /file request targeting sensitive path"
                );
                reply(
                    client,
                    target,
                    msg_id,
                    "🔒 拒绝发送：不允许读取敏感或隐藏路径文件。",
                )
                .await?;
            } else {
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("❌ 文件不存在或无法读取: `{arg}`"),
                )
                .await?;
            }
        }
        Ok(meta) => match validate_explicit_file_path(&file_path, &allowed_roots).await {
            Ok(validated) => {
                deliver_single_file(client, target, msg_id, &validated, meta.len()).await;
            }
            Err(reason) => {
                warn!(
                    chat_id,
                    path = %file_path.display(),
                    reason,
                    "Blocked QQ /file request via validate_explicit_file_path"
                );
                reply(
                    client,
                    target,
                    msg_id,
                    &format!("🔒 拒绝发送该文件：{reason}"),
                )
                .await?;
            }
        },
    }
    Ok(())
}

async fn handle_voice_command<'a>(
    client: &QqClient,
    config: &Config,
    session_mgr: &SessionManager,
    target: QqTarget<'_>,
    chat_id: i64,
    arg: &'a str,
    msg_id: &str,
) -> Result<CommandOutcome<'a>> {
    if arg.eq_ignore_ascii_case("on") || arg == "开启" {
        session_mgr.set_voice_mode(chat_id, true).await;
        reply(
            client,
            target,
            msg_id,
            "🔊 语音回复已开启：后续每条回答会附带一条语音。",
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }
    if arg.eq_ignore_ascii_case("off") || arg == "关闭" {
        session_mgr.set_voice_mode(chat_id, false).await;
        reply(client, target, msg_id, "🔇 语音回复已关闭。").await?;
        return Ok(CommandOutcome::Handled);
    }
    if arg.eq_ignore_ascii_case("toggle") || arg == "切换" {
        let enabled = session_mgr.toggle_voice_mode(chat_id).await;
        reply(
            client,
            target,
            msg_id,
            &format!("🔊 语音回复: {}", if enabled { "已开启" } else { "已关闭" }),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    if arg
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("id"))
    {
        let id_arg = arg
            .split_once(char::is_whitespace)
            .map_or("", |(_, rest)| rest.trim());
        if id_arg.is_empty() {
            let session = session_mgr.get_or_create(chat_id).await;
            let current = session
                .voice_reference_id
                .as_deref()
                .unwrap_or("(未设置，使用默认)");
            reply(
                client,
                target,
                msg_id,
                &format!(
                    "🎙 当前会话音色ID: {current}\n用法: /voice id <音色ID> 更换；/voice id reset 恢复默认"
                ),
            )
            .await?;
        } else if id_arg.eq_ignore_ascii_case("reset")
            || id_arg.eq_ignore_ascii_case("default")
            || id_arg == "重置"
        {
            session_mgr.set_voice_reference(chat_id, None).await;
            let fallback = config
                .fish_reference_id
                .as_deref()
                .unwrap_or("Fish Audio 默认音色");
            reply(
                client,
                target,
                msg_id,
                &format!("✅ 已恢复默认音色: {fallback}"),
            )
            .await?;
        } else if is_valid_voice_reference(id_arg) {
            session_mgr
                .set_voice_reference(chat_id, Some(id_arg.to_string()))
                .await;
            reply(
                client,
                target,
                msg_id,
                &format!("✅ 音色已切换: {id_arg}\n下一轮语音回复生效。"),
            )
            .await?;
        } else {
            reply(
                client,
                target,
                msg_id,
                "⚠️ 音色ID格式不正确：只允许字母、数字、- 和 _，长度 8-128。",
            )
            .await?;
        }
        return Ok(CommandOutcome::Handled);
    }

    if arg.is_empty() {
        let session = session_mgr.get_or_create(chat_id).await;
        let current_ref = session
            .voice_reference_id
            .as_deref()
            .or(config.fish_reference_id.as_deref())
            .unwrap_or("Fish Audio 默认音色");
        reply(
            client,
            target,
            msg_id,
            &format!(
                "🔊 语音回复 (Fish Audio TTS)\n状态: {}\n音色ID: {current_ref}\n用法: /voice on|off|toggle；/voice id <音色ID> 更换音色；/voice id reset 恢复默认；/voice <问题> 单次语音",
                if session.voice_mode { "已开启" } else { "已关闭" }
            ),
        )
        .await?;
        return Ok(CommandOutcome::Handled);
    }

    Ok(CommandOutcome::ExecutePrompt {
        prompt_text: Cow::Borrowed(arg),
        voice_requested: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::qq_openid_to_chat_id;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    async fn spawn_recording_qq_server() -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let messages = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = messages.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::with_capacity(4096);
                    let mut tmp = [0u8; 2048];
                    let header_end;
                    loop {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = pos + 4;
                            break;
                        }
                    }

                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);

                    while buf.len() < header_end + content_len {
                        let Ok(n) = stream.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let req_line = headers.lines().next().unwrap_or("");
                    let body = String::from_utf8_lossy(
                        &buf[header_end..(header_end + content_len).min(buf.len())],
                    )
                    .to_string();

                    let resp_body = if req_line.starts_with("POST /app/getAppAccessToken ") {
                        r#"{"access_token":"tok","expires_in":7200}"#.to_string()
                    } else {
                        recorded.lock().unwrap().push(body);
                        r#"{"id":"msg_out_1"}"#.to_string()
                    };

                    let raw_resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(raw_resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        (format!("http://{addr}"), messages, handle)
    }

    #[test]
    fn test_voice_reference_validation() {
        assert!(is_valid_voice_reference("7f92f8afb8ec43bf81429cc1c9199cb1"));
        assert!(is_valid_voice_reference("clone_voice-01"));
        assert!(!is_valid_voice_reference("../../etc/passwd"));
        assert!(!is_valid_voice_reference("short"));
        assert!(!is_valid_voice_reference("含中文的音色ID"));
    }

    #[tokio::test]
    async fn test_handle_qq_command_updates_session_native_compact_and_filters_unknown() {
        let (base_url, recorded, server) = spawn_recording_qq_server().await;
        let dir = std::env::temp_dir().join(format!("agygram_qq_cmd_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            ..Config::default()
        };
        let mgr = SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        );
        let runtime = QqRuntime::new(CancellationToken::new());
        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let target = QqTarget::C2c("u_test");
        let chat_id = qq_openid_to_chat_id("u_test");
        let meta = IncomingMeta {
            sender_openid: "u_test",
            is_group: false,
            is_shared_group: false,
            ..IncomingMeta::default()
        };

        let out = handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/help", "m1", meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/effort XHigh",
            "m2",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        assert_eq!(mgr.get_or_create(chat_id).await.effort, "xhigh");
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/mode AUTO",
            "m3",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // /thinking and /remote update session toggles
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/thinking on",
            "m3b",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(mgr.get_or_create(chat_id).await.show_thinking, Some(true));
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/remote on",
            "m3c",
            meta,
        )
        .await
        .unwrap();
        assert!(mgr.get_or_create(chat_id).await.remote_agent);

        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/MODEL opus",
            "m4",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let session = mgr.get_or_create(chat_id).await;
        assert!(session.model.contains("claude-opus"));
        assert_eq!(session.mode, "auto");

        // Switching to a suffixed model and changing /effort syncs sibling variant atomically.
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/model neon-high",
            "m4a",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(mgr.get_or_create(chat_id).await.model, "gemini-4-neon-high");
        assert_eq!(mgr.get_or_create(chat_id).await.effort, "high");
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/effort low",
            "m4a2",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(mgr.get_or_create(chat_id).await.effort, "low");
        assert_eq!(mgr.get_or_create(chat_id).await.model, "gemini-4-neon-low");

        let out = handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/model 1", "m4b", meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);
        let expected_first = crate::models::find_model_by_index(1).unwrap().id;
        assert_eq!(mgr.get_or_create(chat_id).await.model, expected_first);

        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/model reset",
            "m4c",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(mgr.get_or_create(chat_id).await.model, "");

        let sub_ws = dir.join("proj_a");
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            &format!("/workspace {}", sub_ws.display()),
            "m4d",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(
            mgr.get_or_create(chat_id).await.workspace,
            sub_ws.to_string_lossy().as_ref()
        );

        let out = handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/compact", "m4e", meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        // Active conversation `/compact api` emits true native `/compact api` prompt!
        mgr.resume_conversation(chat_id, Some("conv_test_1".to_string()))
            .await;
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/compact api",
            "m4f",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            CommandOutcome::ExecutePrompt {
                prompt_text: Cow::Owned("/compact api".to_string()),
                voice_requested: false,
            }
        );

        let out = handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/panel", "m4g", meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/voice 帮我总结一下",
            "m5",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            CommandOutcome::ExecutePrompt {
                prompt_text: Cow::Borrowed("帮我总结一下"),
                voice_requested: true
            }
        );

        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/voice on",
            "m6",
            meta,
        )
        .await
        .unwrap();
        assert!(mgr.get_or_create(chat_id).await.voice_mode);

        // Built-in native agy command `/plan` passes through.
        let out = handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/plan refactor qq",
            "m7",
            meta,
        )
        .await
        .unwrap();
        assert_eq!(
            out,
            CommandOutcome::ExecutePrompt {
                prompt_text: Cow::Borrowed("/plan refactor qq"),
                voice_requested: false
            }
        );

        // Unknown slash command `/old` is intercepted locally with a concise help guide.
        let out = handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/old", "m8", meta,
        )
        .await
        .unwrap();
        assert_eq!(out, CommandOutcome::Handled);

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("xhigh"));
        assert!(sent.contains("auto"));
        assert!(sent.contains("/skills"));
        assert!(sent.contains("/thinking"));
        assert!(sent.contains("/remote"));
        assert!(sent.contains("未识别的指令"));
        assert!(sent.contains("/old"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_handle_file_command_validates_security_and_falls_back_cleanly() {
        let (base_url, recorded, server) = spawn_recording_qq_server().await;
        let dir =
            std::env::temp_dir().join(format!("agygram_qq_file_cmd_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("subdir");
        let hidden_dir = dir.join(".ssh");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&hidden_dir).unwrap();
        let note_file = sub.join("summary.md");
        let secret_file = hidden_dir.join("id_rsa");
        let dotenv_file = dir.join(".env");
        std::fs::write(&note_file, "# Summary\nHello from agygram\n").unwrap();
        std::fs::write(&secret_file, "PRIVATE KEY MATERIAL").unwrap();
        std::fs::write(&dotenv_file, "SECRET=123").unwrap();

        let cfg = Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            ..Config::default()
        };
        let mgr = SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        );
        let runtime = QqRuntime::new(CancellationToken::new());
        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let target = QqTarget::C2c("u_file_test");
        let chat_id = qq_openid_to_chat_id("u_file_test");
        let meta = IncomingMeta {
            sender_openid: "u_file_test",
            is_group: false,
            is_shared_group: false,
            ..IncomingMeta::default()
        };

        handle_qq_command(
            &client, &cfg, &mgr, &runtime, target, chat_id, "/file", "mf1", meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file missing.txt",
            "mf2",
            meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file subdir",
            "mf3",
            meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file subdir/summary.md",
            "mf4",
            meta,
        )
        .await
        .unwrap();
        // Hidden/sensitive files inside or outside workspace MUST be blocked even in C2C.
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file .ssh/id_rsa",
            "mf5",
            meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file .env",
            "mf6",
            meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/file /etc/passwd",
            "mf7",
            meta,
        )
        .await
        .unwrap();

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("请提供 VPS 文件路径"));
        assert!(sent.contains("文件不存在或无法读取"));
        assert!(sent.contains("不是普通文件"));
        assert!(sent.contains("Hello from agygram"));
        assert!(!sent.contains("PRIVATE KEY MATERIAL"));
        assert!(!sent.contains("SECRET=123"));
        assert!(sent.contains("拒绝发送"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_shared_group_rbac_blocks_non_admin_and_allows_admin() {
        let (base_url, recorded, server) = spawn_recording_qq_server().await;
        let dir = std::env::temp_dir().join(format!("agygram_qq_rbac_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let cfg = Config {
            default_workspace: dir.to_string_lossy().into_owned(),
            qq_admin_openids: vec!["ADMIN_OPENID".to_string()],
            ..Config::default()
        };
        let mgr = SessionManager::with_storage_path(
            dir.to_string_lossy().into_owned(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        );
        let runtime = QqRuntime::new(CancellationToken::new());
        let client = QqClient::new_for_test("id", "sec", &base_url, &base_url);
        let target = QqTarget::Group("GRP_SHARED");
        let chat_id = qq_openid_to_chat_id("GRP_SHARED");

        let guest_meta = IncomingMeta {
            sender_openid: "GUEST_OPENID",
            is_group: true,
            is_shared_group: true,
            ..IncomingMeta::default()
        };
        let admin_meta = IncomingMeta {
            sender_openid: "ADMIN_OPENID",
            is_group: true,
            is_shared_group: true,
            ..IncomingMeta::default()
        };

        for blocked_cmd in [
            "/file Cargo.toml",
            "/resume conv_123",
            "/history",
            "/rules",
            "/cd /tmp",
            "/workspace /tmp",
            "/system override",
            "/adddir /tmp",
            "/cleardirs",
            "/sandbox on",
            "/project foo",
            "/remote on",
        ] {
            let out = handle_qq_command(
                &client,
                &cfg,
                &mgr,
                &runtime,
                target,
                chat_id,
                blocked_cmd,
                "m_guest",
                guest_meta,
            )
            .await
            .unwrap();
            assert_eq!(out, CommandOutcome::Handled);
        }
        assert!(!mgr.get_or_create(chat_id).await.remote_agent);
        assert!(!mgr.get_or_create(chat_id).await.sandbox);
        assert_eq!(mgr.get_or_create(chat_id).await.conversation_id, None);

        // Read-only /workspace without mutation arg is allowed for guests.
        assert!(!requires_shared_group_admin("/workspace", ""));
        assert!(requires_shared_group_admin("/workspace", "/tmp"));
        assert!(!requires_shared_group_admin("/system", ""));
        assert!(requires_shared_group_admin("/system", "custom"));

        // Admin CAN run `/resume conv_admin` and `/remote on`.
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/resume conv_admin",
            "m_adm1",
            admin_meta,
        )
        .await
        .unwrap();
        handle_qq_command(
            &client,
            &cfg,
            &mgr,
            &runtime,
            target,
            chat_id,
            "/remote on",
            "m_adm2",
            admin_meta,
        )
        .await
        .unwrap();
        let session = mgr.get_or_create(chat_id).await;
        assert_eq!(session.conversation_id.as_deref(), Some("conv_admin"));
        assert!(session.remote_agent);

        let sent = recorded.lock().unwrap().join("\n");
        assert!(sent.contains("权限不足"));

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compact_question_detector_catches_inquiries_without_blocking_focus() {
        assert!(looks_like_compact_question(
            "为什么频繁触发，有什么可以优化的点"
        ));
        assert!(looks_like_compact_question("这个怎么用？"));
        assert!(looks_like_compact_question("啥意思"));
        assert!(!looks_like_compact_question(""));
        assert!(!looks_like_compact_question("保留最近三轮排障结论"));
        assert!(!looks_like_compact_question("focus on rust tests"));
    }

    #[test]
    fn qq_panel_text_displays_backend_duration_and_optional_auto_compact_turn() {
        let mut session =
            ChatSession::new("/tmp".into(), String::new(), "auto".into(), "medium".into());
        session.turn_count = 9;
        session.total_duration_seconds = 24.5;
        session.last_context_tokens = 95_000;
        session.last_auto_compact_turn = Some(6);

        let cfg = Config {
            executor_backend: "persistent".to_string(),
            qq_default_effort: Some("medium".to_string()),
            ..Config::default()
        };
        let panel = build_qq_panel_text(&session, &cfg, None);
        assert!(panel.contains("执行后端: `persistent`"));
        assert!(panel.contains("累计耗时: 24.5s"));
        assert!(panel.contains("上次自动压缩: 第 6 轮"));

        session.last_auto_compact_turn = None;
        let panel_no_compact = build_qq_panel_text(&session, &cfg, None);
        assert!(!panel_no_compact.contains("上次自动压缩"));
    }
}
