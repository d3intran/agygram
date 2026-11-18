use crate::bot::formatter::TelegramFormatter;
use crate::bot::pipeline::{edit_or_send_html_chunks, log_telegram_err, send_html_chunks};
use crate::bot::widget::{
    AgentCardData, AgentCardStep, AgentStatus, BuiltinGame, CustomCardSpec, OutboundWidget,
    WidgetError, deliver_widget_with_fallback, send_widget_html_file,
};
use crate::config::Config;
use crate::executor::{AgyExecutor, RecentSessionInfo};
use crate::models::{
    Effort, QuotaPool, decode_model_callback, effort_from_model_suffix, encode_model_callback,
    find_model, find_model_family, is_safe_cli_value, is_safe_free_value, is_safe_model_id,
    is_supported_effort, is_supported_mode, is_valid_model, list_model_families,
    model_display_label, normalize_model_name, refresh_models_if_stale, refresh_models_now,
    supports_effort_flag,
};
use crate::quota::{QuotaFetcher, QuotaOverview};
use crate::session::{ChatSession, SessionManager};
use crate::text;
use rustc_hash::FxHashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode};
use tokio::sync::OnceCell;
use tracing::warn;

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;
const MAX_HASH_CACHE_ENTRIES: usize = 512;
const QUICK_QUOTA_TIMEOUT: Duration = Duration::from_millis(1800);

static AGY_VERSION_CACHE: OnceCell<String> = OnceCell::const_new();
static RESUME_HASH_CACHE: OnceLock<RwLock<FxHashMap<String, String>>> = OnceLock::new();
static AGENT_HASH_CACHE: OnceLock<RwLock<FxHashMap<String, String>>> = OnceLock::new();

#[inline]
const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV1A_OFFSET_BASIS;
    let mut i = 0usize;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV1A_PRIME);
        i += 1;
    }
    hash
}

fn cache_hashed_value(
    cache_cell: &OnceLock<RwLock<FxHashMap<String, String>>>,
    value: &str,
) -> String {
    let hex = format!("{:016x}", fnv1a_64(value.as_bytes()));
    let lock = cache_cell.get_or_init(|| RwLock::new(FxHashMap::default()));
    if let Ok(mut map) = lock.write() {
        if map.len() >= MAX_HASH_CACHE_ENTRIES && !map.contains_key(&hex) {
            map.clear();
        }
        map.insert(hex.clone(), value.to_string());
    }
    hex
}

fn lookup_hashed_value(
    cache_cell: &OnceLock<RwLock<FxHashMap<String, String>>>,
    hex: &str,
) -> Option<String> {
    let lock = cache_cell.get()?;
    lock.read().ok()?.get(hex).cloned()
}

#[inline]
fn cb_btn(label: impl Into<String>, data: impl Into<String>) -> InlineKeyboardButton {
    let data_str = data.into();
    debug_assert!(
        data_str.len() <= 64,
        "Telegram callback_data exceeded 64 bytes: {} ({})",
        data_str.len(),
        data_str
    );
    InlineKeyboardButton::callback(label.into(), data_str)
}

#[must_use]
pub(crate) fn encode_resume_callback(conv_id: &str) -> String {
    if conv_id == "continue" {
        return "rs:continue".to_string();
    }
    if conv_id.len() <= 61 && is_safe_cli_value(conv_id) {
        return format!("rs:{conv_id}");
    }
    let hex = cache_hashed_value(&RESUME_HASH_CACHE, conv_id);
    format!("rh:{hex}")
}

#[must_use]
pub(crate) fn decode_resume_callback(data: &str) -> Option<String> {
    if let Some(arg) = data
        .strip_prefix("rs:")
        .or_else(|| data.strip_prefix("resume:"))
    {
        if arg == "continue" || is_safe_cli_value(arg) {
            return Some(arg.to_string());
        }
        return None;
    }
    if let Some(hex) = data.strip_prefix("rh:") {
        return lookup_hashed_value(&RESUME_HASH_CACHE, hex).filter(|v| is_safe_model_id(v));
    }
    None
}

#[must_use]
pub(crate) fn encode_agent_callback(agent: &str) -> String {
    if agent.len() <= 61 && is_safe_cli_value(agent) {
        return format!("sa:{agent}");
    }
    let hex = cache_hashed_value(&AGENT_HASH_CACHE, agent);
    format!("sh:{hex}")
}

#[must_use]
pub(crate) fn decode_agent_callback(data: &str) -> Option<String> {
    if let Some(name) = data.strip_prefix("sa:") {
        return is_safe_cli_value(name).then(|| name.to_string());
    }
    if let Some(hex) = data.strip_prefix("sh:") {
        return lookup_hashed_value(&AGENT_HASH_CACHE, hex).filter(|v| is_safe_free_value(v));
    }
    None
}

/// Returns the cached `agy --version` string so interactive panels do not spawn a subprocess on every click.
pub(crate) async fn get_cached_agy_version() -> &'static str {
    AGY_VERSION_CACHE
        .get_or_init(|| async { AgyExecutor::get_version().await })
        .await
        .as_str()
}

/// Applies `model` to the session and atomically syncs `effort` when the model ID carries an
/// explicit effort suffix (`-xhigh`, `-high`, `-medium`, `-low`, `-max`).
pub(crate) async fn apply_model_and_effort(
    session_mgr: &SessionManager,
    chat_id: i64,
    target_model: &str,
) -> Option<Effort> {
    session_mgr
        .set_model_and_sync_effort(chat_id, target_model.to_string())
        .await
}

#[inline]
fn active_mark(is_active: bool) -> &'static str {
    if is_active { "✓ " } else { "" }
}

fn short_model_badge(model_id: &str) -> String {
    if model_id.trim().is_empty() {
        return "默认".to_string();
    }
    if let Some(entry) = find_model(model_id) {
        return match entry.family_slug.as_str() {
            "h2" => format!("H2·{}", entry.variant_label),
            "carbon" => format!("Carbon·{}", entry.variant_label),
            "argon" => format!("Argon·{}", entry.variant_label),
            "flash" => format!("Flash·{}", entry.variant_label),
            "orch" => entry.variant_label,
            "claude" => entry.variant_label.replace(" (Thinking)", ""),
            "gpt" => "GPT-OSS".to_string(),
            _ => text::truncate_chars(&entry.label, 14).into_owned(),
        };
    }
    text::truncate_chars(model_id, 14).into_owned()
}

fn format_quota_inline_summary(overview: Option<&QuotaOverview>) -> String {
    let Some(ov) = overview else {
        return "<i>点击「📊 实时额度」查看</i>".to_string();
    };
    let gem = ov
        .compact_badge(QuotaPool::Gemini)
        .unwrap_or_else(|| "-".to_string());
    let tp = ov
        .compact_badge(QuotaPool::ThirdParty)
        .unwrap_or_else(|| "-".to_string());
    format!(
        "Gemini <code>{}</code> · Claude/GPT <code>{}</code>",
        text::escape_html(&gem),
        text::escape_html(&tp)
    )
}

/// Shared inline history card builder used by `/history`, `/resume`, `/session`, and `cp:sess`.
#[must_use]
pub(crate) fn build_history_card(
    header: &str,
    recents: &[RecentSessionInfo],
    footer_buttons: Vec<InlineKeyboardButton>,
) -> (String, InlineKeyboardMarkup) {
    build_history_card_with_active(header, recents, None, footer_buttons)
}

#[must_use]
pub(crate) fn build_history_card_with_active(
    header: &str,
    recents: &[RecentSessionInfo],
    active_conv_id: Option<&str>,
    footer_buttons: Vec<InlineKeyboardButton>,
) -> (String, InlineKeyboardMarkup) {
    let mut rows = Vec::with_capacity(recents.len() + 2);
    let mut text_out = String::with_capacity(header.len() + recents.len() * 128);
    text_out.push_str(header);
    text_out.push_str("\n\n");

    for (idx, r) in recents.iter().enumerate() {
        let is_current = active_conv_id == Some(r.id.as_str());
        let mark = active_mark(is_current);
        let flags = match (r.has_plan, r.has_walkthrough) {
            (true, true) => " [Plan+WT]",
            (true, false) => " [Plan]",
            (false, true) => " [WT]",
            (false, false) => "",
        };
        let _ = write!(
            text_out,
            "{mark}{}. <b>{}</b>{}\n   <code>{}</code> · {}\n\n",
            idx + 1,
            text::escape_html(&r.title),
            flags,
            text::escape_html(text::take_chars(&r.id, 8)),
            text::escape_html(&r.updated_time)
        );
        rows.push(vec![cb_btn(
            format!("{mark}{}. {}", idx + 1, text::truncate_chars(&r.title, 20)),
            encode_resume_callback(&r.id),
        )]);
    }

    if !footer_buttons.is_empty() {
        rows.push(footer_buttons);
    }

    (text_out, InlineKeyboardMarkup::new(rows))
}

