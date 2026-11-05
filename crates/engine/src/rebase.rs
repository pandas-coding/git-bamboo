//! Interactive rebase: the engine-side foundation.
//!
//! Three concerns live here:
//!
//! 1. **Todo serialization/parsing** — byte-exact writing of the todo
//!    sidecar git's `GIT_SEQUENCE_EDITOR` copies into
//!    `git-rebase-todo`, and tolerant parsing of todo/done files for
//!    `getRebaseStatus` (lines the parser does not understand — `label`,
//!    `exec`, ... — pass through verbatim instead of being dropped).
//! 2. **In-progress state detection** — reading `rebase-merge` /
//!    `rebase-apply` from the gitdir (head-name, onto, stopped-sha,
//!    msgnum/end, done/remaining), the same files GitLens-style clients
//!    inspect. The engine never trusts these for restore purposes; the
//!    authoritative pre-rebase tip lives in the undo journal.
//! 3. **The editor self-invocation modes** — `--internal-sequence-editor`
//!    and `--internal-message-editor` (handled by main.rs before the
//!    server starts). `git rebase -i` invokes them as
//!    GIT_SEQUENCE_EDITOR / GIT_EDITOR; they replay engine-side state
//!    (the validated todo and message sidecars) without a terminal.
//!
//! Execution strategy (`start_rebase_flow` in write_queue.rs): the engine
//! writes a fully validated todo sidecar, then runs
//! `git rebase -i --no-autosquash --onto <onto> <merge-base> <branch>`
//! with the editors pointed at that sidecar. git does the real replay
//! (same machinery as the CLI), the engine only decides the todo.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use bamboo_protocol::{
    BambooError, GetRebaseStatusResult, RebaseAction, RebaseBackend, RebaseRunState,
    RebaseStatusEntry, RebaseStopReason, RebaseTodoEntry,
};

/// Cap on the todo list. No real interactive rebase approaches this; the
/// bound exists so a malformed client can never push an unbounded file
/// through the queue and the journal.
pub(crate) const MAX_TODO_ENTRIES: usize = 500;

/// Custom reword/squash messages are bounded and control-character-free
/// (a single paragraph; the network-ops message validation, e.g. stash
/// messages, applies the same rules).
const MAX_REBASE_MESSAGE_LEN: usize = 8192;

