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
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import { createContextTargetSlot } from './contextTargetSlot';
import { isValidRepoPath, openResource, type CommitBucket } from './headContent';
import { MergeEditorManager } from './mergeEditor';
import { sideResolution } from './types';
import type { ConflictResolution, StashEntry, StatusItem } from './types';
import { errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

/** Debounce for status refreshes (the engine coalesces fs events at 50ms). */
const REFRESH_DEBOUNCE_MS = 50;

/** Valid context-menu buckets (mirror of the webview's BUCKETS). */
const CONTEXT_BUCKETS = new Set(['conflicts', 'staged', 'changes', 'untracked']);

/** Stash reflog selectors the host accepts (the engine re-validates
 *  the ref at execution time; this gate keeps malformed webview payloads
 *  from reaching the RPC at all). */
const STASH_REF_RE = /^stash@\{\d{1,9}\}$/;

/** Host-side stash message bound — mirrors the engine's limit so an
 *  over-long message is rejected before the write queue is involved. */
const MAX_STASH_MESSAGE_LEN = 500;

interface CommitViewMessage {
  type?: string;
  paths?: string[];
  path?: string;
  status?: string;
  /** Commit-view bucket of the row that requested the open (staged →
   *  index-vs-HEAD diff, changes → worktree-vs-index diff, which hunk
   *  staging's CodeLens actions target). Optional for legacy rows. */
  bucket?: string;
  message?: string;
  amend?: boolean;
  resolution?: string;
  /** Stash section (phase 5). */
  include_untracked?: boolean;
  staged_only?: boolean;
  ref?: string;
  reinstate_index?: boolean;
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

/** Parse a webview-supplied bucket label — webview data is a trust
 *  boundary, so unknown values degrade to the legacy (bucket-less)
 *  openResource behavior instead of being forwarded. */
function parseBucket(value: unknown): CommitBucket | undefined {
  return typeof value === 'string' && CONTEXT_BUCKETS.has(value) ? (value as CommitBucket) : undefined;
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
        void this.refreshStashes();
        break;
      case 'stage':
      case 'unstage':
        void this.changePaths(message.type, Array.isArray(message.paths) ? message.paths : []);
        break;
      case 'openResource':
        if (typeof message.path === 'string') {
          void this.openResource(message.path, message.status ?? '', parseBucket(message.bucket));
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
      case 'stashCreate':
        void this.stashCreate(
          typeof message.message === 'string' ? message.message : '',
          message.include_untracked === true,
          message.staged_only === true,
        );
        break;
      case 'stashPop':
      case 'stashApply':
        void this.stashPop(
          message.type === 'stashApply',
          typeof message.ref === 'string' ? message.ref : '',
          message.reinstate_index !== false,
        );
        break;
      case 'stashDrop':
        void this.stashDrop(typeof message.ref === 'string' ? message.ref : '');
        break;
      case 'stashRefresh':
        void this.refreshStashes();
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

  private async openResource(
    relativePath: string,
    status: string,
    bucket: CommitBucket | undefined,
  ): Promise<void> {
    try {
      await openResource(this.root, relativePath, status, bucket);
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
      if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
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
    // Send immediately when a view is attached; arm the replay flag only
    // when that send may have been dropped (no view yet, or the webview
    // not loaded — postMessage resolves false then). The replay rides on
    // the next status push: a freshly resolved view's webview is not
    // loaded yet, and an armed flag would otherwise re-expand the
    // conflicts section a second time after a successful send.
    const delivered = this.view
      ? await this.view.webview.postMessage({ type: 'showConflicts' })
      : false;
    this.pendingShowConflicts = !delivered;
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
      await this.openResource(target.path, target.status, target.bucket);
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

  // -------------------------------------------------------------------------
  // Stashes section (phase 5)
  //
  // Webview payloads are untrusted: refs are checked against STASH_REF_RE
  // and re-verified against a freshly fetched list before any mutating
  // RPC (stash indices shift as entries are pushed/dropped, so a stale
  // ref would target the wrong entry). The engine re-validates again.
  // -------------------------------------------------------------------------

  /** Pushes the current stash list to the webview (called on ready, after
   *  every stash operation, and on refsChanged — external `git stash`
   *  commands move refs/stash, which the engine's watcher reports). */
  async refreshStashes(): Promise<void> {
    try {
      const entries = await this.client.stashList();
      if (this.disposed) return;
      await this.view?.webview.postMessage({ type: 'stashes', entries });
    } catch (err) {
      // Non-fatal: the stash list is secondary to the status list, so a
      // transient failure shouldn't surface an error toast every push.
      console.warn(`Git Bamboo: stash list refresh failed — ${errorMessage(err)}`);
    }
  }

  /** Stash message gate — the webview cannot be trusted to send sane
   *  text (control characters, oversized payloads). Empty is legal and
   *  becomes the engine's default `WIP on <branch>` message. */
  private normalizeStashMessage(message: string): string | null | undefined {
    const trimmed = message.trim();
    if (trimmed.length === 0) return null;
    if (trimmed.length > MAX_STASH_MESSAGE_LEN) return undefined;
    if (/\u0000|[\u0001-\u001f\u007f]/.test(trimmed)) return undefined;
    return trimmed;
  }

  async stashCreate(message: string, includeUntracked: boolean, stagedOnly: boolean): Promise<void> {
    const normalized = this.normalizeStashMessage(message);
    if (normalized === undefined) {
      vscode.window.showWarningMessage(
        `Git Bamboo: stash message rejected (max ${MAX_STASH_MESSAGE_LEN} characters, no control characters)`,
      );
      return;
    }
    try {
      await this.client.stashPush(normalized, includeUntracked, stagedOnly);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: stash create failed — ${errorMessage(err)}`);
      return; // keep the list as-is on failure
    }
    await this.refreshStashes();
    await this.doRefresh();
  }

  /** Fetches the live stash list and returns the entry named by `ref`,
   *  or undefined (no entry, malformed ref, or RPC failure). */
  private async resolveStashEntry(ref: string): Promise<StashEntry | undefined> {
    if (!STASH_REF_RE.test(ref)) return undefined;
    const entries = await this.client.stashList();
    return entries.find((entry) => entry.ref === ref);
  }

  async stashPop(apply: boolean, ref: string, reinstateIndex: boolean): Promise<void> {
    let entry: StashEntry | undefined;
    try {
      entry = await this.resolveStashEntry(ref);
    } catch {
      // falls through to the not-found path below
    }
    if (!entry) {
      vscode.window.showWarningMessage(
        'Git Bamboo: stash entry is no longer current — the list has changed, retry from the view',
      );
      await this.refreshStashes();
      return;
    }
    try {
      await this.client.stashPop(entry.ref, apply, reinstateIndex);
      vscode.window.setStatusBarMessage(
        `Git Bamboo: stash ${apply ? 'applied' : 'popped'} — ${entry.message} (${entry.ref})`,
        4_000,
      );
    } catch (err) {
      // Pop-with-conflicts keeps the stash entry by design (git never
      // drops a stash that didn't apply cleanly); surface that the
      // conflicts need resolving and offer the section reveal.
      if (rpcErrorCode(err) === RpcErrorCode.STASH_CONFLICT) {
        const action = await vscode.window.showWarningMessage(
          `Git Bamboo: stash applied with conflicts — the stash entry is kept`,
          'Open Conflict Files',
        );
        if (action === 'Open Conflict Files') await this.showConflicts();
      } else {
        vscode.window.showErrorMessage(
          `Git Bamboo: stash ${apply ? 'apply' : 'pop'} failed — ${errorMessage(err)}`,
        );
      }
    }
    // Either way the worktree/index moved (or conflicts appeared): the
    // stash list is unchanged only for apply-without-conflict, but a
    // refresh is cheap and idempotent.
    await this.refreshStashes();
    await this.doRefresh();
  }

  async stashDrop(ref: string): Promise<void> {
    let entry: StashEntry | undefined;
    try {
      entry = await this.resolveStashEntry(ref);
    } catch {
      // falls through to the not-found path below
    }
    if (!entry) {
      vscode.window.showWarningMessage(
        'Git Bamboo: stash entry is no longer current — the list has changed, retry from the view',
      );
      await this.refreshStashes();
      return;
    }
    // Destructive and irreversible: the modal names the entry so the
    // user knows exactly which stash goes away.
    const confirm = await vscode.window.showWarningMessage(
      `Drop ${entry.ref}?`,
      { modal: true, detail: `${entry.message}\n\nThis permanently removes the stash entry.`, },
      'Drop Stash',
    );
    if (confirm !== 'Drop Stash') return;
    try {
      await this.client.stashDrop(entry.ref);
      vscode.window.setStatusBarMessage(`Git Bamboo: dropped ${entry.ref}`, 4_000);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: stash drop failed — ${errorMessage(err)}`);
    }
    await this.refreshStashes();
  }
}
