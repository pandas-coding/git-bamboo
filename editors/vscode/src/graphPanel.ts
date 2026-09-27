/**
 * Editor-area full-width host for the commit graph: a WebviewPanel opened
 * via the `gitBamboo.openGraph` command (JetBrains tool window / GitLens
 * style). Same webview bundle as the sidebar view; a GraphSession drives
 * it. Single instance — re-running openGraph reveals the existing panel —
 * and survives window reloads via a panel serializer.
 */
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { GraphSession } from './graphSession';
import type { RepoState } from './types';
import { buildWebviewHtml } from './webviewHtml';

export const GRAPH_PANEL_TYPE = 'gitBamboo.graphPanel';

/** The currently open editor-area graph panel (singleton). */
let activePanel: vscode.WebviewPanel | undefined;

/** Opens (or reveals) the editor-area commit graph panel. */
export function openGraphPanel(
  context: vscode.ExtensionContext,
  client: EngineClient,
  state: RepoState,
): vscode.WebviewPanel {
  if (activePanel) {
    activePanel.reveal(vscode.ViewColumn.Beside);
    return activePanel;
  }

  const panel = vscode.window.createWebviewPanel(
    GRAPH_PANEL_TYPE,
    'Commit Graph',
    vscode.ViewColumn.Beside,
    {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [
        vscode.Uri.joinPath(context.extensionUri, 'out', 'webview'),
        vscode.Uri.joinPath(context.extensionUri, 'webview'),
      ],
    },
  );
  attachSession(context, client, state, panel);
  return panel;
}

/** Wires HTML + a GraphSession into a fresh or restored panel. */
function attachSession(
  context: vscode.ExtensionContext,
  client: EngineClient,
  state: RepoState,
  panel: vscode.WebviewPanel,
): void {
  panel.webview.html = buildWebviewHtml(context.extensionUri, panel.webview, 'graph.html');
  activePanel = panel;
  const session = new GraphSession(client, state, panel.webview, () => {
    if (activePanel === panel) activePanel = undefined;
  });
  panel.onDidDispose(() => session.dispose());
}

/**
 * Restores the panel across window reloads: graph needs no state replay —
 * the webview re-requests its viewport once the session attaches.
 */
export function registerGraphPanelSerializer(
  context: vscode.ExtensionContext,
  client: EngineClient,
  state: RepoState,
): void {
  context.subscriptions.push(
    vscode.window.registerWebviewPanelSerializer(GRAPH_PANEL_TYPE, {
      deserializeWebviewPanel: async (panel: vscode.WebviewPanel) => {
        attachSession(context, client, state, panel);
      },
    }),
  );
}
