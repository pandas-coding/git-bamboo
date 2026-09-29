/**
 * Sidebar view host for the commit graph (view id: gitBamboo.graph) in the
 * gitBamboo activity bar container. The webview logic itself lives in
 * GraphSession (shared with the editor-area panel host): this class only
 * owns the view lifecycle, webview options, and HTML generation.
 */
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { GraphSession } from './graphSession';
import type { RepoState } from './types';
import { buildWebviewHtml } from './webviewHtml';

export class GraphWebviewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'gitBamboo.graph';

  private session: GraphSession | undefined;
  private viewDisposable: vscode.Disposable | undefined;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly state: RepoState,
  ) {}

  resolveWebviewView(view: vscode.WebviewView): void {
    // Re-resolution (view disposed then re-opened): tear down any prior
    // session first so engine subscriptions never double up.
    this.teardownSession();
    view.webview.options = {
      enableScripts: true,
      localResourceRoots: [
        vscode.Uri.joinPath(this.context.extensionUri, 'out', 'webview'),
        vscode.Uri.joinPath(this.context.extensionUri, 'webview'),
      ],
    };
    view.webview.html = buildWebviewHtml(this.context.extensionUri, view.webview, 'graph.html');
    // The session owns engine subscriptions; it is torn down when the
    // sidebar view is disposed so listeners never leak.
    this.session = new GraphSession(this.client, this.state, view.webview, 'sidebar', this.context, () => {
      this.session = undefined;
    });
    this.viewDisposable?.dispose();
    this.viewDisposable = view.onDidDispose(() => this.teardownSession());
  }

  dispose(): void {
    this.teardownSession();
    this.viewDisposable?.dispose();
    this.viewDisposable = undefined;
  }

  private teardownSession(): void {
    this.session?.dispose();
    this.session = undefined;
  }
}
