/**
 * HEAD-revision diff infrastructure (extracted from the old scmProvider):
 * the ENGINE_SCHEME TextDocumentContentProvider that serves blob content
 * at HEAD for vscode.diff side-by-sides, plus openResource / repo-path
 * helpers reused by the Commit view.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';

export const ENGINE_SCHEME = 'git-bamboo';

export class HeadContentProvider implements vscode.Disposable {
  private readonly registration: vscode.Disposable;

  constructor(private readonly client: EngineClient, private readonly root: vscode.Uri) {
    // HEAD revision content for diffs: "git-bamboo:/abs/path" URIs.
    const contentProvider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: (uri) => this.readHeadRevision(uri),
    };
    this.registration = vscode.workspace.registerTextDocumentContentProvider(ENGINE_SCHEME, contentProvider);
  }

  dispose(): void {
    this.registration.dispose();
  }

  /** Engine `getBlob` RPC: HEAD-revision content for diff side-by-sides. */
  private async readHeadRevision(uri: vscode.Uri): Promise<string> {
    const relativePath = toRelativePath(this.root, uri);
    try {
      return await this.client.getBlob('HEAD', relativePath);
    } catch (err) {
      console.error(`[git-bamboo] getBlob HEAD:${relativePath} failed:`, err);
      return ''; // empty diff side, like the old git-CLI fallback
    }
  }
}

/**
 * Opens a working-tree change: untracked/added/ignored files have no HEAD
 * revision to diff against, so they open directly; everything else opens
 * the HEAD-vs-working-tree diff.
 */
export async function openResource(root: vscode.Uri, relativePath: string, status: string): Promise<void> {
  const uri = resolveRepoPath(root, relativePath);
  if (status === 'untracked' || status === 'added' || status === 'ignored') {
    await vscode.commands.executeCommand('vscode.open', uri);
    return;
  }
  const left = uri.with({ scheme: ENGINE_SCHEME, query: 'HEAD' });
  const title = `${path.basename(uri.fsPath)} (Working Tree — HEAD)`;
  await vscode.commands.executeCommand('vscode.diff', left, uri, title);
}

/** Resolve a repo-relative (slash-separated) path against the repo root. */
export function resolveRepoPath(root: vscode.Uri, relativePath: string): vscode.Uri {
  return vscode.Uri.joinPath(root, ...relativePath.split('/'));
}

/** Repo-relative path (forward slashes) for engine write requests. */
export function toRelativePath(root: vscode.Uri, uri: vscode.Uri): string {
  return path.relative(root.fsPath, uri.fsPath).replace(/\\/g, '/');
}
