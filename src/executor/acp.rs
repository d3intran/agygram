//! Optional Agent Client Protocol (ACP) stdio JSON-RPC 2.0 backend (`agy acp --stdio`).
//!
//! Designed for `agy 1.5.14+` with low-memory VPS awareness:
//! - Reuses a persistent `agy acp --stdio` child process per launch profile `(agent, sandbox, extra_dirs)`.
//! - Multiplexes concurrent sessions over NDJSON stdio (`initialize`, `session/new`, `session/resume`,
//!   `session/load`, `session/prompt`, `session/cancel`).
//! - Maps real-time `session/update` notifications into [`StreamEvent`] items.
//! - Handles server-initiated reverse requests (`session/request_permission`, `elicitation/create`)
//!   through an extensible [`AcpReverseRequestHandler`] trait (defaulting to auto-approve).
//! - Automatically reaps the idle `agy acp` process after `acp_idle_timeout_secs` (default 300s) so
//!   an 892 MB VPS does not pin ~300 MB RSS indefinitely while idle.

use super::protocol::{ExecutionResult, StreamEvent, StreamResultData, TokenUsage};
use super::watchdog::signal_process_tree;
use super::{ExecutionParams, get_agy_path, resolve_extra_dirs};
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tracing::{debug, info, warn};

const ACP_PROTOCOL_VERSION: u32 = 1;
const ACP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ACP_SESSION_OP_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_ACP_IDLE_TIMEOUT_SECS: u64 = 300;
const ACP_REAPER_POLL_INTERVAL: Duration = Duration::from_secs(15);
const MAX_ACP_LINE_CAPACITY: usize = 1024 * 1024;

static ACP_IDLE_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(DEFAULT_ACP_IDLE_TIMEOUT_SECS);
static ACP_POOL: OnceLock<Arc<AcpClientPool>> = OnceLock::new();

/// Updates the idle shutdown timeout for the persistent `agy acp --stdio` worker pool.
#[allow(dead_code)]
pub fn configure_acp_idle_timeout(secs: u64) {
    ACP_IDLE_TIMEOUT_SECS.store(secs.max(30), Ordering::Relaxed);
}

fn effective_acp_idle_timeout() -> Duration {
    let configured = ACP_IDLE_TIMEOUT_SECS.load(Ordering::Relaxed);
    if configured != DEFAULT_ACP_IDLE_TIMEOUT_SECS {
        return Duration::from_secs(configured.max(30));
    }
    if let Ok(raw) = std::env::var("AGYGRAM_ACP_IDLE_TIMEOUT_SECS")
        && let Ok(parsed) = raw.trim().parse::<u64>()
    {
        return Duration::from_secs(parsed.max(30));
    }
    Duration::from_secs(DEFAULT_ACP_IDLE_TIMEOUT_SECS)
}

/// Categorized ACP backend errors so [`super::AgyExecutor::run_prompt_streaming`] can decide
/// whether to fall back transparently to `-p --output-format stream-json`.
#[derive(Debug, Error)]
pub(crate) enum AcpError {
    #[error("ACP connect/handshake failed: {0}")]
    HandshakeOrConnect(#[source] anyhow::Error),
    #[error("ACP turn execution failed before streaming output: {0}")]
    ExecutionBeforeOutput(#[source] anyhow::Error),
    #[error("ACP turn execution failed: {0}")]
    Execution(#[source] anyhow::Error),
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 Wire Protocol
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum IncomingRpcMessage {
    Response(JsonRpcResponse),
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Deserialize)]
struct RawRpcEnvelope {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

pub(crate) fn encode_jsonrpc_line<T: Serialize>(msg: &T) -> Result<String> {
    let mut line = serde_json::to_string(msg).context("failed to serialize JSON-RPC message")?;
    line.push('\n');
    Ok(line)
}

pub(crate) fn parse_incoming_rpc_line(line: &str) -> Result<IncomingRpcMessage> {
    let trimmed = line.trim();
    let env: RawRpcEnvelope =
        serde_json::from_str(trimmed).context("failed to parse JSON-RPC line")?;
    match (env.id, env.method) {
        (Some(id), Some(method)) => Ok(IncomingRpcMessage::ServerRequest {
            id,
            method,
            params: env.params.unwrap_or(Value::Null),
        }),
        (None, Some(method)) => Ok(IncomingRpcMessage::Notification {
            method,
            params: env.params.unwrap_or(Value::Null),
        }),
        (Some(id), None) => Ok(IncomingRpcMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: env.result,
            error: env.error,
        })),
        (None, None) => anyhow::bail!("JSON-RPC frame missing both id and method"),
    }
}

// ---------------------------------------------------------------------------
// ACP Method Builders
// ---------------------------------------------------------------------------

#[must_use]
pub(crate) fn build_initialize_request(id: u64) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "initialize".to_string(),
        params: Some(json!({
            "protocolVersion": ACP_PROTOCOL_VERSION,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "agygram",
                "version": env!("CARGO_PKG_VERSION"),
            }
        })),
    }
}

fn build_session_meta(system_prompt: &str) -> Option<Value> {
    let trimmed = system_prompt.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(json!({
        "antigravity": {
            "systemPrompt": {
                "append": trimmed
            }
        }
    }))
}

