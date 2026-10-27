use serde::{Deserialize, Serialize};

/// Unique session identifier for a repository connection.
pub type SessionId = String;

// ---------------------------------------------------------------------------
// Core data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Commit {
    pub id: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub parent_ids: Vec<String>,
    pub lane: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ref {
    pub name: String,
    pub kind: RefKind,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Branch,
    Tag,
    RemoteBranch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusItem {
    pub path: String,
    pub status: StatusCode,
    pub old_path: Option<String>,
    /// Whether the change is staged (index vs HEAD) as opposed to a
    /// worktree change (index vs worktree). `Modified`/`Deleted` can be
    /// either; this flag disambiguates them for the SCM view.
    pub staged: bool,
    /// Classification of the conflict for `StatusCode::Conflict` entries:
    /// which side added / modified / deleted. Absent (`None`, omitted on
    /// the wire) for non-conflict entries, so older clients that ignore
    /// unknown fields keep working (backwards compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_kind: Option<ConflictKind>,
}

/// Kind of an index conflict, mirroring gix's classification of the
/// unmerged index stages (stage 1 = base, 2 = ours, 3 = theirs). The
/// frontend uses it to decide which resolve actions are legal for a
/// file: content conflicts (both-modified / both-added / added-by-*)
/// accept `ours`/`theirs` (`git checkout --ours/--theirs`), while
/// delete/modify kinds only accept `keepOurs`/`keepTheirs`.
///
/// Rename/rename and rename/delete conflicts surface as one of these
/// kinds as far as the unmerged index entries are concerned (the index
/// only records stage states, not rename detection).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// Both sides deleted the file (porcelain `DD`).
    BothDeleted,
    /// We added the file, their side diverged (porcelain `AU`).
    AddedByUs,
    /// They deleted the file, we modified it (porcelain `UD`).
    DeletedByThem,
    /// They added the file, our side diverged (porcelain `UA`).
    AddedByThem,
    /// We deleted the file, they modified it (porcelain `DU`).
    DeletedByUs,
    /// Both sides added the file with different content (porcelain `AA`).
    BothAdded,
    /// Both sides modified the file (porcelain `UU`).
    BothModified,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Ignored,
    Conflict,
}

// ---------------------------------------------------------------------------
// Graph pagination
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphViewport {
    pub offset: u64,
    pub limit: u32,
    pub anchor_commit: Option<String>,
    /// The epoch the client believes it is viewing; used for cache validation.
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphPage {
    pub commits: Vec<Commit>,
    pub total_approx: u64,
    pub has_more: bool,
    pub epoch: u64,
    pub anchor_commit: Option<String>,
}

// ---------------------------------------------------------------------------
// JSON-RPC request methods
// ---------------------------------------------------------------------------

/// Initialize a session for a given repository path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initialize {
    pub repo_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InitializeResult {
    pub session_id: SessionId,
    pub protocol_version: u32,
    pub engine_version: String,
    /// The session's epoch at response time (may already have been bumped
    /// by the fs watcher between session open and this response).
    pub epoch: u64,
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    pub graph: bool,
    pub status: bool,
    #[serde(rename = "writeOps")]
    pub write_ops: bool,
    pub undo: bool,
    pub notifications: bool,
}

/// Result of git commands whose stdout is surfaced to the user
/// (`switchBranch`, `fetch`, `pull`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputResult {
    pub output: String,
}

/// Result of the `undo` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoResponse {
    pub restored_refs: usize,
    /// Informational only; not serialized to keep the wire format
    /// identical to the original hand-rolled response.
    #[serde(skip)]
    pub noop: bool,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetGraph {
    pub viewport: GraphViewport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetStatus {
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBlob {
    pub revision: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetCommitDetail {
    pub id: String,
}

/// Full commit metadata for the commit detail view (graph rows only carry
/// the summary line). `message` is the complete raw message (multi-line,
/// trailing newline included).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitDetail {
    pub id: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub committer_name: String,
    pub committer_email: String,
    pub committer_time: i64,
    /// Full parent ids in order; the first parent is the diff baseline
    /// (empty for root commits).
    pub parent_ids: Vec<String>,
    /// Files changed vs. the first parent (or the empty tree for root
    /// commits). Merge commits are NOT combined-diff: they diff against
    /// the first parent only.
    pub files: Vec<CommitFile>,
}

/// One changed file in a `CommitDetail`, parsed from
/// `git diff-tree --numstat --name-status -r -M -z` (run twice, one output
/// format per invocation, paired by path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitFile {
    pub path: String,
    /// Pre-rename path when `status` is R/C, else `None`.
    pub old_path: Option<String>,
    /// Single letter: A, D, M, R, C (rename score stripped) or T
    /// (type-change, passed through verbatim).
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

/// Result of `getHead`: current HEAD and branch (worktree-safe; replaces
/// clients parsing `.git/HEAD` directly).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GetHeadResult {
    pub head: String,
    /// Short branch name (e.g. "main"); `None` when HEAD is detached.
    pub branch: Option<String>,
}

/// How a conflict is resolved, per the `resolveConflict` write RPC.
/// `ours`/`theirs` take that side's blob (`git checkout --ours/--theirs`)
/// and are only legal for content conflicts; `keep_ours`/`keep_theirs`
/// keep what our/their side *did* — for delete/modify conflicts that
/// means keeping the file or keeping the deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    Ours,
    Theirs,
    KeepOurs,
    KeepTheirs,
}

