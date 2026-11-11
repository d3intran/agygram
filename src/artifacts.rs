//! Shared artifact discovery and path-confinement helpers used by Telegram and QQ media delivery.

use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};

/// Keep a candidate only if it resolves to a regular file inside one of `canonical_roots`.
/// Canonicalizing defeats `../` traversal and symlink escapes.
#[cfg(test)]
fn resolve_confined_artifact(candidate: &Path, canonical_roots: &[PathBuf]) -> Option<PathBuf> {
    let resolved = candidate.canonicalize().ok()?;
    if !resolved.is_file() {
        return None;
    }
    canonical_roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

/// Keeps a candidate only if it resolves via `tokio::fs` to a regular file inside one of
/// `canonical_roots`. Canonicalizing defeats `../` traversal and symlink escapes.
async fn resolve_confined_artifact_async(
    candidate: &Path,
    canonical_roots: &[PathBuf],
) -> Option<PathBuf> {
    let resolved = tokio::fs::canonicalize(candidate).await.ok()?;
    if !tokio::fs::metadata(&resolved)
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        return None;
    }
    canonical_roots
        .iter()
        .any(|root| resolved.starts_with(root))
        .then_some(resolved)
}

struct ScratchSnapshot {
    root: PathBuf,
    entries: FxHashSet<String>,
}

/// Strips a leading `file://` scheme (or label-glued `file://`), while rejecting non-file URLs
/// like `https://...` or `http://...`. Returns `None` when the token is a non-file URL.
fn strip_file_scheme_or_skip_url(token: &str) -> Option<&str> {
    let Some(scheme_pos) = token.find("://") else {
        return Some(token);
    };
    let prefix = &token[..scheme_pos];
    if !prefix.ends_with("file") {
        return None;
    }
    if prefix.len() > 4 {
        let prev_byte = prefix.as_bytes()[prefix.len() - 5];
        if prev_byte.is_ascii_alphanumeric() {
            return None;
        }
    }
    let after = &token[scheme_pos + 3..];
    Some(
        after
            .strip_prefix("localhost")
            .filter(|s| s.starts_with('/'))
            .unwrap_or(after),
    )
}

fn collect_root_strings(allowed_dirs: &[PathBuf], canonical_roots: &[PathBuf]) -> Vec<String> {
    let mut root_strs = Vec::with_capacity(canonical_roots.len() + allowed_dirs.len());
    for root in canonical_roots.iter().chain(allowed_dirs.iter()) {
        if let Some(s) = root.to_str()
            && !s.is_empty()
            && !root_strs.iter().any(|existing| existing == s)
        {
            root_strs.push(s.to_string());
        }
    }
    root_strs
}

/// Pure prose scanner that avoids per-word allocations and syscalls by checking bare tokens
/// against a pre-fetched snapshot of `scratch` directory entry names.
fn collect_candidate_paths(
    output_text: &str,
    root_strs: &[String],
    scratch_snapshots: &[ScratchSnapshot],
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen_candidates: FxHashSet<PathBuf> = FxHashSet::default();
    let mut path_buf = PathBuf::new();

    for raw in output_text.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '(' | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '"'
                    | '\''
                    | '<'
                    | '>'
                    | '`'
                    | '*'
                    | ','
                    | ';'
                    | '，'
                    | '。'
                    | '、'
                    | '；'
                    | '：'
                    | '！'
                    | '？'
                    | '（'
                    | '）'
                    | '【'
                    | '】'
                    | '「'
                    | '」'
            )
        });
        if token.is_empty() {
            continue;
        }
        let Some(token) = strip_file_scheme_or_skip_url(token) else {
            continue;
        };
        if token.is_empty() {
            continue;
        }

        if Path::new(token).is_absolute() {
            path_buf.clear();
            path_buf.push(token);
            if seen_candidates.insert(path_buf.clone()) {
                candidates.push(path_buf.clone());
            }
            continue;
        }

        if token.contains('/') || (cfg!(windows) && token.contains('\\')) {
            let mut matched_glued_root = false;
            for root_str in root_strs {
                if let Some(idx) = token.find(root_str.as_str()) {
                    path_buf.clear();
                    path_buf.push(&token[idx..]);
                    if seen_candidates.insert(path_buf.clone()) {
                        candidates.push(path_buf.clone());
                    }
                    matched_glued_root = true;
                }
            }
            if !matched_glued_root {
                let first_comp = token.split(['/', '\\']).next().unwrap_or("");
                if !first_comp.is_empty() {
                    for snap in scratch_snapshots {
                        if snap.entries.contains(first_comp) {
                            path_buf.clear();
                            path_buf.push(&snap.root);
                            path_buf.push(token);
                            if seen_candidates.insert(path_buf.clone()) {
                                candidates.push(path_buf.clone());
                            }
                        }
                    }
                }
            }
            continue;
        }

        // Bare filename: zero syscalls unless it exists in a snapshotted scratch directory.
        for snap in scratch_snapshots {
            if snap.entries.contains(token) {
                path_buf.clear();
                path_buf.push(&snap.root);
                path_buf.push(token);
                if seen_candidates.insert(path_buf.clone()) {
                    candidates.push(path_buf.clone());
                }
            }
        }
    }

    candidates
}

