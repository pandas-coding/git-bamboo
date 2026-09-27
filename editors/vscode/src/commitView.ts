/**
 * Webview view provider for the Commit view (view id: gitBamboo.commit):
 * commit message + staged/changes/untracked file list + commit/amend in a
 * single JetBrains-style panel. Serves the built bundle (out/webview/
 * commit.html) and relays stage/unstage/commit/openResource messages to
 * the engine. Status pushes are debounced (50ms) and coalesce concurrent
 * refreshes, mirroring the old scmProvider's refresh pattern.
 */
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { openResource } from './headContent';
import type { RepoState, StatusItem } from './types';
import { errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

/** Debounce for status refreshes (the engine coalesces fs events at 50ms). */
const REFRESH_DEBOUNCE_MS = 50;

interface CommitViewMessage {
  type?: string;
  paths?: string[];
  path?: string;
  status?: string;
  message?: string;
  amend?: boolean;
}

export class CommitViewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'gitBamboo.commit';

  private view: vscode.WebviewView | undefined;
  private refreshTimer: NodeJS.Timeout | undefined;
  private refreshQueued = false;
  private refreshing = false;
  private disposed = false;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
    /** Shared per-repo state (epoch); exposed for future host-side use. */
    readonly state: RepoState,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, 'out', 'webview'),
        vscode.Uri.joinPath(this.context.extensionUri, 'webview'),
      ],
    };
    view.webview.html = buildWebviewHtml(this.context.extensionUri, view.webview, 'commit.html');
    this.disposables.push(
      view.webview.onDidReceiveMessage((message: CommitViewMessage) => this.onMessage(message)),
      view.onDidDispose(() => {
        this.view = undefined;
      }),
    );
    // Push the first status snapshot as soon as the view comes up (the
    // engine notifications only cover later changes).
    void this.doRefresh();
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
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
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
    if (paths.length === 0) return;
    try {
      await this.client.request(operation, { paths });
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

  /**
   * Commit semantics carried over from the old scmProvider: empty message
   * is rejected for plain commits; amend with an empty message runs the
   * engine's `--amend --no-edit` path.
   */
  private async commit(message: string, amend: boolean): Promise<void> {
    if (!message && !amend) {
      vscode.window.showWarningMessage('Git Bamboo: commit message is empty');
      return;
    }
    try {
      await this.client.request('commit', { message, amend });
      await this.view?.webview.postMessage({ type: 'committed' });
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: ${amend ? 'amend' : 'commit'} failed — ${errorMessage(err)}`,
      );
    }
  }
}