/// Builds the Model Family picker card (page 1 of the two-stage Model selector).
#[must_use]
pub fn build_model_family_card(
    session: &ChatSession,
    overview: Option<&QuotaOverview>,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let families = list_model_families();
    let active_family_slug = if session.model.is_empty() {
        None
    } else {
        find_model(&session.model).map(|e| e.family_slug)
    };

    let current_label = model_display_label(&session.model);
    let current_id = if session.model.is_empty() {
        "default (gemini-4-carbon)"
    } else {
        session.model.as_str()
    };

    let mut text_out = String::with_capacity(640);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let _ = write!(
        text_out,
        "🤖 <b>模型控制中心 · 选择模型家族</b>\n\n\
        • <b>当前模型</b>: <code>{}</code>\n\
        • <b>CLI ID</b>: <code>{}</code>\n\
        • <b>当前 Effort</b>: <code>{}</code>{}\n\
        • <b>实时额度</b>: {}\n\n\
        <i>点击模型家族进入具体档位 / 变体选择：</i>",
        text::escape_html(&current_label),
        text::escape_html(current_id),
        text::escape_html(&session.effort),
        if !supports_effort_flag(&session.model) && !session.model.is_empty() {
            " <i>(由模型固定)</i>"
        } else {
            ""
        },
        format_quota_inline_summary(overview),
    );

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut current_row: Vec<InlineKeyboardButton> = Vec::with_capacity(2);
    for fam in &families {
        let is_active = active_family_slug.as_deref() == Some(fam.slug.as_str());
        let pct_suffix = overview
            .and_then(|ov| ov.button_pct(fam.quota_pool))
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        let label = format!(
            "{}{} {}{}",
            active_mark(is_active),
            fam.icon,
            fam.title,
            pct_suffix
        );
        let data = format!("cp:mv:{}", fam.slug);
        current_row.push(cb_btn(label, data));
        if current_row.len() == 2 {
            rows.push(std::mem::take(&mut current_row));
        }
    }
    if !current_row.is_empty() {
        rows.push(current_row);
    }

    rows.push(vec![
        cb_btn(
            format!("{}⚙️ 系统默认", active_mark(session.model.is_empty())),
            "ms:default",
        ),
        cb_btn("🔄 同步 live 模型", "cp:mf:r"),
    ]);
    rows.push(vec![cb_btn("« 返回控制中心", "cp:home")]);

    (text_out, InlineKeyboardMarkup::new(rows))
}

/// Builds the Variant / Effort picker card for one Model Family (page 2 of the Model selector).
#[must_use]
pub fn build_model_variant_card(
    family_slug: &str,
    session: &ChatSession,
    overview: Option<&QuotaOverview>,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let Some(family) = find_model_family(family_slug) else {
        return build_model_family_card(
            session,
            overview,
            Some("⚠️ 该模型家族列表已更新，请重新选择。"),
        );
    };

    let pool_badge = overview
        .and_then(|ov| ov.compact_badge(family.quota_pool))
        .unwrap_or_else(|| "未查询".to_string());
    let active_label = model_display_label(&session.model);

    let mut text_out = String::with_capacity(640);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let _ = write!(
        text_out,
        "{} <b>{} · 选择型号 / 思考档位</b>\n\n\
        • <b>当前生效</b>: <code>{}</code>\n\
        • <b>所属额度池</b>: {} (<code>{}</code>)\n\
        • <b>模型数量</b>: {} 个实时可用变体\n\n\
        <i>选择下方变体将精确同步至会话与 <code>agy --model</code>：</i>",
        family.icon,
        text::escape_html(&family.title),
        text::escape_html(&active_label),
        family.quota_pool.display_name(),
        text::escape_html(&pool_badge),
        family.models.len()
    );

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let per_row = if family
        .models
        .iter()
        .all(|m| m.variant_label.chars().count() <= 10)
    {
        2
    } else {
        1
    };
    let mut row = Vec::with_capacity(per_row);
    for entry in &family.models {
        let is_active = !session.model.is_empty() && session.model.eq_ignore_ascii_case(&entry.id);
        let dyn_tag = if entry.is_dynamic { " 🆕" } else { "" };
        let btn_label = format!("{}{}{dyn_tag}", active_mark(is_active), entry.variant_label);
        row.push(cb_btn(btn_label, encode_model_callback(&entry.id)));
        if row.len() == per_row {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }

    rows.push(vec![
        cb_btn("« 返回模型家族", "cp:mf"),
        cb_btn("« 返回控制中心", "cp:home"),
    ]);

    (text_out, InlineKeyboardMarkup::new(rows))
}

#[must_use]
pub fn build_effort_card(
    session: &ChatSession,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let mut text_out = String::with_capacity(420);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let note = if session.model.is_empty() {
        "默认模型 (Gemini 4 Carbon) 支持 <code>low / medium / high</code>；若选 <code>xhigh</code> 将在 CLI 自动对齐为 <code>high</code>，如需 XHigh 请切换至 Gemini 4 Hydrogen。"
    } else if effort_from_model_suffix(&session.model).is_some() {
        "当前模型含档位后缀；切换 Effort 时会自动匹配同家族对应后缀型号，保持 UI 与 <code>agy --model</code> 严格一致。"
    } else if !supports_effort_flag(&session.model) {
        "当前模型 (Claude / Orchestrator / GPT-OSS) 内置固定推理预算，<code>agy</code> 不允许附加 <code>--effort</code>，此设置将保留给支持调节的模型。"
    } else {
        "调整推理思考深度，将在下一轮 <code>agy</code> 执行时生效。"
    };
    let _ = write!(
        text_out,
        "⚡ <b>思考力度设置 (Thinking Effort)</b>\n\n\
        • <b>当前力度</b>: <code>{}</code>\n\
        • <b>当前模型</b>: <code>{}</code>\n\n\
        <i>💡 {note}</i>",
        text::escape_html(&session.effort),
        text::escape_html(&model_display_label(&session.model)),
    );
    (text_out, get_effort_keyboard_with_active(&session.effort))
}

#[must_use]
pub fn get_effort_keyboard_with_active(active: &str) -> InlineKeyboardMarkup {
    let btn = |label: &str, key: &str| {
        cb_btn(
            format!("{}{label}", active_mark(active.eq_ignore_ascii_case(key))),
            format!("se:{key}"),
        )
    };
    InlineKeyboardMarkup::new(vec![
        vec![
            btn("Low (极速)", "low"),
            btn("Medium (均衡)", "medium"),
            btn("High (深度)", "high"),
        ],
        vec![btn("XHigh (超深度)", "xhigh"), btn("Max (极限)", "max")],
        vec![cb_btn("« 返回控制中心", "cp:home")],
    ])
}

#[must_use]
pub fn build_mode_card(
    session: &ChatSession,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let mut text_out = String::with_capacity(380);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let _ = write!(
        text_out,
        "🎯 <b>Agent 执行模式 (<code>--mode</code>)</b>\n\n\
        • <b>当前模式</b>: <code>{}</code>\n\n\
        • <code>auto</code>: 全自主决策 (自动编辑 + 沙盒外命令智能审查)\n\
        • <code>accept-edits</code>: 自动应用文件编辑 (推荐)\n\
        • <code>plan</code>: 只读规划方案，不直接修改文件",
        text::escape_html(&session.mode),
    );
    (text_out, get_mode_keyboard_with_active(&session.mode))
}

#[must_use]
pub fn get_mode_keyboard_with_active(active: &str) -> InlineKeyboardMarkup {
    let btn = |label: &str, key: &str| {
        cb_btn(
            format!("{}{label}", active_mark(active.eq_ignore_ascii_case(key))),
            format!("sm:{key}"),
        )
    };
    InlineKeyboardMarkup::new(vec![
        vec![
            btn("Auto (全自主)", "auto"),
            btn("Accept Edits (自动编辑)", "accept-edits"),
        ],
        vec![
            btn("Plan (仅规划)", "plan"),
            cb_btn("« 返回控制中心", "cp:home"),
        ],
    ])
}

/// Builds the main multi-page Control Panel dashboard (`cp:home` / `/panel` / `/settings` / `/status`).
#[must_use]
pub fn build_control_panel_home(
    chat_id: i64,
    session: &ChatSession,
    agy_version: &str,
    overview: Option<&QuotaOverview>,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let conv_status = session
        .conversation_id
        .as_deref()
        .map_or("初始会话 (未创建 ID)".to_string(), |id| {
            text::take_chars(id, 12).to_string()
        });
    let current_model_label = model_display_label(&session.model);
    let current_model_id = if session.model.is_empty() {
        "default"
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
    let voice_status = if session.voice_mode {
        "✅ 开启"
    } else {
        "❌ 关闭"
    };
    let sandbox_status = if session.sandbox {
        "🛡️ 开启"
    } else {
        "⚡ 关闭"
    };
    let run_status = if session.is_running {
        format!(
            "⏳ 运行中 (PID {})",
            session
                .active_pid
                .map_or_else(|| "-".to_string(), |p| p.to_string())
        )
    } else {
        "🟢 空闲待命".to_string()
    };

    let mut status_text = String::with_capacity(1024);
    if let Some(msg) = banner {
        let _ = writeln!(status_text, "{msg}\n");
    }
    let _ = write!(
        status_text,
        "🎛 <b>agygram 控制中心</b> <code>v{}</code> · <code>agy v{}</code>\n\n\
        • <b>模型</b>: <b>{}</b> (<code>{}</code>)\n\
        • <b>力度 / 模式</b>: <code>{}</code> · <code>{}</code>\n\
        • <b>实时额度</b>: {}\n\
        • <b>会话</b>: <code>{}</code> · {} 轮 ({} in / {} out tok)\n\
        • <b>工作区</b>: <code>{}</code>{}\n\
        • <b>角色 / 工程</b>: <code>{}</code> · <code>{}</code>\n\
        • <b>沙盒 / 语音</b>: {} · {}\n\
        • <b>Chat / 状态</b>: <code>{}</code> · {}",
        env!("CARGO_PKG_VERSION"),
        text::escape_html(agy_version),
        text::escape_html(&current_model_label),
        text::escape_html(current_model_id),
        text::escape_html(&session.effort),
        text::escape_html(&session.mode),
        format_quota_inline_summary(overview),
        text::escape_html(&conv_status),
        session.turn_count,
        session.total_input_tokens,
        session.total_output_tokens,
        text::escape_html(&session.workspace),
        if session.extra_dirs.is_empty() {
            String::new()
        } else {
            format!(" (+{})", session.extra_dirs.len())
        },
        text::escape_html(current_agent),
        text::escape_html(project_str),
        sandbox_status,
        voice_status,
        chat_id,
        run_status,
    );

    let short_m = short_model_badge(&session.model);
    let voice_btn = if session.voice_mode {
        "🔊 输出/TTS (开)"
    } else {
        "🔇 输出/TTS (关)"
    };
    let action_btn = if session.is_running {
        cb_btn("🛑 中断当前任务", "cp:cancel")
    } else {
        cb_btn("🆕 开启全新会话", "cp:new")
    };

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            cb_btn(format!("🤖 模型 · {short_m}"), "cp:mf"),
            cb_btn("📊 实时额度", "cp:quota"),
        ],
        vec![
            cb_btn(format!("⚡ 力度: {}", session.effort), "cp:eff"),
            cb_btn(format!("🎯 模式: {}", session.mode), "cp:mode"),
        ],
        vec![
            cb_btn("💬 会话管理", "cp:sess"),
            cb_btn("📂 工作区 & 角色", "cp:ws"),
        ],
        vec![
            cb_btn(voice_btn, "cp:out"),
            cb_btn("🔄 刷新面板", "cp:home:r"),
        ],
        vec![cb_btn("🩺 系统与生态", "cp:sys"), action_btn],
    ]);

    (status_text, keyboard)
}

