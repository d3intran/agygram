use crate::config::{Config, ensure_valid_workspace, expand_tilde};
use crate::executor::TokenUsage;
use crate::models::{Effort, Mode};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Keeps concurrent saves from sharing one temp filename.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;
const QQ_NAMESPACE_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;
const MAX_DETACHED_PIDS_PER_SESSION: usize = 8;
const DEFAULT_MAX_CONCURRENT_AGY: usize = 2;

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
    /// Per-session Fish Audio voice model id; overrides `fish_reference_id` from the config.
    #[serde(default)]
    pub voice_reference_id: Option<String>,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub project: String,
    /// Optional per-session override for `agy --settings '{"showThinking": ...}'`.
    #[serde(default)]
    pub show_thinking: Option<bool>,
    /// When true, offloads execution to Antigravity cloud remote agent (`--remote-agent`).
    #[serde(default)]
    pub remote_agent: bool,
    /// Optional Orchestrator planner model override (`--settings '{"orchestrator": ...}'`).
    #[serde(default)]
    pub orchestrator_planner: Option<String>,
    /// Optional Orchestrator executor model override (`--settings '{"orchestrator": ...}'`).
    #[serde(default)]
    pub orchestrator_executor: Option<String>,
    #[serde(skip)]
    pub is_running: bool,
    /// True while an invisible background auto-compaction holds `is_running` for this chat.
    #[serde(skip)]
    pub is_bg_compacting: bool,
    #[serde(default = "default_true")]
    pub is_first_turn: bool,
    #[serde(skip)]
    pub active_pid: Option<u32>,
    /// Detached background `(generation, pid)` pairs kept separate from foreground `active_pid`
    /// so subsequent turns never kill still-running subagents or scheduled follow-ups.
    #[serde(skip)]
    pub detached_pids: Vec<(u64, u32)>,
    /// Monotonic turn token. Only the task holding the current generation may write session
    /// state, so an interrupted task cannot clobber the turn that already replaced it.
    #[serde(skip)]
    pub generation: u64,
    /// Bumped whenever a command replaces the conversation identity (`/new`, `/resume`). A turn
    /// that started before the switch must not write its own conversation id back on completion.
    #[serde(skip)]
    pub conversation_epoch: u64,
    /// Epoch observed when the current turn claimed the chat.
    #[serde(skip)]
    pub turn_started_epoch: u64,
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
    /// Conversation ID of the most recent cumulative telemetry report (prevents `O(N^2)` summing).
    #[serde(default)]
    pub last_reported_conv_id: Option<String>,
    #[serde(default)]
    pub conv_base_input_tokens: u64,
    #[serde(default)]
    pub conv_base_output_tokens: u64,
    #[serde(default)]
    pub conv_base_thinking_tokens: u64,
    #[serde(default)]
    pub conv_base_cache_read_tokens: u64,
    #[serde(default)]
    pub conv_base_duration_seconds: f64,
    /// Estimated real input token delta/context watermark for the most recently completed turn.
    #[serde(default)]
    pub last_turn_input_tokens: u64,
    /// True single-step context window size (`input_tokens + cache_read_tokens`) observed on the
    /// most recently completed model step. Resets on `/new`, `/resume`, or `CHECKPOINT`.
    #[serde(default)]
    pub last_context_tokens: u64,
    /// FNV-1a hash of the static base system prompt already injected into `conversation_id`.
    /// Cleared on `/new`, `/resume`, `/system`, conversation switch, or `CHECKPOINT`.
    #[serde(default)]
    pub last_system_prompt_hash: Option<u64>,
    /// Turn count when a manual compact hint was last shown (cooldown guard).
    #[serde(default)]
    pub last_compact_hint_turn: u64,
    /// Context watermark when a manual compact hint was last shown (cooldown guard).
    #[serde(default)]
    pub last_compact_hint_tokens: u64,
    /// Turn count when silent background auto-compaction last completed (cooldown guard).
    #[serde(default)]
    pub last_auto_compact_turn: u64,
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
            voice_reference_id: None,
            sandbox: false,
            project: String::new(),
            show_thinking: None,
            remote_agent: false,
            orchestrator_planner: None,
            orchestrator_executor: None,
            is_running: false,
            is_bg_compacting: false,
            is_first_turn: true,
            active_pid: None,
            detached_pids: Vec::new(),
            generation: 0,
            conversation_epoch: 0,
            turn_started_epoch: 0,
            turn_count: 0,
            total_input_chars: 0,
            total_output_chars: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_thinking_tokens: 0,
            total_cache_read_tokens: 0,
            total_duration_seconds: 0.0,
            last_reported_conv_id: None,
            conv_base_input_tokens: 0,
            conv_base_output_tokens: 0,
            conv_base_thinking_tokens: 0,
            conv_base_cache_read_tokens: 0,
            conv_base_duration_seconds: 0.0,
            last_turn_input_tokens: 0,
            last_context_tokens: 0,
            last_system_prompt_hash: None,
            last_compact_hint_turn: 0,
            last_compact_hint_tokens: 0,
            last_auto_compact_turn: 0,
            session_start_time: now,
            last_active_time: now,
        }
    }

    #[must_use]
    pub fn get_valid_workspace(&self) -> String {
        ensure_valid_workspace(&self.workspace)
    }
}

/// Resolves user paths (~/..., relative, or absolute) with zero redundant allocations.
#[inline]
#[must_use]
pub fn resolve_user_path(input_path: &str, current_workspace: &str) -> PathBuf {
    let trimmed = input_path.trim();
    if trimmed.is_empty() {
        return PathBuf::from(current_workspace);
    }

    let expanded = expand_tilde(trimmed);
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

#[inline]
fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV1A_PRIME);
    }
    hash
}

/// Stable 64-bit FNV-1a fingerprint used to deduplicate static `[System Instructions]` injection
/// across turns within the same conversation.
#[inline]
#[must_use]
pub fn hash_system_prompt(prompt: &str) -> u64 {
    fnv1a_update(FNV1A_OFFSET_BASIS, prompt.as_bytes())
}

#[inline]
fn project_qq_namespace(hash: u64, tag_nibble: u64) -> i64 {
    -(((hash & QQ_NAMESPACE_MASK) | (tag_nibble << 60)) as i64)
}

/// Deterministically maps a QQ OpenID to a unique `i64` chat_id using stable FNV-1a and
/// disjoint top-nibble `0x4`, avoiding Telegram IDs and QQ group/member namespaces.
#[inline]
#[must_use]
pub fn qq_openid_to_chat_id(openid: &str) -> i64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_update(h, b"qq:c2c:");
    h = fnv1a_update(h, openid.as_bytes());
    project_qq_namespace(h, 0x4)
}

/// Deterministically maps a QQ Group OpenID to a unique `i64` chat_id (top nibble `0x5`).
#[inline]
#[must_use]
pub fn qq_group_openid_to_chat_id(group_openid: &str) -> i64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_update(h, b"qq:group:");
    h = fnv1a_update(h, group_openid.as_bytes());
    project_qq_namespace(h, 0x5)
}

/// Deterministically maps a QQ Group + Member OpenID to a unique `i64` chat_id (top nibble `0x6`).
#[inline]
#[must_use]
pub fn qq_group_member_to_chat_id(group_openid: &str, member_openid: &str) -> i64 {
    let mut h = FNV1A_OFFSET_BASIS;
    h = fnv1a_update(h, b"qq:group_member:");
    h = fnv1a_update(h, group_openid.as_bytes());
    h = fnv1a_update(h, &[0]);
    h = fnv1a_update(h, member_openid.as_bytes());
    project_qq_namespace(h, 0x6)
}

/// Legacy `FxHasher` + overlapping `0x3FFF...` mapping retained exclusively for migrating
/// pre-existing `sessions.json` entries on load.
#[inline]
#[must_use]
pub fn legacy_qq_openid_to_chat_id(openid: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    "qq:c2c:".hash(&mut hasher);
    openid.hash(&mut hasher);
    let u = hasher.finish();
    -(((u & 0x3FFF_FFFF_FFFF_FFFF) | 0x4000_0000_0000_0000) as i64)
}

/// Legacy `FxHasher` + overlapping `0x3FFF...` group mapping for migration.
#[inline]
#[must_use]
pub fn legacy_qq_group_openid_to_chat_id(group_openid: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    "qq:group:".hash(&mut hasher);
    group_openid.hash(&mut hasher);
    let u = hasher.finish();
    -(((u & 0x3FFF_FFFF_FFFF_FFFF) | 0x5000_0000_0000_0000) as i64)
}

/// Legacy `FxHasher` + overlapping `0x3FFF...` group-member mapping for migration.
#[inline]
#[must_use]
pub fn legacy_qq_group_member_to_chat_id(group_openid: &str, member_openid: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    "qq:group_member:".hash(&mut hasher);
    group_openid.hash(&mut hasher);
    member_openid.hash(&mut hasher);
    let u = hasher.finish();
    -(((u & 0x3FFF_FFFF_FFFF_FFFF) | 0x6000_0000_0000_0000) as i64)
}

/// True for chat ids produced by the QQ mappings above: they are projected into
/// `[-i64::MAX, -2^62]`, far outside Telegram's range (supergroups bottom out around -10^13).
/// Components that can only address one platform use this to skip the other's sessions.
#[inline]
#[must_use]
pub fn is_qq_chat_id(chat_id: i64) -> bool {
    chat_id <= -(1i64 << 62)
}

/// What one completed turn contributes to the session record.
#[derive(Debug)]
pub struct TurnOutcome<'a> {
    pub conversation_id: Option<String>,
    pub input_chars: usize,
    pub output_chars: usize,
    pub usage: Option<&'a TokenUsage>,
    /// True single-step context watermark (`input_tokens + cache_read_tokens`) from `step_update`.
    pub context_tokens: Option<u64>,
    /// Model step count reported by `agy` (`result.num_turns`), used as fallback divisor.
    pub num_turns: Option<u64>,
    /// True when `agy` emitted a `CHECKPOINT` compaction step during this turn.
    pub saw_checkpoint: bool,
    /// Hash of the static base system prompt injected on this turn, if any.
    pub injected_system_prompt_hash: Option<u64>,
    pub duration_seconds: Option<f64>,
    pub detached: bool,
}

/// RAII permit representing one active global `agy` execution slot across Telegram and QQ.
#[derive(Debug)]
pub struct ExecSlotGuard {
    _permit: Option<OwnedSemaphorePermit>,
}