/// Synchronous reference implementation retained under `#[cfg(test)]` to cross-check
/// [`collect_artifact_paths_async`].
#[cfg(test)]
#[must_use]
pub(crate) fn collect_artifact_paths(output_text: &str, allowed_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = dir.canonicalize()
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    if canonical_roots.is_empty() {
        return Vec::new();
    }

    let mut scratch_snapshots = Vec::new();
    for root in &canonical_roots {
        if root.file_name().and_then(|n| n.to_str()) == Some("scratch") {
            let mut entries = FxHashSet::default();
            if let Ok(rd) = std::fs::read_dir(root) {
                for entry in rd.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        entries.insert(name.to_owned());
                    }
                }
            }
            if !entries.is_empty() {
                scratch_snapshots.push(ScratchSnapshot {
                    root: root.clone(),
                    entries,
                });
            }
        }
    }

    let root_strs = collect_root_strings(allowed_dirs, &canonical_roots);
    let candidates = collect_candidate_paths(output_text, &root_strs, &scratch_snapshots);

    let mut found = Vec::new();
    let mut seen = FxHashSet::default();
    for candidate in candidates {
        if let Some(resolved) = resolve_confined_artifact(&candidate, &canonical_roots)
            && seen.insert(resolved.clone())
        {
            found.push(resolved);
        }
    }
    found
}

