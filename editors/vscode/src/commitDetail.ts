/**
 * Commit detail panel: editor-area webview showing one commit's full
 * metadata — complete multi-line message, author/committer identities and
 * times, and the changed-file list with per-file +/- counts. Each file row
 * has an "Open Changes" action that opens `vscode.diff` between the commit
 * and its first parent (empty left side for root commits / added files)
 * via git-bamboo: content-provider URIs.
 *
 * Single reusable panel: consecutive opens re-target the existing panel
 * (title + data push) instead of stacking panels. Data is pushed only
 * after the webview's 'ready' handshake (postMessages sent earlier are
 * silently dropped) and only the newest fetch wins (sequence guard), and
 * the fetched CommitDetail stays host-side — the webview only ever sends
 * back paths, while every revision used in a diff URI is derived from the
 * host-held detail, never from webview fields.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { commitRevisionUri, EMPTY_REVISION, isValidRepoPath } from './headContent';
import type { CommitDetail } from './types';
import { COMMIT_ID_RE, errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

const PANEL_TYPE = 'gitBamboo.commitDetail';

/** Inbound webview messages (fields validated field-by-field: the webview
 *  is a trust boundary, so wrong-typed payloads are ignored). */
interface CommitDetailMessage {
  type?: unknown;
  path?: unknown;
  oldPath?: unknown;
}

/** The currently open commit detail panel (singleton). */
let activePanel: vscode.WebviewPanel | undefined;
/** Engine client / repo root of the panel's creator — the 'openChanges'
 *  handler needs them for as long as the panel lives. */
let panelClient: EngineClient | undefined;
let panelRoot: vscode.Uri | undefined;
/** Latest sha requested for the panel; pushed once the webview is ready. */
let pendingSha: string | undefined;
/** Set once the webview sent its 'ready' handshake. */
let webviewReady = false;
/** Host-held detail of the last pushed commit — the trust anchor for
 *  'openChanges' (diff revisions and paths come from here, not the webview). */
let currentDetail: CommitDetail | undefined;
/** Monotonic fetch sequence: stale getCommitDetail responses are dropped
 *  when the panel is re-targeted mid-flight. */
let fetchSeq = 0;

/**
 * Opens (or re-targets) the commit detail panel for `sha`.
 *
 * @param context Extension context (webview resource roots).
 * @param client Engine client — fetches `getCommitDetail`.
 * @param root Repository root Uri (resolves diff-side URIs).
 * @param sha Full commit id; anything not matching the commit-id regex is
 *        silently ignored (webview-supplied ids are untrusted input).
 */
export function openCommitDetailPanel(
  context: vscode.ExtensionContext,
  client: EngineClient,
  root: vscode.Uri,
  sha: string,
): void {
  if (typeof sha !== 'string' || !COMMIT_ID_RE.test(sha)) return;

  if (activePanel) {
    // Re-target the existing panel where the user put it (no forced
    // ViewColumn) — update the title and push fresh data.
    activePanel.reveal();
    activePanel.title = sha.slice(0, 8);
    pendingSha = sha;
    if (webviewReady) void fetchAndPush(sha);
    return;
  }

  const panel = vscode.window.createWebviewPanel(PANEL_TYPE, sha.slice(0, 8), vscode.ViewColumn.Beside, {
    enableScripts: true,
    retainContextWhenHidden: true,
    localResourceRoots: [
      vscode.Uri.joinPath(context.extensionUri, 'out', 'webview'),
      vscode.Uri.joinPath(context.extensionUri, 'webview'),
    ],
  });
  activePanel = panel;
  panelClient = client;
  panelRoot = root;
  pendingSha = sha;
  webviewReady = false;
  currentDetail = undefined;

  panel.webview.html = buildWebviewHtml(context.extensionUri, panel.webview, 'commitDetail.html');
  panel.webview.onDidReceiveMessage((message: CommitDetailMessage) => onMessage(message));
  panel.onDidDispose(() => {
    if (activePanel !== panel) return;
    activePanel = undefined;
    panelClient = undefined;
    panelRoot = undefined;
    pendingSha = undefined;
    currentDetail = undefined;
    webviewReady = false;
    fetchSeq++; // any in-flight fetch for this panel is dead
  });
}

