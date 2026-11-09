import type { Uri } from 'vscode';

/**
 * Shared protocol types mirroring crates/protocol (JSON-RPC 2.0 over stdio).
 * Field names are snake_case to match the engine wire format exactly.
 */

export type StatusCode =
  | 'added'
  | 'modified'
  | 'deleted'
  | 'renamed'
  | 'copied'
  | 'untracked'
  | 'ignored'
  | 'conflict';

/** Conflict classification for StatusItem.status === 'conflict': which
 *  side added / modified / deleted. Mirrors protocol ConflictKind. */
export type ConflictKind =
  | 'both_modified'
  | 'both_added'
  | 'added_by_us'
  | 'added_by_them'
  | 'deleted_by_us'
  | 'deleted_by_them'
  | 'both_deleted';

export interface StatusItem {
  path: string;
  status: StatusCode;
  old_path?: string;
  /** Whether the change is staged (index vs HEAD) vs a worktree change. */
  staged: boolean;
  /** Conflict classification; absent for non-conflict entries. */
  conflict_kind?: ConflictKind;
}

/** How a conflict is resolved, per the engine's `resolveConflict` RPC.
 *  `ours`/`theirs` take that side's blob (content conflicts only);
 *  `keepOurs`/`keepTheirs` keep what that side did — for delete/modify
 *  conflicts, keeping the file or confirming the deletion. */
export type ConflictResolution = 'ours' | 'theirs' | 'keep_ours' | 'keep_theirs';

/** Map a UI side ("accept ours/theirs") to the resolution the engine
 *  accepts for the file's conflict kind: delete/modify kinds have no
 *  blob on one side, so only the keep_* variants are legal. This
 *  mirrors the engine's legality table (`action_for`); the engine
 *  re-validates at execution time, so a stale or forged kind can never
 *  drive an illegal resolution. Shared by commitView (row buttons,
 *  context menu) and mergeEditor (binary fast-path). */
export function sideResolution(
  kind: ConflictKind | undefined,
  side: 'ours' | 'theirs',
): ConflictResolution {
  const deleteModify =
    kind === 'deleted_by_us' || kind === 'deleted_by_them' || kind === 'both_deleted';
  if (side === 'ours') return deleteModify ? 'keep_ours' : 'ours';
  return deleteModify ? 'keep_theirs' : 'theirs';
}

export interface Commit {
  id: string;
  message: string;
  author_name: string;
  author_email: string;
  /** Unix seconds. */
  author_time: number;
  parent_ids: string[];
  lane: number;
}

export interface GraphViewport {
  offset: number;
  limit: number;
  anchor_commit: string | null;
  epoch: number;
}

/** Graph filter conditions, engine-side ("filter as a query"); an
 *  all-null filter means unfiltered. Semantics mirror the engine:
 *  author matches name OR email, case-insensitive substring; message
 *  matches the summary line, case-insensitive substring; path is a
 *  case-sensitive substring of any first-parent-changed path. */
export interface GraphFilter {
  author: string | null;
  message: string | null;
  path: string | null;
}

export interface GraphPage {
  commits: Commit[];
  /** Unfiltered: lower bound. Filtered: EXACT hit count. */
  total_approx: number;
  has_more: boolean;
  epoch: number;
  anchor_commit: string | null;
  /** The filtered walk hit a safety cap; `total_approx` is then a lower
   *  bound and the UI shows "≥ N". Absent on the wire when false. */
  truncated?: boolean;
}

/** Wire format of one file entry in the engine's `getCommitDetail` result. */
export interface CommitFile {
  path: string;
  old_path: string | null;
  status: 'A' | 'D' | 'M' | 'R' | 'C' | 'T';
  additions: number;
  deletions: number;
}

/** Result of the engine's `getCommitDetail` RPC (full commit metadata + files). */
export interface CommitDetail {
  id: string;
  message: string;
  author_name: string;
  author_email: string;
  /** Unix seconds. */
  author_time: number;
  committer_name: string;
  committer_email: string;
  /** Unix seconds. */
  committer_time: number;
  /** First parent is the diff baseline; empty for root commits. */
  parent_ids: string[];
  files: CommitFile[];
}

export type RefKind = 'branch' | 'tag' | 'remote_branch';

export interface Ref {
  name: string;
  kind: RefKind;
  target: string;
}

/** Result of the engine's `getHead` RPC (replaces parsing `.git/HEAD`). */
export interface GetHeadResult {
  /** Full HEAD commit sha. */
  head: string;
  /** Short branch name like "main"; null when HEAD is detached. */
  branch: string | null;
}

/** One remote as reported by the engine's `getRemotes` RPC.
 *  URLs are credential-redacted server-side before they leave the engine. */
