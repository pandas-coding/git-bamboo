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

/// Result of `getHead`: current HEAD and branch (worktree-safe; replaces
/// clients parsing `.git/HEAD` directly).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GetHeadResult {
    pub head: String,
    /// Short branch name (e.g. "main"); `None` when HEAD is detached.
    pub branch: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Push {
    pub remote: String,
    pub branch: String,
    #[serde(default)]
    pub force: bool,
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
#[error("WorkbenchError {code}: {message}")]
pub struct WorkbenchError {
    pub code: i32,
    pub message: String,
}

impl WorkbenchError {
    pub const REPO_NOT_FOUND: i32 = -32001;
    pub const GIT_ERROR: i32 = -32002;
    pub const EPOCH_MISMATCH: i32 = -32004;
    pub const INVALID_ARGUMENT: i32 = -32005;
    pub const NOT_INITIALIZED: i32 = -32006;
    pub const UNDO_BLOCKED: i32 = -32007;

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
}
