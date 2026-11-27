//! Persistent `agy --input-format stream-json --output-format stream-json` worker pool.
//!
//! Keeps warm `agy` processes alive across turns to eliminate cold-start latency (~4s) and
//! server-restart `SYSTEM_MESSAGE` re-injection while strictly bounding RSS on low-RAM VPS hosts
//! (`active + idle <= max_concurrent_agy`).

use super::protocol::{
    ExecutionResult, RawAgyJsonEvent, StreamEvent, StreamResultData, TokenUsage,
    normalize_conversation_id,
};
use super::watchdog::{
    ActivityTracker, TimeoutReason, WatchdogProbeKind, WatchdogState, inspect_process_tree,
    signal_process_tree,
};
use super::{
    DETACH_GRACE_BACKGROUND, DETACH_GRACE_IDLE, DetachDecision, ExecutionParams,
    MAX_LINE_BUF_CAPACITY, MAX_STDERR_BYTES, StreamParserState, TurnDoneData, TurnSignal,
    floor_char_boundary, format_effective_prompt_with_flag, format_timeout_messages, get_agy_path,
    resolve_extra_dirs, send_stream_events,
};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

const DEFAULT_PERSISTENT_IDLE_TIMEOUT_SECS: u64 = 180;
const DEFAULT_MAX_CONCURRENT_AGY: usize = 2;
const PERSISTENT_MIN_IDLE_TIMEOUT_SECS: u64 = 30;
const PERSISTENT_INIT_TIMEOUT: Duration = Duration::from_secs(30);
const PERSISTENT_REAPER_POLL_INTERVAL: Duration = Duration::from_secs(15);
const PERSISTENT_EOF_CLOSE_GRACE: Duration = Duration::from_secs(2);
const PERSISTENT_SIGINT_CLOSE_GRACE: Duration = Duration::from_secs(2);
const PERSISTENT_CANCEL_SIGINT_GRACE: Duration = Duration::from_secs(5);

static PERSISTENT_IDLE_TIMEOUT_SECS: AtomicU64 =
    AtomicU64::new(DEFAULT_PERSISTENT_IDLE_TIMEOUT_SECS);
static PERSISTENT_MAX_CONCURRENT_AGY: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_CONCURRENT_AGY);
static PERSISTENT_POOL: OnceLock<Arc<PersistentWorkerPool>> = OnceLock::new();

/// Configures the global persistent `stream-json` worker pool idle timeout and concurrency cap.
///
/// - `idle_timeout_secs == 0`: close workers immediately after every turn (no idle parking).
/// - `idle_timeout_secs > 0`: clamped to at least 30s.
pub fn configure_persistent_pool(idle_timeout_secs: u64, max_concurrent_agy: usize) {
    PERSISTENT_IDLE_TIMEOUT_SECS.store(idle_timeout_secs, Ordering::Relaxed);
    PERSISTENT_MAX_CONCURRENT_AGY.store(max_concurrent_agy, Ordering::Relaxed);
}

fn effective_idle_timeout_secs() -> u64 {
    let raw = PERSISTENT_IDLE_TIMEOUT_SECS.load(Ordering::Relaxed);
    if raw == 0 {
        0
    } else {
        raw.max(PERSISTENT_MIN_IDLE_TIMEOUT_SECS)
    }
}

fn effective_max_concurrent_agy() -> usize {
    PERSISTENT_MAX_CONCURRENT_AGY.load(Ordering::Relaxed)
}

#[inline]
pub(crate) fn max_idle_workers_for_cap(max_concurrent_agy: usize) -> usize {
    if max_concurrent_agy > 0 && max_concurrent_agy <= 2 {
        1
    } else {
        2
    }
}

#[derive(Serialize)]
struct StdinUserEvent<'a> {
    event: &'static str,
    message: StdinUserMessage<'a>,
}

#[derive(Serialize)]
struct StdinUserMessage<'a> {
    content: &'a str,
}

/// Encodes a single user turn as the exact NDJSON line expected by
/// `agy --input-format stream-json`.
pub(crate) fn encode_stdin_user_turn(content: &str) -> Result<String> {
    let payload = StdinUserEvent {
        event: "user",
        message: StdinUserMessage { content },
    };
    let mut line =
        serde_json::to_string(&payload).context("failed to serialize persistent stdin turn")?;
    line.push('\n');
    Ok(line)
}

/// Immutable launch profile identifying whether an idle worker can be reused for a chat turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WorkerLaunchProfile {
    pub workspace: String,
    pub extra_dirs: Vec<String>,
    pub model: String,
    pub effort: String,
    pub mode: String,
    pub agent: String,
    pub sandbox: bool,
    pub project: String,
    pub settings_json: Option<String>,
    pub remote_agent: bool,
}

impl WorkerLaunchProfile {
    fn from_params(params: &ExecutionParams<'_>) -> Self {
        Self {
            workspace: params.workspace.to_string(),
            extra_dirs: resolve_extra_dirs(params.workspace, params.extra_dirs),
            model: params.model.trim().to_string(),
            effort: params.effort.trim().to_string(),
            mode: params.mode.trim().to_string(),
            agent: params.agent.trim().to_string(),
            sandbox: params.sandbox,
            project: params.project.trim().to_string(),
            settings_json: params
                .settings_json
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            remote_agent: params.remote_agent,
        }
    }
}

/// Diffs cumulative `result.usage` / `result.num_turns` / `result.duration_seconds` across turns
/// on one persistent worker so each turn emits accurate per-turn deltas (`usage_is_cumulative: false`).
#[derive(Debug, Default, Clone)]
pub(crate) struct PersistentCumulativeTracker {
    prev_usage: Option<TokenUsage>,
    prev_num_turns: u64,
    prev_duration_seconds: f64,
}

impl PersistentCumulativeTracker {
    #[inline]
    fn diff_u64(cur: u64, prev: u64) -> u64 {
        if cur >= prev { cur - prev } else { cur }
    }

    pub(crate) fn diff_usage(&mut self, cumulative: Option<TokenUsage>) -> Option<TokenUsage> {
        let cum = cumulative?;
        let delta = if let Some(ref prev) = self.prev_usage {
            TokenUsage {
                input_tokens: Self::diff_u64(cum.input_tokens, prev.input_tokens),
                output_tokens: Self::diff_u64(cum.output_tokens, prev.output_tokens),
                thinking_tokens: Self::diff_u64(cum.thinking_tokens, prev.thinking_tokens),
                cache_read_tokens: Self::diff_u64(cum.cache_read_tokens, prev.cache_read_tokens),
                total_tokens: Self::diff_u64(cum.total_tokens, prev.total_tokens),
            }
        } else {
            cum.clone()
        };
        self.prev_usage = Some(cum);
        Some(delta)
    }

    pub(crate) fn diff_num_turns(&mut self, cumulative: Option<u64>) -> Option<u64> {
        let cum = cumulative?;
        let delta = Self::diff_u64(cum, self.prev_num_turns);
        self.prev_num_turns = cum;
        Some(delta)
    }

    pub(crate) fn diff_duration(&mut self, cumulative: Option<f64>) -> Option<f64> {
        let cum = cumulative?;
        if !cum.is_finite() || cum < 0.0 {
            return None;
        }
        let delta = if cum >= self.prev_duration_seconds {
            cum - self.prev_duration_seconds
        } else {
            cum
        };
        self.prev_duration_seconds = cum;
        Some(delta)
    }
}

struct SharedStderrState {
    buf: StdMutex<String>,
    tracker: StdMutex<Option<ActivityTracker>>,
}

impl SharedStderrState {
    fn new() -> Self {
        Self {
            buf: StdMutex::new(String::new()),
            tracker: StdMutex::new(None),
        }
    }

    fn reset_for_turn(&self, tracker: ActivityTracker) {
        match self.buf.lock() {
            Ok(mut g) => g.clear(),
            Err(p) => p.into_inner().clear(),
        }
        match self.tracker.lock() {
            Ok(mut g) => *g = Some(tracker),
            Err(p) => *p.into_inner() = Some(tracker),
        }
    }

    fn detach_tracker(&self) {
        match self.tracker.lock() {
            Ok(mut g) => *g = None,
            Err(p) => *p.into_inner() = None,
        }
    }

