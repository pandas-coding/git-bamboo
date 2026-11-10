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

/// Graph filter conditions ("filter as a query condition"): the engine
/// applies these while walking the commit graph, so filtering is a
/// first-class query parameter instead of a client-side post-filter of
/// materialized pages — virtual scrolling and lane rendering keep working
/// unchanged at 100k+ commits.
///
/// Semantics (an intentional deviation from git-log's regex flags, which
/// are hostile in a GUI input box and a regex-injection surface):
/// - `author`: case-insensitive substring of the author name OR email;
/// - `message`: case-insensitive substring of the commit summary line
///   (the message the graph rows display);
/// - `path`: case-SENSITIVE substring of any path changed vs. the first
///   parent (full tree diff for root commits).
///
/// Multiple set conditions AND together; each field is validated at the
/// server parse layer: non-empty, ≤ 256 chars, no control characters.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct GraphFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl GraphFilter {
    /// True when no condition is set — a filter with no fields is a no-op
    /// and callers treat it as `None` (the unfiltered fast path).
    pub fn is_empty(&self) -> bool {
        self.author.is_none() && self.message.is_none() && self.path.is_none()
    }

    /// Normalize after deserialization: trim whitespace, drop empty strings.
    /// The server parse layer calls this before validation so an `""` field
    /// behaves exactly like an absent field on the wire.
    pub fn normalized(self) -> Self {
        let trim = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            author: trim(self.author),
            message: trim(self.message),
            path: trim(self.path),
        }
    }
}

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
    /// Unfiltered: a lower bound ("walk so far" count). Filtered: the
    /// EXACT hit count — the engine materializes the full filtered list
    /// once, so the scrollbar always matches the content.
    pub total_approx: u64,
    pub has_more: bool,
    pub epoch: u64,
    pub anchor_commit: Option<String>,
    /// True when a filtered walk hit its safety cap (commit-count or
    /// tree-diff time budget) and was cut short: `total_approx` is then a
    /// lower bound and the UI shows "≥ N commits". Omitted (false) on
    /// the wire for unfiltered pages, so old payloads stay compatible.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
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
    /// Filter conditions; absent = unfiltered (backwards compatible: old
    /// clients that never send the field get the exact prior behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<GraphFilter>,
}

/// Validate a client-supplied graph filter: each set field must be
/// non-empty, at most 256 chars, and free of control characters. Runs in
/// the server parse layer so no oversized or control-character payload
/// ever reaches the walk (the webview is a trust boundary).
pub fn validate_graph_filter(filter: &GraphFilter) -> Result<(), BambooError> {
    for (field, value) in [
        ("author", &filter.author),
        ("message", &filter.message),
        ("path", &filter.path),
    ] {
        if let Some(text) = value {
            if text.is_empty() {
                return Err(BambooError::invalid_argument(format!(
                    "getGraph: filter field {field} must not be empty"
                )));
            }
            if text.chars().count() > 256 {
                return Err(BambooError::invalid_argument(format!(
                    "getGraph: filter field {field} exceeds 256 characters"
                )));
            }
            if text.chars().any(|c| c.is_ascii_control()) {
                return Err(BambooError::invalid_argument(format!(
                    "getGraph: filter field {field} contains control characters"
                )));
            }
        }
    }
    Ok(())
}

/// Resolve a request's filter option in one step — the getGraph parse
/// layer's ONLY filter handling (the single enforcement point; host-side
/// checks are UX sugar at best). Normalize (trim, drop empty fields — an
/// all-empty filter is an unfiltered request), then validate. `Ok(None)`
/// means unfiltered.
pub fn resolve_filter(
    filter: Option<GraphFilter>,
) -> Result<Option<GraphFilter>, BambooError> {
    match filter.map(|f| f.normalized()) {
        None => Ok(None),
        Some(f) if f.is_empty() => Ok(None),
        Some(f) => {
            validate_graph_filter(&f)?;
            Ok(Some(f))
        }
    }
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

/// `getDiff` request: the hunk list for a single path.
/// `staged=false` diffs the worktree against the index (the unstaged
/// changes — identical to worktree-vs-HEAD when nothing is staged);
/// `staged=true` diffs the index against HEAD (the staged changes). The
/// worktree-vs-index base (not HEAD) is what makes incremental hunk
/// staging correct: staging hunk B after hunk A must not clobber A's
/// already-staged content, so `stageRanges` always applies hunks on top
/// of the CURRENT index blob, not HEAD (see write_queue::stage_ranges).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetDiff {
    pub path: String,
    #[serde(default)]
    pub staged: bool,
}

