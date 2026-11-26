pub(crate) mod acp;
pub(crate) mod persistent;
pub(crate) mod protocol;
pub(crate) mod watchdog;
pub use acp::shutdown_acp_pool;
pub use persistent::{
    configure_persistent_pool, evict_chat_worker, evict_idle_for_budget, shutdown_persistent_pool,
};
pub use protocol::*;
pub use watchdog::*;

use crate::config::ExecutorBackend;
use crate::text::{strip_ansi_codes, truncate_chars};
use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock as StdRwLock};
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncReadExt as _, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub(crate) const MAX_STDERR_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LINE_BUF_CAPACITY: usize = 1024 * 1024;
const MAX_PENDING_DETACH_EVENTS: usize = 2048;
/// Grace window after `agent_response DONE` when background activity (subagent/swarm/schedule)
/// was observed during the turn.
pub(crate) const DETACH_GRACE_BACKGROUND: Duration = Duration::from_millis(1500);
/// Longer grace window when no background activity was seen, avoiding false detachments on slow
/// `agy` exit cleanup.
pub(crate) const DETACH_GRACE_IDLE: Duration = Duration::from_millis(5000);
const FINAL_RESULT_PASSIVE_GRACE: Duration = Duration::from_secs(2);
const FINAL_RESULT_SIGINT_GRACE: Duration = Duration::from_secs(3);
const CANCEL_SIGINT_GRACE: Duration = Duration::from_secs(5);
const CLI_SUBCOMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_UPDATE_TIMEOUT: Duration = Duration::from_secs(120);
const AGENTS_CACHE_TTL: Duration = Duration::from_secs(600);
const SKILL_NAMES_CACHE_TTL: Duration = Duration::from_secs(120);
const MAX_TRANSCRIPT_TITLE_SCAN_LINES: usize = 40;
const MAX_TRANSCRIPT_TITLE_SCAN_BYTES: u64 = 128 * 1024;
const MAX_CACHED_TITLES: usize = 512;
const DEFAULT_SENSITIVE_DENY_PATHS: &[&str] =
    &["~/.config/agygram/*", "~/.config/userbot/*", "~/.ssh/*"];

pub(crate) const BARE_PASSTHROUGH_COMMANDS: &[&str] = &[
    "/compact", "/quota", "/credits", "/usage", "/model", "/effort", "/rename", "/clear", "/help",
    "/logout", "/exit", "/quit",
];

pub struct AgyExecutor;

type AgentsCacheSnapshot = Option<(Instant, Vec<String>)>;
type SkillNamesCacheSnapshot = Option<(Instant, Vec<String>)>;
type ParsedMetadataEntries = Vec<(chrono::DateTime<chrono::Utc>, String, String)>;

struct MetadataFileCache {
    path: PathBuf,
    mtime: Option<SystemTime>,
    len: u64,
    entries: ParsedMetadataEntries,
}

static AGY_EXECUTABLE_PATH: OnceLock<PathBuf> = OnceLock::new();
static BRAIN_DIRS_STATIC: OnceLock<Vec<PathBuf>> = OnceLock::new();
static CONV_TITLE_CACHE: OnceLock<StdRwLock<FxHashMap<String, (u64, String)>>> = OnceLock::new();
static CONV_TITLE_SEQ: AtomicU64 = AtomicU64::new(1);
static AGENTS_CACHE: OnceLock<StdRwLock<AgentsCacheSnapshot>> = OnceLock::new();
static AGENTS_BG_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static SKILL_NAMES_CACHE: OnceLock<StdRwLock<SkillNamesCacheSnapshot>> = OnceLock::new();
static METADATA_FILE_CACHE: OnceLock<StdRwLock<Option<MetadataFileCache>>> = OnceLock::new();

struct AgentsBgFlagReset;

impl Drop for AgentsBgFlagReset {
    fn drop(&mut self) {
        AGENTS_BG_IN_FLIGHT.store(false, Ordering::Release);
    }
}

#[must_use]
pub fn get_agy_path() -> &'static PathBuf {
    AGY_EXECUTABLE_PATH.get_or_init(|| {
        if let Ok(p) = which::which("agy") {
            return p;
        }
        let system_bin = PathBuf::from("/usr/local/bin/agy");
        if system_bin.exists() {
            return system_bin;
        }
        if let Some(home) = dirs::home_dir() {
            let home_local_bin = home.join(".local/bin/agy");
            if home_local_bin.exists() {
                return home_local_bin;
            }
            let gemini_bin = home.join(".gemini/antigravity-cli/bin/agy");
            if gemini_bin.exists() {
                return gemini_bin;
            }
        }
        let root_local = PathBuf::from("/root/.local/bin/agy");
        if root_local.exists() {
            return root_local;
        }
        PathBuf::from("agy")
    })
}

/// Resolves extra `--add-dir` entries to absolute paths and drops duplicates.
/// agy rejects non-absolute paths, and sessions persisted by older builds may still hold a
/// literal `~`; the workspace is already mounted via `current_dir`, so it is excluded here to
/// avoid mounting the same workspace twice in `agy`'s language server & file watcher.
pub(crate) fn resolve_extra_dirs(workspace: &str, extra_dirs: &[String]) -> Vec<String> {
    let mut resolved_dirs: Vec<String> = Vec::with_capacity(extra_dirs.len());
    for dir in extra_dirs {
        let expanded = crate::session::resolve_user_path(dir, workspace)
            .to_string_lossy()
            .into_owned();
        if expanded.is_empty()
            || expanded == workspace
            || resolved_dirs.iter().any(|d| d == &expanded)
        {
            continue;
        }
        resolved_dirs.push(expanded);
    }
    resolved_dirs
}

/// True when `agy update` could not write the install dir and a passwordless-sudo retry is
/// warranted. Newer agy reports "not fully accessible (readable: true, writable: false)" and
/// may exit 0 while skipping the update, so keyword matching is required alongside the exit code.
fn update_needs_elevation(combined: &str, success: bool) -> bool {
    !success
        || combined.contains("Permission denied")
        || combined.contains("os error 13")
        || combined.contains("not fully accessible")
        || combined.contains("writable: false")
}

/// Returns `true` iff this turn will execute on the ACP backend (not falling back to `print`
/// for `--json-schema` or `--remote-agent`). Both `Persistent` and `Print` deduplicate static
/// system prompts identically, so callers gate prompt dedup on `!will_use_acp(...)`.
#[inline]
#[must_use]
pub fn will_use_acp(backend: ExecutorBackend, has_json_schema: bool, remote_agent: bool) -> bool {
    backend == ExecutorBackend::Acp && !has_json_schema && !remote_agent
}

#[derive(Debug, Clone)]
pub struct ExecutionParams<'a> {
    pub chat_id: i64,
    pub prompt: &'a str,
    pub conversation_id: Option<&'a str>,
    pub is_first_turn: bool,
    pub workspace: &'a str,
    pub extra_dirs: &'a [String],
    pub model: &'a str,
    pub effort: &'a str,
    pub mode: &'a str,
    pub agent: &'a str,
    pub system_prompt: &'a str,
    pub sandbox: bool,
    pub project: &'a str,
    pub settings_json: Option<String>,
    pub json_schema: Option<&'a str>,
    pub remote_agent: bool,
    pub backend: ExecutorBackend,
}

fn format_permission_deny_rule(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    const RULE_PREFIXES: &[&str] = &[
        "read_file(",
        "write_file(",
        "command(",
        "mcp(",
        "read_url(",
        "execute_url(",
        "download_url(",
    ];
    if trimmed.ends_with(')')
        && RULE_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
    {
        return Some(trimmed.to_string());
    }

    let mut path_str = trimmed;
    while let Some(stripped) = path_str.strip_suffix("/*") {
        path_str = stripped;
    }
    if path_str.len() > 1 {
        path_str = path_str.trim_end_matches('/');
    }
    if path_str.is_empty() {
        return None;
    }

    let expanded = if let Some(rest) = path_str.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest).to_string_lossy().into_owned()
        } else {
            path_str.to_string()
        }
    } else if path_str == "~" {
        dirs::home_dir()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_else(|| path_str.to_string())
    } else {
        path_str.to_string()
    };

    // In Antigravity permissions, `read_file(dir)` matches recursively and denying read implies
    // denying write as well.
    Some(format!("read_file({expanded})"))
}

