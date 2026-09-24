//! Resolution of a repository's git directory.
//!
//! `.git` is a directory for normal repositories but a *file* (pointing at
//! the shared gitdir) for linked worktrees, so every consumer of `.git`
//! internals (fs watching, undo snapshots, the redb cache) must go through
//! [`resolve_git_dir`] instead of assuming `repo_path/.git` is a directory.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use tracing::{debug, warn};

/// Resolve the absolute git directory for the repository at `repo_path`.
///
/// Shells `git rev-parse --absolute-git-dir` (cached by the caller — compute
/// once at engine init). Falls back to `repo_path/.git` when the command
/// fails (e.g. git not on PATH) and that path exists as a directory.
pub fn resolve_git_dir(repo_path: &Path) -> PathBuf {
    let out = StdCommand::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(repo_path)
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let p = PathBuf::from(&s);
                if p.is_dir() {
                    return p;
                }
            }
        }
    }
    warn!(
        repo_path = %repo_path.display(),
        "git rev-parse --absolute-git-dir failed; falling back to .git directory"
    );
    repo_path.join(".git")
}

/// A fingerprint of the ref state used to detect repo movement across engine
/// restarts: HEAD's object id plus the sorted `name sha` list of refs/heads.
/// A change means the epoch must be bumped and the lane cache cleared.
pub fn repo_fingerprint(repo_path: &Path) -> String {
    let head = StdCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // for-each-ref sorts by refname by default.
    let heads = StdCommand::new("git")
        .args(["for-each-ref", "refs/heads", "--format=%(refname) %(objectname)"])
        .current_dir(repo_path)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    debug!(fingerprint_len = head.len() + heads.len(), "computed repo fingerprint");
    format!("{head}\n{heads}")
}

/// True when the git directory lives inside the worktree root (the normal
/// `.git` directory layout). Linked worktrees keep their gitdir elsewhere,
/// so they need the gitdir watched explicitly.
pub fn git_dir_inside_worktree(git_dir: &Path, repo_path: &Path) -> bool {
    git_dir.starts_with(repo_path)
}