/// One hunk of a unified diff (`git diff -U3`), as served by `getDiff`.
/// The engine is the single source of hunk data — clients never compute
/// hunks themselves, so `stageRanges`/`unstageRanges` ranges can always
/// be cross-checked against a freshly computed diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffHunk {
    /// First line (1-based) of the hunk on the old side; 0 when the old
    /// side is empty (new file) or when the file is fully deleted.
    pub old_start: u32,
    /// Line count on the old side.
    pub old_lines: u32,
    /// First line (1-based) on the new side; 0 when the new side is
    /// empty (deleted file).
    pub new_start: u32,
    /// Line count on the new side.
    pub new_lines: u32,
    /// Hunk body: context, removed (old) and added (new) lines, in diff
    /// order, WITHOUT the leading origin marker.
    pub lines: Vec<DiffLine>,
}

/// One line of a diff hunk body. `kind` mirrors the unified-diff origin
/// marker; `text` is the line content without the marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

/// `getDiff` result. `binary` files carry no hunks — hunk-level staging
/// is impossible for them and clients fall back to whole-file staging.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct GetDiffResult {
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
}

/// Inclusive line range on the NEW side of a diff (the side the client
/// sees on the right of the diff editor). Used by `stageRanges` (new
/// side = worktree lines) and `unstageRanges` (new side = index lines):
/// the engine intersects the ranges with a freshly computed diff's hunks
/// and applies the intersecting hunks — hunk granularity, arbitrary line
/// granularity is a later phase (the blob-construction approach below
/// already supports it).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// `stageRanges`: stage selected hunks of one path by reconstructing the
/// index blob ("index + selected hunks"), never `git apply --cached`
/// (a client-supplied patch can hit "patch does not apply" when its
/// context has drifted; engine-side blob construction on a fresh diff
/// cannot).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageRanges {
    pub path: String,
    pub ranges: Vec<LineRange>,
}

/// `unstageRanges`: symmetric — revert selected staged hunks by
/// reconstructing the index blob as "HEAD + remaining staged hunks".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstageRanges {
    pub path: String,
    pub ranges: Vec<LineRange>,
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
    /// Strict ISO-8601 commit date of the stash entry (`git log %cI`), e.g.
    /// `2026-10-26T12:00:00+08:00`. A zero offset is printed as `Z`
    /// (`2026-10-26T04:00:00Z`), not `+00:00`, so clients must accept both
    /// zone forms — which one arrives depends on the timezone of the machine
    /// the engine runs on.
    pub date: String,
}

// ---------------------------------------------------------------------------
// Interactive rebase (phase 1: engine foundation)
// ---------------------------------------------------------------------------

/// Action of one interactive-rebase todo entry. The Phase 1 write set is
/// `pick`/`reword`/`squash`/`drop` — `startRebase` rejects `fixup` and
/// `edit` (Phase 2). They exist in the enum so `getRebaseStatus` can parse
/// externally-started rebases whose todo files use them instead of
/// dropping those lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RebaseAction {
    Pick,
    Reword,
    Squash,
    Fixup,
    Drop,
    Edit,
}

/// One entry of a `startRebase` todo: the commit to replay (`id`, full
/// object id as served by `getRebasePlan`), the action, and an optional
/// custom message for reword/squash. Drops are explicit entries — the
/// engine requires the todo's id set to equal the current
/// `upstream..branch` commit set exactly, so a stale client snapshot can
/// never silently drop or add commits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebaseTodoEntry {
    pub id: String,
    pub action: RebaseAction,
    /// Custom message for `reword`/`squash` (invalid on other actions).
    /// Single-paragraph only: the engine rejects control characters.
    #[serde(default)]
    pub message: Option<String>,
}