/// Constructs an ephemeral `agy --settings <json>` payload combining per-session thinking
/// visibility, orchestrator planner/executor overrides, and `permissions.deny` rules protecting
/// sensitive paths (`~/.config/agygram/*`, `~/.config/userbot/*`, `~/.ssh/*`, plus any caller
/// supplied paths).
#[must_use]
pub fn build_session_settings_json(
    show_thinking: Option<bool>,
    planner: Option<&str>,
    executor: Option<&str>,
    deny_paths: &[String],
) -> Option<String> {
    let mut root = serde_json::Map::new();

    if let Some(show) = show_thinking {
        root.insert("showThinking".to_string(), serde_json::Value::Bool(show));
    }

    let mut orch = serde_json::Map::new();
    if let Some(p) = planner.map(str::trim).filter(|s| !s.is_empty()) {
        orch.insert(
            "planner".to_string(),
            serde_json::Value::String(p.to_string()),
        );
    }
    if let Some(e) = executor.map(str::trim).filter(|s| !s.is_empty()) {
        orch.insert(
            "executor".to_string(),
            serde_json::Value::String(e.to_string()),
        );
    }
    if !orch.is_empty() {
        root.insert("orchestrator".to_string(), serde_json::Value::Object(orch));
    }

    let mut deny_rules: Vec<serde_json::Value> = Vec::new();
    let mut seen_rules: FxHashSet<String> = FxHashSet::default();
    for raw in DEFAULT_SENSITIVE_DENY_PATHS
        .iter()
        .copied()
        .chain(deny_paths.iter().map(String::as_str))
    {
        if let Some(rule) = format_permission_deny_rule(raw)
            && seen_rules.insert(rule.clone())
        {
            deny_rules.push(serde_json::Value::String(rule));
        }
    }
    if !deny_rules.is_empty() {
        let mut perms = serde_json::Map::new();
        perms.insert("deny".to_string(), serde_json::Value::Array(deny_rules));
        root.insert("permissions".to_string(), serde_json::Value::Object(perms));
    }

    (!root.is_empty()).then(|| serde_json::Value::Object(root).to_string())
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnDoneData {
    pub accumulated_text: String,
    pub conversation_id: Option<String>,
    pub final_status: String,
    pub final_usage: Option<TokenUsage>,
    pub last_context_tokens: Option<u64>,
    pub num_turns: Option<u64>,
    pub saw_checkpoint: bool,
    pub final_duration: Option<f64>,
    pub final_finished_reason: Option<String>,
    pub final_structured_output: Option<serde_json::Value>,
    pub is_final_result: bool,
    pub has_background_activity: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnSignal {
    Turn1Done(TurnDoneData),
    ToolResumed,
    FinalResult(TurnDoneData),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetachDecision {
    Pending,
    Synchronous,
    Detached,
}

#[derive(Debug, Clone)]
pub(crate) struct StdoutResult {
    accumulated_text: String,
    conversation_id: Option<String>,
    final_status: String,
    final_usage: Option<TokenUsage>,
    last_context_tokens: Option<u64>,
    num_turns: Option<u64>,
    saw_checkpoint: bool,
    final_duration: Option<f64>,
    final_finished_reason: Option<String>,
    final_structured_output: Option<serde_json::Value>,
}

impl Default for StdoutResult {
    fn default() -> Self {
        Self {
            accumulated_text: String::new(),
            conversation_id: None,
            final_status: "SUCCESS".to_string(),
            final_usage: None,
            last_context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            final_duration: None,
            final_finished_reason: None,
            final_structured_output: None,
        }
    }
}

fn is_background_tool_name(name: &str) -> bool {
    matches!(
        name,
        "invoke_subagent" | "swarm" | "schedule" | "run_workflow"
    ) || crate::text::contains_ascii_case_insensitive_str(name, "subagent")
}

/// Pure, testable stream-json parser and turn/detachment state machine.
pub(crate) struct StreamParserState {
    pub first_turn_text: Option<String>,
    pub working_text: String,
    pub detected_conv_id: Option<String>,
    pub final_status: String,
    pub final_usage: Option<TokenUsage>,
    pub last_context_tokens: Option<u64>,
    pub num_turns: Option<u64>,
    pub saw_checkpoint: bool,
    pub final_duration: Option<f64>,
    pub final_finished_reason: Option<String>,
    pub final_structured_output: Option<serde_json::Value>,
    pub is_detached_mode: bool,
    pub first_turn_completed: bool,
    pub turn_has_unflushed_text: bool,
    pub pending_paragraph_break: bool,
    pub pending_post_turn1_events: Vec<StreamEvent>,
    pub active_tool_steps: FxHashSet<u64>,
    pub seen_tool_steps: FxHashSet<u64>,
    pub seen_subagent_steps: FxHashSet<u64>,
    pub active_anon_tools: usize,
    pub saw_background_activity: bool,
}

impl StreamParserState {
    pub(crate) fn new() -> Self {
        Self {
            first_turn_text: None,
            working_text: String::new(),
            detected_conv_id: None,
            final_status: "SUCCESS".to_string(),
            final_usage: None,
            last_context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            final_duration: None,
            final_finished_reason: None,
            final_structured_output: None,
            is_detached_mode: false,
            first_turn_completed: false,
            turn_has_unflushed_text: false,
            pending_paragraph_break: false,
            pending_post_turn1_events: Vec::new(),
            active_tool_steps: FxHashSet::default(),
            seen_tool_steps: FxHashSet::default(),
            seen_subagent_steps: FxHashSet::default(),
            active_anon_tools: 0,
            saw_background_activity: false,
        }
    }

    #[inline]
    fn has_pending_tools(&self) -> bool {
        !self.active_tool_steps.is_empty() || self.active_anon_tools > 0
    }

    fn append_grace_turn_text(&mut self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        match self.first_turn_text.as_mut() {
            Some(existing) if !existing.trim().is_empty() => {
                let end_len = existing.trim_end().len();
                existing.truncate(end_len);
                existing.push_str("\n\n");
                existing.push_str(text.trim_start());
            }
            _ => {
                self.first_turn_text = Some(text.to_string());
            }
        }
    }

    fn resume_turn1_on_tool(
        &mut self,
        decision: DetachDecision,
        out_events: &mut Vec<StreamEvent>,
        out_signals: &mut Vec<TurnSignal>,
    ) {
        if self.first_turn_completed
            && !self.is_detached_mode
            && decision == DetachDecision::Pending
        {
            let combined = self.combined_non_detached_text();
            self.pending_paragraph_break = !combined.trim().is_empty() && !combined.ends_with('\n');
            self.working_text = combined;
            self.first_turn_text = None;
            self.first_turn_completed = false;
            if !self.pending_post_turn1_events.is_empty() {
                out_events.append(&mut self.pending_post_turn1_events);
            }
            out_signals.push(TurnSignal::ToolResumed);
        }
    }

    /// Applies an updated detachment decision from the coordinator loop.
    /// Returns `(keep_stream_open, events_to_flush)`.
    pub(crate) fn apply_decision(&mut self, decision: DetachDecision) -> (bool, Vec<StreamEvent>) {
        match decision {
            DetachDecision::Pending => (true, Vec::new()),
            DetachDecision::Synchronous => {
                self.pending_post_turn1_events.clear();
                (false, Vec::new())
            }
            DetachDecision::Detached => {
                if !self.is_detached_mode {
                    self.is_detached_mode = true;
                    // Free turn-1 buffered text once detached so long background runs stay lean.
                    self.first_turn_text = None;
                    let flushed = std::mem::take(&mut self.pending_post_turn1_events);
                    (true, flushed)
                } else {
                    (true, Vec::new())
                }
            }
        }
    }

    fn push_event(
        &mut self,
        event: StreamEvent,
        decision: DetachDecision,
        out_events: &mut Vec<StreamEvent>,
    ) {
        if self.first_turn_completed
            && !self.is_detached_mode
            && decision == DetachDecision::Pending
        {
            if self.pending_post_turn1_events.len() < MAX_PENDING_DETACH_EVENTS {
                self.pending_post_turn1_events.push(event);
            }
        } else if decision != DetachDecision::Synchronous {
            out_events.push(event);
        }
    }

    pub(crate) fn combined_non_detached_text(&self) -> String {
        match (
            self.first_turn_text.as_deref(),
            self.working_text.trim().is_empty(),
        ) {
            (Some(t1), true) => t1.to_string(),
            (Some(t1), false) if t1.trim().is_empty() => self.working_text.clone(),
            (Some(t1), false) => {
                let mut combined = String::with_capacity(t1.len() + 2 + self.working_text.len());
                combined.push_str(t1.trim_end());
                combined.push_str("\n\n");
                combined.push_str(self.working_text.trim_start());
                combined
            }
            (None, _) => self.working_text.clone(),
        }
    }

    pub(crate) fn process_line(
        &mut self,
        raw_line: &str,
        decision: DetachDecision,
        out_events: &mut Vec<StreamEvent>,
        out_signals: &mut Vec<TurnSignal>,
    ) {
        let (_, flushed) = self.apply_decision(decision);
        if !flushed.is_empty() {
            out_events.extend(flushed);
        }

        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            return;
        }

        match serde_json::from_str::<RawAgyJsonEvent>(trimmed) {
            Ok(RawAgyJsonEvent::Init {
                conversation_id,
                init,
            }) => {
                if let Some(ref meta) = init {
                    debug!(
                        conversation_id = ?conversation_id,
                        cwd = ?meta.cwd,
                        model = ?meta.model,
                        agent = ?meta.agent,
                        permission_mode = ?meta.permission_mode,
                        tool_count = meta.tools.len(),
                        "agy stream-json init"
                    );
                }
                if let Some(cid) = normalize_conversation_id(conversation_id) {
                    self.detected_conv_id = Some(cid.clone());
                    self.push_event(
                        StreamEvent::Init {
                            conversation_id: cid,
                        },
                        decision,
                        out_events,
                    );
                }
            }
            Ok(RawAgyJsonEvent::StepUpdate { mut step_update }) => {
                if let Some(cid) = normalize_conversation_id(step_update.conversation_id.take())
                    && self.detected_conv_id.is_none()
                {
                    self.detected_conv_id = Some(cid.clone());
                    self.push_event(
                        StreamEvent::Init {
                            conversation_id: cid,
                        },
                        decision,
                        out_events,
                    );
                }

                if let Some(thinking) = step_update.thinking_delta.take() {
                    self.push_event(StreamEvent::ThinkingDelta(thinking), decision, out_events);
                }

                if let Some(delta) = step_update.text_delta.take() {
                    if self.pending_paragraph_break && !delta.is_empty() {
                        if !self.working_text.ends_with("\n\n") && !delta.starts_with('\n') {
                            self.working_text.push_str("\n\n");
                            self.push_event(
                                StreamEvent::TextDelta("\n\n".to_string()),
                                decision,
                                out_events,
                            );
                        }
                        self.pending_paragraph_break = false;
                    }
                    self.working_text.push_str(&delta);
                    self.turn_has_unflushed_text = true;
                    self.push_event(StreamEvent::TextDelta(delta), decision, out_events);
                }

                if let Some(u) = step_update.usage.take() {
                    if !step_update.is_checkpoint() {
                        let step_ctx = u.input_tokens.saturating_add(u.cache_read_tokens);
                        if step_ctx > 0 {
                            self.last_context_tokens = Some(step_ctx);
                        }
                    }
                    self.final_usage = Some(u);
                }
                if let Some(d) = step_update.duration_seconds {
                    self.final_duration = Some(d);
                }

                if let Some(sub_info) = step_update.subagent_info.take() {
                    self.saw_background_activity = true;
                    let should_emit = step_update.step_index.is_none_or(|idx| {
                        if self.seen_subagent_steps.len() > 4096 {
                            self.seen_subagent_steps.clear();
                        }
                        self.seen_subagent_steps.insert(idx)
                    });
                    if should_emit && let Some(summary) = sub_info.summary() {
                        self.push_event(
                            StreamEvent::SubagentUpdate { summary },
                            decision,
                            out_events,
                        );
                    }
                }

                if step_update.is_checkpoint() {
                    self.saw_checkpoint = true;
                    self.last_context_tokens = Some(0);
                    let should_emit = match step_update.step_index {
                        Some(idx) => {
                            if self.seen_tool_steps.len() > 4096 {
                                self.seen_tool_steps.clear();
                            }
                            self.seen_tool_steps.insert(idx)
                        }
                        None => step_update.state != Some(StepState::Active),
                    };
                    if should_emit {
                        self.push_event(StreamEvent::Checkpoint, decision, out_events);
                    }
                } else if step_update.is_tool_step() {
                    let idx_opt = step_update.step_index;
                    let peeked_tool = step_update.peek_tool_name().map(str::to_owned);
                    if peeked_tool.as_deref().is_some_and(is_background_tool_name) {
                        self.saw_background_activity = true;
                    }
                    let tool_error = step_update.take_tool_error(peeked_tool.as_deref());
                    let is_active = StepUpdatePayload::is_active_tool_state(step_update.state);

                    if !is_active {
                        if let Some(idx) = idx_opt {
                            self.active_tool_steps.remove(&idx);
                        } else {
                            self.active_anon_tools = self.active_anon_tools.saturating_sub(1);
                        }
                        let already_seen =
                            idx_opt.is_some_and(|idx| self.seen_tool_steps.contains(&idx));
                        if !already_seen
                            && let Some((tool_name, summary)) = step_update.take_tool_use()
                        {
                            if let Some(idx) = idx_opt {
                                if self.seen_tool_steps.len() > 4096 {
                                    self.seen_tool_steps.clear();
                                }
                                self.seen_tool_steps.insert(idx);
                            }
                            self.push_event(
                                StreamEvent::ToolUse {
                                    tool: tool_name,
                                    summary,
                                },
                                decision,
                                out_events,
                            );
                        }
                    } else {
                        if let Some(idx) = idx_opt {
                            self.active_tool_steps.insert(idx);
                        } else {
                            // Cap anonymous tool count at 1 so repeated ACTIVE frames without a
                            // step_index cannot accumulate and block turn completion indefinitely.
                            self.active_anon_tools = 1;
                        }
                        self.resume_turn1_on_tool(decision, out_events, out_signals);
                        let should_emit = idx_opt.is_none_or(|idx| {
                            if self.seen_tool_steps.len() > 4096 {
                                self.seen_tool_steps.clear();
                            }
                            self.seen_tool_steps.insert(idx)
                        });
                        if should_emit
                            && let Some((tool_name, summary)) = step_update.take_tool_use()
                        {
                            self.push_event(
                                StreamEvent::ToolUse {
                                    tool: tool_name,
                                    summary,
                                },
                                decision,
                                out_events,
                            );
                        }
                    }

                    if let Some((tool, message)) = tool_error {
                        self.push_event(
                            StreamEvent::ToolError { tool, message },
                            decision,
                            out_events,
                        );
                    }
                } else if step_update.peek_tool_name().is_some()
                    || step_update
                        .tool_info
                        .as_ref()
                        .is_some_and(|i| i.error.is_some())
                {
                    let peeked_tool = step_update.peek_tool_name().map(str::to_owned);
                    if peeked_tool.as_deref().is_some_and(is_background_tool_name) {
                        self.saw_background_activity = true;
                    }
                    let tool_error = step_update.take_tool_error(peeked_tool.as_deref());
                    if let Some((tool_name, summary)) = step_update.take_tool_use() {
                        self.resume_turn1_on_tool(decision, out_events, out_signals);
                        self.push_event(
                            StreamEvent::ToolUse {
                                tool: tool_name,
                                summary,
                            },
                            decision,
                            out_events,
                        );
                    }
                    if let Some((tool, message)) = tool_error {
                        self.push_event(
                            StreamEvent::ToolError { tool, message },
                            decision,
                            out_events,
                        );
                    }
                } else if step_update.is_agent_turn_done()
                    && !self.working_text.trim().is_empty()
                    && !self.has_pending_tools()
                {
                    self.turn_has_unflushed_text = false;
                    self.pending_paragraph_break = false;
                    if !self.first_turn_completed {
                        let captured = std::mem::take(&mut self.working_text);
                        if decision != DetachDecision::Synchronous {
                            out_events.push(StreamEvent::TurnComplete {
                                text: captured.clone(),
                            });
                        }
                        self.first_turn_completed = true;
                        self.first_turn_text = Some(captured.clone());
                        out_signals.push(TurnSignal::Turn1Done(TurnDoneData {
                            accumulated_text: captured,
                            conversation_id: self.detected_conv_id.clone(),
                            final_status: self.final_status.clone(),
                            final_usage: self.final_usage.clone(),
                            last_context_tokens: self.last_context_tokens,
                            num_turns: self.num_turns,
                            saw_checkpoint: self.saw_checkpoint,
                            final_duration: self.final_duration,
                            final_finished_reason: self.final_finished_reason.clone(),
                            final_structured_output: self.final_structured_output.clone(),
                            is_final_result: false,
                            has_background_activity: self.saw_background_activity,
                        }));
                    } else {
                        // Always take `working_text` so a subsequent Turn 3 during or after the
                        // 1.5s grace window never concatenates stale Turn 2 text.
                        let turn_text = std::mem::take(&mut self.working_text);
                        if !self.is_detached_mode {
                            self.append_grace_turn_text(&turn_text);
                        }
                        self.push_event(
                            StreamEvent::TurnComplete { text: turn_text },
                            decision,
                            out_events,
                        );
                    }
                }
            }
            Ok(RawAgyJsonEvent::Result { result }) => {
                let is_waiting = result.is_waiting();
                if let Some(cid) = normalize_conversation_id(result.conversation_id) {
                    self.detected_conv_id = Some(cid);
                }
                if let Some(st) = result.status {
                    self.final_status = st;
                }
                if let Some(u) = result.usage {
                    self.final_usage = Some(u);
                }
                if let Some(n) = result.num_turns {
                    self.num_turns = Some(n);
                }
                if let Some(d) = result.duration_seconds {
                    self.final_duration = Some(d);
                }
                if let Some(reason) = result.finished_reason {
                    self.final_finished_reason = Some(reason);
                }
                if let Some(structured) = result.structured_output {
                    self.final_structured_output = Some(structured);
                }
                if is_waiting {
                    info!(
                        conversation_id = ?self.detected_conv_id,
                        finished_reason = ?self.final_finished_reason,
                        "agy returned WAITING status in stream-json mode"
                    );
                } else if !self.final_status.eq_ignore_ascii_case("SUCCESS") {
                    warn!(
                        conversation_id = ?self.detected_conv_id,
                        status = %self.final_status,
                        finished_reason = ?self.final_finished_reason,
                        error = ?result.error,
                        "agy stream-json result reported non-SUCCESS status"
                    );
                }

                if self.is_detached_mode {
                    if self.turn_has_unflushed_text && !self.working_text.trim().is_empty() {
                        let flushed_text = std::mem::take(&mut self.working_text);
                        self.turn_has_unflushed_text = false;
                        out_events.push(StreamEvent::TurnComplete { text: flushed_text });
                    } else if let Some(err) = result.error.filter(|e| !e.trim().is_empty()) {
                        out_events.push(StreamEvent::Error(err));
                        self.working_text.clear();
                    } else {
                        self.working_text.clear();
                    }
                    // Emit final detached Result with empty response (text already sent via
                    // TurnComplete) so TG/QQ detached follow-ups propagate context_tokens & checkpoint.
                    out_events.push(StreamEvent::Result(Box::new(StreamResultData {
                        conversation_id: self.detected_conv_id.clone(),
                        response: String::new(),
                        status: self.final_status.clone(),
                        usage: self.final_usage.clone(),
                        context_tokens: self.last_context_tokens,
                        num_turns: self.num_turns,
                        saw_checkpoint: self.saw_checkpoint,
                        duration_seconds: self.final_duration,
                        finished_reason: self.final_finished_reason.clone(),
                        structured_output: self.final_structured_output.clone(),
                    })));
                } else {
                    let should_use_result_response = !self.first_turn_completed
                        || (self.working_text.trim().is_empty()
                            && self
                                .first_turn_text
                                .as_deref()
                                .is_none_or(|t| t.trim().is_empty()));
                    if should_use_result_response {
                        if let Some(resp) = result.response
                            && !resp.trim().is_empty()
                        {
                            self.working_text = resp;
                        } else if let Some(err) = result.error
                            && !err.trim().is_empty()
                        {
                            self.working_text = format!("❌ {err}");
                        } else if is_waiting {
                            self.working_text =
                                "⏸️ Agent 正在等待确认或交互输入（当前非交互模式无法响应）。"
                                    .to_string();
                        }
                    }

                    let final_text = self.combined_non_detached_text();
                    out_signals.push(TurnSignal::FinalResult(TurnDoneData {
                        accumulated_text: final_text,
                        conversation_id: self.detected_conv_id.clone(),
                        final_status: self.final_status.clone(),
                        final_usage: self.final_usage.clone(),
                        last_context_tokens: self.last_context_tokens,
                        num_turns: self.num_turns,
                        saw_checkpoint: self.saw_checkpoint,
                        final_duration: self.final_duration,
                        final_finished_reason: self.final_finished_reason.clone(),
                        final_structured_output: self.final_structured_output.clone(),
                        is_final_result: true,
                        has_background_activity: self.saw_background_activity,
                    }));
                }
            }
            Ok(RawAgyJsonEvent::Unknown) => {
                debug!("ignoring unknown stream-json event");
            }
            Err(err) => {
                if trimmed.starts_with('{') {
                    warn!(
                        error = %err,
                        line_len = trimmed.len(),
                        "dropping malformed stream-json line starting with '{{'"
                    );
                    return;
                }
                self.working_text.push_str(trimmed);
                self.working_text.push('\n');
                self.turn_has_unflushed_text = true;
                let mut delta = String::with_capacity(trimmed.len() + 1);
                delta.push_str(trimmed);
                delta.push('\n');
                self.push_event(StreamEvent::TextDelta(delta), decision, out_events);
            }
        }
    }

    pub(crate) fn finish_eof(mut self) -> (Option<StreamEvent>, StdoutResult) {
        let eof_event = if self.is_detached_mode
            && self.turn_has_unflushed_text
            && !self.working_text.trim().is_empty()
        {
            Some(StreamEvent::TurnComplete {
                text: std::mem::take(&mut self.working_text),
            })
        } else {
            None
        };

        let accumulated_text = if self.is_detached_mode {
            self.working_text
        } else {
            self.combined_non_detached_text()
        };

        (
            eof_event,
            StdoutResult {
                accumulated_text,
                conversation_id: self.detected_conv_id,
                final_status: self.final_status,
                final_usage: self.final_usage,
                last_context_tokens: self.last_context_tokens,
                num_turns: self.num_turns,
                saw_checkpoint: self.saw_checkpoint,
                final_duration: self.final_duration,
                final_finished_reason: self.final_finished_reason,
                final_structured_output: self.final_structured_output,
            },
        )
    }
}

/// Ensures the spawned process group and reader tasks are terminated if `run_prompt_streaming` is
/// dropped/cancelled before transferring ownership to a background reaper.
struct ProcessCleanupGuard {
    pid: Option<u32>,
    stdout_abort: Option<tokio::task::AbortHandle>,
    stderr_abort: Option<tokio::task::AbortHandle>,
    armed: bool,
}

impl ProcessCleanupGuard {
    fn new(
        pid: Option<u32>,
        stdout_abort: tokio::task::AbortHandle,
        stderr_abort: tokio::task::AbortHandle,
    ) -> Self {
        Self {
            pid,
            stdout_abort: Some(stdout_abort),
            stderr_abort: Some(stderr_abort),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.stdout_abort = None;
        self.stderr_abort = None;
    }
}

impl Drop for ProcessCleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(handle) = self.stdout_abort.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_abort.take() {
            handle.abort();
        }
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGKILL);
        }
    }
}

/// Returns `true` when `prompt` is a CLI-answered slash command that `agy --input-format stream-json`
/// rejects (every entry in [`BARE_PASSTHROUGH_COMMANDS`] except `/compact`, which works natively
/// in persistent mode).
#[must_use]
pub(crate) fn is_persistent_incompatible_slash_command(prompt: &str) -> bool {
    let trimmed_start = prompt.trim_start();
    if !trimmed_start.starts_with('/') {
        return false;
    }
    let cmd_token = trimmed_start.split_whitespace().next().unwrap_or("");
    !cmd_token.eq_ignore_ascii_case("/compact")
        && BARE_PASSTHROUGH_COMMANDS
            .iter()
            .any(|cmd| cmd_token.eq_ignore_ascii_case(cmd))
}