/// Validate a custom reword/squash message.
pub(crate) fn validate_rebase_message(message: &str) -> anyhow::Result<()> {
    if message.is_empty() {
        return Err(BambooError::invalid_argument("rebase message must not be empty").into());
    }
    if message.len() > MAX_REBASE_MESSAGE_LEN {
        return Err(BambooError::invalid_argument(format!(
            "rebase message exceeds {MAX_REBASE_MESSAGE_LEN} bytes"
        ))
        .into());
    }
    if message.contains('\0') || message.chars().any(|c| c.is_ascii_control()) {
        return Err(
            BambooError::invalid_argument("rebase message contains control characters").into(),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Todo serialization
// ---------------------------------------------------------------------------

/// One serialized todo line: `<action-word> <sha> <subject>`.
pub(crate) struct TodoLine {
    pub action: RebaseAction,
    pub id: String,
    pub subject: String,
}

/// The canonical action word for a todo line (never an abbreviation: the
/// engine writes, it does not parse, its own todo).
fn action_word(action: RebaseAction) -> &'static str {
    match action {
        RebaseAction::Pick => "pick",
        RebaseAction::Reword => "reword",
        RebaseAction::Squash => "squash",
        RebaseAction::Fixup => "fixup",
        RebaseAction::Drop => "drop",
        RebaseAction::Edit => "edit",
    }
}

/// Serialize todo lines to the exact bytes written to the sidecar and then
/// to `git-rebase-todo`. Subjects come from engine-decoded commit data,
/// never from the client, and contain no newlines (git's todo format is
/// strictly one entry per line).
pub(crate) fn serialize_todo(lines: &[TodoLine]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 * lines.len());
    for line in lines {
        out.extend_from_slice(action_word(line.action).as_bytes());
        out.push(b' ');
        out.extend_from_slice(line.id.as_bytes());
        out.push(b' ');
        out.extend_from_slice(line.subject.as_bytes());
        out.push(b'\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Todo parsing (for status reads)
// ---------------------------------------------------------------------------

/// One parsed line of a `git-rebase-todo` / `done` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedTodoLine {
    /// A pick-like entry: `<action> <sha> <subject>`.
    Entry {
        action: RebaseAction,
        id: String,
        subject: String,
    },
    /// Anything the parser does not recognize (`label`, `exec`, `reset`,
    /// `break`, `noop`, `update-ref`, `merge`, malformed lines, ...):
    /// kept verbatim so status consumers see the real file content. Never
    /// interpreted, never dropped.
    Other(String),
    /// Blank or comment line.
    Empty,
}

/// Parse one todo line. Tolerant by design: the engine only ever parses
/// files git wrote (done/git-rebase-todo), including for externally
/// started rebases, so unknown syntax must pass through, not fail.
pub(crate) fn parse_todo_line(line: &str) -> ParsedTodoLine {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return ParsedTodoLine::Empty;
    }
    // `pick <sha> <subject...>` — split off the action word and the id,
    // the rest (possibly containing spaces) is the subject.
    let mut parts = trimmed.splitn(3, ' ');
    let (Some(word), Some(id)) = (parts.next(), parts.next()) else {
        return ParsedTodoLine::Other(line.to_string());
    };
    let subject = parts.next().unwrap_or("").to_string();
    match parse_action_word(word) {
        // A pick-like line must carry a hex id; `exec <cmd>` lines share
        // the word shape with nothing (no clash with the six action words),
        // but stay defensive: a non-hex second token means the line is
        // something else and passes through.
        Some(action) if is_hex_id(id) => ParsedTodoLine::Entry {
            action,
            id: id.to_string(),
            subject,
        },
        _ => ParsedTodoLine::Other(line.to_string()),
    }
}

/// Action-word lookup covering both full words and git's one-letter
/// abbreviations (tolerating files written by other tools).
fn parse_action_word(word: &str) -> Option<RebaseAction> {
    match word {
        "pick" | "p" => Some(RebaseAction::Pick),
        "reword" | "r" => Some(RebaseAction::Reword),
        "squash" | "s" => Some(RebaseAction::Squash),
        "fixup" | "f" => Some(RebaseAction::Fixup),
        "drop" | "d" => Some(RebaseAction::Drop),
        "edit" | "e" => Some(RebaseAction::Edit),
        _ => None,
    }
}

/// 4–64 lowercase hex chars (todo files record abbreviated ids; the engine
/// validates, never trusts, them).
fn is_hex_id(id: &str) -> bool {
    (4..=64).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Strict validation for todo ids from the CLIENT (`^[0-9a-f]{40,64}$`,
/// the same rule as the graph-side commit ids). The freshness check
/// compares these against full object ids computed engine-side.
pub(crate) fn is_valid_todo_id(id: &str) -> bool {
    (40..=64).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Filter raw todo lines into parsed pick-like status entries.
pub(crate) fn parse_status_entries(lines: &[String]) -> Vec<RebaseStatusEntry> {
    lines
        .iter()
        .filter_map(|line| match parse_todo_line(line) {
            ParsedTodoLine::Entry {
                action,
                id,
                subject,
            } => Some(RebaseStatusEntry {
                action,
                id,
                subject,
            }),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// In-progress state detection (getRebaseStatus)
// ---------------------------------------------------------------------------

/// Raw in-progress rebase state, read best-effort from the gitdir.
#[derive(Debug, Clone, Default)]
pub(crate) struct RebaseStateInternal {
    pub backend: Option<RebaseBackend>,
    pub branch: Option<String>,
    pub onto: Option<String>,
    pub orig_head: Option<String>,
    pub stopped_sha: Option<String>,
    pub msgnum: Option<u32>,
    pub end: Option<u32>,
    pub done: Vec<String>,
    pub remaining: Vec<String>,
    /// A stop marker was found (stopped-sha file / REBASE_HEAD): the
    /// rebase is waiting for the user rather than actively replaying.
    pub stopped: bool,
}

impl RebaseStateInternal {
    /// A rebase state dir exists on disk.
    pub fn in_progress(&self) -> bool {
        self.backend.is_some()
    }
}

/// Read the in-progress rebase state from the gitdir (rebase-merge first,
/// then rebase-apply). Best-effort: individual missing/unreadable fields
/// are `None`/empty, matching how GitLens-style clients treat the same
/// files. One git subprocess at most (REBASE_HEAD fallback for the
/// stopped sha when the stopped-sha file is absent).
pub(crate) fn read_rebase_state(
    repo_path: &Path,
    git_dir: &Path,
) -> RebaseStateInternal {
    let merge_dir = git_dir.join("rebase-merge");
    let apply_dir = git_dir.join("rebase-apply");
    if !merge_dir.is_dir() && !apply_dir.is_dir() {
        return RebaseStateInternal::default();
    }

    let mut state = RebaseStateInternal::default();
    if merge_dir.is_dir() {
        state.backend = Some(RebaseBackend::Merge);
        state.branch = short_branch(read_trimmed(&merge_dir.join("head-name")));
        state.onto = read_trimmed(&merge_dir.join("onto"));
        state.orig_head = read_trimmed(&merge_dir.join("orig-head"));
        state.msgnum = read_trimmed(&merge_dir.join("msgnum")).and_then(|s| s.parse().ok());
        state.end = read_trimmed(&merge_dir.join("end")).and_then(|s| s.parse().ok());
        state.stopped_sha = read_trimmed(&merge_dir.join("stopped-sha"));
        state.done = read_lines(&merge_dir.join("done"));
        state.remaining = read_lines(&merge_dir.join("git-rebase-todo"));
    } else {
        // Legacy am-based backend: `next`/`last` are the msgnum/end
        // equivalents; there is no todo, so done/remaining stay empty.
        state.backend = Some(RebaseBackend::Apply);
        state.branch = short_branch(read_trimmed(&apply_dir.join("head-name")));
        state.onto = read_trimmed(&apply_dir.join("onto"));
        state.orig_head = read_trimmed(&apply_dir.join("orig-head"));
        state.msgnum = read_trimmed(&apply_dir.join("next")).and_then(|s| s.parse().ok());
        state.end = read_trimmed(&apply_dir.join("last")).and_then(|s| s.parse().ok());
        state.stopped_sha = read_trimmed(&apply_dir.join("stopped-sha"));
    }

    if state.stopped_sha.is_none() {
        // git (>=2.26) writes rebase-merge/stopped-sha; for older git or
        // the apply backend, fall back to resolving REBASE_HEAD.
        state.stopped_sha = resolve_rebase_head(repo_path);
    }
    // The apply backend only exists while waiting for the user (git am /
    // rebase -m conflicts); the merge backend is "stopped" when a stop
    // marker exists, "running" when the dir is present without one.
    state.stopped = state.stopped_sha.is_some() || state.backend == Some(RebaseBackend::Apply);
    state
}

/// `git rev-parse --quiet --verify REBASE_HEAD` (empty when it doesn't
/// resolve). The state dirs are per-worktree, so the command must run in
/// the worktree, not the bare gitdir.
fn resolve_rebase_head(repo_path: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--quiet", "--verify", "REBASE_HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// The commit the rebase stopped on, derived from the last executed todo
/// entry (`rebase-merge/done`). git does NOT write `stopped-sha` (nor
/// always `REBASE_HEAD`) for the message-editing stops of reword/squash —
/// but the entry being replayed is always the last line of `done` at that
/// point (verified against git 2.55: "reword <sha> ..." / "squash <sha>
/// ..." sit at the tail while COMMIT_EDITMSG is open).
fn stopped_sha_from_done(git_dir: &Path) -> Option<String> {
    let done = read_lines(&git_dir.join("rebase-merge").join("done"));
    for line in done.iter().rev() {
        if let ParsedTodoLine::Entry { id, .. } = parse_todo_line(line) {
            return Some(id);
        }
    }
    None
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `head-name` files record either `refs/heads/<name>` or `detached HEAD`;
/// only the former maps to a short branch name.
fn short_branch(head_name: Option<String>) -> Option<String> {
    head_name.and_then(|name| {
        name.strip_prefix("refs/heads/").map(str::to_string)
    })
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Map the raw state to the protocol `getRebaseStatus` result.
pub(crate) fn rebase_status_result(state: &RebaseStateInternal) -> GetRebaseStatusResult {
    if !state.in_progress() {
        return GetRebaseStatusResult {
            state: RebaseRunState::Idle,
            ..Default::default()
        };
    }
    GetRebaseStatusResult {
        state: if state.stopped {
            RebaseRunState::Stopped
        } else {
            RebaseRunState::Running
        },
        backend: state.backend,
        branch: state.branch.clone(),
        onto: state.onto.clone(),
        orig_head: state.orig_head.clone(),
        stopped_sha: state.stopped_sha.clone(),
        msgnum: state.msgnum,
        end: state.end,
        done: state.done.clone(),
        remaining: state.remaining.clone(),
        done_entries: parse_status_entries(&state.done),
        remaining_entries: parse_status_entries(&state.remaining),
    }
}

/// Classify why a rebase stopped: unresolved index entries → conflict,
/// anything else (message editing etc.) → reword_stop.
pub(crate) fn classify_stop_reason(has_unmerged: bool) -> RebaseStopReason {
    if has_unmerged {
        RebaseStopReason::Conflict
    } else {
        RebaseStopReason::RewordStop
    }
}

// ---------------------------------------------------------------------------
// Commit chain computation (getRebasePlan + startRebase freshness)
// ---------------------------------------------------------------------------

/// One commit of the replay set, decoded engine-side.
#[derive(Debug, Clone)]
pub(crate) struct ChainCommit {
    pub id: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
    pub parent_ids: Vec<String>,
}

/// The merge-base of `onto` and `branch`, plus the commits in
/// `merge-base..branch` in REPLAY ORDER (oldest → newest, parents before
/// children — a deterministic topological sort, matching what
/// `git rebase` itself would list in the todo).
///
/// Returns `None` when the two commits share no common ancestor (git's
/// rebase would refuse the same way).
pub(crate) fn rebase_chain(
    repo: &gix::Repository,
    onto_id: gix::ObjectId,
    branch_id: gix::ObjectId,
) -> anyhow::Result<Option<(gix::ObjectId, Vec<ChainCommit>)>> {
    let base = match repo.merge_base(onto_id, branch_id) {
        Ok(base) => base.detach(),
        Err(gix::repository::merge_base::Error::NotFound { .. }) => return Ok(None),
        Err(e) => anyhow::bail!("merge-base computation failed: {e}"),
    };

    // Walk branch..base (hidden tips exclude the base itself, like
    // `rev-list base..branch`).
    let walk_ids: Vec<gix::ObjectId> = {
        let platform = repo
            .rev_walk([branch_id])
            .with_hidden([base])
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ));
        let walk = platform.all()?;
        let mut ids = Vec::new();
        for info in walk {
            ids.push(info?.id);
        }
        ids
    };

    // Decode every commit once (metadata + parents for the topo sort).
    let mut commits: Vec<ChainCommit> = Vec::with_capacity(walk_ids.len());
    let mut by_id: HashMap<String, usize> = HashMap::with_capacity(walk_ids.len());
    for id in walk_ids {
        let obj = repo
            .find_object(id)
            .map_err(|e| anyhow::anyhow!("failed to read commit {id}: {e}"))?;
        let commit = obj
            .try_into_commit()
            .map_err(|e| anyhow::anyhow!("{id} is not a commit: {e}"))?;
        let decoded = commit.decode()?;
        let author = decoded
            .author()
            .map(|a| String::from_utf8_lossy(a.name.as_ref()).into_owned())
            .unwrap_or_default();
        let time = decoded
            .author()
            .ok()
            .and_then(|a| a.time().ok())
            .map(|t| t.seconds)
            .unwrap_or(0);
        let subject = String::from_utf8_lossy(&decoded.message_summary()).into_owned();
        let parent_ids = decoded.parents().map(|p| p.to_string()).collect::<Vec<_>>();
        by_id.insert(id.to_string(), commits.len());
        commits.push(ChainCommit {
            id: id.to_string(),
            subject,
            author,
            time,
            parent_ids,
        });
    }

    // Kahn's algorithm over the subgraph: an edge parent → child. The
    // ready set is ordered by (time, id) so the result is deterministic
    // and, for the linear chains interactive rebase targets, identical to
    // the chronological order.
    let id_set: HashSet<&String> = commits.iter().map(|c| &c.id).collect();
    let mut in_degree = vec![0usize; commits.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); commits.len()];
    for (idx, c) in commits.iter().enumerate() {
        for p in &c.parent_ids {
            if let Some(&pidx) = by_id.get(p) {
                in_degree[idx] += 1;
                children[pidx].push(idx);
            } else if !id_set.contains(p) {
                // Parent below the merge base (the normal case): no edge.
                continue;
            }
        }
    }
    // (time, id) min-heap of ready commits.
    let mut ready: std::collections::BTreeSet<(i64, String)> = commits
        .iter()
        .enumerate()
        .filter(|(idx, _)| in_degree[*idx] == 0)
        .map(|(_, c)| (c.time, c.id.clone()))
        .collect();
    let mut ordered: Vec<ChainCommit> = Vec::with_capacity(commits.len());
    while let Some(first) = ready.iter().next().cloned() {
        ready.remove(&first);
        let (_, id) = first;
        let idx = by_id[&id];
        ordered.push(commits[idx].clone());
        for &child in &children[idx] {
            in_degree[child] -= 1;
            if in_degree[child] == 0 {
                let c = &commits[child];
                ready.insert((c.time, c.id.clone()));
            }
        }
    }
    if ordered.len() != commits.len() {
        // Cycle or missing parent data — impossible in a valid git DAG;
        // refuse rather than emitting a half-ordered todo.
        anyhow::bail!("internal error: topological sort of the replay set failed");
    }

    Ok(Some((base, ordered)))
}

// ---------------------------------------------------------------------------
// Editor sidecars (GIT_SEQUENCE_EDITOR / GIT_EDITOR)
// ---------------------------------------------------------------------------

/// Directory holding the rebase sidecars: `<gitdir>/bamboo/rebase/`. It is
/// engine-owned state (like `bamboo/undo`), lives with the gitdir so
/// linked worktrees work, and survives across the whole rebase (start →
/// stops → continue) — unlike a tempdir.
pub(crate) fn sidecar_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("bamboo").join("rebase")
}

pub(crate) fn todo_sidecar_path(git_dir: &Path) -> PathBuf {
    sidecar_dir(git_dir).join("todo.sidecar")
}

pub(crate) fn messages_sidecar_path(git_dir: &Path) -> PathBuf {
    sidecar_dir(git_dir).join("messages.sidecar")
}

/// Remove the sidecar dir (after the rebase finished/aborted). Best-effort.
pub(crate) fn cleanup_sidecars(git_dir: &Path) {
    let _ = fs::remove_dir_all(sidecar_dir(git_dir));
}

/// Write both sidecars. The todo sidecar holds the serialized todo; the
/// message sidecar maps `<full-sha> <message>` lines for the
/// reword/squash commits with custom messages.
pub(crate) fn write_sidecars(
    git_dir: &Path,
    todo: &[TodoLine],
    messages: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let dir = sidecar_dir(git_dir);
    fs::create_dir_all(&dir)?;
    fs::write(todo_sidecar_path(git_dir), serialize_todo(todo))?;
    let mut msg_data = String::new();
    for (sha, message) in messages {
        // Messages are pre-validated: single paragraph, no newlines, so
        // one line per entry round-trips exactly.
        msg_data.push_str(sha);
        msg_data.push(' ');
        msg_data.push_str(message);
        msg_data.push('\n');
    }
    fs::write(messages_sidecar_path(git_dir), msg_data)?;
    Ok(())
}

/// The executable to re-invoke for the editor modes. Normally the engine
/// itself (`current_exe`); overridable so integration tests can point the
/// editor at the real engine binary — the test binary's own main cannot
/// handle the editor flags.
pub(crate) fn internal_editor_exe() -> anyhow::Result<PathBuf> {
    if let Ok(exe) = std::env::var("BAMBOO_ENGINE_EDITOR_EXE") {
        if !exe.is_empty() {
            return Ok(PathBuf::from(exe));
        }
    }
    std::env::current_exe().map_err(|e| anyhow::anyhow!("failed to resolve engine exe: {e}"))
}

/// Double-quote a string for embedding in a GIT_*_EDITOR value. git runs
/// editor values containing metacharacters through the shell, so the
/// engine path and sidecar paths (which may contain spaces) must be
/// quoted.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// GIT_SEQUENCE_EDITOR value: `<engine> --internal-sequence-editor <sidecar>`.
/// git appends the todo file path when invoking it.
pub(crate) fn sequence_editor_value(git_dir: &Path) -> anyhow::Result<String> {
    let exe = internal_editor_exe()?;
    Ok(format!(
        "{} --internal-sequence-editor {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&todo_sidecar_path(git_dir).to_string_lossy())
    ))
}

/// GIT_EDITOR value: `<engine> --internal-message-editor <sidecar>`.
/// git appends the COMMIT_EDITMSG path when invoking it. Used by
/// startRebase AND rebaseContinue (a reword stop survived into a
/// continue still needs the custom message).
pub(crate) fn message_editor_value(git_dir: &Path) -> anyhow::Result<String> {
    let exe = internal_editor_exe()?;
    Ok(format!(
        "{} --internal-message-editor {}",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&messages_sidecar_path(git_dir).to_string_lossy())
    ))
}

/// Implementation of `--internal-sequence-editor`: copy the validated
/// sidecar over git's freshly generated `git-rebase-todo`, byte-exact.
/// Exit 0 = git proceeds with OUR todo; a non-zero exit makes git cancel
/// the rebase cleanly (the engine treats that as a hard error).
pub fn run_sequence_editor(sidecar: &str, todo_target: &str) -> i32 {
    let result: anyhow::Result<()> = (|| {
        let data = fs::read(sidecar)?;
        fs::write(todo_target, data)?;
        Ok(())
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("bamboo: sequence editor failed: {e:#}");
            1
        }
    }
}

/// Implementation of `--internal-message-editor`: git has stopped with
/// COMMIT_EDITMSG open (reword/squash message editing). If the stopped
/// commit has a custom message in the sidecar, replace COMMIT_EDITMSG's
/// content with it; otherwise exit 0 without touching the file (git's own
/// message — the original for reword, the folded combination for squash —
/// is kept, so the editor is a no-op pass-through).
///
/// git runs editors with cwd = worktree root, so the gitdir is resolved
/// from cwd to read `rebase-merge/stopped-sha`.
pub fn run_message_editor(sidecar: &str, commit_editmsg: &str) -> i32 {
    let lookup = read_message_sidecar(sidecar);
    let repo_path = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("bamboo: message editor failed to resolve cwd: {e}");
            return 1;
        }
    };
    let git_dir = crate::gitdir::resolve_git_dir(&repo_path);
    let stopped_sha = read_trimmed(&git_dir.join("rebase-merge").join("stopped-sha"))
        .or_else(|| stopped_sha_from_done(&git_dir))
        .or_else(|| resolve_rebase_head(&repo_path));
    let custom_message = match (&stopped_sha, &lookup) {
        (Some(sha), Some(messages)) => messages.get_line(sha).cloned(),
        _ => None,
    };
    if let Some(message) = custom_message {
        // Byte-exact: the validated single-paragraph message plus the
        // trailing newline git expects.
        if let Err(e) = fs::write(commit_editmsg, format!("{message}\n")) {
            eprintln!("bamboo: message editor failed to write {commit_editmsg}: {e}");
            return 1;
        }
    }
    // No custom message (or no stopped sha): pass git's message through.
    0
}

/// Parsed `messages.sidecar`: full sha → message.
struct MessageSidecar {
    entries: HashMap<String, String>,
}

impl MessageSidecar {
    fn get_line(&self, sha: &str) -> Option<&String> {
        self.entries.get(sha)
    }
}

fn read_message_sidecar(path: &str) -> Option<MessageSidecar> {
    let content = fs::read_to_string(path).ok()?;
    let mut entries = HashMap::new();
    for line in content.lines() {
        if let Some((sha, message)) = line.split_once(' ') {
            entries.insert(sha.to_string(), message.to_string());
        }
    }
    Some(MessageSidecar { entries })
}

// ---------------------------------------------------------------------------
// startRebase validation
// ---------------------------------------------------------------------------

/// Validate the client todo against the freshly computed replay set:
///
/// * the entry id set must EQUAL the engine-computed set (drop is an
///   explicit entry, so a stale plan can never silently drop or add
///   commits — any deviation is INVALID_ARGUMENT, the stale-snapshot
///   signal),
/// * no duplicate ids,
/// * ids are 40/64 lowercase hex,
/// * messages only on reword/squash (and valid),
/// * fixup/edit are Phase-2 actions and are rejected here.
///
/// On success returns the serialized todo lines in the CLIENT's order
/// (reordering/squashing is the whole point) with subjects filled from
/// engine-decoded commit data.
pub(crate) fn build_validated_todo(
    todo: &[RebaseTodoEntry],
    chain: &[ChainCommit],
) -> anyhow::Result<(Vec<TodoLine>, HashMap<String, String>)> {
    if todo.len() > MAX_TODO_ENTRIES {
        return Err(BambooError::invalid_argument(format!(
            "startRebase: todo exceeds {MAX_TODO_ENTRIES} entries"
        ))
        .into());
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(todo.len());
    let mut messages: HashMap<String, String> = HashMap::new();
    let mut lines: Vec<TodoLine> = Vec::with_capacity(todo.len());
    let chain_by_id: HashMap<&str, &ChainCommit> =
        chain.iter().map(|c| (c.id.as_str(), c)).collect();

    for entry in todo {
        if !is_valid_todo_id(&entry.id) {
            return Err(BambooError::invalid_argument(format!(
                "startRebase: invalid todo id {:?}",
                &entry.id[..entry.id.len().min(16)]
            ))
            .into());
        }
        match entry.action {
            RebaseAction::Fixup | RebaseAction::Edit => {
                return Err(BambooError::invalid_argument(format!(
                    "startRebase: action {:?} is not supported yet",
                    action_word(entry.action)
                ))
                .into());
            }
            _ => {}
        }
        if let Some(message) = &entry.message {
            if !matches!(entry.action, RebaseAction::Reword | RebaseAction::Squash) {
                return Err(BambooError::invalid_argument(
                    "startRebase: message is only valid on reword/squash entries",
                )
                .into());
            }
            validate_rebase_message(message)?;
            messages.insert(entry.id.clone(), message.clone());
        }
        if !seen.insert(&entry.id) {
            return Err(BambooError::invalid_argument(
                "startRebase: duplicate todo id",
            )
            .into());
        }
        let Some(commit) = chain_by_id.get(entry.id.as_str()) else {
            return Err(BambooError::invalid_argument(
                "startRebase: todo does not match the current commit chain (stale plan?)",
            )
            .into());
        };
        lines.push(TodoLine {
            action: entry.action,
            id: commit.id.clone(),
            subject: commit.subject.clone(),
        });
    }

    // Set equality, the freshness gate: every chain commit must appear in
    // the todo (and, via the duplicate check above, exactly once).
    let todo_set: HashSet<&str> = todo.iter().map(|e| e.id.as_str()).collect();
    let chain_set: HashSet<&str> = chain.iter().map(|c| c.id.as_str()).collect();
    if todo_set != chain_set {
        return Err(BambooError::invalid_argument(
            "startRebase: todo does not match the current commit chain (stale plan?)",
        )
        .into());
    }
    Ok((lines, messages))
}