/// `getRebasePlan` request: the commit chain that a "rebase onto X" would
/// replay. `branch: null` (default) = the current HEAD branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetRebasePlan {
    pub onto: String,
    #[serde(default)]
    pub branch: Option<String>,
}

/// One commit of a rebase plan (the replay candidates for the plan UI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebasePlanCommit {
    pub id: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
}

/// `getRebasePlan` result. `upstream` is `merge-base(onto, branch)` — the
/// rebase replays exactly `upstream..branch`; `commits` lists them in
/// replay order (oldest → newest).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetRebasePlanResult {
    pub onto: String,
    pub upstream: String,
    pub branch: String,
    pub commits: Vec<RebasePlanCommit>,
}

/// `getRebaseStatus` request (no parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRebaseStatus {}

/// High-level rebase state. `idle` = no rebase in progress. `stopped` =
/// a rebase is on disk waiting for the user (conflict or message stop;
/// includes externally-started rebases). `running` = the rebase-merge/
/// rebase-apply state dir exists but no stop is recorded (an external
/// process is actively replaying — best-effort classification).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RebaseRunState {
    #[default]
    Idle,
    Stopped,
    Running,
}

/// Which on-disk restate backend git is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RebaseBackend {
    /// `rebase-merge` (the interactive/merge backend).
    Merge,
    /// `rebase-apply` (am/legacy backend).
    Apply,
}

/// One line of a `getRebaseStatus` todo list (`done` / remaining
/// `git-rebase-todo`): either a parsed pick-like entry, or a line the
/// parser does not recognize (`label`, `exec`, `reset`, `merge`,
/// `break`, `noop`, `update-ref`, malformed lines, ...) kept verbatim —
/// never interpreted, never dropped.
///
/// UNTRUSTED DATA: subjects and raw lines originate from the repository's
/// commit messages (todo files are generated from them), so a malicious
/// clone can put arbitrary text here. UI consumers must treat every
/// string as plain text (escape on render, no HTML/markdown interpolation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RebaseStatusEntry {
    /// A parsed `<action> <sha> <subject>` line.
    Entry {
        action: RebaseAction,
        id: String,
        subject: String,
    },
    /// A passthrough line, verbatim.
    Raw { line: String },
}

/// `getRebaseStatus` result: the direct mapping of the on-disk rebase
/// state (all fields `None`/empty in the idle case). `done` / `remaining`
/// carry the todo lists once: each line is either a parsed entry or a
/// verbatim passthrough line (see [`RebaseStatusEntry`]).
///
/// UNTRUSTED DATA: these strings mirror repo content (commit subjects);
/// escape before rendering anywhere.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetRebaseStatusResult {
    pub state: RebaseRunState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<RebaseBackend>,
    /// Short name of the branch being rebased; `None` when detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The `--onto` commit (sha or ref, as git recorded it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
    /// The pre-rebase branch tip as git recorded it (informational; the
    /// engine keeps its own authoritative copy in the undo journal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orig_head: Option<String>,
    /// The commit the rebase stopped on (REBASE_HEAD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_sha: Option<String>,
    /// 1-based index of the commit being replayed (`msgnum`), and the
    /// total number of commits to replay (`end`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msgnum: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u32>,
    /// The already-replayed todo lines (`done` file).
    #[serde(default)]
    pub done: Vec<RebaseStatusEntry>,
    /// The not-yet-replayed todo lines (`git-rebase-todo`).
    #[serde(default)]
    pub remaining: Vec<RebaseStatusEntry>,
}

/// `startRebase` request: execute an interactive rebase with an
/// engine-revalidated todo list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRebase {
    pub onto: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub todo: Vec<RebaseTodoEntry>,
}

/// `rebaseContinue` / `rebaseSkip` / `rebaseAbort` request (they operate
/// on the current in-progress rebase; no parameters).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseContinue {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseSkip {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebaseAbort {}

