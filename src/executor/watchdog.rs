use rustc_hash::FxHashMap;
use std::borrow::Cow;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Tracks the last observed activity timestamp relative to a monotonic `Instant` origin.
/// Lock-free, zero allocation on hot paths (`AtomicU64`).
#[derive(Clone, Debug)]
pub struct ActivityTracker {
    origin: Instant,
    last_offset_ms: Arc<AtomicU64>,
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_offset_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    #[inline]
    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Record new activity (token delta, stdout line, tool call, etc.).
    /// Zero-allocation, lock-free monotonic update.
    #[inline]
    pub fn record_activity(&self) {
        self.last_offset_ms
            .fetch_max(self.elapsed_ms(), Ordering::Relaxed);
    }

    /// Returns how long the process has been silent / idle since the last recorded activity.
    #[inline]
    #[must_use]
    pub fn idle_duration(&self) -> Duration {
        let now_ms = self.elapsed_ms();
        let last_ms = self.last_offset_ms.load(Ordering::Relaxed);
        Duration::from_millis(now_ms.saturating_sub(last_ms))
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
    /// Task has produced zero output AND zero CPU/IO activity for `idle_timeout_secs`.
    FrozenDeadlock {
        idle_secs: u64,
        process_count: usize,
        active_processes: Vec<String>,
    },
    /// Hard execution ceiling exceeded (prevents infinite CPU-burning loops).
    MaxExecutionExceeded { max_secs: u64 },
}

/// Parses `/proc/<pid>/stat`.
/// Format: `pid (comm) state ppid pgrp ... utime(14) stime(15) ...`
/// Returns: `(state: char, ppid: u32, pgrp: u32, utime: u64, stime: u64)`.
#[must_use]
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

/// Parses `/proc/<pid>/io` to extract total syscall I/O bytes (`rchar + wchar`).
#[must_use]
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
#[must_use]
pub fn is_stdin_waiting(wchan: &str, state: char) -> bool {
    let trimmed = wchan.trim();
    matches!(state, 'S' | 'D') && (trimmed == "n_tty_read" || trimmed.contains("n_tty"))
}

/// Signals the process group led by `pid` (spawned with `process_group(0)`, so the child is
/// the group leader) and the leader itself as a fallback.
pub fn signal_process_tree(pid: u32, signal: i32) {
    // `pid == 0` would target the calling process's own group and `pid == 1` (`-1`) would
    // broadcast to every process the user can signal.
    if pid <= 1 {
        tracing::warn!(pid, signal, "refusing to signal unsafe PID <= 1");
        return;
    }
    let Ok(raw) = i32::try_from(pid) else {
        tracing::warn!(pid, signal, "PID exceeds i32::MAX; refusing to signal");
        return;
    };

    for (target, kind) in [(-raw, "process_group"), (raw, "pid")] {
        // SAFETY: `pid` was checked to satisfy `2 <= pid <= i32::MAX as u32`, so `raw >= 2`.
        // `-raw <= -2` targets only the spawned child's process group (never `0` or `-1`), and
        // `raw >= 2` targets only that child PID. `libc::kill` requires only valid PID/signal ints.
        let rc = unsafe { libc::kill(target, signal) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                tracing::warn!(
                    pid,
                    signal,
                    target_kind = kind,
                    error = %err,
                    "libc::kill failed"
                );
            }
        }
    }
}

#[inline]
fn read_proc_str(path: &str, buf: &mut String) -> bool {
    buf.clear();
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    file.read_to_string(buf).is_ok()
}

#[inline]
fn read_proc_bytes(path: &str, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    file.read_to_end(buf).is_ok()
}

fn cmdline_first_arg(bytes: &[u8]) -> Option<Cow<'_, str>> {
    let first = bytes.split(|&b| b == 0).find(|s| !s.is_empty())?;
    Some(String::from_utf8_lossy(first))
}

fn format_cmdline(bytes: &[u8]) -> String {
    let mut clean = String::with_capacity(bytes.len());
    for slice in bytes.split(|&b| b == 0).filter(|slice| !slice.is_empty()) {
        if !clean.is_empty() {
            clean.push(' ');
        }
        clean.push_str(&String::from_utf8_lossy(slice));
    }
    clean
}