#[must_use]
pub fn build_session_card(
    session: &ChatSession,
    recents: &[RecentSessionInfo],
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let mut header = String::with_capacity(512);
    if let Some(msg) = banner {
        let _ = writeln!(header, "{msg}\n");
    }
    let conv_disp = session
        .conversation_id
        .as_deref()
        .unwrap_or("初始会话 (尚未产生 ID)");
    let _ = write!(
        header,
        "💬 <b>会话管理中心</b>\n\n\
        • <b>当前会话 ID</b>: <code>{}</code>\n\
        • <b>对话轮次</b>: {} 轮 · <b>累计耗时</b>: {:.1}s\n\
        • <b>Token 统计</b>: {} in / {} out / {} think / {} cache\n\
        • <b>首轮状态</b>: {}\n\n",
        text::escape_html(conv_disp),
        session.turn_count,
        session.total_duration_seconds,
        session.total_input_tokens,
        session.total_output_tokens,
        session.total_thinking_tokens,
        session.total_cache_read_tokens,
        if session.is_first_turn {
            "新对话起点"
        } else {
            "上下文接续中"
        },
    );

    if recents.is_empty() {
        header.push_str("<i>暂未扫描到 VPS 历史会话目录。</i>");
    } else {
        header.push_str("<b>📜 点击下方最近会话即可一键切换/接续：</b>");
    }

    let mut rows = Vec::new();
    for (idx, r) in recents.iter().enumerate() {
        let is_current = session.conversation_id.as_deref() == Some(r.id.as_str());
        let mark = active_mark(is_current);
        let flags = match (r.has_plan, r.has_walkthrough) {
            (true, true) => " [P+W]",
            (true, false) => " [P]",
            (false, true) => " [W]",
            (false, false) => "",
        };
        let _ = writeln!(
            header,
            "\n{mark}{}. <b>{}</b>{} · <code>{}</code> ({})",
            idx + 1,
            text::escape_html(&r.title),
            flags,
            text::escape_html(text::take_chars(&r.id, 8)),
            text::escape_html(&r.updated_time)
        );
        rows.push(vec![cb_btn(
            format!("{mark}{}. {}", idx + 1, text::truncate_chars(&r.title, 22)),
            encode_resume_callback(&r.id),
        )]);
    }

    rows.push(vec![
        cb_btn("🔄 接续最近 (-c)", "rs:continue"),
        cb_btn("🆕 新建会话 (/new)", "cp:new"),
    ]);
    rows.push(vec![
        cb_btn("📋 查看 /plan", "cp:plan"),
        cb_btn("📝 查看 /walkthrough", "cp:wt"),
    ]);
    if session.is_running {
        rows.push(vec![
            cb_btn("🛑 强行中断 (/cancel)", "cp:cancel"),
            cb_btn("« 返回控制中心", "cp:home"),
        ]);
    } else {
        rows.push(vec![cb_btn("« 返回控制中心", "cp:home")]);
    }

    (header, InlineKeyboardMarkup::new(rows))
}

#[must_use]
pub fn build_workspace_card(
    session: &ChatSession,
    agents: &[String],
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let mut text_out = String::with_capacity(640);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let extra_disp = if session.extra_dirs.is_empty() {
        "无".to_string()
    } else {
        session.extra_dirs.join(", ")
    };
    let sys_preview = if session.system_prompt.trim().is_empty() {
        "系统默认".to_string()
    } else {
        text::truncate_chars(session.system_prompt.trim(), 60).into_owned()
    };

    let _ = write!(
        text_out,
        "📂 <b>工作区与角色设定</b>\n\n\
        • <b>主目录 (<code>/cd</code>)</b>: <code>{}</code>\n\
        • <b>附加目录 (<code>/adddir</code>)</b>: <code>{}</code>\n\
        • <b>绑定工程 (<code>/project</code>)</b>: <code>{}</code>\n\
        • <b>当前 Agent (<code>/agent</code>)</b>: <code>{}</code>\n\
        • <b>System Prompt (<code>/system</code>)</b>: <code>{}</code>\n\
        • <b>沙盒隔离 (<code>--sandbox</code>)</b>: {}",
        text::escape_html(&session.workspace),
        text::escape_html(&extra_disp),
        text::escape_html(if session.project.is_empty() {
            "默认"
        } else {
            &session.project
        }),
        text::escape_html(if session.agent.is_empty() {
            "默认 Agent"
        } else {
            &session.agent
        }),
        text::escape_html(&sys_preview),
        if session.sandbox {
            "🛡️ 开启 (受限沙盒)"
        } else {
            "⚡ 关闭 (完整权限)"
        }
    );

    let mut rows = Vec::new();
    if !agents.is_empty() {
        let mut agent_row = Vec::with_capacity(2);
        for agent_name in agents.iter().take(6) {
            let is_active = session.agent == *agent_name;
            agent_row.push(cb_btn(
                format!(
                    "{}🕵️ {}",
                    active_mark(is_active),
                    text::truncate_chars(agent_name, 14)
                ),
                encode_agent_callback(agent_name),
            ));
            if agent_row.len() == 2 {
                rows.push(std::mem::take(&mut agent_row));
            }
        }
        if !agent_row.is_empty() {
            rows.push(agent_row);
        }
    }

    rows.push(vec![
        cb_btn(
            if session.sandbox {
                "🔓 关闭沙盒"
            } else {
                "🛡️ 开启沙盒"
            },
            "cp:tsbx",
        ),
        cb_btn("♻️ 恢复默认工作区", "cp:defws"),
    ]);
    rows.push(vec![
        cb_btn("🧹 清空附加目录", "cp:cldirs"),
        cb_btn("🕵️ 重置默认 Agent", "cp:clagent"),
    ]);
    rows.push(vec![
        cb_btn("🧠 清空 System Prompt", "cp:clsys"),
        cb_btn("📁 解绑 Project", "cp:clproj"),
    ]);
    rows.push(vec![cb_btn("« 返回控制中心", "cp:home")]);

    (text_out, InlineKeyboardMarkup::new(rows))
}

