use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;
use tracing::{debug, warn};

/// Maximum raw HTML byte length allowed by Telegram Bot API 10.4 for in-message widgets (32 KB).
pub const MAX_RAW_HTML_BYTES: usize = 32 * 1024;

/// Maximum compressed HTML byte length allowed by Telegram Bot API 10.4 servers (4 KB).
#[allow(dead_code)]
pub const MAX_COMPRESSED_HTML_BYTES: usize = 4 * 1024;

const DEFAULT_TELEGRAM_API_BASE: &str = "https://api.telegram.org";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

static WIDGET_HTTP_CLIENT_STATE: OnceLock<RwLock<(Option<String>, Client)>> = OnceLock::new();

/// Error types encountered when creating, validating, or sending Telegram 10.4 widgets.
#[derive(Debug, thiserror::Error)]
pub enum WidgetError {
    #[error("HTML payload exceeds 32 KB limit ({size} bytes > {max} bytes)")]
    PayloadTooLarge { size: usize, max: usize },

    #[error("HTML payload cannot be empty")]
    EmptyPayload,

    #[error("Telegram 10.4 widgets are only supported in private chats (chat_id: {0})")]
    NonPrivateChat(i64),

    #[error("Telegram Bot API error ({code}): {description}")]
    ApiError { code: i32, description: String },

    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid widget parameter: {0}")]
    InvalidParameter(String),
}

/// Response returned by Telegram Bot API on successful `sendWidget`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WidgetMessageResponse {
    pub ok: bool,
    pub message_id: i32,
    pub chat_id: Option<i64>,
    pub date: Option<i64>,
}

/// Dynamic execution status of an Agent represented on the widget card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Thinking,
    Executing,
    Finished,
    Error,
}

impl AgentStatus {
    pub fn badge_class(&self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::Executing => "executing",
            Self::Finished => "finished",
            Self::Error => "error",
        }
    }

    pub fn display_label(&self) -> &'static str {
        match self {
            Self::Thinking => "思考中",
            Self::Executing => "工具执行中",
            Self::Finished => "已完成",
            Self::Error => "执行异常",
        }
    }

    pub fn indicator_html(&self) -> &'static str {
        match self {
            Self::Thinking => r#"<span class="pulse-dot"></span>"#,
            Self::Executing => r#"<span class="spinner-icon"></span>"#,
            Self::Finished => r#"<span class="check-icon">✓</span>"#,
            Self::Error => r#"<span class="alert-icon">!</span>"#,
        }
    }
}

/// A collapsible trace/timeline step on the Agent Card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardStep {
    pub title: String,
    pub details: String,
    pub is_expanded: bool,
}

impl AgentCardStep {
    pub fn new(title: impl Into<String>, details: impl Into<String>, is_expanded: bool) -> Self {
        Self {
            title: title.into(),
            details: details.into(),
            is_expanded,
        }
    }
}

/// Data model representing an interactive Agent Card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCardData {
    pub title: String,
    pub subtitle: String,
    pub model: String,
    pub status: AgentStatus,
    pub result: String,
    pub steps: Vec<AgentCardStep>,
}

