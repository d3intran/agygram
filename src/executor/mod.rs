pub mod protocol;
pub mod watchdog;
pub use protocol::*;
pub use watchdog::*;

use anyhow::{Context, Result};
use rustc_hash::FxHashSet;
use serde::Deserialize;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_LINE_BUF_CAPACITY: usize = 1024 * 1024;
const MAX_PENDING_DETACH_EVENTS: usize = 2048;
const FINAL_RESULT_REAPER_GRACE: Duration = Duration::from_secs(15);
const CLI_SUBCOMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_UPDATE_TIMEOUT: Duration = Duration::from_secs(120);

pub struct AgyExecutor;

static AGY_EXECUTABLE_PATH: OnceLock<PathBuf> = OnceLock::new();
static STDBUF_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

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

fn get_stdbuf_path() -> Option<&'static PathBuf> {
    STDBUF_PATH
        .get_or_init(|| which::which("stdbuf").ok())
        .as_ref()
}

/// Resolves extra `--add-dir` entries to absolute paths and drops duplicates.
/// agy rejects non-absolute paths, and sessions persisted by older builds may still hold a
/// literal `~`; the workspace is added separately, so it is excluded here to avoid a redundant flag.
fn resolve_extra_dirs(workspace: &str, extra_dirs: &[String]) -> Vec<String> {
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

#[derive(Debug, Clone)]
pub struct ExecutionParams<'a> {
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
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TurnDoneData {
    pub accumulated_text: String,
    pub conversation_id: Option<String>,
    pub final_status: String,
    pub final_usage: Option<TokenUsage>,
    pub final_duration: Option<f64>,
    pub is_final_result: bool,
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
    final_duration: Option<f64>,
}

impl Default for StdoutResult {
    fn default() -> Self {
        Self {
            accumulated_text: String::new(),
            conversation_id: None,
            final_status: "SUCCESS".to_string(),
            final_usage: None,
            final_duration: None,
        }
    }
}

/// Pure, testable stream-json parser and turn/detachment state machine.
pub(crate) struct StreamParserState {
    pub first_turn_text: Option<String>,
    pub working_text: String,
    pub detected_conv_id: Option<String>,
    pub final_status: String,
    pub final_usage: Option<TokenUsage>,
    pub final_duration: Option<f64>,
    pub is_detached_mode: bool,
    pub first_turn_completed: bool,
    pub turn_has_unflushed_text: bool,
    pub pending_post_turn1_events: Vec<StreamEvent>,
}

impl StreamParserState {
    pub(crate) fn new() -> Self {
        Self {
            first_turn_text: None,
            working_text: String::new(),
            detected_conv_id: None,
            final_status: "SUCCESS".to_string(),
            final_usage: None,
            final_duration: None,
            is_detached_mode: false,
            first_turn_completed: false,
            turn_has_unflushed_text: false,
            pending_post_turn1_events: Vec::new(),
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
        if self.first_turn_completed && !self.is_detached_mode && decision == DetachDecision::Pending
        {
            if self.pending_post_turn1_events.len() < MAX_PENDING_DETACH_EVENTS {
                self.pending_post_turn1_events.push(event);
            }
        } else if decision != DetachDecision::Synchronous {
            out_events.push(event);
        }
    }

    pub(crate) fn combined_non_detached_text(&self) -> String {
        match (self.first_turn_text.as_deref(), self.working_text.trim().is_empty()) {
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
        out_turn_done: &mut Vec<TurnDoneData>,
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
            Ok(RawAgyJsonEvent::Init { conversation_id }) => {
                if let Some(cid) = conversation_id {
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
                if let Some(cid) = step_update.conversation_id.take() {
                    if self.detected_conv_id.is_none() {
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

                if let Some(thinking) = step_update.thinking_delta.take() {
                    self.push_event(StreamEvent::ThinkingDelta(thinking), decision, out_events);
                }

                if let Some(delta) = step_update.text_delta.take() {
                    self.working_text.push_str(&delta);
                    self.turn_has_unflushed_text = true;
                    self.push_event(StreamEvent::TextDelta(delta), decision, out_events);
                }

                if let Some(u) = step_update.usage.take() {
                    self.final_usage = Some(u);
                }
                if let Some(d) = step_update.duration_seconds {
                    self.final_duration = Some(d);
                }

                if let Some((tool_name, summary)) = step_update.take_tool_use() {
                    self.push_event(
                        StreamEvent::ToolUse {
                            tool: tool_name,
                            summary,
                        },
                        decision,
                        out_events,
                    );
                } else if step_update.is_agent_turn_done() && !self.working_text.trim().is_empty()
                {
                    self.turn_has_unflushed_text = false;
                    if !self.first_turn_completed {
                        let captured = std::mem::take(&mut self.working_text);
                        if decision != DetachDecision::Synchronous {
                            out_events.push(StreamEvent::TurnComplete {
                                text: captured.clone(),
                            });
                        }
                        self.first_turn_completed = true;
                        self.first_turn_text = Some(captured.clone());
                        out_turn_done.push(TurnDoneData {
                            accumulated_text: captured,
                            conversation_id: self.detected_conv_id.clone(),
                            final_status: self.final_status.clone(),
                            final_usage: self.final_usage.clone(),
                            final_duration: self.final_duration,
                            is_final_result: false,
                        });
                    } else {
                        let turn_text = if self.is_detached_mode {
                            std::mem::take(&mut self.working_text)
                        } else {
                            self.working_text.clone()
                        };
                        self.push_event(
                            StreamEvent::TurnComplete { text: turn_text },
                            decision,
                            out_events,
                        );
                    }
                }
            }
            Ok(RawAgyJsonEvent::Result { result }) => {
                if let Some(cid) = result.conversation_id {
                    self.detected_conv_id = Some(cid);
                }
                if let Some(st) = result.status {
                    self.final_status = st;
                }
                if let Some(u) = result.usage {
                    self.final_usage = Some(u);
                }
                if let Some(d) = result.duration_seconds {
                    self.final_duration = Some(d);
                }

                if self.is_detached_mode {
                    if self.turn_has_unflushed_text && !self.working_text.trim().is_empty() {
                        let flushed_text = std::mem::take(&mut self.working_text);
                        self.turn_has_unflushed_text = false;
                        out_events.push(StreamEvent::TurnComplete { text: flushed_text });
                    } else {
                        self.working_text.clear();
                    }
                } else {
                    let should_use_result_response = !self.first_turn_completed
                        || (self.working_text.trim().is_empty()
                            && self
                                .first_turn_text
                                .as_deref()
                                .is_none_or(|t| t.trim().is_empty()));
                    if should_use_result_response {
                        if let Some(resp) = result.response {
                            if !resp.trim().is_empty() {
                                self.working_text = resp;
                            }
                        }
                    }

                    let final_text = self.combined_non_detached_text();
                    out_turn_done.push(TurnDoneData {
                        accumulated_text: final_text,
                        conversation_id: self.detected_conv_id.clone(),
                        final_status: self.final_status.clone(),
                        final_usage: self.final_usage.clone(),
                        final_duration: self.final_duration,
                        is_final_result: true,
                    });
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
                        "failed to parse stream-json line starting with '{{'"
                    );
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
                final_duration: self.final_duration,
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

fn build_agy_command(params: &ExecutionParams<'_>) -> Command {
    let final_prompt: Cow<'_, str> = if params.system_prompt.is_empty() {
        Cow::Borrowed(params.prompt)
    } else {
        Cow::Owned(format!(
            "[System Instructions: {}]\n{}",
            params.system_prompt, params.prompt
        ))
    };

    let agy_bin = get_agy_path();
    let mut cmd = if let Some(stdbuf) = get_stdbuf_path() {
        let mut c = Command::new(stdbuf);
        c.arg("-oL").arg("-eL").arg(agy_bin);
        c
    } else {
        Command::new(agy_bin)
    };

    cmd.kill_on_drop(true);
    cmd.process_group(0);
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");

    cmd.arg("-p")
        .arg(final_prompt.as_ref())
        .arg("--output-format")
        .arg("stream-json")
        .arg("--dangerously-skip-permissions");

    if let Some(conv_id) = params.conversation_id {
        cmd.arg("--conversation").arg(conv_id);
    } else if !params.is_first_turn {
        cmd.arg("-c");
    }

    if !params.workspace.is_empty() {
        cmd.arg("--add-dir").arg(params.workspace);
        cmd.current_dir(params.workspace);
    }

    for dir in resolve_extra_dirs(params.workspace, params.extra_dirs) {
        cmd.arg("--add-dir").arg(&dir);
    }

    let model_str = params.model.trim();
    if !model_str.is_empty() {
        cmd.arg("--model").arg(model_str);
    }

    // Only pass --effort if the model does not already encode its effort tier or reject --effort
    let has_explicit_tier = model_str.ends_with("-high")
        || model_str.ends_with("-medium")
        || model_str.ends_with("-med")
        || model_str.ends_with("-low")
        || model_str.ends_with("-xhigh")
        || model_str.ends_with("-max")
        || model_str.starts_with("claude");

    if !has_explicit_tier && !params.effort.is_empty() {
        cmd.arg("--effort").arg(params.effort);
    }

    if !params.mode.is_empty() {
        cmd.arg("--mode").arg(params.mode);
    }

    if !params.agent.is_empty() {
        cmd.arg("--agent").arg(params.agent);
    }

    if params.sandbox {
        cmd.arg("--sandbox");
    }

    if !params.project.is_empty() {
        cmd.arg("--project").arg(params.project);
    }

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

async fn send_stream_events(
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
    turn_done_tx: mpsc::Sender<TurnDoneData>,
    mut detach_rx: watch::Receiver<DetachDecision>,
) -> tokio::task::JoinHandle<StdoutResult> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut state = StreamParserState::new();
        let mut stream_tx_opt = Some(stream_tx);
        let mut out_events: Vec<StreamEvent> = Vec::with_capacity(8);
        let mut out_turn_done: Vec<TurnDoneData> = Vec::with_capacity(2);

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
                    &mut out_turn_done,
                );
            }

            line_buf.clear();
            if line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                line_buf = Vec::with_capacity(8192);
            }

            if !out_events.is_empty() {
                send_stream_events(&mut stream_tx_opt, out_events.drain(..)).await;
            }
            for done in out_turn_done.drain(..) {
                if turn_done_tx.send(done).await.is_err() {
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
fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
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

fn spawn_background_reaper(
    mut child: Child,
    child_pid: Option<u32>,
    mut stdout_task: tokio::task::JoinHandle<StdoutResult>,
    mut stderr_task: tokio::task::JoinHandle<String>,
    wait_timeout: Duration,
    mode_label: &'static str,
) {
    tokio::spawn(async move {
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

fn format_timeout_messages(reason: TimeoutReason) -> (String, String) {
    match reason {
        TimeoutReason::StdinDeadlock { command } => {
            let escaped = crate::bot::formatter::TelegramFormatter::escape_html(&command);
            (
                format!(
                    "agy 任务终止：检测到子进程卡死在等待终端交互输入 (<code>{escaped}</code>)。由于非交互模式无法输入，已自动终止。"
                ),
                format!(
                    "⚠️ <b>任务死锁终止</b>\n检测到后台子进程正在等待终端用户输入：\n<code>{escaped}</code>\n非交互模式无法响应，已自动终止以防挂起。"
                ),
            )
        }
        TimeoutReason::FrozenDeadlock {
            idle_secs,
            process_count: _,
            active_processes,
        } => {
            let proc_str = if active_processes.is_empty() {
                "无活动子进程".to_string()
            } else {
                active_processes.join(", ")
            };
            let escaped_procs = crate::bot::formatter::TelegramFormatter::escape_html(&proc_str);
            (
                format!(
                    "agy 执行卡死 ({idle_secs}s 无任何输出且系统 0 算力/IO 消耗)，任务已终止。关联进程: {proc_str}"
                ),
                format!(
                    "⏱ <b>任务卡死终止</b>\n超过 {idle_secs}s 无任何输出，且系统监测到 0 算力消耗与 0 I/O 进展（可能网络连接静默断开或死锁）。\n关联进程: <code>{escaped_procs}</code>\n\n请发送 /new 重置会话后重试。"
                ),
            )
        }
        TimeoutReason::MaxExecutionExceeded { max_secs } => (
            format!("agy 执行超时 (超过全局安全上限 {max_secs}s)，任务已强制终止。"),
            format!(
                "⏱ <b>任务超时终止</b>\n任务总运行时间已超过全局安全上限 ({max_secs}s / {}分钟)。已强制终止以防死循环。\n请发送 /new 重置会话后重试。",
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
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute {context_label}"))?;

    let output = tokio::time::timeout(timeout_dur, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("{context_label} timed out after {}s", timeout_dur.as_secs()))?
        .with_context(|| format!("Failed while waiting for {context_label}"))?;

    Ok(CliOutput {
        success: output.status.success(),
        status_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn brain_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".gemini/antigravity-cli/brain"));
        let legacy = home.join(".gemini/antigravity/brain");
        if !candidates.contains(&legacy) {
            candidates.push(legacy);
        }
    }
    candidates
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

async fn extract_transcript_title(transcript_path: &Path) -> Option<String> {
    let file = tokio::fs::File::open(transcript_path).await.ok()?;
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::with_capacity(1024);

    loop {
        line_buf.clear();
        let n = reader.read_until(b'\n', &mut line_buf).await.ok()?;
        if n == 0 {
            return None;
        }
        let line = String::from_utf8_lossy(&line_buf);
        if !line.contains("USER_INPUT") {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<TranscriptLine<'_>>(line.as_ref()) {
            if parsed.kind == Some("USER_INPUT") {
                if let Some(c) = parsed.content {
                    let clean_prompt = clean_user_prompt_summary(&c);
                    if !clean_prompt.is_empty() {
                        return Some(clean_prompt);
                    }
                }
            }
        }
    }
}

async fn read_skill_description(skill_md: &Path) -> String {
    let Ok(file) = tokio::fs::File::open(skill_md).await else {
        return String::new();
    };
    let mut reader = BufReader::new(file);
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);
    let mut reading_multiline = false;

    for _ in 0..15 {
        line_buf.clear();
        let Ok(n) = reader.read_until(b'\n', &mut line_buf).await else {
            break;
        };
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&line_buf);
        let trimmed_line = line.trim();
        if reading_multiline {
            if !trimmed_line.is_empty() && !trimmed_line.starts_with("---") {
                return trimmed_line
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string();
            }
            break;
        }
        if let Some(d) = trimmed_line.strip_prefix("description:") {
            let val = d.trim().trim_matches('"').trim_matches('\'');
            if val == ">" || val == "|" || val == ">-" || val == "|-" {
                reading_multiline = true;
                continue;
            }
            return val.to_string();
        }
    }
    String::new()
}

async fn collect_skill_sections() -> Result<Vec<(&'static str, Vec<(String, String)>)>, &'static str>
{
    let Some(home) = dirs::home_dir() else {
        return Err("未找到家目录");
    };

    let search_dirs = [
        ("User Skills", home.join(".gemini/config/skills")),
        (
            "Builtin Skills (CLI)",
            home.join(".gemini/antigravity-cli/builtin/skills"),
        ),
        (
            "Builtin Skills (Legacy)",
            home.join(".gemini/antigravity/builtin/skills"),
        ),
    ];

    let mut sections = Vec::new();
    for (section, dir) in search_dirs {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };

        let mut section_skills = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry_is_dir(&entry).await {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let desc = read_skill_description(&entry.path().join("SKILL.md")).await;
            section_skills.push((name, desc));
        }

        if !section_skills.is_empty() {
            section_skills.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            sections.push((section, section_skills));
        }
    }

    Ok(sections)
}

struct RuleSection {
    icon: &'static str,
    title: &'static str,
    body: String,
}

async fn collect_rule_sections(workspace: &str) -> Result<Vec<RuleSection>, &'static str> {
    let Some(home) = dirs::home_dir() else {
        return Err("未找到家目录");
    };

    let mut sections = Vec::new();

    if let Ok(content) = tokio::fs::read_to_string(home.join(".gemini/GEMINI.md")).await {
        if !content.trim().is_empty() {
            sections.push(RuleSection {
                icon: "🌐",
                title: "Global Rules (~/.gemini/GEMINI.md)",
                body: content,
            });
        }
    }

    if let Ok(content) = tokio::fs::read_to_string(home.join(".gemini/config/config.json")).await {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(rules) = val.get("rules").or_else(|| val.get("custom_rules")) {
                sections.push(RuleSection {
                    icon: "🌐",
                    title: "Global Config Rules",
                    body: rules.to_string(),
                });
            }
        }
    }

    let ws_gemini_file = PathBuf::from(workspace).join("GEMINI.md");
    if let Ok(content) = tokio::fs::read_to_string(&ws_gemini_file).await {
        sections.push(RuleSection {
            icon: "📁",
            title: "Workspace Rules (GEMINI.md)",
            body: content,
        });
    }

    let ws_rule_file = PathBuf::from(workspace).join(".gemini/RULE.md");
    if ws_rule_file != ws_gemini_file {
        if let Ok(content) = tokio::fs::read_to_string(&ws_rule_file).await {
            sections.push(RuleSection {
                icon: "📁",
                title: "Workspace Rules (.gemini/RULE.md)",
                body: content,
            });
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

impl AgyExecutor {
    /// Executes agy using stream-json format and streams structured events line-by-line with unbuffered pipes.
    #[tracing::instrument(
        skip_all,
        fields(
            first_turn = params.is_first_turn,
            prompt_len = params.prompt.len(),
            has_conv_id = params.conversation_id.is_some()
        )
    )]
    pub async fn run_prompt_streaming(
        params: ExecutionParams<'_>,
        stream_tx: mpsc::Sender<StreamEvent>,
        pid_tx: Option<tokio::sync::oneshot::Sender<u32>>,
        idle_timeout_secs: u64,
        max_execution_secs: u64,
    ) -> Result<ExecutionResult> {
        info!(
            first_turn = params.is_first_turn,
            prompt_len = params.prompt.len(),
            "Executing agy with stream-json format"
        );

        let mut cmd = build_agy_command(&params);
        let mut child = cmd.spawn().context("Failed to spawn agy CLI process")?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
            if let Some(tx) = pid_tx {
                let _ = tx.send(pid);
            }
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
        let (turn_done_tx, mut turn_done_rx) = mpsc::channel::<TurnDoneData>(2);
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
        let mut probe_ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + probe_interval,
            probe_interval,
        );
        probe_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let start_time = std::time::Instant::now();

        let mut prev_metrics = ProcessTreeMetrics::default();
        let mut has_baseline = false;
        let mut consecutive_silent_probes = 0u32;
        let mut timeout_reason: Option<TimeoutReason> = None;
        let mut exit_status = None;
        let mut turn_detached_data: Option<TurnDoneData> = None;
        let mut final_result_data: Option<TurnDoneData> = None;

        loop {
            tokio::select! {
                status_res = child.wait() => {
                    exit_status = Some(status_res);
                    break;
                }
                Some(done_data) = turn_done_rx.recv() => {
                    if done_data.is_final_result {
                        final_result_data = Some(done_data);
                        break;
                    }
                    info!(pid = ?child_pid, "agent turn completed; waiting up to 2.5s for final result or background detachment");
                    let grace_deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
                    let mut final_result_seen = false;
                    loop {
                        tokio::select! {
                            status_res = child.wait() => {
                                exit_status = Some(status_res);
                                break;
                            }
                            maybe_next = turn_done_rx.recv() => {
                                match maybe_next {
                                    Some(next_done) if next_done.is_final_result => {
                                        final_result_data = Some(next_done);
                                        break;
                                    }
                                    Some(_) => {}
                                    None => {
                                        final_result_seen = true;
                                        break;
                                    }
                                }
                            }
                            _ = tokio::time::sleep_until(grace_deadline) => {
                                info!(pid = ?child_pid, "background tasks active in process; detaching turn response");
                                turn_detached_data = Some(done_data);
                                break;
                            }
                        }
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

                    let idle_duration = tracker.idle_duration();
                    if idle_duration < idle_timeout {
                        consecutive_silent_probes = 0;
                        has_baseline = false;
                        continue;
                    }

                    let Some(pid) = child_pid else {
                        timeout_reason = Some(TimeoutReason::FrozenDeadlock {
                            idle_secs: idle_duration.as_secs(),
                            process_count: 0,
                            active_processes: Vec::new(),
                        });
                        break;
                    };

                    let Ok(inspection) =
                        tokio::task::spawn_blocking(move || inspect_process_tree(pid)).await
                    else {
                        warn!(pid, "process tree inspection task failed");
                        continue;
                    };

                    if let Some(cmd) = inspection.stdin_blocked_command {
                        warn!(pid, command = %cmd, "process tree deadlock detected: blocked waiting on stdin");
                        timeout_reason = Some(TimeoutReason::StdinDeadlock { command: cmd });
                        break;
                    }

                    if !has_baseline {
                        prev_metrics = inspection.metrics;
                        has_baseline = true;
                        consecutive_silent_probes = 0;
                        debug!(
                            pid,
                            cpu_ticks = prev_metrics.total_cpu_ticks,
                            io_bytes = prev_metrics.total_io_bytes,
                            process_count = prev_metrics.process_count,
                            "seeded watchdog baseline metrics after idle timeout"
                        );
                        continue;
                    }

                    let cpu_delta = inspection
                        .metrics
                        .total_cpu_ticks
                        .saturating_sub(prev_metrics.total_cpu_ticks);
                    let io_delta = inspection
                        .metrics
                        .total_io_bytes
                        .saturating_sub(prev_metrics.total_io_bytes);
                    prev_metrics = inspection.metrics;

                    if cpu_delta > 0 || io_delta > 0 {
                        debug!(
                            pid,
                            cpu_delta,
                            io_delta,
                            process_count = inspection.metrics.process_count,
                            active_processes = ?inspection.active_process_names,
                            "process tree active; extending idle deadline"
                        );
                        tracker.record_activity();
                        consecutive_silent_probes = 0;
                        has_baseline = false;
                        continue;
                    }

                    consecutive_silent_probes += 1;
                    // Require at least 2 consecutive silent probes after baseline to confirm freeze.
                    if consecutive_silent_probes >= 2 {
                        timeout_reason = Some(TimeoutReason::FrozenDeadlock {
                            idle_secs: idle_duration.as_secs(),
                            process_count: inspection.metrics.process_count,
                            active_processes: inspection.active_process_names,
                        });
                        break;
                    }
                }
            }
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
            let conv_after_timeout =
                await_reader_or_abort(&mut stdout_task, Duration::from_secs(2))
                    .await
                    .and_then(|res| res.conversation_id);

            cleanup_guard.disarm();

            let (error_event, summary_msg) = format_timeout_messages(reason);
            let _ = stream_tx.send(StreamEvent::Error(error_event)).await;
            return Ok(ExecutionResult {
                output: summary_msg,
                conversation_id: conv_after_timeout,
                status: "TIMEOUT".to_string(),
                usage: None,
                duration_seconds: None,
                detached: false,
            });
        }

        let status = exit_status.and_then(|r| r.ok());

        if let Some(final_data) = final_result_data {
            let mut final_output = final_data.accumulated_text.trim().to_string();
            if final_output.is_empty() {
                final_output = "Task completed with no text output.".to_string();
            }
            let _ = stream_tx
                .send(StreamEvent::Result {
                    conversation_id: final_data.conversation_id.clone(),
                    response: final_output.clone(),
                    status: final_data.final_status.clone(),
                    usage: final_data.final_usage.clone(),
                    duration_seconds: final_data.final_duration,
                })
                .await;

            // Tell stdout_task to drop its stream_tx clone immediately so consumers draining to
            // EOF (such as QQ) do not block on language-server shutdown.
            let _ = detach_tx.send(DetachDecision::Synchronous);
            cleanup_guard.disarm();
            spawn_background_reaper(
                child,
                child_pid,
                stdout_task,
                stderr_task,
                FINAL_RESULT_REAPER_GRACE,
                "final_result",
            );

            return Ok(ExecutionResult {
                output: final_output,
                conversation_id: final_data.conversation_id,
                status: final_data.final_status,
                usage: final_data.final_usage,
                duration_seconds: final_data.final_duration,
                detached: false,
            });
        }

        if let Some(detached) = turn_detached_data {
            let mut final_output = detached.accumulated_text.trim().to_string();
            if final_output.is_empty() {
                final_output = "Task completed with no text output.".to_string();
            }
            // Send turn-1 Result BEFORE switching detach_tx so follow-up events stay ordered after Result.
            let _ = stream_tx
                .send(StreamEvent::Result {
                    conversation_id: detached.conversation_id.clone(),
                    response: final_output.clone(),
                    status: detached.final_status.clone(),
                    usage: detached.final_usage.clone(),
                    duration_seconds: detached.final_duration,
                })
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
            );

            return Ok(ExecutionResult {
                output: final_output,
                conversation_id: detached.conversation_id,
                status: detached.final_status,
                usage: detached.final_usage,
                duration_seconds: detached.final_duration,
                detached: true,
            });
        }

        let _ = detach_tx.send(DetachDecision::Synchronous);
        let stdout_res = await_reader_or_abort(&mut stdout_task, Duration::from_secs(5))
            .await
            .unwrap_or_default();
        let stderr_text = await_reader_or_abort(&mut stderr_task, Duration::from_secs(2))
            .await
            .unwrap_or_default();
        cleanup_guard.disarm();

        let mut final_output = stdout_res.accumulated_text.trim().to_string();
        if final_output.is_empty() && !stderr_text.trim().is_empty() {
            final_output = format!("Error:\n{}", stderr_text.trim());
        } else if !stderr_text.trim().is_empty() && status.is_some_and(|s| !s.success()) {
            warn!(pid = ?child_pid, stderr = %stderr_text.trim(), "agy exited non-zero with stderr");
        }

        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }

        let _ = stream_tx
            .send(StreamEvent::Result {
                conversation_id: stdout_res.conversation_id.clone(),
                response: final_output.clone(),
                status: stdout_res.final_status.clone(),
                usage: stdout_res.final_usage.clone(),
                duration_seconds: stdout_res.final_duration,
            })
            .await;

        Ok(ExecutionResult {
            output: final_output,
            conversation_id: stdout_res.conversation_id,
            status: stdout_res.final_status,
            usage: stdout_res.final_usage,
            duration_seconds: stdout_res.final_duration,
            detached: false,
        })
    }

    /// Generic helper to execute an agy CLI subcommand with timeout, exit-status check, and ANSI stripping.
    async fn run_cli_command(args: &[&str]) -> Result<String> {
        let label = format!("agy {}", args.join(" "));
        let out = exec_command_with_timeout(
            get_agy_path(),
            args,
            CLI_SUBCOMMAND_TIMEOUT,
            &label,
        )
        .await?;

        if !out.success {
            let combined = combine_cli_output(&out.stdout, &out.stderr);
            let clean_err = crate::bot::formatter::TelegramFormatter::strip_ansi_codes(&combined);
            anyhow::bail!(
                "{} failed (status {:?}): {}",
                label,
                out.status_code,
                clean_err.trim()
            );
        }

        let clean = crate::bot::formatter::TelegramFormatter::strip_ansi_codes(&out.stdout);
        Ok(clean.trim().to_string())
    }

    async fn run_agy_subcommand(subcommand: &str, args: &[&str]) -> Result<String> {
        let mut full_args = Vec::with_capacity(1 + args.len());
        full_args.push(subcommand);
        full_args.extend_from_slice(args);
        let label = format!("agy {}", full_args.join(" "));

        let out = exec_command_with_timeout(
            get_agy_path(),
            &full_args,
            CLI_SUBCOMMAND_TIMEOUT,
            &label,
        )
        .await?;

        let combined = combine_cli_output(&out.stdout, &out.stderr);
        let clean = crate::bot::formatter::TelegramFormatter::strip_ansi_codes(&combined);
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
        let clean = Self::run_cli_command(&["models"]).await?;
        if clean.is_empty() {
            Ok("gemini-3.8-flash-high (Gemini 3.8 Flash High)".to_string())
        } else {
            Ok(clean)
        }
    }

    pub async fn list_mcp() -> Result<String> {
        let clean = Self::run_cli_command(&["mcp", "list"]).await?;
        if clean.is_empty() {
            Ok("No MCP servers configured.".to_string())
        } else {
            Ok(clean)
        }
    }

    pub async fn list_agents() -> Result<String> {
        let clean = Self::run_cli_command(&["agents"]).await?;
        if clean.is_empty() {
            Ok("Default agent".to_string())
        } else {
            Ok(clean)
        }
    }

    pub async fn list_plugins() -> Result<String> {
        let clean = Self::run_cli_command(&["plugins"]).await?;
        if clean.is_empty() {
            Ok("No active plugins installed.".to_string())
        } else {
            Ok(clean)
        }
    }

    pub async fn get_version() -> String {
        if let Ok(out) = exec_command_with_timeout(
            get_agy_path(),
            &["--version"],
            Duration::from_secs(10),
            "agy --version",
        )
        .await
        {
            if out.success {
                let version = out.stdout.trim();
                if !version.is_empty() {
                    return version.to_string();
                }
            }
        }
        "1.1.13".to_string()
    }

    pub async fn show_changelog() -> Result<String> {
        let clean = Self::run_cli_command(&["changelog"]).await?;
        if clean.is_empty() {
            let ver = Self::get_version().await;
            Ok(format!("Antigravity CLI v{ver} - Features structured output (--output-format stream-json/json), JSON Schema enforcement, TUI settings, and performance enhancements."))
        } else {
            Ok(clean)
        }
    }

    /// Returns all existing brain directories (modern CLI brain + legacy fallback).
    #[must_use]
    pub fn get_brain_dirs() -> Vec<PathBuf> {
        brain_dir_candidates()
            .into_iter()
            .filter(|p| p.exists())
            .collect()
    }

    /// Async variant of [`Self::get_brain_dirs`] using non-blocking `tokio::fs`.
    pub async fn get_brain_dirs_async() -> Vec<PathBuf> {
        let mut out = Vec::with_capacity(2);
        for candidate in brain_dir_candidates() {
            if tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
                out.push(candidate);
            }
        }
        out
    }

    /// Lists recent conversations from `~/.gemini/antigravity-cli/brain/` and legacy `~/.gemini/antigravity/brain/`.
    pub async fn list_recent_conversations(limit: usize) -> Vec<RecentSessionInfo> {
        if limit == 0 {
            return Vec::new();
        }
        let brain_dirs = brain_dir_candidates();
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
                extract_transcript_title(&transcript_path),
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

    /// Scan and list all active Antigravity skills (Telegram HTML formatted).
    pub async fn list_skills() -> String {
        let sections = match collect_skill_sections().await {
            Ok(s) => s,
            Err(msg) => return msg.to_string(),
        };

        let mut result = String::new();
        for (section, section_skills) in sections {
            let _ = writeln!(result, "📦 <b>{section}</b>:");
            for (name, desc) in section_skills {
                if desc.is_empty() {
                    let _ = writeln!(result, "• <code>/{name}</code>");
                } else {
                    let short_desc =
                        crate::bot::formatter::TelegramFormatter::truncate_chars(&desc, 50);
                    let _ = writeln!(
                        result,
                        "• <code>/{name}</code> : <i>{}</i>",
                        crate::bot::formatter::TelegramFormatter::escape_html(&short_desc)
                    );
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
                    let short_desc =
                        crate::bot::formatter::TelegramFormatter::truncate_chars(&desc, 50);
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

    /// Read global and workspace rules (Telegram HTML formatted).
    pub async fn list_rules(workspace: &str) -> String {
        let sections = match collect_rule_sections(workspace).await {
            Ok(s) => s,
            Err(msg) => return msg.to_string(),
        };

        let mut output = String::new();
        for sec in sections {
            let _ = writeln!(output, "{} <b>{}</b>:", sec.icon, sec.title);
            let _ = writeln!(
                output,
                "<pre><code>{}</code></pre>\n",
                crate::bot::formatter::TelegramFormatter::escape_html(&sec.body)
            );
        }

        if output.is_empty() {
            "📋 <b>当前暂无活跃的自定义规则文件。</b>\n可在项目根目录下创建 <code>GEMINI.md</code> 添加项目规则。".to_string()
        } else {
            output.trim_end().to_string()
        }
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
        let out = exec_command_with_timeout(
            agy_bin,
            &["update"],
            CLI_UPDATE_TIMEOUT,
            "agy update",
        )
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

#[derive(Debug, Clone)]
pub struct RecentSessionInfo {
    pub id: String,
    pub title: String,
    pub updated_time: String,
    pub has_plan: bool,
    pub has_walkthrough: bool,
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
    let single_line = clean
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    crate::bot::formatter::TelegramFormatter::truncate_chars(single_line, 40).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        clean_user_prompt_summary, combine_cli_output, floor_char_boundary, resolve_extra_dirs,
        update_needs_elevation, DetachDecision, StreamEvent, StreamParserState, TurnDoneData,
    };

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
        let access_err =
            "Error: directory /usr/local/bin is not fully accessible (readable: true, writable: false)";
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
        let mut turns: Vec<TurnDoneData> = Vec::new();

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
        assert!(!turns[0].is_final_result);
        assert_eq!(turns[0].accumulated_text, "Hello world");

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
        assert!(turns[1].is_final_result);
        assert_eq!(turns[1].accumulated_text, "Hello world");
        assert_eq!(
            turns[1].final_usage.as_ref().map(|u| u.total_tokens),
            Some(42)
        );

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
        assert_eq!(turns[0].accumulated_text, "Turn 1");
        events.clear();

        // Events arriving during the 2.5s grace window stay buffered so they follow Turn 1 Result.
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
        assert_eq!(events, vec![StreamEvent::TurnComplete { text: "Turn 2".to_string() }]);
        // Turn-2 completion in detached mode must not emit another TurnDoneData.
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn stream_parser_emits_non_json_and_logs_malformed_json() {
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
            vec![
                StreamEvent::TextDelta("plain text line\n".to_string()),
                StreamEvent::TextDelta("{malformed json\n".to_string()),
            ]
        );
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
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = tokio::sync::mpsc::channel(4);
        let (detach_tx, detach_rx) = tokio::sync::watch::channel(DetachDecision::Pending);

        let reader_handle =
            super::spawn_stdout_reader(stdout, tracker, stream_tx, turn_done_tx, detach_rx);

        let done = tokio::time::timeout(std::time::Duration::from_secs(2), turn_done_rx.recv())
            .await
            .expect("turn_done timely")
            .expect("turn_done present");
        assert!(done.is_final_result);
        assert_eq!(done.accumulated_text, "ok");

        let _ = detach_tx.send(DetachDecision::Synchronous);

        // Drain stream_rx: must reach None immediately without waiting for the 5s sleep.
        let closed_fast = tokio::time::timeout(std::time::Duration::from_millis(500), async {
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
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = tokio::sync::mpsc::channel(4);
        let (detach_tx, detach_rx) = tokio::sync::watch::channel(DetachDecision::Pending);

        let reader_handle =
            super::spawn_stdout_reader(stdout, tracker, stream_tx, turn_done_tx, detach_rx);

        let done = tokio::time::timeout(std::time::Duration::from_secs(2), turn_done_rx.recv())
            .await
            .expect("turn1 timely")
            .expect("turn1 present");
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

        let followup = tokio::time::timeout(std::time::Duration::from_secs(2), async {
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
        let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(32);
        let (turn_done_tx, mut turn_done_rx) = tokio::sync::mpsc::channel(4);
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
        let done = turn_done_rx.recv().await.expect("result turn done");
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
        tokio::fs::write(&transcript, content).await.expect("write transcript");
        assert_eq!(
            super::extract_transcript_title(&transcript).await.as_deref(),
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
}