/// Cross-platform runtime activity snapshot across Telegram and QQ chats.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalRuntimeStatus {
    pub tg_running: usize,
    pub qq_running: usize,
    pub detached_count: usize,
    pub active_exec_slots: usize,
    pub max_exec_slots: usize,
}

fn build_exec_semaphore(max_concurrent_agy: usize) -> Option<Arc<Semaphore>> {
    if max_concurrent_agy == 0 {
        None
    } else {
        Some(Arc::new(Semaphore::new(max_concurrent_agy)))
    }
}

#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<FxHashMap<i64, ChatSession>>>,
    save_lock: Arc<Mutex<()>>,
    turn_notify: Arc<Notify>,
    pending_compacts: Arc<std::sync::Mutex<FxHashMap<i64, CancellationToken>>>,
    exec_semaphore: Option<Arc<Semaphore>>,
    max_concurrent_agy: usize,
    default_workspace: String,
    default_model: String,
    default_mode: String,
    default_effort: String,
    storage_path: PathBuf,
}

fn migrate_entry(map: &mut FxHashMap<i64, ChatSession>, legacy_id: i64, new_id: i64) -> bool {
    if legacy_id == new_id || map.contains_key(&new_id) {
        return false;
    }
    if let Some(session) = map.remove(&legacy_id) {
        map.insert(new_id, session);
        return true;
    }
    false
}

fn migrate_legacy_qq_map(
    map: &mut FxHashMap<i64, ChatSession>,
    qq_openids: &[String],
    qq_groups: &[String],
) -> usize {
    if map.is_empty() || (qq_openids.is_empty() && qq_groups.is_empty()) {
        return 0;
    }
    let mut migrated = 0usize;

    for openid in qq_openids {
        let id = openid.trim();
        if id.is_empty() {
            continue;
        }
        if migrate_entry(
            map,
            legacy_qq_openid_to_chat_id(id),
            qq_openid_to_chat_id(id),
        ) {
            migrated += 1;
        }
        // `qq_allowed_openids` historically also held group OpenIDs.
        if migrate_entry(
            map,
            legacy_qq_group_openid_to_chat_id(id),
            qq_group_openid_to_chat_id(id),
        ) {
            migrated += 1;
        }
    }

    for group_id in qq_groups {
        let gid = group_id.trim();
        if gid.is_empty() {
            continue;
        }
        if migrate_entry(
            map,
            legacy_qq_group_openid_to_chat_id(gid),
            qq_group_openid_to_chat_id(gid),
        ) {
            migrated += 1;
        }
    }

    if qq_groups.len().saturating_mul(qq_openids.len()) <= 4096 {
        for group_id in qq_groups {
            let gid = group_id.trim();
            if gid.is_empty() {
                continue;
            }
            for member_id in qq_openids {
                let mid = member_id.trim();
                if mid.is_empty() {
                    continue;
                }
                if migrate_entry(
                    map,
                    legacy_qq_group_member_to_chat_id(gid, mid),
                    qq_group_member_to_chat_id(gid, mid),
                ) {
                    migrated += 1;
                }
            }
        }
    }

    migrated
}

fn write_sessions_sync(
    storage_path: &Path,
    map: &FxHashMap<i64, ChatSession>,
) -> std::io::Result<()> {
    use std::io::Write;

    let bytes = serde_json::to_vec(map)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp_path = storage_path.with_file_name(format!(
        "sessions.tmp-{}-{}",
        std::process::id(),
        SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let res = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp_path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_data()?;
        std::fs::rename(&tmp_path, storage_path)
    })();

    if res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    res
}

fn format_proc_path<'a>(buf: &'a mut [u8; 48], pid: u32, file: &str) -> Option<&'a str> {
    use std::io::Write;
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    write!(cursor, "/proc/{pid}/{file}").ok()?;
    let len = usize::try_from(cursor.position()).ok()?;
    std::str::from_utf8(&buf[..len]).ok()
}