impl AgentCardData {
    pub fn new(
        title: impl Into<String>,
        subtitle: impl Into<String>,
        model: impl Into<String>,
        status: AgentStatus,
        result: impl Into<String>,
        steps: Vec<AgentCardStep>,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: subtitle.into(),
            model: model.into(),
            status,
            result: result.into(),
            steps,
        }
    }

    /// Creates a rich, interactive demo card demonstrating Thinking, Tool Execution, and Results.
    pub fn demo() -> Self {
        Self {
            title: "agygram Agent".to_string(),
            subtitle: "Antigravity Swarm Coordinator".to_string(),
            model: "Claude 3.7 Sonnet (xhigh)".to_string(),
            status: AgentStatus::Finished,
            result: "✅ **Telegram 10.4 QuickJS Widget 架构验证成功**\n\n已成功集成 client-side QuickJS runtime sandbox 动态卡片，具备零外部依赖、自动暗色/亮色主题匹配、折叠式推理与多 Tab 切换。".to_string(),
            steps: vec![
                AgentCardStep::new(
                    "Step 1: 协议规格与沙盒边界推导",
                    "• 运行时环境: QuickJS Offline Sandbox\n• 限制条件: 纯内联 CSS/JS，无外网访问 (no XHR/fetch)\n• 容量上限: 原始 HTML <= 32 KB，服务器压缩后 <= 4 KB",
                    false,
                ),
                AgentCardStep::new(
                    "Step 2: 工具调用 — cargo check & unit tests",
                    "Command: cargo test --lib bot::widget\nOutput:\nrunning 6 tests\ntest bot::widget::tests::test_template_raw_size_within_limits ... ok\ntest bot::widget::tests::test_template_escaping ... ok\ntest bot::widget::tests::test_send_widget_payload_construction ... ok\ntest bot::widget::tests::test_edit_widget_data_payload_construction ... ok\ntest bot::widget::tests::test_error_types ... ok\ntest bot::widget::tests::test_agent_card_data_serialization ... ok\ntest result: ok. 6 passed; 0 failed",
                    true,
                ),
                AgentCardStep::new(
                    "Step 3: 优雅降级与响应式呈现",
                    "• 私聊环境: 原生渲染 TelegramWidget 交互卡片\n• 群聊/旧客户端: 自动捕获并无缝降级为标准 HTML 消息\n• 事件挂钩: 绑定 TelegramWidget.chatData / TelegramWidget.publicData",
                    false,
                ),
            ],
        }
    }

    /// Renders this card into a standalone, zero-dependency HTML document under 32 KB.
    pub fn render_html(&self) -> Result<String, WidgetError> {
        let mut html = String::with_capacity(4096);

        let safe_title = escape_html(&self.title);
        let safe_subtitle = escape_html(&self.subtitle);
        let safe_model = escape_html(&self.model);
        let safe_result = escape_html(&self.result);
        let status_badge_class = self.status.badge_class();
        let status_label = self.status.display_label();
        let status_indicator = self.status.indicator_html();

        html.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1,user-scalable=no\"><style>");
        html.push_str(CARD_INLINE_CSS);
        html.push_str("</style></head><body>");

        // Card container
        html.push_str("<div class=\"widget-card\">");

        // Header
        html.push_str("<div class=\"card-header\"><div class=\"agent-info\"><div class=\"agent-avatar\">🤖</div><div class=\"agent-meta\">");
        let _ = write!(
            html,
            "<div class=\"agent-title\">{safe_title}</div><div class=\"agent-subtitle\">{safe_subtitle} · {safe_model}</div></div></div>"
        );
        let _ = write!(
            html,
            "<div id=\"statusBadge\" class=\"status-badge {status_badge_class}\">{status_indicator} <span>{status_label}</span></div></div>"
        );

        // Tab Navigation
        html.push_str("<div class=\"tabs-nav\"><button class=\"tab-btn active\" data-tab=\"result\" onclick=\"switchTab('result')\">📋 结果 (Result)</button><button class=\"tab-btn\" data-tab=\"steps\" onclick=\"switchTab('steps')\">⏱️ 步骤 (Steps)</button></div>");

        // Tab Content
        html.push_str("<div class=\"tab-content\">");

        // Panel 1: Result
        let _ = write!(
            html,
            "<div id=\"panel-result\" class=\"tab-panel active\"><div id=\"resultBody\" class=\"result-body\">{safe_result}</div></div>"
        );

        // Panel 2: Steps Timeline Accordion
        html.push_str("<div id=\"panel-steps\" class=\"tab-panel\"><div class=\"accordion\">");
        for (idx, step) in self.steps.iter().enumerate() {
            let open_class = if step.is_expanded { " open" } else { "" };
            let safe_step_title = escape_html(&step.title);
            let safe_step_details = escape_html(&step.details);
            let _ = write!(
                html,
                "<div class=\"accordion-item{open_class}\"><div class=\"accordion-header\" onclick=\"toggleAccordion(this)\"><span class=\"step-title-text\">{safe_step_title}</span><span class=\"accordion-chevron\">▶</span></div><div class=\"accordion-body\"><pre>{safe_step_details}</pre></div></div>"
            );
            debug!(step_index = idx, "rendered step");
        }
        html.push_str("</div></div></div>");

        // Footer
        html.push_str("<div class=\"card-footer\"><span class=\"footer-brand\">⚡ Telegram 10.4 QuickJS Widget</span><span class=\"footer-time\">agygram</span></div>");

        html.push_str("</div>");

        // Inline QuickJS Logic
        html.push_str("<script>");
        html.push_str(CARD_INLINE_JS);
        html.push_str("</script></body></html>");

        validate_widget_html(&html)?;
        Ok(html)
    }

    /// Renders a graceful fallback Telegram HTML message when widget rendering is unavailable.
    pub fn render_fallback_html(&self) -> String {
        let mut msg = String::with_capacity(1024);
        let status_emoji = match self.status {
            AgentStatus::Thinking => "🤔",
            AgentStatus::Executing => "⚙️",
            AgentStatus::Finished => "✅",
            AgentStatus::Error => "❌",
        };

        let _ = write!(
            msg,
            "🎴 <b>{}</b>\n<b>模型:</b> <code>{}</code>\n<b>状态:</b> {} {}\n\n",
            crate::text::escape_html(&self.title),
            crate::text::escape_html(&self.model),
            status_emoji,
            self.status.display_label()
        );

        let _ = write!(
            msg,
            "<b>📝 执行结果:</b>\n{}\n\n",
            crate::text::escape_html(&self.result)
        );

        if !self.steps.is_empty() {
            msg.push_str("<b>🔍 执行步骤与时间线:</b>\n");
            for step in &self.steps {
                let _ = write!(
                    msg,
                    "• <b>{}</b>\n<blockquote expandable>{}</blockquote>\n",
                    crate::text::escape_html(&step.title),
                    crate::text::escape_html(&step.details)
                );
            }
        }

        msg.push_str("\n<i>(💡 提示: 当前客户端或网络环境不支持 Telegram 10.4 In-Message HTML Widget，已无缝降级为标准消息)</i>");
        msg
    }
}