/** Field-by-field dispatch of inbound webview messages (trust boundary). */
function onMessage(message: CommitDetailMessage): void {
  if (!message || typeof message !== 'object') return;
  if (message.type === 'ready') {
    // The webview's listener is up by now — the first data push (and any
    // re-push after a webview reload) is safe from here on.
    webviewReady = true;
    if (pendingSha) void fetchAndPush(pendingSha);
    return;
  }
  if (message.type === 'openChanges') {
    if (typeof message.path !== 'string' || !isValidRepoPath(message.path)) return;
    if (message.oldPath !== undefined && message.oldPath !== null) {
      if (typeof message.oldPath !== 'string' || !isValidRepoPath(message.oldPath)) return;
    }
    void openChanges(message.path);
  }
}

/** Fetches the commit detail and pushes it to the webview; superseded
 *  fetches (a newer open re-targeted the panel mid-flight) are dropped. */
async function fetchAndPush(sha: string): Promise<void> {
  const client = panelClient;
  const panel = activePanel;
  if (!client || !panel) return;
  const seq = ++fetchSeq;
  const detail = await client.getCommitDetail(sha);
  if (seq !== fetchSeq || activePanel !== panel) return; // superseded
  if (!detail || detail.id !== sha) {
    // null = engine error / malformed response (surfaced to the user);
    // an id mismatch means a stale or wrong response — dropped, the
    // sequence guard above normally catches re-target races first.
    if (detail) console.error(`[git-bamboo] getCommitDetail id mismatch: asked for ${sha}, got ${detail.id}`);
    else vscode.window.showErrorMessage(`Git Bamboo: commit details unavailable — ${sha.slice(0, 8)}`);
    currentDetail = undefined;
    if (webviewReady) void panel.webview.postMessage({ type: 'error' });
    return;
  }
  currentDetail = detail;
  if (webviewReady) void panel.webview.postMessage({ type: 'commitDetail', detail });
}

/** Opens the parent↔commit diff for a file row. Everything except the row
 *  path (validated above) is derived from the host-held detail. */
async function openChanges(filePath: string): Promise<void> {
  const root = panelRoot;
  const detail = currentDetail;
  if (!root || !detail) return;
  const entry = detail.files.find((file) => file.path === filePath);
  if (!entry) return;
  // Engine-sourced, but validated anyway before they reach URI building.
  if (!isValidRepoPath(entry.path)) return;
  if (entry.old_path !== null && !isValidRepoPath(entry.old_path)) return;

  const shortSha = detail.id.slice(0, 8);
  let leftRef: string;
  if (entry.status === 'A' || detail.parent_ids.length === 0) {
    leftRef = EMPTY_REVISION; // added file or root commit: no parent side
  } else {
    const parent = detail.parent_ids[0]; // diff baseline
    if (typeof parent !== 'string' || !COMMIT_ID_RE.test(parent)) return;
    leftRef = parent;
  }
  // The file lived at old_path in the parent (rename); the new path at
  // this commit.
  const leftPath = entry.old_path ?? entry.path;

  try {
    const leftUri = commitRevisionUri(root, leftPath, leftRef);
    const rightUri = commitRevisionUri(root, entry.path, detail.id);
    const base = path.basename(entry.path);
    const title =
      leftRef === EMPTY_REVISION
        ? `${base} (${shortSha})`
        : `${base} (${shortSha}^) ↔ ${base} (${shortSha})`;
    await vscode.commands.executeCommand('vscode.diff', leftUri, rightUri, title);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: open changes failed — ${errorMessage(err)}`);
  }
}