/// Why a rebase stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebaseStopReason {
    /// A merge conflict — resolve the files, then `rebaseContinue`.
    Conflict,
    /// Any other stop (message editing etc.).
    RewordStop,
}

/// Structured outcome of `startRebase` / `rebaseContinue` /
/// `rebaseSkip`. A stop is NORMAL semantics, not an error: `stopped: true`
/// means the rebase is on disk waiting for the user (with `reason`,
/// `stopped_sha`, `msgnum` and `end` describing the stop); `stopped: false`
/// means the rebase is no longer in progress (completed, aborted, or
/// nothing left to continue — the idempotent double-click case). Real
/// failures (invalid refs, git crashing) surface as GIT_ERROR.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RebaseResult {
    pub stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RebaseStopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msgnum: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<u32>,
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
// Blame (line-level attribution)
// ---------------------------------------------------------------------------

/// `getBlame` request: attribute every line of one file to the commit that
/// introduced it (JetBrains "Annotate" / `git blame` semantics).
///
/// The engine joins commit metadata server-side so a client can render
/// gutter annotations and hover cards from ONE round trip — it must never
/// issue one RPC per line or per entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetBlame {
    /// Repository-relative path (validated engine-side like every other
    /// path argument: non-empty, no leading `-`, no control characters).
    pub path: String,
    /// Revision to walk back from; `None` means HEAD. Anything that
    /// resolves to a commit is accepted (branch, tag, sha, `HEAD~2`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// Unsaved editor buffer to blame instead of the file on disk
    /// (dirty-buffer blame). Lines that exist only in `contents` are
    /// attributed to the null commit id, i.e. "not committed yet".
    /// Size-capped engine-side; an oversized buffer must be saved first
    /// (INVALID_ARGUMENT).
    ///
    /// With `rev`, this selects WHICH BYTES get annotated. The returned
    /// line numbers refer to that content, so it has to be the content the
    /// caller renders — the same rule `git blame` follows:
    ///   contents given          → the buffer, blamed against `rev`/HEAD
    ///   no contents, no rev     → the worktree file, blamed against HEAD
    ///   no contents, rev given  → the file as of that revision
    /// A worktree file that has been deleted annotates as empty (nothing to
    /// show) rather than falling back to HEAD's copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
}

/// Commit metadata attached to every blame entry — enough for the gutter
/// text and the hover card, without a second RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameCommit {
    /// Full hex object id. The all-zero id means "not committed yet" (dirty
    /// buffer lines): the name/email/summary are then display strings rather
    /// than repository data, and both times are 0.
    pub id: String,
    pub author_name: String,
    pub author_email: String,
    /// Author time, Unix seconds.
    pub author_time: i64,
    /// Committer time, Unix seconds.
    pub committer_time: i64,
    /// Subject line (first line) of the commit message.
    pub summary: String,
}

/// One hunk of consecutive lines attributed to a single commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlameEntry {
    /// First line of the hunk in the blamed file, 0-BASED (editor line =
    /// `start_line + 1`), mirroring `gix_blame::BlameEntry`.
    pub start_line: u32,
    /// Number of lines in the hunk (>= 1).
    pub len: u32,
    pub commit: BlameCommit,
    /// The file's path in `commit` when it differs from the blamed path
    /// (i.e. the file was renamed somewhere after that commit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// Which implementation produced the entries. Surfaced so the porcelain
/// fallback stays observable instead of being a silent degradation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BlameVia {
    /// gix-blame, in-process and commit-graph accelerated (the default).
    Gix,
    /// `git blame --porcelain` fallback, taken when the gix path fails
    /// (large/pathological files, or an API boundary we do not cover).
    GitPorcelain,
}

/// `getBlame` response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GetBlameResult {
    /// Resolved commit id of the blamed revision (empty string for an
    /// unborn HEAD, where `entries` is empty). Clients key their render
    /// cache on `(path, rev)`.
    pub rev: String,
    /// Hunks in ascending `start_line` order. Coverage is total: they tile
    /// every line of the blamed content (asserted engine-side against the
    /// blob's line count before the response is built).
    pub entries: Vec<BlameEntry>,
    pub via: BlameVia,
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