/// Checks that the raw HTML size is within Bot API limits and non-empty.
pub fn validate_widget_html(html: &str) -> Result<(), WidgetError> {
    if html.trim().is_empty() {
        return Err(WidgetError::EmptyPayload);
    }
    if html.len() > MAX_RAW_HTML_BYTES {
        return Err(WidgetError::PayloadTooLarge {
            size: html.len(),
            max: MAX_RAW_HTML_BYTES,
        });
    }
    Ok(())
}

/// Escapes HTML special characters to prevent XSS / broken markup inside the QuickJS sandbox.
pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn build_widget_http_client(proxy_url: Option<&str>) -> Client {
    let mut builder = Client::builder()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .tcp_nodelay(true);

    if let Some(proxy) = proxy_url.map(str::trim).filter(|s| !s.is_empty()) {
        match reqwest::Proxy::all(proxy) {
            Ok(p) => {
                builder = builder.proxy(p);
            }
            Err(err) => {
                warn!("Invalid proxy URL for widget client {:?}: {err}", proxy);
            }
        }
    }

    builder.build().unwrap_or_else(|err| {
        warn!("Failed to build widget HTTP client: {err}; fallback to default");
        Client::new()
    })
}

/// Configures global HTTP proxy for Telegram widget calls.
pub fn configure_proxy(proxy_url: Option<&str>) {
    let normalized = proxy_url.map(str::trim).filter(|s| !s.is_empty());
    let state = WIDGET_HTTP_CLIENT_STATE
        .get_or_init(|| RwLock::new((None, build_widget_http_client(None))));

    let mut guard = match state.write() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };

    if guard.0.as_deref() != normalized {
        guard.0 = normalized.map(ToString::to_string);
        guard.1 = build_widget_http_client(normalized);
    }
}

