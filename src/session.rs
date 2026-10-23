use crate::executor::TokenUsage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;
use tracing::{error, info};

/// Keeps concurrent saves from sharing one temp filename.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub extra_dirs: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub voice_mode: bool,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub project: String,
    #[serde(skip)]
    pub is_running: bool,
    #[serde(default = "default_true")]
    pub is_first_turn: bool,
    #[serde(skip)]
    pub active_pid: Option<u32>,
    /// Monotonic turn token. Only the task holding the current generation may write session
    /// state, so an interrupted task cannot clobber the turn that already replaced it.
    #[serde(skip)]
    pub generation: u64,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub total_input_chars: usize,
    #[serde(default)]
    pub total_output_chars: usize,
    #[serde(default)]
    pub total_input_tokens: u64,
    #[serde(default)]
    pub total_output_tokens: u64,
    #[serde(default)]
    pub total_thinking_tokens: u64,
    #[serde(default)]
    pub total_cache_read_tokens: u64,
    #[serde(default)]
    pub total_duration_seconds: f64,
    #[serde(skip, default = "Instant::now")]
    pub session_start_time: Instant,
    #[serde(skip, default = "Instant::now")]
    pub last_active_time: Instant,
}

fn default_true() -> bool {
    true
}

impl ChatSession {
    pub fn new(
        default_workspace: String,
        default_model: String,
        default_mode: String,
        default_effort: String,
    ) -> Self {
        let now = Instant::now();
        Self {
            conversation_id: None,
            workspace: default_workspace,
            extra_dirs: Vec::new(),
            model: default_model,
            effort: default_effort,
            mode: default_mode,
            agent: String::new(),
            system_prompt: String::new(),
            voice_mode: false,
            sandbox: false,
            project: String::new(),
            is_running: false,
            is_first_turn: true,
            active_pid: None,
            generation: 0,
            turn_count: 0,
            total_input_chars: 0,
            total_output_chars: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_thinking_tokens: 0,
            total_cache_read_tokens: 0,
            total_duration_seconds: 0.0,
            session_start_time: now,
            last_active_time: now,
        }
    }

    pub fn get_valid_workspace(&self) -> String {
        let trimmed = self.workspace.trim();
        let expanded = if let Some(stripped) = trimmed.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                home.join(stripped)
            } else {
                PathBuf::from(trimmed)
            }
        } else if trimmed == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
        } else {
            PathBuf::from(trimmed)
        };

        if (expanded.exists() && expanded.is_dir()) || std::fs::create_dir_all(&expanded).is_ok() {
            return expanded.to_string_lossy().to_string();
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .to_string_lossy()
            .to_string()
    }
}

/// Resolves user paths (~/..., relative, or absolute) with zero redundant allocations
#[inline]
pub fn resolve_user_path(input_path: &str, current_workspace: &str) -> PathBuf {
    let trimmed = input_path.trim();
    if trimmed.is_empty() {
        return PathBuf::from(current_workspace);
    }

    let expanded = if let Some(stripped) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(stripped)
        } else {
            PathBuf::from(trimmed)
        }
    } else if trimmed == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
    } else {
        PathBuf::from(trimmed)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        let ws_relative = PathBuf::from(current_workspace).join(&expanded);
        if ws_relative.exists() {
            ws_relative
        } else if let Some(home) = dirs::home_dir() {
            let home_relative = home.join(&expanded);
            if home_relative.exists() {
                home_relative
            } else {
                ws_relative
            }
        } else {
            ws_relative
        }
    }
}

/// What one completed turn contributes to the session record.
#[derive(Debug)]
pub struct TurnOutcome<'a> {
    pub conversation_id: Option<String>,
    pub input_chars: usize,
    pub output_chars: usize,
    pub usage: Option<&'a TokenUsage>,
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<i64, ChatSession>>>,
    default_workspace: String,
    default_model: String,
    default_mode: String,
    default_effort: String,
    storage_path: PathBuf,
}

impl SessionManager {
    pub fn new(
        default_workspace: String,
        default_model: String,
        default_mode: String,
        default_effort: String,
    ) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let storage_path = home.join(".config").join("agygram").join("sessions.json");

        let mut initial_map = HashMap::new();
        if storage_path.exists() {
            match std::fs::read_to_string(&storage_path) {
                Ok(content) => match serde_json::from_str::<HashMap<i64, ChatSession>>(&content) {
                    Ok(loaded) => {
                        info!("已恢复持久化会话数据: {} 个会话", loaded.len());
                        initial_map = loaded;
                    }
                    // This used to be swallowed silently, so any schema drift reset every
                    // session with no trace in the log.
                    Err(err) => error!(
                        "会话文件 {} 解析失败，本次以空会话启动: {:?}",
                        storage_path.display(),
                        err
                    ),
                },
                Err(err) => error!("无法读取会话文件 {}: {:?}", storage_path.display(), err),
            }
        }

