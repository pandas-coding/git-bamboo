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

export interface GraphPage {
  commits: Commit[];
  total_approx: number;
  has_more: boolean;
  epoch: number;
  anchor_commit: string | null;
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
