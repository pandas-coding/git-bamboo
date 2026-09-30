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
import { createContextTargetSlot } from './contextTargetSlot';
import { isValidRepoPath, openResource } from './headContent';
import type { StatusItem } from './types';
import { errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

/** Debounce for status refreshes (the engine coalesces fs events at 50ms). */
const REFRESH_DEBOUNCE_MS = 50;

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

/** Bucketing rule mirrored from the webview (commit.js bucketOf). */
function bucketOf(item: StatusItem): 'staged' | 'changes' | 'untracked' {
  if (item.staged) return 'staged';
  if (item.status === 'untracked') return 'untracked';
  return 'changes';
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
   *  openResource helper. */
  async runContextCommand(action: 'openChanges' | 'stageFile' | 'unstageFile'): Promise<boolean> {
    const target = this.takeContextTarget();
    if (!target) return false;
    // Bucket cross-check, mirroring the menus' when clauses: stageFile only
    //  for unstaged buckets, unstageFile only for staged.
    if (action === 'stageFile' && target.bucket === 'staged') return false;
    if (action === 'unstageFile' && target.bucket !== 'staged') return false;
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