#[must_use]
pub fn build_output_tts_card(
    config: &Config,
    session: &ChatSession,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    let mut text_out = String::with_capacity(480);
    if let Some(msg) = banner {
        let _ = writeln!(text_out, "{msg}\n");
    }
    let has_fish_key = config
        .fish_api_key
        .as_deref()
        .is_some_and(|k| !k.trim().is_empty());
    let effective_voice = session
        .voice_reference_id
        .as_deref()
        .or(config.fish_reference_id.as_deref())
        .unwrap_or("Fish Audio 默认音色");

    let _ = write!(
        text_out,
        "🔊 <b>输出流与语音 (TTS) 设置</b>\n\n\
        • <b>常驻语音回复</b>: {}\n\
        • <b>Fish Audio Key</b>: {}\n\
        • <b>当前音色 ID</b>: <code>{}</code>\n\
        • <b>流式刷新间隔</b>: <code>{} ms</code>\n\
        • <b>新消息自动打断</b>: {}\n\n\
        <i>💡 发送 <code>/voice id &lt;音色ID&gt;</code> 可更换音色，或直接发 <code>/voice &lt;问题&gt;</code> 单次语音。</i>",
        if session.voice_mode {
            "✅ 已开启 (自动附带 OGG 语音)"
        } else {
            "❌ 已关闭 (纯文本流式)"
        },
        if has_fish_key {
            "✅ 已配置"
        } else {
            "⚪ 未配置 (需在 config.toml 配置 fish_api_key)"
        },
        text::escape_html(effective_voice),
        config.draft_update_interval_ms,
        if config.interrupt_on_new_message {
            "✅ 开启"
        } else {
            "⏳ 排队等待"
        }
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            cb_btn(
                if session.voice_mode {
                    "🔇 关闭常驻语音"
                } else {
                    "🔊 开启常驻语音"
                },
                "cp:tvoice",
            ),
            cb_btn("🎙 重置默认音色", "cp:rvoice"),
        ],
        vec![cb_btn("« 返回控制中心", "cp:home")],
    ]);

    (text_out, keyboard)
}

#[must_use]
pub fn build_system_status_card(
    config: &Config,
    session: &ChatSession,
    agy_version: &str,
) -> (String, InlineKeyboardMarkup) {
    let models_count = crate::models::list_all_models().len();
    let dynamic_count = crate::models::list_all_models()
        .iter()
        .filter(|m| m.is_dynamic)
        .count();
    let has_proxy = config
        .proxy_url
        .as_deref()
        .is_some_and(|p| !p.trim().is_empty());
    let platforms = match (config.has_telegram(), config.has_qq()) {
        (true, true) => "Telegram + QQ 双栈并行",
        (true, false) => "Telegram",
        (false, true) => "QQ Bot",
        (false, false) => "未配置",
    };

    let text_out = format!(
        "🩺 <b>守护进程与 Antigravity CLI 状态</b>\n\n\
        • <b>agygram 版本</b>: <code>v{}</code>\n\
        • <b>agy CLI 版本</b>: <code>v{}</code>\n\
        • <b>CLI 二进制路径</b>: <code>{}</code>\n\
        • <b>运行平台</b>: {platforms}\n\
        • <b>模型目录</b>: {models_count} 个已注册 (含 {dynamic_count} 个动态发现)\n\
        • <b>当前会话状态</b>: {}\n\
        • <b>看门狗阈值</b>: 空闲 {}s · 上限 {}s\n\
        • <b>HTTP 代理</b>: {}",
        env!("CARGO_PKG_VERSION"),
        text::escape_html(agy_version),
        text::escape_html(&crate::executor::get_agy_path().display().to_string()),
        if session.is_running {
            "⏳ 执行中"
        } else {
            "🟢 空闲"
        },
        config.idle_timeout_secs,
        config.max_execution_secs,
        if has_proxy { "已启用" } else { "直连" }
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            cb_btn("🧩 查看 Skills", "cp:skills"),
            cb_btn("🛡️ 查看 Rules", "cp:rules"),
        ],
        vec![
            cb_btn("⚙️ MCP 服务器", "cp:mcp"),
            cb_btn("🔌 已装插件", "cp:plug"),
        ],
        vec![
            cb_btn("📢 CLI 更新日志", "cp:clog"),
            cb_btn("🎮 交互卡片 & 游戏", "card:hub"),
        ],
        vec![
            cb_btn("❓ 指令速查", "cp:help"),
            cb_btn("« 返回控制中心", "cp:home"),
        ],
    ]);

    (text_out, keyboard)
}

/// Builds the `/card` & `/widget` hub menu with built-in offline mini-games, demo cards,
/// and standalone `.html` downloads (all callbacks strictly <= 64 bytes).
#[must_use]
pub fn build_card_hub() -> (String, InlineKeyboardMarkup) {
    let text_out = "🎮 <b>QuickJS 交互卡片 & 离线小游戏中心</b>\n\n\
        支持 <b>Telegram Bot API 10.4+</b> 原生 QuickJS 气泡小组件与离线可玩小游戏：\n\
        • <b>直玩按钮</b>：私聊 (&lt;= 4 KB) 优先推送原生 <code>sendWidget</code> 交互气泡，群聊或客户端受限时自动降级为可玩 <code>.html</code> 附件或富文本卡片；\n\
        • <b>HTML 导出</b>：直接发送独立零依赖 <code>.html</code>，可在 Telegram 内置浏览器畅玩；\n\
        • <b>AI 自定义生成</b>：直接发送 <code>/card 写一个五子棋</code> 或 <code>/card 复利计算器</code> 让 Antigravity 为你现场生成！";

    let mut rows = Vec::new();
    let games = BuiltinGame::all();
    for pair in games.chunks(2) {
        let row = pair
            .iter()
            .map(|g| cb_btn(g.button_label(), format!("card:game:{}", g.id())))
            .collect();
        rows.push(row);
    }
    for pair in games.chunks(2) {
        let row = pair
            .iter()
            .map(|g| {
                cb_btn(
                    format!("📄 {}.html", g.id()),
                    format!("card:html:{}", g.id()),
                )
            })
            .collect();
        rows.push(row);
    }

    rows.push(vec![
        cb_btn("📊 运行看板卡", "card:demo:dash"),
        cb_btn("❓ QuickJS 速测", "card:demo:quiz"),
    ]);
    rows.push(vec![
        cb_btn("📋 通道对比表", "card:demo:table"),
        cb_btn("🃏 Agent 演示卡", "card:demo:agent"),
    ]);
    rows.push(vec![
        cb_btn("🩺 当前会话状态卡", "card:demo:status"),
        cb_btn("« 返回控制中心", "cp:home"),
    ]);

    (text_out.to_string(), InlineKeyboardMarkup::new(rows))
}

/// Builds a live status [`AgentCardData`] widget from a chat session.
#[must_use]
pub fn build_session_agent_card(session: &ChatSession) -> AgentCardData {
    let agent_name = if session.agent.is_empty() {
        "default"
    } else {
        &session.agent
    };
    AgentCardData::new(
        format!("agygram Agent ({agent_name})"),
        format!("Workspace: {}", session.workspace),
        format!(
            "{} (effort: {})",
            model_display_label(&session.model),
            session.effort
        ),
        if session.is_running {
            AgentStatus::Executing
        } else {
            AgentStatus::Finished
        },
        format!(
            "会话状态: {}\n会话 ID: {}\n模式: {} · 沙盒: {}\n交互轮次: {} ({} in / {} out tok)",
            if session.is_running {
                "运行中"
            } else {
                "就绪 (Ready)"
            },
            session.conversation_id.as_deref().unwrap_or("未创建/初始"),
            session.mode,
            if session.sandbox { "开启" } else { "关闭" },
            session.turn_count,
            session.total_input_tokens,
            session.total_output_tokens,
        ),
        vec![
            AgentCardStep::new(
                "工作区与工程路径",
                format!(
                    "主目录: {}\n附加目录: {}",
                    session.workspace,
                    if session.extra_dirs.is_empty() {
                        "无".to_string()
                    } else {
                        session.extra_dirs.join(", ")
                    }
                ),
                false,
            ),
            AgentCardStep::new(
                "模型与执行策略",
                format!(
                    "模型: {}\n力度: {} · 模式: {}\nAgent: {}",
                    model_display_label(&session.model),
                    session.effort,
                    session.mode,
                    agent_name
                ),
                true,
            ),
            AgentCardStep::new(
                "系统设定与语音",
                format!(
                    "System Prompt: {}\n语音模式: {}",
                    if session.system_prompt.is_empty() {
                        "系统默认"
                    } else {
                        "已自定义"
                    },
                    if session.voice_mode {
                        "开启"
                    } else {
                        "关闭"
                    }
                ),
                false,
            ),
        ],
    )
}

