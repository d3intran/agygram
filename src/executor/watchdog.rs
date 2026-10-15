use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tracks the activity timestamp in milliseconds since UNIX epoch.
/// Lock-free, zero allocation on hot paths (AtomicU64).
#[derive(Clone, Debug)]
pub struct ActivityTracker {
    last_activity_ms: Arc<AtomicU64>,
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityTracker {
    pub fn new() -> Self {
        Self {
            last_activity_ms: Arc::new(AtomicU64::new(Self::now_ms())),
        }
    }

    #[inline]
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64
    }

    /// Record new activity (token delta, stdout line, tool call, etc.)
    /// Zero-allocation, nanosecond cost.
    #[inline]
    pub fn record_activity(&self) {
        self.last_activity_ms.store(Self::now_ms(), Ordering::Relaxed);
    }

    /// Returns how long the process has been silent / idle since the last recorded activity.
    #[inline]
    pub fn idle_duration(&self) -> Duration {
        let now = Self::now_ms();
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        if now > last {
            Duration::from_millis(now - last)
        } else {
            Duration::ZERO
        }
    }
}

/// Cumulative performance and resource metrics across a process tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessTreeMetrics {
    pub total_cpu_ticks: u64, // utime + stime across all processes in tree
    pub total_io_bytes: u64,  // rchar + wchar across all processes in tree
    pub process_count: usize, // number of active processes in tree
}

/// Outcome of a deep process tree inspection.
#[derive(Debug, Clone)]
pub struct ProcessInspectionResult {
    pub metrics: ProcessTreeMetrics,
    /// Set if any child process is blocked waiting for terminal/stdin input in non-interactive mode.
    pub stdin_blocked_command: Option<String>,
    /// Summary of commands currently running in the process tree (e.g. "cargo", "rustc", "python3").
    pub active_process_names: Vec<String>,
}

/// Reasons why the watchdog terminated a task.
#[derive(Debug, Clone)]
pub enum TimeoutReason {
    /// Child process is blocked waiting for interactive terminal input (e.g., sudo, git credentials, read).
    StdinDeadlock { command: String },
    /// Task has produced zero output AND zero CPU/IO activity for idle_timeout_secs.
    FrozenDeadlock {
        idle_secs: u64,
        process_count: usize,
        active_processes: Vec<String>,
    },
    /// Hard execution ceiling exceeded (prevents infinite CPU-burning loops).
    MaxExecutionExceeded { max_secs: u64 },
}

/// Parses /proc/<pid>/stat.
/// Format: pid (comm) state ppid pgrp ... utime(14) stime(15) ...
/// Returns: (state: char, ppid: u32, pgrp: u32, utime: u64, stime: u64)
pub fn parse_proc_stat(stat_content: &str) -> Option<(char, u32, u32, u64, u64)> {
    let rparen_idx = stat_content.rfind(')')?;
    let rest = stat_content.get(rparen_idx + 1..)?.trim_start();
    let mut parts = rest.split_ascii_whitespace();

    let state = parts.next()?.chars().next()?;
    let ppid: u32 = parts.next()?.parse().ok()?;
    let pgrp: u32 = parts.next()?.parse().ok()?;

    // Skipping indices 3..10 (session, tty_nr, tpgid, flags, minflt, cminflt, majflt, cmajflt)
    // parts.nth(8) consumes 8 elements and yields the 9th from current position, which is index 11 (utime, 1-based field 14)
    let utime: u64 = parts.nth(8)?.parse().ok()?;
    let stime: u64 = parts.next()?.parse().ok()?;

    Some((state, ppid, pgrp, utime, stime))
}