/// Collects artifact paths mentioned in model output, confined to `allowed_dirs`, using
/// `tokio::fs` directory snapshotting so Tokio worker threads never block on per-word syscalls.
pub(crate) async fn collect_artifact_paths_async(
    output_text: &str,
    allowed_dirs: &[PathBuf],
) -> Vec<PathBuf> {
    let mut canonical_roots: Vec<PathBuf> = Vec::with_capacity(allowed_dirs.len());
    for dir in allowed_dirs {
        if let Ok(canon) = tokio::fs::canonicalize(dir).await
            && !canonical_roots.contains(&canon)
        {
            canonical_roots.push(canon);
        }
    }
    if canonical_roots.is_empty() {
        return Vec::new();
    }

    let mut scratch_snapshots = Vec::new();
    for root in &canonical_roots {
        if root.file_name().and_then(|n| n.to_str()) == Some("scratch") {
            let mut entries = FxHashSet::default();
            if let Ok(mut rd) = tokio::fs::read_dir(root).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    if let Some(name) = entry.file_name().to_str() {
                        entries.insert(name.to_owned());
                    }
                }
            }
            if !entries.is_empty() {
                scratch_snapshots.push(ScratchSnapshot {
                    root: root.clone(),
                    entries,
                });
            }
        }
    }

    let root_strs = collect_root_strings(allowed_dirs, &canonical_roots);
    let candidates = collect_candidate_paths(output_text, &root_strs, &scratch_snapshots);

    let mut found = Vec::new();
    let mut seen = FxHashSet::default();
    for candidate in candidates {
        if let Some(resolved) = resolve_confined_artifact_async(&candidate, &canonical_roots).await
            && seen.insert(resolved.clone())
        {
            found.push(resolved);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("temp fixture should be creatable");
        base
    }

    #[test]
    fn files_outside_the_allowed_roots_are_refused() {
        let base = fixture("agygram_confine_plain");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let inside = workspace.join("chart.png");
        std::fs::write(&inside, b"png-bytes").unwrap();
        let secret = outside.join("credentials.json");
        std::fs::write(&secret, b"secret").unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&inside, &roots),
            Some(inside.canonicalize().unwrap())
        );
        assert_eq!(resolve_confined_artifact(&secret, &roots), None);
        assert_eq!(
            resolve_confined_artifact(Path::new("/etc/hostname"), &roots),
            None
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn directories_are_never_delivered() {
        let base = fixture("agygram_confine_dir");
        let workspace = base.join("ws");
        std::fs::create_dir_all(workspace.join("nested.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("nested.json"), &roots),
            None,
            "a directory must not be treated as an artifact"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifact_paths_are_found_in_prose_and_file_urls() {
        let base = fixture("agygram_collect_paths");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("karina_event.png"), b"png").unwrap();
        std::fs::write(scratch.join("chart.jpg"), b"jpg").unwrap();
        std::fs::write(scratch.join("diagram.svg"), b"svg").unwrap();
        std::fs::write(workspace.join("report.pdf"), b"pdf").unwrap();
        std::fs::write(workspace.join("README.md"), b"readme").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "1. 活动照\n   文件路径：{}/karina_event.png）\n2. `karina_event.png`\n3. chart.jpg,\n4. https://example.com/a.png\n5. README.md\n6. file://{}/diagram.svg\n7. [报告](file://{}/report.pdf)",
            scratch.display(),
            scratch.display(),
            workspace.display()
        );

        let found = collect_artifact_paths(&text, &roots);
        let mut names: Vec<String> = found
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["chart.jpg", "diagram.svg", "karina_event.png", "report.pdf"]
        );
        assert!(
            !names.contains(&"README.md".to_string()),
            "a bare project file in the workspace root must not be auto-sent"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn artifact_paths_async_matches_sync_collection() {
        let base = fixture("agygram_collect_paths_async");
        let workspace = base.join("ws");
        let scratch = workspace.join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("plot.png"), b"png").unwrap();

        let roots = vec![workspace.clone(), scratch.clone()];
        let text = format!(
            "Generated plot.png and file://{}/plot.png plus https://example.com/plot.png",
            scratch.display()
        );

        let sync_paths = collect_artifact_paths(&text, &roots);
        let async_paths = collect_artifact_paths_async(&text, &roots).await;
        assert_eq!(sync_paths, async_paths);
        assert_eq!(async_paths.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn artifact_paths_cannot_escape_roots() {
        let base = fixture("agygram_collect_escape");
        let workspace = base.join("ws");
        let outside = base.join("out");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.png"), b"x").unwrap();

        let roots = vec![workspace];
        let text = format!("见 {}", outside.join("secret.png").display());
        assert!(
            collect_artifact_paths(&text, &roots).is_empty(),
            "a path outside the allowed roots must not be collected"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn traversal_and_symlink_escapes_are_refused() {
        let base = fixture("agygram_confine_escape");
        let workspace = base.join("ws");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let secret = outside.join("wallet.json");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, workspace.join("art.json")).unwrap();

        let roots = vec![workspace.canonicalize().unwrap()];
        assert_eq!(
            resolve_confined_artifact(&workspace.join("art.json"), &roots),
            None,
            "symlink escape must not deliver the target"
        );
        assert_eq!(
            resolve_confined_artifact(&workspace.join("../../outside/wallet.json"), &roots),
            None,
            "dot-dot traversal must not leave the workspace"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