#[must_use]
pub(crate) fn build_session_new_request(id: u64, cwd: &str, system_prompt: &str) -> JsonRpcRequest {
    let mut params = serde_json::Map::new();
    params.insert("cwd".to_string(), Value::String(cwd.to_string()));
    params.insert("mcpServers".to_string(), Value::Array(Vec::new()));
    if let Some(meta) = build_session_meta(system_prompt) {
        params.insert("_meta".to_string(), meta);
    }
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "session/new".to_string(),
        params: Some(Value::Object(params)),
    }
}

#[must_use]
pub(crate) fn build_session_resume_request(
    id: u64,
    session_id: &str,
    cwd: &str,
    system_prompt: &str,
) -> JsonRpcRequest {
    let mut params = serde_json::Map::new();
    params.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    params.insert("cwd".to_string(), Value::String(cwd.to_string()));
    params.insert("mcpServers".to_string(), Value::Array(Vec::new()));
    if let Some(meta) = build_session_meta(system_prompt) {
        params.insert("_meta".to_string(), meta);
    }
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "session/resume".to_string(),
        params: Some(Value::Object(params)),
    }
}

#[must_use]
pub(crate) fn build_session_load_request(
    id: u64,
    session_id: &str,
    cwd: &str,
    system_prompt: &str,
) -> JsonRpcRequest {
    let mut params = serde_json::Map::new();
    params.insert(
        "sessionId".to_string(),
        Value::String(session_id.to_string()),
    );
    params.insert("cwd".to_string(), Value::String(cwd.to_string()));
    params.insert("mcpServers".to_string(), Value::Array(Vec::new()));
    if let Some(meta) = build_session_meta(system_prompt) {
        params.insert("_meta".to_string(), meta);
    }
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "session/load".to_string(),
        params: Some(Value::Object(params)),
    }
}

#[must_use]
pub(crate) fn build_session_prompt_request(
    id: u64,
    session_id: &str,
    prompt: &str,
) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "session/prompt".to_string(),
        params: Some(json!({
            "sessionId": session_id,
            "prompt": [
                {
                    "type": "text",
                    "text": prompt,
                }
            ]
        })),
    }
}

#[must_use]
pub(crate) fn build_session_cancel_notification(session_id: &str) -> JsonRpcNotification {
    JsonRpcNotification {
        jsonrpc: "2.0".to_string(),
        method: "session/cancel".to_string(),
        params: Some(json!({
            "sessionId": session_id
        })),
    }
}

// ---------------------------------------------------------------------------
// Reverse Request Handler (Permissions & Elicitation Extension Point)
// ---------------------------------------------------------------------------

/// Decision produced by an [`AcpReverseRequestHandler`] for server-initiated RPC calls.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum ReverseRequestDecision {
    ApprovePermission {
        option_id: Option<String>,
    },
    CancelPermission,
    ElicitationResponse {
        action: String,
        content: Option<Value>,
    },
    Unsupported(String),
}

/// Extension point for interactive or automated handling of ACP reverse requests
/// (`session/request_permission`, `elicitation/create`, etc.).
pub(crate) trait AcpReverseRequestHandler: Send + Sync {
    fn handle_request(&self, method: &str, params: &Value) -> ReverseRequestDecision;
}

/// Default headless handler: auto-approves permission prompts (preferring `allow_once`) and
/// cleanly declines interactive elicitation forms so non-interactive turns never deadlock.
#[derive(Debug, Default)]
pub(crate) struct AutoApproveReverseHandler;

impl AutoApproveReverseHandler {
    fn pick_allow_option(params: &Value) -> Option<String> {
        let options = params.get("options")?.as_array()?;
        let pick_by_kind = |target: &str| {
            options.iter().find_map(|opt| {
                let kind = opt.get("kind").and_then(Value::as_str)?;
                if kind == target {
                    opt.get("optionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            })
        };
        pick_by_kind("allow_once")
            .or_else(|| pick_by_kind("allow_always"))
            .or_else(|| {
                options.iter().find_map(|opt| {
                    let kind = opt.get("kind").and_then(Value::as_str).unwrap_or("");
                    if !kind.starts_with("reject") {
                        opt.get("optionId")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            })
    }
}

impl AcpReverseRequestHandler for AutoApproveReverseHandler {
    fn handle_request(&self, method: &str, params: &Value) -> ReverseRequestDecision {
        match method {
            "session/request_permission" => ReverseRequestDecision::ApprovePermission {
                option_id: Self::pick_allow_option(params),
            },
            m if m == "elicitation/create" || m.contains("elicitation") => {
                ReverseRequestDecision::ElicitationResponse {
                    action: "decline".to_string(),
                    content: None,
                }
            }
            other => ReverseRequestDecision::Unsupported(format!(
                "unsupported ACP client method: {other}"
            )),
        }
    }
}

#[must_use]
pub(crate) fn build_reverse_request_response(
    id: Value,
    decision: ReverseRequestDecision,
) -> JsonRpcResponse {
    match decision {
        ReverseRequestDecision::ApprovePermission {
            option_id: Some(option_id),
        } => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": option_id
                }
            })),
            error: None,
        },
        ReverseRequestDecision::ApprovePermission { option_id: None }
        | ReverseRequestDecision::CancelPermission => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(json!({
                "outcome": {
                    "outcome": "cancelled"
                }
            })),
            error: None,
        },
        ReverseRequestDecision::ElicitationResponse { action, content } => {
            let mut obj = serde_json::Map::new();
            obj.insert("action".to_string(), Value::String(action));
            if let Some(c) = content {
                obj.insert("content".to_string(), c);
            }
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(Value::Object(obj)),
                error: None,
            }
        }
        ReverseRequestDecision::Unsupported(msg) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: msg,
                data: None,
            }),
        },
    }
}