/// Formats the prompt passed to `agy`, preserving leading `/` commands so `agy` triggers native
/// slash command and skill expansion. Also reports whether a non-empty `system_prompt` was
/// actually embedded (`false` when empty or stripped by [`BARE_PASSTHROUGH_COMMANDS`]).
pub(crate) fn format_effective_prompt_with_flag<'a>(
    prompt: &'a str,
    system_prompt: &str,
) -> (Cow<'a, str>, bool) {
    let sys_trimmed = system_prompt.trim();
    if sys_trimmed.is_empty() {
        return (Cow::Borrowed(prompt), false);
    }

    let trimmed_start = prompt.trim_start();
    if trimmed_start.starts_with('/') {
        let cmd_token = trimmed_start.split_whitespace().next().unwrap_or("");
        if BARE_PASSTHROUGH_COMMANDS
            .iter()
            .any(|cmd| cmd_token.eq_ignore_ascii_case(cmd))
        {
            return (Cow::Borrowed(trimmed_start), false);
        }
        return (
            Cow::Owned(format!(
                "{trimmed_start}\n\n[System Instructions: {sys_trimmed}]"
            )),
            true,
        );
    }

    (
        Cow::Owned(format!("[System Instructions: {system_prompt}]\n{prompt}")),
        true,
    )
}

#[cfg(test)]
fn format_effective_prompt<'a>(prompt: &'a str, system_prompt: &str) -> Cow<'a, str> {
    format_effective_prompt_with_flag(prompt, system_prompt).0
}

fn build_agy_command_with_prompt(params: &ExecutionParams<'_>, final_prompt: &str) -> Command {
    let mut cmd = Command::new(get_agy_path());
    cmd.kill_on_drop(true);
    cmd.process_group(0);
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");

    cmd.arg("-p")
        .arg(final_prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--dangerously-skip-permissions");

    if let Some(conv_id) = params
        .conversation_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && crate::models::is_safe_cli_value(s))
    {
        cmd.arg("--conversation").arg(conv_id);
    } else if !params.is_first_turn {
        cmd.arg("-c");
    }

    if !params.workspace.is_empty() {
        // `agy` automatically mounts its working directory as the primary workspace root; passing
        // `--add-dir <workspace>` on top would duplicate `workspaceDirs` and double file-watcher load.
        cmd.current_dir(params.workspace);
    }

    for dir in resolve_extra_dirs(params.workspace, params.extra_dirs) {
        cmd.arg("--add-dir").arg(&dir);
    }

    let model_str = params.model.trim();
    if !model_str.is_empty() && crate::models::is_safe_model_id(model_str) {
        cmd.arg("--model").arg(model_str);
    }

    if let Some(effort_arg) = crate::models::effective_cli_effort(model_str, params.effort)
        && crate::models::is_supported_effort(effort_arg)
    {
        cmd.arg("--effort").arg(effort_arg);
    }

    if !params.mode.is_empty() && crate::models::is_supported_mode(params.mode.trim()) {
        cmd.arg("--mode").arg(params.mode.trim());
    }

    if !params.agent.is_empty() && crate::models::is_safe_free_value(params.agent.trim()) {
        cmd.arg("--agent").arg(params.agent.trim());
    }

    if params.sandbox {
        cmd.arg("--sandbox");
    }

    if !params.project.is_empty() && crate::models::is_safe_free_value(params.project.trim()) {
        cmd.arg("--project").arg(params.project.trim());
    }

    if let Some(ref settings) = params.settings_json {
        let trimmed = settings.trim();
        if !trimmed.is_empty() {
            cmd.arg("--settings").arg(trimmed);
        }
    }

    if let Some(schema) = params.json_schema {
        let trimmed = schema.trim();
        if !trimmed.is_empty() {
            cmd.arg("--json-schema").arg(trimmed);
        }
    }

    if params.remote_agent {
        cmd.arg("--remote-agent");
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

#[cfg(test)]
fn build_agy_command(params: &ExecutionParams<'_>) -> Command {
    let (final_prompt, _) = format_effective_prompt_with_flag(params.prompt, params.system_prompt);
    build_agy_command_with_prompt(params, final_prompt.as_ref())
}

pub(crate) async fn send_stream_events(
    stream_tx_opt: &mut Option<mpsc::Sender<StreamEvent>>,
    events: impl IntoIterator<Item = StreamEvent>,
) {
    for ev in events {
        let Some(tx) = stream_tx_opt.as_ref() else {
            break;
        };
        if tx.send(ev).await.is_err() {
            *stream_tx_opt = None;
            break;
        }
    }
}

fn spawn_stdout_reader(
    stdout: ChildStdout,
    tracker: ActivityTracker,
    stream_tx: mpsc::Sender<StreamEvent>,
    turn_done_tx: mpsc::Sender<TurnSignal>,
    mut detach_rx: watch::Receiver<DetachDecision>,
) -> tokio::task::JoinHandle<StdoutResult> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut state = StreamParserState::new();
        let mut stream_tx_opt = Some(stream_tx);
        let mut out_events: Vec<StreamEvent> = Vec::with_capacity(8);
        let mut out_signals: Vec<TurnSignal> = Vec::with_capacity(2);

        loop {
            let decision = *detach_rx.borrow_and_update();
            let (keep_open, flushed) = state.apply_decision(decision);
            if !keep_open {
                stream_tx_opt = None;
            } else if !flushed.is_empty() {
                send_stream_events(&mut stream_tx_opt, flushed).await;
            }

            let read_res = if decision == DetachDecision::Pending && stream_tx_opt.is_some() {
                tokio::select! {
                    res = reader.read_until(b'\n', &mut line_buf) => Some(res),
                    changed = detach_rx.changed() => {
                        if changed.is_ok() {
                            let next = *detach_rx.borrow_and_update();
                            let (keep, flushed_now) = state.apply_decision(next);
                            if !keep {
                                stream_tx_opt = None;
                            } else if !flushed_now.is_empty() {
                                send_stream_events(&mut stream_tx_opt, flushed_now).await;
                            }
                        }
                        None
                    }
                }
            } else {
                Some(reader.read_until(b'\n', &mut line_buf).await)
            };

            let Some(res) = read_res else {
                continue;
            };

            let bytes_read = match res {
                Ok(0) => {
                    if line_buf.is_empty() {
                        break;
                    }
                    0
                }
                Ok(n) => n,
                Err(err) => {
                    warn!(error = %err, "error reading agy stdout");
                    break;
                }
            };

            tracker.record_activity();
            let current_decision = *detach_rx.borrow_and_update();
            {
                let line = String::from_utf8_lossy(&line_buf);
                state.process_line(
                    line.as_ref(),
                    current_decision,
                    &mut out_events,
                    &mut out_signals,
                );
            }

            line_buf.clear();
            if line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                line_buf = Vec::with_capacity(8192);
            }

            if !out_events.is_empty() {
                send_stream_events(&mut stream_tx_opt, out_events.drain(..)).await;
            }
            for sig in out_signals.drain(..) {
                if turn_done_tx.send(sig).await.is_err() {
                    break;
                }
            }

            if bytes_read == 0 {
                break;
            }
        }

        let (eof_event, result) = state.finish_eof();
        if let Some(ev) = eof_event {
            send_stream_events(&mut stream_tx_opt, std::iter::once(ev)).await;
        }
        drop(stream_tx_opt);
        result
    })
}

fn spawn_stderr_reader(
    stderr: ChildStderr,
    tracker: ActivityTracker,
) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line_buf: Vec<u8> = Vec::with_capacity(1024);
        let mut accumulated_stderr = String::new();
        let mut truncated = false;

        loop {
            line_buf.clear();
            let bytes_read = match reader.read_until(b'\n', &mut line_buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    warn!(error = %err, "error reading agy stderr");
                    break;
                }
            };

            tracker.record_activity();
            if !truncated {
                let line = String::from_utf8_lossy(&line_buf);
                let trimmed_end = line.trim_end_matches(&['\r', '\n'][..]);
                let remaining = MAX_STDERR_BYTES.saturating_sub(accumulated_stderr.len());
                if trimmed_end.len() < remaining {
                    accumulated_stderr.push_str(trimmed_end);
                    accumulated_stderr.push('\n');
                } else {
                    let cut = floor_char_boundary(trimmed_end, remaining);
                    accumulated_stderr.push_str(&trimmed_end[..cut]);
                    accumulated_stderr.push_str("\n...[stderr truncated]\n");
                    truncated = true;
                }
            }

            if line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                line_buf = Vec::with_capacity(4096);
            }
            if bytes_read == 0 {
                break;
            }
        }

        accumulated_stderr
    })
}

#[inline]
pub(crate) fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

async fn await_reader_or_abort<T>(
    handle: &mut tokio::task::JoinHandle<T>,
    grace: Duration,
) -> Option<T> {
    match tokio::time::timeout(grace, &mut *handle).await {
        Ok(Ok(val)) => Some(val),
        Ok(Err(_)) => None,
        Err(_) => {
            handle.abort();
            None
        }
    }
}

/// Short reaper for completed one-shot `Print` processes: 2s passive wait -> `SIGINT` -> 3s `SIGKILL`.
fn spawn_final_result_reaper(
    mut child: Child,
    child_pid: Option<u32>,
    mut stdout_task: tokio::task::JoinHandle<StdoutResult>,
    mut stderr_task: tokio::task::JoinHandle<String>,
    budget_guard: persistent::ExternalProcessBudgetGuard,
) {
    tokio::spawn(async move {
        let _budget_guard = budget_guard;
        if tokio::time::timeout(FINAL_RESULT_PASSIVE_GRACE, child.wait())
            .await
            .is_ok()
        {
            debug!(pid = ?child_pid, mode = "final_result", "agy process exited cleanly");
        } else {
            debug!(
                pid = ?child_pid,
                mode = "final_result",
                "agy still alive after 2s post-Result passive grace; sending SIGINT"
            );
            if let Some(pid) = child_pid {
                signal_process_tree(pid, libc::SIGINT);
            }
            if tokio::time::timeout(FINAL_RESULT_SIGINT_GRACE, child.wait())
                .await
                .is_err()
            {
                warn!(
                    pid = ?child_pid,
                    mode = "final_result",
                    "agy process exceeded post-SIGINT grace; killing process group"
                );
                if let Some(pid) = child_pid {
                    signal_process_tree(pid, libc::SIGKILL);
                }
                let _ = child.kill().await;
            }
        }
        await_reader_or_abort(&mut stdout_task, Duration::from_secs(2)).await;
        await_reader_or_abort(&mut stderr_task, Duration::from_secs(1)).await;
    });
}

fn spawn_background_reaper(
    mut child: Child,
    child_pid: Option<u32>,
    mut stdout_task: tokio::task::JoinHandle<StdoutResult>,
    mut stderr_task: tokio::task::JoinHandle<String>,
    wait_timeout: Duration,
    mode_label: &'static str,
    budget_guard: persistent::ExternalProcessBudgetGuard,
) {
    tokio::spawn(async move {
        let _budget_guard = budget_guard;
        match tokio::time::timeout(wait_timeout, child.wait()).await {
            Ok(_) => debug!(pid = ?child_pid, mode = mode_label, "agy process exited cleanly"),
            Err(_) => {
                warn!(
                    pid = ?child_pid,
                    mode = mode_label,
                    timeout_secs = wait_timeout.as_secs(),
                    "agy process exceeded reaper timeout; killing process group"
                );
                if let Some(pid) = child_pid {
                    signal_process_tree(pid, libc::SIGKILL);
                }
                let _ = child.kill().await;
            }
        }
        await_reader_or_abort(&mut stdout_task, Duration::from_secs(5)).await;
        await_reader_or_abort(&mut stderr_task, Duration::from_secs(2)).await;
    });
}

pub(crate) fn format_timeout_messages(reason: TimeoutReason) -> (String, String) {
    match reason {
        TimeoutReason::StdinDeadlock { command } => {
            let safe_cmd = command.replace("```", "` ` `");
            (
                format!(
                    "agy 任务终止：检测到子进程卡死在等待终端交互输入 ({command})。由于非交互模式无法输入，已自动终止。"
                ),
                format!(
                    "⚠️ **任务死锁终止**\n检测到后台子进程正在等待终端用户输入：\n```\n{safe_cmd}\n```\n非交互模式无法响应，已自动终止以防挂起。"
                ),
            )
        }
        TimeoutReason::FrozenDeadlock {
            idle_secs,
            active_processes,
        } => {
            let proc_str = if active_processes.is_empty() {
                "无活动子进程".to_string()
            } else {
                active_processes.join(", ")
            };
            let safe_procs = proc_str.replace('`', "'");
            (
                format!(
                    "agy 执行卡死 ({idle_secs}s 无任何输出且系统 0 算力/IO 消耗)，任务已终止。关联进程: {proc_str}"
                ),
                format!(
                    "⏱ **任务卡死终止**\n超过 {idle_secs}s 无任何输出，且系统监测到 0 算力消耗与 0 I/O 进展（可能网络连接静默断开或死锁）。\n关联进程: `{safe_procs}`\n\n请发送 /new 重置会话后重试。"
                ),
            )
        }
        TimeoutReason::MaxExecutionExceeded { max_secs } => (
            format!("agy 执行超时 (超过全局安全上限 {max_secs}s)，任务已强制终止。"),
            format!(
                "⏱ **任务超时终止**\n任务总运行时间已超过全局安全上限 ({max_secs}s / {}分钟)。已强制终止以防死循环。\n请发送 /new 重置会话后重试。",
                max_secs / 60
            ),
        ),
    }
}

struct CliOutput {
    success: bool,
    status_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn combine_cli_output(stdout: &str, stderr: &str) -> String {
    let out_trimmed = stdout.trim();
    let err_trimmed = stderr.trim();
    match (out_trimmed.is_empty(), err_trimmed.is_empty()) {
        (true, true) => String::new(),
        (false, true) => out_trimmed.to_string(),
        (true, false) => err_trimmed.to_string(),
        (false, false) => format!("{out_trimmed}\n{err_trimmed}"),
    }
}

async fn exec_command_with_timeout(
    program: impl AsRef<std::ffi::OsStr>,
    args: &[&str],
    timeout_dur: Duration,
    context_label: &str,
) -> Result<CliOutput> {
    evict_idle_for_budget().await;

    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    cmd.process_group(0);

    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute {context_label}"))?;
    let child_pid = child.id();

    let output = match tokio::time::timeout(timeout_dur, child.wait_with_output()).await {
        Ok(res) => res.with_context(|| format!("Failed while waiting for {context_label}"))?,
        Err(_) => {
            if let Some(pid) = child_pid {
                signal_process_tree(pid, libc::SIGKILL);
            }
            anyhow::bail!("{context_label} timed out after {}s", timeout_dur.as_secs());
        }
    };

    Ok(CliOutput {
        success: output.status.success(),
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn brain_dir_candidates() -> &'static [PathBuf] {
    BRAIN_DIRS_STATIC.get_or_init(|| {
        let mut candidates = Vec::with_capacity(2);
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(".gemini/antigravity-cli/brain"));
            let legacy = home.join(".gemini/antigravity/brain");
            if !candidates.contains(&legacy) {
                candidates.push(legacy);
            }
        }
        candidates
    })
}

async fn entry_dir_metadata(entry: &tokio::fs::DirEntry) -> Option<std::fs::Metadata> {
    let ft = entry.file_type().await.ok()?;
    if ft.is_dir() {
        entry.metadata().await.ok()
    } else if ft.is_symlink() {
        match tokio::fs::metadata(entry.path()).await {
            Ok(meta) if meta.is_dir() => Some(meta),
            _ => None,
        }
    } else {
        None
    }
}

async fn entry_is_dir(entry: &tokio::fs::DirEntry) -> bool {
    match entry.file_type().await {
        Ok(ft) if ft.is_dir() => true,
        Ok(ft) if ft.is_symlink() => tokio::fs::metadata(entry.path())
            .await
            .map(|m| m.is_dir())
            .unwrap_or(false),
        _ => false,
    }
}

#[derive(Deserialize)]
struct TranscriptLine<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
    content: Option<String>,
}

