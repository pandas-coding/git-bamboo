/**
 * Webview view provider for the commit graph (view id: gitWorkbench.graph).
 *
 * Serves the built webview bundle (out/webview/graph.html) with CSP and
 * webview asset URIs rewritten, relays viewportChanged messages from the
 * webview to engine getGraph requests (debounced), and forwards graphPage
 * results back. On graphInvalidated it tells the webview to drop its cache
 * and re-request while preserving its scroll anchor.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import type { GraphPage, RepoState } from './types';
import { errorMessage } from './util';

/** Debounce for viewport requests coming from scroll events. */
const VIEWPORT_DEBOUNCE_MS = 32;

export class GraphWebviewProvider implements vscode.WebviewViewProvider {
  static readonly viewId = 'gitWorkbench.graph';

  private view: vscode.WebviewView | undefined;
  private debounceTimer: NodeJS.Timeout | undefined;
  /** Coalesces invalidation bursts from the engine's fs watcher. */
  private invalidateTimer: NodeJS.Timeout | undefined;
  /** Sequence number so stale getGraph responses are dropped. */
  private requestSeq = 0;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly state: RepoState,
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
    view.webview.html = this.buildHtml(view.webview);
    view.webview.onDidReceiveMessage((message) => this.onMessage(message));
  }

  /** Called on engine graphInvalidated notifications (coalesced). */
  invalidate(epoch: number): void {
    this.state.epoch = epoch;
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    this.invalidateTimer = setTimeout(() => {
      this.invalidateTimer = undefined;
      void this.view?.webview.postMessage({ type: 'graphInvalidated', epoch });
    }, VIEWPORT_DEBOUNCE_MS);
  }

  private onMessage(message: { type?: string; offset?: number; limit?: number; anchorCommit?: string | null }): void {
    if (message?.type === 'viewportChanged') {
      const offset = Math.max(0, message.offset ?? 0);
      const limit = Math.min(2000, Math.max(1, message.limit ?? 50));
      const anchor = message.anchorCommit ?? null;
      if (this.debounceTimer) clearTimeout(this.debounceTimer);
      this.debounceTimer = setTimeout(() => void this.requestGraph(offset, limit, anchor), VIEWPORT_DEBOUNCE_MS);
    }
  }

  private async requestGraph(offset: number, limit: number, anchor: string | null): Promise<void> {
    const seq = ++this.requestSeq;
    try {
      const page = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset, limit, anchor_commit: anchor, epoch: this.state.epoch },
      });
      if (seq !== this.requestSeq) return; // superseded by a newer scroll position
      this.state.epoch = page.epoch;
      await this.view?.webview.postMessage({ type: 'graphPage', offset, page });
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: graph request failed — ${errorMessage(err)}`);
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