    fn snapshot(&self) -> String {
        match self.buf.lock() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

fn spawn_persistent_stderr_drain(
    stderr: ChildStderr,
    state: Arc<SharedStderrState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line_buf: Vec<u8> = Vec::with_capacity(1024);
        loop {
            line_buf.clear();
            let bytes_read = match reader.read_until(b'\n', &mut line_buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) => {
                    debug!(error = %err, "persistent agy stderr reader closed");
                    break;
                }
            };
            if let Ok(g) = state.tracker.lock()
                && let Some(ref tracker) = *g
            {
                tracker.record_activity();
            }
            let line = String::from_utf8_lossy(&line_buf);
            let trimmed_end = line.trim_end_matches(['\r', '\n']);
            if !trimmed_end.is_empty() {
                debug!(stderr = %trimmed_end, "persistent agy stderr");
                let mut buf = match state.buf.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                if buf.len() < MAX_STDERR_BYTES {
                    let remaining = MAX_STDERR_BYTES.saturating_sub(buf.len());
                    if trimmed_end.len() < remaining {
                        buf.push_str(trimmed_end);
                        buf.push('\n');
                    } else {
                        let cut = floor_char_boundary(trimmed_end, remaining);
                        buf.push_str(&trimmed_end[..cut]);
                        buf.push_str("\n...[stderr truncated]\n");
                    }
                }
            }
            if line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                line_buf = Vec::with_capacity(4096);
            }
            if bytes_read == 0 {
                break;
            }
        }
    })
}

pub(crate) fn build_persistent_command(params: &ExecutionParams<'_>) -> Command {
    let mut cmd = Command::new(get_agy_path());
    cmd.kill_on_drop(true);
    cmd.process_group(0);
    cmd.env("PYTHONUNBUFFERED", "1");
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd.env("LANG", "C.UTF-8");
    cmd.env("LC_ALL", "C.UTF-8");

    // CRITICAL: NEVER pass `-p` / `--print` with `--input-format stream-json`.
    cmd.arg("--input-format")
        .arg("stream-json")
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

    let mode_str = params.mode.trim();
    if !mode_str.is_empty() && crate::models::is_supported_mode(mode_str) {
        cmd.arg("--mode").arg(mode_str);
    }

    let agent_str = params.agent.trim();
    if !agent_str.is_empty() && crate::models::is_safe_free_value(agent_str) {
        cmd.arg("--agent").arg(agent_str);
    }

    if params.sandbox {
        cmd.arg("--sandbox");
    }

    let project_str = params.project.trim();
    if !project_str.is_empty() && crate::models::is_safe_free_value(project_str) {
        cmd.arg("--project").arg(project_str);
    }

    if let Some(ref settings) = params.settings_json {
        let trimmed = settings.trim();
        if !trimmed.is_empty() {
            cmd.arg("--settings").arg(trimmed);
        }
    }

    if params.remote_agent {
        cmd.arg("--remote-agent");
    }

    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

struct PersistentWorker {
    child: Child,
    pid: Option<u32>,
    stdin: Option<ChildStdin>,
    stdout_reader: BufReader<ChildStdout>,
    line_buf: Vec<u8>,
    stderr_state: Arc<SharedStderrState>,
    stderr_task: tokio::task::JoinHandle<()>,
    conversation_id: Option<String>,
    cumulative: PersistentCumulativeTracker,
    cleanup_on_drop: bool,
}

impl Drop for PersistentWorker {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        self.stderr_state.detach_tracker();
        self.stderr_task.abort();
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGKILL);
        }
    }
}

impl PersistentWorker {
    async fn spawn_and_wait_init(
        mut cmd: Command,
        fallback_conv_id: Option<&str>,
        turn_cancel: Option<&CancellationToken>,
        init_timeout: Duration,
    ) -> std::result::Result<Self, WorkerSpawnFailure> {
        if turn_cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(WorkerSpawnFailure::Cancelled);
        }

        let mut child = cmd
            .spawn()
            .context("failed to spawn persistent `agy --input-format stream-json`")
            .map_err(WorkerSpawnFailure::Failed)?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .context("failed to capture persistent agy stdin")
            .map_err(WorkerSpawnFailure::Failed)?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture persistent agy stdout")
            .map_err(WorkerSpawnFailure::Failed)?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture persistent agy stderr")
            .map_err(WorkerSpawnFailure::Failed)?;

        let stderr_state = Arc::new(SharedStderrState::new());
        let stderr_task = spawn_persistent_stderr_drain(stderr, Arc::clone(&stderr_state));

        let mut worker = Self {
            child,
            pid,
            stdin: Some(stdin),
            stdout_reader: BufReader::new(stdout),
            line_buf: Vec::with_capacity(4096),
            stderr_state,
            stderr_task,
            conversation_id: normalize_conversation_id(fallback_conv_id.map(str::to_string)),
            cumulative: PersistentCumulativeTracker::default(),
            cleanup_on_drop: true,
        };

        let deadline = tokio::time::Instant::now() + init_timeout;
        loop {
            let read_res = tokio::select! {
                biased;
                () = wait_cancelled_ref(turn_cancel), if turn_cancel.is_some() => {
                    worker.kill_immediately().await;
                    return Err(WorkerSpawnFailure::Cancelled);
                }
                () = tokio::time::sleep_until(deadline) => {
                    let stderr_tail = worker.stderr_state.snapshot();
                    worker.kill_immediately().await;
                    return Err(WorkerSpawnFailure::Failed(anyhow::anyhow!(
                        "persistent agy startup init timed out after {}s (stderr: {})",
                        init_timeout.as_secs(),
                        stderr_tail.trim()
                    )));
                }
                res = worker.stdout_reader.read_until(b'\n', &mut worker.line_buf) => res,
            };

            let bytes_read = match read_res {
                Ok(0) => {
                    if worker.line_buf.is_empty() {
                        let stderr_tail = worker.stderr_state.snapshot();
                        worker.kill_immediately().await;
                        return Err(WorkerSpawnFailure::Failed(anyhow::anyhow!(
                            "persistent agy exited before startup init (stderr: {})",
                            stderr_tail.trim()
                        )));
                    }
                    0
                }
                Ok(n) => n,
                Err(err) => {
                    worker.kill_immediately().await;
                    return Err(WorkerSpawnFailure::Failed(
                        anyhow::Error::from(err)
                            .context("error reading persistent agy startup init"),
                    ));
                }
            };

            let parsed_init = {
                let line = String::from_utf8_lossy(&worker.line_buf);
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    match serde_json::from_str::<RawAgyJsonEvent>(trimmed) {
                        Ok(RawAgyJsonEvent::Init {
                            conversation_id,
                            init,
                        }) => {
                            if let Some(ref meta) = init {
                                debug!(
                                    pid = ?worker.pid,
                                    conversation_id = ?conversation_id,
                                    cwd = ?meta.cwd,
                                    model = ?meta.model,
                                    agent = ?meta.agent,
                                    permission_mode = ?meta.permission_mode,
                                    tool_count = meta.tools.len(),
                                    "persistent agy stream-json init"
                                );
                            }
                            Some(Ok(conversation_id))
                        }
                        Ok(RawAgyJsonEvent::Result { result }) => {
                            let err_msg = result
                                .error
                                .or(result.response)
                                .unwrap_or_else(|| "persistent agy failed during init".to_string());
                            Some(Err(err_msg))
                        }
                        _ => None,
                    }
                }
            };

