/**
 * Webview view provider for the Commit view (view id: gitBamboo.commit):
 * commit message + staged/changes/untracked file list + commit/amend in a
 * single JetBrains-style panel. Serves the built bundle (out/webview/
 * commit.html) and relays stage/unstage/commit/openResource messages to
 * the engine; right-clicked file rows park a context target consumed by
 * the webview/context menu commands. Status pushes are debounced (50ms)
 * and coalesce concurrent refreshes, mirroring the old scmProvider's
 * refresh pattern.
 */
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { isValidRepoPath, openResource } from './headContent';
import type { StatusItem } from './types';
import { errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

/** Debounce for status refreshes (the engine coalesces fs events at 50ms). */
const REFRESH_DEBOUNCE_MS = 50;

/** How long a parked context-menu target stays usable: the native menu
 *  opens (and is either acted on or dismissed) immediately, so anything
 *  not consumed within this window is stale — this guards against
 *  programmatic invocation of the hidden palette commands long after the
 *  menu was dismissed. Mirrors graphSession's CONTEXT_TARGET_TTL_MS. */
const CONTEXT_TARGET_TTL_MS = 30_000;

/** Valid context-menu buckets (mirror of the webview's BUCKETS). */
const CONTEXT_BUCKETS = new Set(['staged', 'changes', 'untracked']);

interface CommitViewMessage {
  type?: string;
  paths?: string[];
  path?: string;
  status?: string;
  message?: string;
  amend?: boolean;
  /** contextTarget payload: the file under the webview's context menu
   *  (null = right-click landed on a non-file row; clears any target). */
  file?: { path?: unknown; status?: unknown; bucket?: unknown } | null;
}

/** The file the Commit view's context menu targeted, as shown in its row. */
export interface CommitContextTarget {
  path: string;
  status: string;
  bucket: 'staged' | 'changes' | 'untracked';
}

export class CommitViewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'gitBamboo.commit';

  private view: vscode.WebviewView | undefined;
  private refreshTimer: NodeJS.Timeout | undefined;
  private refreshQueued = false;
  private refreshing = false;
  private committing = false;
  private disposed = false;
  /** Parked context-menu target (right-clicked file row), consumed by the
   *  webview/context menu commands in extension.ts. */
  private contextTarget: CommitContextTarget | undefined;
  private contextTargetTimer: NodeJS.Timeout | undefined;
  /** Per-view subscriptions, torn down on re-resolution so listeners
   *  never double up (mirrors GraphWebviewProvider.teardownSession). */
  private viewDisposables: vscode.Disposable[] = [];

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {}

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
    this.clearContextTarget();
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
      case 'commit':
        void this.commit(
          typeof message.message === 'string' ? message.message : '',
          message.amend === true,
        );
        break;
      case 'contextTarget':
        // Right-click on a file row (fires before the native menu opens);
        // null (non-row click) clears the parked target. Webview data is a
        // trust boundary: only a well-formed repo-relative path is parked.
        if (message.file === null) {
          this.clearContextTarget();
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
      await this.view?.webview.postMessage({
        type: 'status',
        items: Array.isArray(items) ? items : [],
      });
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

  private async openResource(relativePath: string, status: string): Promise<void> {
    try {
      await openResource(this.root, relativePath, status);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: opening ${relativePath} failed — ${errorMessage(err)}`);
    }
  }

  /** Returns (and clears) the file targeted by the Commit view's context
   *  menu, if any. Called by the context-menu command handlers in
   *  extension.ts (Copy Path reads the target directly). */
  takeContextTarget(): CommitContextTarget | undefined {
    if (this.contextTargetTimer) clearTimeout(this.contextTargetTimer);
    this.contextTargetTimer = undefined;
    const target = this.contextTarget;
    this.contextTarget = undefined;
    return target;
  }

  /** Runs a context-menu action against the parked target. Returns false
   *  when no target is parked (safety net for programmatic invocation).
   *  Stage/unstage go through changePaths (path validation + refresh);
   *  open delegates to the shared openResource helper. */
  async runContextCommand(action: 'openChanges' | 'stageFile' | 'unstageFile'): Promise<boolean> {
    const target = this.takeContextTarget();
    if (!target) return false;
    if (action === 'openChanges') {
      await this.openResource(target.path, target.status);
    } else {
      await this.changePaths(action === 'stageFile' ? 'stage' : 'unstage', [target.path]);
    }
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
      return; // malformed payload: park nothing, keep any prior target
    }
    this.contextTarget = { path, status, bucket: bucket as CommitContextTarget['bucket'] };
    if (this.contextTargetTimer) clearTimeout(this.contextTargetTimer);
    this.contextTargetTimer = setTimeout(() => {
      this.contextTarget = undefined;
      this.contextTargetTimer = undefined;
    }, CONTEXT_TARGET_TTL_MS);
  }

  private clearContextTarget(): void {
    if (this.contextTargetTimer) clearTimeout(this.contextTargetTimer);
    this.contextTargetTimer = undefined;
    this.contextTarget = undefined;
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