async fn extract_transcript_title(conv_id: &str, transcript_path: &Path) -> Option<String> {
    let cache_lock = CONV_TITLE_CACHE.get_or_init(|| StdRwLock::new(FxHashMap::default()));
    if let Ok(guard) = cache_lock.read()
        && let Some((_, cached)) = guard.get(conv_id)
    {
        return Some(cached.clone());
    }

    let file = tokio::fs::File::open(transcript_path).await.ok()?;
    let mut reader = BufReader::new(file.take(MAX_TRANSCRIPT_TITLE_SCAN_BYTES));
    let mut line_buf: Vec<u8> = Vec::with_capacity(1024);

    for _ in 0..MAX_TRANSCRIPT_TITLE_SCAN_LINES {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf).await.ok()?;
        if n == 0 {
            return None;
        }
        let line = String::from_utf8_lossy(&line_buf);
        if !line.contains("USER_INPUT") {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<TranscriptLine<'_>>(line.as_ref())
            && parsed.kind == Some("USER_INPUT")
            && let Some(c) = parsed.content
        {
            let clean_prompt = clean_user_prompt_summary(&c);
            if !clean_prompt.is_empty() {
                if let Ok(mut guard) = cache_lock.write() {
                    if guard.len() >= MAX_CACHED_TITLES {
                        let mut seqs: Vec<u64> = guard.values().map(|(seq, _)| *seq).collect();
                        seqs.sort_unstable();
                        let cutoff = seqs[seqs.len() / 2];
                        guard.retain(|_, (seq, _)| *seq >= cutoff);
                    }
                    let seq = CONV_TITLE_SEQ.fetch_add(1, Ordering::Relaxed);
                    guard.insert(conv_id.to_string(), (seq, clean_prompt.clone()));
                }
                return Some(clean_prompt);
            }
        }
    }
    None
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SkillMetadata {
    name: Option<String>,
    description: String,
    disable_slash_command: bool,
    #[allow(dead_code)]
    disable_model_invocation: bool,
}

fn parse_yaml_bool_true(raw: &str) -> bool {
    raw.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .eq_ignore_ascii_case("true")
}

async fn read_skill_metadata(skill_md: &Path) -> SkillMetadata {
    let Ok(file) = tokio::fs::File::open(skill_md).await else {
        return SkillMetadata::default();
    };
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    let mut meta = SkillMetadata::default();
    let mut in_frontmatter = false;
    let mut saw_first_line = false;
    let mut reading_multiline_desc = false;

    for _ in 0..80 {
        line_buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut line_buf).await else {
            break;
        };
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        let raw_line = line.trim_end_matches(['\r', '\n']);
        let trimmed = raw_line.trim();

        if !saw_first_line {
            saw_first_line = true;
            if trimmed == "---" {
                in_frontmatter = true;
                continue;
            }
            // Tolerate files without leading `---` delimiter.
        } else if in_frontmatter && trimmed == "---" {
            break;
        }

        let is_indented = raw_line.starts_with(' ') || raw_line.starts_with('\t');
        if reading_multiline_desc {
            if is_indented {
                if meta.description.is_empty() && !trimmed.is_empty() {
                    meta.description = trimmed.trim_matches('"').trim_matches('\'').to_string();
                }
                continue;
            }
            if trimmed.is_empty() {
                continue;
            }
            reading_multiline_desc = false;
        }

        if is_indented {
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("name:") {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                meta.name = Some(val.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("description:") {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if matches!(val, ">" | "|" | ">-" | "|-") {
                reading_multiline_desc = true;
            } else {
                meta.description = val.to_string();
            }
        } else if let Some(rest) = trimmed.strip_prefix("disable-slash-command:") {
            meta.disable_slash_command = parse_yaml_bool_true(rest);
        } else if let Some(rest) = trimmed.strip_prefix("disable-model-invocation:") {
            meta.disable_model_invocation = parse_yaml_bool_true(rest);
        }
    }

    meta
}

#[cfg(test)]
async fn read_skill_description(skill_md: &Path) -> String {
    read_skill_metadata(skill_md).await.description
}

async fn scan_skill_dir(dir: &Path, seen_dirs: &mut FxHashSet<PathBuf>) -> Vec<(String, String)> {
    let canon = tokio::fs::canonicalize(dir)
        .await
        .unwrap_or_else(|_| dir.to_path_buf());
    if !seen_dirs.insert(canon) {
        return Vec::new();
    }
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };

    let mut skills = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        if !entry_is_dir(&entry).await {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        if dir_name.starts_with('.') {
            continue;
        }
        let meta = read_skill_metadata(&entry.path().join("SKILL.md")).await;
        if meta.disable_slash_command {
            continue;
        }
        let skill_name = meta.name.unwrap_or(dir_name);
        skills.push((skill_name, meta.description));
    }
    skills.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    skills
}

async fn scan_plugin_skills_root(
    plugins_root: &Path,
    seen_dirs: &mut FxHashSet<PathBuf>,
) -> Vec<(String, String)> {
    let Ok(mut plugins) = tokio::fs::read_dir(plugins_root).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while let Ok(Some(plugin_entry)) = plugins.next_entry().await {
        if !entry_is_dir(&plugin_entry).await {
            continue;
        }
        let skills_dir = plugin_entry.path().join("skills");
        out.extend(scan_skill_dir(&skills_dir, seen_dirs).await);
    }
    out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    out
}

async fn collect_skill_sections() -> Result<Vec<(&'static str, Vec<(String, String)>)>, &'static str>
{
    let Some(home) = dirs::home_dir() else {
        return Err("未找到家目录");
    };

    let mut sections: Vec<(&'static str, Vec<(String, String)>)> = Vec::new();
    let mut seen_dirs: FxHashSet<PathBuf> = FxHashSet::default();

    // 1. Workspace `.agents/skills` (walking from cwd up to git root when distinct from home).
    if let Ok(cwd) = std::env::current_dir() {
        let mut ws_skills = Vec::new();
        let mut cur: Option<&Path> = Some(cwd.as_path());
        while let Some(dir) = cur {
            if dir != home.as_path() {
                ws_skills.extend(scan_skill_dir(&dir.join(".agents/skills"), &mut seen_dirs).await);
            }
            if tokio::fs::try_exists(dir.join(".git"))
                .await
                .unwrap_or(false)
            {
                break;
            }
            cur = dir.parent();
        }
        if !ws_skills.is_empty() {
            ws_skills.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            sections.push(("Workspace Skills (.agents)", ws_skills));
        }
    }

    // 2. Global user skills.
    let global_dirs = [
        ("User Skills (.agents)", home.join(".agents/skills")),
        ("User Skills (.gemini)", home.join(".gemini/config/skills")),
    ];
    for (section, dir) in global_dirs {
        let found = scan_skill_dir(&dir, &mut seen_dirs).await;
        if !found.is_empty() {
            sections.push((section, found));
        }
    }

    // 3. Installed plugin skills (`~/.gemini/config/plugins/*/skills/` & workspace plugins).
    let mut plugin_skills =
        scan_plugin_skills_root(&home.join(".gemini/config/plugins"), &mut seen_dirs).await;
    if let Ok(cwd) = std::env::current_dir()
        && cwd != home
    {
        plugin_skills
            .extend(scan_plugin_skills_root(&cwd.join(".agents/plugins"), &mut seen_dirs).await);
    }
    if !plugin_skills.is_empty() {
        plugin_skills.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        sections.push(("Plugin Skills", plugin_skills));
    }

    // 4. Builtin skills (CLI + legacy fallback).
    let builtin_dirs = [
        (
            "Builtin Skills (CLI)",
            home.join(".gemini/antigravity-cli/builtin/skills"),
        ),
        (
            "Builtin Skills (Legacy)",
            home.join(".gemini/antigravity/builtin/skills"),
        ),
    ];
    for (section, dir) in builtin_dirs {
        let found = scan_skill_dir(&dir, &mut seen_dirs).await;
        if !found.is_empty() {
            sections.push((section, found));
        }
    }

    Ok(sections)
}

pub(crate) struct RuleSection {
    pub(crate) icon: &'static str,
    pub(crate) title: String,
    pub(crate) body: String,
}

async fn collect_rule_sections(workspace: &str) -> Result<Vec<RuleSection>, &'static str> {
    let Some(home) = dirs::home_dir() else {
        return Err("未找到家目录");
    };

    let mut sections = Vec::new();

    if let Ok(content) = tokio::fs::read_to_string(home.join(".gemini/GEMINI.md")).await
        && !content.trim().is_empty()
    {
        sections.push(RuleSection {
            icon: "🌐",
            title: "Global Rules (~/.gemini/GEMINI.md)".to_string(),
            body: content,
        });
    }

    if let Ok(content) = tokio::fs::read_to_string(home.join("AGENTS.md")).await
        && !content.trim().is_empty()
    {
        sections.push(RuleSection {
            icon: "🌐",
            title: "Home Rules (~/AGENTS.md)".to_string(),
            body: content,
        });
    }

    if let Ok(content) = tokio::fs::read_to_string(home.join(".gemini/config/config.json")).await
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(rules) = val.get("rules").or_else(|| val.get("custom_rules"))
    {
        sections.push(RuleSection {
            icon: "🌐",
            title: "Global Config Rules".to_string(),
            body: rules.to_string(),
        });
    }

    let ws_dir = PathBuf::from(workspace);
    let ws_gemini_file = ws_dir.join("GEMINI.md");
    if let Ok(content) = tokio::fs::read_to_string(&ws_gemini_file).await {
        sections.push(RuleSection {
            icon: "📁",
            title: "Workspace Rules (GEMINI.md)".to_string(),
            body: content,
        });
    }

    let ws_agents_file = ws_dir.join("AGENTS.md");
    if ws_agents_file != home.join("AGENTS.md")
        && let Ok(content) = tokio::fs::read_to_string(&ws_agents_file).await
        && !content.trim().is_empty()
    {
        sections.push(RuleSection {
            icon: "📁",
            title: "Workspace Rules (AGENTS.md)".to_string(),
            body: content,
        });
    }

    let ws_rule_file = ws_dir.join(".gemini/RULE.md");
    if ws_rule_file != ws_gemini_file
        && let Ok(content) = tokio::fs::read_to_string(&ws_rule_file).await
    {
        sections.push(RuleSection {
            icon: "📁",
            title: "Workspace Rules (.gemini/RULE.md)".to_string(),
            body: content,
        });
    }

    let agents_rules_dir = ws_dir.join(".agents/rules");
    if let Ok(mut rd) = tokio::fs::read_dir(&agents_rules_dir).await {
        let mut rule_files = Vec::new();
        while let Ok(Some(entry)) = rd.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") {
                rule_files.push((name, entry.path()));
            }
        }
        rule_files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for (name, path) in rule_files {
            if let Ok(content) = tokio::fs::read_to_string(&path).await
                && !content.trim().is_empty()
            {
                sections.push(RuleSection {
                    icon: "📁",
                    title: format!("Workspace Rule (.agents/rules/{name})"),
                    body: content,
                });
            }
        }
    }

    Ok(sections)
}

async fn read_brain_artifact(conv_id: &str, filename: &str) -> Option<String> {
    if conv_id.is_empty()
        || conv_id.contains('/')
        || conv_id.contains('\\')
        || conv_id == "."
        || conv_id == ".."
    {
        return None;
    }
    for brain_dir in brain_dir_candidates() {
        let artifact_path = brain_dir.join(conv_id).join(filename);
        if let Ok(content) = tokio::fs::read_to_string(&artifact_path).await {
            return Some(content);
        }
    }
    None
}

async fn wait_cancelled(token: &Option<CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn run_prompt_print(
    params: ExecutionParams<'_>,
    stream_tx: mpsc::Sender<StreamEvent>,
    pid_tx: Option<tokio::sync::oneshot::Sender<u32>>,
    idle_timeout_secs: u64,
    max_execution_secs: u64,
    turn_cancel: Option<CancellationToken>,
) -> Result<ExecutionResult> {
    if turn_cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Ok(ExecutionResult {
            output: String::new(),
            conversation_id: normalize_conversation_id(params.conversation_id.map(str::to_string)),
            status: "CANCELED".to_string(),
            usage: None,
            usage_is_cumulative: false,
            system_prompt_sent: false,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            duration_seconds: Some(0.0),
            detached: false,
            finished_reason: Some("canceled".to_string()),
            structured_output: None,
        });
    }

    // Enforce low-RAM budget and evict any warm worker on the same chat/conversation BEFORE spawn.
    let budget_guard =
        persistent::reserve_external_process_budget(params.chat_id, params.conversation_id).await;

    info!(
        chat_id = params.chat_id,
        first_turn = params.is_first_turn,
        prompt_len = params.prompt.len(),
        "Executing agy with one-shot print stream-json format"
    );

    let (final_prompt, sys_prompt_injected) =
        format_effective_prompt_with_flag(params.prompt, params.system_prompt);
    let mut cmd = build_agy_command_with_prompt(&params, final_prompt.as_ref());
    let mut child = cmd.spawn().context("Failed to spawn agy CLI process")?;
    let child_pid = child.id();
    if let Some(pid) = child_pid
        && let Some(tx) = pid_tx
    {
        let _ = tx.send(pid);
    }
    let stdout = child
        .stdout
        .take()
        .context("Failed to capture agy stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("Failed to capture agy stderr")?;

    let tracker = ActivityTracker::new();
    let (turn_done_tx, mut turn_done_rx) = mpsc::channel::<TurnSignal>(4);
    let (detach_tx, detach_rx) = watch::channel(DetachDecision::Pending);

    let mut stdout_task = spawn_stdout_reader(
        stdout,
        tracker.clone(),
        stream_tx.clone(),
        turn_done_tx,
        detach_rx,
    );
    let mut stderr_task = spawn_stderr_reader(stderr, tracker.clone());
    let mut cleanup_guard = ProcessCleanupGuard::new(
        child_pid,
        stdout_task.abort_handle(),
        stderr_task.abort_handle(),
    );

    let idle_timeout = Duration::from_secs(idle_timeout_secs.max(30));
    let max_exec_timeout = Duration::from_secs(max_execution_secs.max(60));
    let probe_interval = Duration::from_secs(2);
    let mut probe_ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + probe_interval, probe_interval);
    probe_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start_time = Instant::now();

    let mut watchdog = WatchdogState::new();
    let mut timeout_reason: Option<TimeoutReason> = None;
    let mut exit_status = None;
    let mut turn_detached_data: Option<TurnDoneData> = None;
    let mut final_result_data: Option<TurnDoneData> = None;
    let mut cancelled = false;

    loop {
        tokio::select! {
            biased;
            () = wait_cancelled(&turn_cancel), if turn_cancel.is_some() => {
                cancelled = true;
                break;
            }
            status_res = child.wait() => {
                exit_status = Some(status_res);
                break;
            }
            Some(signal) = turn_done_rx.recv() => {
                let done_data = match signal {
                    TurnSignal::FinalResult(done_data) => {
                        final_result_data = Some(done_data);
                        break;
                    }
                    TurnSignal::ToolResumed => continue,
                    TurnSignal::Turn1Done(done_data) => done_data,
                };
                let grace_dur = if done_data.has_background_activity {
                    DETACH_GRACE_BACKGROUND
                } else {
                    DETACH_GRACE_IDLE
                };
                info!(
                    pid = ?child_pid,
                    grace_ms = grace_dur.as_millis(),
                    has_background = done_data.has_background_activity,
                    "agent turn completed; waiting for final result or background detachment"
                );
                let grace_deadline = tokio::time::Instant::now() + grace_dur;
                let mut final_result_seen = false;
                let mut resumed_by_tool = false;
                loop {
                    tokio::select! {
                        biased;
                        () = wait_cancelled(&turn_cancel), if turn_cancel.is_some() => {
                            cancelled = true;
                            break;
                        }
                        status_res = child.wait() => {
                            exit_status = Some(status_res);
                            break;
                        }
                        maybe_next = turn_done_rx.recv() => {
                            match maybe_next {
                                Some(TurnSignal::FinalResult(next_done)) => {
                                    final_result_data = Some(next_done);
                                    break;
                                }
                                Some(TurnSignal::ToolResumed) => {
                                    debug!(pid = ?child_pid, "foreground tool started during detach grace; resuming turn");
                                    resumed_by_tool = true;
                                    break;
                                }
                                Some(TurnSignal::Turn1Done(_)) => {}
                                None => {
                                    final_result_seen = true;
                                    break;
                                }
                            }
                        }
                        _ = tokio::time::sleep_until(grace_deadline) => {
                            if done_data.has_background_activity {
                                info!(
                                    pid = ?child_pid,
                                    "active background subagent/task detected; detaching turn response"
                                );
                            } else {
                                info!(
                                    pid = ?child_pid,
                                    "agy exit cleanup exceeded detach grace window; delivering turn response early"
                                );
                            }
                            turn_detached_data = Some(done_data);
                            break;
                        }
                    }
                }
                if cancelled {
                    break;
                }
                if resumed_by_tool {
                    continue;
                }
                if final_result_seen && exit_status.is_none() && final_result_data.is_none() {
                    match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                        Ok(status_res) => exit_status = Some(status_res),
                        Err(_) => {
                            warn!(
                                pid = ?child_pid,
                                "agy process failed to exit within 10s after stdout closed; killing process group"
                            );
                            if let Some(pid) = child_pid {
                                signal_process_tree(pid, libc::SIGKILL);
                            }
                            let _ = child.kill().await;
                        }
                    }
                }
                break;
            }
            _ = probe_ticker.tick() => {
                if start_time.elapsed() >= max_exec_timeout {
                    timeout_reason = Some(TimeoutReason::MaxExecutionExceeded {
                        max_secs: max_exec_timeout.as_secs(),
                    });
                    break;
                }

                let probe_kind = watchdog.plan_probe(&tracker, idle_timeout, Instant::now());
                if probe_kind == WatchdogProbeKind::None {
                    continue;
                }

                let Some(pid) = child_pid else {
                    if probe_kind == WatchdogProbeKind::FullIdleCheck {
                        timeout_reason = Some(TimeoutReason::FrozenDeadlock {
                            idle_secs: tracker.idle_duration().as_secs(),
                            active_processes: Vec::new(),
                        });
                        break;
                    }
                    continue;
                };

                let Ok(inspection) =
                    tokio::task::spawn_blocking(move || inspect_process_tree(pid)).await
                else {
                    warn!(pid, "process tree inspection task failed");
                    continue;
                };

                if let Some(reason) =
                    watchdog.evaluate_inspection(probe_kind, inspection, &tracker, pid)
                {
                    timeout_reason = Some(reason);
                    break;
                }
            }
        }
    }

    if cancelled {
        info!(pid = ?child_pid, "turn_cancel triggered for print agy process; sending SIGINT");
        if let Some(pid) = child_pid {
            signal_process_tree(pid, libc::SIGINT);
        }
        if tokio::time::timeout(CANCEL_SIGINT_GRACE, child.wait())
            .await
            .is_err()
        {
            if let Some(pid) = child_pid {
                signal_process_tree(pid, libc::SIGKILL);
            }
            let _ = child.kill().await;
        }
        stderr_task.abort();
        let _ = detach_tx.send(DetachDecision::Synchronous);
        let conv_after_cancel = normalize_conversation_id(
            await_reader_or_abort(&mut stdout_task, Duration::from_millis(500))
                .await
                .and_then(|res| res.conversation_id),
        );
        cleanup_guard.disarm();
        return Ok(ExecutionResult {
            output: String::new(),
            conversation_id: conv_after_cancel,
            status: "CANCELED".to_string(),
            usage: None,
            usage_is_cumulative: false,
            system_prompt_sent: false,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            duration_seconds: Some(start_time.elapsed().as_secs_f64()),
            detached: false,
            finished_reason: Some("canceled".to_string()),
            structured_output: None,
        });
    }

    if let Some(reason) = timeout_reason {
        warn!(pid = ?child_pid, reason = ?reason, "agy process watchdog triggered; killing process group");
        if let Some(pid) = child_pid {
            signal_process_tree(pid, libc::SIGKILL);
        }
        let _ = child.kill().await;
        stderr_task.abort();
        let _ = detach_tx.send(DetachDecision::Synchronous);

        // Draining stdout briefly preserves the conversation_id agy already emitted before hang.
        let conv_after_timeout = normalize_conversation_id(
            await_reader_or_abort(&mut stdout_task, Duration::from_secs(2))
                .await
                .and_then(|res| res.conversation_id),
        );

        cleanup_guard.disarm();

        let (error_event, summary_msg) = format_timeout_messages(reason);
        let _ = stream_tx.send(StreamEvent::Error(error_event)).await;
        return Ok(ExecutionResult {
            output: summary_msg,
            conversation_id: conv_after_timeout,
            status: "TIMEOUT".to_string(),
            usage: None,
            usage_is_cumulative: false,
            system_prompt_sent: false,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            duration_seconds: None,
            detached: false,
            finished_reason: Some("timeout".to_string()),
            structured_output: None,
        });
    }

    let status = exit_status.and_then(|r| r.ok());

    if let Some(final_data) = final_result_data {
        let conv_id = normalize_conversation_id(final_data.conversation_id);
        let mut final_output = final_data.accumulated_text.trim().to_string();
        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }
        let succeeded = final_data.final_status.eq_ignore_ascii_case("SUCCESS");
        let _ = stream_tx
            .send(StreamEvent::Result(Box::new(StreamResultData {
                conversation_id: conv_id.clone(),
                response: final_output.clone(),
                status: final_data.final_status.clone(),
                usage: final_data.final_usage.clone(),
                context_tokens: final_data.last_context_tokens,
                num_turns: final_data.num_turns,
                saw_checkpoint: final_data.saw_checkpoint,
                duration_seconds: final_data.final_duration,
                finished_reason: final_data.final_finished_reason.clone(),
                structured_output: final_data.final_structured_output.clone(),
            })))
            .await;

        // Tell stdout_task to drop its stream_tx clone immediately so consumers draining to
        // EOF (such as QQ) do not block on language-server shutdown.
        let _ = detach_tx.send(DetachDecision::Synchronous);
        cleanup_guard.disarm();
        spawn_final_result_reaper(child, child_pid, stdout_task, stderr_task, budget_guard);

        return Ok(ExecutionResult {
            output: final_output,
            conversation_id: conv_id,
            status: final_data.final_status,
            usage: final_data.final_usage,
            usage_is_cumulative: true,
            system_prompt_sent: sys_prompt_injected && succeeded,
            context_tokens: final_data.last_context_tokens,
            num_turns: final_data.num_turns,
            saw_checkpoint: final_data.saw_checkpoint,
            duration_seconds: final_data.final_duration,
            detached: false,
            finished_reason: final_data.final_finished_reason,
            structured_output: final_data.final_structured_output,
        });
    }

    if let Some(detached) = turn_detached_data {
        let conv_id = normalize_conversation_id(detached.conversation_id);
        let mut final_output = detached.accumulated_text.trim().to_string();
        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }
        let succeeded = detached.final_status.eq_ignore_ascii_case("SUCCESS");
        // Send turn-1 Result BEFORE switching detach_tx so follow-up events stay ordered after Result.
        let _ = stream_tx
            .send(StreamEvent::Result(Box::new(StreamResultData {
                conversation_id: conv_id.clone(),
                response: final_output.clone(),
                status: detached.final_status.clone(),
                usage: detached.final_usage.clone(),
                context_tokens: detached.last_context_tokens,
                num_turns: detached.num_turns,
                saw_checkpoint: detached.saw_checkpoint,
                duration_seconds: detached.final_duration,
                finished_reason: detached.final_finished_reason.clone(),
                structured_output: detached.final_structured_output.clone(),
            })))
            .await;

        let _ = detach_tx.send(DetachDecision::Detached);
        let remaining_timeout = max_exec_timeout
            .saturating_sub(start_time.elapsed())
            .max(Duration::from_secs(60));
        cleanup_guard.disarm();
        spawn_background_reaper(
            child,
            child_pid,
            stdout_task,
            stderr_task,
            remaining_timeout,
            "detached",
            budget_guard,
        );

        return Ok(ExecutionResult {
            output: final_output,
            conversation_id: conv_id,
            status: detached.final_status,
            usage: detached.final_usage,
            usage_is_cumulative: false,
            system_prompt_sent: sys_prompt_injected && succeeded,
            context_tokens: detached.last_context_tokens,
            num_turns: detached.num_turns,
            saw_checkpoint: detached.saw_checkpoint,
            duration_seconds: detached.final_duration,
            detached: true,
            finished_reason: detached.final_finished_reason,
            structured_output: detached.final_structured_output,
        });
    }

    let _ = detach_tx.send(DetachDecision::Synchronous);
    let mut stdout_res = await_reader_or_abort(&mut stdout_task, Duration::from_secs(5))
        .await
        .unwrap_or_default();
    let stderr_text = await_reader_or_abort(&mut stderr_task, Duration::from_secs(2))
        .await
        .unwrap_or_default();
    cleanup_guard.disarm();

    let exit_failed = status.is_none_or(|s| !s.success());
    if exit_failed {
        stdout_res.final_status = "ERROR".to_string();
        if stdout_res.final_finished_reason.is_none() {
            stdout_res.final_finished_reason = Some("process_exit_error".to_string());
        }
        warn!(
            pid = ?child_pid,
            exit_code = ?status.and_then(|s| s.code()),
            stderr = %stderr_text.trim(),
            output_empty = stdout_res.accumulated_text.trim().is_empty(),
            "agy exited non-zero or terminated unexpectedly"
        );
    }

    let conv_id = normalize_conversation_id(stdout_res.conversation_id);
    let mut final_output = stdout_res.accumulated_text.trim().to_string();
    if final_output.is_empty() && !stderr_text.trim().is_empty() {
        final_output = format!("Error:\n{}", stderr_text.trim());
    }

    if final_output.is_empty() {
        final_output = if exit_failed {
            "❌ agy exited unexpectedly with no output.".to_string()
        } else {
            "Task completed with no text output.".to_string()
        };
    }

    let succeeded = stdout_res.final_status.eq_ignore_ascii_case("SUCCESS");

    let _ = stream_tx
        .send(StreamEvent::Result(Box::new(StreamResultData {
            conversation_id: conv_id.clone(),
            response: final_output.clone(),
            status: stdout_res.final_status.clone(),
            usage: stdout_res.final_usage.clone(),
            context_tokens: stdout_res.last_context_tokens,
            num_turns: stdout_res.num_turns,
            saw_checkpoint: stdout_res.saw_checkpoint,
            duration_seconds: stdout_res.final_duration,
            finished_reason: stdout_res.final_finished_reason.clone(),
            structured_output: stdout_res.final_structured_output.clone(),
        })))
        .await;

    Ok(ExecutionResult {
        output: final_output,
        conversation_id: conv_id,
        status: stdout_res.final_status,
        usage: stdout_res.final_usage,
        usage_is_cumulative: true,
        system_prompt_sent: sys_prompt_injected && succeeded,
        context_tokens: stdout_res.last_context_tokens,
        num_turns: stdout_res.num_turns,
        saw_checkpoint: stdout_res.saw_checkpoint,
        duration_seconds: stdout_res.final_duration,
        detached: false,
        finished_reason: stdout_res.final_finished_reason,
        structured_output: stdout_res.final_structured_output,
    })
}