        Self {
            sessions: Arc::new(RwLock::new(initial_map)),
            default_workspace,
            default_model,
            default_mode,
            default_effort,
            storage_path,
        }
    }

    /// Test-only constructor: points the session file at a temp directory so unit tests never
    /// touch the operator's real ~/.config/agygram/sessions.json.
    #[cfg(test)]
    pub fn with_storage_path(
        default_workspace: String,
        default_model: String,
        default_mode: String,
        storage_path: PathBuf,
    ) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            default_workspace,
            default_model,
            default_mode,
            default_effort: "high".to_string(),
            storage_path,
        }
    }

    #[inline]
    fn create_default_session(&self) -> ChatSession {
        ChatSession::new(
            self.default_workspace.clone(),
            self.default_model.clone(),
            self.default_mode.clone(),
            self.default_effort.clone(),
        )
    }

    /// Persist the session map through a uniquely named temp file, flushed before an atomic rename.
    async fn save_to_disk(&self) {
        let data = {
            let map = self.sessions.read().await;
            serde_json::to_vec_pretty(&*map)
        };

        let Ok(bytes) = data else { return };

        let storage_path = &self.storage_path;
        // Every mutating command awaits this, so a single shared "sessions.tmp" allowed two
        // writers to interleave their write+rename and publish a torn file.
        let tmp_path = storage_path.with_file_name(format!(
            "sessions.tmp-{}-{}",
            std::process::id(),
            SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));

        if let Some(parent) = storage_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let written = async {
            let mut file = tokio::fs::File::create(&tmp_path).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            tokio::fs::rename(&tmp_path, storage_path).await
        }
        .await;

        if let Err(err) = written {
            error!("会话持久化失败 ({}): {:?}", storage_path.display(), err);
            let _ = tokio::fs::remove_file(&tmp_path).await;
        }
    }

    pub async fn get_or_create(&self, chat_id: i64) -> ChatSession {
        {
            let map = self.sessions.read().await;
            if let Some(session) = map.get(&chat_id) {
                return session.clone();
            }
        }
        let mut map = self.sessions.write().await;
        map.entry(chat_id)
            .or_insert_with(|| self.create_default_session())
            .clone()
    }

    /// Claim the chat for exactly one turn and hand back its generation token.
    /// Returns None when a turn is already in flight: the busy check and the flag flip happen
    /// inside a single write lock, so two messages arriving together can no longer both pass a
    /// stale "not running" snapshot and spawn two `agy` processes on one session.
    pub async fn begin_turn(&self, chat_id: i64) -> Option<u64> {
        let (gen, stale_pid) = {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            if session.is_running {
                return None;
            }
            let stale_pid = session.active_pid.take();
            session.is_running = true;
            session.generation += 1;
            (session.generation, stale_pid)
        };
        if let Some(pid) = stale_pid {
            if Self::process_needs_signal(pid) {
                Self::signal_process_tree(pid, libc::SIGINT);
            }
        }
        Some(gen)
    }

    #[inline]
    fn generation_is_current(
        map: &std::collections::HashMap<i64, ChatSession>,
        chat_id: i64,
        generation: u64,
    ) -> bool {
        map.get(&chat_id)
            .is_some_and(|session| session.generation == generation)
    }

    pub async fn is_generation_current(&self, chat_id: i64, generation: u64) -> bool {
        let map = self.sessions.read().await;
        Self::generation_is_current(&map, chat_id, generation)
    }

    pub async fn clear_detached_pid(&self, chat_id: i64, generation: u64) {
        let mut map = self.sessions.write().await;
        if !Self::generation_is_current(&map, chat_id, generation) {
            return;
        }
        if let Some(session) = map.get_mut(&chat_id) {
            session.active_pid = None;
        }
    }

    /// Free the turn lock without touching telemetry. Safe to call from a drop guard so an
    /// early `?` return can never strand the chat as "running"; a no-op once a newer turn has
    /// taken over or `finish_turn` has already closed the foreground turn.
    pub async fn release_turn(&self, chat_id: i64, generation: u64) {
        let mut map = self.sessions.write().await;
        if !Self::generation_is_current(&map, chat_id, generation) {
            return;
        }
        if let Some(session) = map.get_mut(&chat_id) {
            if session.is_running {
                session.is_running = false;
                session.active_pid = None;
            }
        }
    }

    pub async fn set_pid(&self, chat_id: i64, pid: u32, generation: u64) {
        let mut map = self.sessions.write().await;
        if !Self::generation_is_current(&map, chat_id, generation) {
            return;
        }
        if let Some(session) = map.get_mut(&chat_id) {
            session.active_pid = Some(pid);
        }
    }

    pub async fn reset_conversation(&self, chat_id: i64) {
        {
            let mut map = self.sessions.write().await;
            if let Some(session) = map.get_mut(&chat_id) {
                session.conversation_id = None;
                session.is_first_turn = true;
                session.turn_count = 0;
                session.total_input_chars = 0;
                session.total_output_chars = 0;
                session.total_input_tokens = 0;
                session.total_output_tokens = 0;
                session.total_thinking_tokens = 0;
                session.total_cache_read_tokens = 0;
                session.total_duration_seconds = 0.0;
                let now = Instant::now();
                session.session_start_time = now;
                session.last_active_time = now;
            }
        }
        self.save_to_disk().await;
    }

    /// Returns all (chat_id, conversation_id) pairs for sessions that are currently idle (no foreground or detached agy process running)
    pub async fn get_idle_conversations(&self) -> Vec<(i64, String)> {
        let map = self.sessions.read().await;
        map.iter()
            .filter_map(|(&chat_id, s)| {
                let has_live_process = s.active_pid.is_some_and(Self::process_needs_signal);
                if !s.is_running && !has_live_process {
                    s.conversation_id.as_ref().map(|cid| (chat_id, cid.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    pub async fn resume_conversation(&self, chat_id: i64, conv_id: Option<String>) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.is_first_turn = false;
            if conv_id.is_some() {
                session.conversation_id = conv_id;
            }
        }
        self.save_to_disk().await;
    }

    /// Close out a turn: apply telemetry and release the lock, but only when the caller still
    /// owns the generation. A cancelled task therefore cannot overwrite the conversation id or
    /// the counters of the turn that already replaced it.
    pub async fn finish_turn(
        &self,
        chat_id: i64,
        generation: u64,
        outcome: TurnOutcome<'_>,
    ) -> bool {
        {
            let mut map = self.sessions.write().await;
            let Some(session) = map.get_mut(&chat_id) else {
                return false;
            };
            if session.generation != generation {
                return false;
            }
            session.is_first_turn = false;
            session.turn_count += 1;
            session.total_input_chars += outcome.input_chars;
            session.total_output_chars += outcome.output_chars;
            if let Some(u) = outcome.usage {
                session.total_input_tokens += u.input_tokens;
                session.total_output_tokens += u.output_tokens;
                session.total_thinking_tokens += u.thinking_tokens;
                session.total_cache_read_tokens += u.cache_read_tokens;
            }
            if let Some(d) = outcome.duration_seconds {
                session.total_duration_seconds += d;
            }
            session.last_active_time = Instant::now();
            if outcome.conversation_id.is_some() {
                session.conversation_id = outcome.conversation_id;
            }
            session.is_running = false;
            if !session.active_pid.is_some_and(Self::process_needs_signal) {
                session.active_pid = None;
            }
        }
        self.save_to_disk().await;
        true
    }

    pub async fn set_workspace(&self, chat_id: i64, workspace: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.workspace = workspace;
        }
        self.save_to_disk().await;
    }

    pub async fn add_extra_dir(&self, chat_id: i64, dir: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            if !session.extra_dirs.contains(&dir) {
                session.extra_dirs.push(dir);
            }
        }
        self.save_to_disk().await;
    }

    pub async fn clear_extra_dirs(&self, chat_id: i64) {
        {
            let mut map = self.sessions.write().await;
            if let Some(session) = map.get_mut(&chat_id) {
                session.extra_dirs.clear();
            }
        }
        self.save_to_disk().await;
    }

    pub async fn set_model(&self, chat_id: i64, model: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.model = model;
        }
        self.save_to_disk().await;
    }

    pub async fn set_effort(&self, chat_id: i64, effort: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.effort = effort;
        }
        self.save_to_disk().await;
    }

    pub async fn set_mode(&self, chat_id: i64, mode: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.mode = mode;
        }
        self.save_to_disk().await;
    }

    pub async fn set_agent(&self, chat_id: i64, agent: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.agent = agent;
        }
        self.save_to_disk().await;
    }

    pub async fn set_system_prompt(&self, chat_id: i64, prompt: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.system_prompt = prompt;
        }
        self.save_to_disk().await;
    }

    /// Interrupt whatever is running in this chat and retire its generation.
    ///
    /// SIGINT first and SIGKILL only if the tree is still alive after a grace window: agy needs
    /// that window to flush `transcript.jsonl`, which is exactly what `/history` and `-c` resume
    /// read back. The old back-to-back INT+KILL sequence defeated its own stated purpose.
    pub async fn cancel_task(&self, chat_id: i64) -> (Option<u32>, bool) {
        let (pid, was_running) = {
            let mut map = self.sessions.write().await;
            let Some(session) = map.get_mut(&chat_id) else {
                return (None, false);
            };
            let was_running =
                session.is_running || session.active_pid.is_some_and(Self::process_needs_signal);
            let pid = session.active_pid;
            // Bumping the generation makes the killed task's own cleanup a no-op, so it cannot
            // race the turn that replaces it.
            session.generation += 1;
            session.is_running = false;
            session.active_pid = None;
            (pid, was_running)
        };

        let Some(pid) = pid else {
            return (None, was_running);
        };

        Self::signal_process_tree(pid, libc::SIGINT);
        tokio::spawn(async move {
            let grace = std::time::Duration::from_secs(5);
            let deadline = tokio::time::Instant::now() + grace;
            while tokio::time::Instant::now() < deadline {
                if !Self::process_needs_signal(pid) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            if Self::process_needs_signal(pid) {
                Self::signal_process_tree(pid, libc::SIGKILL);
            }
        });

        (Some(pid), was_running)
    }

    /// True while `pid` still names a live (non-zombie) process that is ours to signal. The
    /// identity check matters: after a grace window the original process may have been reaped and
    /// its PID recycled, and signalling a recycled PID would kill an unrelated process group.
    fn process_needs_signal(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let alive = match crate::executor::watchdog::parse_proc_stat(&stat) {
            Some((state, ..)) => state != 'Z',
            None => false,
        };
        alive && Self::cmdline_looks_like_agy(pid)
    }

    fn cmdline_looks_like_agy(pid: u32) -> bool {
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            return false;
        };
        let Some(argv0) = raw.split(|b| *b == 0).next() else {
            return false;
        };
        let basename = argv0.rsplit(|b| *b == b'/').next().unwrap_or(argv0);
        let name = String::from_utf8_lossy(basename).to_lowercase();
        name.contains("agy") || name.contains("antigravity") || name.contains("stdbuf")
    }

    /// Signal the process group led by `pid` (spawned with `process_group(0)`, so the child is
    /// the group leader) and the leader itself as a fallback.
    fn signal_process_tree(pid: u32, signal: i32) {
        let raw = pid as i32;
        unsafe {
            libc::kill(-raw, signal);
            libc::kill(raw, signal);
        }
    }

    pub async fn toggle_voice_mode(&self, chat_id: i64) -> bool {
        let new_state = {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.voice_mode = !session.voice_mode;
            session.voice_mode
        };
        self.save_to_disk().await;
        new_state
    }

    pub async fn set_voice_mode(&self, chat_id: i64, enabled: bool) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.voice_mode = enabled;
        }
        self.save_to_disk().await;
    }

    pub async fn toggle_sandbox(&self, chat_id: i64) -> bool {
        let new_state = {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.sandbox = !session.sandbox;
            session.sandbox
        };
        self.save_to_disk().await;
        new_state
    }

    pub async fn set_sandbox(&self, chat_id: i64, enabled: bool) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.sandbox = enabled;
        }
        self.save_to_disk().await;
    }

    pub async fn set_project(&self, chat_id: i64, project: String) {
        {
            let mut map = self.sessions.write().await;
            let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());
            session.project = project;
        }
        self.save_to_disk().await;
    }
}