            worker.line_buf.clear();
            if worker.line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                worker.line_buf = Vec::with_capacity(8192);
            }

            match parsed_init {
                Some(Ok(cid)) => {
                    if let Some(clean_cid) = normalize_conversation_id(cid) {
                        worker.conversation_id = Some(clean_cid);
                    }
                    info!(
                        pid = ?worker.pid,
                        conversation_id = ?worker.conversation_id,
                        "initialized persistent agy stream-json worker"
                    );
                    return Ok(worker);
                }
                Some(Err(err_msg)) => {
                    worker.kill_immediately().await;
                    return Err(WorkerSpawnFailure::Failed(anyhow::anyhow!(
                        "persistent agy init returned error: {err_msg}"
                    )));
                }
                None if bytes_read == 0 => {
                    let stderr_tail = worker.stderr_state.snapshot();
                    worker.kill_immediately().await;
                    return Err(WorkerSpawnFailure::Failed(anyhow::anyhow!(
                        "persistent agy closed stdout before init (stderr: {})",
                        stderr_tail.trim()
                    )));
                }
                None => {}
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        self.stdin.is_some() && matches!(self.child.try_wait(), Ok(None))
    }

    async fn close_gracefully(mut self) {
        self.cleanup_on_drop = false;
        self.stderr_state.detach_tracker();
        // 1. Drop stdin to send EOF -> agy flushes & exits rc=0 in ~0.9s.
        drop(self.stdin.take());
        if tokio::time::timeout(PERSISTENT_EOF_CLOSE_GRACE, self.child.wait())
            .await
            .is_ok()
        {
            self.stderr_task.abort();
            return;
        }
        // 2. Escalate to SIGINT -> wait 2s -> SIGKILL.
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGINT);
        }
        if tokio::time::timeout(PERSISTENT_SIGINT_CLOSE_GRACE, self.child.wait())
            .await
            .is_ok()
        {
            self.stderr_task.abort();
            return;
        }
        self.kill_immediately().await;
    }

    async fn interrupt_and_close(mut self, grace: Duration) {
        self.cleanup_on_drop = false;
        self.stderr_state.detach_tracker();
        drop(self.stdin.take());
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGINT);
        }
        if tokio::time::timeout(grace, self.child.wait()).await.is_ok() {
            self.stderr_task.abort();
            return;
        }
        self.kill_immediately().await;
    }

    async fn kill_immediately(&mut self) {
        self.cleanup_on_drop = false;
        self.stderr_state.detach_tracker();
        drop(self.stdin.take());
        if let Some(pid) = self.pid {
            signal_process_tree(pid, libc::SIGKILL);
        }
        let _ = self.child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
        self.stderr_task.abort();
    }
}

enum WorkerSpawnFailure {
    Cancelled,
    Failed(anyhow::Error),
}

struct PooledWorker {
    chat_id: i64,
    conversation_id: Option<String>,
    profile: WorkerLaunchProfile,
    last_used: Instant,
    worker: PersistentWorker,
}

/// Global persistent worker pool with strict low-RAM budget enforcement.
pub(crate) struct PersistentWorkerPool {
    idle_workers: AsyncMutex<VecDeque<PooledWorker>>,
    active_persistent: AtomicUsize,
    external_active: AtomicUsize,
}

impl PersistentWorkerPool {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            idle_workers: AsyncMutex::new(VecDeque::new()),
            active_persistent: AtomicUsize::new(0),
            external_active: AtomicUsize::new(0),
        })
    }

    fn global() -> Arc<Self> {
        Arc::clone(PERSISTENT_POOL.get_or_init(|| {
            let pool = Self::new();
            Self::spawn_idle_reaper(Arc::clone(&pool));
            pool
        }))
    }

    fn spawn_idle_reaper(pool: Arc<Self>) {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut ticker = tokio::time::interval(PERSISTENT_REAPER_POLL_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let timeout_secs = effective_idle_timeout_secs();
                    let stale = {
                        let mut guard = pool.idle_workers.lock().await;
                        let mut evicted = Vec::new();
                        let mut idx = 0;
                        while idx < guard.len() {
                            let expired = timeout_secs == 0
                                || guard[idx].last_used.elapsed()
                                    >= Duration::from_secs(timeout_secs);
                            let dead = !guard[idx].worker.is_alive();
                            if expired || dead {
                                if let Some(entry) = guard.remove(idx) {
                                    evicted.push(entry);
                                }
                            } else {
                                idx += 1;
                            }
                        }
                        evicted
                    };
                    for entry in stale {
                        info!(
                            chat_id = entry.chat_id,
                            pid = ?entry.worker.pid,
                            idle_secs = entry.last_used.elapsed().as_secs(),
                            "reaping idle persistent agy worker to reclaim memory"
                        );
                        entry.worker.close_gracefully().await;
                    }
                }
            });
        }
    }

    fn total_active(&self) -> usize {
        self.active_persistent
            .load(Ordering::Acquire)
            .saturating_add(self.external_active.load(Ordering::Acquire))
    }

    /// Evicts LRU idle workers synchronously so `total_active + additional_spawns + idle <= max_concurrent_agy`.
    async fn evict_idle_to_fit(&self, additional_spawns: usize) {
        let max_c = effective_max_concurrent_agy();
        if max_c == 0 {
            return;
        }
        let needed_active = self.total_active().saturating_add(additional_spawns);
        let allowed_idle = max_c.saturating_sub(needed_active);
        let to_close = {
            let mut guard = self.idle_workers.lock().await;
            let mut evicted = Vec::new();
            while guard.len() > allowed_idle {
                if let Some(entry) = guard.pop_front() {
                    evicted.push(entry);
                }
            }
            evicted
        };
        for entry in to_close {
            info!(
                chat_id = entry.chat_id,
                pid = ?entry.worker.pid,
                max_concurrent_agy = max_c,
                "evicting LRU idle persistent agy worker to preserve RAM budget"
            );
            entry.worker.close_gracefully().await;
        }
    }

    /// Evicts any idle worker belonging to `chat_id` or bound to `conv_id`.
    async fn evict_matching(&self, chat_id: i64, conv_id: Option<&str>) {
        let target_conv = conv_id.map(str::trim).filter(|s| !s.is_empty());
        let to_close = {
            let mut guard = self.idle_workers.lock().await;
            let mut evicted = Vec::new();
            let mut idx = 0;
            while idx < guard.len() {
                let matches_chat = guard[idx].chat_id == chat_id;
                let matches_conv =
                    target_conv.is_some() && guard[idx].conversation_id.as_deref() == target_conv;
                if matches_chat || matches_conv {
                    if let Some(entry) = guard.remove(idx) {
                        evicted.push(entry);
                    }
                } else {
                    idx += 1;
                }
            }
            evicted
        };
        for entry in to_close {
            debug!(
                chat_id = entry.chat_id,
                pid = ?entry.worker.pid,
                "evicting persistent agy worker"
            );
            entry.worker.close_gracefully().await;
        }
    }

    /// Tries to take an existing warm idle worker matching `(chat_id, conv_id, profile)`.
    /// Any mismatched/dead worker for `chat_id` or conflicting `conv_id` is synchronously closed.
    async fn take_warm_or_evict_conflicts(
        &self,
        chat_id: i64,
        requested_conv_id: Option<&str>,
        profile: &WorkerLaunchProfile,
    ) -> Option<PersistentWorker> {
        let req_conv = requested_conv_id.map(str::trim).filter(|s| !s.is_empty());
        let (matched, to_close) = {
            let mut guard = self.idle_workers.lock().await;
            let mut found: Option<PersistentWorker> = None;
            let mut evicted: Vec<PooledWorker> = Vec::new();
            let mut idx = 0;
            while idx < guard.len() {
                let same_chat = guard[idx].chat_id == chat_id;
                let same_conv =
                    req_conv.is_some() && guard[idx].conversation_id.as_deref() == req_conv;
                if same_chat {
                    if let Some(mut entry) = guard.remove(idx) {
                        if req_conv.is_some()
                            && entry.conversation_id.as_deref() == req_conv
                            && &entry.profile == profile
                            && entry.worker.is_alive()
                            && found.is_none()
                        {
                            found = Some(entry.worker);
                        } else {
                            evicted.push(entry);
                        }
                    }
                } else if same_conv {
                    // Never allow two agy processes on the same conversation_id.
                    if let Some(entry) = guard.remove(idx) {
                        evicted.push(entry);
                    }
                } else {
                    idx += 1;
                }
            }
            (found, evicted)
        };

        for entry in to_close {
            debug!(
                chat_id = entry.chat_id,
                pid = ?entry.worker.pid,
                "closing stale or conflicting persistent worker before turn"
            );
            entry.worker.close_gracefully().await;
        }

        matched
    }

    /// Parks a healthy worker back into the idle pool, enforcing `idle_timeout_secs`,
    /// `max_idle_workers`, and `active + idle <= max_concurrent_agy`.
    async fn return_worker(
        &self,
        chat_id: i64,
        profile: WorkerLaunchProfile,
        mut worker: PersistentWorker,
    ) {
        let idle_timeout_secs = effective_idle_timeout_secs();
        if idle_timeout_secs == 0 || !worker.is_alive() {
            worker.close_gracefully().await;
            return;
        }

        let max_c = effective_max_concurrent_agy();
        let max_idle_cfg = max_idle_workers_for_cap(max_c);
        let budget_idle = if max_c > 0 {
            max_c.saturating_sub(self.total_active())
        } else {
            max_idle_cfg
        };
        let cap = max_idle_cfg.min(budget_idle);
        if cap == 0 {
            debug!(
                chat_id,
                pid = ?worker.pid,
                "closing persistent worker immediately: zero idle RAM budget remaining"
            );
            worker.close_gracefully().await;
            return;
        }

        let conv_id = worker.conversation_id.clone();
        let evicted = {
            let mut guard = self.idle_workers.lock().await;
            let mut to_close = Vec::new();
            // Ensure uniqueness per chat_id and conversation_id.
            let mut idx = 0;
            while idx < guard.len() {
                let dup_chat = guard[idx].chat_id == chat_id;
                let dup_conv = conv_id.is_some() && guard[idx].conversation_id == conv_id;
                if dup_chat || dup_conv {
                    if let Some(old) = guard.remove(idx) {
                        to_close.push(old);
                    }
                } else {
                    idx += 1;
                }
            }
            guard.push_back(PooledWorker {
                chat_id,
                conversation_id: conv_id,
                profile,
                last_used: Instant::now(),
                worker,
            });
            while guard.len() > cap {
                if let Some(old) = guard.pop_front() {
                    to_close.push(old);
                }
            }
            to_close
        };

        for old in evicted {
            debug!(
                chat_id = old.chat_id,
                pid = ?old.worker.pid,
                "evicting LRU idle worker after parking newer worker"
            );
            old.worker.close_gracefully().await;
        }
    }

    async fn shutdown_all(&self) {
        let all: Vec<PooledWorker> = {
            let mut guard = self.idle_workers.lock().await;
            guard.drain(..).collect()
        };
        for entry in all {
            info!(
                chat_id = entry.chat_id,
                pid = ?entry.worker.pid,
                "closing persistent agy worker during shutdown"
            );
            entry.worker.close_gracefully().await;
        }
    }
}