impl AgyExecutor {
    /// Executes agy using the configured backend (`Persistent`, `Print`, or `Acp`) and streams
    /// structured [`StreamEvent`] items line-by-line.
    #[tracing::instrument(
        skip_all,
        fields(
            chat_id = params.chat_id,
            backend = params.backend.as_str(),
            first_turn = params.is_first_turn,
            prompt_len = params.prompt.len(),
            has_conv_id = params.conversation_id.is_some()
        )
    )]
    pub async fn run_prompt_streaming(
        params: ExecutionParams<'_>,
        stream_tx: mpsc::Sender<StreamEvent>,
        mut pid_tx: Option<tokio::sync::oneshot::Sender<u32>>,
        idle_timeout_secs: u64,
        max_execution_secs: u64,
        turn_cancel: Option<CancellationToken>,
    ) -> Result<ExecutionResult> {
        match params.backend {
            ExecutorBackend::Acp => {
                if will_use_acp(
                    params.backend,
                    params.json_schema.is_some(),
                    params.remote_agent,
                ) {
                    info!(
                        chat_id = params.chat_id,
                        first_turn = params.is_first_turn,
                        prompt_len = params.prompt.len(),
                        "Executing agy via ACP stdio backend"
                    );
                    match Box::pin(acp::run_prompt_acp(
                        &params,
                        stream_tx.clone(),
                        idle_timeout_secs,
                        max_execution_secs,
                        turn_cancel.clone(),
                    ))
                    .await
                    {
                        Ok(result) => return Ok(result),
                        Err(acp::AcpError::HandshakeOrConnect(err)) => {
                            warn!(
                                error = %err,
                                "ACP connect/handshake failed; safely falling back to stream-json print mode"
                            );
                        }
                        Err(acp::AcpError::ExecutionBeforeOutput(err)) => {
                            warn!(
                                error = %err,
                                "ACP turn failed before streaming output; safely falling back to stream-json print mode"
                            );
                        }
                        Err(acp::AcpError::Execution(err)) => return Err(err),
                    }
                }
            }
            ExecutorBackend::Persistent => {
                let must_use_print = params.json_schema.is_some()
                    || is_persistent_incompatible_slash_command(params.prompt);
                if must_use_print {
                    debug!(
                        chat_id = params.chat_id,
                        has_schema = params.json_schema.is_some(),
                        "Routing persistent-incompatible turn to one-shot print backend"
                    );
                    evict_chat_worker(params.chat_id).await;
                } else {
                    match Box::pin(persistent::run_prompt_persistent(
                        &params,
                        stream_tx.clone(),
                        &mut pid_tx,
                        idle_timeout_secs,
                        max_execution_secs,
                        turn_cancel.clone(),
                    ))
                    .await
                    {
                        Ok(result) => return Ok(result),
                        Err(persistent::PersistentRunError::SpawnOrInitFailed(err)) => {
                            warn!(
                                chat_id = params.chat_id,
                                error = %err,
                                "Persistent stream-json spawn/init failed; safely falling back to one-shot print mode"
                            );
                        }
                    }
                }
            }
            ExecutorBackend::Print => {}
        }

        Box::pin(run_prompt_print(
            params,
            stream_tx,
            pid_tx,
            idle_timeout_secs,
            max_execution_secs,
            turn_cancel,
        ))
        .await
    }

    /// Generic helper to execute an agy CLI subcommand with timeout, exit-status check, and ANSI stripping.
    async fn run_cli_command(args: &[&str]) -> Result<String> {
        let label = format!("agy {}", args.join(" "));
        let out =
            exec_command_with_timeout(get_agy_path(), args, CLI_SUBCOMMAND_TIMEOUT, &label).await?;

        if !out.success {
            let combined = combine_cli_output(&out.stdout, &out.stderr);
            let clean_err = strip_ansi_codes(&combined);
            anyhow::bail!(
                "{} failed (status {:?}): {}",
                label,
                out.status_code,
                clean_err.trim()
            );
        }

        let clean = strip_ansi_codes(&out.stdout);
        Ok(clean.trim().to_string())
    }

    async fn run_agy_subcommand(subcommand: &str, args: &[&str]) -> Result<String> {
        let mut full_args = Vec::with_capacity(1 + args.len());
        full_args.push(subcommand);
        full_args.extend_from_slice(args);
        let label = format!("agy {}", full_args.join(" "));

        let out =
            exec_command_with_timeout(get_agy_path(), &full_args, CLI_SUBCOMMAND_TIMEOUT, &label)
                .await?;

        let combined = combine_cli_output(&out.stdout, &out.stderr);
        let clean = strip_ansi_codes(&combined);
        let trimmed = clean.trim().to_string();

        if !out.success {
            anyhow::bail!(
                "{} failed (status {:?}): {}",
                label,
                out.status_code,
                trimmed
            );
        }

        Ok(trimmed)
    }

    pub async fn list_models() -> Result<String> {
        let _cli_guard = crate::quota::metadata_cli_lock().lock().await;
        let clean = Self::run_cli_command(&["models"]).await?;
        if clean.is_empty() {
            Ok(crate::models::SUPPORTED_MODELS
                .iter()
                .map(|(id, label)| format!("{id}\t{label}"))
                .collect::<Vec<_>>()
                .join("\n"))
        } else {
            Ok(clean)
        }
    }

    fn parse_and_cache_agents(raw: &str) -> Vec<String> {
        let parsed: Vec<String> = raw
            .lines()
            .filter_map(|line| {
                let name = line.split_whitespace().next()?.trim();
                crate::models::is_safe_free_value(name).then(|| name.to_string())
            })
            .collect();
        let lock = AGENTS_CACHE.get_or_init(|| StdRwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some((Instant::now(), parsed.clone()));
        }
        parsed
    }

    pub async fn list_agents() -> Result<String> {
        let _cli_guard = crate::quota::metadata_cli_lock().lock().await;
        let clean = Self::run_cli_command(&["agent"]).await?;
        let _ = Self::parse_and_cache_agents(&clean);
        if clean.is_empty() {
            Ok("Default agent".to_string())
        } else {
            Ok(clean)
        }
    }

    /// Parses custom agent names returned by `agy agent` (empty when only the built-in default exists).
    pub async fn list_agents_parsed() -> Vec<String> {
        let _cli_guard = crate::quota::metadata_cli_lock().lock().await;
        let Ok(raw) = Self::run_cli_command(&["agent"]).await else {
            return Vec::new();
        };
        Self::parse_and_cache_agents(&raw)
    }

    /// Instant non-blocking snapshot of parsed custom agents (`0 ms` subprocess wait) using
    /// stale-while-revalidate caching so Workspace UI cards respond instantaneously.
    #[must_use]
    pub fn list_agents_parsed_cached() -> Vec<String> {
        let lock = AGENTS_CACHE.get_or_init(|| StdRwLock::new(None));
        let (cached, needs_refresh) = match lock.read() {
            Ok(guard) => match &*guard {
                Some((ts, list)) => (list.clone(), ts.elapsed() >= AGENTS_CACHE_TTL),
                None => (Vec::new(), true),
            },
            Err(poisoned) => match &*poisoned.into_inner() {
                Some((ts, list)) => (list.clone(), ts.elapsed() >= AGENTS_CACHE_TTL),
                None => (Vec::new(), true),
            },
        };
        if needs_refresh
            && AGENTS_BG_IN_FLIGHT
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _reset = AgentsBgFlagReset;
                    let _ = Self::list_agents_parsed().await;
                });
            } else {
                AGENTS_BG_IN_FLIGHT.store(false, Ordering::Release);
            }
        }
        cached
    }

    pub async fn list_plugins() -> Result<String> {
        let clean = Self::run_cli_command(&["plugin", "list"]).await?;
        if clean.is_empty() {
            Ok("No active plugins installed.".to_string())
        } else {
            Ok(clean)
        }
    }

    pub async fn get_version() -> String {
        let _cli_guard = crate::quota::metadata_cli_lock().lock().await;
        if let Ok(out) = exec_command_with_timeout(
            get_agy_path(),
            &["--version"],
            Duration::from_secs(10),
            "agy --version",
        )
        .await
            && out.success
        {
            let version = out.stdout.trim();
            if !version.is_empty() {
                return version.to_string();
            }
        }
        "unknown".to_string()
    }

    pub async fn show_changelog() -> Result<String> {
        let clean = Self::run_cli_command(&["changelog"]).await?;
        if clean.is_empty() {
            let ver = Self::get_version().await;
            Ok(format!(
                "Antigravity CLI v{ver} - Features structured output (--output-format stream-json/json), JSON Schema enforcement, TUI settings, and performance enhancements."
            ))
        } else {
            Ok(clean)
        }
    }

    /// Returns all existing brain directories (modern CLI brain + legacy fallback) using
    /// non-blocking `tokio::fs`.
    pub async fn get_brain_dirs_async() -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(2);
        for candidate in brain_dir_candidates() {
            if tokio::fs::try_exists(candidate).await.unwrap_or(false) {
                out.push(candidate.clone());
            }
        }
        out
    }

    /// Lists recent conversations, preferring `~/.gemini/antigravity-cli/cache/conversation_metadata.json`
    /// (which filters out `is_internal` subagent/swarm sessions and provides AI-generated titles)
    /// and falling back to scanning brain directories when the cache is absent or unreadable.
    pub async fn list_recent_conversations(limit: usize) -> Vec<RecentSessionInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let brain_dirs = brain_dir_candidates();

        if let Some(home) = dirs::home_dir() {
            let meta_path = home.join(".gemini/antigravity-cli/cache/conversation_metadata.json");
            if let Some(from_meta) =
                load_recent_conversations_from_metadata(&meta_path, brain_dirs, limit).await
            {
                return from_meta;
            }
        }

        if brain_dirs.is_empty() {
            return Vec::new();
        }

        let mut candidates: Vec<(SystemTime, String, PathBuf)> = Vec::new();
        let mut seen_ids: FxHashSet<String> = FxHashSet::default();

        for brain_dir in brain_dirs {
            let Ok(mut entries) = tokio::fs::read_dir(&brain_dir).await else {
                continue;
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                let id = entry.file_name().to_string_lossy().into_owned();
                if id.starts_with('.') || id.len() < 8 {
                    continue;
                }
                let Some(meta) = entry_dir_metadata(&entry).await else {
                    continue;
                };
                if !seen_ids.insert(id.clone()) {
                    continue;
                }
                let modified_time = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                candidates.push((modified_time, id, entry.path()));
            }
        }

        // Sort and truncate BEFORE opening any transcript or artifact files.
        candidates.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        candidates.truncate(limit);

        let mut sessions = Vec::with_capacity(candidates.len());
        for (modified_time, id, path) in candidates {
            let updated_time = if modified_time != SystemTime::UNIX_EPOCH {
                let datetime: chrono::DateTime<chrono::Local> = modified_time.into();
                datetime.format("%m-%d %H:%M").to_string()
            } else {
                String::new()
            };

            let plan_path = path.join("implementation_plan.md");
            let wt_path = path.join("walkthrough.md");
            let transcript_path = path.join(".system_generated/logs/transcript.jsonl");

            let (plan_res, wt_res, title_opt) = tokio::join!(
                tokio::fs::try_exists(&plan_path),
                tokio::fs::try_exists(&wt_path),
                extract_transcript_title(&id, &transcript_path),
            );

            sessions.push(RecentSessionInfo {
                id,
                title: title_opt.unwrap_or_else(|| "New Conversation".to_string()),
                updated_time,
                has_plan: plan_res.unwrap_or(false),
                has_walkthrough: wt_res.unwrap_or(false),
            });
        }

        sessions
    }

    /// Read implementation plan from brain for a given conversation ID.
    pub async fn read_implementation_plan(conv_id: &str) -> Option<String> {
        read_brain_artifact(conv_id, "implementation_plan.md").await
    }

    /// Read walkthrough from brain for a given conversation ID.
    pub async fn read_walkthrough(conv_id: &str) -> Option<String> {
        read_brain_artifact(conv_id, "walkthrough.md").await
    }

    /// Scan and list all active Antigravity skill sections.
    pub(crate) async fn list_skill_sections()
    -> Result<Vec<(&'static str, Vec<(String, String)>)>, &'static str> {
        collect_skill_sections().await
    }

    /// Returns a sorted, deduplicated list of slash-invocable skill names (lowercased) across
    /// workspace, user, plugin, and builtin skill directories (cached for 120s).
    pub async fn list_known_skill_names() -> Vec<String> {
        let cache = SKILL_NAMES_CACHE.get_or_init(|| StdRwLock::new(None));
        if let Ok(guard) = cache.read()
            && let Some((ts, ref cached)) = *guard
            && ts.elapsed() < SKILL_NAMES_CACHE_TTL
        {
            return cached.clone();
        }
        let Ok(sections) = collect_skill_sections().await else {
            return Vec::new();
        };
        let mut names: Vec<String> = sections
            .into_iter()
            .flat_map(|(_, items)| {
                items
                    .into_iter()
                    .map(|(name, _)| name.trim().to_ascii_lowercase())
            })
            .filter(|name| !name.is_empty())
            .collect();
        names.sort_unstable();
        names.dedup();
        if let Ok(mut guard) = cache.write() {
            *guard = Some((Instant::now(), names.clone()));
        }
        names
    }

    /// Scan and list all active Antigravity skills as plain text without HTML markup.
    pub async fn list_skills_plain() -> String {
        let sections = match collect_skill_sections().await {
            Ok(s) => s,
            Err(msg) => return msg.to_string(),
        };

        let mut result = String::new();
        for (section, section_skills) in sections {
            let _ = writeln!(result, "📦 {section}:");
            for (name, desc) in section_skills {
                if desc.is_empty() {
                    let _ = writeln!(result, "• /{name}");
                } else {
                    let short_desc = truncate_chars(&desc, 50);
                    let _ = writeln!(result, "• /{name} : {short_desc}");
                }
            }
            result.push('\n');
        }

        if result.is_empty() {
            "未发现已安装的 Antigravity 技能。".to_string()
        } else {
            result.trim_end().to_string()
        }
    }

    /// Read global and workspace rule sections.
    pub(crate) async fn list_rule_sections(
        workspace: &str,
    ) -> Result<Vec<RuleSection>, &'static str> {
        collect_rule_sections(workspace).await
    }

    /// Read global and workspace rules as plain text without HTML markup.
    pub async fn list_rules_plain(workspace: &str) -> String {
        let sections = match collect_rule_sections(workspace).await {
            Ok(s) => s,
            Err(msg) => return msg.to_string(),
        };

        let mut output = String::new();
        for sec in sections {
            let _ = writeln!(output, "{} {}:", sec.icon, sec.title);
            let _ = writeln!(output, "{}\n", sec.body.trim_end());
        }

        if output.is_empty() {
            "📋 当前暂无活跃的自定义规则文件。\n可在项目根目录下创建 GEMINI.md 添加项目规则。"
                .to_string()
        } else {
            output.trim_end().to_string()
        }
    }

    /// Run `agy update` to upgrade CLI (with passwordless sudo fallback if permission denied).
    pub async fn run_update() -> Result<String> {
        let agy_bin = get_agy_path();
        let out = exec_command_with_timeout(agy_bin, &["update"], CLI_UPDATE_TIMEOUT, "agy update")
            .await?;
        let combined = combine_cli_output(&out.stdout, &out.stderr);

        if update_needs_elevation(&combined, out.success) {
            let agy_str = agy_bin.to_string_lossy();
            if let Ok(sudo_out) = exec_command_with_timeout(
                "sudo",
                &["-n", agy_str.as_ref(), "update"],
                CLI_UPDATE_TIMEOUT,
                "sudo -n agy update",
            )
            .await
            {
                let sudo_combined = combine_cli_output(&sudo_out.stdout, &sudo_out.stderr);
                if sudo_out.success && !sudo_combined.is_empty() {
                    return Ok(sudo_combined);
                }
                if !sudo_out.success {
                    let detail = if sudo_combined.is_empty() {
                        combined.clone()
                    } else {
                        sudo_combined
                    };
                    anyhow::bail!(
                        "agy update elevation failed (status {:?}): {}",
                        sudo_out.status_code,
                        detail
                    );
                }
            }
        }

        if !out.success {
            anyhow::bail!(
                "agy update failed (status {:?}): {}",
                out.status_code,
                combined
            );
        }

        Ok(combined)
    }

    /// Run `agy mcp` subcommands.
    pub async fn run_mcp_cmd(args: &[&str]) -> Result<String> {
        Self::run_agy_subcommand("mcp", args).await
    }

    /// Run `agy plugin` subcommands.
    pub async fn run_plugin_cmd(args: &[&str]) -> Result<String> {
        Self::run_agy_subcommand("plugin", args).await
    }
}