/// Retrieves the cached reqwest client configured for widget requests.
pub fn get_widget_client() -> Client {
    let state = WIDGET_HTTP_CLIENT_STATE
        .get_or_init(|| RwLock::new((None, build_widget_http_client(None))));
    match state.read() {
        Ok(g) => g.1.clone(),
        Err(poisoned) => poisoned.into_inner().1.clone(),
    }
}

#[derive(Serialize)]
struct SendWidgetPayload<'a> {
    chat_id: i64,
    html: &'a str,
}

#[derive(Serialize)]
struct EditMessageWidgetDataPayload<'a> {
    chat_id: i64,
    message_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_data: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_data: Option<&'a serde_json::Value>,
}

#[derive(Deserialize)]
struct RawTelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    error_code: Option<i32>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct RawMessageResult {
    message_id: i32,
    chat: Option<RawChatResult>,
    date: Option<i64>,
}

#[derive(Deserialize)]
struct RawChatResult {
    id: i64,
}

/// Sends an in-message HTML widget to a private Telegram chat using Bot API 10.4.
pub async fn send_widget(
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
) -> Result<WidgetMessageResponse, WidgetError> {
    let client = get_widget_client();
    send_widget_with_client(
        &client,
        DEFAULT_TELEGRAM_API_BASE,
        bot_token,
        chat_id,
        html_content,
    )
    .await
}

/// Sends an in-message HTML widget with an explicit reqwest client and API base URL.
#[allow(dead_code)]
pub async fn send_widget_with_client(
    client: &Client,
    api_base: &str,
    bot_token: &str,
    chat_id: i64,
    html_content: &str,
) -> Result<WidgetMessageResponse, WidgetError> {
    validate_widget_html(html_content)?;

    // Telegram Bot API 10.4 constraint: widgets are private chat only in production
    if chat_id < 0 {
        return Err(WidgetError::NonPrivateChat(chat_id));
    }

    let url = format!("{api_base}/bot{bot_token}/sendWidget");
    let payload = SendWidgetPayload {
        chat_id,
        html: html_content,
    };

    let response = client.post(&url).json(&payload).send().await?;
    let status = response.status();
    let body_text = response.text().await?;

    let parsed: RawTelegramEnvelope<RawMessageResult> =
        serde_json::from_str(&body_text).map_err(|e| {
            debug!(body = %body_text, "Failed to parse Telegram API response JSON");
            WidgetError::Serialization(e)
        })?;

    if parsed.ok
        && let Some(msg) = parsed.result
    {
        return Ok(WidgetMessageResponse {
            ok: true,
            message_id: msg.message_id,
            chat_id: msg.chat.map(|c| c.id),
            date: msg.date,
        });
    }

    Err(WidgetError::ApiError {
        code: parsed.error_code.unwrap_or(status.as_u16() as i32),
        description: parsed
            .description
            .unwrap_or_else(|| "Unknown Telegram API error".to_string()),
    })
}

/// Updates the dynamic `chatData` or `publicData` of an in-message HTML widget via Telegram Bot API 10.4.
#[allow(dead_code)]
pub async fn edit_message_widget_data(
    bot_token: &str,
    chat_id: i64,
    message_id: i32,
    chat_data: Option<&serde_json::Value>,
    public_data: Option<&serde_json::Value>,
) -> Result<(), WidgetError> {
    let client = get_widget_client();
    edit_message_widget_data_with_client(
        &client,
        DEFAULT_TELEGRAM_API_BASE,
        bot_token,
        chat_id,
        message_id,
        chat_data,
        public_data,
    )
    .await
}

