/**
 * Webview view provider for the Commit view (view id: gitBamboo.commit):
 * commit message + staged/changes/untracked file list + commit/amend in a
 * single JetBrains-style panel. Serves the built bundle (out/webview/
 * commit.html) and relays stage/unstage/commit/openResource
 * messages to the engine; right-clicked file rows park a context target
 * consumed by
 * the webview/context menu commands. Status pushes are debounced (50ms)
 * and coalesce concurrent refreshes, mirroring the old scmProvider's
 * refresh pattern.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient, rpcErrorCode } from './engineClient';
import { createContextTargetSlot } from './contextTargetSlot';
import { isValidRepoPath, openResource } from './headContent';
import { MergeEditorManager } from './mergeEditor';
import type { ConflictKind, ConflictResolution, StatusItem } from './types';
import { errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

/** Engine JSON-RPC INVALID_ARGUMENT (see protocol BambooError) — the
 *  resolveConflict kind-mismatch / stale-snapshot rejection. */
const RPC_INVALID_ARGUMENT = -32005;

/** Debounce for status refreshes (the engine coalesces fs events at 50ms). */
const REFRESH_DEBOUNCE_MS = 50;

/** Valid context-menu buckets (mirror of the webview's BUCKETS). */
const CONTEXT_BUCKETS = new Set(['conflicts', 'staged', 'changes', 'untracked']);

interface CommitViewMessage {
  type?: string;
  paths?: string[];
  path?: string;
  status?: string;
  message?: string;
  amend?: boolean;
  resolution?: string;
  /** contextTarget payload: the file under the webview's context menu
   *  (null = right-click landed on a non-file row; clears any target). */
  file?: { path?: unknown; status?: unknown; bucket?: unknown } | null;
}

/** The file the Commit view's context menu targeted, as shown in its row. */
export interface CommitContextTarget {
  path: string;
  status: string;
  bucket: 'conflicts' | 'staged' | 'changes' | 'untracked';
}

/** Bucketing rule mirrored from the webview (commit.js bucketOf):
 *  conflicted files first (conflicts take priority over staged state —
 *  an unmerged entry is neither staged nor unstaged), then staged,
 *  then untracked, then worktree changes. */
function bucketOf(item: StatusItem): 'conflicts' | 'staged' | 'changes' | 'untracked' {
  if (item.status === 'conflict') return 'conflicts';
  if (item.staged) return 'staged';
  if (item.status === 'untracked') return 'untracked';
  return 'changes';
}

/** Map a UI side ("accept ours/theirs") to the resolution the engine
 *  accepts for the file's conflict kind: delete/modify kinds need the
 *  keep_* variants (one side has no blob to take). The webview row
 *  buttons apply the same mapping client-side. */
export function sideResolution(
  kind: ConflictKind | undefined,
  side: 'ours' | 'theirs',
): ConflictResolution {
  const deleteModify =
    kind === 'deleted_by_us' || kind === 'deleted_by_them' || kind === 'both_deleted';
  if (side === 'ours') return deleteModify ? 'keep_ours' : 'ours';
  return deleteModify ? 'keep_theirs' : 'theirs';
}