/// Parses /proc/<pid>/io to extract total syscall I/O bytes (rchar + wchar).
pub fn parse_proc_io(io_content: &str) -> u64 {
    let mut total = 0u64;
    for line in io_content.lines() {
        if let Some(val_str) = line.strip_prefix("rchar:") {
            if let Ok(v) = val_str.trim().parse::<u64>() {
                total = total.saturating_add(v);
            }
        } else if let Some(val_str) = line.strip_prefix("wchar:") {
            if let Ok(v) = val_str.trim().parse::<u64>() {
                total = total.saturating_add(v);
            }
        }
    }
    total
}

/// Checks whether a process is blocked on terminal or stdin input.
/// Only a tty read wait proves that. A stopped process (state `T`) normally means job control or
/// a debugger, and `read_events` is a generic kernel wait-queue symbol, so accepting either used
/// to kill perfectly healthy tasks.
pub fn is_stdin_waiting(wchan: &str, state: char) -> bool {
    let trimmed = wchan.trim();
    matches!(state, 'S' | 'D') && (trimmed == "n_tty_read" || trimmed.contains("n_tty"))
}

/// Reads /proc/<pid>/cmdline, returning clean whitespace-separated arguments.
pub fn read_proc_cmdline(pid: u32) -> String {
    let path = format!("/proc/{}/cmdline", pid);
    if let Ok(bytes) = fs::read(path) {
        let clean: String = bytes
            .split(|&b| b == 0)
            .filter(|slice| !slice.is_empty())
            .map(|slice| String::from_utf8_lossy(slice).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        if !clean.is_empty() {
            return clean;
        }
    }
    String::new()
}

/// Performs a deep inspection of all descendant processes and process group members of `root_pid`.
/// Gathers aggregate CPU ticks, I/O bytes, active process names, and checks for stdin deadlocks.
pub fn inspect_process_tree(root_pid: u32) -> ProcessInspectionResult {
    let mut metrics = ProcessTreeMetrics::default();
    let mut stdin_blocked_cmd = None;
    let mut active_processes = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else {
        return ProcessInspectionResult {
            metrics,
            stdin_blocked_command: None,
            active_process_names: active_processes,
        };
    };

    // Store (pid, ppid, pgrp, state, utime, stime)
    let mut proc_table: Vec<(u32, u32, u32, char, u64, u64)> = Vec::with_capacity(64);

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if let Ok(pid) = name_str.parse::<u32>() {
            let stat_path = format!("/proc/{}/stat", pid);
            if let Ok(stat_content) = fs::read_to_string(&stat_path) {
                if let Some((state, ppid, pgrp, utime, stime)) = parse_proc_stat(&stat_content) {
                    proc_table.push((pid, ppid, pgrp, state, utime, stime));
                }
            }
        }
    }

    // Collect all PIDs belonging to root_pid's process tree or process group
    let mut tree_pids = Vec::with_capacity(16);
    tree_pids.push(root_pid);

    let mut queue = vec![root_pid];
    while let Some(current_ppid) = queue.pop() {
        for &(pid, ppid, _, _, _, _) in &proc_table {
            if ppid == current_ppid && !tree_pids.contains(&pid) {
                tree_pids.push(pid);
                queue.push(pid);
            }
        }
    }

    // Also include any processes that share the same process group (pgrp == root_pid)
    for &(pid, _, pgrp, _, _, _) in &proc_table {
        if pgrp == root_pid && !tree_pids.contains(&pid) {
            tree_pids.push(pid);
        }
    }

    metrics.process_count = tree_pids.len();

    // Inspect each process in the tree
    for &pid in &tree_pids {
        if let Some(&(_, _, _, state, utime, stime)) = proc_table.iter().find(|p| p.0 == pid) {
            metrics.total_cpu_ticks = metrics.total_cpu_ticks.saturating_add(utime + stime);

            // Read I/O statistics
            let io_path = format!("/proc/{}/io", pid);
            if let Ok(io_content) = fs::read_to_string(&io_path) {
                let io_bytes = parse_proc_io(&io_content);
                metrics.total_io_bytes = metrics.total_io_bytes.saturating_add(io_bytes);
            }

            // Check wait channel (wchan) for terminal stdin lock
            let wchan_path = format!("/proc/{}/wchan", pid);
            let wchan = fs::read_to_string(&wchan_path).unwrap_or_default();
            if is_stdin_waiting(&wchan, state) && pid != root_pid {
                let cmd = read_proc_cmdline(pid);
                let desc = if cmd.is_empty() {
                    format!("PID {} (wchan: {})", pid, wchan.trim())
                } else {
                    cmd
                };
                stdin_blocked_cmd = Some(desc);
            }

            // Capture active child process name
            if pid != root_pid {
                let cmd = read_proc_cmdline(pid);
                if !cmd.is_empty() {
                    let first_arg = cmd.split_whitespace().next().unwrap_or(&cmd);
                    let short_name = Path::new(first_arg)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(first_arg)
                        .to_string();
                    if !active_processes.contains(&short_name) {
                        active_processes.push(short_name);
                    }
                }
            }
        }
    }

    ProcessInspectionResult {
        metrics,
        stdin_blocked_command: stdin_blocked_cmd,
        active_process_names: active_processes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_activity_tracker_basic() {
        let tracker = ActivityTracker::new();
        std::thread::sleep(Duration::from_millis(15));
        let idle = tracker.idle_duration();
        assert!(idle >= Duration::from_millis(10));

        tracker.record_activity();
        let idle_after = tracker.idle_duration();
        assert!(idle_after < Duration::from_millis(10));
    }

    #[test]
    fn test_parse_proc_stat_standard() {
        let sample = "1342 (cat) R 1148 1342 1342 34818 1342 4194304 630 0 12 0 15 25 0 0 20 0 1 0";
        let parsed = parse_proc_stat(sample);
        assert!(parsed.is_some());
        let (state, ppid, pgrp, utime, stime) = parsed.unwrap();
        assert_eq!(state, 'R');
        assert_eq!(ppid, 1148);
        assert_eq!(pgrp, 1342);
        assert_eq!(utime, 15);
        assert_eq!(stime, 25);
    }

    #[test]
    fn test_parse_proc_stat_with_spaces_and_parens() {
        let sample = "999 (Web (Sub) Process) S 100 200 200 0 0 0 0 0 0 0 42 58 0 0";
        let parsed = parse_proc_stat(sample);
        assert!(parsed.is_some());
        let (state, ppid, pgrp, utime, stime) = parsed.unwrap();
        assert_eq!(state, 'S');
        assert_eq!(ppid, 100);
        assert_eq!(pgrp, 200);
        assert_eq!(utime, 42);
        assert_eq!(stime, 58);
    }

    #[test]
    fn test_parse_proc_io() {
        let sample = "rchar: 12345\nwchar: 67890\nsyscr: 10\nsyscw: 20\n";
        let total = parse_proc_io(sample);
        assert_eq!(total, 12345 + 67890);
    }

    #[test]
    fn test_is_stdin_waiting() {
        assert!(is_stdin_waiting("n_tty_read", 'S'));
        assert!(is_stdin_waiting("n_tty_read\n", 'S'));
        assert!(is_stdin_waiting("n_tty_write", 'D'));
        // A stopped process is job control or a debugger, not a stdin deadlock.
        assert!(!is_stdin_waiting("0", 'T'));
        // A generic wait-queue symbol must not be read as a terminal block.
        assert!(!is_stdin_waiting("read_events", 'S'));
        assert!(!is_stdin_waiting("epoll_wait", 'S'));
        assert!(!is_stdin_waiting("futex_wait", 'S'));
        assert!(!is_stdin_waiting("n_tty_read", 'R'));
    }

    #[test]
    fn test_inspect_current_process_tree() {
        let my_pid = std::process::id();
        let res = inspect_process_tree(my_pid);
        assert!(res.metrics.process_count >= 1);
        // Process CPU ticks or I/O should be accessible without errors
        assert!(res.stdin_blocked_command.is_none());
    }
}
