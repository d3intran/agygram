use crate::executor::TokenUsage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub conversation_id: Option<String>,
    pub workspace: String,
    #[serde(default)]
    pub extra_dirs: Vec<String>,
    pub model: String,
    pub effort: String,
    pub mode: String,
    pub agent: String,
    pub system_prompt: String,
    #[serde(default)]
    pub voice_mode: bool,
    #[serde(skip)]
    pub is_running: bool,
    pub is_first_turn: bool,
    #[serde(skip)]
    pub active_pid: Option<u32>,
    pub turn_count: u64,
    pub total_input_chars: usize,
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

impl ChatSession {
    pub fn new(default_workspace: String, default_model: String, default_mode: String) -> Self {
        let now = Instant::now();
        Self {
            conversation_id: None,
            workspace: default_workspace,
            extra_dirs: Vec::new(),
            model: default_model,
            effort: "high".to_string(),
            mode: default_mode,
            agent: String::new(),
            system_prompt: String::new(),
            voice_mode: false,
            is_running: false,
            is_first_turn: true,
            active_pid: None,
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
        let path = PathBuf::from(&self.workspace);
        if (path.exists() && path.is_dir()) || std::fs::create_dir_all(&path).is_ok() {
            return self.workspace.clone();
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

#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<i64, ChatSession>>>,
    default_workspace: String,
    default_model: String,
    default_mode: String,
    storage_path: PathBuf,
}

impl SessionManager {
    pub fn new(default_workspace: String, default_model: String, default_mode: String) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let storage_path = home.join(".config").join("agygram").join("sessions.json");

        let mut initial_map = HashMap::new();
        if storage_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&storage_path) {
                if let Ok(loaded) = serde_json::from_str::<HashMap<i64, ChatSession>>(&content) {
                    info!("已恢复持久化会话数据: {} 个会话", loaded.len());
                    initial_map = loaded;
                }
            }
        }

        Self {
            sessions: Arc::new(RwLock::new(initial_map)),
            default_workspace,
            default_model,
            default_mode,
            storage_path,
        }
    }

    #[inline]
    fn create_default_session(&self) -> ChatSession {
        ChatSession::new(
            self.default_workspace.clone(),
            self.default_model.clone(),
            self.default_mode.clone(),
        )
    }

    /// Atomic asynchronous persistence to disk via temporary file rename
    async fn save_to_disk(&self) {
        let data = {
            let map = self.sessions.read().await;
            serde_json::to_vec_pretty(&*map)
        };

        if let Ok(bytes) = data {
            let storage_path = &self.storage_path;
            let tmp_path = storage_path.with_extension("tmp");

            if let Some(parent) = storage_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }

            if tokio::fs::write(&tmp_path, &bytes).await.is_ok() {
                if let Err(e) = tokio::fs::rename(&tmp_path, storage_path).await {
                    error!("Failed to rename temporary sessions file: {:?}", e);
                }
            }
        }
    }

    pub async fn get_or_create(&self, chat_id: i64) -> ChatSession {
        let mut map = self.sessions.write().await;
        map.entry(chat_id)
            .or_insert_with(|| self.create_default_session())
            .clone()
    }

    pub async fn set_running(&self, chat_id: i64, running: bool, pid: Option<u32>) -> bool {
        let mut map = self.sessions.write().await;
        let session = map.entry(chat_id).or_insert_with(|| self.create_default_session());

        if running && session.is_running {
            return false;
        }
        session.is_running = running;
        session.active_pid = if running { pid } else { None };
        true
    }

    pub async fn set_pid(&self, chat_id: i64, pid: u32) {
        let mut map = self.sessions.write().await;
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

    pub async fn mark_turn_completed(
        &self,
        chat_id: i64,
        conv_id: Option<String>,
        input_len: usize,
        output_len: usize,
        usage: Option<&TokenUsage>,
        duration: Option<f64>,
    ) {
        {
            let mut map = self.sessions.write().await;
            if let Some(session) = map.get_mut(&chat_id) {
                session.is_first_turn = false;
                session.turn_count += 1;
                session.total_input_chars += input_len;
                session.total_output_chars += output_len;
                if let Some(u) = usage {
                    session.total_input_tokens += u.input_tokens;
                    session.total_output_tokens += u.output_tokens;
                    session.total_thinking_tokens += u.thinking_tokens;
                    session.total_cache_read_tokens += u.cache_read_tokens;
                }
                if let Some(d) = duration {
                    session.total_duration_seconds += d;
                }
                session.last_active_time = Instant::now();
                if conv_id.is_some() {
                    session.conversation_id = conv_id;
                }
            }
        }
        self.save_to_disk().await;
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

    pub async fn cancel_task(&self, chat_id: i64) -> (Option<u32>, bool) {
        let mut map = self.sessions.write().await;
        if let Some(session) = map.get_mut(&chat_id) {
            let was_running = session.is_running;
            let pid = session.active_pid;
            session.is_running = false;
            session.active_pid = None;

            if let Some(p) = pid {
                // SAFETY: `p` is the process group leader PID spawned with `tokio::process::Command::process_group(0)`.
                // Passing `-p` sends the signal to all processes in the group.
                // We send SIGINT first to allow agy to flush transcript.jsonl buffers, followed by SIGKILL for certainty.
                unsafe {
                    libc::kill(-(p as i32), libc::SIGINT);
                    libc::kill(p as i32, libc::SIGINT);
                    libc::kill(-(p as i32), libc::SIGKILL);
                    libc::kill(p as i32, libc::SIGKILL);
                }
            }

            return (pid, was_running);
        }
        (None, false)
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
}