export class CommitViewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'gitBamboo.commit';

  private view: vscode.WebviewView | undefined;
  private refreshTimer: NodeJS.Timeout | undefined;
  private refreshQueued = false;
  private refreshing = false;
  private committing = false;
  private disposed = false;
  /** Last status items pushed to the webview — context targets are
   *  validated against this at consumption so a parked target always
   *  refers to a file currently listed in the view. */
  private lastItems: StatusItem[] = [];
  /** Parked context-menu target (right-clicked file row), consumed by the
   *  webview/context menu commands in extension.ts. */
  private readonly contextSlot = createContextTargetSlot<CommitContextTarget>();
  /** Per-view subscriptions, torn down on re-resolution so listeners
   *  never double up (mirrors GraphWebviewProvider.teardownSession). */
  private viewDisposables: vscode.Disposable[] = [];
  /** Merge-editor sessions for this repo's conflicts (tracks saves of
   *  open merge-editor outputs and stages finished resolutions). */
  private readonly mergeEditors: MergeEditorManager;
  /** A showConflicts request that raced the webview's readiness (net-ops
   *  banner can fire before the view is resolved): replayed with the
   *  next status push. */
  private pendingShowConflicts = false;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {
    this.mergeEditors = new MergeEditorManager(client, root);
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    // Re-resolution (view disposed then re-opened): tear down any prior
    // view subscriptions first, so stale onDidDispose handlers can never
    // clear the new view or accumulate.
    this.teardownView();
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, 'out', 'webview'),
        vscode.Uri.joinPath(this.context.extensionUri, 'webview'),
      ],
    };
    view.webview.html = buildWebviewHtml(this.context.extensionUri, view.webview, 'commit.html');
    this.viewDisposables.push(
      view.webview.onDidReceiveMessage((message: CommitViewMessage) => this.onMessage(message)),
      view.onDidDispose(() => {
        this.view = undefined;
        this.teardownView();
      }),
    );
    // No initial push here: the webview sends 'ready' once it has finished
    // loading (a postMessage before that would be dropped anyway), and the
    // ready handler performs the first status fetch.
  }

  /** Disposes the current view's subscriptions (re-resolution / disposal). */
  private teardownView(): void {
    for (const disposable of this.viewDisposables) disposable.dispose();
    this.viewDisposables.length = 0;
    this.contextSlot.clear();
  }

  /** Debounced refresh entry point — engine notification hooks call this. */
  refresh(): void {
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = setTimeout(() => void this.doRefresh(), REFRESH_DEBOUNCE_MS);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.mergeEditors.dispose();
    this.teardownView();
    this.view = undefined;
  }

  private onMessage(message: CommitViewMessage): void {
    if (this.disposed || !message || typeof message.type !== 'string') return;
    switch (message.type) {
      case 'ready':
        void this.doRefresh();
        break;
      case 'stage':
      case 'unstage':
        void this.changePaths(message.type, Array.isArray(message.paths) ? message.paths : []);
        break;
      case 'openResource':
        if (typeof message.path === 'string') {
          void this.openResource(message.path, message.status ?? '');
        }
        break;
      case 'resolveConflict':
        if (typeof message.path === 'string') {
          void this.resolveConflict(message.path, message.resolution ?? '');
        }
        break;
      case 'openMergeEditor':
        if (typeof message.path === 'string') {
          void this.openMergeEditor(message.path);
        }
        break;
      case 'commit':
        void this.commit(
          typeof message.message === 'string' ? message.message : '',
          message.amend === true,
        );
        break;
      case 'contextTarget':
        // Right-click on a file row (fires before the native menu opens);
        // anything but a well-formed payload (null non-row click or
        // malformed data) clears the parked target so it can't outlive
        // its gesture. Webview data is a trust boundary: only a
        // well-formed repo-relative path is parked.
        if (message.file === null) {
          this.contextSlot.clear();
        } else if (message.file) {
          this.parkContextTarget(message.file);
        }
        break;
    }
  }

  private async doRefresh(): Promise<void> {
    if (this.refreshing) {
      this.refreshQueued = true;
      return;
    }
    this.refreshing = true;
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      if (this.disposed) return;
      this.lastItems = Array.isArray(items) ? items : [];
      await this.view?.webview.postMessage({
        type: 'status',
        items: this.lastItems,
      });
      // Replay a showConflicts request that raced the webview's readiness
      // (sent while the view was resolving and its webview not yet loaded).
      if (this.pendingShowConflicts && this.view) {
        this.pendingShowConflicts = false;
        await this.view.webview.postMessage({ type: 'showConflicts' });
      }
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: status refresh failed — ${errorMessage(err)}`);
    } finally {
      this.refreshing = false;
      if (this.refreshQueued) {
        this.refreshQueued = false;
        this.refresh();
      }
    }
  }

  private async changePaths(operation: 'stage' | 'unstage', paths: string[]): Promise<void> {
    // Webview-supplied paths are a trust boundary: only forward repo-relative
    // paths reach the engine write RPCs.
    const safePaths = paths.filter((p) => isValidRepoPath(p));
    if (safePaths.length === 0) return;
    try {
      await this.client.request(operation, { paths: safePaths });
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: ${operation} failed — ${errorMessage(err)}`);
    }
  }

  /** Discards worktree changes for `paths` after the irreversible
   *  confirmation dialog (VS Code/GitLens convention: modal + IRREVERSIBLE
   *  detail). `labelPath` names the file in the single-file message (the
   *  single path by default). After the modal, status is re-fetched and
   *  every path re-checked — the dialog was answered against a snapshot,
   *  and an irreversible write must not act on paths whose state changed
   *  underneath it. The engine re-classifies again at execution time. */
  private async discardPaths(
    paths: string[],
    labelPath = paths.length === 1 ? paths[0] : '',
  ): Promise<void> {
    // Same trust boundary as changePaths: only repo-relative paths are
    // forwarded to the engine write RPC.
    const safePaths = paths.filter((p) => isValidRepoPath(p));
    if (safePaths.length === 0) return;
    if (!(await this.confirmDiscard(safePaths.length, labelPath))) return;
    if (!(await this.verifyStillDiscardable(safePaths))) return;
    try {
      await this.client.discard(safePaths);
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: discard failed — ${errorMessage(err)}`);
    }
  }

  /** Post-confirmation freshness check (TOCTOU guard): re-reads status and
   *  requires every path to still be listed in a discardable (non-staged)
   *  bucket, mirroring takeContextTarget's bucket cross-check. Aborts with
   *  a warning — nothing is discarded — when the file list changed since
   *  the dialog was answered. */
  private async verifyStillDiscardable(paths: string[]): Promise<boolean> {
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      const current = Array.isArray(items) ? items : [];
      this.lastItems = current; // keep the cache fresh either way
      const changed = paths.some((p) => {
        const item = current.find((i) => i.path === p);
        return item === undefined || bucketOf(item) === 'staged';
      });
      if (changed) {
        vscode.window.showWarningMessage(
          'Git Bamboo: the file list changed since the confirmation — ' +
            'nothing was discarded. Please retry.',
        );
        return false;
      }
      return true;
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: discard pre-check failed — ${errorMessage(err)}`,
      );
      return false;
    }
  }

  /** Modal confirmation for the destructive discard flow. Returns true only
   *  when the explicit destructive button was picked (modal dialogs treat
   *  ESC/close as cancel). */
  private async confirmDiscard(count: number, labelPath: string): Promise<boolean> {
    const message =
      count === 1
        ? `Are you sure you want to discard changes in '${path.posix.basename(labelPath)}'?`
        : `Are you sure you want to discard changes in ${count} files?`;
    const action =
      count === 1 ? 'Discard Changes' : `Discard All ${count} Files`;
    const confirmed = await vscode.window.showWarningMessage(
      message,
      {
        modal: true,
        detail: 'This is IRREVERSIBLE! Your current working set will be FOREVER LOST.',
      },
      action,
    );
    return confirmed === action;
  }

  private async openResource(relativePath: string, status: string): Promise<void> {
    try {
      await openResource(this.root, relativePath, status);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: opening ${relativePath} failed — ${errorMessage(err)}`);
    }
  }

  /** Resolve one conflicted file (webview "Accept Yours/Theirs" or
   *  keep-file/keep-deletion buttons). Same flow shape as discard minus
   *  the modal (each resolution is reversible enough via git); the engine
   *  re-classifies the conflict at execution time and rejects a stale
   *  snapshot with INVALID_ARGUMENT, surfaced here as a warning. */
  private async resolveConflict(relativePath: string, resolution: string): Promise<void> {
    // Trust boundary: only repo-relative paths and the four known
    // resolutions reach the engine write RPC.
    const resolutions = new Set<ConflictResolution>(['ours', 'theirs', 'keep_ours', 'keep_theirs']);
    if (!isValidRepoPath(relativePath) || !resolutions.has(resolution as ConflictResolution)) {
      return;
    }
    // Freshness cross-check: the buttons were rendered against a status
    // snapshot; require the file to still be listed as conflicted.
    const current = this.lastItems.find(
      (item) => item.path === relativePath && item.status === 'conflict',
    );
    if (current === undefined) {
      vscode.window.showWarningMessage(
        `Git Bamboo: ${path.posix.basename(relativePath)} is no longer conflicted — nothing was changed.`,
      );
      return;
    }
    try {
      await this.client.resolveConflict(relativePath, resolution as ConflictResolution);
      await this.doRefresh();
    } catch (err) {
      // INVALID_ARGUMENT = kind mismatch or stale path (engine message is
      // user-readable); anything else is a real engine failure.
      if (rpcErrorCode(err) === RPC_INVALID_ARGUMENT) {
        vscode.window.showWarningMessage(`Git Bamboo: ${errorMessage(err)}`);
      } else {
        vscode.window.showErrorMessage(
          `Git Bamboo: resolving ${relativePath} failed — ${errorMessage(err)}`,
        );
      }
    }
  }

  /** Open the 3-way merge editor for one conflicted file (webview
   *  "Resolve in Merge Editor" button). */
  private async openMergeEditor(relativePath: string): Promise<void> {
    if (!isValidRepoPath(relativePath)) return;
    const current = this.lastItems.find(
      (item) => item.path === relativePath && item.status === 'conflict',
    );
    if (current === undefined) {
      vscode.window.showWarningMessage(
        `Git Bamboo: ${path.posix.basename(relativePath)} is no longer conflicted.`,
      );
      return;
    }
    try {
      await this.mergeEditors.openFor(relativePath, current.conflict_kind);
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: opening the merge editor for ${relativePath} failed — ${errorMessage(err)}`,
      );
    }
  }

  /** Focus the Commit view and expand its Conflicts section (net-ops
   *  banner's "Open Conflict Files" button). */
  async showConflicts(): Promise<void> {
    if (this.view) {
      this.view.show(true);
    } else {
      await vscode.commands.executeCommand(`${CommitViewProvider.viewId}.focus`);
    }
    // Send immediately when a view is attached, and flag the request so
    // the next status push replays it: a freshly resolved view's webview
    // is not loaded yet (the ready handshake triggers that push), and
    // would silently drop the message. The replay is idempotent.
    this.pendingShowConflicts = true;
    if (this.view) {
      await this.view.webview.postMessage({ type: 'showConflicts' });
    }
  }

  /** Returns (and clears) the file targeted by the Commit view's context
   *  menu, if any — but only when the file is still listed in the current
   *  status under the same bucket (a refresh may have moved or removed it
   *  since the right-click). Called by the context-menu command handlers
   *  in extension.ts (Copy Path reads the target directly). */
  takeContextTarget(): CommitContextTarget | undefined {
    const target = this.contextSlot.take();
    if (!target) return undefined;
    const current = this.lastItems.find((item) => item.path === target.path);
    return current !== undefined && bucketOf(current) === target.bucket ? target : undefined;
  }

  /** Runs a context-menu action against the parked target. Returns false
   *  when no current target is parked (safety net for programmatic
   *  invocation of the palette-hidden commands). Stage/unstage go through
   *  changePaths (path validation + refresh); open delegates to the shared
   *  openResource helper; discard goes through the confirmation flow;
   *  conflict actions (merge editor / accept a side) apply only to the
   *  conflicts bucket, with ours/theirs mapped to the kind's legal
   *  resolution. */
  async runContextCommand(
    action: 'openChanges' | 'stageFile' | 'unstageFile' | 'discardFile' | 'openMergeEditor' | 'resolveConflict',
    side?: 'ours' | 'theirs',
  ): Promise<boolean> {
    const target = this.takeContextTarget();
    if (!target) return false;
    // Bucket cross-check, mirroring the menus' when clauses: stageFile only
    //  for unstaged buckets, unstageFile only for staged.
    if (action === 'stageFile' && target.bucket === 'staged') return false;
    if (action === 'unstageFile' && target.bucket !== 'staged') return false;
    // Discard restores the worktree from the index, which for a staged file
    //  would silently rewrite it to its pre-stage content — the staged
    //  bucket deliberately offers no discard (mirrors the menu when clause).
    if (action === 'discardFile' && target.bucket === 'staged') return false;
    if (
      (action === 'openMergeEditor' || action === 'resolveConflict') &&
      target.bucket !== 'conflicts'
    ) {
      return false;
    }
    if (action === 'openChanges') {
      await this.openResource(target.path, target.status);
    } else if (action === 'openMergeEditor') {
      await this.openMergeEditor(target.path);
    } else if (action === 'resolveConflict') {
      const kind = this.lastItems.find(
        (item) => item.path === target.path && item.status === 'conflict',
      )?.conflict_kind;
      await this.resolveConflict(
        target.path,
        sideResolution(kind, side === 'theirs' ? 'theirs' : 'ours'),
      );
    } else if (action === 'discardFile') {
      await this.discardPaths([target.path]);
    } else {
      await this.changePaths(action === 'stageFile' ? 'stage' : 'unstage', [target.path]);
    }
    return true;
  }

  /** View-title "Discard All" for one bucket: every path currently listed
   *  in that bucket, one IRREVERSIBLE confirmation, one engine discard RPC.
   *  Returns false when the bucket is empty (nothing to discard). The
   *  staged bucket is deliberately not supported (see runContextCommand). */
  async discardBucket(bucket: 'changes' | 'untracked'): Promise<boolean> {
    const paths = this.lastItems
      .filter((item) => bucketOf(item) === bucket)
      .map((item) => item.path);
    if (paths.length === 0) return false;
    await this.discardPaths(paths);
    return true;
  }

  private parkContextTarget(file: NonNullable<CommitViewMessage['file']>): void {
    const { path, status, bucket } = file;
    if (
      typeof path !== 'string' ||
      !isValidRepoPath(path) ||
      typeof status !== 'string' ||
      typeof bucket !== 'string' ||
      !CONTEXT_BUCKETS.has(bucket)
    ) {
      this.contextSlot.clear(); // malformed payload: clear, don't keep stale
      return;
    }
    this.contextSlot.park({ path, status, bucket: bucket as CommitContextTarget['bucket'] });
  }

  /**
   * Commit semantics carried over from the old scmProvider: empty message
   * is rejected for plain commits; amend with an empty message runs the
   * engine's `--amend --no-edit` path.
   */
  private async commit(message: string, amend: boolean): Promise<void> {
    if (this.committing) return; // in-flight guard: no double commits
    if (!message && !amend) {
      vscode.window.showWarningMessage('Git Bamboo: commit message is empty');
      return;
    }
    this.committing = true;
    try {
      await this.client.request('commit', { message, amend });
      await this.view?.webview.postMessage({ type: 'committed' });
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: ${amend ? 'amend' : 'commit'} failed — ${errorMessage(err)}`,
      );
    } finally {
      this.committing = false;
    }
  }
}
