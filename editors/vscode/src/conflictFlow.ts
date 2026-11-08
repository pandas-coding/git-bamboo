/**
 * Shared conflict flow (phase 2 extraction, second-consumer rule): the
 * single controller for "conflict set query → conflicts bucket →
 * resolution actions → merge editor open" that every conflict producer
 * consumes. Before the rebase panel, pull/stash conflicts (netOps → the
 * Commit view's conflicts section) were the only consumer and the logic
 * lived inline in commitView.ts; the rebase flow is the second consumer,
 * so the controller moved here — one implementation, no copies.
 *
 * What is shared and what is not: the engine queries (conflicted items,
 * resolveConflict), the freshness cross-check, and the merge-editor open
 * live here. Surface-specific concerns — how a consumer renders the
 * conflicts bucket (the Commit view), or how it points the user at it
 * (showConflictsView below) — stay with the consumer. The merge editor's
 * input-order titles (ours/theirs vs. rebase's) are decided inside
 * mergeEditor.ts from the on-disk rebase state (isRebaseActive) — the
 * single mechanism, no per-consumer parameter plumbing.
 *
 * Inbound data rule: paths, resolutions, and orders arriving from a
 * webview are untrusted. Callers validate webview payloads field-by-field
 * before calling into this module; this module re-validates the shapes it
 * receives (repo-relative path, known resolution) so a forged caller
 * cannot reach an engine write RPC directly either. The engine
 * re-classifies every conflict at execution time (INVALID_ARGUMENT on a
 * stale snapshot) — the final arbiter, unchanged.
 */
import * as vscode from 'vscode';
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import { isValidRepoPath } from './headContent';
import { MergeEditorManager } from './mergeEditor';
import type { ConflictResolution, StatusItem } from './types';
import { errorMessage } from './util';

/** The four resolutions the engine's resolveConflict RPC accepts. */
const RESOLUTIONS: ReadonlySet<string> = new Set(['ours', 'theirs', 'keep_ours', 'keep_theirs']);

/** Outcome of ConflictFlow.resolve: consumers surface each kind with
 *  their own UI; the messages here are plain (engine text embedded). */
export type ResolveOutcome =
  /** Resolution applied — the consumer refreshes its status view. */
  | { kind: 'resolved' }
  /** Trust-boundary rejection: invalid path/resolution — dropped silently
   *  (the payload never reaches the engine; no error echo to probe). */
  | { kind: 'rejected' }
  /** The file is no longer conflicted in the checked snapshot. */
  | { kind: 'stale' }
  /** The engine rejected the resolution (kind mismatch / stale path);
   *  `message` is user-readable engine text. */
  | { kind: 'engine-rejected'; message: string }
  /** Real engine/transport failure; `message` is the raw error text. */
  | { kind: 'failed'; message: string };

/** Outcome of ConflictFlow.openMergeEditor. */
export type OpenMergeEditorOutcome =
  | { kind: 'opened' }
  | { kind: 'rejected' }
  | { kind: 'stale' }
  | { kind: 'failed'; message: string };

export class ConflictFlow implements vscode.Disposable {
  /** Merge-editor sessions for this repo's conflicts (tracks saves of open
   *  merge-editor outputs and stages finished resolutions). One manager
   *  per flow — pull/stash and rebase conflicts share the save hook. */
  private readonly mergeEditors: MergeEditorManager;

  constructor(
    private readonly client: EngineClient,
    root: vscode.Uri,
  ) {
    this.mergeEditors = new MergeEditorManager(client, root);
  }

  dispose(): void {
    this.mergeEditors.dispose();
  }

  /** The current conflicted entries (status === 'conflict'), straight
   *  from the engine. Resolves undefined when the status RPC fails or
   *  returns a malformed payload — callers treat that as "unknown",
   *  never as "no conflicts". */
  async currentConflicts(): Promise<StatusItem[] | undefined> {
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      if (!Array.isArray(items)) return undefined;
      return items.filter((item) => item && item.status === 'conflict');
    } catch {
      return undefined;
    }
  }

  /**
   * Resolve one conflicted file ("accept yours/theirs", keep-file /
   * keep-deletion buttons). Freshness cross-check against `snapshot`
   * (the status list the buttons were rendered on) when the caller has
   * one; otherwise against a freshly fetched status. A failed fresh
   * fetch skips the cross-check — the engine's own re-validation is the
   * final arbiter — instead of blocking resolution on a transient RPC
   * failure.
   */
  async resolve(
    relativePath: string,
    resolution: ConflictResolution,
    snapshot?: StatusItem[],
  ): Promise<ResolveOutcome> {
    if (!isValidRepoPath(relativePath) || !RESOLUTIONS.has(resolution)) {
      return { kind: 'rejected' };
    }
    let items = snapshot;
    if (items === undefined) {
      const fresh = await this.currentConflicts();
      if (fresh !== undefined) items = fresh;
    }
    if (
      items !== undefined &&
      !items.some((item) => item.path === relativePath && item.status === 'conflict')
    ) {
      return { kind: 'stale' };
    }
    try {
      await this.client.resolveConflict(relativePath, resolution);
      return { kind: 'resolved' };
    } catch (err) {
      // INVALID_ARGUMENT = kind mismatch or stale path (engine message is
      // user-readable); anything else is a real engine failure.
      if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
        return { kind: 'engine-rejected', message: errorMessage(err) };
      }
      return { kind: 'failed', message: errorMessage(err) };
    }
  }

  /**
   * Open the 3-way merge editor for one conflicted file. The input-order
   *  titles (ours/theirs vs. rebase's) are derived inside mergeEditor.ts
   * from the on-disk rebase state — the one mechanism for every
   *  consumer. The freshness cross-check mirrors `resolve`.
   */
  async openMergeEditor(
    relativePath: string,
    snapshot?: StatusItem[],
  ): Promise<OpenMergeEditorOutcome> {
    if (!isValidRepoPath(relativePath)) return { kind: 'rejected' };
    let items = snapshot;
    if (items === undefined) {
      const fresh = await this.currentConflicts();
      if (fresh !== undefined) items = fresh;
    }
    const current = items?.find(
      (item) => item.path === relativePath && item.status === 'conflict',
    );
    if (items !== undefined && !current) return { kind: 'stale' };
    try {
      await this.mergeEditors.openFor(relativePath, current?.conflict_kind);
      return { kind: 'opened' };
    } catch (err) {
      return { kind: 'failed', message: errorMessage(err) };
    }
  }
}

/**
 * Focuses the Commit view and expands its Conflicts section — the shared
 * entry point every conflict consumer uses to point the user at the one
 * conflicts bucket (the netOps pull/stash conflict banner's "Open
 * Conflict Files" button, and the rebase panel's docked conflict notice).
 * One conflict surface, one entry — consumers never build their own.
 */
export async function showConflictsView(): Promise<void> {
  await vscode.commands.executeCommand('gitBamboo.commit.showConflicts');
}