impl SessionManager {
    /// Creates a [`SessionManager`] from [`Config`] and automatically migrates any legacy QQ
    /// session IDs matching configured whitelists on load.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let storage_path = Config::get_config_dir().join("sessions.json");
        Self::load_from_storage(
            storage_path,
            config.get_valid_workspace(),
            config.default_model.clone(),
            config.default_mode.clone(),
            config.default_effort.clone(),
            &config.qq_allowed_openids,
            &config.qq_allowed_groups,
        )
        .with_max_concurrent_agy(config.max_concurrent_agy)
    }

    /// Configures the global maximum concurrent `agy` subprocess limit (`0` = unlimited).
    #[must_use]
    pub fn with_max_concurrent_agy(mut self, max_concurrent_agy: usize) -> Self {
        self.max_concurrent_agy = max_concurrent_agy;
        self.exec_semaphore = build_exec_semaphore(max_concurrent_agy);
        self
    }

    /// Loads persisted sessions from `storage_path`, creating its parent directory once and
    /// migrating any legacy QQ session keys found for `qq_openids` / `qq_groups`.
    #[must_use]
    pub fn load_from_storage(
        storage_path: PathBuf,
        default_workspace: String,
        default_model: String,
        default_mode: String,
        default_effort: String,
        qq_openids: &[String],
        qq_groups: &[String],
    ) -> Self {
        if let Some(parent) = storage_path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            error!("无法创建会话目录 {}: {:?}", parent.display(), err);
        }

        let mut initial_map = FxHashMap::default();
        match std::fs::read(&storage_path) {
            Ok(bytes) => match serde_json::from_slice::<FxHashMap<i64, ChatSession>>(&bytes) {
                Ok(mut loaded) => {
                    let migrated_qq = migrate_legacy_qq_map(&mut loaded, qq_openids, qq_groups);
                    let mut normalized_models = 0usize;
                    for sess in loaded.values_mut() {
                        if !sess.model.trim().is_empty() {
                            let norm = crate::models::normalize_model_name(&sess.model);
                            if norm.as_ref() != sess.model {
                                sess.model = norm.into_owned();
                                normalized_models += 1;
                            }
                        }
                        if let Ok(eff) = sess.effort.parse::<Effort>() {
                            let canonical = eff.as_str();
                            if sess.effort != canonical {
                                sess.effort = canonical.to_string();
                                normalized_models += 1;
                            }
                        } else if !sess.effort.is_empty() {
                            sess.effort = Effort::default().as_str().to_string();
                            normalized_models += 1;
                        }
                        if let Ok(mode) = sess.mode.parse::<Mode>() {
                            let canonical = mode.as_str();
                            if sess.mode != canonical {
                                sess.mode = canonical.to_string();
                                normalized_models += 1;
                            }
                        } else if !sess.mode.is_empty() {
                            sess.mode = Mode::default().as_str().to_string();
                            normalized_models += 1;
                        }
                    }
                    if migrated_qq > 0 || normalized_models > 0 {
                        info!(
                            migrated_qq,
                            normalized_models,
                            "已自动迁移/规范化持久化的 QQ 会话 ID 与历史模型名称"
                        );
                        if let Err(err) = write_sessions_sync(&storage_path, &loaded) {
                            error!(
                                "迁移后写入会话文件 {} 失败: {:?}",
                                storage_path.display(),
                                err
                            );
                        }
                    }
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => error!("无法读取会话文件 {}: {:?}", storage_path.display(), err),
        }

        Self {
            sessions: Arc::new(RwLock::new(initial_map)),
            save_lock: Arc::new(Mutex::new(())),
            turn_notify: Arc::new(Notify::new()),
            pending_compacts: Arc::new(std::sync::Mutex::new(FxHashMap::default())),
            exec_semaphore: build_exec_semaphore(DEFAULT_MAX_CONCURRENT_AGY),
            max_concurrent_agy: DEFAULT_MAX_CONCURRENT_AGY,
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
        if let Some(parent) = storage_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self {
            sessions: Arc::new(RwLock::new(FxHashMap::default())),
            save_lock: Arc::new(Mutex::new(())),
            turn_notify: Arc::new(Notify::new()),
            pending_compacts: Arc::new(std::sync::Mutex::new(FxHashMap::default())),
            exec_semaphore: build_exec_semaphore(DEFAULT_MAX_CONCURRENT_AGY),
            max_concurrent_agy: DEFAULT_MAX_CONCURRENT_AGY,
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

    /// Migrates a single legacy QQ chat ID to its new FNV-1a ID if present and not yet migrated.
    pub async fn migrate_qq_session(&self, legacy_id: i64, new_id: i64) -> bool {
        let changed = {
            let mut map = self.sessions.write().await;
            migrate_entry(&mut map, legacy_id, new_id)
        };
        if changed {
            self.save_to_disk().await;
        }
        changed
    }

    /// Persist the session map through a uniquely named temp file, serialized by `save_lock` so
    /// concurrent saves can never reorder an older snapshot over a newer mutation.
    async fn save_to_disk(&self) {
        let _save_guard = self.save_lock.lock().await;
        let data = {
            let map = self.sessions.read().await;
            serde_json::to_vec(&*map)
        };

        let Ok(bytes) = data else { return };

        let storage_path = &self.storage_path;
        let tmp_path = storage_path.with_file_name(format!(
            "sessions.tmp-{}-{}",
            std::process::id(),
            SAVE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));

        let written = async {
            let mut opts = tokio::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut file = opts.open(&tmp_path).await?;
            file.write_all(&bytes).await?;
            file.flush().await?;
            file.sync_data().await?;
            tokio::fs::rename(&tmp_path, storage_path).await
        }
        .await;

        if let Err(err) = written {
            error!("会话持久化失败 ({}): {:?}", storage_path.display(), err);
            let _ = tokio::fs::remove_file(&tmp_path).await;
        }
    }

    async fn mutate_and_save<R>(
        &self,
        chat_id: i64,
        create_if_missing: bool,
        f: impl FnOnce(&mut ChatSession) -> R,
    ) -> Option<R> {
        let result = {
            let mut map = self.sessions.write().await;
            let session = if create_if_missing {
                map.entry(chat_id)
                    .or_insert_with(|| self.create_default_session())
            } else {
                map.get_mut(&chat_id)?
            };
            f(session)
        };
        self.save_to_disk().await;
        Some(result)
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
        self.cancel_pending_bg_compact(chat_id);
        let (turn_gen, stale_pid) = {
            let mut map = self.sessions.write().await;
            let session = map
                .entry(chat_id)
                .or_insert_with(|| self.create_default_session());
            if session.is_running {
                return None;
            }
            let stale_pid = session.active_pid.take();
            session.is_running = true;
            session.is_bg_compacting = false;
            session.generation += 1;
            session.turn_started_epoch = session.conversation_epoch;
            (session.generation, stale_pid)
        };
        if let Some(pid) = stale_pid
            && Self::process_needs_signal(pid)
        {
            Self::signal_process_tree(pid, libc::SIGINT);
        }
        Some(turn_gen)
    }

    /// True while a silent background auto-compaction holds `chat_id`'s turn lock.
    pub async fn is_bg_compacting(&self, chat_id: i64) -> bool {
        let map = self.sessions.read().await;
        map.get(&chat_id)
            .is_some_and(|s| s.is_running && s.is_bg_compacting)
    }

    /// Registers a new cancellable idle timer token for `chat_id`, cancelling any previous one.
    pub(crate) fn register_pending_bg_compact(&self, chat_id: i64) -> CancellationToken {
        let token = CancellationToken::new();
        let prev = {
            let mut map = match self.pending_compacts.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.insert(chat_id, token.clone())
        };
        if let Some(old) = prev {
            old.cancel();
        }
        token
    }

    /// Cancels any pending idle auto-compact timer for `chat_id`.
    pub(crate) fn cancel_pending_bg_compact(&self, chat_id: i64) {
        let prev = {
            let mut map = match self.pending_compacts.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.remove(&chat_id)
        };
        if let Some(old) = prev {
            old.cancel();
        }
    }

    fn cancel_all_pending_bg_compacts(&self) {
        let tokens: Vec<CancellationToken> = {
            let mut map = match self.pending_compacts.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            map.drain().map(|(_, tok)| tok).collect()
        };
        for tok in tokens {
            tok.cancel();
        }
    }

    #[cfg(test)]
    pub(crate) fn has_pending_bg_compact(&self, chat_id: i64) -> bool {
        let map = match self.pending_compacts.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        map.get(&chat_id).is_some_and(|t| !t.is_cancelled())
    }

    /// Atomically claims `chat_id` for a silent background `/compact` if and only if the chat and
    /// host are completely idle, the conversation and turn count have not moved, no detached tasks
    /// are alive, and cooldown/threshold checks still hold.
    pub(crate) async fn try_begin_bg_compact(
        &self,
        chat_id: i64,
        expected_conv_id: &str,
        expected_turn_count: u64,
        threshold: u64,
        min_turn_cooldown: u64,
    ) -> Option<(u64, ChatSession)> {
        let detached_to_check: Vec<u32> = {
            let map = self.sessions.read().await;
            if map.values().any(|s| s.is_running) {
                return None;
            }
            let session = map.get(&chat_id)?;
            if session.conversation_id.as_deref() != Some(expected_conv_id)
                || session.turn_count != expected_turn_count
                || session.last_context_tokens < threshold
                || session
                    .turn_count
                    .saturating_sub(session.last_auto_compact_turn)
                    < min_turn_cooldown
            {
                return None;
            }
            session.detached_pids.iter().map(|&(_, pid)| pid).collect()
        };

        // Probe `/proc` outside the lock so we never block writers.
        if detached_to_check
            .into_iter()
            .any(Self::process_needs_signal)
        {
            return None;
        }

        let mut map = self.sessions.write().await;
        if map.values().any(|s| s.is_running) {
            return None;
        }
        let session = map.get_mut(&chat_id)?;
        if session.conversation_id.as_deref() != Some(expected_conv_id)
            || session.turn_count != expected_turn_count
            || session.last_context_tokens < threshold
            || session
                .turn_count
                .saturating_sub(session.last_auto_compact_turn)
                < min_turn_cooldown
        {
            return None;
        }

        session.is_running = true;
        session.is_bg_compacting = true;
        session.generation += 1;
        session.turn_started_epoch = session.conversation_epoch;
        let turn_gen = session.generation;
        let snapshot = session.clone();
        drop(map);

        // Clear the timer entry now that execution has started.
        let mut timers = match self.pending_compacts.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        timers.remove(&chat_id);

        Some((turn_gen, snapshot))
    }

    /// Completes a silent background `/compact`, resetting the context watermark and static
    /// system-prompt hash on success so the next user turn refreshes rules cleanly.
    pub(crate) async fn finish_bg_compact(
        &self,
        chat_id: i64,
        generation: u64,
        succeeded: bool,
        context_tokens: Option<u64>,
        saw_checkpoint: bool,
    ) {
        let should_save = {
            let mut map = self.sessions.write().await;
            if !Self::generation_is_current(&map, chat_id, generation) {
                return;
            }
            let Some(session) = map.get_mut(&chat_id) else {
                return;
            };
            session.is_running = false;
            session.is_bg_compacting = false;
            session.active_pid = None;
            let conversation_switched = session.conversation_epoch != session.turn_started_epoch;
            if !conversation_switched && (succeeded || saw_checkpoint) {
                session.last_context_tokens = context_tokens.unwrap_or(0);
                session.last_system_prompt_hash = None;
                session.last_auto_compact_turn = session.turn_count;
                session.last_compact_hint_turn = session.turn_count;
                session.last_compact_hint_tokens = session.last_context_tokens;
                true
            } else {
                false
            }
        };
        self.turn_notify.notify_waiters();
        if should_save {
            self.save_to_disk().await;
        }
    }

    /// Cooled-down manual compact hint check (used only when background auto-compact is disabled).
    pub async fn should_show_compact_hint(&self, chat_id: i64, threshold: u64) -> bool {
        const MIN_TURNS_BETWEEN_HINTS: u64 = 10;
        const MIN_TOKEN_GROWTH_BETWEEN_HINTS: u64 = 80_000;

        let map = self.sessions.read().await;
        let Some(session) = map.get(&chat_id) else {
            return false;
        };
        if session.last_context_tokens < threshold {
            return false;
        }
        if session.last_compact_hint_tokens == 0 {
            return true;
        }
        let turns_since = session
            .turn_count
            .saturating_sub(session.last_compact_hint_turn);
        let growth = session
            .last_context_tokens
            .saturating_sub(session.last_compact_hint_tokens);
        turns_since >= MIN_TURNS_BETWEEN_HINTS || growth >= MIN_TOKEN_GROWTH_BETWEEN_HINTS
    }

    /// Records that a manual compact hint was just appended to a reply.
    pub async fn record_compact_hint_shown(&self, chat_id: i64) {
        self.mutate_and_save(chat_id, false, |session| {
            session.last_compact_hint_turn = session.turn_count;
            session.last_compact_hint_tokens = session.last_context_tokens;
        })
        .await;
    }

    #[cfg(test)]
    #[inline]
    pub async fn is_running(&self, chat_id: i64) -> bool {
        let map = self.sessions.read().await;
        map.get(&chat_id).is_some_and(|s| s.is_running)
    }

    /// Claim the chat for one turn, waiting up to `timeout` via [`Notify`] if busy.
    pub async fn begin_turn_with_timeout(
        &self,
        chat_id: i64,
        timeout: std::time::Duration,
    ) -> Option<u64> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.turn_notify.notified();
            tokio::pin!(notified);
            // Register as a waiter before checking `begin_turn` so a concurrent release cannot
            // land between the check and the await.
            notified.as_mut().enable();

            if let Some(turn_gen) = self.begin_turn(chat_id).await {
                return Some(turn_gen);
            }

            if tokio::time::Instant::now() >= deadline {
                return None;
            }

            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.begin_turn(chat_id).await;
            }
        }
    }

    #[inline]
    fn generation_is_current(
        map: &FxHashMap<i64, ChatSession>,
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
        if let Some(session) = map.get_mut(&chat_id) {
            session.detached_pids.retain(|&(g, _)| g != generation);
            if session.generation == generation {
                session.active_pid = None;
            }
        }
    }

    /// Free the turn lock without touching telemetry. Safe to call from a drop guard so an
    /// early `?` return can never strand the chat as "running"; a no-op once a newer turn has
    /// taken over or `finish_turn` has already closed the foreground turn.
    pub async fn release_turn(&self, chat_id: i64, generation: u64) {
        let released = {
            let mut map = self.sessions.write().await;
            if !Self::generation_is_current(&map, chat_id, generation) {
                return;
            }
            if let Some(session) = map.get_mut(&chat_id) {
                if session.is_running {
                    session.is_running = false;
                    session.is_bg_compacting = false;
                    session.active_pid = None;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if released {
            self.turn_notify.notify_waiters();
        }
    }

    /// Records the live `agy` PID for `generation`. If `generation` is already stale (cancelled
    /// or superseded before `agy` finished spawning), immediately sends `SIGINT` to the orphan
    /// process tree with a 3s `SIGKILL` fallback so double `agy` processes cannot collide.
    pub async fn set_pid(&self, chat_id: i64, pid: u32, generation: u64) {
        let accepted = {
            let mut map = self.sessions.write().await;
            if !Self::generation_is_current(&map, chat_id, generation) {
                false
            } else if let Some(session) = map.get_mut(&chat_id) {
                session.active_pid = Some(pid);
                true
            } else {
                false
            }
        };

        if !accepted {
            warn!(
                chat_id,
                pid,
                generation,
                "rejecting stale generation PID and terminating orphan agy process"
            );
            Self::terminate_pid_with_grace(pid, std::time::Duration::from_secs(3));
        }
    }

    pub async fn reset_conversation(&self, chat_id: i64) {
        self.cancel_pending_bg_compact(chat_id);
        self.mutate_and_save(chat_id, false, |session| {
            session.conversation_epoch += 1;
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
            session.last_reported_conv_id = None;
            session.conv_base_input_tokens = 0;
            session.conv_base_output_tokens = 0;
            session.conv_base_thinking_tokens = 0;
            session.conv_base_cache_read_tokens = 0;
            session.conv_base_duration_seconds = 0.0;
            session.last_turn_input_tokens = 0;
            session.last_context_tokens = 0;
            session.last_system_prompt_hash = None;
            session.last_compact_hint_turn = 0;
            session.last_compact_hint_tokens = 0;
            session.last_auto_compact_turn = 0;
            let now = Instant::now();
            session.session_start_time = now;
            session.last_active_time = now;
        })
        .await;
    }

    /// Returns all `(chat_id, conversation_id)` pairs for sessions that are currently idle
    /// (no foreground or detached `agy` process running). `/proc` probes happen outside `RwLock`.
    pub async fn get_idle_conversations(&self) -> Vec<(i64, String)> {
        let candidates: Vec<(i64, Option<u32>, Vec<u32>, String)> = {
            let map = self.sessions.read().await;
            map.iter()
                .filter_map(|(&chat_id, s)| {
                    if !s.is_running {
                        s.conversation_id.as_ref().map(|cid| {
                            let detached: Vec<u32> =
                                s.detached_pids.iter().map(|&(_, pid)| pid).collect();
                            (chat_id, s.active_pid, detached, cid.clone())
                        })
                    } else {
                        None
                    }
                })
                .collect()
        };

        candidates
            .into_iter()
            .filter_map(|(chat_id, active_pid, detached_pids, cid)| {
                let has_live_fg = active_pid.is_some_and(Self::process_needs_signal);
                let has_live_detached = detached_pids.into_iter().any(Self::process_needs_signal);
                (!has_live_fg && !has_live_detached).then_some((chat_id, cid))
            })
            .collect()
    }

    pub async fn resume_conversation(&self, chat_id: i64, conv_id: Option<String>) {
        self.cancel_pending_bg_compact(chat_id);
        self.mutate_and_save(chat_id, true, |session| {
            session.conversation_epoch += 1;
            session.is_first_turn = false;
            session.conversation_id = conv_id;
            session.last_reported_conv_id = None;
            session.conv_base_input_tokens = 0;
            session.conv_base_output_tokens = 0;
            session.conv_base_thinking_tokens = 0;
            session.conv_base_cache_read_tokens = 0;
            session.conv_base_duration_seconds = 0.0;
            session.last_turn_input_tokens = 0;
            session.last_context_tokens = 0;
            session.last_system_prompt_hash = None;
            session.last_compact_hint_turn = 0;
            session.last_compact_hint_tokens = 0;
            session.last_auto_compact_turn = 0;
            session.total_input_tokens = 0;
            session.total_output_tokens = 0;
            session.total_thinking_tokens = 0;
            session.total_cache_read_tokens = 0;
            session.total_duration_seconds = 0.0;
        })
        .await;
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
        let (pid_to_probe, older_detached, detached_exceeded) = {
            let mut map = self.sessions.write().await;
            let Some(session) = map.get_mut(&chat_id) else {
                return false;
            };
            if session.generation != generation {
                return false;
            }
            // A `/new` or `/resume` issued while this turn was in flight owns the conversation
            // identity now; don't resurrect the id this turn started with.
            let conversation_switched = session.conversation_epoch != session.turn_started_epoch;
            if !conversation_switched {
                session.is_first_turn = false;
            }
            session.turn_count = session.turn_count.saturating_add(1);
            session.total_input_chars = session
                .total_input_chars
                .saturating_add(outcome.input_chars);
            session.total_output_chars = session
                .total_output_chars
                .saturating_add(outcome.output_chars);

            if !conversation_switched {
                let effective_conv_id = outcome
                    .conversation_id
                    .as_deref()
                    .or(session.conversation_id.as_deref())
                    .map(str::to_string);

                if let Some(cid) = effective_conv_id {
                    let (same_conv_reported, prev_conv_input, prev_conv_cache) =
                        if session.last_reported_conv_id.as_deref() == Some(cid.as_str()) {
                            (
                                true,
                                session
                                    .total_input_tokens
                                    .saturating_sub(session.conv_base_input_tokens),
                                session
                                    .total_cache_read_tokens
                                    .saturating_sub(session.conv_base_cache_read_tokens),
                            )
                        } else if session.last_reported_conv_id.is_none() {
                            // First reported turn on a fresh/resumed session OR first turn on a
                            // legacy inflated `sessions.json`: keep `conv_base_*` at 0 and overwrite
                            // `total_*` so inflated legacy counters self-heal immediately.
                            session.last_reported_conv_id = Some(cid);
                            (false, 0, 0)
                        } else {
                            // Conversation ID changed in-place without `/new` or `/resume`: roll
                            // previous conversation totals into the new baseline.
                            session.conv_base_input_tokens = session.total_input_tokens;
                            session.conv_base_output_tokens = session.total_output_tokens;
                            session.conv_base_thinking_tokens = session.total_thinking_tokens;
                            session.conv_base_cache_read_tokens = session.total_cache_read_tokens;
                            session.conv_base_duration_seconds = session.total_duration_seconds;
                            session.last_reported_conv_id = Some(cid);
                            session.last_system_prompt_hash = None;
                            (false, 0, 0)
                        };

                    if let Some(u) = outcome.usage {
                        session.last_turn_input_tokens = if u.input_tokens >= prev_conv_input {
                            u.input_tokens - prev_conv_input
                        } else {
                            u.input_tokens
                        };
                        session.total_input_tokens = session
                            .conv_base_input_tokens
                            .saturating_add(u.input_tokens);
                        session.total_output_tokens = session
                            .conv_base_output_tokens
                            .saturating_add(u.output_tokens);
                        session.total_thinking_tokens = session
                            .conv_base_thinking_tokens
                            .saturating_add(u.thinking_tokens);
                        session.total_cache_read_tokens = session
                            .conv_base_cache_read_tokens
                            .saturating_add(u.cache_read_tokens);
                    }

                    // Record exact single-step context watermark. Never fall back to whole-history
                    // cumulative tokens on the first report after a restart/resume.
                    if let Some(ctx) = outcome.context_tokens {
                        session.last_context_tokens = ctx;
                    } else if outcome.saw_checkpoint {
                        session.last_context_tokens = 0;
                    } else if same_conv_reported && let Some(u) = outcome.usage {
                        let in_delta = u.input_tokens.saturating_sub(prev_conv_input);
                        let cache_delta = u.cache_read_tokens.saturating_sub(prev_conv_cache);
                        let steps = outcome.num_turns.unwrap_or(1).max(1);
                        let est = in_delta.saturating_add(cache_delta) / steps;
                        if est > 0 {
                            session.last_context_tokens = est;
                        }
                    }

                    if let Some(d) = outcome.duration_seconds
                        && d.is_finite()
                        && d >= 0.0
                    {
                        session.total_duration_seconds = session.conv_base_duration_seconds + d;
                    }
                } else {
                    if let Some(u) = outcome.usage {
                        session.last_turn_input_tokens = u.input_tokens;
                        session.total_input_tokens =
                            session.total_input_tokens.saturating_add(u.input_tokens);
                        session.total_output_tokens =
                            session.total_output_tokens.saturating_add(u.output_tokens);
                        session.total_thinking_tokens = session
                            .total_thinking_tokens
                            .saturating_add(u.thinking_tokens);
                        session.total_cache_read_tokens = session
                            .total_cache_read_tokens
                            .saturating_add(u.cache_read_tokens);
                    }
                    if let Some(ctx) = outcome.context_tokens {
                        session.last_context_tokens = ctx;
                    } else if outcome.saw_checkpoint {
                        session.last_context_tokens = 0;
                    }
                    if let Some(d) = outcome.duration_seconds
                        && d.is_finite()
                        && d >= 0.0
                    {
                        session.total_duration_seconds += d;
                    }
                }

                if outcome.saw_checkpoint {
                    // Force the next user turn after compaction to refresh static platform rules.
                    session.last_system_prompt_hash = None;
                    session.last_compact_hint_turn = session.turn_count;
                    session.last_compact_hint_tokens = session.last_context_tokens;
                    session.last_auto_compact_turn = session.turn_count;
                } else if let Some(hash) = outcome.injected_system_prompt_hash {
                    session.last_system_prompt_hash = Some(hash);
                }
            }

            session.last_active_time = Instant::now();
            if !conversation_switched && outcome.conversation_id.is_some() {
                session.conversation_id = outcome.conversation_id;
            }
            session.is_running = false;
            session.is_bg_compacting = false;

            if outcome.detached {
                if let Some(pid) = session.active_pid.take() {
                    session
                        .detached_pids
                        .retain(|&(g, p)| g != generation && p != pid);
                    session.detached_pids.push((generation, pid));
                }
                let older: Vec<(u64, u32)> = session
                    .detached_pids
                    .iter()
                    .copied()
                    .filter(|&(g, _)| g != generation)
                    .collect();
                let exceeded = session.detached_pids.len() > MAX_DETACHED_PIDS_PER_SESSION;
                (None, older, exceeded)
            } else {
                (session.active_pid, Vec::new(), false)
            }
        };

        self.turn_notify.notify_waiters();

        if outcome.detached {
            let dead_older: Vec<(u64, u32)> = older_detached
                .into_iter()
                .filter(|&(_, pid)| !Self::process_needs_signal(pid))
                .collect();
            if !dead_older.is_empty() || detached_exceeded {
                let mut map = self.sessions.write().await;
                if let Some(session) = map.get_mut(&chat_id) {
                    if !dead_older.is_empty() {
                        session
                            .detached_pids
                            .retain(|entry| !dead_older.contains(entry));
                    }
                    if session.detached_pids.len() > MAX_DETACHED_PIDS_PER_SESSION {
                        let excess = session.detached_pids.len() - MAX_DETACHED_PIDS_PER_SESSION;
                        session.detached_pids.drain(..excess);
                    }
                }
            }
        } else if let Some(pid) = pid_to_probe
            && !Self::process_needs_signal(pid)
        {
            let mut map = self.sessions.write().await;
            if let Some(session) = map.get_mut(&chat_id)
                && session.generation == generation
                && session.active_pid == Some(pid)
            {
                session.active_pid = None;
            }
        }

        self.save_to_disk().await;
        true
    }

    pub async fn set_workspace(&self, chat_id: i64, workspace: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.workspace = workspace;
        })
        .await;
    }

    pub async fn add_extra_dir(&self, chat_id: i64, dir: String) {
        self.mutate_and_save(chat_id, true, |session| {
            if !session.extra_dirs.contains(&dir) {
                session.extra_dirs.push(dir);
            }
        })
        .await;
    }

    pub async fn clear_extra_dirs(&self, chat_id: i64) {
        self.mutate_and_save(chat_id, false, |session| {
            session.extra_dirs.clear();
        })
        .await;
    }

    #[allow(dead_code)]
    pub async fn set_model(&self, chat_id: i64, model: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.model = model;
        })
        .await;
    }

    /// Atomically updates `model` and syncs `effort` if the model ID carries an effort suffix.
    pub async fn set_model_and_sync_effort(&self, chat_id: i64, model: String) -> Option<Effort> {
        self.mutate_and_save(chat_id, true, |session| {
            let inferred = crate::models::effort_from_model_suffix(&model);
            if let Some(eff) = inferred {
                session.effort = eff.as_str().to_string();
            }
            session.model = model;
            inferred
        })
        .await
        .flatten()
    }

    #[allow(dead_code)]
    pub async fn set_effort(&self, chat_id: i64, effort: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.effort = effort;
        })
        .await;
    }

    #[allow(dead_code)]
    pub async fn set_effort_enum(&self, chat_id: i64, effort: Effort) {
        self.set_effort(chat_id, effort.as_str().to_string()).await;
    }

    /// Atomically updates `effort` and switches to a sibling variant model if the current model
    /// embeds a conflicting effort suffix (e.g. `gemini-4-carbon-high` -> `gemini-4-carbon-low`).
    /// Returns `(effective_model, model_was_synced)`.
    pub async fn set_effort_and_sync_model(&self, chat_id: i64, effort: Effort) -> (String, bool) {
        self.mutate_and_save(chat_id, true, |session| {
            session.effort = effort.as_str().to_string();
            if let Some(sibling) = crate::models::sibling_model_for_effort(&session.model, effort)
                && sibling != session.model
            {
                session.model = sibling.clone();
                (sibling, true)
            } else {
                (session.model.clone(), false)
            }
        })
        .await
        .unwrap_or_default()
    }

    pub async fn set_mode(&self, chat_id: i64, mode: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.mode = mode;
        })
        .await;
    }

    pub async fn set_mode_enum(&self, chat_id: i64, mode: Mode) {
        self.set_mode(chat_id, mode.as_str().to_string()).await;
    }

    pub async fn set_agent(&self, chat_id: i64, agent: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.agent = agent;
        })
        .await;
    }

    pub async fn set_system_prompt(&self, chat_id: i64, prompt: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.system_prompt = prompt;
            session.last_system_prompt_hash = None;
        })
        .await;
    }

    /// Non-blocking attempt to acquire a global `agy` execution slot (used by silent background
    /// auto-compaction so it never blocks or contends with foreground user traffic).
    #[must_use]
    pub fn try_acquire_exec_slot(&self) -> Option<ExecSlotGuard> {
        let Some(ref sem) = self.exec_semaphore else {
            return Some(ExecSlotGuard { _permit: None });
        };
        Arc::clone(sem)
            .try_acquire_owned()
            .ok()
            .map(|p| ExecSlotGuard { _permit: Some(p) })
    }

    /// Acquires one global `agy` execution slot (bounded by `max_concurrent_agy`; unlimited when `0`).
    /// Returns `None` if `cancel` is triggered before a slot becomes available.
    pub async fn acquire_exec_slot(
        &self,
        cancel: Option<&CancellationToken>,
    ) -> Option<ExecSlotGuard> {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return None;
        }
        let Some(ref sem) = self.exec_semaphore else {
            return Some(ExecSlotGuard { _permit: None });
        };
        if let Some(token) = cancel {
            tokio::select! {
                biased;
                () = token.cancelled() => None,
                permit = Arc::clone(sem).acquire_owned() => {
                    permit.ok().map(|p| ExecSlotGuard { _permit: Some(p) })
                }
            }
        } else {
            Arc::clone(sem)
                .acquire_owned()
                .await
                .ok()
                .map(|p| ExecSlotGuard { _permit: Some(p) })
        }
    }

    /// Captures a cross-platform snapshot of active Telegram/QQ turns, live detached processes,
    /// and global execution slot occupancy.
    pub async fn global_status_snapshot(&self) -> GlobalRuntimeStatus {
        let (tg_running, qq_running, detached_pids) = {
            let map = self.sessions.read().await;
            let mut tg = 0usize;
            let mut qq = 0usize;
            let mut detached = Vec::new();
            for (&chat_id, session) in map.iter() {
                if session.is_running {
                    if is_qq_chat_id(chat_id) {
                        qq += 1;
                    } else {
                        tg += 1;
                    }
                }
                for &(_, pid) in &session.detached_pids {
                    detached.push(pid);
                }
            }
            (tg, qq, detached)
        };

        let detached_count = detached_pids
            .into_iter()
            .filter(|&pid| Self::process_needs_signal(pid))
            .count();

        let active_exec_slots = match &self.exec_semaphore {
            Some(sem) => self
                .max_concurrent_agy
                .saturating_sub(sem.available_permits()),
            None => tg_running.saturating_add(qq_running),
        };

        GlobalRuntimeStatus {
            tg_running,
            qq_running,
            detached_count,
            active_exec_slots,
            max_exec_slots: self.max_concurrent_agy,
        }
    }

    /// Sends `SIGINT` to all tracked foreground and detached `agy` process trees during daemon
    /// shutdown so they can flush `transcript.jsonl` cleanly rather than leaking as orphans.
    pub async fn shutdown_all_processes(&self) {
        self.cancel_all_pending_bg_compacts();
        let pids: Vec<u32> = {
            let mut map = self.sessions.write().await;
            let mut all = Vec::new();
            for session in map.values_mut() {
                session.generation += 1;
                session.is_running = false;
                session.is_bg_compacting = false;
                if let Some(pid) = session.active_pid.take() {
                    all.push(pid);
                }
                for (_, pid) in session.detached_pids.drain(..) {
                    if !all.contains(&pid) {
                        all.push(pid);
                    }
                }
            }
            all
        };

        self.turn_notify.notify_waiters();

        let mut signaled = 0usize;
        for pid in pids {
            if Self::process_needs_signal(pid) {
                Self::signal_process_tree(pid, libc::SIGINT);
                signaled += 1;
            }
        }
        if signaled > 0 {
            info!(
                signaled,
                "sent SIGINT to active agy process trees during shutdown"
            );
        }
    }

    fn terminate_pid_with_grace(pid: u32, grace: std::time::Duration) -> bool {
        if !Self::process_needs_signal(pid) {
            return false;
        }
        Self::signal_process_tree(pid, libc::SIGINT);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
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
        }
        true
    }

    /// Interrupt whatever is running in this chat (foreground and any live detached `agy` tasks)
    /// and retire its generation.
    ///
    /// SIGINT first and SIGKILL only if the tree is still alive after a grace window: agy needs
    /// that window to flush `transcript.jsonl`, which is exactly what `/history` and `-c` resume
    /// read back.
    pub async fn cancel_task(&self, chat_id: i64) -> (Option<u32>, bool) {
        self.cancel_pending_bg_compact(chat_id);
        let (was_flag_running, fg_pid, detached_pids) = {
            let mut map = self.sessions.write().await;
            let Some(session) = map.get_mut(&chat_id) else {
                return (None, false);
            };
            let was_flag_running = session.is_running;
            let fg_pid = session.active_pid.take();
            let detached_pids: Vec<u32> = session
                .detached_pids
                .drain(..)
                .map(|(_, pid)| pid)
                .collect();
            // Bumping the generation makes the killed task's own cleanup a no-op, so it cannot
            // race the turn that replaces it.
            session.generation += 1;
            session.is_running = false;
            session.is_bg_compacting = false;
            (was_flag_running, fg_pid, detached_pids)
        };

        if was_flag_running {
            self.turn_notify.notify_waiters();
        }

        let grace = std::time::Duration::from_secs(5);
        let mut any_live = false;
        let mut reported_pid = fg_pid;

        if let Some(pid) = fg_pid
            && Self::terminate_pid_with_grace(pid, grace)
        {
            any_live = true;
        }

        for pid in detached_pids {
            if Some(pid) == fg_pid {
                continue;
            }
            if Self::terminate_pid_with_grace(pid, grace) {
                any_live = true;
                if reported_pid.is_none() {
                    reported_pid = Some(pid);
                }
            }
        }

        let was_running = was_flag_running || any_live;
        info!(
            chat_id,
            fg_pid = ?fg_pid,
            reported_pid = ?reported_pid,
            was_flag_running,
            any_live,
            was_running,
            "cancel_task processed"
        );
        (reported_pid, was_running)
    }

    /// True while `pid` still names a live (non-zombie) process that is ours to signal.
    fn process_needs_signal(pid: u32) -> bool {
        use std::io::Read;
        let mut path_buf = [0u8; 48];
        let Some(path) = format_proc_path(&mut path_buf, pid, "stat") else {
            return false;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        let mut stat_buf = [0u8; 1024];
        let Ok(n) = file.read(&mut stat_buf) else {
            return false;
        };
        let Ok(stat) = std::str::from_utf8(&stat_buf[..n]) else {
            return false;
        };
        let alive = match crate::executor::watchdog::parse_proc_stat(stat) {
            Some((state, ..)) => state != 'Z',
            None => false,
        };
        alive && Self::cmdline_looks_like_agy(pid)
    }

    fn cmdline_looks_like_agy(pid: u32) -> bool {
        use std::io::Read;
        let mut path_buf = [0u8; 48];
        let Some(path) = format_proc_path(&mut path_buf, pid, "cmdline") else {
            return false;
        };
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        let mut buf = [0u8; 256];
        let Ok(n) = file.read(&mut buf) else {
            return false;
        };
        if n == 0 {
            return false;
        }
        let raw = &buf[..n];
        let Some(argv0) = raw.split(|&b| b == 0).next() else {
            return false;
        };
        let basename = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
        basename.windows(3).any(|w| w.eq_ignore_ascii_case(b"agy"))
            || basename
                .windows(11)
                .any(|w| w.eq_ignore_ascii_case(b"antigravity"))
            || basename
                .windows(6)
                .any(|w| w.eq_ignore_ascii_case(b"stdbuf"))
    }

    /// Signal the process group led by `pid` (spawned with `process_group(0)`, so the child is
    /// the group leader) and the leader itself as a fallback.
    #[inline]
    fn signal_process_tree(pid: u32, signal: i32) {
        crate::executor::watchdog::signal_process_tree(pid, signal);
    }

    pub async fn toggle_voice_mode(&self, chat_id: i64) -> bool {
        self.mutate_and_save(chat_id, true, |session| {
            session.voice_mode = !session.voice_mode;
            session.voice_mode
        })
        .await
        .unwrap_or(false)
    }

    pub async fn set_voice_mode(&self, chat_id: i64, enabled: bool) {
        self.mutate_and_save(chat_id, true, |session| {
            session.voice_mode = enabled;
        })
        .await;
    }

    /// `None` clears the session override so the configured default voice is used again.
    pub async fn set_voice_reference(&self, chat_id: i64, reference_id: Option<String>) {
        self.mutate_and_save(chat_id, true, |session| {
            session.voice_reference_id = reference_id;
        })
        .await;
    }

    pub async fn toggle_sandbox(&self, chat_id: i64) -> bool {
        self.mutate_and_save(chat_id, true, |session| {
            session.sandbox = !session.sandbox;
            session.sandbox
        })
        .await
        .unwrap_or(false)
    }

    pub async fn set_sandbox(&self, chat_id: i64, enabled: bool) {
        self.mutate_and_save(chat_id, true, |session| {
            session.sandbox = enabled;
        })
        .await;
    }

    pub async fn set_project(&self, chat_id: i64, project: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.project = project;
        })
        .await;
    }

    pub async fn set_show_thinking(&self, chat_id: i64, show_thinking: Option<bool>) {
        self.mutate_and_save(chat_id, true, |session| {
            session.show_thinking = show_thinking;
        })
        .await;
    }

    pub async fn set_remote_agent(&self, chat_id: i64, enabled: bool) {
        self.mutate_and_save(chat_id, true, |session| {
            session.remote_agent = enabled;
        })
        .await;
    }

    pub async fn toggle_remote_agent(&self, chat_id: i64) -> bool {
        self.mutate_and_save(chat_id, true, |session| {
            session.remote_agent = !session.remote_agent;
            session.remote_agent
        })
        .await
        .unwrap_or(false)
    }

    #[allow(dead_code)]
    pub async fn set_orchestrator_override(
        &self,
        chat_id: i64,
        planner: Option<String>,
        executor: Option<String>,
    ) {
        self.mutate_and_save(chat_id, true, |session| {
            session.orchestrator_planner = planner
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            session.orchestrator_executor = executor
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        })
        .await;
    }
}