/// Resolves a `/card <target>` or `card:game:<id>` / `card:demo:<id>` token into an [`OutboundWidget`].
/// Returns `Ok(None)` when `target` is a free-form AI generation request.
pub async fn resolve_builtin_widget(
    target: &str,
    session_mgr: &SessionManager,
    chat_id: i64,
) -> Result<Option<OutboundWidget>, WidgetError> {
    let trimmed = target.trim();
    if let Some(game) = BuiltinGame::parse(trimmed) {
        return Ok(Some(OutboundWidget::from_builtin_game(game)?));
    }

    let lower = trimmed.to_ascii_lowercase();
    let widget = match lower.as_str() {
        "agent" | "demo" | "card" => OutboundWidget::from_agent_card(&AgentCardData::demo())?,
        "dash" | "dashboard" | "metric" | "metrics" | "看板" => {
            OutboundWidget::from_custom_card(&CustomCardSpec::demo_dashboard())?
        }
        "quiz" | "test" | "速测" | "测验" => {
            OutboundWidget::from_custom_card(&CustomCardSpec::demo_quiz())?
        }
        "table" | "cmp" | "compare" | "comparison" | "matrix" | "对比" => {
            OutboundWidget::from_custom_card(&CustomCardSpec::demo_comparison())?
        }
        "status" | "状态" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let card = build_session_agent_card(&session);
            OutboundWidget::from_agent_card(&card)?
        }
        _ => return Ok(None),
    };
    Ok(Some(widget))
}

#[must_use]
pub fn build_onboarding_card() -> (String, InlineKeyboardMarkup) {
    let text = format!(
        "🚀 <b>欢迎使用 agygram (Antigravity CLI Daemon for Telegram) v{}</b>\n\n\
        这是专为 <b>Google Antigravity CLI (agy)</b> 打造的高性能 Telegram 随身指挥中心。\n\n\
        <b>💡 极速交互指南：</b>\n\
        • 直接发送<b>文本、需求、代码文件或图片</b>，即可触发实时打字机流式推导；\n\
        • 发送 <code>/panel</code> 或 <code>/status</code> 可随时唤起多级交互控制中心；\n\
        • 发送 <code>/model</code> 可浏览实时模型家族、变体与额度百分比；\n\
        • 发送 <code>/session</code> 或 <code>/history</code> 可一键管理并接续历史会话。\n\n\
        <i>轻触下方按钮即可立即开始配置与探索：</i>",
        env!("CARGO_PKG_VERSION")
    );

    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            cb_btn("🎛 打开控制中心", "cp:home"),
            cb_btn("🤖 选择模型", "cp:mf"),
        ],
        vec![
            cb_btn("📊 实时额度", "cp:quota"),
            cb_btn("💬 会话管理", "cp:sess"),
        ],
        vec![
            cb_btn("⚡ 思考力度", "cp:eff"),
            cb_btn("❓ 帮助指南", "cp:help"),
        ],
    ]);

    (text, keyboard)
}

/// Reject callback payloads that are >64 bytes, unknown, out of range, or could inject extra CLI flags.
/// Returns a human-readable reason when the payload must not be applied.
#[must_use]
pub fn reject_reason(data: &str) -> Option<&'static str> {
    if data.is_empty() || data.len() > 64 {
        return Some("⚠️ 按钮指令长度无效。");
    }

    if let Some(arg) = data.strip_prefix("set_model:") {
        if !is_valid_model(&normalize_model_name(arg)) {
            return Some("⚠️ 该模型不在受支持的列表内。");
        }
        return None;
    }
    if data.starts_with("ms:") || data.starts_with("mh:") {
        if decode_model_callback(data).is_none() {
            return Some("⚠️ 该模型不在受支持的列表内（可尝试刷新模型列表）。");
        }
        return None;
    }
    if let Some(slug) = data.strip_prefix("cp:mv:") {
        if slug.is_empty() || !is_safe_cli_value(slug) {
            return Some("⚠️ 无效的模型家族标识。");
        }
        return None;
    }
    if let Some(arg) = data
        .strip_prefix("set_effort:")
        .or_else(|| data.strip_prefix("se:"))
    {
        if !is_supported_effort(arg) {
            return Some("⚠️ 无效的 thinking effort。");
        }
        return None;
    }
    if let Some(arg) = data
        .strip_prefix("set_mode:")
        .or_else(|| data.strip_prefix("sm:"))
    {
        if !is_supported_mode(arg) {
            return Some("⚠️ 无效的执行模式。支持: auto, accept-edits, plan");
        }
        return None;
    }
    if data.starts_with("rs:") || data.starts_with("rh:") || data.starts_with("resume:") {
        if decode_resume_callback(data).is_none() {
            return Some("⚠️ 无效或已过期的会话按钮，请重新打开 /session。");
        }
        return None;
    }
    if data.starts_with("sa:") || data.starts_with("sh:") {
        if decode_agent_callback(data).is_none() {
            return Some("⚠️ 无效或已过期的 Agent 按钮，请重新打开工作区设置。");
        }
        return None;
    }
    if let Some(id) = data.strip_prefix("card:game:") {
        if BuiltinGame::parse(id).is_none() {
            return Some("⚠️ 未知的内置小游戏。");
        }
        return None;
    }
    if let Some(id) = data.strip_prefix("card:demo:") {
        if !matches!(id, "agent" | "dash" | "quiz" | "table" | "status") {
            return Some("⚠️ 未知的交互卡片模板。");
        }
        return None;
    }
    if let Some(id) = data.strip_prefix("card:html:") {
        if BuiltinGame::parse(id).is_none()
            && !matches!(id, "agent" | "dash" | "quiz" | "table" | "status")
        {
            return Some("⚠️ 未知的 HTML 导出目标。");
        }
        return None;
    }

    if matches!(
        data,
        "cp:home"
            | "cp:home:r"
            | "cp:mf"
            | "cp:mf:r"
            | "cp:eff"
            | "cp:mode"
            | "cp:sess"
            | "cp:ws"
            | "cp:out"
            | "cp:quota"
            | "cp:quota:r"
            | "cp:sys"
            | "cp:skills"
            | "cp:rules"
            | "cp:mcp"
            | "cp:plug"
            | "cp:clog"
            | "cp:help"
            | "cp:plan"
            | "cp:wt"
            | "cp:new"
            | "cp:cancel"
            | "cp:tsbx"
            | "cp:tsbx_h"
            | "cp:tvoice"
            | "cp:rvoice"
            | "cp:cldirs"
            | "cp:defws"
            | "cp:clagent"
            | "cp:clsys"
            | "cp:clproj"
            | "card:hub"
            // Legacy static routes retained for pinned messages
            | "menu:status"
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
        return None;
    }

    Some("⚠️ 未知的按钮操作。")
}

fn back_to_home_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![cb_btn("« 返回控制中心", "cp:home")]])
}

