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
        let clean = strip_ansi_spinners(&stdout_str);
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
}

/// Single-pass ANSI & CLI spinner stripper without intermediate allocations
fn strip_ansi_spinners(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_escape = false;

    for line in input.lines() {
        let trimmed_line = line.trim_start_matches(|ch: char| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(ch) || ch.is_whitespace());
        for c in trimmed_line.chars() {
            if c == '\x1B' {
                in_escape = true;
            } else if in_escape {
                if c.is_ascii_alphabetic() {
                    in_escape = false;
                }
            } else {
                result.push(c);
            }
        }
        result.push('\n');
    }

    result.trim_end().to_string()
}
