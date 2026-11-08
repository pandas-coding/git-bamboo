/**
 * Shared webview HTML loader: reads the built bundle (out/webview/{name},
 * falling back to the unbuilt webview/ source for dev), injects the CSP
 * pinned to the target webview, binds script execution to this exact
 * document load via a per-webview nonce, and rewrites relative src/href
 * references to webview-asset URIs.
 */
import * as fs from 'node:fs';
import { randomUUID } from 'node:crypto';
import * as path from 'node:path';
import * as vscode from 'vscode';

/**
 * Builds the HTML document for a webview from the vite bundle.
 *
 * @param extensionUri Extension root (out/webview + webview live here).
 * @param webview Target webview — used for cspSource and asset URI rewrite.
 * @param htmlFileName e.g. "graph.html" or "commit.html" (entry file name).
 */
export function buildWebviewHtml(extensionUri: vscode.Uri, webview: vscode.Webview, htmlFileName: string): string {
  const builtDir = path.join(extensionUri.fsPath, 'out', 'webview');
  const sourceDir = path.join(extensionUri.fsPath, 'webview');
  const baseDir = fs.existsSync(path.join(builtDir, htmlFileName)) ? builtDir : sourceDir;
  let html = fs.readFileSync(path.join(baseDir, htmlFileName), 'utf8');
  // Per-webview nonce: fresh per buildWebviewHtml call, so two webviews
  // (and a reloaded one) never share a script-binding token.
  const nonce = randomUUID().replace(/-/g, '');

  const csp = [
    "default-src 'none'",
    `img-src ${webview.cspSource} data:`,
    `script-src ${webview.cspSource} 'nonce-${nonce}'`,
    `style-src ${webview.cspSource} 'unsafe-inline'`,
    `font-src ${webview.cspSource}`,
  ].join('; ');
  html = html.replace(
    /<meta\s+http-equiv=["']Content-Security-Policy["'][^>]*>/i,
    `<meta http-equiv="Content-Security-Policy" content="${csp}">`,
  );

  // Bind script execution to exactly this document load: every <script>
  // tag carries the CSP nonce (all entries are external module scripts —
  // assetsInlineLimit: 0 keeps them out of line; no inline scripts exist).
  // A same-source script injection (an asset path collision, a future edit
  // reintroducing inline script) would lack the nonce and not execute.
  html = html.replace(/<script\b/gi, `<script nonce="${nonce}"`);

  // Rewrite relative asset references ("./x.js" or "/assets/x.js") to
  // webview URIs.
  html = html.replace(/((?:src|href)=["'])(?:\.\/|\/)([^"']+)/g, (_match, prefix: string, rel: string) => {
    const assetUri = webview.asWebviewUri(vscode.Uri.file(path.join(baseDir, rel)));
    return `${prefix}${assetUri.toString()}`;
  });
  return html;
}
