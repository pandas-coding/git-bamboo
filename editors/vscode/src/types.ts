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

export interface StatusItem {
  path: string;
  status: StatusCode;
  old_path?: string;
  /** Whether the change is staged (index vs HEAD) vs a worktree change. */
  staged: boolean;
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
}