/// Updates widget data with an explicit reqwest client and API base URL.
#[allow(dead_code)]
pub async fn edit_message_widget_data_with_client(
    client: &Client,
    api_base: &str,
    bot_token: &str,
    chat_id: i64,
    message_id: i32,
    chat_data: Option<&serde_json::Value>,
    public_data: Option<&serde_json::Value>,
) -> Result<(), WidgetError> {
    if chat_data.is_none() && public_data.is_none() {
        return Err(WidgetError::InvalidParameter(
            "Either chat_data or public_data must be provided".to_string(),
        ));
    }

    let url = format!("{api_base}/bot{bot_token}/editMessageWidgetData");
    let payload = EditMessageWidgetDataPayload {
        chat_id,
        message_id,
        chat_data,
        public_data,
    };

    let response = client.post(&url).json(&payload).send().await?;
    let status = response.status();
    let body_text = response.text().await?;

    let parsed: RawTelegramEnvelope<serde_json::Value> =
        serde_json::from_str(&body_text).map_err(|e| {
            debug!(body = %body_text, "Failed to parse Telegram API response JSON");
            WidgetError::Serialization(e)
        })?;

    if parsed.ok {
        return Ok(());
    }

    Err(WidgetError::ApiError {
        code: parsed.error_code.unwrap_or(status.as_u16() as i32),
        description: parsed
            .description
            .unwrap_or_else(|| "Unknown Telegram API error".to_string()),
    })
}