#[derive(Deserialize)]
struct ConversationMetadataFile {
    #[serde(default)]
    conversations: FxHashMap<String, ConversationMetadataEntry>,
}

#[derive(Deserialize)]
struct ConversationMetadataEntry {
    #[serde(default)]
    is_internal: bool,
    #[serde(default)]
    last_modified_time: Option<String>,
    #[serde(default)]
    summary: Option<ConversationSummaryMeta>,
}

#[derive(Deserialize)]
struct ConversationSummaryMeta {
    #[serde(rename = "Title", default)]
    title: Option<String>,
    #[serde(rename = "Preview", default)]
    preview: Option<String>,
    #[serde(rename = "NumSteps", default)]
    #[allow(dead_code)]
    num_steps: Option<u64>,
    #[serde(rename = "UpdatedAt", default)]
    updated_at: Option<String>,
    #[serde(rename = "Internal", default)]
    internal: bool,
}

async fn has_brain_file(brain_dirs: &[PathBuf], conv_id: &str, filename: &str) -> bool {
    for dir in brain_dirs {
        if tokio::fs::try_exists(dir.join(conv_id).join(filename))
            .await
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

async fn load_recent_conversations_from_metadata(
    meta_path: &Path,
    brain_dirs: &[PathBuf],
    limit: usize,
) -> Option<Vec<RecentSessionInfo>> {
    let meta = tokio::fs::metadata(meta_path).await.ok()?;
    let mtime = meta.modified().ok();
    let len = meta.len();

    let cache = METADATA_FILE_CACHE.get_or_init(|| StdRwLock::new(None));
    let cached_entries = cache.read().ok().and_then(|guard| {
        guard.as_ref().and_then(|c| {
            (c.path == meta_path && c.mtime == mtime && c.len == len)
                .then(|| c.entries.iter().take(limit).cloned().collect::<Vec<_>>())
        })
    });

    let entries = if let Some(hit) = cached_entries {
        hit
    } else {
        let raw = tokio::fs::read_to_string(meta_path).await.ok()?;
        let parsed: ConversationMetadataFile = serde_json::from_str(&raw).ok()?;
        if parsed.conversations.is_empty() {
            return None;
        }

        let mut all_entries: ParsedMetadataEntries = Vec::new();
        for (id, entry) in parsed.conversations {
            if id.starts_with('.') || id.len() < 8 {
                continue;
            }
            if entry.is_internal || entry.summary.as_ref().is_some_and(|s| s.internal) {
                continue;
            }

            let ts_str = entry
                .last_modified_time
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    entry
                        .summary
                        .as_ref()
                        .and_then(|s| s.updated_at.as_deref())
                        .filter(|s| !s.trim().is_empty())
                });
            let parsed_dt = ts_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH);

            let title = entry
                .summary
                .as_ref()
                .and_then(|s| s.title.as_deref())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| truncate_chars(t, 40).into_owned())
                .or_else(|| {
                    entry
                        .summary
                        .as_ref()
                        .and_then(|s| s.preview.as_deref())
                        .map(clean_user_prompt_summary)
                        .filter(|p| !p.is_empty())
                })
                .unwrap_or_else(|| "New Conversation".to_string());

            all_entries.push((parsed_dt, id, title));
        }

        all_entries.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let top: ParsedMetadataEntries = all_entries.iter().take(limit).cloned().collect();
        if let Ok(mut guard) = cache.write() {
            *guard = Some(MetadataFileCache {
                path: meta_path.to_path_buf(),
                mtime,
                len,
                entries: all_entries,
            });
        }
        top
    };

    let mut sessions = Vec::with_capacity(entries.len());
    for (dt, id, title) in entries {
        let updated_time = if dt != chrono::DateTime::<chrono::Utc>::UNIX_EPOCH {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        } else {
            String::new()
        };
        let (has_plan, has_walkthrough) = tokio::join!(
            has_brain_file(brain_dirs, &id, "implementation_plan.md"),
            has_brain_file(brain_dirs, &id, "walkthrough.md"),
        );
        sessions.push(RecentSessionInfo {
            id,
            title,
            updated_time,
            has_plan,
            has_walkthrough,
        });
    }

    Some(sessions)
}

#[derive(Debug, Clone)]
pub struct RecentSessionInfo {
    pub id: String,
    pub title: String,
    pub updated_time: String,
    pub has_plan: bool,
    pub has_walkthrough: bool,
}