/// Releases the turn lock when a handler exits, including through an early `?` after a failed
/// Telegram call. Without this, such a return stranded `is_running = true` and the chat rejected
/// every later message until a manual /cancel.
pub struct TurnGuard {
    manager: Arc<SessionManager>,
    chat_id: i64,
    generation: u64,
}

impl TurnGuard {
    pub fn new(manager: Arc<SessionManager>, chat_id: i64, generation: u64) -> Self {
        Self {
            manager,
            chat_id,
            generation,
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        let manager = self.manager.clone();
        let (chat_id, generation) = (self.chat_id, self.generation);
        // Drop cannot await; the spawned release is generation-gated and idempotent, so it is a
        // no-op after finish_turn already closed the turn or after a newer turn took over.
        tokio::spawn(async move { manager.release_turn(chat_id, generation).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_user_path_absolute() {
        let ws = "/tmp/test_ws";
        let res = resolve_user_path("/var/log", ws);
        assert_eq!(res, PathBuf::from("/var/log"));
    }

    #[test]
    fn test_resolve_user_path_empty() {
        let ws = "/tmp/test_ws";
        let res = resolve_user_path("", ws);
        assert_eq!(res, PathBuf::from("/tmp/test_ws"));
    }

    fn test_manager(tag: &str) -> Arc<SessionManager> {
        let dir = std::env::temp_dir().join(format!("agygram_session_tests_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Arc::new(SessionManager::with_storage_path(
            dir.to_string_lossy().to_string(),
            "gemini-3.8-flash-high".to_string(),
            "accept-edits".to_string(),
            dir.join("sessions.json"),
        ))
    }

    #[tokio::test]
    async fn begin_turn_is_exclusive_and_reusable_after_release() {
        let mgr = test_manager("exclusive");
        let first = mgr.begin_turn(1).await.expect("first claim succeeds");
        assert_eq!(first, 1);
        assert!(
            mgr.begin_turn(1).await.is_none(),
            "a second concurrent message must not claim the same chat"
        );

        mgr.release_turn(1, first).await;
        let second = mgr.begin_turn(1).await.expect("lock was released");
        assert_eq!(second, 2, "each turn gets a fresh generation");
    }

    #[tokio::test]
    async fn releasing_a_stale_generation_does_not_free_the_current_turn() {
        let mgr = test_manager("stale_release");
        let old = mgr.begin_turn(2).await.unwrap();
        mgr.release_turn(2, old).await;
        let current = mgr.begin_turn(2).await.unwrap();

        // The interrupted turn's guard fires late; it must not unlock the replacement turn.
        mgr.release_turn(2, old).await;
        assert!(
            mgr.begin_turn(2).await.is_none(),
            "stale release must be a no-op"
        );
        let outcome = TurnOutcome {
            conversation_id: None,
            input_chars: 0,
            output_chars: 0,
            usage: None,
            duration_seconds: None,
        };
        assert!(mgr.finish_turn(2, current, outcome).await);
    }

    #[tokio::test]
    async fn cancel_invalidates_the_killed_turns_writes() {
        let mgr = test_manager("cancel");
        let gen = mgr.begin_turn(3).await.unwrap();

        let (pid, was_running) = mgr.cancel_task(3).await;
        assert_eq!(pid, None);
        assert!(was_running);

        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        let stale = TurnOutcome {
            conversation_id: Some("stale-conv".to_string()),
            input_chars: 100,
            output_chars: 50,
            usage: Some(&usage),
            duration_seconds: Some(1.0),
        };
        assert!(
            !mgr.finish_turn(3, gen, stale).await,
            "a superseded turn must not write session state"
        );

        let session = mgr.get_or_create(3).await;
        assert_eq!(session.turn_count, 0, "telemetry must be untouched");
        assert_eq!(session.conversation_id, None, "conversation id must be untouched");

        let fresh = mgr.begin_turn(3).await.unwrap();
        assert!(fresh > gen);
        let ok = TurnOutcome {
            conversation_id: Some("real".to_string()),
            input_chars: 1,
            output_chars: 1,
            usage: None,
            duration_seconds: None,
        };
        assert!(mgr.finish_turn(3, fresh, ok).await);
        assert_eq!(
            mgr.get_or_create(3).await.conversation_id.as_deref(),
            Some("real")
        );
    }

    #[tokio::test]
    async fn set_pid_ignores_a_stale_generation() {
        let mgr = test_manager("pid");
        let old = mgr.begin_turn(4).await.unwrap();
        mgr.release_turn(4, old).await;
        let current = mgr.begin_turn(4).await.unwrap();

        mgr.set_pid(4, 999_999, old).await;
        assert_eq!(mgr.get_or_create(4).await.active_pid, None);

        mgr.set_pid(4, 4242, current).await;
        assert_eq!(mgr.get_or_create(4).await.active_pid, Some(4242));
    }

    #[tokio::test]
    async fn turn_guard_frees_the_lock_when_the_handler_exits() {
        let mgr = test_manager("guard");
        let gen = mgr.begin_turn(5).await.unwrap();
        {
            let _guard = TurnGuard::new(mgr.clone(), 5, gen);
        }
        // The guard releases from a spawned task, so give the runtime a tick.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            mgr.begin_turn(5).await.is_some(),
            "the lock must not outlive the handler that held it"
        );
    }
}