// ---------------------------------------------------------------------------
// ACP Event & Completion Mapping
// ---------------------------------------------------------------------------

fn extract_content_text(update: &Value) -> Option<String> {
    if let Some(text) = update
        .get("content")
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }
    if let Some(arr) = update.get("content").and_then(Value::as_array) {
        let mut combined = String::new();
        for item in arr {
            if let Some(t) = item
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
                .or_else(|| item.get("text").and_then(Value::as_str))
            {
                combined.push_str(t);
            }
        }
        if !combined.is_empty() {
            return Some(combined);
        }
    }
    update
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_tool_error_message(update: &Value) -> String {
    if let Some(msg) = update
        .get("error")
        .and_then(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .or_else(|| e.as_str())
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return msg.to_string();
    }
    if let Some(text) = extract_content_text(update)
        && !text.trim().is_empty()
    {
        return text.trim().to_string();
    }
    if let Some(raw) = update.get("rawOutput") {
        if let Some(s) = raw.as_str()
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        if !raw.is_null() {
            return raw.to_string();
        }
    }
    "tool execution failed".to_string()
}

/// Converts a single ACP `session/update` notification `params` object into zero or more
/// [`StreamEvent`]s, appending streamed response text into `accumulated_text`.
pub(crate) fn map_acp_update_to_events(
    params: &Value,
    accumulated_text: &mut String,
) -> Vec<StreamEvent> {
    let update = params.get("update").unwrap_or(params);
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        return Vec::new();
    };

    let mut events = Vec::new();
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = extract_content_text(update)
                && !text.is_empty()
            {
                accumulated_text.push_str(&text);
                events.push(StreamEvent::TextDelta(text));
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = extract_content_text(update)
                && !text.is_empty()
            {
                events.push(StreamEvent::ThinkingDelta(text));
            }
        }
        "tool_call" => {
            let tool = update
                .get("toolName")
                .or_else(|| update.get("title"))
                .or_else(|| update.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let summary = update
                .get("rawInput")
                .filter(|v| !v.is_null())
                .map(ToString::to_string)
                .or_else(|| {
                    update
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            events.push(StreamEvent::ToolUse {
                tool: tool.clone(),
                summary,
            });
            if matches!(
                update.get("status").and_then(Value::as_str),
                Some("failed" | "error")
            ) {
                events.push(StreamEvent::ToolError {
                    tool,
                    message: extract_tool_error_message(update),
                });
            }
        }
        "tool_call_update" => {
            let status = update.get("status").and_then(Value::as_str).unwrap_or("");
            let tool = update
                .get("toolName")
                .or_else(|| update.get("title"))
                .or_else(|| update.get("toolCallId"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            if matches!(status, "failed" | "error") || update.get("error").is_some() {
                events.push(StreamEvent::ToolError {
                    tool,
                    message: extract_tool_error_message(update),
                });
            } else if update.get("kind").and_then(Value::as_str) == Some("subagent") {
                let summary = update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("subagent")
                    .to_string();
                events.push(StreamEvent::SubagentUpdate { summary });
            }
        }
        "checkpoint" | "compaction" => {
            events.push(StreamEvent::Checkpoint);
        }
        "subagent_update" => {
            let summary = update
                .get("summary")
                .or_else(|| update.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("subagent")
                .to_string();
            events.push(StreamEvent::SubagentUpdate { summary });
        }
        other => {
            debug!(
                session_update = other,
                "ignoring informational ACP session/update"
            );
        }
    }
    events
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AcpPromptCompletion {
    pub status: String,
    pub finished_reason: Option<String>,
    pub usage: Option<TokenUsage>,
    pub structured_output: Option<Value>,
}

#[must_use]
pub(crate) fn extract_prompt_completion(result: &Value) -> AcpPromptCompletion {
    let stop_reason = result
        .get("stopReason")
        .or_else(|| result.get("finished_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let status = match stop_reason.as_deref() {
        Some("cancelled" | "canceled") => "CANCELED".to_string(),
        Some("refusal" | "error") => "ERROR".to_string(),
        Some("waiting" | "waiting_for_input") => "WAITING".to_string(),
        _ => "SUCCESS".to_string(),
    };

    let usage = result
        .get("usage")
        .and_then(|u| serde_json::from_value::<TokenUsage>(u.clone()).ok());
    let structured_output = result
        .get("structuredOutput")
        .or_else(|| result.get("structured_output"))
        .cloned();

    AcpPromptCompletion {
        status,
        finished_reason: stop_reason,
        usage,
        structured_output,
    }
}

// ---------------------------------------------------------------------------
// Persistent Pooled ACP Process + Idle Reaper
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct AcpLaunchProfile {
    agent: String,
    sandbox: bool,
    extra_dirs: Vec<String>,
}

struct AcpSharedState {
    pending_requests: AsyncMutex<FxHashMap<u64, oneshot::Sender<JsonRpcResponse>>>,
    session_updates: AsyncMutex<FxHashMap<String, mpsc::UnboundedSender<Value>>>,
    active_turns: AtomicUsize,
    last_activity_ms: AtomicU64,
    origin: Instant,
}

impl AcpSharedState {
    fn new() -> Self {
        Self {
            pending_requests: AsyncMutex::new(FxHashMap::default()),
            session_updates: AsyncMutex::new(FxHashMap::default()),
            active_turns: AtomicUsize::new(0),
            last_activity_ms: AtomicU64::new(0),
            origin: Instant::now(),
        }
    }

    fn touch(&self) {
        let elapsed = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_activity_ms.fetch_max(elapsed, Ordering::Relaxed);
    }

    fn idle_duration(&self) -> Duration {
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        Duration::from_millis(now.saturating_sub(last))
    }
}

struct AcpConnection {
    profile: AcpLaunchProfile,
    pid: Option<u32>,
    stdin: Arc<AsyncMutex<ChildStdin>>,
    shared: Arc<AcpSharedState>,
    next_id: AtomicU64,
    child: AsyncMutex<Child>,
    reader_task: tokio::task::JoinHandle<()>,
    stderr_task: tokio::task::JoinHandle<()>,
}

impl AcpConnection {
    async fn spawn_and_initialize(
        profile: AcpLaunchProfile,
        reverse_handler: Arc<dyn AcpReverseRequestHandler>,
    ) -> Result<Arc<Self>> {
        let mut cmd = Command::new(get_agy_path());
        cmd.arg("acp")
            .arg("--stdio")
            .arg("--dangerously-skip-permissions");
        if profile.sandbox {
            cmd.arg("--sandbox");
        }
        if !profile.agent.is_empty() && crate::models::is_safe_free_value(&profile.agent) {
            cmd.arg("--agent").arg(&profile.agent);
        }
        for dir in &profile.extra_dirs {
            cmd.arg("--add-dir").arg(dir);
        }
        cmd.kill_on_drop(true);
        cmd.process_group(0);
        cmd.env("LANG", "C.UTF-8");
        cmd.env("LC_ALL", "C.UTF-8");
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().context("failed to spawn `agy acp --stdio`")?;
        let pid = child.id();
        let stdin = child.stdin.take().context("failed to open ACP stdin")?;
        let stdout = child.stdout.take().context("failed to open ACP stdout")?;
        let stderr = child.stderr.take().context("failed to open ACP stderr")?;

        let stdin_arc = Arc::new(AsyncMutex::new(stdin));
        let shared = Arc::new(AcpSharedState::new());

        let reader_stdin = Arc::clone(&stdin_arc);
        let reader_shared = Arc::clone(&shared);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
            loop {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(err) => {
                        warn!(error = %err, "ACP stdout reader error");
                        break;
                    }
                }
                reader_shared.touch();
                let line = String::from_utf8_lossy(&line_buf);
                match parse_incoming_rpc_line(line.as_ref()) {
                    Ok(IncomingRpcMessage::Response(resp)) => {
                        if let Some(id_num) = resp.id.as_u64() {
                            let tx = reader_shared.pending_requests.lock().await.remove(&id_num);
                            if let Some(tx) = tx {
                                let _ = tx.send(resp);
                            }
                        }
                    }
                    Ok(IncomingRpcMessage::Notification { method, params }) => {
                        if method == "session/update"
                            && let Some(sid) = params.get("sessionId").and_then(Value::as_str)
                        {
                            let guard = reader_shared.session_updates.lock().await;
                            if let Some(tx) = guard.get(sid) {
                                let _ = tx.send(params);
                            }
                        }
                    }
                    Ok(IncomingRpcMessage::ServerRequest { id, method, params }) => {
                        let decision = reverse_handler.handle_request(&method, &params);
                        let resp = build_reverse_request_response(id, decision);
                        if let Ok(encoded) = encode_jsonrpc_line(&resp) {
                            let mut w = reader_stdin.lock().await;
                            let _ = w.write_all(encoded.as_bytes()).await;
                            let _ = w.flush().await;
                        }
                    }
                    Err(err) => {
                        debug!(error = %err, "ignoring non-JSON-RPC line from ACP stdout");
                    }
                }
                if line_buf.capacity() > MAX_ACP_LINE_CAPACITY {
                    line_buf = Vec::with_capacity(8192);
                }
            }
            // Wake any pending requests on EOF.
            reader_shared.pending_requests.lock().await.clear();
            reader_shared.session_updates.lock().await.clear();
        });

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line_buf: Vec<u8> = Vec::with_capacity(512);
            loop {
                line_buf.clear();
                match reader.read_until(b'\n', &mut line_buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let msg = String::from_utf8_lossy(&line_buf);
                        debug!(stderr = %msg.trim_end(), "agy acp stderr");
                    }
                }
            }
        });

        let conn = Arc::new(Self {
            profile,
            pid,
            stdin: stdin_arc,
            shared,
            next_id: AtomicU64::new(1),
            child: AsyncMutex::new(child),
            reader_task,
            stderr_task,
        });

        // Perform ACP protocol handshake (`initialize`).
        let init_id = conn.next_request_id();
        let init_req = build_initialize_request(init_id);
        match conn.call_rpc(init_req, ACP_HANDSHAKE_TIMEOUT).await {
            Ok(resp) => {
                if let Some(err) = resp.error {
                    conn.shutdown().await;
                    anyhow::bail!("ACP initialize error {}: {}", err.code, err.message);
                }
                info!(pid = ?conn.pid, "initialized persistent agy acp --stdio process");
                Ok(conn)
            }
            Err(err) => {
                conn.shutdown().await;
                Err(err)
            }
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn is_alive(&self) -> bool {
        !self.reader_task.is_finished()
    }

    async fn send_notification(&self, notif: &JsonRpcNotification) -> Result<()> {
        let encoded = encode_jsonrpc_line(notif)?;
        let mut w = self.stdin.lock().await;
        w.write_all(encoded.as_bytes()).await?;
        w.flush().await?;
        self.shared.touch();
        Ok(())
    }

    async fn call_rpc(&self, req: JsonRpcRequest, timeout: Duration) -> Result<JsonRpcResponse> {
        let id = req.id;
        let encoded = encode_jsonrpc_line(&req)?;
        let (tx, rx) = oneshot::channel();
        self.shared.pending_requests.lock().await.insert(id, tx);

        let write_res = {
            let mut w = self.stdin.lock().await;
            let r = w.write_all(encoded.as_bytes()).await;
            if r.is_ok() { w.flush().await } else { r }
        };
        if let Err(err) = write_res {
            self.shared.pending_requests.lock().await.remove(&id);
            return Err(err).context("failed to write ACP JSON-RPC request");
        }
        self.shared.touch();

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => anyhow::bail!("ACP server closed connection before responding"),
            Err(_) => {
                self.shared.pending_requests.lock().await.remove(&id);
                anyhow::bail!(
                    "ACP request {} timed out after {}s",
                    req.method,
                    timeout.as_secs()
                );
            }
        }
    }

    async fn shutdown(&self) {
        self.reader_task.abort();
        self.stderr_task.abort();
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGKILL);
        }
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
}

