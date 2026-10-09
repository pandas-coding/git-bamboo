/**
 * Commit detail panel: editor-area webview showing one commit's full
 * metadata — complete multi-line message, author/committer identities and
 * times, and the changed-file list with per-file +/- counts. Each file row
 * has an "Open Changes" action that opens `vscode.diff` between the commit
 * and its first parent (empty left side for root commits / added files)
 * via git-bamboo: content-provider URIs.
 *
 * Single reusable panel: consecutive opens re-target the existing panel
 * (title + data push) instead of stacking panels, refreshing the engine
 * client/repo root so a restarted engine keeps the panel working. Data is
 * pushed only after the webview's 'ready' handshake (postMessages sent
 * earlier are silently dropped) and only the newest fetch wins (sequence
 * guard), and the fetched CommitDetail stays host-side — the webview only
 * ever sends back paths, while every revision used in a diff URI is
 * derived from the host-held detail, never from webview fields.
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
let activePanel: CommitDetailPanel | undefined;

/**
 * Opens (or re-targets) the commit detail panel for `sha`.
 *
 * @param context Extension context (webview resource roots).
 * @param client Engine client — fetches `getCommitDetail`. Refreshed on
 *        every re-target so a restarted engine keeps the panel working.
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
    // ViewColumn) — update the title, clear the stale content, and push
    // fresh data through the caller's (possibly newer) client/root.
    activePanel.retarget(client, root, sha);
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
  activePanel = new CommitDetailPanel(context, panel, client, root, sha);
}

/**
 * State of one commit detail webview panel. Owns the panel lifecycle:
 * disposal clears the whole per-panel state in one reference drop, and
 * the webview message subscription is disposed with the panel.
 */
class CommitDetailPanel {
  /** Latest sha requested for the panel; pushed once the webview is ready. */
  private pendingSha: string | undefined;
  /** Set once the webview sent its 'ready' handshake. */
  private webviewReady = false;
  /** Host-held detail of the last pushed commit — the trust anchor for
   *  'openChanges' (diff revisions and paths come from here, not the
   *  webview). Cleared while a re-target fetch is in flight. */
  private currentDetail: CommitDetail | undefined;
  /** Monotonic fetch sequence: stale getCommitDetail responses are dropped
   *  when the panel is re-targeted mid-flight. */
  private fetchSeq = 0;
  private readonly messageSub: vscode.Disposable;

  constructor(
    context: vscode.ExtensionContext,
    private readonly panel: vscode.WebviewPanel,
    private client: EngineClient,
    private root: vscode.Uri,
    sha: string,
  ) {
    this.pendingSha = sha;
    panel.webview.html = buildWebviewHtml(context.extensionUri, panel.webview, 'commitDetail.html');
    this.messageSub = panel.webview.onDidReceiveMessage((message: CommitDetailMessage) =>
      this.onMessage(message),
    );
    panel.onDidDispose(() => {
      if (activePanel !== this) return;
      activePanel = undefined;
      this.fetchSeq++; // any in-flight fetch for this panel is dead
      this.messageSub.dispose();
    });
  }

  /** Re-targets the panel at a new commit, refreshing client/root: the
   *  panel may outlive the engine client it was created with. */
  retarget(client: EngineClient, root: vscode.Uri, sha: string): void {
    this.client = client;
    this.root = root;
    this.currentDetail = undefined; // stale rows must not stay clickable
    this.pendingSha = sha;
    this.panel.reveal();
    this.panel.title = sha.slice(0, 8);
    if (this.webviewReady) {
      void this.panel.webview.postMessage({ type: 'loading' });
      void this.fetchAndPush(sha);
    }
  }

  /** Field-by-field dispatch of inbound webview messages (trust boundary). */
  private onMessage(message: CommitDetailMessage): void {
    if (!message || typeof message !== 'object') return;
    if (message.type === 'ready') {
      // The webview's listener is up by now — the first data push (and any
      // re-push after a webview reload) is safe from here on.
      this.webviewReady = true;
      if (this.pendingSha) void this.fetchAndPush(this.pendingSha);
      return;
    }
    if (message.type === 'openChanges') {
      if (typeof message.path !== 'string' || !isValidRepoPath(message.path)) return;
      if (message.oldPath !== undefined && message.oldPath !== null) {
        if (typeof message.oldPath !== 'string' || !isValidRepoPath(message.oldPath)) return;
      }
      void this.openChanges(message.path);
    }
  }

  /** Fetches the commit detail and pushes it to the webview; superseded
   *  fetches (a newer open re-targeted the panel mid-flight) are dropped. */
  private async fetchAndPush(sha: string): Promise<void> {
    const seq = ++this.fetchSeq;
    const detail = await this.client.getCommitDetail(sha);
    if (seq !== this.fetchSeq) return; // superseded by a newer request
    if (!detail) {
      // null = engine error / malformed response (surfaced to the user)
      vscode.window.showErrorMessage(`Git Bamboo: commit details unavailable — ${sha.slice(0, 8)}`);
      this.currentDetail = undefined;
      if (this.webviewReady) void this.panel.webview.postMessage({ type: 'error' });
      return;
    }
    this.currentDetail = detail;
    if (this.webviewReady) void this.panel.webview.postMessage({ type: 'commitDetail', detail });
  }

  /** Opens the parent↔commit diff for a file row. Everything except the
   *  row path (validated at the message boundary above) is derived from
   *  the host-held detail; URI building re-validates paths internally
   *  (resolveRepoPath), throwing into the catch below on failure. */
  private async openChanges(filePath: string): Promise<void> {
    const detail = this.currentDetail;
    if (!detail) return;
    const entry = detail.files.find((file) => file.path === filePath);
    if (!entry) return;

    const shortSha = detail.id.slice(0, 8);
    let leftRef: string;
    if (entry.status === 'A' || detail.parent_ids.length === 0) {
      leftRef = EMPTY_REVISION; // added file or root commit: no parent side
    } else {
      const parent = detail.parent_ids[0]; // diff baseline
      if (!COMMIT_ID_RE.test(parent)) return;
      leftRef = parent;
    }
    // The file lived at old_path in the parent (rename); the new path at
    // this commit.
    const leftPath = entry.old_path ?? entry.path;

    try {
      const leftUri = commitRevisionUri(this.root, leftPath, leftRef);
      const rightUri = commitRevisionUri(this.root, entry.path, detail.id);
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
}