struct ActivePersistentGuard {
    pool: Arc<PersistentWorkerPool>,
}

impl ActivePersistentGuard {
    fn acquire(pool: Arc<PersistentWorkerPool>) -> Self {
        pool.active_persistent.fetch_add(1, Ordering::AcqRel);
        Self { pool }
    }
}

impl Drop for ActivePersistentGuard {
    fn drop(&mut self) {
        self.pool.active_persistent.fetch_sub(1, Ordering::AcqRel);
    }
}

/// RAII guard tracking one active external (one-shot `Print`) `agy` subprocess against the
/// shared low-RAM budget so `active_persistent + external_active + idle <= max_concurrent_agy`.
pub(crate) struct ExternalProcessBudgetGuard {
    pool: Arc<PersistentWorkerPool>,
}

impl Drop for ExternalProcessBudgetGuard {
    fn drop(&mut self) {
        self.pool.external_active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Claims an external process budget slot and synchronously evicts any idle workers that would
/// exceed `max_concurrent_agy` (plus any idle worker belonging to `chat_id` or `conversation_id`).
pub(crate) async fn reserve_external_process_budget(
    chat_id: i64,
    conversation_id: Option<&str>,
) -> ExternalProcessBudgetGuard {
    let pool = PersistentWorkerPool::global();
    pool.external_active.fetch_add(1, Ordering::AcqRel);
    let guard = ExternalProcessBudgetGuard {
        pool: Arc::clone(&pool),
    };
    pool.evict_matching(chat_id, conversation_id).await;
    pool.evict_idle_to_fit(0).await;
    guard
}

/// Synchronously closes and evicts any idle persistent worker owned by `chat_id`.
pub async fn evict_chat_worker(chat_id: i64) {
    if let Some(pool) = PERSISTENT_POOL.get() {
        pool.evict_matching(chat_id, None).await;
    }
}

/// Returns `true` if `pid` belongs to a warm idle worker currently parked in the global pool.
pub async fn is_pooled_worker_pid(pid: u32) -> bool {
    let Some(pool) = PERSISTENT_POOL.get() else {
        return false;
    };
    let guard = pool.idle_workers.lock().await;
    guard.iter().any(|entry| entry.worker.pid == Some(pid))
}

/// Synchronously evicts LRU idle persistent workers to guarantee room for 1 additional `agy` spawn
/// within `max_concurrent_agy`.
pub async fn evict_idle_for_budget() {
    if let Some(pool) = PERSISTENT_POOL.get() {
        pool.evict_idle_to_fit(1).await;
    }
}

/// Cleanly closes every idle persistent worker during daemon shutdown.
pub async fn shutdown_persistent_pool() {
    if let Some(pool) = PERSISTENT_POOL.get() {
        pool.shutdown_all().await;
    }
}

async fn wait_cancelled(token: &Option<CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

async fn wait_cancelled_ref(token: Option<&CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending().await,
    }
}

fn spawn_detached_persistent_drainer(
    mut worker: PersistentWorker,
    mut parser_state: StreamParserState,
    stream_tx: mpsc::Sender<StreamEvent>,
    remaining_timeout: Duration,
    active_guard: Option<ActivePersistentGuard>,
) {
    // Drop stdin immediately so agy finishes in-flight background follow-ups and auto-exits rc=0.
    drop(worker.stdin.take());
    tokio::spawn(async move {
        let _active_guard = active_guard;
        let mut stream_tx_opt = Some(stream_tx);
        let (_, flushed) = parser_state.apply_decision(DetachDecision::Detached);
        if !flushed.is_empty() {
            send_stream_events(&mut stream_tx_opt, flushed).await;
        }

        let deadline = tokio::time::Instant::now() + remaining_timeout;
        let mut out_events: Vec<StreamEvent> = Vec::with_capacity(8);
        let mut out_signals: Vec<TurnSignal> = Vec::with_capacity(2);
        let mut timed_out = false;

        loop {
            let read_res = tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => {
                    timed_out = true;
                    break;
                }
                res = worker.stdout_reader.read_until(b'\n', &mut worker.line_buf) => res,
            };

            let bytes_read = match read_res {
                Ok(0) => {
                    if worker.line_buf.is_empty() {
                        break;
                    }
                    0
                }
                Ok(n) => n,
                Err(err) => {
                    warn!(pid = ?worker.pid, error = %err, "error reading detached persistent agy stdout");
                    break;
                }
            };

            {
                let line = String::from_utf8_lossy(&worker.line_buf);
                parser_state.process_line(
                    line.as_ref(),
                    DetachDecision::Detached,
                    &mut out_events,
                    &mut out_signals,
                );
            }
            worker.line_buf.clear();
            if worker.line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                worker.line_buf = Vec::with_capacity(8192);
            }
            out_signals.clear();
            if !out_events.is_empty() {
                send_stream_events(&mut stream_tx_opt, out_events.drain(..)).await;
            }
            if bytes_read == 0 {
                break;
            }
        }

        let (eof_event, _) = parser_state.finish_eof();
        if let Some(ev) = eof_event {
            send_stream_events(&mut stream_tx_opt, std::iter::once(ev)).await;
        }
        drop(stream_tx_opt);

        if timed_out {
            warn!(
                pid = ?worker.pid,
                "detached persistent agy exceeded remaining execution timeout; killing process group"
            );
            worker.kill_immediately().await;
        } else {
            worker.close_gracefully().await;
        }
    });
}

enum TurnRunOutcome {
    /// Turn finished cleanly; worker can be returned to the idle pool.
    Completed {
        result: ExecutionResult,
        worker: Box<PersistentWorker>,
    },
    /// Turn finished, detached, timed out, or cancelled; worker already closed or handed off.
    Terminal(ExecutionResult),
    /// Warm worker died/EOF'd before emitting any stdout event — caller should transparently retry once.
    WarmFailedBeforeOutput(anyhow::Error),
}

struct WorkerTurnOpts<'a> {
    effective_prompt: &'a str,
    sys_prompt_injected: bool,
    is_warm: bool,
    stream_tx: mpsc::Sender<StreamEvent>,
    pid_tx: &'a mut Option<oneshot::Sender<u32>>,
    active_guard: &'a mut Option<ActivePersistentGuard>,
    idle_timeout_secs: u64,
    max_execution_secs: u64,
    turn_cancel: Option<CancellationToken>,
}