// Inline CSS for Agent Card (< 1.5 KB minified, Telegram theme native)
const CARD_INLINE_CSS: &str = r#"
:root{--bg:var(--tg-theme-bg-color,#18222d);--text:var(--tg-theme-text-color,#fff);--hint:var(--tg-theme-hint-color,#7d8b99);--link:var(--tg-theme-link-color,#64b5f6);--btn:var(--tg-theme-button-color,#2b5278);--sec-bg:var(--tg-theme-secondary-bg-color,#131b24);--hdr-bg:var(--tg-theme-header-bg-color,#242f3d);--accent:var(--tg-theme-accent-text-color,#64b5f6);--border:rgba(255,255,255,0.08);--ok:#4caf50;--warn:#ff9800;--err:#f44336}
@media(prefers-color-scheme:light){:root{--bg:var(--tg-theme-bg-color,#fff);--text:var(--tg-theme-text-color,#222);--hint:var(--tg-theme-hint-color,#8e8e93);--link:var(--tg-theme-link-color,#2481cc);--btn:var(--tg-theme-button-color,#2481cc);--sec-bg:var(--tg-theme-secondary-bg-color,#f4f4f5);--hdr-bg:var(--tg-theme-header-bg-color,#f7f7f8);--accent:var(--tg-theme-accent-text-color,#2481cc);--border:rgba(0,0,0,0.08)}}
*{box-sizing:border-box;margin:0;padding:0}body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif;background:var(--bg);color:var(--text);padding:10px;font-size:13.5px;line-height:1.45}
.widget-card{background:var(--sec-bg);border:1px solid var(--border);border-radius:12px;overflow:hidden}
.card-header{padding:10px 12px;display:flex;align-items:center;justify-content:space-between;background:var(--hdr-bg);border-bottom:1px solid var(--border)}
.agent-info{display:flex;align-items:center;gap:8px}.agent-avatar{font-size:18px}.agent-meta{display:flex;flex-direction:column}
.agent-title{font-weight:600;font-size:13px}.agent-subtitle{font-size:11px;color:var(--hint)}
.status-badge{display:inline-flex;align-items:center;gap:5px;padding:3px 8px;border-radius:12px;font-size:11.5px;font-weight:500}
.status-badge.thinking{background:rgba(100,181,246,0.15);color:var(--accent)}
.status-badge.executing{background:rgba(255,152,0,0.15);color:var(--warn)}
.status-badge.finished{background:rgba(76,175,80,0.15);color:var(--ok)}
.status-badge.error{background:rgba(244,67,54,0.15);color:var(--err)}
.pulse-dot{width:7px;height:7px;border-radius:50%;background:currentColor;display:inline-block;animation:pulse 1.3s infinite ease-in-out}
.spinner-icon{width:10px;height:10px;border:2px solid currentColor;border-top-color:transparent;border-radius:50%;display:inline-block;animation:spin .8s linear infinite}
@keyframes pulse{0%,100%{opacity:1;transform:scale(1)}50%{opacity:.3;transform:scale(1.3)}}
@keyframes spin{to{transform:rotate(360deg)}}
.tabs-nav{display:flex;background:var(--hdr-bg);border-bottom:1px solid var(--border)}
.tab-btn{flex:1;padding:8px 0;border:none;background:transparent;color:var(--hint);font-size:12.5px;font-weight:500;cursor:pointer;border-bottom:2px solid transparent;transition:all .2s ease}
.tab-btn.active{color:var(--accent);border-bottom:2px solid var(--accent)}
.tab-content{padding:12px}.tab-panel{display:none}.tab-panel.active{display:block}
.result-body{font-size:13px;line-height:1.55;white-space:pre-wrap;word-break:break-word}
.accordion{display:flex;flex-direction:column;gap:6px}
.accordion-item{border:1px solid var(--border);border-radius:8px;background:var(--bg);overflow:hidden}
.accordion-header{padding:8px 10px;font-size:12px;font-weight:500;display:flex;justify-content:space-between;align-items:center;cursor:pointer;user-select:none}
.accordion-chevron{font-size:9px;color:var(--hint);transition:transform .2s ease}
.accordion-item.open .accordion-chevron{transform:rotate(90deg)}
.accordion-body{display:none;padding:8px 10px;font-size:11.5px;color:var(--hint);border-top:1px solid var(--border);background:rgba(0,0,0,0.02)}
.accordion-item.open .accordion-body{display:block}
.accordion-body pre{white-space:pre-wrap;word-break:break-all;font-family:ui-monospace,SFMono-Regular,Consolas,monospace}
.card-footer{padding:6px 12px;font-size:10.5px;color:var(--hint);border-top:1px solid var(--border);display:flex;justify-content:space-between;align-items:center}
"#;

// Inline QuickJS Logic (< 1.2 KB)
const CARD_INLINE_JS: &str = r#"
function switchTab(t){
  document.querySelectorAll('.tab-btn').forEach(function(b){b.classList.toggle('active',b.dataset.tab===t)});
  document.querySelectorAll('.tab-panel').forEach(function(p){p.classList.toggle('active',p.id==='panel-'+t)});
}
function toggleAccordion(h){
  h.parentElement.classList.toggle('open');
}
function applyWidgetState(d){
  if(!d)return;
  if(d.status){
    var b=document.getElementById('statusBadge');
    if(b){
      var s=d.status.toLowerCase();
      b.className='status-badge '+s;
      var icon=s==='thinking'?'<span class="pulse-dot"></span>':s==='executing'?'<span class="spinner-icon"></span>':s==='finished'?'<span>✓</span>':'<span>!</span>';
      var text=s==='thinking'?'思考中':s==='executing'?'工具执行中':s==='finished'?'已完成':'执行异常';
      b.innerHTML=icon+' <span>'+text+'</span>';
    }
  }
  if(d.result){
    var r=document.getElementById('resultBody');
    if(r)r.textContent=d.result;
  }
}
if(typeof TelegramWidget!=='undefined'){
  if(TelegramWidget.chatData)applyWidgetState(typeof TelegramWidget.chatData==='string'?JSON.parse(TelegramWidget.chatData):TelegramWidget.chatData);
  if(TelegramWidget.publicData)applyWidgetState(typeof TelegramWidget.publicData==='string'?JSON.parse(TelegramWidget.publicData):TelegramWidget.publicData);
  if(typeof TelegramWidget.onEvent==='function'){
    TelegramWidget.onEvent('data_updated',function(d){applyWidgetState(typeof d==='string'?JSON.parse(d):d)});
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_raw_size_within_limits() {
        let demo = AgentCardData::demo();
        let html = demo.render_html().expect("HTML render failed");

        // Max raw HTML is 32 KB (32,768 bytes)
        assert!(
            html.len() <= MAX_RAW_HTML_BYTES,
            "Raw HTML size {} bytes exceeded limit of {}",
            html.len(),
            MAX_RAW_HTML_BYTES
        );

        // Our lean template should easily be under 16 KB raw
        assert!(
            html.len() < 16 * 1024,
            "Template raw size {} bytes is unexpectedly large",
            html.len()
        );

        // Verify Telegram Bot API 10.4 compressed limit definition
        assert_eq!(MAX_COMPRESSED_HTML_BYTES, 4 * 1024);
    }

    #[test]
    fn test_edit_message_widget_data_validation() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let res = edit_message_widget_data("dummy_token", 123456, 1, None, None).await;
            match res {
                Err(WidgetError::InvalidParameter(msg)) => {
                    assert!(msg.contains("Either chat_data or public_data"));
                }
                other => panic!("Expected InvalidParameter error, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_template_escaping() {
        assert_eq!(
            escape_html("<script>alert(\"XSS & 'bad'\")</script>"),
            "&lt;script&gt;alert(&quot;XSS &amp; &#39;bad&#39;&quot;)&lt;/script&gt;"
        );

        let custom = AgentCardData::new(
            "<Special & Title>",
            "\"Subtitle's test\"",
            "Model <v2>",
            AgentStatus::Thinking,
            "Result with <tags> & \"quotes\"",
            vec![AgentCardStep::new(
                "Step <1>",
                "Details with & special > chars",
                false,
            )],
        );

        let html = custom.render_html().unwrap();
        assert!(!html.contains("<Special & Title>"));
        assert!(html.contains("&lt;Special &amp; Title&gt;"));
        assert!(!html.contains("<tags>"));
        assert!(html.contains("&lt;tags&gt;"));
    }

    #[test]
    fn test_send_widget_payload_construction() {
        let payload = SendWidgetPayload {
            chat_id: 123456789,
            html: "<b>Hello Widget</b>",
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        assert!(json_str.contains("\"chat_id\":123456789"));
        assert!(json_str.contains("\"html\":\"<b>Hello Widget</b>\""));
    }

    #[test]
    fn test_edit_widget_data_payload_construction() {
        let chat_val = serde_json::json!({ "status": "executing", "progress": 50 });
        let payload = EditMessageWidgetDataPayload {
            chat_id: 987654321,
            message_id: 42,
            chat_data: Some(&chat_val),
            public_data: None,
        };
        let json_str = serde_json::to_string(&payload).unwrap();
        assert!(json_str.contains("\"chat_id\":987654321"));
        assert!(json_str.contains("\"message_id\":42"));
        assert!(json_str.contains("\"chat_data\":{\"progress\":50,\"status\":\"executing\"}"));
        assert!(!json_str.contains("public_data"));
    }

    #[test]
    fn test_error_types() {
        // Empty payload
        let err = validate_widget_html("   ").unwrap_err();
        match err {
            WidgetError::EmptyPayload => {}
            _ => panic!("Expected EmptyPayload error, got {err:?}"),
        }

        // Payload too large
        let large_html = "x".repeat(MAX_RAW_HTML_BYTES + 1);
        let err = validate_widget_html(&large_html).unwrap_err();
        match err {
            WidgetError::PayloadTooLarge { size, max } => {
                assert_eq!(size, MAX_RAW_HTML_BYTES + 1);
                assert_eq!(max, MAX_RAW_HTML_BYTES);
            }
            _ => panic!("Expected PayloadTooLarge error, got {err:?}"),
        }

        // Non-private chat check in payload
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let res = send_widget("dummy_token", -1001234567, "<div>Valid</div>").await;
            match res {
                Err(WidgetError::NonPrivateChat(id)) => assert_eq!(id, -1001234567),
                other => panic!("Expected NonPrivateChat error, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_agent_card_data_serialization() {
        let demo = AgentCardData::demo();
        let serialized = serde_json::to_string(&demo).expect("Serialize AgentCardData failed");
        let deserialized: AgentCardData =
            serde_json::from_str(&serialized).expect("Deserialize AgentCardData failed");
        assert_eq!(demo, deserialized);
    }

    #[test]
    fn test_fallback_html_rendering() {
        let demo = AgentCardData::demo();
        let fallback = demo.render_fallback_html();
        assert!(fallback.contains("<b>agygram Agent</b>"));
        assert!(fallback.contains("Claude 3.7 Sonnet (xhigh)"));
        assert!(fallback.contains("已完成"));
        assert!(
            fallback.contains(
                "💡 提示: 当前客户端或网络环境不支持 Telegram 10.4 In-Message HTML Widget"
            )
        );
    }
}