/// Releases the turn lock when a handler exits, including through an early `?` after a failed
/// Telegram call. Call [`TurnGuard::disarm`] after `finish_turn` has already closed the turn to
/// skip the redundant cleanup task spawn.
pub struct TurnGuard {
    manager: Arc<SessionManager>,
    chat_id: i64,
    generation: u64,
    armed: bool,
}

impl TurnGuard {
    #[must_use]
    pub fn new(manager: Arc<SessionManager>, chat_id: i64, generation: u64) -> Self {
        Self {
            manager,
            chat_id,
            generation,
            armed: true,
        }
    }

    /// Disarms the guard after [`SessionManager::finish_turn`] has completed normally.
    #[inline]
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let manager = self.manager.clone();
            let (chat_id, generation) = (self.chat_id, self.generation);
            handle.spawn(async move { manager.release_turn(chat_id, generation).await });
        }
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

    #[test]
    fn test_qq_id_mappings() {
        let openid = "87233760CED716E069E2BF634E7BCB30";
        let grp_a = "B55E344ED08A6C9175E12E4D9E50D325";
        let grp_b = "8822CE1C0C0BB6A9BC75323724830F50";
        let mem_b = "70F5D2930DCD78080A6B3F5201BD0FCC";

        // Preserve historical FxHasher expectations for legacy migration.
        assert_eq!(legacy_qq_openid_to_chat_id(openid), -8142664994840455790);
        assert_eq!(
            legacy_qq_group_openid_to_chat_id(grp_a),
            -5770226939528808687
        );
        assert_eq!(
            legacy_qq_group_openid_to_chat_id(grp_b),
            -6160516967238314706
        );

        let c2c_user = qq_openid_to_chat_id(openid);
        let group_1 = qq_group_openid_to_chat_id(grp_a);
        let group_2 = qq_group_openid_to_chat_id(grp_b);
        let member_1 = qq_group_member_to_chat_id(grp_a, openid);
        let member_2 = qq_group_member_to_chat_id(grp_a, mem_b);

        // New stable FNV-1a mappings project into strictly disjoint top nibbles: 0x4, 0x5, 0x6.
        assert_eq!((-c2c_user) >> 60, 0x4);
        assert_eq!((-group_1) >> 60, 0x5);
        assert_eq!((-group_2) >> 60, 0x5);
        assert_eq!((-member_1) >> 60, 0x6);
        assert_eq!((-member_2) >> 60, 0x6);

        assert_ne!(c2c_user, group_1);
        assert_ne!(group_1, group_2);
        assert_ne!(member_1, member_2);
        assert_ne!(member_1, group_1);
        assert!(c2c_user < 0);
        assert!(group_1 < 0);
        assert!(group_2 < 0);
        assert!(member_1 < 0);
        assert!(member_2 < 0);
    }

    #[test]
    fn is_qq_chat_id_separates_platforms() {
        assert!(is_qq_chat_id(qq_openid_to_chat_id(
            "87233760CED716E069E2BF634E7BCB30"
        )));
        assert!(is_qq_chat_id(qq_group_openid_to_chat_id(
            "B55E344ED08A6C9175E12E4D9E50D325"
        )));
        assert!(is_qq_chat_id(qq_group_member_to_chat_id("g", "m")));
        // Legacy 0x7... IDs still classify as QQ IDs too.
        assert!(is_qq_chat_id(-8142664994840455790));
        // Telegram private chats, supergroups (-10^12 range) and small negative ids stay clear.
        assert!(!is_qq_chat_id(123_456_789));
        assert!(!is_qq_chat_id(-1_001_234_567_890));
        assert!(!is_qq_chat_id(-1));
    }

    #[test]
    fn legacy_qq_sessions_migrate_automatically_on_load() {
        let dir = std::env::temp_dir().join("agygram_session_tests_qq_migration");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage_path = dir.join("sessions.json");

        let user_id = "87233760CED716E069E2BF634E7BCB30";
        let group_id = "B55E344ED08A6C9175E12E4D9E50D325";

        let mut legacy_map = FxHashMap::default();
        let mut user_sess =
            ChatSession::new("/tmp".into(), "m".into(), "auto".into(), "high".into());
        user_sess.conversation_id = Some("conv-c2c".into());
        user_sess.turn_count = 7;
        legacy_map.insert(legacy_qq_openid_to_chat_id(user_id), user_sess);

        let mut grp_sess = ChatSession::new("/tmp".into(), "m".into(), "plan".into(), "low".into());
        grp_sess.conversation_id = Some("conv-grp".into());
        legacy_map.insert(legacy_qq_group_openid_to_chat_id(group_id), grp_sess);

        let mut mem_sess = ChatSession::new(
            "/tmp".into(),
            "m".into(),
            "accept-edits".into(),
            "max".into(),
        );
        mem_sess.conversation_id = Some("conv-mem".into());
        legacy_map.insert(
            legacy_qq_group_member_to_chat_id(group_id, user_id),
            mem_sess,
        );

        // Telegram session must remain untouched.
        let mut tg_sess = ChatSession::new("/tmp".into(), "m".into(), "auto".into(), "high".into());
        tg_sess.conversation_id = Some("conv-tg".into());
        legacy_map.insert(42_i64, tg_sess);

        std::fs::write(&storage_path, serde_json::to_vec(&legacy_map).unwrap()).unwrap();

        let mgr = SessionManager::load_from_storage(
            storage_path.clone(),
            "/tmp".into(),
            String::new(),
            "accept-edits".into(),
            "high".into(),
            &[user_id.to_string()],
            &[group_id.to_string()],
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let new_c2c = qq_openid_to_chat_id(user_id);
            let new_grp = qq_group_openid_to_chat_id(group_id);
            let new_mem = qq_group_member_to_chat_id(group_id, user_id);

            let map = mgr.sessions.read().await;
            assert_eq!(map.len(), 4);
            assert_eq!(
                map.get(&new_c2c).unwrap().conversation_id.as_deref(),
                Some("conv-c2c")
            );
            assert_eq!(map.get(&new_c2c).unwrap().turn_count, 7);
            assert_eq!(
                map.get(&new_grp).unwrap().conversation_id.as_deref(),
                Some("conv-grp")
            );
            assert_eq!(
                map.get(&new_mem).unwrap().conversation_id.as_deref(),
                Some("conv-mem")
            );
            assert_eq!(
                map.get(&42).unwrap().conversation_id.as_deref(),
                Some("conv-tg")
            );
            assert!(!map.contains_key(&legacy_qq_openid_to_chat_id(user_id)));
        });

        // Persisted file should already reflect the migrated IDs.
        let reloaded: FxHashMap<i64, ChatSession> =
            serde_json::from_slice(&std::fs::read(&storage_path).unwrap()).unwrap();
        assert!(reloaded.contains_key(&qq_openid_to_chat_id(user_id)));
        assert!(!reloaded.contains_key(&legacy_qq_openid_to_chat_id(user_id)));

        let _ = std::fs::remove_dir_all(&dir);
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
    async fn begin_turn_with_timeout_wakes_immediately_on_release_and_times_out_when_busy() {
        let mgr = test_manager("notify_wakeup");
        let g1 = mgr.begin_turn(10).await.unwrap();

        let mgr_rel = mgr.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            mgr_rel.release_turn(10, g1).await;
        });

        let start = Instant::now();
        let g2 = mgr
            .begin_turn_with_timeout(10, std::time::Duration::from_secs(2))
            .await
            .expect("should wake via Notify");
        assert_eq!(g2, 2);
        assert!(
            start.elapsed() < std::time::Duration::from_millis(120),
            "Notify wakeup took too long: {:?}",
            start.elapsed()
        );

        let timed_out = mgr
            .begin_turn_with_timeout(10, std::time::Duration::from_millis(20))
            .await;
        assert!(timed_out.is_none());
    }

    #[tokio::test]
    async fn concurrent_saves_preserve_latest_state() {
        let mgr = test_manager("concurrent_saves");
        let mut handles = Vec::new();
        for i in 0..20_i64 {
            let m = mgr.clone();
            handles.push(tokio::spawn(async move {
                m.set_project(i, format!("proj-{i}")).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let persisted: FxHashMap<i64, ChatSession> =
            serde_json::from_slice(&std::fs::read(&mgr.storage_path).unwrap()).unwrap();
        assert_eq!(persisted.len(), 20);
        for i in 0..20_i64 {
            assert_eq!(persisted.get(&i).unwrap().project, format!("proj-{i}"));
        }
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
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            injected_system_prompt_hash: None,
            duration_seconds: None,
            detached: false,
        };
        assert!(mgr.finish_turn(2, current, outcome).await);
    }

    #[tokio::test]
    async fn reset_mid_turn_is_not_undone_by_finish_turn() {
        let mgr = test_manager("reset_mid_turn");
        mgr.resume_conversation(6, Some("old-conv".to_string()))
            .await;
        let turn_gen = mgr.begin_turn(6).await.unwrap();

        // `/new` no longer queues behind the running turn, so it can land mid-flight.
        mgr.reset_conversation(6).await;

        let outcome = TurnOutcome {
            conversation_id: Some("old-conv".to_string()),
            input_chars: 10,
            output_chars: 20,
            usage: None,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            injected_system_prompt_hash: None,
            duration_seconds: Some(1.0),
            detached: false,
        };
        assert!(mgr.finish_turn(6, turn_gen, outcome).await);

        let session = mgr.get_or_create(6).await;
        assert_eq!(
            session.conversation_id, None,
            "the reset must survive the turn's cleanup"
        );
        assert!(
            session.is_first_turn,
            "the next prompt must start a fresh conversation"
        );
        assert_eq!(
            session.turn_count, 1,
            "telemetry still records the completed turn"
        );
    }

    #[tokio::test]
    async fn cancel_invalidates_the_killed_turns_writes() {
        let mgr = test_manager("cancel");
        let turn_gen = mgr.begin_turn(3).await.unwrap();

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
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            injected_system_prompt_hash: None,
            duration_seconds: Some(1.0),
            detached: false,
        };
        assert!(
            !mgr.finish_turn(3, turn_gen, stale).await,
            "a superseded turn must not write session state"
        );

        let session = mgr.get_or_create(3).await;
        assert_eq!(session.turn_count, 0, "telemetry must be untouched");
        assert_eq!(
            session.conversation_id, None,
            "conversation id must be untouched"
        );

        let fresh = mgr.begin_turn(3).await.unwrap();
        assert!(fresh > turn_gen);
        let ok = TurnOutcome {
            conversation_id: Some("real".to_string()),
            input_chars: 1,
            output_chars: 1,
            usage: None,
            context_tokens: None,
            num_turns: None,
            saw_checkpoint: false,
            injected_system_prompt_hash: None,
            duration_seconds: None,
            detached: false,
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
        let turn_gen = mgr.begin_turn(5).await.unwrap();
        {
            let _guard = TurnGuard::new(mgr.clone(), 5, turn_gen);
        }
        // The guard releases from a spawned task, so give the runtime a tick.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            mgr.begin_turn(5).await.is_some(),
            "the lock must not outlive the handler that held it"
        );
    }

    #[tokio::test]
    async fn turn_guard_disarm_skips_release() {
        let mgr = test_manager("guard_disarm");
        let turn_gen = mgr.begin_turn(7).await.unwrap();
        {
            let mut guard = TurnGuard::new(mgr.clone(), 7, turn_gen);
            guard.disarm();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            mgr.is_running(7).await,
            "disarmed guard must not release an active turn"
        );
        mgr.release_turn(7, turn_gen).await;
    }

    #[test]
    fn turn_guard_drop_outside_tokio_runtime_does_not_panic() {
        let mgr = test_manager("guard_no_rt");
        let guard = TurnGuard::new(mgr, 9, 1);
        drop(guard);
    }

    #[tokio::test]
    async fn resume_none_clears_old_conversation_id_and_effort_syncs_sibling_model() {
        let mgr = test_manager("resume_none_and_effort_sync");
        mgr.resume_conversation(100, Some("old-conv-id".to_string()))
            .await;
        assert_eq!(
            mgr.get_or_create(100).await.conversation_id.as_deref(),
            Some("old-conv-id")
        );

        mgr.resume_conversation(100, None).await;
        let after = mgr.get_or_create(100).await;
        assert_eq!(after.conversation_id, None);
        assert!(!after.is_first_turn);

        let inferred = mgr
            .set_model_and_sync_effort(100, "gemini-4-carbon-high".to_string())
            .await;
        assert_eq!(inferred, Some(Effort::High));

        let (synced_model, changed) = mgr.set_effort_and_sync_model(100, Effort::Low).await;
        assert!(changed);
        assert_eq!(synced_model, "gemini-4-carbon-low");
        assert_eq!(mgr.get_or_create(100).await.effort, "low");
    }

    #[test]
    fn load_from_storage_preserves_5_5_models_and_normalizes_neon_alias() {
        let dir = std::env::temp_dir().join(format!("agygram_sess_mig_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let storage_path = dir.join("sessions.json");

        let mut map = FxHashMap::default();
        map.insert(
            77_i64,
            ChatSession::new(
                "/tmp".into(),
                "claude-opus-5-5-high".into(),
                "accept-edits".into(),
                "high".into(),
            ),
        );
        map.insert(
            78_i64,
            ChatSession::new(
                "/tmp".into(),
                "neon".into(),
                "accept-edits".into(),
                "high".into(),
            ),
        );
        std::fs::write(&storage_path, serde_json::to_vec(&map).unwrap()).unwrap();

        let mgr = SessionManager::load_from_storage(
            storage_path,
            "/tmp".into(),
            String::new(),
            "accept-edits".into(),
            "high".into(),
            &[],
            &[],
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(
                mgr.get_or_create(77).await.model,
                "claude-opus-5-5-high",
                "Claude 5.5 models must be preserved without downgrading to 4.6"
            );
            assert_eq!(mgr.get_or_create(78).await.model, "gemini-4-neon-high");
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn finish_turn_overwrites_within_same_conversation_and_heals_legacy_inflated_totals() {
        let mgr = test_manager("token_overwrite_and_heal");

        // Seed a legacy inflated session (as from pre-v1.7 O(N^2) accumulation, with last_reported_conv_id = None).
        {
            let mut map = mgr.sessions.write().await;
            let mut legacy =
                ChatSession::new("/tmp".into(), String::new(), "auto".into(), "high".into());
            legacy.conversation_id = Some("conv-1".into());
            legacy.total_input_tokens = 99_000_000;
            legacy.total_output_tokens = 5_000_000;
            legacy.total_duration_seconds = 40_000_000.0;
            map.insert(201, legacy);
        }

        // Turn 1 on conv-1: overwrites legacy inflated totals cleanly (self-healing).
        let g1 = mgr.begin_turn(201).await.unwrap();
        let u1 = TokenUsage {
            input_tokens: 100_000,
            output_tokens: 2_000,
            thinking_tokens: 500,
            cache_read_tokens: 40_000,
            ..Default::default()
        };
        assert!(
            mgr.finish_turn(
                201,
                g1,
                TurnOutcome {
                    conversation_id: Some("conv-1".into()),
                    input_chars: 10,
                    output_chars: 20,
                    usage: Some(&u1),
                    context_tokens: Some(140_000),
                    num_turns: Some(1),
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: Some(12.5),
                    detached: false,
                },
            )
            .await
        );
        let s1 = mgr.get_or_create(201).await;
        assert_eq!(s1.total_input_tokens, 100_000);
        assert_eq!(s1.total_output_tokens, 2_000);
        assert_eq!(s1.total_thinking_tokens, 500);
        assert_eq!(s1.total_cache_read_tokens, 40_000);
        assert!((s1.total_duration_seconds - 12.5).abs() < 1e-6);
        assert_eq!(s1.last_turn_input_tokens, 100_000);
        assert_eq!(s1.last_context_tokens, 140_000);

        // Turn 2 on same conv-1: cumulative reported values overwrite instead of O(N^2) summing.
        let g2 = mgr.begin_turn(201).await.unwrap();
        let u2 = TokenUsage {
            input_tokens: 450_000,
            output_tokens: 5_000,
            thinking_tokens: 1_200,
            cache_read_tokens: 200_000,
            ..Default::default()
        };
        assert!(
            mgr.finish_turn(
                201,
                g2,
                TurnOutcome {
                    conversation_id: Some("conv-1".into()),
                    input_chars: 15,
                    output_chars: 30,
                    usage: Some(&u2),
                    context_tokens: Some(180_000),
                    num_turns: Some(3),
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: Some(30.0),
                    detached: false,
                },
            )
            .await
        );
        let s2 = mgr.get_or_create(201).await;
        assert_eq!(s2.total_input_tokens, 450_000);
        assert_eq!(s2.total_output_tokens, 5_000);
        assert_eq!(s2.total_thinking_tokens, 1_200);
        assert_eq!(s2.total_cache_read_tokens, 200_000);
        assert!((s2.total_duration_seconds - 30.0).abs() < 1e-6);
        assert_eq!(s2.last_turn_input_tokens, 350_000);
        assert_eq!(s2.last_context_tokens, 180_000);

        // In-place switch to conv-2 rolls conv-1 totals into conv_base_*.
        let g3 = mgr.begin_turn(201).await.unwrap();
        let u3 = TokenUsage {
            input_tokens: 50_000,
            output_tokens: 1_000,
            thinking_tokens: 100,
            cache_read_tokens: 10_000,
            ..Default::default()
        };
        assert!(
            mgr.finish_turn(
                201,
                g3,
                TurnOutcome {
                    conversation_id: Some("conv-2".into()),
                    input_chars: 5,
                    output_chars: 5,
                    usage: Some(&u3),
                    context_tokens: Some(60_000),
                    num_turns: Some(1),
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: Some(4.0),
                    detached: false,
                },
            )
            .await
        );
        let s3 = mgr.get_or_create(201).await;
        assert_eq!(s3.conv_base_input_tokens, 450_000);
        assert_eq!(s3.total_input_tokens, 500_000);
        assert!((s3.total_duration_seconds - 34.0).abs() < 1e-6);
        assert_eq!(s3.last_turn_input_tokens, 50_000);

        // `/resume` resets baselines cleanly.
        mgr.resume_conversation(201, Some("conv-3".into())).await;
        let s_resumed = mgr.get_or_create(201).await;
        assert_eq!(s_resumed.last_reported_conv_id, None);
        assert_eq!(s_resumed.conv_base_input_tokens, 0);
        assert_eq!(s_resumed.total_input_tokens, 0);
        assert_eq!(s_resumed.last_turn_input_tokens, 0);

        // `/new` resets baselines cleanly.
        mgr.reset_conversation(201).await;
        let s_reset = mgr.get_or_create(201).await;
        assert_eq!(s_reset.last_reported_conv_id, None);
        assert_eq!(s_reset.conv_base_input_tokens, 0);
        assert_eq!(s_reset.total_input_tokens, 0);
        assert_eq!(s_reset.last_turn_input_tokens, 0);
    }

    #[tokio::test]
    async fn detached_pids_survive_begin_turn_and_clear_on_finish_or_cancel() {
        let mgr = test_manager("detached_pids");
        let g1 = mgr.begin_turn(301).await.unwrap();
        mgr.set_pid(301, 77_001, g1).await;

        assert!(
            mgr.finish_turn(
                301,
                g1,
                TurnOutcome {
                    conversation_id: Some("conv-d".into()),
                    input_chars: 5,
                    output_chars: 5,
                    usage: None,
                    context_tokens: None,
                    num_turns: None,
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: Some(1.0),
                    detached: true,
                },
            )
            .await
        );

        let after_detach = mgr.get_or_create(301).await;
        assert_eq!(after_detach.active_pid, None);
        assert_eq!(after_detach.detached_pids, vec![(g1, 77_001)]);

        // Starting a new foreground turn must NOT disturb detached_pids.
        let g2 = mgr.begin_turn(301).await.unwrap();
        mgr.set_pid(301, 77_002, g2).await;
        let during_g2 = mgr.get_or_create(301).await;
        assert_eq!(during_g2.active_pid, Some(77_002));
        assert_eq!(during_g2.detached_pids, vec![(g1, 77_001)]);

        // clear_detached_pid for g1 removes g1 from detached_pids without touching g2's active_pid.
        mgr.clear_detached_pid(301, g1).await;
        let after_clear = mgr.get_or_create(301).await;
        assert!(after_clear.detached_pids.is_empty());
        assert_eq!(after_clear.active_pid, Some(77_002));

        // Detach g2, then cancel_task drains both active and detached PIDs.
        assert!(
            mgr.finish_turn(
                301,
                g2,
                TurnOutcome {
                    conversation_id: Some("conv-d".into()),
                    input_chars: 1,
                    output_chars: 1,
                    usage: None,
                    context_tokens: None,
                    num_turns: None,
                    saw_checkpoint: false,
                    injected_system_prompt_hash: None,
                    duration_seconds: None,
                    detached: true,
                },
            )
            .await
        );
        assert_eq!(
            mgr.get_or_create(301).await.detached_pids,
            vec![(g2, 77_002)]
        );
        let (_pid, _was_running) = mgr.cancel_task(301).await;
        assert!(mgr.get_or_create(301).await.detached_pids.is_empty());

        // shutdown_all_processes clears active and detached PIDs across all sessions.
        let g3 = mgr.begin_turn(301).await.unwrap();
        mgr.set_pid(301, 77_003, g3).await;
        mgr.shutdown_all_processes().await;
        let after_shutdown = mgr.get_or_create(301).await;
        assert!(!after_shutdown.is_running);
        assert_eq!(after_shutdown.active_pid, None);
        assert!(after_shutdown.detached_pids.is_empty());
    }

    #[tokio::test]
    async fn global_exec_slot_semaphore_limits_concurrency_respects_cancel_and_reports_status() {
        let dir = std::env::temp_dir().join("agygram_session_tests_exec_slots");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = Arc::new(
            SessionManager::with_storage_path(
                dir.to_string_lossy().to_string(),
                String::new(),
                "accept-edits".to_string(),
                dir.join("sessions.json"),
            )
            .with_max_concurrent_agy(2),
        );

        let slot1 = mgr.acquire_exec_slot(None).await.expect("slot 1");
        let slot2 = mgr.acquire_exec_slot(None).await.expect("slot 2");

        // Running turns on both TG and QQ chats show up in global_status_snapshot.
        let _tg_gen = mgr.begin_turn(12345).await.unwrap();
        let qq_chat = qq_openid_to_chat_id("USER_OPENID");
        let _qq_gen = mgr.begin_turn(qq_chat).await.unwrap();

        let snap = mgr.global_status_snapshot().await;
        assert_eq!(snap.tg_running, 1);
        assert_eq!(snap.qq_running, 1);
        assert_eq!(snap.active_exec_slots, 2);
        assert_eq!(snap.max_exec_slots, 2);

        // A third waiter must respect cancellation while all slots are busy.
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel_trigger.cancel();
        });
        assert!(
            mgr.acquire_exec_slot(Some(&cancel)).await.is_none(),
            "cancelled waiter must not hang on full semaphore"
        );

        // Dropping slot1 immediately unblocks a waiting caller.
        let mgr_wait = mgr.clone();
        let waiter = tokio::spawn(async move { mgr_wait.acquire_exec_slot(None).await });
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        drop(slot1);
        let slot3 = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter must wake")
            .unwrap()
            .expect("slot 3 acquired");

        drop(slot2);
        drop(slot3);
        assert_eq!(mgr.global_status_snapshot().await.active_exec_slots, 0);

        // max_concurrent_agy == 0 disables semaphore cap (unlimited) while still respecting cancel.
        let unlimited = SessionManager::with_storage_path(
            dir.to_string_lossy().to_string(),
            String::new(),
            "accept-edits".to_string(),
            dir.join("sessions_unlimited.json"),
        )
        .with_max_concurrent_agy(0);
        let _u1 = unlimited.acquire_exec_slot(None).await.expect("u1");
        let _u2 = unlimited.acquire_exec_slot(None).await.expect("u2");
        let _u3 = unlimited.acquire_exec_slot(None).await.expect("u3");
        let pre_cancelled = CancellationToken::new();
        pre_cancelled.cancel();
        assert!(
            unlimited
                .acquire_exec_slot(Some(&pre_cancelled))
                .await
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wave3_session_settings_persist_and_deserialize_defaults() {
        let mgr = test_manager("wave3_settings");
        mgr.set_show_thinking(401, Some(true)).await;
        assert!(mgr.toggle_remote_agent(401).await);
        mgr.set_orchestrator_override(401, Some(" gemini-4-neon-xhigh ".into()), Some("".into()))
            .await;

        let s = mgr.get_or_create(401).await;
        assert_eq!(s.show_thinking, Some(true));
        assert!(s.remote_agent);
        assert_eq!(
            s.orchestrator_planner.as_deref(),
            Some("gemini-4-neon-xhigh")
        );
        assert_eq!(s.orchestrator_executor, None);

        mgr.set_remote_agent(401, false).await;
        assert!(!mgr.get_or_create(401).await.remote_agent);

        // Legacy JSON missing v1.7 fields deserializes cleanly with defaults.
        let legacy_json = r#"{"conversation_id":"c-old","workspace":"/tmp"}"#;
        let parsed: ChatSession = serde_json::from_str(legacy_json).expect("legacy json parses");
        assert_eq!(parsed.show_thinking, None);
        assert!(!parsed.remote_agent);
        assert_eq!(parsed.orchestrator_planner, None);
        assert_eq!(parsed.orchestrator_executor, None);
        assert_eq!(parsed.last_reported_conv_id, None);
        assert_eq!(parsed.last_turn_input_tokens, 0);
        assert_eq!(parsed.last_context_tokens, 0);
        assert_eq!(parsed.last_system_prompt_hash, None);
        assert!(parsed.detached_pids.is_empty());
    }

    #[tokio::test]
    async fn context_watermark_prompt_dedup_cooldown_and_bg_compact_lifecycle() {
        let mgr = test_manager("v171_compaction_lifecycle");

        // Turn 1: multi-step turn with 197K + 198K = 396K cumulative input_tokens,
        // but true single-step context_tokens is only 198,613.
        let g1 = mgr.begin_turn(501).await.unwrap();
        let u1 = TokenUsage {
            input_tokens: 396_056,
            output_tokens: 966,
            cache_read_tokens: 0,
            ..Default::default()
        };
        let sys_hash = hash_system_prompt("qq default rules");
        assert!(
            mgr.finish_turn(
                501,
                g1,
                TurnOutcome {
                    conversation_id: Some("conv-501".into()),
                    input_chars: 100,
                    output_chars: 200,
                    usage: Some(&u1),
                    context_tokens: Some(198_613),
                    num_turns: Some(2),
                    saw_checkpoint: false,
                    injected_system_prompt_hash: Some(sys_hash),
                    duration_seconds: Some(15.0),
                    detached: false,
                },
            )
            .await
        );

        let s1 = mgr.get_or_create(501).await;
        assert_eq!(s1.last_context_tokens, 198_613);
        assert_eq!(s1.last_system_prompt_hash, Some(sys_hash));
        assert!(
            !mgr.should_show_compact_hint(501, 300_000).await,
            "198K real context must not trigger 300K hint even when cumulative sum is 396K"
        );

        // Advance 5 turns and push real context above 360K threshold.
        for turn in 0..5 {
            let g = mgr.begin_turn(501).await.unwrap();
            assert!(
                mgr.finish_turn(
                    501,
                    g,
                    TurnOutcome {
                        conversation_id: Some("conv-501".into()),
                        input_chars: 20,
                        output_chars: 20,
                        usage: None,
                        context_tokens: Some(370_000 + turn * 2_000),
                        num_turns: Some(1),
                        saw_checkpoint: false,
                        injected_system_prompt_hash: None,
                        duration_seconds: Some(3.0),
                        detached: false,
                    },
                )
                .await
            );
        }

        // Manual hint cooldown: first check true, immediately after recording -> false until +10 turns or +80K.
        assert!(mgr.should_show_compact_hint(501, 360_000).await);
        mgr.record_compact_hint_shown(501).await;
        assert!(!mgr.should_show_compact_hint(501, 360_000).await);

        // Another chat running blocks try_begin_bg_compact to protect low-RAM VPS.
        let other_g = mgr.begin_turn(502).await.unwrap();
        assert!(
            mgr.try_begin_bg_compact(501, "conv-501", 6, 360_000, 5)
                .await
                .is_none()
        );
        mgr.release_turn(502, other_g).await;

        // Now host is idle -> try_begin_bg_compact succeeds and sets is_bg_compacting.
        let (bg_gen, _snap) = mgr
            .try_begin_bg_compact(501, "conv-501", 6, 360_000, 5)
            .await
            .expect("bg compact claims idle chat");
        assert!(mgr.is_bg_compacting(501).await);
        assert!(mgr.begin_turn(501).await.is_none());

        // Completing bg compact resets context watermark, clears system prompt hash, and updates cooldown.
        mgr.finish_bg_compact(501, bg_gen, true, Some(0), true)
            .await;
        assert!(!mgr.is_bg_compacting(501).await);
        let after = mgr.get_or_create(501).await;
        assert_eq!(after.last_context_tokens, 0);
        assert_eq!(
            after.last_system_prompt_hash, None,
            "checkpoint must clear static system-prompt hash so next turn refreshes rules"
        );
        assert_eq!(after.last_auto_compact_turn, 6);
        assert_eq!(
            after.turn_count, 6,
            "bg compact must not inflate user turn_count"
        );
    }
}