async fn edit_card_html(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
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

pub(crate) async fn render_home_card_data(
    chat_id: i64,
    session_mgr: &SessionManager,
    force_refresh: bool,
    banner: Option<&str>,
) -> (String, InlineKeyboardMarkup) {
    if force_refresh {
        let _ = refresh_models_now().await;
    } else {
        let _ = refresh_models_if_stale().await;
    }
    let overview = if force_refresh {
        QuotaFetcher::fetch_overview(true).await.ok()
    } else {
        QuotaFetcher::fetch_overview_quick(QUICK_QUOTA_TIMEOUT).await
    };
    let session = session_mgr.get_or_create(chat_id).await;
    let ver = get_cached_agy_version().await;
    build_control_panel_home(chat_id, &session, ver, overview.as_ref(), banner)
}

async fn render_and_edit_home_card(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    force_refresh: bool,
    banner: Option<&str>,
) {
    let (html, keyboard) = render_home_card_data(chat_id, session_mgr, force_refresh, banner).await;
    edit_card_html(bot, target_chat, msg_id, html, keyboard, "edit_panel_home").await;
}

async fn render_and_edit_model_families(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    force_refresh: bool,
    banner: Option<&str>,
) {
    if force_refresh {
        let _ = refresh_models_now().await;
    } else {
        let _ = refresh_models_if_stale().await;
    }
    let overview = if force_refresh {
        QuotaFetcher::fetch_overview(true).await.ok()
    } else {
        QuotaFetcher::fetch_overview_quick(QUICK_QUOTA_TIMEOUT).await
    };
    let session = session_mgr.get_or_create(chat_id).await;
    let (html, keyboard) = build_model_family_card(&session, overview.as_ref(), banner);
    edit_card_html(
        bot,
        target_chat,
        msg_id,
        html,
        keyboard,
        "menu_model_family",
    )
    .await;
}

async fn render_and_edit_model_variants(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    slug: &str,
    banner: Option<&str>,
) {
    let _ = refresh_models_if_stale().await;
    let overview = QuotaFetcher::fetch_overview_quick(QUICK_QUOTA_TIMEOUT).await;
    let session = session_mgr.get_or_create(chat_id).await;
    let (html, keyboard) = build_model_variant_card(slug, &session, overview.as_ref(), banner);
    edit_card_html(
        bot,
        target_chat,
        msg_id,
        html,
        keyboard,
        "menu_model_variant",
    )
    .await;
}

async fn render_and_edit_session_card(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    banner: Option<&str>,
) {
    let (session, recents) = tokio::join!(
        session_mgr.get_or_create(chat_id),
        AgyExecutor::list_recent_conversations(6),
    );
    let (html, keyboard) = build_session_card(&session, &recents, banner);
    edit_card_html(bot, target_chat, msg_id, html, keyboard, "menu_session").await;
}

async fn render_and_edit_workspace_card(
    bot: &Bot,
    target_chat: ChatId,
    msg_id: MessageId,
    chat_id: i64,
    session_mgr: &SessionManager,
    banner: Option<&str>,
) {
    let (session, agents) = tokio::join!(
        session_mgr.get_or_create(chat_id),
        AgyExecutor::list_agents_parsed(),
    );
    let (html, keyboard) = build_workspace_card(&session, &agents, banner);
    edit_card_html(bot, target_chat, msg_id, html, keyboard, "menu_workspace").await;
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

    if data.starts_with("ms:") || data.starts_with("mh:") {
        let Some(target_model) = decode_model_callback(&data) else {
            render_and_edit_model_families(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                true,
                Some("⚠️ 模型列表已刷新，请重新选择。"),
            )
            .await;
            return Ok(());
        };
        let inferred = apply_model_and_effort(&session_mgr, chat_id, &target_model).await;
        let label = model_display_label(&target_model);
        let notice = match inferred {
            Some(eff) => format!(
                "✅ <b>已切换模型:</b> <code>{}</code> (<code>{}</code> · Effort 同步为 <code>{}</code>)",
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
                "✅ <b>已切换模型:</b> <code>{}</code> (<code>{}</code>)",
                text::escape_html(&label),
                text::escape_html(&target_model)
            ),
        };
        if let Some(entry) = find_model(&target_model) {
            render_and_edit_model_variants(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                &entry.family_slug,
                Some(&notice),
            )
            .await;
        } else {
            render_and_edit_model_families(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                false,
                Some(&notice),
            )
            .await;
        }
        return Ok(());
    }

    if let Some(model_arg) = data.strip_prefix("set_model:") {
        let target_model = normalize_model_name(model_arg);
        apply_model_and_effort(&session_mgr, chat_id, &target_model).await;
        render_and_edit_home_card(
            &bot,
            target_chat,
            msg_id,
            chat_id,
            &session_mgr,
            false,
            None,
        )
        .await;
        return Ok(());
    }

    if let Some(slug) = data.strip_prefix("cp:mv:") {
        render_and_edit_model_variants(
            &bot,
            target_chat,
            msg_id,
            chat_id,
            &session_mgr,
            slug,
            None,
        )
        .await;
        return Ok(());
    }

    if let Some(eff_raw) = data
        .strip_prefix("se:")
        .or_else(|| data.strip_prefix("set_effort:"))
    {
        if let Ok(effort) = eff_raw.parse::<Effort>() {
            let (synced_model, changed) =
                session_mgr.set_effort_and_sync_model(chat_id, effort).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let notice = if changed {
                format!(
                    "✅ <b>思考力度设为</b> <code>{}</code> · <b>模型已同步为</b> <code>{}</code>",
                    effort.as_str(),
                    text::escape_html(&synced_model)
                )
            } else if !supports_effort_flag(&session.model) && !session.model.is_empty() {
                format!(
                    "✅ <b>默认思考力度已记录为</b> <code>{}</code> <i>(当前模型 {} 使用固定推理预算)</i>",
                    effort.as_str(),
                    text::escape_html(&session.model)
                )
            } else {
                format!("✅ <b>思考力度已设为:</b> <code>{}</code>", effort.as_str())
            };
            let (html, kb) = build_effort_card(&session, Some(&notice));
            edit_card_html(&bot, target_chat, msg_id, html, kb, "set_effort").await;
        }
        return Ok(());
    }

    if let Some(mode_raw) = data
        .strip_prefix("sm:")
        .or_else(|| data.strip_prefix("set_mode:"))
    {
        session_mgr.set_mode(chat_id, mode_raw.to_string()).await;
        let session = session_mgr.get_or_create(chat_id).await;
        let notice = format!(
            "✅ <b>执行模式已切换为:</b> <code>{}</code>",
            text::escape_html(mode_raw)
        );
        let (html, kb) = build_mode_card(&session, Some(&notice));
        edit_card_html(&bot, target_chat, msg_id, html, kb, "set_mode").await;
        return Ok(());
    }

    if data.starts_with("rs:") || data.starts_with("rh:") || data.starts_with("resume:") {
        if let Some(resume_id) = decode_resume_callback(&data) {
            if resume_id == "continue" {
                session_mgr.resume_conversation(chat_id, None).await;
                render_and_edit_session_card(
                    &bot,
                    target_chat,
                    msg_id,
                    chat_id,
                    &session_mgr,
                    Some("🔄 <b>已开启接续模式 (<code>-c/--continue</code>)</b>，下一条消息将接续最近一次对话。"),
                )
                .await;
            } else {
                session_mgr
                    .resume_conversation(chat_id, Some(resume_id.clone()))
                    .await;
                let notice = format!(
                    "✅ <b>已绑定历史会话:</b> <code>{}</code>",
                    text::escape_html(&resume_id)
                );
                render_and_edit_session_card(
                    &bot,
                    target_chat,
                    msg_id,
                    chat_id,
                    &session_mgr,
                    Some(&notice),
                )
                .await;
            }
        }
        return Ok(());
    }

    if data.starts_with("sa:") || data.starts_with("sh:") {
        if let Some(agent_name) = decode_agent_callback(&data) {
            session_mgr.set_agent(chat_id, agent_name.clone()).await;
            let notice = format!(
                "✅ <b>Agent 已切换为:</b> <code>{}</code>",
                text::escape_html(&agent_name)
            );
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(&notice),
            )
            .await;
        }
        return Ok(());
    }

    match data.as_str() {
        "cp:home" | "menu:status" => {
            render_and_edit_home_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                false,
                None,
            )
            .await;
        }
        "cp:home:r" => {
            render_and_edit_home_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                true,
                Some("🔄 <b>已同步最新模型与实时额度</b>"),
            )
            .await;
        }
        "cp:mf" | "menu:model" => {
            render_and_edit_model_families(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                false,
                None,
            )
            .await;
        }
        "cp:mf:r" => {
            render_and_edit_model_families(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                true,
                Some("🔄 <b>已刷新 <code>agy models</code> 实时列表与额度</b>"),
            )
            .await;
        }
        "cp:eff" | "menu:effort" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let (html, kb) = build_effort_card(&session, None);
            edit_card_html(&bot, target_chat, msg_id, html, kb, "menu_effort").await;
        }
        "cp:mode" | "menu:mode" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let (html, kb) = build_mode_card(&session, None);
            edit_card_html(&bot, target_chat, msg_id, html, kb, "menu_mode").await;
        }
        "cp:sess" | "menu:history" => {
            render_and_edit_session_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None)
                .await;
        }
        "cp:new" | "cmd:new" => {
            session_mgr.reset_conversation(chat_id).await;
            render_and_edit_session_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some("🆕 <b>会话已重置</b>，已清空上下文与统计。"),
            )
            .await;
        }
        "cp:cancel" => {
            let (pid, was_running) = session_mgr.cancel_task(chat_id).await;
            let msg_txt = if was_running {
                format!(
                    "🛑 <b>已发送中断信号</b> (PID: {})，会话锁已释放。",
                    pid.unwrap_or(0)
                )
            } else {
                "ℹ️ 当前没有运行中的任务。".to_string()
            };
            render_and_edit_session_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(&msg_txt),
            )
            .await;
        }
        "cp:ws" => {
            render_and_edit_workspace_card(&bot, target_chat, msg_id, chat_id, &session_mgr, None)
                .await;
        }
        "cp:tsbx" | "cmd:toggle_sandbox" => {
            let on = session_mgr.toggle_sandbox(chat_id).await;
            let notice = if on {
                "🛡️ <b>沙盒模式已开启</b>"
            } else {
                "⚡ <b>沙盒模式已关闭</b>"
            };
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(notice),
            )
            .await;
        }
        "cp:tsbx_h" => {
            let on = session_mgr.toggle_sandbox(chat_id).await;
            let notice = if on {
                "🛡️ <b>沙盒模式已开启</b>"
            } else {
                "⚡ <b>沙盒模式已关闭</b>"
            };
            render_and_edit_home_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                false,
                Some(notice),
            )
            .await;
        }
        "cp:cldirs" => {
            session_mgr.clear_extra_dirs(chat_id).await;
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some("🧹 <b>已清空所有附加工作目录</b>"),
            )
            .await;
        }
        "cp:defws" => {
            let default_ws = config.get_valid_workspace();
            session_mgr.set_workspace(chat_id, default_ws.clone()).await;
            let notice = format!(
                "♻️ <b>已恢复默认工作区:</b> <code>{}</code>",
                text::escape_html(&default_ws)
            );
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some(&notice),
            )
            .await;
        }
        "cp:clagent" => {
            session_mgr.set_agent(chat_id, String::new()).await;
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some("🕵️ <b>已重置为默认 Agent</b>"),
            )
            .await;
        }
        "cp:clsys" => {
            session_mgr.set_system_prompt(chat_id, String::new()).await;
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some("🧠 <b>已清空自定义 System Prompt</b>"),
            )
            .await;
        }
        "cp:clproj" => {
            session_mgr.set_project(chat_id, String::new()).await;
            render_and_edit_workspace_card(
                &bot,
                target_chat,
                msg_id,
                chat_id,
                &session_mgr,
                Some("📁 <b>已解绑自定义 Project</b>"),
            )
            .await;
        }
        "cp:out" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let (html, kb) = build_output_tts_card(&config, &session, None);
            edit_card_html(&bot, target_chat, msg_id, html, kb, "menu_output").await;
        }
        "cp:tvoice" | "cmd:toggle_voice" => {
            let enabled = session_mgr.toggle_voice_mode(chat_id).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let notice = if enabled {
                "🔊 <b>常驻语音回复已开启</b>"
            } else {
                "🔇 <b>常驻语音回复已关闭</b>"
            };
            let (html, kb) = build_output_tts_card(&config, &session, Some(notice));
            edit_card_html(&bot, target_chat, msg_id, html, kb, "toggle_voice").await;
        }
        "cp:rvoice" => {
            session_mgr.set_voice_reference(chat_id, None).await;
            let session = session_mgr.get_or_create(chat_id).await;
            let (html, kb) =
                build_output_tts_card(&config, &session, Some("🎙 <b>已恢复配置默认音色</b>"));
            edit_card_html(&bot, target_chat, msg_id, html, kb, "reset_voice").await;
        }
        "cp:quota" | "cp:quota:r" | "cmd:quota" => {
            let force = data == "cp:quota:r";
            let quota_html = match QuotaFetcher::format_models_and_quota_with_refresh(force).await {
                Ok(html) => html,
                Err(err) => {
                    warn!(chat_id, error = %err, "callback quota fetch failed");
                    format!(
                        "⚠️ <b>实时 Quota 获取异常:</b> <code>{}</code>\n\n<i>💡 可在 VPS 终端执行 <code>agygram quota</code> 快速验证。</i>",
                        text::escape_html(&err.to_string())
                    )
                }
            };
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    cb_btn("🔄 刷新额度", "cp:quota:r"),
                    cb_btn("🤖 切换模型", "cp:mf"),
                ],
                vec![cb_btn("« 返回控制中心", "cp:home")],
            ]);
            edit_or_send_html_chunks(&bot, target_chat, msg_id, &quota_html, Some(kb)).await;
        }
        "cp:sys" => {
            let (session, ver) =
                tokio::join!(session_mgr.get_or_create(chat_id), get_cached_agy_version());
            let (html, kb) = build_system_status_card(&config, &session, ver);
            edit_card_html(&bot, target_chat, msg_id, html, kb, "menu_sys").await;
        }
        "cp:skills" => {
            let html = match AgyExecutor::list_skill_sections().await {
                Ok(sections) => {
                    let mut out = String::from("🧩 <b>Antigravity 技能库 (Skills)</b>\n\n");
                    for (section, items) in sections {
                        let _ = writeln!(out, "📦 <b>{}</b>:", text::escape_html(section));
                        for (name, desc) in items {
                            if desc.is_empty() {
                                let _ =
                                    writeln!(out, "• <code>/{}</code>", text::escape_html(&name));
                            } else {
                                let short = text::truncate_chars(&desc, 50);
                                let _ = writeln!(
                                    out,
                                    "• <code>/{}</code> : <i>{}</i>",
                                    text::escape_html(&name),
                                    text::escape_html(&short)
                                );
                            }
                        }
                        out.push('\n');
                    }
                    out.trim_end().to_string()
                }
                Err(msg_str) => msg_str.to_string(),
            };
            edit_or_send_html_chunks(
                &bot,
                target_chat,
                msg_id,
                &html,
                Some(InlineKeyboardMarkup::new(vec![vec![
                    cb_btn("« 返回系统状态", "cp:sys"),
                    cb_btn("« 控制中心", "cp:home"),
                ]])),
            )
            .await;
        }
        "cp:rules" => {
            let session = session_mgr.get_or_create(chat_id).await;
            let html = match AgyExecutor::list_rule_sections(&session.workspace).await {
                Ok(sections) if !sections.is_empty() => {
                    let mut out = String::from("🛡️ <b>Antigravity 规则配置 (Rules)</b>\n\n");
                    for sec in sections {
                        let _ = writeln!(
                            out,
                            "{} <b>{}</b>:\n<pre><code>{}</code></pre>\n",
                            sec.icon,
                            text::escape_html(&sec.title),
                            text::escape_html(&sec.body)
                        );
                    }
                    out.trim_end().to_string()
                }
                Ok(_) => "🛡️ <b>当前暂无自定义规则文件。</b>".to_string(),
                Err(msg_str) => msg_str.to_string(),
            };
            edit_or_send_html_chunks(
                &bot,
                target_chat,
                msg_id,
                &html,
                Some(InlineKeyboardMarkup::new(vec![vec![
                    cb_btn("« 返回系统状态", "cp:sys"),
                    cb_btn("« 控制中心", "cp:home"),
                ]])),
            )
            .await;
        }
        "cp:mcp" => {
            let html = match AgyExecutor::run_mcp_cmd(&["list"]).await {
                Ok(out) => {
                    let body = if out.trim().is_empty() {
                        "暂无配置的 MCP 服务器。"
                    } else {
                        out.trim()
                    };
                    format!(
                        "⚙️ <b>MCP 服务器列表:</b>\n\n<pre><code>{}</code></pre>",
                        text::escape_html(body)
                    )
                }
                Err(err) => format!(
                    "❌ <b>MCP 查询失败:</b> <code>{}</code>",
                    text::escape_html(&err.to_string())
                ),
            };
            edit_or_send_html_chunks(
                &bot,
                target_chat,
                msg_id,
                &html,
                Some(InlineKeyboardMarkup::new(vec![vec![
                    cb_btn("« 返回系统状态", "cp:sys"),
                    cb_btn("« 控制中心", "cp:home"),
                ]])),
            )
            .await;
        }
        "cp:plug" => {
            let html = match AgyExecutor::list_plugins().await {
                Ok(out) => format!(
                    "🔌 <b>已安装的 agy 插件:</b>\n\n<pre><code>{}</code></pre>",
                    text::escape_html(&out)
                ),
                Err(err) => format!(
                    "❌ <b>插件查询失败:</b> <code>{}</code>",
                    text::escape_html(&err.to_string())
                ),
            };
            edit_or_send_html_chunks(
                &bot,
                target_chat,
                msg_id,
                &html,
                Some(InlineKeyboardMarkup::new(vec![vec![
                    cb_btn("« 返回系统状态", "cp:sys"),
                    cb_btn("« 控制中心", "cp:home"),
                ]])),
            )
            .await;
        }
        "cp:clog" => {
            let raw = AgyExecutor::show_changelog().await.unwrap_or_default();
            let clipped = text::truncate_chars(&raw, 3200);
            let html = format!(
                "📢 <b>Antigravity CLI 更新日志:</b>\n\n{}",
                TelegramFormatter::to_telegram_html(&clipped)
            );
            edit_or_send_html_chunks(
                &bot,
                target_chat,
                msg_id,
                &html,
                Some(InlineKeyboardMarkup::new(vec![vec![
                    cb_btn("« 返回系统状态", "cp:sys"),
                    cb_btn("« 控制中心", "cp:home"),
                ]])),
            )
            .await;
        }
        "cp:plan" | "cp:wt" => {
            let is_plan = data == "cp:plan";
            let session = session_mgr.get_or_create(chat_id).await;
            let body = if let Some(ref conv_id) = session.conversation_id {
                let doc = if is_plan {
                    AgyExecutor::read_implementation_plan(conv_id).await
                } else {
                    AgyExecutor::read_walkthrough(conv_id).await
                };
                match doc {
                    Some(md) => TelegramFormatter::to_telegram_html(&md),
                    None if is_plan => {
                        "📋 <b>当前会话尚未生成 implementation_plan.md。</b>".to_string()
                    }
                    None => "📝 <b>当前会话尚未生成 walkthrough.md。</b>".to_string(),
                }
            } else {
                "ℹ️ <b>当前为初始会话，尚未关联会话 ID。</b>".to_string()
            };
            send_html_chunks(&bot, target_chat, &body).await;
        }
        "cp:help" | "cmd:help_card" => {
            let ver = get_cached_agy_version().await;
            let help_text = format!(
                "📖 <b>agygram 核心指令速查 (v{} / agy v{})</b>\n\n\
                • /panel / /settings / /status : 多级交互式控制中心\n\
                • /model / /models : 实时模型家族选择器与列表\n\
                • /quota / /usage : Google Cloud 实时额度监控\n\
                • /session / /history / /resume : 会话管理与接续\n\
                • /new / /cancel : 重置上下文 / 中断运行任务\n\
                • /effort / /mode / /sandbox : 力度、模式与沙盒控制\n\
                • /plan / /walkthrough : 架构方案与执行报告\n\
                • /skills / /rules / /mcp / /plugins : 定制化生态\n\
                • /cd / /adddir / /dirs / /file : 工作目录与文件直传\n\
                • /voice / /card : 语音回复与 QuickJS 交互卡片",
                env!("CARGO_PKG_VERSION"),
                text::escape_html(ver)
            );
            edit_card_html(
                &bot,
                target_chat,
                msg_id,
                help_text,
                back_to_home_keyboard(),
                "cmd_help_card",
            )
            .await;
        }
        "card:hub" => {
            let (card_text, keyboard) = build_card_hub();
            edit_card_html(&bot, target_chat, msg_id, card_text, keyboard, "card_hub").await;
        }
        other if other.starts_with("card:") => {
            Box::pin(handle_card_callback(
                &bot,
                &config,
                &session_mgr,
                target_chat,
                other,
            ))
            .await;
        }
        _ => {}
    }

    Ok(())
}