async fn execute_turn_on_worker(
    mut worker: PersistentWorker,
    opts: WorkerTurnOpts<'_>,
) -> TurnRunOutcome {
    let WorkerTurnOpts {
        effective_prompt,
        sys_prompt_injected,
        is_warm,
        stream_tx,
        pid_tx,
        active_guard,
        idle_timeout_secs,
        max_execution_secs,
        turn_cancel,
    } = opts;
    if turn_cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        let conv = worker.conversation_id.clone();
        worker
            .interrupt_and_close(PERSISTENT_CANCEL_SIGINT_GRACE)
            .await;
        return TurnRunOutcome::Terminal(cancelled_result(conv, 0.0));
    }

    let tracker = ActivityTracker::new();
    worker.stderr_state.reset_for_turn(tracker.clone());

    let encoded_line = match encode_stdin_user_turn(effective_prompt) {
        Ok(line) => line,
        Err(err) => {
            worker.close_gracefully().await;
            return TurnRunOutcome::WarmFailedBeforeOutput(err);
        }
    };

    let write_res = match worker.stdin.as_mut() {
        Some(stdin) => match stdin.write_all(encoded_line.as_bytes()).await {
            Ok(()) => stdin.flush().await,
            Err(e) => Err(e),
        },
        None => Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "persistent worker stdin already closed",
        )),
    };
    if let Err(err) = write_res {
        worker.kill_immediately().await;
        let err = anyhow::Error::from(err).context("failed to write turn to persistent agy stdin");
        if is_warm {
            return TurnRunOutcome::WarmFailedBeforeOutput(err);
        }
        return TurnRunOutcome::Terminal(error_result(
            worker.conversation_id.clone(),
            format!("❌ {err}"),
            0.0,
        ));
    }

    if !is_warm
        && let Some(pid) = worker.pid
        && let Some(tx) = pid_tx.take()
    {
        let _ = tx.send(pid);
    }

    let mut parser_state = StreamParserState::new();
    parser_state.detected_conv_id = worker.conversation_id.clone();
    let mut emitted_init = false;
    let mut streamed_any_stdout = false;
    let mut saw_turn_output = false;
    let mut stream_tx_opt = Some(stream_tx);
    let mut out_events: Vec<StreamEvent> = Vec::with_capacity(8);
    let mut out_signals: Vec<TurnSignal> = Vec::with_capacity(2);

    let idle_timeout = Duration::from_secs(idle_timeout_secs.max(30));
    let max_exec_timeout = Duration::from_secs(max_execution_secs.max(60));
    let probe_interval = Duration::from_secs(2);
    let mut probe_ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + probe_interval, probe_interval);
    probe_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let start_time = Instant::now();
    let mut watchdog = WatchdogState::new();

    let mut pending_turn1: Option<(TurnDoneData, tokio::time::Instant)> = None;
    let mut final_result_data: Option<TurnDoneData> = None;
    let mut turn_detached_data: Option<TurnDoneData> = None;
    let mut timeout_reason: Option<TimeoutReason> = None;
    let mut cancelled = false;
    let mut saw_eof = false;

    loop {
        let grace_sleep = async {
            match pending_turn1 {
                Some((_, deadline)) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;
            () = wait_cancelled(&turn_cancel), if turn_cancel.is_some() => {
                cancelled = true;
                break;
            }
            () = grace_sleep, if pending_turn1.is_some() => {
                if let Some((done_data, _)) = pending_turn1.take() {
                    if done_data.has_background_activity {
                        info!(
                            pid = ?worker.pid,
                            "active background subagent/task detected on persistent worker; detaching turn"
                        );
                    } else {
                        info!(
                            pid = ?worker.pid,
                            "persistent worker post-turn cleanup exceeded detach grace; delivering early"
                        );
                    }
                    turn_detached_data = Some(done_data);
                    break;
                }
            }
            read_res = worker.stdout_reader.read_until(b'\n', &mut worker.line_buf) => {
                let bytes_read = match read_res {
                    Ok(0) => {
                        if worker.line_buf.is_empty() {
                            saw_eof = true;
                            break;
                        }
                        0
                    }
                    Ok(n) => n,
                    Err(err) => {
                        warn!(pid = ?worker.pid, error = %err, "error reading persistent agy stdout");
                        saw_eof = true;
                        break;
                    }
                };

                streamed_any_stdout = true;
                tracker.record_activity();

                {
                    let line = String::from_utf8_lossy(&worker.line_buf);
                    parser_state.process_line(
                        line.as_ref(),
                        DetachDecision::Pending,
                        &mut out_events,
                        &mut out_signals,
                    );
                }

                worker.line_buf.clear();
                if worker.line_buf.capacity() > MAX_LINE_BUF_CAPACITY {
                    worker.line_buf = Vec::with_capacity(8192);
                }

                if out_events
                    .iter()
                    .any(|ev| !matches!(ev, StreamEvent::Init { .. }))
                {
                    saw_turn_output = true;
                }

                let immediate_warm_error = is_warm
                    && !saw_turn_output
                    && out_signals.iter().any(|sig| {
                        matches!(
                            sig,
                            TurnSignal::FinalResult(done)
                                if !done.final_status.eq_ignore_ascii_case("SUCCESS")
                        )
                    });

                if !emitted_init && !immediate_warm_error {
                    emitted_init = true;
                    if let Some(pid) = worker.pid
                        && let Some(tx) = pid_tx.take()
                    {
                        let _ = tx.send(pid);
                    }
                    if let Some(ref cid) = worker.conversation_id {
                        send_stream_events(
                            &mut stream_tx_opt,
                            std::iter::once(StreamEvent::Init {
                                conversation_id: cid.clone(),
                            }),
                        )
                        .await;
                    }
                }

                if !out_events.is_empty() {
                    send_stream_events(&mut stream_tx_opt, out_events.drain(..)).await;
                }

                for sig in out_signals.drain(..) {
                    match sig {
                        TurnSignal::FinalResult(done_data) => {
                            final_result_data = Some(done_data);
                            break;
                        }
                        TurnSignal::ToolResumed => {
                            debug!(pid = ?worker.pid, "foreground tool resumed during persistent detach grace");
                            pending_turn1 = None;
                        }
                        TurnSignal::Turn1Done(done_data) => {
                            saw_turn_output = true;
                            let grace = if done_data.has_background_activity {
                                DETACH_GRACE_BACKGROUND
                            } else {
                                DETACH_GRACE_IDLE
                            };
                            info!(
                                pid = ?worker.pid,
                                grace_ms = grace.as_millis(),
                                has_background = done_data.has_background_activity,
                                "persistent agent turn completed; awaiting final result or detachment"
                            );
                            pending_turn1 = Some((done_data, tokio::time::Instant::now() + grace));
                        }
                    }
                }

                if final_result_data.is_some() {
                    break;
                }
                if bytes_read == 0 {
                    saw_eof = true;
                    break;
                }
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

                let Some(pid) = worker.pid else {
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
                    warn!(pid, "persistent worker process tree inspection task failed");
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

    worker.stderr_state.detach_tracker();

    if cancelled {
        let conv_id = normalize_conversation_id(
            parser_state
                .detected_conv_id
                .or_else(|| worker.conversation_id.clone()),
        );
        info!(pid = ?worker.pid, "cancelling persistent agy worker via SIGINT and evicting");
        worker
            .interrupt_and_close(PERSISTENT_CANCEL_SIGINT_GRACE)
            .await;
        return TurnRunOutcome::Terminal(cancelled_result(
            conv_id,
            start_time.elapsed().as_secs_f64(),
        ));
    }

    if let Some(reason) = timeout_reason {
        warn!(pid = ?worker.pid, reason = ?reason, "persistent agy watchdog triggered; killing worker");
        let conv_id = normalize_conversation_id(
            parser_state
                .detected_conv_id
                .or_else(|| worker.conversation_id.clone()),
        );
        worker.kill_immediately().await;
        let (error_event, summary_msg) = format_timeout_messages(reason);
        send_stream_events(
            &mut stream_tx_opt,
            std::iter::once(StreamEvent::Error(error_event)),
        )
        .await;
        return TurnRunOutcome::Terminal(ExecutionResult {
            output: summary_msg,
            conversation_id: conv_id,
            status: "TIMEOUT".to_string(),
            usage: None,
            usage_is_cumulative: false,
            system_prompt_sent: false,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            duration_seconds: Some(start_time.elapsed().as_secs_f64()),
            detached: false,
            finished_reason: Some("timeout".to_string()),
            structured_output: None,
        });
    }

    if let Some(final_data) = final_result_data {
        let succeeded = final_data.final_status.eq_ignore_ascii_case("SUCCESS");
        if is_warm && !succeeded && !saw_turn_output {
            let reason = final_data
                .final_finished_reason
                .as_deref()
                .unwrap_or("unknown");
            let detail = final_data.accumulated_text.trim().to_string();
            worker.close_gracefully().await;
            return TurnRunOutcome::WarmFailedBeforeOutput(anyhow::anyhow!(
                "warm persistent agy returned status={} (reason={}) before producing any turn output: {}",
                final_data.final_status,
                reason,
                detail
            ));
        }

        let conv_id = normalize_conversation_id(
            final_data
                .conversation_id
                .or_else(|| worker.conversation_id.clone()),
        );
        if let Some(ref cid) = conv_id {
            worker.conversation_id = Some(cid.clone());
        }

        let delta_usage = worker.cumulative.diff_usage(final_data.final_usage);
        let delta_turns = worker.cumulative.diff_num_turns(final_data.num_turns);
        let delta_duration = worker
            .cumulative
            .diff_duration(final_data.final_duration)
            .or_else(|| Some(start_time.elapsed().as_secs_f64()));

        let mut final_output = final_data.accumulated_text.trim().to_string();
        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }

        send_stream_events(
            &mut stream_tx_opt,
            std::iter::once(StreamEvent::Result(Box::new(StreamResultData {
                conversation_id: conv_id.clone(),
                response: final_output.clone(),
                status: final_data.final_status.clone(),
                usage: delta_usage.clone(),
                context_tokens: final_data.last_context_tokens,
                num_turns: delta_turns,
                saw_checkpoint: final_data.saw_checkpoint,
                duration_seconds: delta_duration,
                finished_reason: final_data.final_finished_reason.clone(),
                structured_output: final_data.final_structured_output.clone(),
            }))),
        )
        .await;

        let res = ExecutionResult {
            output: final_output,
            conversation_id: conv_id,
            status: final_data.final_status,
            usage: delta_usage,
            usage_is_cumulative: false,
            system_prompt_sent: sys_prompt_injected && succeeded,
            context_tokens: final_data.last_context_tokens,
            num_turns: delta_turns,
            saw_checkpoint: final_data.saw_checkpoint,
            duration_seconds: delta_duration,
            detached: false,
            finished_reason: final_data.final_finished_reason,
            structured_output: final_data.final_structured_output,
        };

        // Keep worker warm only if it succeeded and is still alive; evict on ERROR so corrupted
        // CLI state never poisons subsequent turns.
        if succeeded && worker.is_alive() {
            return TurnRunOutcome::Completed {
                result: res,
                worker: Box::new(worker),
            };
        }
        worker.close_gracefully().await;
        return TurnRunOutcome::Terminal(res);
    }

    if let Some(detached) = turn_detached_data {
        let conv_id = normalize_conversation_id(
            detached
                .conversation_id
                .or_else(|| worker.conversation_id.clone()),
        );
        let mut final_output = detached.accumulated_text.trim().to_string();
        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }
        let turn_dur = Some(start_time.elapsed().as_secs_f64());
        let succeeded = detached.final_status.eq_ignore_ascii_case("SUCCESS");

        // Emit Turn-1 Result before handing the worker off to the detached drainer so follow-up
        // events remain strictly ordered after Turn-1 Result.
        if let Some(tx) = stream_tx_opt.take() {
            let _ = tx
                .send(StreamEvent::Result(Box::new(StreamResultData {
                    conversation_id: conv_id.clone(),
                    response: final_output.clone(),
                    status: detached.final_status.clone(),
                    usage: detached.final_usage.clone(),
                    context_tokens: detached.last_context_tokens,
                    num_turns: detached.num_turns,
                    saw_checkpoint: detached.saw_checkpoint,
                    duration_seconds: turn_dur,
                    finished_reason: detached.final_finished_reason.clone(),
                    structured_output: detached.final_structured_output.clone(),
                })))
                .await;
            let remaining_timeout = max_exec_timeout
                .saturating_sub(start_time.elapsed())
                .max(Duration::from_secs(60));
            spawn_detached_persistent_drainer(
                worker,
                parser_state,
                tx,
                remaining_timeout,
                active_guard.take(),
            );
        } else {
            worker.close_gracefully().await;
        }

        return TurnRunOutcome::Terminal(ExecutionResult {
            output: final_output,
            conversation_id: conv_id,
            status: detached.final_status,
            usage: detached.final_usage,
            usage_is_cumulative: false,
            system_prompt_sent: sys_prompt_injected && succeeded,
            context_tokens: detached.last_context_tokens,
            num_turns: detached.num_turns,
            saw_checkpoint: detached.saw_checkpoint,
            duration_seconds: turn_dur,
            detached: true,
            finished_reason: detached.final_finished_reason,
            structured_output: detached.final_structured_output,
        });
    }

    // Unexpected EOF / process exit before Result.
    let exit_status = tokio::time::timeout(Duration::from_secs(2), worker.child.wait())
        .await
        .ok()
        .and_then(|r| r.ok());
    let stderr_text = worker.stderr_state.snapshot();
    worker.kill_immediately().await;

    if is_warm && !streamed_any_stdout && saw_eof {
        return TurnRunOutcome::WarmFailedBeforeOutput(anyhow::anyhow!(
            "warm persistent agy exited unexpectedly before producing output (status={:?}, stderr={})",
            exit_status.and_then(|s| s.code()),
            stderr_text.trim()
        ));
    }

    let (_, mut stdout_res) = parser_state.finish_eof();
    let exit_failed = exit_status.is_none_or(|s| !s.success());
    if exit_failed {
        stdout_res.final_status = "ERROR".to_string();
        if stdout_res.final_finished_reason.is_none() {
            stdout_res.final_finished_reason = Some("process_exit_error".to_string());
        }
        warn!(
            pid = ?worker.pid,
            exit_code = ?exit_status.and_then(|s| s.code()),
            stderr = %stderr_text.trim(),
            output_empty = stdout_res.accumulated_text.trim().is_empty(),
            "persistent agy exited unexpectedly before emitting result"
        );
    }

    let conv_id = normalize_conversation_id(
        stdout_res
            .conversation_id
            .or_else(|| worker.conversation_id.clone()),
    );
    let mut final_output = stdout_res.accumulated_text.trim().to_string();
    if final_output.is_empty() && !stderr_text.trim().is_empty() {
        final_output = format!("Error:\n{}", stderr_text.trim());
    }
    if final_output.is_empty() {
        final_output = if exit_failed {
            "❌ persistent agy exited unexpectedly with no output.".to_string()
        } else {
            "Task completed with no text output.".to_string()
        };
    }

    let succeeded = stdout_res.final_status.eq_ignore_ascii_case("SUCCESS");
    let elapsed = Some(start_time.elapsed().as_secs_f64());

    send_stream_events(
        &mut stream_tx_opt,
        std::iter::once(StreamEvent::Result(Box::new(StreamResultData {
            conversation_id: conv_id.clone(),
            response: final_output.clone(),
            status: stdout_res.final_status.clone(),
            usage: stdout_res.final_usage.clone(),
            context_tokens: stdout_res.last_context_tokens,
            num_turns: stdout_res.num_turns,
            saw_checkpoint: stdout_res.saw_checkpoint,
            duration_seconds: elapsed,
            finished_reason: stdout_res.final_finished_reason.clone(),
            structured_output: stdout_res.final_structured_output.clone(),
        }))),
    )
    .await;

    TurnRunOutcome::Terminal(ExecutionResult {
        output: final_output,
        conversation_id: conv_id,
        status: stdout_res.final_status,
        usage: stdout_res.final_usage,
        usage_is_cumulative: false,
        system_prompt_sent: sys_prompt_injected && succeeded,
        context_tokens: stdout_res.last_context_tokens,
        num_turns: stdout_res.num_turns,
        saw_checkpoint: stdout_res.saw_checkpoint,
        duration_seconds: elapsed,
        detached: false,
        finished_reason: stdout_res.final_finished_reason,
        structured_output: stdout_res.final_structured_output,
    })
}

fn cancelled_result(conversation_id: Option<String>, elapsed_secs: f64) -> ExecutionResult {
    ExecutionResult {
        output: String::new(),
        conversation_id,
        status: "CANCELED".to_string(),
        usage: None,
        usage_is_cumulative: false,
        system_prompt_sent: false,
        context_tokens: None,
        num_turns: None,
        saw_checkpoint: false,
        duration_seconds: Some(elapsed_secs),
        detached: false,
        finished_reason: Some("canceled".to_string()),
        structured_output: None,
    }
}

fn error_result(
    conversation_id: Option<String>,
    output: String,
    elapsed_secs: f64,
) -> ExecutionResult {
    ExecutionResult {
        output,
        conversation_id,
        status: "ERROR".to_string(),
        usage: None,
        usage_is_cumulative: false,
        system_prompt_sent: false,
        context_tokens: None,
        num_turns: None,
        saw_checkpoint: false,
        duration_seconds: Some(elapsed_secs),
        detached: false,
        finished_reason: Some("error".to_string()),
        structured_output: None,
    }
}

/// Errors from persistent startup that allow transparent fallback to one-shot `Print`.
pub(crate) enum PersistentRunError {
    SpawnOrInitFailed(anyhow::Error),
}

/// Executes a prompt turn against the persistent `stream-json` worker pool.
///
/// - Reuses an idle worker when `(chat_id, conversation_id, launch_profile)` matches.
/// - Enforces `active + idle <= max_concurrent_agy` before any fresh spawn.
/// - If a warm worker fails before producing output, transparently retries ONCE with a fresh spawn.
pub(crate) async fn run_prompt_persistent(
    params: &ExecutionParams<'_>,
    stream_tx: mpsc::Sender<StreamEvent>,
    pid_tx: &mut Option<oneshot::Sender<u32>>,
    idle_timeout_secs: u64,
    max_execution_secs: u64,
    turn_cancel: Option<CancellationToken>,
) -> std::result::Result<ExecutionResult, PersistentRunError> {
    if turn_cancel
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Ok(cancelled_result(
            normalize_conversation_id(params.conversation_id.map(str::to_string)),
            0.0,
        ));
    }

    let pool = PersistentWorkerPool::global();
    let mut active_guard = Some(ActivePersistentGuard::acquire(Arc::clone(&pool)));
    let profile = WorkerLaunchProfile::from_params(params);
    let (effective_prompt, sys_prompt_injected) =
        format_effective_prompt_with_flag(params.prompt, params.system_prompt);

    if let Some(worker) = pool
        .take_warm_or_evict_conflicts(params.chat_id, params.conversation_id, &profile)
        .await
    {
        info!(
            chat_id = params.chat_id,
            pid = ?worker.pid,
            prompt_len = params.prompt.len(),
            "Reusing warm persistent agy stream-json worker"
        );
        let warm_opts = WorkerTurnOpts {
            effective_prompt: effective_prompt.as_ref(),
            sys_prompt_injected,
            is_warm: true,
            stream_tx: stream_tx.clone(),
            pid_tx,
            active_guard: &mut active_guard,
            idle_timeout_secs,
            max_execution_secs,
            turn_cancel: turn_cancel.clone(),
        };
        match Box::pin(execute_turn_on_worker(worker, warm_opts)).await {
            TurnRunOutcome::Completed { result, worker } => {
                drop(active_guard.take());
                pool.return_worker(params.chat_id, profile, *worker).await;
                return Ok(result);
            }
            TurnRunOutcome::Terminal(result) => return Ok(result),
            TurnRunOutcome::WarmFailedBeforeOutput(err) => {
                warn!(
                    chat_id = params.chat_id,
                    error = %err,
                    "warm persistent worker failed before producing output; retrying once with fresh spawn"
                );
            }
        }
    }

    // Fresh spawn path: enforce hard RAM budget BEFORE spawning.
    pool.evict_idle_to_fit(0).await;

    info!(
        chat_id = params.chat_id,
        first_turn = params.is_first_turn,
        prompt_len = params.prompt.len(),
        "Spawning fresh persistent agy stream-json worker"
    );
    let cmd = build_persistent_command(params);
    let fresh_worker = match PersistentWorker::spawn_and_wait_init(
        cmd,
        params.conversation_id,
        turn_cancel.as_ref(),
        PERSISTENT_INIT_TIMEOUT,
    )
    .await
    {
        Ok(w) => w,
        Err(WorkerSpawnFailure::Cancelled) => {
            return Ok(cancelled_result(
                normalize_conversation_id(params.conversation_id.map(str::to_string)),
                0.0,
            ));
        }
        Err(WorkerSpawnFailure::Failed(err)) => {
            return Err(PersistentRunError::SpawnOrInitFailed(err));
        }
    };

    let fresh_opts = WorkerTurnOpts {
        effective_prompt: effective_prompt.as_ref(),
        sys_prompt_injected,
        is_warm: false,
        stream_tx,
        pid_tx,
        active_guard: &mut active_guard,
        idle_timeout_secs,
        max_execution_secs,
        turn_cancel,
    };
    match Box::pin(execute_turn_on_worker(fresh_worker, fresh_opts)).await {
        TurnRunOutcome::Completed { result, worker } => {
            drop(active_guard.take());
            pool.return_worker(params.chat_id, profile, *worker).await;
            Ok(result)
        }
        TurnRunOutcome::Terminal(result) => Ok(result),
        TurnRunOutcome::WarmFailedBeforeOutput(err) => {
            Err(PersistentRunError::SpawnOrInitFailed(err))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ExecutorBackend;

    fn mock_persistent_script_cmd(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd.kill_on_drop(true);
        cmd.process_group(0);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    #[test]
    fn encode_stdin_user_turn_matches_verified_agy_ndjson_wire_format() {
        let line = encode_stdin_user_turn("hello\nworld").expect("encode turn");
        assert!(line.ends_with('\n'));
        let val: serde_json::Value = serde_json::from_str(line.trim()).expect("valid json");
        assert_eq!(val["event"], "user");
        assert_eq!(val["message"]["content"], "hello\nworld");
        assert!(val.get("type").is_none());
    }

    #[test]
    fn build_persistent_command_never_passes_print_flag() {
        let params = ExecutionParams {
            chat_id: 42,
            prompt: "hi",
            conversation_id: Some("conv-1"),
            is_first_turn: false,
            workspace: "/tmp",
            extra_dirs: &[],
            model: "gemini-3.8-flash-low",
            effort: "low",
            mode: "accept-edits",
            agent: "",
            system_prompt: "",
            sandbox: false,
            project: "",
            settings_json: Some(r#"{"showThinking":false}"#.to_string()),
            json_schema: None,
            remote_agent: false,
            backend: ExecutorBackend::Persistent,
        };
        let args: Vec<String> = build_persistent_command(&params)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.contains(&"-p".to_string()));
        assert!(!args.contains(&"--print".to_string()));
        assert!(
            args.windows(2)
                .any(|w| w == ["--input-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--output-format", "stream-json"])
        );
        assert!(args.windows(2).any(|w| w == ["--conversation", "conv-1"]));
        assert!(args.contains(&"--settings".to_string()));
    }

    #[test]
    fn cumulative_tracker_diffs_usage_turns_and_duration_across_turns() {
        let mut tracker = PersistentCumulativeTracker::default();

        // Turn 1: baseline
        let d1 = tracker
            .diff_usage(Some(TokenUsage {
                input_tokens: 9822,
                output_tokens: 100,
                thinking_tokens: 50,
                cache_read_tokens: 4000,
                total_tokens: 9972,
            }))
            .unwrap();
        assert_eq!(d1.input_tokens, 9822);
        assert_eq!(tracker.diff_num_turns(Some(1)), Some(1));
        assert!((tracker.diff_duration(Some(3.9)).unwrap() - 3.9).abs() < 1e-6);

        // Turn 2: cumulative 11946 -> delta 2124
        let d2 = tracker
            .diff_usage(Some(TokenUsage {
                input_tokens: 11946,
                output_tokens: 180,
                thinking_tokens: 70,
                cache_read_tokens: 10000,
                total_tokens: 12196,
            }))
            .unwrap();
        assert_eq!(d2.input_tokens, 2124);
        assert_eq!(d2.output_tokens, 80);
        assert_eq!(d2.thinking_tokens, 20);
        assert_eq!(d2.cache_read_tokens, 6000);
        assert_eq!(d2.total_tokens, 2224);
        assert_eq!(tracker.diff_num_turns(Some(2)), Some(1));
        assert!((tracker.diff_duration(Some(6.1)).unwrap() - 2.2).abs() < 1e-6);
    }

    #[test]
    fn max_idle_workers_respects_low_ram_cap() {
        assert_eq!(max_idle_workers_for_cap(1), 1);
        assert_eq!(max_idle_workers_for_cap(2), 1);
        assert_eq!(max_idle_workers_for_cap(3), 2);
        assert_eq!(max_idle_workers_for_cap(0), 2);
    }

    #[tokio::test]
    async fn mock_persistent_worker_runs_multi_turn_diffs_usage_and_evicts_cleanly() {
        // Simulates `agy --input-format stream-json --output-format stream-json`:
        // emits startup init, reads 2 NDJSON user turns with cumulative result counters, then exits 0 on EOF.
        let script = r#"
printf '{"event":"init","conversation_id":"conv-warm-1","init":{"cwd":"/tmp","model":"mock"}}\n'
turn=0
while IFS= read -r _line; do
  turn=$((turn + 1))
  if [ "$turn" -eq 1 ]; then
    printf '{"event":"step_update","step_update":{"step_index":1,"step_type":"agent_response","state":"DONE","text_delta":"reply-1","usage":{"input_tokens":500,"cache_read_tokens":1000,"output_tokens":20}}}\n'
    printf '{"event":"result","result":{"conversation_id":"conv-warm-1","status":"SUCCESS","response":"reply-1","num_turns":1,"duration_seconds":1.5,"usage":{"input_tokens":1500,"cache_read_tokens":1000,"output_tokens":20,"total_tokens":1520}}}\n'
  else
    printf '{"event":"step_update","step_update":{"step_index":2,"step_type":"agent_response","state":"DONE","text_delta":"reply-2","usage":{"input_tokens":300,"cache_read_tokens":1600,"output_tokens":30}}}\n'
    printf '{"event":"result","result":{"conversation_id":"conv-warm-1","status":"SUCCESS","response":"reply-2","num_turns":2,"duration_seconds":2.5,"usage":{"input_tokens":3400,"cache_read_tokens":2600,"output_tokens":50,"total_tokens":3450}}}\n'
  fi
done
"#;

        let worker = PersistentWorker::spawn_and_wait_init(
            mock_persistent_script_cmd(script),
            None,
            None,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .expect("worker initializes");
        assert_eq!(worker.conversation_id.as_deref(), Some("conv-warm-1"));

        // Turn 1
        let (tx1, _rx1) = mpsc::channel(32);
        let mut pid_tx1 = None;
        let mut guard1 = None;
        let (res1, worker) = match execute_turn_on_worker(
            worker,
            WorkerTurnOpts {
                effective_prompt: "[System Instructions: rules]\nhi",
                sys_prompt_injected: true,
                is_warm: false,
                stream_tx: tx1,
                pid_tx: &mut pid_tx1,
                active_guard: &mut guard1,
                idle_timeout_secs: 30,
                max_execution_secs: 60,
                turn_cancel: None,
            },
        )
        .await
        {
            TurnRunOutcome::Completed { result, worker } => (result, *worker),
            _ => panic!("expected turn 1 completed"),
        };
        assert_eq!(res1.output, "reply-1");
        assert_eq!(res1.status, "SUCCESS");
        assert!(!res1.usage_is_cumulative);
        assert!(res1.system_prompt_sent);
        assert_eq!(res1.context_tokens, Some(1500));
        assert_eq!(res1.usage.as_ref().map(|u| u.input_tokens), Some(1500));
        assert_eq!(res1.num_turns, Some(1));

        // Turn 2 on SAME warm worker -> diffed deltas!
        let (tx2, _rx2) = mpsc::channel(32);
        let mut pid_tx2 = None;
        let mut guard2 = None;
        let (res2, worker) = match execute_turn_on_worker(
            worker,
            WorkerTurnOpts {
                effective_prompt: "second",
                sys_prompt_injected: false,
                is_warm: true,
                stream_tx: tx2,
                pid_tx: &mut pid_tx2,
                active_guard: &mut guard2,
                idle_timeout_secs: 30,
                max_execution_secs: 60,
                turn_cancel: None,
            },
        )
        .await
        {
            TurnRunOutcome::Completed { result, worker } => (result, *worker),
            _ => panic!("expected turn 2 completed"),
        };
        assert_eq!(res2.output, "reply-2");
        assert!(!res2.usage_is_cumulative);
        assert!(!res2.system_prompt_sent);
        assert_eq!(res2.context_tokens, Some(1900));
        assert_eq!(res2.usage.as_ref().map(|u| u.input_tokens), Some(1900));
        assert_eq!(res2.usage.as_ref().map(|u| u.output_tokens), Some(30));
        assert_eq!(res2.num_turns, Some(1));
        assert!((res2.duration_seconds.unwrap() - 1.0).abs() < 1e-6);

        // Graceful EOF close exits cleanly without hanging.
        worker.close_gracefully().await;
    }

    #[tokio::test]
    async fn warm_worker_unexpected_eof_before_output_signals_retry() {
        let script = r#"
printf '{"event":"init","conversation_id":"conv-die","init":{}}\n'
IFS= read -r _line
exit 1
"#;
        let worker = PersistentWorker::spawn_and_wait_init(
            mock_persistent_script_cmd(script),
            Some("conv-die"),
            None,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .expect("worker initializes");

        let (tx, _rx) = mpsc::channel(8);
        let mut pid_tx = None;
        let mut guard = None;
        let outcome = execute_turn_on_worker(
            worker,
            WorkerTurnOpts {
                effective_prompt: "hello",
                sys_prompt_injected: false,
                is_warm: true,
                stream_tx: tx,
                pid_tx: &mut pid_tx,
                active_guard: &mut guard,
                idle_timeout_secs: 30,
                max_execution_secs: 60,
                turn_cancel: None,
            },
        )
        .await;
        assert!(matches!(outcome, TurnRunOutcome::WarmFailedBeforeOutput(_)));
    }

    #[tokio::test]
    async fn warm_worker_immediate_error_result_before_output_signals_retry() {
        let script = r#"
printf '{"event":"init","conversation_id":"conv-stale","init":{}}\n'
IFS= read -r _line
printf '{"event":"result","result":{"conversation_id":"conv-stale","status":"ERROR","finished_reason":"interrupted","response":"stale previous reply","error":"stream input cancelled: context canceled"}}\n'
"#;
        let worker = PersistentWorker::spawn_and_wait_init(
            mock_persistent_script_cmd(script),
            Some("conv-stale"),
            None,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .expect("worker initializes");

        let (tx, mut rx) = mpsc::channel(8);
        let (ptx, _prx) = oneshot::channel();
        let mut pid_tx = Some(ptx);
        let mut guard = None;
        let outcome = execute_turn_on_worker(
            worker,
            WorkerTurnOpts {
                effective_prompt: "second turn",
                sys_prompt_injected: false,
                is_warm: true,
                stream_tx: tx,
                pid_tx: &mut pid_tx,
                active_guard: &mut guard,
                idle_timeout_secs: 30,
                max_execution_secs: 60,
                turn_cancel: None,
            },
        )
        .await;
        assert!(matches!(outcome, TurnRunOutcome::WarmFailedBeforeOutput(_)));
        assert!(
            pid_tx.is_some(),
            "pid_tx must remain available for the fresh retry spawn"
        );
        assert!(
            rx.try_recv().is_err(),
            "failed warm worker must not emit Init/Result before cold retry"
        );
    }

    #[tokio::test]
    async fn persistent_worker_honors_turn_cancel_mid_turn() {
        let script = r#"
printf '{"event":"init","conversation_id":"conv-cancel","init":{}}\n'
IFS= read -r _line
sleep 30
"#;
        let worker = PersistentWorker::spawn_and_wait_init(
            mock_persistent_script_cmd(script),
            Some("conv-cancel"),
            None,
            Duration::from_secs(5),
        )
        .await
        .ok()
        .expect("worker initializes");

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            trigger.cancel();
        });

        let (tx, _rx) = mpsc::channel(8);
        let mut pid_tx = None;
        let mut guard = None;
        let outcome = execute_turn_on_worker(
            worker,
            WorkerTurnOpts {
                effective_prompt: "long task",
                sys_prompt_injected: false,
                is_warm: false,
                stream_tx: tx,
                pid_tx: &mut pid_tx,
                active_guard: &mut guard,
                idle_timeout_secs: 30,
                max_execution_secs: 60,
                turn_cancel: Some(cancel),
            },
        )
        .await;
        match outcome {
            TurnRunOutcome::Terminal(res) => {
                assert_eq!(res.status, "CANCELED");
                assert_eq!(res.finished_reason.as_deref(), Some("canceled"));
                assert!(!res.system_prompt_sent);
            }
            _ => panic!("expected Terminal CANCELED outcome"),
        }
    }
}
