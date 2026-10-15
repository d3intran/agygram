pub mod protocol;
pub mod watchdog;
pub use protocol::*;
pub use watchdog::*;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub struct AgyExecutor;

static AGY_EXECUTABLE_PATH: OnceLock<PathBuf> = OnceLock::new();
static STDBUF_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn get_agy_path() -> &'static PathBuf {
    AGY_EXECUTABLE_PATH.get_or_init(|| {
        if let Ok(p) = which::which("agy") {
            return p;
        }
        let usr_local = PathBuf::from("/usr/local/bin/agy");
        if usr_local.exists() {
            return usr_local;
        }
        if let Some(home) = dirs::home_dir() {
            let user_local = home.join(".local/bin/agy");
            if user_local.exists() {
                return user_local;
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
    STDBUF_PATH.get_or_init(|| which::which("stdbuf").ok()).as_ref()
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

impl AgyExecutor {
    /// Executes agy using stream-json format and streams structured events line-by-line with unbuffered pipes.
    pub async fn run_prompt_streaming(
        params: ExecutionParams<'_>,
        stream_tx: mpsc::Sender<StreamEvent>,
        pid_tx: Option<tokio::sync::oneshot::Sender<u32>>,
        idle_timeout_secs: u64,
        max_execution_secs: u64,
    ) -> Result<ExecutionResult> {
        info!(
            "Executing agy with stream-json format (first turn: {}): {}",
            params.is_first_turn, params.prompt
        );

        let final_prompt = if !params.system_prompt.is_empty() {
            format!("[System Instructions: {}]\n{}", params.system_prompt, params.prompt)
        } else {
            params.prompt.to_string()
        };

        let agy_bin = get_agy_path();
        let mut cmd = if let Some(stdbuf) = get_stdbuf_path() {
            let mut c = Command::new(stdbuf);
            c.arg("-oL").arg("-eL").arg(agy_bin);
            c
        } else {
            Command::new(agy_bin)
        };

        cmd.process_group(0);
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");
        cmd.env("LANG", "C.UTF-8");
        cmd.env("LC_ALL", "C.UTF-8");

        cmd.arg("-p")
            .arg(&final_prompt)
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

        for dir in params.extra_dirs {
            cmd.arg("--add-dir").arg(dir);
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

        let mut child = cmd.spawn().context("Failed to spawn agy CLI process")?;
        let child_pid = child.id();
        if let Some(pid) = child_pid {
            if let Some(tx) = pid_tx {
                let _ = tx.send(pid);
            }
        }
        let stdout = child.stdout.take().context("Failed to capture agy stdout")?;
        let stderr = child.stderr.take().context("Failed to capture agy stderr")?;

        let tracker = ActivityTracker::new();
        let tracker_stdout = tracker.clone();
        let tracker_stderr = tracker.clone();

        let stream_tx_clone = stream_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut accumulated_text = String::new();
            let mut detected_conv_id: Option<String> = None;
            let mut final_status = "SUCCESS".to_string();
            let mut final_usage: Option<TokenUsage> = None;
            let mut final_duration: Option<f64> = None;

            while let Ok(Some(line)) = stdout_reader.next_line().await {
                tracker_stdout.record_activity();
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Ok(parsed) = serde_json::from_str::<RawAgyJsonEvent>(trimmed) {
                    match parsed {
                        RawAgyJsonEvent::Init { conversation_id, .. } => {
                            if let Some(cid) = conversation_id {
                                detected_conv_id = Some(cid.clone());
                                let _ = stream_tx_clone.send(StreamEvent::Init { conversation_id: cid }).await;
                            }
                        }
                        RawAgyJsonEvent::StepUpdate { step_update } => {
                            if let Some(cid) = step_update.conversation_id {
                                if detected_conv_id.is_none() {
                                    detected_conv_id = Some(cid.clone());
                                    let _ = stream_tx_clone.send(StreamEvent::Init { conversation_id: cid }).await;
                                }
                            }

                            if let Some(thinking) = step_update.thinking_delta {
                                let _ = stream_tx_clone.send(StreamEvent::ThinkingDelta(thinking)).await;
                            }

                            if let Some(delta) = step_update.text_delta {
                                accumulated_text.push_str(&delta);
                                let _ = stream_tx_clone.send(StreamEvent::TextDelta(delta)).await;
                            }

                            if let Some(u) = step_update.usage {
                                final_usage = Some(u);
                            }
                            if let Some(d) = step_update.duration_seconds {
                                final_duration = Some(d);
                            }

                            let maybe_tool = step_update.tool_name
                                .or_else(|| {
                                    step_update.tool_info.as_ref().and_then(|info| {
                                        info.get("name").and_then(|v| v.as_str()).map(|s| s.to_string())
                                    })
                                })
                                .or_else(|| {
                                    step_update.tool_call.as_ref().and_then(|call| {
                                        call.get("name").or_else(|| call.get("tool")).and_then(|v| v.as_str()).map(|s| s.to_string())
                                    })
                                });

                            if let Some(tool_name) = maybe_tool {
                                let summary = if let Some(ref info) = step_update.tool_info {
                                    if let Some(params) = info.get("parameters") {
                                        params.to_string()
                                    } else {
                                        String::new()
                                    }
                                } else if let Some(ref call) = step_update.tool_call {
                                    call.to_string()
                                } else {
                                    String::new()
                                };
                                let _ = stream_tx_clone.send(StreamEvent::ToolUse { tool: tool_name, summary }).await;
                            }
                        }
                        RawAgyJsonEvent::Result { result } => {
                            if let Some(cid) = result.conversation_id {
                                detected_conv_id = Some(cid);
                            }
                            if let Some(st) = result.status {
                                final_status = st;
                            }
                            if let Some(u) = result.usage {
                                final_usage = Some(u);
                            }
                            if let Some(d) = result.duration_seconds {
                                final_duration = Some(d);
                            }
                            if let Some(resp) = result.response {
                                if !resp.trim().is_empty() {
                                    accumulated_text = resp;
                                }
                            }
                        }
                        RawAgyJsonEvent::Unknown => {}
                    }
                } else {
                    accumulated_text.push_str(trimmed);
                    accumulated_text.push('\n');
                    let _ = stream_tx_clone.send(StreamEvent::TextDelta(format!("{}\n", trimmed))).await;
                }
            }

            (accumulated_text, detected_conv_id, final_status, final_usage, final_duration)
        });

        let stderr_task = tokio::spawn(async move {
            let mut stderr_reader = BufReader::new(stderr).lines();
            let mut accumulated_stderr = String::new();
            while let Ok(Some(line)) = stderr_reader.next_line().await {
                tracker_stderr.record_activity();
                accumulated_stderr.push_str(&line);
                accumulated_stderr.push('\n');
            }
            accumulated_stderr
        });

        let idle_timeout = Duration::from_secs(idle_timeout_secs.max(30));
        let max_exec_timeout = Duration::from_secs(max_execution_secs.max(60));
        let probe_interval = Duration::from_secs(2);
        let start_time = std::time::Instant::now();

        let mut prev_metrics = ProcessTreeMetrics::default();
        let mut consecutive_silent_probes = 0u32;
        let mut timeout_reason: Option<TimeoutReason> = None;
        let mut exit_status = None;

        loop {
            tokio::select! {
                status_res = child.wait() => {
                    exit_status = Some(status_res);
                    break;
                }
                _ = tokio::time::sleep(probe_interval) => {
                    let total_elapsed = start_time.elapsed();
                    // 1. Hard maximum ceiling timeout
                    if total_elapsed >= max_exec_timeout {
                        timeout_reason = Some(TimeoutReason::MaxExecutionExceeded {
                            max_secs: max_exec_timeout.as_secs(),
                        });
                        break;
                    }

                    // 2. Check idle duration
                    let idle_duration = tracker.idle_duration();
                    if idle_duration < idle_timeout {
                        consecutive_silent_probes = 0;
                        continue;
                    }

                    // 3. Deep Process Tree Inspection
                    if let Some(pid) = child_pid {
                        let inspection = inspect_process_tree(pid);

                        // Check fatal deadlock: blocked waiting for terminal stdin in non-interactive background
                        if let Some(cmd) = inspection.stdin_blocked_command {
                            warn!("Process tree deadlock detected: PID {} blocked waiting on stdin: {}", pid, cmd);
                            timeout_reason = Some(TimeoutReason::StdinDeadlock { command: cmd });
                            break;
                        }

                        let cpu_delta = inspection.metrics.total_cpu_ticks.saturating_sub(prev_metrics.total_cpu_ticks);
                        let io_delta = inspection.metrics.total_io_bytes.saturating_sub(prev_metrics.total_io_bytes);
                        prev_metrics = inspection.metrics;

                        if cpu_delta > 0 || io_delta > 0 {
                            debug!(
                                "Process tree (PID {}) is active: +{} CPU ticks, +{} IO bytes, {} active processes ({:?}). Extending idle deadline.",
                                pid, cpu_delta, io_delta, inspection.metrics.process_count, inspection.active_process_names
                            );
                            tracker.record_activity();
                            consecutive_silent_probes = 0;
                            continue;
                        }

                        consecutive_silent_probes += 1;
                        // Require at least 2 consecutive silent probes after idle_timeout has elapsed
                        if consecutive_silent_probes >= 2 {
                            timeout_reason = Some(TimeoutReason::FrozenDeadlock {
                                idle_secs: idle_duration.as_secs(),
                                process_count: inspection.metrics.process_count,
                                active_processes: inspection.active_process_names,
                            });
                            break;
                        }
                    } else {
                        timeout_reason = Some(TimeoutReason::FrozenDeadlock {
                            idle_secs: idle_duration.as_secs(),
                            process_count: 0,
                            active_processes: Vec::new(),
                        });
                        break;
                    }
                }
            }
        }

        let status = if let Some(reason) = timeout_reason {
            warn!("agy process watchdog triggered: {:?}, killing process group...", reason);
            if let Some(pid) = child_pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            stdout_task.abort();
            stderr_task.abort();

            let (error_event, summary_msg) = match reason {
                TimeoutReason::StdinDeadlock { command } => (
                    format!(
                        "agy 任务终止：检测到子进程卡死在等待终端交互输入 (<code>{}</code>)。由于非交互模式无法输入，已自动终止。",
                        crate::bot::formatter::TelegramFormatter::escape_html(&command)
                    ),
                    format!(
                        "⚠️ <b>任务死锁终止</b>\n检测到后台子进程正在等待终端用户输入：\n<code>{}</code>\n非交互模式无法响应，已自动终止以防挂起。",
                        crate::bot::formatter::TelegramFormatter::escape_html(&command)
                    ),
                ),
                TimeoutReason::FrozenDeadlock { idle_secs, process_count: _, active_processes } => {
                    let proc_str = if active_processes.is_empty() {
                        "无活动子进程".to_string()
                    } else {
                        active_processes.join(", ")
                    };
                    (
                        format!(
                            "agy 执行卡死 ({}s 无任何输出且系统 0 算力/IO 消耗)，任务已终止。关联进程: {}",
                            idle_secs, proc_str
                        ),
                        format!(
                            "⏱ <b>任务卡死终止</b>\n超过 {}s 无任何输出，且系统监测到 0 算力消耗与 0 I/O 进展（可能网络连接静默断开或死锁）。\n关联进程: <code>{}</code>\n\n请发送 /new 重置会话后重试。",
                            idle_secs,
                            crate::bot::formatter::TelegramFormatter::escape_html(&proc_str)
                        ),
                    )
                }
                TimeoutReason::MaxExecutionExceeded { max_secs } => (
                    format!("agy 执行超时 (超过全局安全上限 {}s)，任务已强制终止。", max_secs),
                    format!(
                        "⏱ <b>任务超时终止</b>\n任务总运行时间已超过全局安全上限 ({}s / {}分钟)。已强制终止以防死循环。\n请发送 /new 重置会话后重试。",
                        max_secs,
                        max_secs / 60
                    ),
                ),
            };

            let _ = stream_tx.send(StreamEvent::Error(error_event)).await;
            return Ok(ExecutionResult {
                output: summary_msg,
                conversation_id: None,
                status: "TIMEOUT".to_string(),
                usage: None,
                duration_seconds: None,
            });
        } else {
            exit_status.and_then(|r| r.ok())
        };

        let (accum_text, conv_id, final_status, final_usage, final_duration) = stdout_task.await.unwrap_or_default();
        let stderr_text = stderr_task.await.unwrap_or_default();

        let mut final_output = accum_text.trim().to_string();
        if final_output.is_empty() && !stderr_text.trim().is_empty() {
            final_output = format!("Error:\n{}", stderr_text.trim());
        } else if !stderr_text.trim().is_empty() && status.map(|s| !s.success()).unwrap_or(false) {
            warn!("agy stderr: {}", stderr_text.trim());
        }

        if final_output.is_empty() {
            final_output = "Task completed with no text output.".to_string();
        }

        let _ = stream_tx.send(StreamEvent::Result {
            conversation_id: conv_id.clone(),
            response: final_output.clone(),
            status: final_status.clone(),
            usage: final_usage.clone(),
            duration_seconds: final_duration,
        }).await;

        Ok(ExecutionResult {
            output: final_output,
            conversation_id: conv_id,
            status: final_status,
            usage: final_usage,
            duration_seconds: final_duration,
        })
    }

    /// Generic helper to execute an agy CLI subcommand and strip ANSI/spinners
    async fn run_cli_command(args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(get_agy_path());
        cmd.args(args);
        let output = cmd.output().await.with_context(|| format!("Failed to execute agy {:?}", args))?;
        let stdout_str = String::from_utf8_lossy(&output.stdout);
        let clean = crate::bot::formatter::TelegramFormatter::strip_ansi_codes(&stdout_str);
        Ok(clean.trim().to_string())
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
        let mut cmd = Command::new(get_agy_path());
        cmd.arg("--version");
        if let Ok(output) = cmd.output().await {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !stdout.is_empty() {
                return stdout;
            }
        }
        "1.1.13".to_string()
    }

    pub async fn show_changelog() -> Result<String> {
        let clean = Self::run_cli_command(&["changelog"]).await?;
        if clean.is_empty() {
            let ver = Self::get_version().await;
            Ok(format!("Antigravity CLI v{} - Features structured output (--output-format stream-json/json), JSON Schema enforcement, TUI settings, and performance enhancements.", ver))
        } else {
            Ok(clean)
        }
    }

    /// Returns all brain directories (modern CLI brain + legacy fallback)
    pub fn get_brain_dirs() -> Vec<PathBuf> {
        let mut dirs_list = Vec::new();
        if let Some(home) = dirs::home_dir() {
            let p1 = home.join(".gemini/antigravity-cli/brain");
            if p1.exists() {
                dirs_list.push(p1);
            }
            let p2 = home.join(".gemini/antigravity/brain");
            if p2.exists() && !dirs_list.contains(&p2) {
                dirs_list.push(p2);
            }
        }
        dirs_list
    }

    /// Lists recent conversations from ~/.gemini/antigravity-cli/brain/ and legacy ~/.gemini/antigravity/brain/
    pub async fn list_recent_conversations(limit: usize) -> Vec<RecentSessionInfo> {
        let brain_dirs = Self::get_brain_dirs();
        if brain_dirs.is_empty() {
            return Vec::new();
        }

        let mut sessions: Vec<(std::time::SystemTime, RecentSessionInfo)> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for brain_dir in brain_dirs {
            let mut entries = match tokio::fs::read_dir(&brain_dir).await {
                Ok(e) => e,
                Err(_) => continue,
            };

            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.is_dir() {
                    let id = entry.file_name().to_string_lossy().to_string();
                    // Filter out non-uuid or internal folders, and deduplicate
                    if id.starts_with('.') || id.len() < 8 || !seen_ids.insert(id.clone()) {
                        continue;
                    }

                    let mut title = "New Conversation".to_string();
                    let mut updated_time = String::new();
                    let mut modified_time = std::time::SystemTime::UNIX_EPOCH;
                    let has_plan = path.join("implementation_plan.md").exists();
                    let has_walkthrough = path.join("walkthrough.md").exists();

                    if let Ok(metadata) = entry.metadata().await {
                        if let Ok(modified) = metadata.modified() {
                            modified_time = modified;
                            let datetime: chrono::DateTime<chrono::Local> = modified.into();
                            updated_time = datetime.format("%m-%d %H:%M").to_string();
                        }
                    }

                    // Try reading first user prompt from transcript.jsonl
                    let transcript_path = path.join(".system_generated/logs/transcript.jsonl");
                    if transcript_path.exists() {
                        if let Ok(content) = tokio::fs::read_to_string(&transcript_path).await {
                            for line in content.lines() {
                                if line.contains("\"type\":\"USER_INPUT\"") {
                                    if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(line) {
                                        if let Some(c) = json_val.get("content").and_then(|v| v.as_str()) {
                                            let clean_prompt = clean_user_prompt_summary(c);
                                            if !clean_prompt.is_empty() {
                                                title = clean_prompt;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    sessions.push((
                        modified_time,
                        RecentSessionInfo {
                            id,
                            title,
                            updated_time,
                            has_plan,
                            has_walkthrough,
                        },
                    ));
                }
            }
        }

        // Sort by accurate file modification time (latest first)
        sessions.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        sessions.truncate(limit);
        sessions.into_iter().map(|(_, info)| info).collect()
    }

    /// Read implementation plan from brain for a given conversation ID
    pub async fn read_implementation_plan(conv_id: &str) -> Option<String> {
        for brain_dir in Self::get_brain_dirs() {
            let plan_path = brain_dir.join(conv_id).join("implementation_plan.md");
            if plan_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(plan_path).await {
                    return Some(content);
                }
            }
        }
        None
    }

    /// Read walkthrough from brain for a given conversation ID
    pub async fn read_walkthrough(conv_id: &str) -> Option<String> {
        for brain_dir in Self::get_brain_dirs() {
            let wt_path = brain_dir.join(conv_id).join("walkthrough.md");
            if wt_path.exists() {
                if let Ok(content) = tokio::fs::read_to_string(wt_path).await {
                    return Some(content);
                }
            }
        }
        None
    }

    /// Scan and list all active Antigravity skills
    pub async fn list_skills() -> String {
        let mut result = String::new();
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return "未找到家目录".to_string(),
        };

        let search_dirs = [
            ("User Skills", home.join(".gemini/config/skills")),
            ("Builtin Skills (CLI)", home.join(".gemini/antigravity-cli/builtin/skills")),
            ("Builtin Skills (Legacy)", home.join(".gemini/antigravity/builtin/skills")),
        ];

        for (section, dir) in search_dirs {
            if dir.exists() {
                let mut entries = match tokio::fs::read_dir(&dir).await {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let mut section_skills = Vec::new();
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = entry.file_name().to_string_lossy().to_string();
                        let skill_md = path.join("SKILL.md");
                        let mut desc = String::new();
                        if skill_md.exists() {
                            if let Ok(content) = tokio::fs::read_to_string(&skill_md).await {
                                for line in content.lines().take(15) {
                                    if let Some(d) = line.strip_prefix("description:") {
                                        desc = d.trim().trim_matches('"').trim_matches('\'').to_string();
                                        break;
                                    }
                                }
                            }
                        }
                        section_skills.push((name, desc));
                    }
                }

                if !section_skills.is_empty() {
                    result.push_str(&format!("📦 <b>{}</b>:\n", section));
                    for (name, desc) in section_skills {
                        if desc.is_empty() {
                            result.push_str(&format!("• <code>/{}</code>\n", name));
                        } else {
                            let short_desc = crate::bot::formatter::TelegramFormatter::truncate_chars(&desc, 50);
                            result.push_str(&format!("• <code>/{}</code> : <i>{}</i>\n", name, crate::bot::formatter::TelegramFormatter::escape_html(&short_desc)));
                        }
                    }
                    result.push('\n');
                }
            }
        }

        if result.is_empty() {
            "未发现已安装的 Antigravity 技能。".to_string()
        } else {
            result.trim_end().to_string()
        }
    }

    /// Read global and workspace rules
    pub async fn list_rules(workspace: &str) -> String {
        let mut output = String::new();
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return "未找到家目录".to_string(),
        };

        // Check global GEMINI.md
        let global_gemini = home.join(".gemini/GEMINI.md");
        if global_gemini.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&global_gemini).await {
                if !content.trim().is_empty() {
                    output.push_str("🌐 <b>Global Rules (~/.gemini/GEMINI.md)</b>:\n");
                    output.push_str(&format!("<pre><code>{}</code></pre>\n\n", crate::bot::formatter::TelegramFormatter::escape_html(&content)));
                }
            }
        }

        // Check global config rules
        let global_config = home.join(".gemini/config/config.json");
        if global_config.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&global_config).await {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(rules) = val.get("rules").or_else(|| val.get("custom_rules")) {
                        output.push_str("🌐 <b>Global Config Rules</b>:\n");
                        output.push_str(&format!("<pre><code>{}</code></pre>\n\n", crate::bot::formatter::TelegramFormatter::escape_html(&rules.to_string())));
                    }
                }
            }
        }

        // Check workspace rules (GEMINI.md or .gemini/RULE.md)
        let ws_gemini_file = std::path::PathBuf::from(workspace).join("GEMINI.md");
        if ws_gemini_file.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&ws_gemini_file).await {
                output.push_str("📁 <b>Workspace Rules (GEMINI.md)</b>:\n");
                output.push_str(&format!("<pre><code>{}</code></pre>\n\n", crate::bot::formatter::TelegramFormatter::escape_html(&content)));
            }
        }
        let ws_rule_file = std::path::PathBuf::from(workspace).join(".gemini/RULE.md");
        if ws_rule_file.exists() && ws_rule_file != ws_gemini_file {
            if let Ok(content) = tokio::fs::read_to_string(&ws_rule_file).await {
                output.push_str("📁 <b>Workspace Rules (.gemini/RULE.md)</b>:\n");
                output.push_str(&format!("<pre><code>{}</code></pre>\n", crate::bot::formatter::TelegramFormatter::escape_html(&content)));
            }
        }

        if output.is_empty() {
            "📋 <b>当前暂无活跃的自定义规则文件。</b>\n可在项目根目录下创建 <code>GEMINI.md</code> 添加项目规则。".to_string()
        } else {
            output.trim_end().to_string()
        }
    }

    /// Run agy update to upgrade CLI (with passwordless sudo fallback if permission denied)
    pub async fn run_update() -> Result<String> {
        let agy_bin = get_agy_path();
        let output = tokio::process::Command::new(agy_bin)
            .arg("update")
            .output()
            .await
            .context("Failed to run agy update")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = if stdout.trim().is_empty() { stderr } else { stdout };

        if combined.contains("Permission denied") || combined.contains("os error 13") {
            if let Ok(sudo_output) = tokio::process::Command::new("sudo")
                .arg("-n")
                .arg(agy_bin)
                .arg("update")
                .output()
                .await
            {
                let sudo_stdout = String::from_utf8_lossy(&sudo_output.stdout).to_string();
                let sudo_stderr = String::from_utf8_lossy(&sudo_output.stderr).to_string();
                let sudo_combined = if sudo_stdout.trim().is_empty() { sudo_stderr } else { sudo_stdout };
                if !sudo_combined.trim().is_empty() {
                    return Ok(sudo_combined.trim().to_string());
                }
            }
        }

        Ok(combined.trim().to_string())
    }

    /// Run agy mcp subcommands
    pub async fn run_mcp_cmd(args: &[&str]) -> Result<String> {
        let agy_bin = get_agy_path();
        let mut cmd = tokio::process::Command::new(agy_bin);
        cmd.arg("mcp");
        for arg in args {
            cmd.arg(arg);
        }
        let output = cmd.output().await.context("Failed to run agy mcp command")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{}\n{}", stdout, stderr);
        Ok(combined.trim().to_string())
    }

    /// Run agy plugin subcommands
    pub async fn run_plugin_cmd(args: &[&str]) -> Result<String> {
        let agy_bin = get_agy_path();
        let mut cmd = tokio::process::Command::new(agy_bin);
        cmd.arg("plugin");
        for arg in args {
            cmd.arg(arg);
        }
        let output = cmd.output().await.context("Failed to run agy plugin command")?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{}\n{}", stdout, stderr);
        Ok(combined.trim().to_string())
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
    let single_line = clean.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    crate::bot::formatter::TelegramFormatter::truncate_chars(single_line, 40).into_owned()
}

#[cfg(test)]
mod tests {
    use super::clean_user_prompt_summary;

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
        assert_eq!(clean_user_prompt_summary("<USER_REQUEST> no closer here"), "no closer here");
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
}