export interface RemoteInfo {
  name: string;
  /** Fetch URL (first configured, `insteadOf` rewrites applied); null if
   *  no URL resolves. Credentials are redacted by the engine. */
  url: string | null;
  /** Whether bamboo treats this remote as the default push target
   *  (`remote.default` git config when set, else the first remote). */
  default: boolean;
}

/** Ahead/behind of a branch vs. its upstream (`getBranchStatus` RPC). */
export interface BranchStatus {
  branch: string;
  /** Short upstream name (e.g. "origin/main"); null when the branch has
   *  no configured upstream. */
  upstream: string | null;
  ahead: number;
  behind: number;
}

/** One entry of the engine's `stashList` RPC. */
export interface StashEntry {
  /** Reflog selector, e.g. "stash@{0}". */
  ref: string;
  message: string;
  /** Strict ISO-8601 commit date of the stash entry. */
  date: string;
}

/** One line of a diff hunk body, as served by the engine's `getDiff` RPC.
 *  `kind` mirrors the unified-diff origin marker; `text` excludes it. */
export type DiffLineKind = 'context' | 'added' | 'removed';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
}

/** One hunk of the engine's `getDiff` RPC. `new_start`/`new_lines` address
 *  the NEW side — the worktree lines for an unstaged diff, the index lines
 *  for a staged diff — and are the basis of stageRanges/unstageRanges. */
export interface DiffHunk {
  /** First line (1-based) on the old side; 0 when the old side is empty. */
  old_start: number;
  old_lines: number;
  /** First line (1-based) on the new side; 0 when the new side is empty
   *  (whole-file deletion). */
  new_start: number;
  new_lines: number;
  lines: DiffLine[];
}

export interface GetDiffResult {
  /** Binary files carry no hunks — hunk staging is impossible; the UI
   *  falls back to whole-file staging. */
  binary: boolean;
  hunks: DiffHunk[];
}

/** Inclusive 1-based line range on the new side of a diff, addressed by
 *  stageRanges (worktree lines) / unstageRanges (index lines). */
export interface LineRange {
  start: number;
  end: number;
}

// ---------------------------------------------------------------------------
// Blame (engine phase 3: getBlame)
// ---------------------------------------------------------------------------

/** Commit metadata the engine joins onto every blame entry — enough to
 *  render gutter annotations and hover cards without a second RPC (the
 *  client must never fan out per entry). */
export interface BlameCommit {
  /** Full hex object id. ALL ZEROS = "not committed yet" (a line that only
   *  exists in the blamed buffer): the identity fields are then display
   *  strings rather than repository data and both times are 0. */
  id: string;
  author_name: string;
  author_email: string;
  /** Unix seconds. */
  author_time: number;
  /** Unix seconds. */
  committer_time: number;
  /** Subject line (first line) of the commit message. */
  summary: string;
}

/** One hunk of consecutive lines attributed to a single commit. */
export interface BlameEntry {
  /** First line of the hunk in the blamed file, 0-BASED (editor line =
   *  `start_line + 1`). */
  start_line: number;
  /** Hunk length in lines (>= 1). Hunks tile the whole blamed content in
   *  ascending order. */
  len: number;
  commit: BlameCommit;
  /** The file's path in `commit` when it differs from the blamed path, i.e.
   *  the file was renamed somewhere after that commit — the path to open
   *  when walking back to the commit's parent revision. */
  source_path?: string;
}

/** Which implementation produced the entries (kept observable so the
 *  porcelain fallback is never a silent degradation). */
export type BlameVia = 'gix' | 'git-porcelain';

/** Result of the engine's `getBlame` RPC. */
export interface GetBlameResult {
  /** Resolved commit id of the blamed revision; empty string for an unborn
   *  HEAD, where `entries` is empty. */
  rev: string;
  entries: BlameEntry[];
  via: BlameVia;
}

/** The engine's placeholder object id for a line that exists only in the
 *  blamed buffer: all zeros, at whatever width the repository's hash uses
 *  (40 for SHA-1, 64 for SHA-256) — hence the run-of-zeros pattern rather
 *  than one literal. Shared by isUncommittedBlame and the getBlame response
 *  validator so the two cannot drift: a validator that rejected an id this
 *  predicate accepts would throw away exactly the entries the UI knows how
 *  to render, and one that accepted more would let an arbitrary string
 *  reach surfaces that interpolate commit ids. */
export const NULL_COMMIT_ID_RE = /^0+$/;

/** True for the engine's "not committed yet" placeholder commit (the
 *  all-zero object id, SHA-1 or SHA-256 width). */
export function isUncommittedBlame(commit: BlameCommit): boolean {
  return NULL_COMMIT_ID_RE.test(commit.id);
}

/** The hunk's new-side action interval — the shared host/engine contract
 *  (engine `RawHunk::new_side_interval`): 1-based inclusive with a floor of
 *  line 1 and a minimum width of one, so empty new sides stay addressable. */
