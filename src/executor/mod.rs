pub mod protocol;
pub use protocol::*;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{info, warn};

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
}

impl AgyExecutor {
    /// Executes agy using stream-json format and streams structured events line-by-line with unbuffered pipes.
    pub async fn run_prompt_streaming(
        params: ExecutionParams<'_>,
        stream_tx: mpsc::Sender<StreamEvent>,
        pid_tx: Option<tokio::sync::oneshot::Sender<u32>>,
        timeout_secs: u64,
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

        if !params.model.is_empty() {
            cmd.arg("--model").arg(params.model);
        }

        if !params.effort.is_empty() {
            cmd.arg("--effort").arg(params.effort);
        }

        if !params.mode.is_empty() {
            cmd.arg("--mode").arg(params.mode);
        }

        if !params.agent.is_empty() {
            cmd.arg("--agent").arg(params.agent);
        }

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

        let stream_tx_clone = stream_tx.clone();
        let stdout_task = tokio::spawn(async move {
            let mut stdout_reader = BufReader::new(stdout).lines();
            let mut accumulated_text = String::new();
            let mut detected_conv_id: Option<String> = None;
            let mut final_status = "SUCCESS".to_string();
            let mut final_usage: Option<TokenUsage> = None;
            let mut final_duration: Option<f64> = None;

            while let Ok(Some(line)) = stdout_reader.next_line().await {
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
                accumulated_stderr.push_str(&line);
                accumulated_stderr.push('\n');
            }
            accumulated_stderr
        });

        let wait_result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            child.wait(),
        ).await;

        let status = match wait_result {
            Ok(s) => s,
            Err(_) => {
                warn!("agy process timed out after {}s, killing process group...", timeout_secs);
                if let Some(pid) = child_pid {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
                let _ = child.kill().await;
                stdout_task.abort();
                stderr_task.abort();
                let _ = stream_tx.send(StreamEvent::Error(format!(
                    "agy 执行超时 ({}s)，任务已强行终止。请尝试 /new 重置会话后重试。",
                    timeout_secs
                ))).await;
                return Ok(ExecutionResult {
                    output: format!("⏱ 执行超时 ({}s)，agy 进程已终止。\n请发送 /new 重置会话后重试。", timeout_secs),
                    conversation_id: None,
                    status: "TIMEOUT".to_string(),
                    usage: None,
                    duration_seconds: None,
                });
            }
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
            Ok("gemini-3.7-flash-high (Gemini 3.7 Flash High)".to_string())
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

    /// Lists recent conversations from ~/.gemini/antigravity/brain/
    pub async fn list_recent_conversations(limit: usize) -> Vec<RecentSessionInfo> {
        let mut sessions = Vec::new();
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return sessions,
        };
        let brain_dir = home.join(".gemini/antigravity/brain");
        if !brain_dir.exists() {
            return sessions;
        }

        let mut entries = match tokio::fs::read_dir(&brain_dir).await {
            Ok(e) => e,
            Err(_) => return sessions,
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                let id = entry.file_name().to_string_lossy().to_string();
                // Filter out non-uuid or internal folders
                if id.starts_with('.') || id.len() < 8 {
                    continue;
                }

                let mut title = "New Conversation".to_string();
                let mut updated_time = String::new();
                let has_plan = path.join("implementation_plan.md").exists();
                let has_walkthrough = path.join("walkthrough.md").exists();

                if let Ok(metadata) = entry.metadata().await {
                    if let Ok(modified) = metadata.modified() {
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

                sessions.push(RecentSessionInfo {
                    id,
                    title,
                    updated_time,
                    has_plan,
                    has_walkthrough,
                });
            }
        }

        // Sort by updated time (or keep latest first)
        sessions.sort_by(|a, b| b.updated_time.cmp(&a.updated_time));
        sessions.truncate(limit);
        sessions
    }

    /// Read implementation plan from brain for a given conversation ID
    pub async fn read_implementation_plan(conv_id: &str) -> Option<String> {
        let home = dirs::home_dir()?;
        let plan_path = home.join(".gemini/antigravity/brain").join(conv_id).join("implementation_plan.md");
        if plan_path.exists() {
            tokio::fs::read_to_string(plan_path).await.ok()
        } else {
            None
        }
    }

    /// Read walkthrough from brain for a given conversation ID
    pub async fn read_walkthrough(conv_id: &str) -> Option<String> {
        let home = dirs::home_dir()?;
        let wt_path = home.join(".gemini/antigravity/brain").join(conv_id).join("walkthrough.md");
        if wt_path.exists() {
            tokio::fs::read_to_string(wt_path).await.ok()
        } else {
            None
        }
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
            ("Builtin Skills", home.join(".gemini/antigravity/builtin/skills")),
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
                            let short_desc = if desc.chars().count() > 50 {
                                let cut = desc.char_indices().nth(50).map(|(i, _)| i).unwrap_or(desc.len());
                                format!("{}...", &desc[..cut])
                            } else {
                                desc
                            };
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

        // Check global config rules
        let global_config = home.join(".gemini/config/config.json");
        if global_config.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&global_config).await {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(rules) = val.get("rules").or_else(|| val.get("custom_rules")) {
                        output.push_str("🌐 <b>Global Rules (全局规则)</b>:\n");
                        output.push_str(&format!("<pre><code>{}</code></pre>\n\n", crate::bot::formatter::TelegramFormatter::escape_html(&rules.to_string())));
                    }
                }
            }
        }

        // Check workspace rules
        let ws_rule_file = std::path::PathBuf::from(workspace).join(".gemini/RULE.md");
        if ws_rule_file.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&ws_rule_file).await {
                output.push_str("📁 <b>Workspace Rules (项目规则)</b>:\n");
                output.push_str(&format!("<pre><code>{}</code></pre>\n", crate::bot::formatter::TelegramFormatter::escape_html(&content)));
            }
        }

        if output.is_empty() {
            "📋 <b>当前暂无活跃的自定义规则文件。</b>\n可在项目根目录下创建 <code>.gemini/RULE.md</code> 添加项目规则。".to_string()
        } else {
            output.trim_end().to_string()
        }
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
    let mut clean = raw;
    if let Some(start) = raw.find("<USER_REQUEST>") {
        if let Some(end) = raw.find("</USER_REQUEST>") {
            clean = &raw[start + 14..end];
        }
    }
    let single_line = clean.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if single_line.chars().count() > 40 {
        let cut = single_line.char_indices().nth(40).map(|(i, _)| i).unwrap_or(single_line.len());
        format!("{}...", &single_line[..cut])
    } else {
        single_line.to_string()
    }
}