async fn handle_card_callback(
    bot: &Bot,
    config: &Config,
    session_mgr: &SessionManager,
    target_chat: ChatId,
    data: &str,
) {
    if let Some(target) = data
        .strip_prefix("card:game:")
        .or_else(|| data.strip_prefix("card:demo:"))
    {
        match resolve_builtin_widget(target, session_mgr, target_chat.0).await {
            Ok(Some(widget)) => {
                deliver_widget_with_fallback(bot, &config.bot_token, target_chat, &widget).await;
            }
            Ok(None) => {
                send_html_chunks(bot, target_chat, "⚠️ 未知的卡片或小游戏标识。").await;
            }
            Err(err) => {
                let err_html = format!(
                    "❌ <b>渲染交互卡片失败:</b> <code>{}</code>",
                    text::escape_html(&err.to_string())
                );
                send_html_chunks(bot, target_chat, &err_html).await;
            }
        }
    } else if let Some(target) = data.strip_prefix("card:html:") {
        match resolve_builtin_widget(target, session_mgr, target_chat.0).await {
            Ok(Some(widget)) => {
                if let Err(err) = send_widget_html_file(bot, target_chat, &widget).await {
                    log_telegram_err(target_chat.0, "send_widget_html_file", err);
                }
            }
            Ok(None) => {
                send_html_chunks(bot, target_chat, "⚠️ 未知的 HTML 导出目标。").await;
            }
            Err(err) => {
                let err_html = format!(
                    "❌ <b>构建独立 HTML 失败:</b> <code>{}</code>",
                    text::escape_html(&err.to_string())
                );
                send_html_chunks(bot, target_chat, &err_html).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SUPPORTED_MODELS, list_all_models};

    fn assert_keyboard_bytes_within_64(kb: &InlineKeyboardMarkup) {
        for row in &kb.inline_keyboard {
            for btn in row {
                if let teloxide::types::InlineKeyboardButtonKind::CallbackData(ref data) = btn.kind
                {
                    assert!(
                        data.len() <= 64,
                        "callback_data exceeded 64 bytes ({}): {:?}",
                        data.len(),
                        data
                    );
                    assert!(
                        reject_reason(data).is_none(),
                        "rendered button callback_data must pass reject_reason: {:?}",
                        data
                    );
                }
            }
        }
    }

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
        assert!(reject_reason("ms:gemini-4-carbon-high").is_none());
        assert!(reject_reason("ms:gemini-pro-orchestrator-argon").is_none());
        assert!(reject_reason("ms:claude-opus-4-6-thinking").is_none());
        assert!(reject_reason("ms:default").is_none());
        assert!(reject_reason("ms:gpt-4o").is_some());
        assert!(reject_reason("set_model:gemini-3.8-flash-high").is_none());
        assert!(reject_reason("set_model:gemini-4-argon-medium").is_none());
        assert!(reject_reason("set_model:claude-opus-5-5-high").is_none());
        assert!(reject_reason("set_model:default").is_none());
        assert!(reject_reason("set_model:gpt-4o").is_some());
        assert!(reject_reason("se:max").is_none());
        assert!(reject_reason("set_effort:max").is_none());
        assert!(reject_reason("set_effort:ultra").is_some());
        assert!(reject_reason("sm:auto").is_none());
        assert!(reject_reason("set_mode:accept-edits").is_none());
        assert!(reject_reason("set_mode:--sandbox").is_some());
        assert!(reject_reason("rs:8de907d6-1c2b").is_none());
        assert!(reject_reason("rs:continue").is_none());
        assert!(reject_reason("resume:--conversation evil").is_some());
        assert!(reject_reason("cp:home").is_none());
        assert!(reject_reason("cp:mf").is_none());
        assert!(reject_reason("cp:mv:carbon").is_none());
        assert!(reject_reason("menu:status").is_none());
        assert!(reject_reason("cmd:new").is_none());
        assert!(reject_reason("cmd:rm_rf").is_some());
        assert!(reject_reason("").is_some());
        assert!(reject_reason(&"a".repeat(65)).is_some());
    }

    #[test]
    fn all_control_panel_pages_produce_valid_callbacks_under_64_bytes() {
        let mut session = ChatSession::new(
            "/home/user/proj".into(),
            "gemini-pro-orchestrator-argon".into(),
            "accept-edits".into(),
            "high".into(),
        );
        session.conversation_id = Some("8de907d6-1c2b-4a5f-9e3d-77f0a1b2c3d4".into());
        let cfg = Config::default();

        let (home_txt, home_kb) = build_control_panel_home(42, &session, "1.5.10", None, None);
        assert!(home_txt.contains("Orchestrator (Argon)"));
        assert!(home_txt.contains("gemini-pro-orchestrator-argon"));
        assert_keyboard_bytes_within_64(&home_kb);

        let (mf_txt, mf_kb) = build_model_family_card(&session, None, None);
        assert!(mf_txt.contains("模型控制中心"));
        assert_keyboard_bytes_within_64(&mf_kb);

        for fam in list_model_families() {
            let (_, mv_kb) = build_model_variant_card(&fam.slug, &session, None, None);
            assert_keyboard_bytes_within_64(&mv_kb);
        }

        let (_, eff_kb) = build_effort_card(&session, None);
        assert_keyboard_bytes_within_64(&eff_kb);

        let (_, mode_kb) = build_mode_card(&session, None);
        assert_keyboard_bytes_within_64(&mode_kb);

        let recents = vec![
            RecentSessionInfo {
                id: "8de907d6-1c2b-4a5f-9e3d-77f0a1b2c3d4".into(),
                title: "Short Conv".into(),
                updated_time: "11-18 00:10".into(),
                has_plan: true,
                has_walkthrough: false,
            },
            RecentSessionInfo {
                id: "very-long-conversation-id-exceeding-64-bytes-limit-1234567890abcdef1234567890"
                    .into(),
                title: "Long ID Conv".into(),
                updated_time: "11-18 00:05".into(),
                has_plan: false,
                has_walkthrough: true,
            },
        ];
        let (_, sess_kb) = build_session_card(&session, &recents, None);
        assert_keyboard_bytes_within_64(&sess_kb);

        let (_, ws_kb) = build_workspace_card(
            &session,
            &["reviewer".into(), "long-agent-name".into()],
            None,
        );
        assert_keyboard_bytes_within_64(&ws_kb);

        let (_, out_kb) = build_output_tts_card(&cfg, &session, None);
        assert_keyboard_bytes_within_64(&out_kb);

        let (_, sys_kb) = build_system_status_card(&cfg, &session, "1.5.10");
        assert_keyboard_bytes_within_64(&sys_kb);

        let (_, onb_kb) = build_onboarding_card();
        assert_keyboard_bytes_within_64(&onb_kb);

        let (hub_txt, hub_kb) = build_card_hub();
        assert!(hub_txt.contains("QuickJS"));
        assert_keyboard_bytes_within_64(&hub_kb);
    }

    #[test]
    fn every_builtin_model_roundtrips_through_callback_tokens() {
        for &(id, _) in SUPPORTED_MODELS {
            let tok = encode_model_callback(id);
            assert!(tok.len() <= 64, "model token > 64B: {tok}");
            assert!(reject_reason(&tok).is_none());
            assert_eq!(decode_model_callback(&tok).as_deref(), Some(id));
        }
        assert!(list_all_models().len() >= SUPPORTED_MODELS.len());
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
            vec![InlineKeyboardButton::callback("Back", "cp:home")],
        );
        assert!(card.contains("Fix &lt;script&gt; &amp; bug"));
        assert!(card.contains("[Plan+WT]"));
        assert!(card.contains("<code>8de907d6</code>"));
        assert_eq!(kb.inline_keyboard.len(), 2);
        assert_keyboard_bytes_within_64(&kb);
    }
}
