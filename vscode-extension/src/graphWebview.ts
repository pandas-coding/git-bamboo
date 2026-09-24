/**
 * Webview view provider for the commit graph (view id: gitWorkbench.graph).
 *
 * Serves the built webview bundle (out/webview/graph.html) with CSP and
 * webview asset URIs rewritten, relays viewportChanged messages from the
 * webview to engine getGraph requests (debounced), and forwards graphPage
 * results back. On graphInvalidated it refreshes the client epoch FIRST
 * (via an unvalidated epoch-0 probe), then tells the webview to drop its
 * cache and re-request while preserving its scroll anchor. Stale getGraph
 * responses are dropped via a request sequence number, and EPOCH_MISMATCH
 * errors are retried once with the fresh epoch.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient, RpcError } from './engineClient';
import type { GraphPage, RepoState } from './types';
import { errorMessage } from './util';

/** JSON-RPC error code the engine returns when the client's epoch is stale. */
const EPOCH_MISMATCH = -32004;

/** Debounce for viewport requests coming from scroll events. */
const VIEWPORT_DEBOUNCE_MS = 16;

export class GraphWebviewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'gitWorkbench.graph';

  private view: vscode.WebviewView | undefined;
  private debounceTimer: NodeJS.Timeout | undefined;
  /** Coalesces invalidation bursts from the engine's fs watcher. */
  private invalidateTimer: NodeJS.Timeout | undefined;
  /** Sequence number so stale getGraph responses are dropped. */
  private requestSeq = 0;
  private disposed = false;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly state: RepoState,
  ) {
    // Engine-driven invalidation and ref updates are owned here so that
    // dispose() can unhook every engine listener again.
    this.disposables.push(
      client.onNotification('graphInvalidated', (params) => {
        const epoch = numberField(params, 'epoch', this.state.epoch);
        this.invalidate(epoch);
      }),
      client.onNotification('refsChanged', () => void this.sendRefs()),
    );
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, 'out', 'webview'),
        vscode.Uri.joinPath(this.context.extensionUri, 'webview'),
      ],
    };
    view.webview.html = this.buildHtml(view.webview);
    this.disposables.push(
      view.webview.onDidReceiveMessage((message) => this.onMessage(message)),
      view.onDidDispose(() => {
        this.view = undefined;
      }),
    );
    void this.sendRefs();
  }

  /** Called on engine graphInvalidated notifications (coalesced). */
  invalidate(epoch: number): void {
    this.state.epoch = epoch;
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    this.invalidateTimer = setTimeout(() => {
      this.invalidateTimer = undefined;
      void this.refreshAfterInvalidation(epoch);
    }, VIEWPORT_DEBOUNCE_MS);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.debounceTimer) clearTimeout(this.debounceTimer);
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    // Unsubscribe engine notification listeners and webview subscriptions.
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
    this.view = undefined;
  }

  /**
   * Refresh the epoch BEFORE re-requesting: after an invalidation the
   * client's epoch is stale by definition, so re-requesting with it would
   * guarantee an EPOCH_MISMATCH loop. The probe sends epoch 0, which the
   * engine treats as "no validation", and adopts the fresh epoch from the
   * response before the webview re-requests its viewport.
   */
  private async refreshAfterInvalidation(notifiedEpoch: number): Promise<void> {
    try {
      const probe = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset: 0, limit: 1, anchor_commit: null, epoch: 0 },
      });
      this.state.epoch = probe.epoch;
    } catch (err) {
      console.error('[git-workbench] epoch probe after invalidation failed:', err);
      this.state.epoch = notifiedEpoch;
    }
    await this.view?.webview.postMessage({ type: 'graphInvalidated', epoch: this.state.epoch });
  }

  private onMessage(message: {
    type?: string;
    offset?: number;
    limit?: number;
    anchorCommit?: string | null;
    id?: string;
  }): void {
    if (message?.type === 'viewportChanged') {
      const offset = Math.max(0, message.offset ?? 0);
      const limit = Math.min(2000, Math.max(1, message.limit ?? 50));
      const anchor = message.anchorCommit ?? null;
      if (this.debounceTimer) clearTimeout(this.debounceTimer);
      this.debounceTimer = setTimeout(() => void this.requestGraph(offset, limit, anchor), VIEWPORT_DEBOUNCE_MS);
    } else if (message?.type === 'checkoutCommit' && typeof message.id === 'string') {
      // The engine has no checkout-commit RPC yet (checked against
      // crates/protocol and the engine dispatch); the real detach flow
      // lands in T2.
      vscode.window.showInformationMessage(
        `Git Workbench: detached checkout of ${message.id.slice(0, 8)} coming in T2`,
      );
    }
  }

  private async requestGraph(offset: number, limit: number, anchor: string | null, retried = false): Promise<void> {
    if (this.disposed) return;
    const seq = ++this.requestSeq;
    try {
      const page = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset, limit, anchor_commit: anchor, epoch: this.state.epoch },
      });
      if (seq !== this.requestSeq || this.disposed) return; // superseded by a newer scroll position
      this.state.epoch = page.epoch;
      await this.view?.webview.postMessage({ type: 'graphPage', offset, page });
    } catch (err) {
      if (!retried && !this.disposed && err instanceof RpcError && err.code === EPOCH_MISMATCH) {
        // Our epoch is stale: retry ONCE with the fresh epoch from the
        // error (or epoch 0 = "no validation" if unavailable), then adopt
        // the epoch from the successful response.
        this.state.epoch = epochFromError(err);
        await this.requestGraph(offset, limit, anchor, true);
        return;
      }
      if (seq !== this.requestSeq || this.disposed) return;
      vscode.window.showErrorMessage(`Git Workbench: graph request failed — ${errorMessage(err)}`);
    }
  }

  /** Pushes refs to the webview so it can render branch-tip tags. */
  private async sendRefs(): Promise<void> {
    if (this.disposed) return;
    try {
      const refs = await this.client.getRefs();
      await this.view?.webview.postMessage({ type: 'refs', refs });
    } catch (err) {
      console.error('[git-workbench] refs fetch for graph failed:', err);
    }
  }

  /**
   * Reads the built HTML (out/webview/graph.html, falling back to the
   * unbuilt webview/ source for dev), injects the CSP, and rewrites
   * relative src/href references to webview-asset URIs.
   */
  private buildHtml(webview: vscode.Webview): string {
    const builtDir = path.join(this.context.extensionUri.fsPath, 'out', 'webview');
    const sourceDir = path.join(this.context.extensionUri.fsPath, 'webview');
    const baseDir = fs.existsSync(path.join(builtDir, 'graph.html')) ? builtDir : sourceDir;
    let html = fs.readFileSync(path.join(baseDir, 'graph.html'), 'utf8');

    const csp = [
      "default-src 'none'",
      `img-src ${webview.cspSource} data:`,
      `script-src ${webview.cspSource}`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `font-src ${webview.cspSource}`,
    ].join('; ');
    html = html.replace(
      /<meta\s+http-equiv=["']Content-Security-Policy["'][^>]*>/i,
      `<meta http-equiv="Content-Security-Policy" content="${csp}">`,
    );

    // Rewrite relative asset references ("./x.js" or "/assets/x.js") to
    // webview URIs.
    html = html.replace(/((?:src|href)=["'])(?:\.\/|\/)([^"']+)/g, (_match, prefix: string, rel: string) => {
      const assetUri = webview.asWebviewUri(vscode.Uri.file(path.join(baseDir, rel)));
      return `${prefix}${assetUri.toString()}`;
    });
    return html;
  }
}

/** Extracts the engine's current epoch from an EPOCH_MISMATCH error. */
function epochFromError(err: RpcError): number {
  const data = err.data as Record<string, unknown> | undefined;
  if (data) {
    for (const key of ['expected_epoch', 'expected', 'epoch']) {
      const value = data[key];
      if (typeof value === 'number') return value;
    }
  }
  const match = /expected (\d+)/.exec(err.message);
  return match ? Number(match[1]) : 0; // 0 = "no validation" probe
}

function numberField(params: Record<string, unknown>, key: string, fallback: number): number {
  const value = params[key];
  return typeof value === 'number' ? value : fallback;
}