struct AcpClientPool {
    conn: AsyncMutex<Option<Arc<AcpConnection>>>,
}

impl AcpClientPool {
    fn global() -> Arc<Self> {
        Arc::clone(ACP_POOL.get_or_init(|| {
            let pool = Arc::new(Self {
                conn: AsyncMutex::new(None),
            });
            Self::spawn_idle_reaper(Arc::clone(&pool));
            pool
        }))
    }

    fn spawn_idle_reaper(pool: Arc<Self>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut ticker = tokio::time::interval(ACP_REAPER_POLL_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let idle_limit = effective_acp_idle_timeout();
                    let stale = {
                        let mut guard = pool.conn.lock().await;
                        match guard.as_ref() {
                            Some(conn)
                                if conn.shared.active_turns.load(Ordering::Acquire) == 0
                                    && (!conn.is_alive()
                                        || conn.shared.idle_duration() >= idle_limit) =>
                            {
                                guard.take()
                            }
                            _ => None,
                        }
                    };
                    if let Some(conn) = stale {
                        info!(
                            pid = ?conn.pid,
                            idle_secs = conn.shared.idle_duration().as_secs(),
                            "reaping idle agy acp process to reclaim memory"
                        );
                        conn.shutdown().await;
                    }
                }
            });
        }
    }

    async fn acquire(
        &self,
        profile: AcpLaunchProfile,
    ) -> std::result::Result<Arc<AcpConnection>, AcpError> {
        let mut guard = self.conn.lock().await;
        if let Some(existing) = guard.as_ref() {
            if existing.is_alive() && existing.profile == profile {
                existing.shared.touch();
                return Ok(Arc::clone(existing));
            }
            if existing.shared.active_turns.load(Ordering::Acquire) == 0
                && let Some(old) = guard.take()
            {
                tokio::spawn(async move {
                    old.shutdown().await;
                });
            }
        }
        let handler: Arc<dyn AcpReverseRequestHandler> = Arc::new(AutoApproveReverseHandler);
        let fresh = AcpConnection::spawn_and_initialize(profile, handler)
            .await
            .map_err(AcpError::HandshakeOrConnect)?;
        *guard = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    async fn invalidate(&self, failed: &Arc<AcpConnection>) {
        let mut guard = self.conn.lock().await;
        if guard.as_ref().is_some_and(|c| Arc::ptr_eq(c, failed))
            && let Some(old) = guard.take()
        {
            tokio::spawn(async move {
                old.shutdown().await;
            });
        }
    }
}