/// `resolveConflict` request: resolve one conflicted path. The engine
/// re-classifies the path's conflict kind at execution time — a stale
/// client snapshot must never drive the resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveConflict {
    pub path: String,
    pub resolution: ConflictResolution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unstage {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discard {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitMsg {
    pub message: String,
    #[serde(default)]
    pub amend: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    pub commit_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateBranch {
    pub name: String,
    pub base: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteBranch {
    pub name: String,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchBranch {
    pub name: String,
    /// Defaults to `true` when absent, matching the original hand-rolled
    /// parser's behavior.
    #[serde(default = "default_true")]
    pub auto_stash: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fetch {
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pull {
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// How the merge is performed; `Default` appends no flag so the
    /// user's `pull.rebase` / `pull.ff` git config is respected.
    #[serde(default)]
    pub strategy: PullStrategy,
    #[serde(default)]
    pub auto_stash: bool,
}

/// Merge strategy for `pull`. `Default` adds no flag (git config decides);
/// the others map to `--rebase` / `--no-rebase` / `--ff-only`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullStrategy {
    #[default]
    Default,
    Merge,
    Rebase,
    FfOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Push {
    pub remote: String,
    pub branch: String,
    #[serde(default)]
    pub force: ForceMode,
    #[serde(default)]
    pub set_upstream: bool,
}

/// Push force semantics (three tiers).
///
/// Backwards wire compatibility: older clients sent `force` as a bool,
/// so deserialization also accepts `true` (mapped to `Lease`) and `false`
/// (mapped to `None`) — no protocol version jump.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ForceMode {
    /// No force at all.
    #[default]
    None,
    /// `--force-with-lease`: rejected when the remote moved since our
    /// remote-tracking ref was last updated.
    Lease,
    /// `--force`: unconditional overwrite (only reachable after an
    /// explicit client-side confirmation modal).
    Force,
}

impl<'de> Deserialize<'de> for ForceMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Untagged proxy: accepts the new string form and the legacy bool
        // form in one pass.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Mode(String),
            Legacy(bool),
        }
        match Raw::deserialize(deserializer)? {
            // Legacy bool: `true` meant "force with safety", i.e. lease.
            Raw::Legacy(true) => Ok(ForceMode::Lease),
            Raw::Legacy(false) => Ok(ForceMode::None),
            Raw::Mode(s) => match s.as_str() {
                "none" => Ok(ForceMode::None),
                "lease" => Ok(ForceMode::Lease),
                "force" => Ok(ForceMode::Force),
                _ => Err(serde::de::Error::custom(format!(
                    "unknown force mode {s:?}; expected \"none\" | \"lease\" | \"force\""
                ))),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Remotes / branch status / stash
// ---------------------------------------------------------------------------

/// `getRemotes` has no parameters (an empty object).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRemotes {}

/// One remote as reported by `getRemotes`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// Fetch URL (first configured, `insteadOf` rewrites applied);
    /// `None` if no URL resolves. Credentials embedded in the URL
    /// (`https://user:token@host/...`) are redacted before the value
    /// leaves the engine.
    pub url: Option<String>,
    /// Whether this is the remote bamboo treats as the default push
    /// target: the remote named by the `remote.default` git config when
    /// set, else the first remote in list order. This is a bamboo-side
    /// convention — git itself has no default-push-remote concept
    /// (`checkout.defaultRemote` only affects `git pull`'s resolution
    /// of an ambiguous branch).
    pub default: bool,
}

/// `getBranchStatus`: branch defaults to the current HEAD branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBranchStatus {
    #[serde(default)]
    pub branch: Option<String>,
}

/// Ahead/behind of a branch vs. its upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BranchStatus {
    pub branch: String,
    /// Short upstream name (e.g. "origin/main"); `None` when the branch
    /// has no configured upstream.
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// `stashPush` request. `stash_ref` is `stash@{N}` or `None` for the latest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashPush {
    pub message: Option<String>,
    #[serde(default)]
    pub include_untracked: bool,
    /// `--staged` (staged changes only).
    #[serde(default)]
    pub staged_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashPop {
    /// `stash@{N}`, or `None` for the latest entry.
    pub stash_ref: Option<String>,
    /// `true` = `git stash apply` (entry kept), `false` = `pop`.
    #[serde(default)]
    pub apply: bool,
    /// `--index`: restore the staged/unstaged split as it was.
    #[serde(default)]
    pub reinstate_index: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashDrop {
    /// `stash@{N}`, or `None` for the latest entry.
    pub stash_ref: Option<String>,
}

/// One entry of `stashList` (`git stash list`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StashEntry {
    /// Reflog selector, e.g. `stash@{0}`.
    #[serde(rename = "ref")]
    pub stash_ref: String,
    /// Subject line of the stash commit (message given at push time or
    /// the generated "WIP on ..." line).
    pub message: String,
    /// Strict ISO-8601 commit date of the stash entry, e.g.
    /// `2026-10-26T12:00:00+08:00` (`git log %cI`).
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Undo {
    pub transaction_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoResult {
    pub restored: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Raw (unclamped) limit as sent by the client; the server clamps it
/// to [1, 500] before use.
pub struct ListUndoStack {
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_limit() -> u64 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoEntry {
    pub id: u64,
    pub timestamp: i64,
    pub description: String,
    pub epoch_at_creation: u64,
}

// ---------------------------------------------------------------------------
// JSON-RPC notification types (server → client)
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefsChanged {
    pub changed_refs: Vec<String>,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeChanged {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexChanged {
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeadChanged {
    pub new_head: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphInvalidated {
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("BambooError {code}: {message}")]
pub struct BambooError {
    pub code: i32,
    pub message: String,
}

impl BambooError {
    pub const REPO_NOT_FOUND: i32 = -32001;
    pub const GIT_ERROR: i32 = -32002;
    pub const EPOCH_MISMATCH: i32 = -32004;
    pub const INVALID_ARGUMENT: i32 = -32005;
    pub const NOT_INITIALIZED: i32 = -32006;
    pub const UNDO_BLOCKED: i32 = -32007;
    /// Stash pop/apply hit a merge conflict or would overwrite local
    /// changes. Git has NOT dropped the stash — the entry is retained
    /// (the error message says so). For a merge conflict the conflicted
    /// files are already in the worktree for the conflict flow to pick
    /// up; for a "would be overwritten" failure git applied nothing.
    pub const STASH_CONFLICT: i32 = -32008;
    /// Push was rejected by the remote (non-fast-forward / stale
    /// force-with-lease / fetch-first). Recovery (pull→push) is a
    /// client-side interaction, not engine orchestration.
    pub const PUSH_REJECTED: i32 = -32009;
    /// Remote authentication failed / credentials unavailable. The client
    /// should guide the user through terminal-based authentication.
    pub const AUTH_REQUIRED: i32 = -32010;

    pub fn repo_not_found(msg: impl Into<String>) -> Self {
        Self {
            code: Self::REPO_NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn git_error(msg: impl Into<String>) -> Self {
        Self {
            code: Self::GIT_ERROR,
            message: msg.into(),
        }
    }

    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self {
            code: Self::INVALID_ARGUMENT,
            message: msg.into(),
        }
    }

    pub fn not_initialized(msg: impl Into<String>) -> Self {
        Self {
            code: Self::NOT_INITIALIZED,
            message: msg.into(),
        }
    }

    pub fn epoch_mismatch(expected: u64, actual: u64) -> Self {
        Self {
            code: Self::EPOCH_MISMATCH,
            message: format!("epoch mismatch: expected {expected}, got {actual}"),
        }
    }

    pub fn undo_blocked(reason: impl Into<String>) -> Self {
        Self {
            code: Self::UNDO_BLOCKED,
            message: reason.into(),
        }
    }

    pub fn stash_conflict(msg: impl Into<String>) -> Self {
        Self {
            code: Self::STASH_CONFLICT,
            message: msg.into(),
        }
    }

    pub fn push_rejected(msg: impl Into<String>) -> Self {
        Self {
            code: Self::PUSH_REJECTED,
            message: msg.into(),
        }
    }

    pub fn auth_required(msg: impl Into<String>) -> Self {
        Self {
            code: Self::AUTH_REQUIRED,
            message: msg.into(),
        }
    }
}