export function hunkNewInterval(hunk: DiffHunk): LineRange {
  const start = Math.max(hunk.new_start, 1);
  return { start, end: start + Math.max(hunk.new_lines, 1) - 1 };
}

/** Merge strategy for `pull`. `default` adds no flag so the user's
 *  `pull.rebase` / `pull.ff` git config is respected. */
export type PullStrategy = 'default' | 'merge' | 'rebase' | 'ff-only';

/** Push force semantics (three tiers, see protocol `ForceMode`). */
export type ForceMode = 'none' | 'lease' | 'force';

export interface UndoEntry {
  id: number;
  /** Unix seconds. */
  timestamp: number;
  description: string;
  epoch_at_creation: number;
}

export interface InitializeResult {
  session_id: string;
  protocol_version: string;
  engine_version: string;
  epoch: number;
  capabilities: Record<string, unknown>;
}

/** Mutable per-repo state shared between the extension host components. */
export interface RepoState {
  epoch: number;
  currentBranch: string | undefined;
  /** Repository root, used to resolve repo-relative paths (e.g. for commit detail). */
  root: Uri;
}

// ---------------------------------------------------------------------------
// Interactive rebase (engine phase 1)
// ---------------------------------------------------------------------------

/** One todo-list action of an interactive rebase, as the engine accepts it.
 *  `pick`/`reword`/`squash`/`drop` are valid in a startRebase todo;
 *  `fixup`/`edit` only appear in status entries reported by the engine. */
export type RebaseAction = 'pick' | 'reword' | 'squash' | 'fixup' | 'drop' | 'edit';

/** One entry of the todo list sent to the engine's `startRebase` RPC.
 *  `id` is a full sha from getRebasePlan; the engine requires the todo's
 *  id set to exactly equal the current upstream..branch commit set.
 *  `message` is only honored for `reword`/`squash`. */
export interface RebaseTodoEntry {
  id: string;
  action: RebaseAction;
  message?: string;
}

/** One commit of the engine's `getRebasePlan` result, in replay order
 *  (oldest first). `time` is unix seconds. */
export interface RebasePlanCommit {
  id: string;
  subject: string;
  author: string;
  time: number;
}

/** Result of the engine's `getRebasePlan` RPC: the replay window for an
 *  interactive rebase onto `onto` — commits are upstream..branch, in
 *  replay order. `upstream` is merge-base(onto, branch) as a full sha. */
export interface GetRebasePlanResult {
  onto: string;
  upstream: string;
  branch: string;
  commits: RebasePlanCommit[];
}

/** Whether an interactive rebase is on disk, per `getRebaseStatus`. */
export type RebaseRunState = 'idle' | 'stopped' | 'running';

/** Rebase backend in use, per `getRebaseStatus` (omitted when idle). */
export type RebaseBackend = 'merge' | 'apply';

/** One entry of the engine's `getRebaseStatus` result, tagged on the
 *  wire (`kind`):
 *  - `entry`: a parsed pick-like todo line (comments and blank lines are
 *    dropped by the engine);
 *  - `raw`: any other non-empty todo line (labels, `exec`, `break`,
 *    `merge`, unknown syntax), passed through verbatim so status views
 *    never lose information.
 *  UNTRUSTED DATA: subjects and raw lines originate in the repository's
 *  todo files; treat them as plain text (no markup interpretation). */
export type RebaseStatusEntry =
  | { kind: 'entry'; action: RebaseAction; id: string; subject: string }
  | { kind: 'raw'; line: string };

/** Result of the engine's `getRebaseStatus` RPC. `done`/`remaining` are
 *  the parsed (and raw-passthrough) views of git's todo files; blank and
 *  comment lines are dropped. Optional fields are omitted by the engine
 *  when unknown or not applicable (e.g. when idle). */
export interface GetRebaseStatusResult {
  state: RebaseRunState;
  backend?: RebaseBackend;
  branch?: string;
  onto?: string;
  orig_head?: string;
  stopped_sha?: string;
  msgnum?: number;
  end?: number;
  done: RebaseStatusEntry[];
  remaining: RebaseStatusEntry[];
}

/** Why a rebase stopped: a merge conflict, or a `reword` waiting for the
 *  user's message (a normal outcome, not an error). */
export type RebaseStopReason = 'conflict' | 'reword_stop';

/** Result of the engine's `startRebase`/`rebaseContinue`/`rebaseSkip`
 *  (and `rebaseAbort`, where it reports the aborted run). `stopped: true`
 *  means the rebase is on disk waiting for the user — NOT an error;
 *  `stopped: false` means completed/nothing left (`!stopped` IS the
 *  finished state). Real failures reject with the engine's RPC error
 *  instead. */
export interface RebaseResult {
  stopped: boolean;
  reason?: RebaseStopReason;
  stopped_sha?: string;
  msgnum?: number;
  end?: number;
}