/// Strips one or more leading `[System Instructions: ...]` blocks (handling nested `[...]` such as
/// `[[send_file:...]]`) so conversation titles show the user's real request.
fn strip_leading_system_instructions(input: &str) -> &str {
    const PREFIX: &str = "[System Instructions:";
    let mut rest = input.trim_start();
    while rest.starts_with(PREFIX) {
        let mut depth = 0usize;
        let mut close_idx = None;
        for (idx, b) in rest.bytes().enumerate() {
            match b {
                b'[' => depth += 1,
                b']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close_idx = Some(idx);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = close_idx else {
            // Truncated preview containing only an unclosed `[System Instructions:` prefix.
            return "";
        };
        rest = rest[end + 1..].trim_start();
    }
    rest
}

#[must_use]
fn clean_user_prompt_summary(raw: &str) -> String {
    const OPEN_TAG: &str = "<USER_REQUEST>";
    let mut clean = raw;
    if let Some(start) = raw.find(OPEN_TAG) {
        // Search for the closing tag only after the opening one: a closing tag occurring
        // earlier would make the slice range invert and panic the daemon (panic = "abort").
        let body = &raw[start + OPEN_TAG.len()..];
        clean = match body.find("</USER_REQUEST>") {
            Some(end) => &body[..end],
            None => body,
        };
    }
    clean = strip_leading_system_instructions(clean);
    let single_line = clean
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate_chars(single_line, 40).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        DETACH_GRACE_BACKGROUND, DETACH_GRACE_IDLE, DetachDecision, ExecutionParams, StreamEvent,
        StreamParserState, TimeoutReason, TurnDoneData, TurnSignal, build_agy_command,
        build_session_settings_json, clean_user_prompt_summary, combine_cli_output,
        floor_char_boundary, format_effective_prompt, format_effective_prompt_with_flag,
        format_timeout_messages, is_persistent_incompatible_slash_command,
        load_recent_conversations_from_metadata, read_skill_metadata, resolve_extra_dirs,
        scan_plugin_skills_root, scan_skill_dir, update_needs_elevation, will_use_acp,
    };
    use crate::config::ExecutorBackend;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn unwrap_turn1(sig: &TurnSignal) -> &TurnDoneData {
        match sig {
            TurnSignal::Turn1Done(d) => d,
            other => panic!("expected Turn1Done, got {other:?}"),
        }
    }

    fn unwrap_final(sig: &TurnSignal) -> &TurnDoneData {
        match sig {
            TurnSignal::FinalResult(d) => d,
            other => panic!("expected FinalResult, got {other:?}"),
        }
    }

    #[test]
    fn closing_tag_before_opening_tag_does_not_panic() {
        let raw = "</USER_REQUEST> stale preamble <USER_REQUEST> real ask";
        assert_eq!(clean_user_prompt_summary(raw), "real ask");
    }

    #[test]
    fn extracts_wrapped_request_and_skips_blank_lines() {
        let raw = "<USER_REQUEST>\n\n   重构一下这个模块\n</USER_REQUEST>";
        assert_eq!(clean_user_prompt_summary(raw), "重构一下这个模块");
    }

    #[test]
    fn unclosed_wrapper_falls_back_to_body() {
        assert_eq!(
            clean_user_prompt_summary("<USER_REQUEST> no closer here"),
            "no closer here"
        );
        assert_eq!(clean_user_prompt_summary("<USER_REQUEST>"), "");
    }

    #[test]
    fn long_cjk_summary_truncates_on_char_boundary() {
        let raw = format!("<USER_REQUEST>{}</USER_REQUEST>", "中".repeat(60));
        let out = clean_user_prompt_summary(&raw);
        assert!(out.ends_with("..."));
        assert_eq!(out.chars().count(), 43);
    }

    #[test]
    fn plain_prompt_without_wrapper_is_untouched_when_short() {
        assert_eq!(clean_user_prompt_summary("say 1"), "say 1");
    }

    #[test]
    fn extra_dirs_expand_tilde_and_drop_duplicates() {
        let home = dirs::home_dir().expect("home dir");
        let home_str = home.to_string_lossy().into_owned();
        let workspace = home.join("agygram").to_string_lossy().into_owned();
        let extra = vec![
            "~".to_string(),
            home_str.clone(),
            "~/agygram".to_string(),
            "/tmp".to_string(),
            "/tmp".to_string(),
        ];
        let resolved = resolve_extra_dirs(&workspace, &extra);
        assert_eq!(resolved, vec![home_str, "/tmp".to_string()]);
    }

    #[test]
    fn update_elevation_triggers_on_new_access_error() {
        let access_err = "Error: directory /usr/local/bin is not fully accessible (readable: true, writable: false)";
        // Newer agy may exit 0 yet skip the update, so keywords must trigger on their own.
        assert!(update_needs_elevation(access_err, true));
        assert!(update_needs_elevation(access_err, false));
        assert!(update_needs_elevation("Permission denied", true));
        assert!(update_needs_elevation("os error 13", true));
        assert!(update_needs_elevation("network timeout", false));
        assert!(!update_needs_elevation("Already up to date", true));
    }

    #[test]
    fn stream_parser_synchronous_turn_followed_by_result_does_not_block() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut turns: Vec<TurnSignal> = Vec::new();

        state.process_line(
            r#"{"event":"init","conversation_id":"conv-1","init":{"ignored":true}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"text_delta":"Hello "}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":"world"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        assert_eq!(turns.len(), 1);
        let t1 = unwrap_turn1(&turns[0]);
        assert!(!t1.is_final_result);
        assert_eq!(t1.accumulated_text, "Hello world");

        // An intermediate step update after turn-1 completion must NOT drop turn-1 text or block.
        state.process_line(
            r#"{"event":"step_update","step_update":{"usage":{"total_tokens":42}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        state.process_line(
            r#"{"event":"result","result":{"status":"SUCCESS","response":"Hello world"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        assert_eq!(turns.len(), 2);
        let fin = unwrap_final(&turns[1]);
        assert!(fin.is_final_result);
        assert_eq!(fin.accumulated_text, "Hello world");
        assert_eq!(fin.final_usage.as_ref().map(|u| u.total_tokens), Some(42));

        let (keep_open, flushed) = state.apply_decision(DetachDecision::Synchronous);
        assert!(!keep_open);
        assert!(flushed.is_empty());
    }

    #[test]
    fn stream_parser_buffers_pending_post_turn1_events_and_flushes_on_detach() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut turns = Vec::new();

        state.process_line(
            r#"{"event":"step_update","step_update":{"text_delta":"Turn 1"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        assert_eq!(turns.len(), 1);
        assert_eq!(unwrap_turn1(&turns[0]).accumulated_text, "Turn 1");
        events.clear();

        // Events arriving during the 1.5s grace window stay buffered so they follow Turn 1 Result.
        state.process_line(
            r#"{"event":"step_update","step_update":{"text_delta":"Turn 2"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        assert!(events.is_empty());

        // Coordinator enters detached mode: buffered turn-2 events flush, turn-1 text stays separate.
        let (keep_open, flushed) = state.apply_decision(DetachDecision::Detached);
        assert!(keep_open);
        assert_eq!(flushed, vec![StreamEvent::TextDelta("Turn 2".to_string())]);

        state.process_line(
            r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE"}}"#,
            DetachDecision::Detached,
            &mut events,
            &mut turns,
        );
        assert_eq!(
            events,
            vec![StreamEvent::TurnComplete {
                text: "Turn 2".to_string()
            }]
        );
        // Turn-2 completion in detached mode must not emit another TurnSignal.
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn stream_parser_tracks_active_tools_and_resumes_turn_instead_of_premature_detach() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();

        // 1. Model outputs preamble text and completes step 1.
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","state":"DONE","text_delta":"Checking files..."}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(signals.len(), 1);
        assert!(matches!(signals[0], TurnSignal::Turn1Done(_)));
        signals.clear();
        events.clear();

        // 2. Foreground tool starts during the 1.5s grace window -> must emit ToolResumed & flush!
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ACTIVE","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"}}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(signals, vec![TurnSignal::ToolResumed]);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::ToolUse { .. }));
        signals.clear();
        events.clear();

        // 3. Tool finishes (DONE) -> must NOT emit duplicate ToolUse for same step_index!
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"DONE","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"echo hi"},"output":"hi"}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert!(
            events.is_empty(),
            "DONE step for seen tool must not duplicate ToolUse"
        );
        assert!(signals.is_empty());

        // 4. Final answer arrives and completes -> Turn1Done emits combined text!
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"step_type":"agent_response","state":"DONE","text_delta":"All good."}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(signals.len(), 1);
        let done = unwrap_turn1(&signals[0]);
        assert_eq!(done.accumulated_text, "Checking files...\n\nAll good.");
    }

    #[test]
    fn stream_parser_surfaces_result_error_when_response_empty() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();

        state.process_line(
            r#"{"event":"result","result":{"status":"ERROR","response":"","error":"invalid model selection"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(signals.len(), 1);
        let fin = unwrap_final(&signals[0]);
        assert_eq!(fin.final_status, "ERROR");
        assert!(fin.accumulated_text.contains("invalid model selection"));
    }

    #[test]
    fn build_agy_command_enforces_effort_compatibility_and_safe_flags() {
        assert_eq!(DETACH_GRACE_BACKGROUND, Duration::from_millis(1500));
        assert_eq!(DETACH_GRACE_IDLE, Duration::from_millis(5000));

        let make_cmd = |model: &str, effort: &str| {
            let params = ExecutionParams {
                chat_id: 1,
                prompt: "hello",
                conversation_id: None,
                is_first_turn: true,
                workspace: "/tmp",
                extra_dirs: &[],
                model,
                effort,
                mode: "accept-edits",
                agent: "",
                system_prompt: "",
                sandbox: false,
                project: "",
                settings_json: None,
                json_schema: None,
                remote_agent: false,
                backend: ExecutorBackend::Print,
            };
            build_agy_command(&params)
        };
        let make_args = |model: &str, effort: &str| -> Vec<String> {
            make_cmd(model, effort)
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };

        // Direct invocation of agy (no ineffective `stdbuf` wrapper on Go binary)
        let cmd = make_cmd("", "high");
        let prog = cmd.as_std().get_program().to_string_lossy();
        assert!(
            !prog.contains("stdbuf"),
            "agy must be invoked directly without stdbuf, got program={prog}"
        );

        let orch = make_args("gemini-pro-orchestrator", "high");
        assert!(orch.contains(&"--model".to_string()));
        assert!(!orch.contains(&"--effort".to_string()));

        let orch_argon = make_args("gemini-pro-orchestrator-argon", "high");
        assert!(!orch_argon.contains(&"--effort".to_string()));

        let claude = make_args("claude-sonnet-4-6", "high");
        assert!(!claude.contains(&"--effort".to_string()));

        let carbon_high = make_args("gemini-4-carbon-high", "low");
        assert!(!carbon_high.contains(&"--effort".to_string()));

        // Default model (Gemini 4 Neon) natively supports xhigh; only max clamps to xhigh.
        let default_xhigh = make_args("", "xhigh");
        let idx = default_xhigh.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(default_xhigh[idx + 1], "xhigh");

        let default_max = make_args("", "max");
        let max_idx = default_max.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(default_max[max_idx + 1], "xhigh");
        assert!(
            !default_xhigh.contains(&"--add-dir".to_string()),
            "primary workspace is mounted via current_dir and must not be duplicated via --add-dir"
        );
    }

    #[test]
    fn build_agy_command_preserves_leading_slash_commands_and_wave3_flags() {
        assert_eq!(
            format_effective_prompt("/compact", "system guide"),
            "/compact"
        );
        assert_eq!(
            format_effective_prompt("  /compact focus on auth", "system guide"),
            "/compact focus on auth"
        );
        let swarm_prompt = format_effective_prompt("/swarm refactor executor", "be concise");
        assert!(swarm_prompt.starts_with("/swarm refactor executor"));
        assert!(swarm_prompt.ends_with("[System Instructions: be concise]"));

        let normal_prompt = format_effective_prompt("explain rust", "be concise");
        assert_eq!(
            normal_prompt,
            "[System Instructions: be concise]\nexplain rust"
        );

        let settings = build_session_settings_json(
            Some(true),
            Some("gemini-4-neon-xhigh"),
            Some("gemini-4-hydrogen-high"),
            &[
                "~/.config/agygram/*".to_string(),
                "write_file(.env)".to_string(),
            ],
        )
        .expect("settings json");
        let parsed: serde_json::Value =
            serde_json::from_str(&settings).expect("valid settings json");
        assert_eq!(parsed["showThinking"], true);
        assert_eq!(parsed["orchestrator"]["planner"], "gemini-4-neon-xhigh");
        assert_eq!(parsed["orchestrator"]["executor"], "gemini-4-hydrogen-high");
        let deny = parsed["permissions"]["deny"]
            .as_array()
            .expect("deny array");
        assert!(deny.iter().any(|v| {
            v.as_str()
                .is_some_and(|s| s.contains(".config/agygram") && s.starts_with("read_file("))
        }));
        assert!(
            deny.iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains(".config/userbot")))
        );
        assert!(
            deny.iter()
                .any(|v| v.as_str().is_some_and(|s| s.contains(".ssh")))
        );
        assert!(deny.iter().any(|v| v.as_str() == Some("write_file(.env)")));

        let params = ExecutionParams {
            chat_id: 2,
            prompt: "/plan design api",
            conversation_id: Some("conv-9"),
            is_first_turn: false,
            workspace: "/tmp",
            extra_dirs: &[],
            model: "gemini-4-neon-high",
            effort: "high",
            mode: "auto",
            agent: "",
            system_prompt: "follow style",
            sandbox: false,
            project: "",
            settings_json: Some(settings.clone()),
            json_schema: Some(r#"{"type":"object"}"#),
            remote_agent: true,
            backend: ExecutorBackend::Print,
        };
        let args: Vec<String> = build_agy_command(&params)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let p_idx = args.iter().position(|a| a == "-p").unwrap();
        assert!(args[p_idx + 1].starts_with("/plan design api"));
        assert!(args[p_idx + 1].contains("[System Instructions: follow style]"));
        let s_idx = args.iter().position(|a| a == "--settings").unwrap();
        assert_eq!(args[s_idx + 1], settings);
        let j_idx = args.iter().position(|a| a == "--json-schema").unwrap();
        assert_eq!(args[j_idx + 1], r#"{"type":"object"}"#);
        assert!(args.contains(&"--remote-agent".to_string()));
    }

    #[test]
    fn stream_parser_emits_non_json_and_drops_malformed_json_starting_with_brace() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut turns = Vec::new();

        state.process_line(
            "plain text line",
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        state.process_line(
            "{malformed json",
            DetachDecision::Pending,
            &mut events,
            &mut turns,
        );
        assert_eq!(
            events,
            vec![StreamEvent::TextDelta("plain text line\n".to_string())]
        );
        assert!(!state.working_text.contains("{malformed json"));
    }

    #[test]
    fn stream_parser_handles_checkpoint_tool_error_subagent_waiting_and_structured_output() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();

        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"checkpoint","state":"DONE"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"tool","state":"ERROR","tool_name":"read_file","tool_info":{"name":"read_file","error":{"type":"permission_denied","message":"denied"}}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"step_type":"tool","state":"ACTIVE","subagent_info":{"subagents":[{"role":"reviewer","conversation_id":"abcdef123456"}]}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert!(state.saw_background_activity);
        assert!(events.contains(&StreamEvent::Checkpoint));
        assert!(events.contains(&StreamEvent::ToolError {
            tool: "read_file".to_string(),
            message: "permission_denied: denied".to_string(),
        }));
        assert!(events.contains(&StreamEvent::SubagentUpdate {
            summary: "reviewer (abcdef12)".to_string(),
        }));

        state.process_line(
            r#"{"event":"result","result":{"status":"WAITING","response":"","finished_reason":"waiting_for_input","structured_output":{"card":"ok"}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        let fin = unwrap_final(signals.last().expect("final signal"));
        assert_eq!(fin.final_status, "WAITING");
        assert_eq!(
            fin.final_finished_reason.as_deref(),
            Some("waiting_for_input")
        );
        assert_eq!(
            fin.final_structured_output,
            Some(serde_json::json!({ "card": "ok" }))
        );
        assert!(fin.has_background_activity);
        assert!(fin.accumulated_text.contains("等待确认"));
    }

    #[test]
    fn stream_parser_releases_unknown_tool_state_caps_anon_tools_and_takes_grace_turn2() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();

        // Repeated anonymous tool ACTIVE updates must cap at 1 so one non-active state clears it.
        for _ in 0..3 {
            state.process_line(
                r#"{"event":"step_update","step_update":{"step_type":"tool","state":"ACTIVE","tool_name":"search"}}"#,
                DetachDecision::Pending,
                &mut events,
                &mut signals,
            );
        }
        assert_eq!(state.active_anon_tools, 1);

        // Unknown/Other state must release tracking rather than leak pending tools.
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_type":"tool","state":"COMPLETED_UNKNOWN","tool_name":"search"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(state.active_anon_tools, 0);

        // Turn 1 DONE -> enters grace window.
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","state":"DONE","text_delta":"Turn 1"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(signals.len(), 1);
        events.clear();

        // Turn 2 completes INSIDE the 1.5s grace window -> working_text must be taken so Turn 3 never duplicates Turn 2!
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"step_type":"agent_response","state":"DONE","text_delta":"Turn 2"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert!(state.working_text.is_empty());

        let (_, flushed) = state.apply_decision(DetachDecision::Detached);
        assert!(flushed.contains(&StreamEvent::TurnComplete {
            text: "Turn 2".to_string()
        }));

        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"step_type":"agent_response","state":"DONE","text_delta":"Turn 3"}}"#,
            DetachDecision::Detached,
            &mut events,
            &mut signals,
        );
        assert!(events.contains(&StreamEvent::TurnComplete {
            text: "Turn 3".to_string()
        }));
    }

    #[test]
    fn clean_user_prompt_summary_strips_system_instructions_with_nested_brackets() {
        let raw =
            "[System Instructions: 规则：输出 [[send_file:/tmp/a.png]] 发送文件]\n帮我排查线上 OOM";
        assert_eq!(clean_user_prompt_summary(raw), "帮我排查线上 OOM");

        let wrapped =
            "<USER_REQUEST>[System Instructions: 规则 [[send_file:x]]]\n真实问题</USER_REQUEST>";
        assert_eq!(clean_user_prompt_summary(wrapped), "真实问题");

        let truncated_preview = "[System Instructions: 很长的系统指令被截断了";
        assert_eq!(clean_user_prompt_summary(truncated_preview), "");
    }

    #[tokio::test]
    async fn metadata_history_loader_filters_internal_and_prefers_ai_title() {
        let base = std::env::temp_dir().join(format!("agygram_meta_hist_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        let brain = base.join("brain");
        tokio::fs::create_dir_all(brain.join("conv-newer-0001"))
            .await
            .unwrap();
        tokio::fs::write(
            brain.join("conv-newer-0001").join("implementation_plan.md"),
            "# plan",
        )
        .await
        .unwrap();

        let meta_path = base.join("conversation_metadata.json");
        let meta_json = serde_json::json!({
            "conversations": {
                "conv-internal-01": {
                    "is_internal": true,
                    "last_modified_time": "2026-11-21T12:00:00Z",
                    "summary": { "Title": "Subagent Worker", "Preview": "internal", "NumSteps": 5 }
                },
                "conv-older-0002": {
                    "is_internal": false,
                    "last_modified_time": "2026-11-20T10:00:00Z",
                    "summary": {
                        "Title": "",
                        "Preview": "[System Instructions: rules [[send_file:a]]]\n修复 QQ 语音后缀",
                        "NumSteps": 12
                    }
                },
                "conv-newer-0001": {
                    "is_internal": false,
                    "last_modified_time": "2026-11-21T10:00:00Z",
                    "summary": {
                        "Title": "VPS Security Audit",
                        "Preview": "ignored preview",
                        "NumSteps": 30
                    }
                }
            }
        });
        tokio::fs::write(&meta_path, meta_json.to_string())
            .await
            .unwrap();

        let sessions = load_recent_conversations_from_metadata(&meta_path, &[brain], 5)
            .await
            .expect("metadata parsed");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "conv-newer-0001");
        assert_eq!(sessions[0].title, "VPS Security Audit");
        assert!(sessions[0].has_plan);
        assert!(!sessions[0].has_walkthrough);
        assert_eq!(sessions[1].id, "conv-older-0002");
        assert_eq!(sessions[1].title, "修复 QQ 语音后缀");

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[tokio::test]
    async fn skill_metadata_parser_respects_disable_flags_and_plugin_discovery() {
        let base = std::env::temp_dir().join(format!("agygram_skills_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        let skills_root = base.join("skills");
        let plugins_root = base.join("plugins");

        let public_dir = skills_root.join("my_skill_dir");
        let internal_dir = skills_root.join("swarm_monitor");
        let manual_dir = skills_root.join("native_swarm");
        let plugin_skill_dir = plugins_root
            .join("demo-plugin")
            .join("skills")
            .join("lint-helper");

        for d in [&public_dir, &internal_dir, &manual_dir, &plugin_skill_dir] {
            tokio::fs::create_dir_all(d).await.unwrap();
        }

        tokio::fs::write(
            public_dir.join("SKILL.md"),
            "---\nname: public-tool\ndescription: >-\n  Helpful public skill\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            internal_dir.join("SKILL.md"),
            "---\nname: swarm_monitor\ndescription: Internal only\ndisable-slash-command: true\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            manual_dir.join("SKILL.md"),
            "---\nname: swarm\ndescription: Run swarm\ndisable-model-invocation: true\n---\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            plugin_skill_dir.join("SKILL.md"),
            "---\nname: lint-helper\ndescription: Plugin linter\n---\n",
        )
        .await
        .unwrap();

        let internal_meta = read_skill_metadata(&internal_dir.join("SKILL.md")).await;
        assert!(internal_meta.disable_slash_command);
        let manual_meta = read_skill_metadata(&manual_dir.join("SKILL.md")).await;
        assert!(manual_meta.disable_model_invocation);
        assert!(!manual_meta.disable_slash_command);

        let mut seen = rustc_hash::FxHashSet::default();
        let scanned = scan_skill_dir(&skills_root, &mut seen).await;
        let names: Vec<&str> = scanned.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["public-tool", "swarm"]);

        let plugin_scanned = scan_plugin_skills_root(&plugins_root, &mut seen).await;
        assert_eq!(
            plugin_scanned,
            vec![("lint-helper".to_string(), "Plugin linter".to_string())]
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[test]
    fn combine_cli_output_and_char_boundary_helpers_work() {
        assert_eq!(combine_cli_output("out\n", "err\n"), "out\nerr");
        assert_eq!(combine_cli_output("", "  err  "), "err");
        assert_eq!(combine_cli_output("  out ", ""), "out");
        assert_eq!(combine_cli_output(" ", " "), "");

        let s = "a中b"; // 'a'(1), '中'(3 bytes: 1..4), 'b'(1)
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 1), 1);
        assert_eq!(floor_char_boundary(s, 2), 1);
        assert_eq!(floor_char_boundary(s, 3), 1);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 99), 5);
    }

    #[tokio::test]
    async fn stdout_reader_drops_stream_tx_immediately_on_synchronous_decision() {
        use std::process::Stdio;
        use tokio::process::Command;

        // Child outputs turn-1 + result, then sleeps 5s keeping stdout open (simulating LS shutdown).
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(
                r#"printf '%s\n' '{"event":"step_update","step_update":{"text_delta":"ok"}}' '{"event":"result","result":{"status":"SUCCESS","response":"ok"}}'; sleep 5"#,
            )
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let stdout = child.stdout.take().expect("stdout");

        let tracker = super::ActivityTracker::new();
        let (stream_tx, mut stream_rx) = mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = mpsc::channel(4);
        let (detach_tx, detach_rx) = tokio::sync::watch::channel(DetachDecision::Pending);

        let reader_handle =
            super::spawn_stdout_reader(stdout, tracker, stream_tx, turn_done_tx, detach_rx);

        let sig = tokio::time::timeout(Duration::from_secs(2), turn_done_rx.recv())
            .await
            .expect("turn_done timely")
            .expect("turn_done present");
        let done = unwrap_final(&sig);
        assert!(done.is_final_result);
        assert_eq!(done.accumulated_text, "ok");

        let _ = detach_tx.send(DetachDecision::Synchronous);

        // Drain stream_rx: must reach None immediately without waiting for the 5s sleep.
        let closed_fast = tokio::time::timeout(Duration::from_millis(500), async {
            let mut seen = Vec::new();
            while let Some(ev) = stream_rx.recv().await {
                seen.push(ev);
            }
            seen
        })
        .await
        .expect("stream_rx closed without waiting for child exit");
        assert_eq!(closed_fast, vec![StreamEvent::TextDelta("ok".to_string())]);

        reader_handle.abort();
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn stdout_reader_keeps_stream_tx_open_and_delivers_followup_on_detach() {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(
                r#"printf '%s\n' '{"event":"step_update","step_update":{"text_delta":"Turn1"}}' '{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE"}}'; sleep 0.05; printf '%s\n' '{"event":"step_update","step_update":{"text_delta":"Turn2"}}' '{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE"}}'"#,
            )
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let stdout = child.stdout.take().expect("stdout");

        let tracker = super::ActivityTracker::new();
        let (stream_tx, mut stream_rx) = mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = mpsc::channel(4);
        let (detach_tx, detach_rx) = tokio::sync::watch::channel(DetachDecision::Pending);

        let reader_handle =
            super::spawn_stdout_reader(stdout, tracker, stream_tx, turn_done_tx, detach_rx);

        let sig = tokio::time::timeout(Duration::from_secs(2), turn_done_rx.recv())
            .await
            .expect("turn1 timely")
            .expect("turn1 present");
        let done = unwrap_turn1(&sig);
        assert!(!done.is_final_result);
        assert_eq!(done.accumulated_text, "Turn1");

        // Drain turn-1 events before detaching, mirroring ingest_task -> stream_detached_followups.
        assert_eq!(
            stream_rx.recv().await,
            Some(StreamEvent::TextDelta("Turn1".to_string()))
        );
        assert_eq!(
            stream_rx.recv().await,
            Some(StreamEvent::TurnComplete {
                text: "Turn1".to_string()
            })
        );

        let _ = detach_tx.send(DetachDecision::Detached);

        let followup = tokio::time::timeout(Duration::from_secs(2), async {
            let mut rest = Vec::new();
            while let Some(ev) = stream_rx.recv().await {
                rest.push(ev);
            }
            rest
        })
        .await
        .expect("followup timely");

        assert_eq!(
            followup,
            vec![
                StreamEvent::TextDelta("Turn2".to_string()),
                StreamEvent::TurnComplete {
                    text: "Turn2".to_string()
                },
            ]
        );
        let _ = reader_handle.await;
        let _ = child.wait().await;
    }

    #[tokio::test]
    async fn readers_handle_invalid_utf8_and_bound_stderr_to_64kib() {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(
                r#"printf 'before-\377-after\n'; printf '{"event":"result","result":{"response":"survived"}}\n'; dd if=/dev/zero bs=1024 count=80 2>/dev/null | tr '\0' 'E' >&2"#,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sh");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let tracker = super::ActivityTracker::new();
        let (stream_tx, mut stream_rx) = mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = mpsc::channel(4);
        let (_detach_tx, detach_rx) = tokio::sync::watch::channel(DetachDecision::Pending);

        let stdout_handle =
            super::spawn_stdout_reader(stdout, tracker.clone(), stream_tx, turn_done_tx, detach_rx);
        let stderr_handle = super::spawn_stderr_reader(stderr, tracker);

        let _ = child.wait().await;
        let _ = stdout_handle.await.expect("stdout ok");
        let stderr_out = stderr_handle.await.expect("stderr ok");

        let first_ev = stream_rx.recv().await.expect("first event");
        match first_ev {
            StreamEvent::TextDelta(s) => assert!(s.contains('\u{FFFD}') && s.contains("after")),
            other => panic!("unexpected event: {other:?}"),
        }
        let sig = turn_done_rx.recv().await.expect("result turn done");
        let done = unwrap_final(&sig);
        assert!(done.is_final_result);
        assert_eq!(done.accumulated_text, "survived");

        assert!(stderr_out.len() <= super::MAX_STDERR_BYTES + 64);
        assert!(stderr_out.contains("...[stderr truncated]"));
    }

    #[tokio::test]
    async fn transcript_and_skill_readers_parse_streaming_and_stop_early() {
        let base = std::env::temp_dir().join(format!("agygram_exec_test_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&base).await;
        tokio::fs::create_dir_all(&base).await.expect("mkdir");

        let transcript = base.join("transcript.jsonl");
        let content = concat!(
            "{\"type\":\"SYSTEM\",\"content\":\"ignored\"}\n",
            "{\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>\\nFix the bug\\n</USER_REQUEST>\"}\n",
            "{\"type\":\"USER_INPUT\",\"content\":\"Second prompt should not overwrite\"}\n",
        );
        tokio::fs::write(&transcript, content)
            .await
            .expect("write transcript");
        assert_eq!(
            super::extract_transcript_title("test-conv-memo", &transcript)
                .await
                .as_deref(),
            Some("Fix the bug")
        );
        // Verify memoized CONV_TITLE_CACHE hit even after file removal:
        let _ = tokio::fs::remove_file(&transcript).await;
        assert_eq!(
            super::extract_transcript_title("test-conv-memo", &transcript)
                .await
                .as_deref(),
            Some("Fix the bug")
        );

        let skill_md = base.join("SKILL.md");
        tokio::fs::write(
            &skill_md,
            "---\nname: demo\ndescription: >\n  Fast async reader test\n---\n",
        )
        .await
        .expect("write skill");
        assert_eq!(
            super::read_skill_description(&skill_md).await,
            "Fast async reader test"
        );

        let _ = tokio::fs::remove_dir_all(&base).await;
    }

    #[test]
    fn timeout_messages_emit_plain_and_markdown_without_telegram_html() {
        let (err_stdin, out_stdin) = format_timeout_messages(TimeoutReason::StdinDeadlock {
            command: "ssh user@host < /tmp/key && echo `ok`".to_string(),
        });
        assert!(!err_stdin.contains("<b>") && !err_stdin.contains("<code>"));
        assert!(!out_stdin.contains("<b>") && !out_stdin.contains("<code>"));
        assert!(out_stdin.contains("**任务死锁终止**"));
        assert!(out_stdin.contains("ssh user@host < /tmp/key"));

        let (err_frozen, out_frozen) = format_timeout_messages(TimeoutReason::FrozenDeadlock {
            idle_secs: 180,
            active_processes: vec!["agy<main>&1".to_string()],
        });
        assert!(!err_frozen.contains("<b>") && !err_frozen.contains("<code>"));
        assert!(!out_frozen.contains("<b>") && !out_frozen.contains("<code>"));
        assert!(out_frozen.contains("**任务卡死终止**"));
        assert!(out_frozen.contains("`agy<main>&1`"));

        let (err_max, out_max) =
            format_timeout_messages(TimeoutReason::MaxExecutionExceeded { max_secs: 3600 });
        assert!(!err_max.contains("<b>") && !err_max.contains("<code>"));
        assert!(!out_max.contains("<b>") && !out_max.contains("<code>"));
        assert!(out_max.contains("**任务超时终止**"));
        assert!(out_max.contains("60分钟"));
    }

    #[test]
    fn parser_tracks_single_step_context_tokens_and_resets_on_checkpoint() {
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();

        // Multi-step turn with two model steps (197,443 + 198,613 = 396,056 cumulative in result).
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","state":"DONE","usage":{"input_tokens":197443,"cache_read_tokens":0,"output_tokens":411}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"step_type":"agent_response","state":"DONE","text_delta":"done","usage":{"input_tokens":2613,"cache_read_tokens":196000,"output_tokens":555}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        state.process_line(
            r#"{"event":"result","result":{"conversation_id":"conv-ctx","status":"SUCCESS","response":"done","num_turns":2,"usage":{"input_tokens":396056,"cache_read_tokens":196000,"output_tokens":966}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );

        let final_sig = signals
            .iter()
            .rev()
            .find_map(|s| match s {
                TurnSignal::FinalResult(data) => Some(data),
                _ => None,
            })
            .expect("final result signal");
        assert_eq!(
            final_sig.last_context_tokens,
            Some(198_613),
            "context watermark must reflect single-step input+cache, not multi-step cumulative sum"
        );
        assert_eq!(final_sig.num_turns, Some(2));
        assert!(!final_sig.saw_checkpoint);

        // A checkpoint resets the context watermark and marks saw_checkpoint = true.
        let mut cp_state = StreamParserState::new();
        events.clear();
        signals.clear();
        cp_state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":10,"step_type":"checkpoint","state":"DONE","usage":{"input_tokens":2470,"cache_read_tokens":197744,"output_tokens":5375}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert!(cp_state.saw_checkpoint);
        assert_eq!(cp_state.last_context_tokens, Some(0));

        // A subsequent post-checkpoint model step populates the fresh compacted context size.
        cp_state.process_line(
            r#"{"event":"step_update","step_update":{"step_index":11,"step_type":"agent_response","state":"DONE","text_delta":"ok","usage":{"input_tokens":18782,"cache_read_tokens":0,"output_tokens":100}}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(cp_state.last_context_tokens, Some(18_782));
    }

    #[test]
    fn v18_routing_helpers_and_empty_conv_id_filtering() {
        assert!(will_use_acp(ExecutorBackend::Acp, false, false));
        assert!(!will_use_acp(ExecutorBackend::Acp, true, false));
        assert!(!will_use_acp(ExecutorBackend::Acp, false, true));
        assert!(!will_use_acp(ExecutorBackend::Persistent, false, false));
        assert!(!will_use_acp(ExecutorBackend::Print, false, false));

        // `/compact` works natively in persistent mode; other CLI-answered slash commands route to print.
        assert!(!is_persistent_incompatible_slash_command("/compact"));
        assert!(!is_persistent_incompatible_slash_command(
            "/compact focus on tests"
        ));
        assert!(is_persistent_incompatible_slash_command("/model"));
        assert!(is_persistent_incompatible_slash_command(
            "  /rename new-name"
        ));
        assert!(is_persistent_incompatible_slash_command("/quota"));
        assert!(!is_persistent_incompatible_slash_command(
            "/plan build feature"
        ));
        assert!(!is_persistent_incompatible_slash_command("normal prompt"));

        // `format_effective_prompt_with_flag` accurately tracks whether system prompt was embedded.
        assert!(!format_effective_prompt_with_flag("hi", "").1);
        assert!(!format_effective_prompt_with_flag("/rename abc", "rules").1);
        assert!(!format_effective_prompt_with_flag("/compact", "rules").1);
        assert!(format_effective_prompt_with_flag("/plan api", "rules").1);
        assert!(format_effective_prompt_with_flag("hello", "rules").1);

        // Empty conversation_id on ERROR Result must not clobber previously detected conv_id.
        let mut state = StreamParserState::new();
        let mut events = Vec::new();
        let mut signals = Vec::new();
        state.process_line(
            r#"{"event":"init","conversation_id":"conv-real-1"}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        state.process_line(
            r#"{"event":"result","result":{"conversation_id":"","status":"ERROR","error":"bad model"}}"#,
            DetachDecision::Pending,
            &mut events,
            &mut signals,
        );
        assert_eq!(state.detected_conv_id.as_deref(), Some("conv-real-1"));

        // Detached Result emits StreamEvent::Result with empty response + checkpoint/context metadata.
        let mut dstate = StreamParserState::new();
        events.clear();
        signals.clear();
        dstate.apply_decision(DetachDecision::Detached);
        dstate.process_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"step_type":"checkpoint","state":"DONE"}}"#,
            DetachDecision::Detached,
            &mut events,
            &mut signals,
        );
        dstate.process_line(
            r#"{"event":"result","result":{"conversation_id":"conv-det","status":"SUCCESS","response":"dup"}}"#,
            DetachDecision::Detached,
            &mut events,
            &mut signals,
        );
        assert!(events.iter().any(|ev| matches!(
            ev,
            StreamEvent::Result(r) if r.response.is_empty() && r.saw_checkpoint && r.context_tokens == Some(0)
        )));
    }
}
