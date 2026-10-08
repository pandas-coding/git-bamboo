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

import type { Uri } from 'vscode';

/** Mutable per-repo state shared between the extension host components. */
export interface RepoState {
  epoch: number;
  currentBranch: string | undefined;
  /** Repository root, used to resolve repo-relative paths (e.g. for commit detail). */
  root: Uri;
}
