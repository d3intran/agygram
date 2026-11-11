use crate::config::{ensure_valid_workspace, expand_tilde, Config};
use crate::executor::TokenUsage;
use crate::models::{Effort, Mode};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Notify, RwLock};
use tracing::{error, info};

/// Keeps concurrent saves from sharing one temp filename.
static SAVE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const FNV1A_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;
const QQ_NAMESPACE_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;

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
            is_running: false,
            is_first_turn: true,
            active_pid: None,
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
            session_start_time: now,
            last_active_time: now,
        }
    }

    /// Typed thinking effort parsed from [`Self::effort`].
    #[inline]
    #[must_use]
    pub fn effort_enum(&self) -> Effort {
        self.effort.parse().unwrap_or_default()
    }

    /// Typed execution mode parsed from [`Self::mode`].
    #[inline]
    #[must_use]
    pub fn mode_enum(&self) -> Mode {
        self.mode.parse().unwrap_or_default()
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
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SessionManager {
    sessions: Arc<RwLock<FxHashMap<i64, ChatSession>>>,
    save_lock: Arc<Mutex<()>>,
    turn_notify: Arc<Notify>,
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
    pub fn new(
        default_workspace: String,
        default_model: String,
        default_mode: String,
        default_effort: String,
    ) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let storage_path = home.join(".config").join("agygram").join("sessions.json");
        Self::load_from_storage(
            storage_path,
            default_workspace,
            default_model,
            default_mode,
            default_effort,
            &[],
            &[],
        )
    }

    /// Creates a [`SessionManager`] from [`Config`] and automatically migrates any legacy QQ
    /// session IDs matching configured whitelists on load.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let storage_path = home.join(".config").join("agygram").join("sessions.json");
        Self::load_from_storage(
            storage_path,
            config.get_valid_workspace(),
            config.default_model.clone(),
            config.default_mode.clone(),
            config.default_effort.clone(),
            &config.qq_allowed_openids,
            &config.qq_allowed_groups,
        )
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
        if let Some(parent) = storage_path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                error!("无法创建会话目录 {}: {:?}", parent.display(), err);
            }
        }

        let mut initial_map = FxHashMap::default();
        match std::fs::read(&storage_path) {
            Ok(bytes) => match serde_json::from_slice::<FxHashMap<i64, ChatSession>>(&bytes) {
                Ok(mut loaded) => {
                    let migrated = migrate_legacy_qq_map(&mut loaded, qq_openids, qq_groups);
                    if migrated > 0 {
                        info!("已自动迁移 {} 个旧版 QQ 会话 ID 至稳定命名空间", migrated);
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
        let (gen, stale_pid) = {
            let mut map = self.sessions.write().await;
            let session = map
                .entry(chat_id)
                .or_insert_with(|| self.create_default_session());
            if session.is_running {
                return None;
            }
            let stale_pid = session.active_pid.take();
            session.is_running = true;
            session.generation += 1;
            session.turn_started_epoch = session.conversation_epoch;
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

            if let Some(gen) = self.begin_turn(chat_id).await {
                return Some(gen);
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
        let released = {
            let mut map = self.sessions.write().await;
            if !Self::generation_is_current(&map, chat_id, generation) {
                return;
            }
            if let Some(session) = map.get_mut(&chat_id) {
                if session.is_running {
                    session.is_running = false;
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
            let now = Instant::now();
            session.session_start_time = now;
            session.last_active_time = now;
        })
        .await;
    }

    /// Returns all `(chat_id, conversation_id)` pairs for sessions that are currently idle
    /// (no foreground or detached `agy` process running). `/proc` probes happen outside `RwLock`.
    pub async fn get_idle_conversations(&self) -> Vec<(i64, String)> {
        let candidates: Vec<(i64, Option<u32>, String)> = {
            let map = self.sessions.read().await;
            map.iter()
                .filter_map(|(&chat_id, s)| {
                    if !s.is_running {
                        s.conversation_id
                            .as_ref()
                            .map(|cid| (chat_id, s.active_pid, cid.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        candidates
            .into_iter()
            .filter_map(|(chat_id, active_pid, cid)| {
                let has_live_process = active_pid.is_some_and(Self::process_needs_signal);
                (!has_live_process).then_some((chat_id, cid))
            })
            .collect()
    }

    pub async fn resume_conversation(&self, chat_id: i64, conv_id: Option<String>) {
        self.mutate_and_save(chat_id, true, |session| {
            session.conversation_epoch += 1;
            session.is_first_turn = false;
            if conv_id.is_some() {
                session.conversation_id = conv_id;
            }
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
        let pid_to_probe = {
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
            if let Some(u) = outcome.usage {
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
            if let Some(d) = outcome.duration_seconds {
                if d.is_finite() && d >= 0.0 {
                    session.total_duration_seconds += d;
                }
            }
            session.last_active_time = Instant::now();
            if !conversation_switched && outcome.conversation_id.is_some() {
                session.conversation_id = outcome.conversation_id;
            }
            session.is_running = false;
            session.active_pid
        };

        self.turn_notify.notify_waiters();

        if let Some(pid) = pid_to_probe {
            if !Self::process_needs_signal(pid) {
                let mut map = self.sessions.write().await;
                if let Some(session) = map.get_mut(&chat_id) {
                    if session.generation == generation && session.active_pid == Some(pid) {
                        session.active_pid = None;
                    }
                }
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

    pub async fn set_model(&self, chat_id: i64, model: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.model = model;
        })
        .await;
    }

    pub async fn set_effort(&self, chat_id: i64, effort: String) {
        self.mutate_and_save(chat_id, true, |session| {
            session.effort = effort;
        })
        .await;
    }

    pub async fn set_effort_enum(&self, chat_id: i64, effort: Effort) {
        self.set_effort(chat_id, effort.as_str().to_string()).await;
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
        })
        .await;
    }

    /// Interrupt whatever is running in this chat and retire its generation.
    ///
    /// SIGINT first and SIGKILL only if the tree is still alive after a grace window: agy needs
    /// that window to flush `transcript.jsonl`, which is exactly what `/history` and `-c` resume
    /// read back.
    pub async fn cancel_task(&self, chat_id: i64) -> (Option<u32>, bool) {
        let (was_flag_running, pid) = {
            let mut map = self.sessions.write().await;
            let Some(session) = map.get_mut(&chat_id) else {
                return (None, false);
            };
            let was_flag_running = session.is_running;
            let pid = session.active_pid;
            // Bumping the generation makes the killed task's own cleanup a no-op, so it cannot
            // race the turn that replaces it.
            session.generation += 1;
            session.is_running = false;
            session.active_pid = None;
            (was_flag_running, pid)
        };

        if was_flag_running {
            self.turn_notify.notify_waiters();
        }

        let Some(pid) = pid else {
            return (None, was_flag_running);
        };

        let live = Self::process_needs_signal(pid);
        let was_running = was_flag_running || live;
        if live {
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
        }

        (Some(pid), was_running)
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
            duration_seconds: None,
        };
        assert!(mgr.finish_turn(2, current, outcome).await);
    }

    #[tokio::test]
    async fn reset_mid_turn_is_not_undone_by_finish_turn() {
        let mgr = test_manager("reset_mid_turn");
        mgr.resume_conversation(6, Some("old-conv".to_string()))
            .await;
        let gen = mgr.begin_turn(6).await.unwrap();

        // `/new` no longer queues behind the running turn, so it can land mid-flight.
        mgr.reset_conversation(6).await;

        let outcome = TurnOutcome {
            conversation_id: Some("old-conv".to_string()),
            input_chars: 10,
            output_chars: 20,
            usage: None,
            duration_seconds: Some(1.0),
        };
        assert!(mgr.finish_turn(6, gen, outcome).await);

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
        assert_eq!(
            session.conversation_id, None,
            "conversation id must be untouched"
        );

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

    #[tokio::test]
    async fn turn_guard_disarm_skips_release() {
        let mgr = test_manager("guard_disarm");
        let gen = mgr.begin_turn(7).await.unwrap();
        {
            let mut guard = TurnGuard::new(mgr.clone(), 7, gen);
            guard.disarm();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(
            mgr.is_running(7).await,
            "disarmed guard must not release an active turn"
        );
        mgr.release_turn(7, gen).await;
    }

    #[test]
    fn turn_guard_drop_outside_tokio_runtime_does_not_panic() {
        let mgr = test_manager("guard_no_rt");
        let guard = TurnGuard::new(mgr, 9, 1);
        drop(guard);
    }
}