struct ActiveTurnGuard {
    shared: Arc<AcpSharedState>,
}

impl ActiveTurnGuard {
    fn new(shared: Arc<AcpSharedState>) -> Self {
        shared.active_turns.fetch_add(1, Ordering::AcqRel);
        shared.touch();
        Self { shared }
    }
}

impl Drop for ActiveTurnGuard {
    fn drop(&mut self) {
        self.shared.touch();
        self.shared.active_turns.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn open_or_resume_session(
    conn: &AcpConnection,
    params: &ExecutionParams<'_>,
) -> std::result::Result<String, AcpError> {
    let cwd = if params.workspace.trim().is_empty() {
        "."
    } else {
        params.workspace
    };

    if let Some(existing_id) = params
        .conversation_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && crate::models::is_safe_cli_value(s))
    {
        // Prefer `session/resume` (no history replay overhead); fall back to `session/load`.
        let resume_req = build_session_resume_request(
            conn.next_request_id(),
            existing_id,
            cwd,
            params.system_prompt,
        );
        if let Ok(resp) = conn.call_rpc(resume_req, ACP_SESSION_OP_TIMEOUT).await
            && resp.error.is_none()
        {
            return Ok(resp
                .result
                .as_ref()
                .and_then(|r| r.get("sessionId").and_then(Value::as_str))
                .unwrap_or(existing_id)
                .to_string());
        }

        let load_req = build_session_load_request(
            conn.next_request_id(),
            existing_id,
            cwd,
            params.system_prompt,
        );
        let resp = conn
            .call_rpc(load_req, ACP_SESSION_OP_TIMEOUT)
            .await
            .map_err(AcpError::HandshakeOrConnect)?;
        if let Some(err) = resp.error {
            return Err(AcpError::HandshakeOrConnect(anyhow::anyhow!(
                "ACP session/load failed ({}): {}",
                err.code,
                err.message
            )));
        }
        return Ok(resp
            .result
            .as_ref()
            .and_then(|r| r.get("sessionId").and_then(Value::as_str))
            .unwrap_or(existing_id)
            .to_string());
    }

    let new_req = build_session_new_request(conn.next_request_id(), cwd, params.system_prompt);
    let resp = conn
        .call_rpc(new_req, ACP_SESSION_OP_TIMEOUT)
        .await
        .map_err(AcpError::HandshakeOrConnect)?;
    if let Some(err) = resp.error {
        return Err(AcpError::HandshakeOrConnect(anyhow::anyhow!(
            "ACP session/new failed ({}): {}",
            err.code,
            err.message
        )));
    }
    let session_id = resp
        .result
        .as_ref()
        .and_then(|r| r.get("sessionId").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| {
            AcpError::HandshakeOrConnect(anyhow::anyhow!(
                "ACP session/new response missing sessionId"
            ))
        })?;
    Ok(session_id)
}

/// Executes a single prompt turn via the persistent `agy acp --stdio` backend, streaming
/// [`StreamEvent`] updates to `stream_tx`.
pub(crate) async fn run_prompt_acp(
    params: &ExecutionParams<'_>,
    stream_tx: mpsc::Sender<StreamEvent>,
    idle_timeout_secs: u64,
    max_execution_secs: u64,
) -> std::result::Result<ExecutionResult, AcpError> {
    let profile = AcpLaunchProfile {
        agent: params.agent.trim().to_string(),
        sandbox: params.sandbox,
        extra_dirs: resolve_extra_dirs(params.workspace, params.extra_dirs),
    };

    let pool = AcpClientPool::global();
    let conn = pool.acquire(profile).await?;
    let _active_guard = ActiveTurnGuard::new(Arc::clone(&conn.shared));

    let session_id = match open_or_resume_session(&conn, params).await {
        Ok(sid) => sid,
        Err(err) => {
            if !conn.is_alive() {
                pool.invalidate(&conn).await;
            }
            return Err(err);
        }
    };

    let _ = stream_tx
        .send(StreamEvent::Init {
            conversation_id: session_id.clone(),
        })
        .await;

    let (upd_tx, mut upd_rx) = mpsc::unbounded_channel::<Value>();
    conn.shared
        .session_updates
        .lock()
        .await
        .insert(session_id.clone(), upd_tx);

    let prompt_id = conn.next_request_id();
    let prompt_text = if params.prompt.trim_start().starts_with('/') {
        params.prompt.trim_start()
    } else {
        params.prompt
    };
    let prompt_req = build_session_prompt_request(prompt_id, &session_id, prompt_text);
    let encoded = match encode_jsonrpc_line(&prompt_req) {
        Ok(line) => line,
        Err(err) => {
            conn.shared.session_updates.lock().await.remove(&session_id);
            return Err(AcpError::HandshakeOrConnect(err));
        }
    };

    let (resp_tx, mut resp_rx) = oneshot::channel::<JsonRpcResponse>();
    conn.shared
        .pending_requests
        .lock()
        .await
        .insert(prompt_id, resp_tx);

    let write_res = {
        let mut w = conn.stdin.lock().await;
        let r = w.write_all(encoded.as_bytes()).await;
        if r.is_ok() { w.flush().await } else { r }
    };
    if let Err(err) = write_res {
        conn.shared.pending_requests.lock().await.remove(&prompt_id);
        conn.shared.session_updates.lock().await.remove(&session_id);
        pool.invalidate(&conn).await;
        return Err(AcpError::HandshakeOrConnect(err.into()));
    }

    let start_time = Instant::now();
    let idle_limit = Duration::from_secs(idle_timeout_secs.max(30));
    let max_limit = Duration::from_secs(max_execution_secs.max(60));
    let max_deadline = tokio::time::Instant::now() + max_limit;
    let mut last_event_at = tokio::time::Instant::now();
    let mut accumulated_text = String::new();
    let mut streamed_any_output = false;
    let mut upd_open = true;

    let final_rpc_res: std::result::Result<JsonRpcResponse, AcpError> = loop {
        let idle_deadline = last_event_at + idle_limit;
        let next_deadline = idle_deadline.min(max_deadline);

        tokio::select! {
            biased;
            maybe_upd = upd_rx.recv(), if upd_open => {
                if let Some(upd_params) = maybe_upd {
                    last_event_at = tokio::time::Instant::now();
                    let events = map_acp_update_to_events(&upd_params, &mut accumulated_text);
                    for ev in events {
                        if matches!(ev, StreamEvent::TextDelta(_)) {
                            streamed_any_output = true;
                        }
                        let _ = stream_tx.send(ev).await;
                    }
                } else {
                    upd_open = false;
                }
            }
            resp = &mut resp_rx => {
                // Drain any notifications queued right before the response frame.
                while let Ok(upd_params) = upd_rx.try_recv() {
                    let events = map_acp_update_to_events(&upd_params, &mut accumulated_text);
                    for ev in events {
                        if matches!(ev, StreamEvent::TextDelta(_)) {
                            streamed_any_output = true;
                        }
                        let _ = stream_tx.send(ev).await;
                    }
                }
                match resp {
                    Ok(rpc_resp) => break Ok(rpc_resp),
                    Err(_) => {
                        pool.invalidate(&conn).await;
                        let err = anyhow::anyhow!("ACP server disconnected mid-turn");
                        break Err(if streamed_any_output {
                            AcpError::Execution(err)
                        } else {
                            AcpError::ExecutionBeforeOutput(err)
                        });
                    }
                }
            }
            _ = tokio::time::sleep_until(next_deadline) => {
                let _ = conn.send_notification(&build_session_cancel_notification(&session_id)).await;
                let msg = if tokio::time::Instant::now() >= max_deadline {
                    format!("ACP 执行超过全局安全上限 ({}s)，已取消。", max_limit.as_secs())
                } else {
                    format!("ACP 超过 {}s 无事件响应，已取消。", idle_limit.as_secs())
                };
                let _ = stream_tx.send(StreamEvent::Error(msg.clone())).await;
                conn.shared.pending_requests.lock().await.remove(&prompt_id);
                conn.shared.session_updates.lock().await.remove(&session_id);
                return Ok(ExecutionResult {
                    output: msg,
                    conversation_id: Some(session_id),
                    status: "TIMEOUT".to_string(),
                    usage: None,
                    duration_seconds: Some(start_time.elapsed().as_secs_f64()),
                    detached: false,
                    finished_reason: Some("timeout".to_string()),
                    structured_output: None,
                });
            }
        }
    };

    conn.shared.session_updates.lock().await.remove(&session_id);

    let rpc_resp = final_rpc_res?;
    if let Some(err) = rpc_resp.error {
        let err_msg = format!("ACP error {}: {}", err.code, err.message);
        if !streamed_any_output {
            return Err(AcpError::ExecutionBeforeOutput(anyhow::anyhow!(err_msg)));
        }
        let _ = stream_tx.send(StreamEvent::Error(err_msg.clone())).await;
        return Ok(ExecutionResult {
            output: if accumulated_text.trim().is_empty() {
                format!("❌ {err_msg}")
            } else {
                accumulated_text.trim().to_string()
            },
            conversation_id: Some(session_id),
            status: "ERROR".to_string(),
            usage: None,
            duration_seconds: Some(start_time.elapsed().as_secs_f64()),
            detached: false,
            finished_reason: Some("error".to_string()),
            structured_output: None,
        });
    }

    let completion = extract_prompt_completion(rpc_resp.result.as_ref().unwrap_or(&Value::Null));
    let mut final_output = accumulated_text.trim().to_string();
    if final_output.is_empty() {
        final_output = "Task completed with no text output.".to_string();
    }
    let elapsed = Some(start_time.elapsed().as_secs_f64());

    let _ = stream_tx
        .send(StreamEvent::Result(Box::new(StreamResultData {
            conversation_id: Some(session_id.clone()),
            response: final_output.clone(),
            status: completion.status.clone(),
            usage: completion.usage.clone(),
            duration_seconds: elapsed,
            finished_reason: completion.finished_reason.clone(),
            structured_output: completion.structured_output.clone(),
        })))
        .await;

    Ok(ExecutionResult {
        output: final_output,
        conversation_id: Some(session_id),
        status: completion.status,
        usage: completion.usage,
        duration_seconds: elapsed,
        detached: false,
        finished_reason: completion.finished_reason,
        structured_output: completion.structured_output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_request_builders_match_acp_v1_schema() {
        let init = build_initialize_request(1);
        let encoded = encode_jsonrpc_line(&init).expect("encode init");
        assert!(encoded.ends_with('\n'));
        let val: Value = serde_json::from_str(encoded.trim()).expect("valid json");
        assert_eq!(val["jsonrpc"], "2.0");
        assert_eq!(val["id"], 1);
        assert_eq!(val["method"], "initialize");
        assert_eq!(val["params"]["protocolVersion"], 1);
        assert_eq!(val["params"]["clientInfo"]["name"], "agygram");
        assert_eq!(
            val["params"]["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );

        let new_req = build_session_new_request(2, "/workspace", "Keep answers concise.");
        let new_val: Value =
            serde_json::from_str(encode_jsonrpc_line(&new_req).unwrap().trim()).unwrap();
        assert_eq!(new_val["method"], "session/new");
        assert_eq!(new_val["params"]["cwd"], "/workspace");
        assert_eq!(
            new_val["params"]["_meta"]["antigravity"]["systemPrompt"]["append"],
            "Keep answers concise."
        );

        let resume_req = build_session_resume_request(3, "sess-9", "/workspace", "");
        let resume_val: Value =
            serde_json::from_str(encode_jsonrpc_line(&resume_req).unwrap().trim()).unwrap();
        assert_eq!(resume_val["method"], "session/resume");
        assert_eq!(resume_val["params"]["sessionId"], "sess-9");
        assert!(resume_val["params"].get("_meta").is_none());

        let load_req = build_session_load_request(4, "sess-9", "/workspace", "sys");
        let load_val: Value =
            serde_json::from_str(encode_jsonrpc_line(&load_req).unwrap().trim()).unwrap();
        assert_eq!(load_val["method"], "session/load");
        assert_eq!(load_val["params"]["sessionId"], "sess-9");

        let prompt_req = build_session_prompt_request(5, "sess-9", "hello world");
        let prompt_val: Value =
            serde_json::from_str(encode_jsonrpc_line(&prompt_req).unwrap().trim()).unwrap();
        assert_eq!(prompt_val["method"], "session/prompt");
        assert_eq!(prompt_val["params"]["prompt"][0]["type"], "text");
        assert_eq!(prompt_val["params"]["prompt"][0]["text"], "hello world");

        let cancel_notif = build_session_cancel_notification("sess-9");
        let cancel_val: Value =
            serde_json::from_str(encode_jsonrpc_line(&cancel_notif).unwrap().trim()).unwrap();
        assert_eq!(cancel_val["method"], "session/cancel");
        assert!(cancel_val.get("id").is_none());
        assert_eq!(cancel_val["params"]["sessionId"], "sess-9");
    }

    #[test]
    fn incoming_rpc_parser_classifies_responses_notifications_and_server_requests() {
        let resp = parse_incoming_rpc_line(
            r#"{"jsonrpc":"2.0","id":7,"result":{"stopReason":"end_turn"}}"#,
        )
        .expect("response parses");
        assert!(matches!(resp, IncomingRpcMessage::Response(r) if r.id == json!(7)));

        let notif = parse_incoming_rpc_line(
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"checkpoint"}}}"#,
        )
        .expect("notification parses");
        assert!(matches!(
            notif,
            IncomingRpcMessage::Notification { ref method, .. } if method == "session/update"
        ));

        let req = parse_incoming_rpc_line(
            r#"{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{"options":[{"optionId":"opt-1","kind":"allow_once"}]}}"#,
        )
        .expect("server request parses");
        assert!(matches!(
            req,
            IncomingRpcMessage::ServerRequest { ref id, ref method, .. }
                if id == &json!(42) && method == "session/request_permission"
        ));

        assert!(parse_incoming_rpc_line("not json").is_err());
        assert!(parse_incoming_rpc_line(r#"{"jsonrpc":"2.0"}"#).is_err());
    }

    #[test]
    fn map_acp_update_to_events_handles_text_thinking_tools_errors_checkpoint_and_subagents() {
        let mut acc = String::new();

        let text_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Hello " }
                }
            }),
            &mut acc,
        );
        assert_eq!(
            text_events,
            vec![StreamEvent::TextDelta("Hello ".to_string())]
        );
        assert_eq!(acc, "Hello ");

        let thought_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "reasoning..." }
                }
            }),
            &mut acc,
        );
        assert_eq!(
            thought_events,
            vec![StreamEvent::ThinkingDelta("reasoning...".to_string())]
        );

        let tool_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolName": "run_command",
                    "rawInput": { "CommandLine": "ls -la" }
                }
            }),
            &mut acc,
        );
        assert_eq!(tool_events.len(), 1);
        assert!(matches!(
            &tool_events[0],
            StreamEvent::ToolUse { tool, summary } if tool == "run_command" && summary.contains("ls -la")
        ));

        let err_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolName": "read_file",
                    "status": "failed",
                    "error": { "message": "denied" }
                }
            }),
            &mut acc,
        );
        assert_eq!(
            err_events,
            vec![StreamEvent::ToolError {
                tool: "read_file".to_string(),
                message: "denied".to_string(),
            }]
        );

        let cp_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": { "sessionUpdate": "checkpoint" }
            }),
            &mut acc,
        );
        assert_eq!(cp_events, vec![StreamEvent::Checkpoint]);

        let sub_events = map_acp_update_to_events(
            &json!({
                "sessionId": "s1",
                "update": { "sessionUpdate": "subagent_update", "summary": "researcher (abc12345)" }
            }),
            &mut acc,
        );
        assert_eq!(
            sub_events,
            vec![StreamEvent::SubagentUpdate {
                summary: "researcher (abc12345)".to_string()
            }]
        );
    }

    #[test]
    fn auto_approve_reverse_handler_approves_permissions_and_declines_elicitations() {
        let handler = AutoApproveReverseHandler;

        let perm_decision = handler.handle_request(
            "session/request_permission",
            &json!({
                "options": [
                    { "optionId": "deny-1", "kind": "reject_once" },
                    { "optionId": "allow-always", "kind": "allow_always" },
                    { "optionId": "allow-once", "kind": "allow_once" }
                ]
            }),
        );
        let perm_resp = build_reverse_request_response(json!(11), perm_decision);
        assert_eq!(
            perm_resp.result,
            Some(json!({
                "outcome": {
                    "outcome": "selected",
                    "optionId": "allow-once"
                }
            }))
        );

        let elicit_decision = handler.handle_request("elicitation/create", &json!({}));
        let elicit_resp = build_reverse_request_response(json!(12), elicit_decision);
        assert_eq!(elicit_resp.result, Some(json!({ "action": "decline" })));

        let unknown_decision = handler.handle_request("unknown/method", &json!({}));
        let unknown_resp = build_reverse_request_response(json!(13), unknown_decision);
        assert_eq!(unknown_resp.error.as_ref().map(|e| e.code), Some(-32601));
    }

    #[test]
    fn extract_prompt_completion_maps_stop_reason_usage_and_structured_output() {
        let completed = extract_prompt_completion(&json!({
            "stopReason": "end_turn",
            "usage": { "input_tokens": 100, "output_tokens": 25, "total_tokens": 125 },
            "structuredOutput": { "ok": true }
        }));
        assert_eq!(completed.status, "SUCCESS");
        assert_eq!(completed.finished_reason.as_deref(), Some("end_turn"));
        assert_eq!(completed.usage.as_ref().map(|u| u.total_tokens), Some(125));
        assert_eq!(completed.structured_output, Some(json!({ "ok": true })));

        let canceled = extract_prompt_completion(&json!({ "stopReason": "cancelled" }));
        assert_eq!(canceled.status, "CANCELED");
    }
}