/// Reads the first argument (executable name/path) from `/proc/<pid>/cmdline`.
#[must_use]
pub fn read_proc_first_arg(pid: u32) -> Option<String> {
    let mut path = String::with_capacity(32);
    let _ = write!(path, "/proc/{pid}/cmdline");
    let mut bytes = Vec::with_capacity(128);
    if !read_proc_bytes(&path, &mut bytes) {
        return None;
    }
    cmdline_first_arg(&bytes).map(Cow::into_owned)
}

/// Reads `/proc/<pid>/cmdline`, returning clean whitespace-separated arguments.
#[must_use]
pub fn read_proc_cmdline(pid: u32) -> String {
    let mut path = String::with_capacity(32);
    let _ = write!(path, "/proc/{pid}/cmdline");
    let mut bytes = Vec::with_capacity(128);
    if !read_proc_bytes(&path, &mut bytes) {
        return String::new();
    }
    format_cmdline(&bytes)
}

struct ProcEntry {
    pid: u32,
    pgrp: u32,
    state: char,
    utime: u64,
    stime: u64,
}

/// Performs a deep inspection of all descendant processes and process group members of `root_pid`.
/// Gathers aggregate CPU ticks, I/O bytes, active process names, and checks for stdin deadlocks.
#[must_use]
pub fn inspect_process_tree(root_pid: u32) -> ProcessInspectionResult {
    let mut metrics = ProcessTreeMetrics::default();
    let mut stdin_blocked_cmd = None;
    let mut active_processes: Vec<String> = Vec::new();

    let Ok(entries) = fs::read_dir("/proc") else {
        return ProcessInspectionResult {
            metrics,
            stdin_blocked_command: None,
            active_process_names: active_processes,
        };
    };

    let mut proc_table: Vec<ProcEntry> = Vec::with_capacity(256);
    let mut pid_to_idx: FxHashMap<u32, usize> =
        FxHashMap::with_capacity_and_hasher(256, Default::default());
    let mut head_by_ppid: FxHashMap<u32, usize> =
        FxHashMap::with_capacity_and_hasher(128, Default::default());
    let mut next_sibling: Vec<usize> = Vec::with_capacity(256);
    let mut same_pgrp: Vec<usize> = Vec::new();

    let mut path_buf = String::with_capacity(32);
    let mut text_buf = String::with_capacity(512);
    let mut bytes_buf: Vec<u8> = Vec::with_capacity(256);

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };

        path_buf.clear();
        let _ = write!(path_buf, "/proc/{pid}/stat");
        if !read_proc_str(&path_buf, &mut text_buf) {
            continue;
        }
        let Some((state, ppid, pgrp, utime, stime)) = parse_proc_stat(&text_buf) else {
            continue;
        };

        let idx = proc_table.len();
        proc_table.push(ProcEntry {
            pid,
            pgrp,
            state,
            utime,
            stime,
        });
        pid_to_idx.insert(pid, idx);
        let prev_head = head_by_ppid.insert(ppid, idx).unwrap_or(usize::MAX);
        next_sibling.push(prev_head);
        if pgrp == root_pid {
            same_pgrp.push(idx);
        }
    }

    // Traverse descendants of `root_pid` and process-group members in O(N) via indexed links.
    let mut visited = vec![false; proc_table.len()];
    let mut tree_indices: Vec<usize> = Vec::with_capacity(16);
    let mut queue: Vec<u32> = Vec::with_capacity(16);

    if let Some(&root_idx) = pid_to_idx.get(&root_pid) {
        visited[root_idx] = true;
        tree_indices.push(root_idx);
    }
    queue.push(root_pid);

    while let Some(current_ppid) = queue.pop() {
        let mut cur = head_by_ppid
            .get(&current_ppid)
            .copied()
            .unwrap_or(usize::MAX);
        while cur != usize::MAX {
            if !visited[cur] {
                visited[cur] = true;
                tree_indices.push(cur);
                queue.push(proc_table[cur].pid);
            }
            cur = next_sibling[cur];
        }
    }

    for idx in same_pgrp {
        if !visited[idx] && proc_table[idx].pgrp == root_pid {
            visited[idx] = true;
            tree_indices.push(idx);
        }
    }

    metrics.process_count = tree_indices.len();

    for idx in tree_indices {
        let entry = &proc_table[idx];
        let pid = entry.pid;
        metrics.total_cpu_ticks = metrics
            .total_cpu_ticks
            .saturating_add(entry.utime)
            .saturating_add(entry.stime);

        path_buf.clear();
        let _ = write!(path_buf, "/proc/{pid}/io");
        if read_proc_str(&path_buf, &mut text_buf) {
            let io_bytes = parse_proc_io(&text_buf);
            metrics.total_io_bytes = metrics.total_io_bytes.saturating_add(io_bytes);
        }

        if pid == root_pid {
            continue;
        }

        // Read `/proc/{pid}/cmdline` at most once per child, reusing it for both active process
        // display and stdin-blocked diagnostics.
        path_buf.clear();
        let _ = write!(path_buf, "/proc/{pid}/cmdline");
        let has_cmdline = read_proc_bytes(&path_buf, &mut bytes_buf);

        if has_cmdline {
            if let Some(first_arg) = cmdline_first_arg(&bytes_buf) {
                let short_name = Path::new(first_arg.as_ref())
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(first_arg.as_ref());
                if !short_name.is_empty() && !active_processes.iter().any(|s| s == short_name) {
                    active_processes.push(short_name.to_owned());
                }
            }
        }

        if stdin_blocked_cmd.is_none() && matches!(entry.state, 'S' | 'D') {
            path_buf.clear();
            let _ = write!(path_buf, "/proc/{pid}/wchan");
            if read_proc_str(&path_buf, &mut text_buf) && is_stdin_waiting(&text_buf, entry.state) {
                let cmd = if has_cmdline {
                    format_cmdline(&bytes_buf)
                } else {
                    String::new()
                };
                let desc = if cmd.is_empty() {
                    format!("PID {} (wchan: {})", pid, text_buf.trim())
                } else {
                    cmd
                };
                stdin_blocked_cmd = Some(desc);
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
    fn test_activity_tracker_saturates_when_offset_ahead() {
        let tracker = ActivityTracker::new();
        tracker.last_offset_ms.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(tracker.idle_duration(), Duration::ZERO);
    }

    #[test]
    fn test_signal_process_tree_guards_unsafe_pids() {
        // If pid <= 1 were not guarded, SIGKILL on 0 or 1 would kill this test process / host.
        signal_process_tree(0, libc::SIGKILL);
        signal_process_tree(1, libc::SIGKILL);
        signal_process_tree(u32::MAX, libc::SIGKILL);
        // Signal 0 checks PID validity without delivering a fatal signal.
        signal_process_tree(std::process::id(), 0);
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
    fn test_parse_proc_stat_rejects_malformed() {
        assert_eq!(parse_proc_stat(""), None);
        assert_eq!(parse_proc_stat("123 (incomplete"), None);
        assert_eq!(parse_proc_stat("123 (short) R 1 2 3"), None);
    }

    #[test]
    fn test_parse_proc_io() {
        let sample = "rchar: 12345\nwchar: 67890\nsyscr: 10\nsyscw: 20\n";
        let total = parse_proc_io(sample);
        assert_eq!(total, 12345 + 67890);
    }

    #[test]
    fn test_parse_proc_io_saturates_on_overflow() {
        let sample = format!("rchar: {}\nwchar: 100\n", u64::MAX);
        assert_eq!(parse_proc_io(&sample), u64::MAX);
    }

    #[test]
    fn test_cmdline_helpers() {
        let raw = b"/usr/bin/cargo\0test\0--all-targets\0";
        assert_eq!(cmdline_first_arg(raw).as_deref(), Some("/usr/bin/cargo"));
        assert_eq!(format_cmdline(raw), "/usr/bin/cargo test --all-targets");

        assert_eq!(cmdline_first_arg(b"\0\0"), None);
        assert_eq!(format_cmdline(b"\0\0"), "");
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
