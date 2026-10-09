/**
 * Revision-content diff infrastructure (extracted from the old scmProvider):
 * the ENGINE_SCHEME TextDocumentContentProvider that serves blob content at
 * any validated revision (HEAD, a full commit sha, or the synthetic empty
 * revision) for vscode.diff side-by-sides, plus openResource, commitRevisionUri
 * and repo-path validation helpers.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { COMMIT_ID_RE } from './util';

export const ENGINE_SCHEME = 'git-bamboo';

/** Sentinel revision for the empty left side of a diff (added files, root
 *  commits): the provider resolves '' for it without an engine call. Distinct
 *  from any real git revision, so it can never collide with a sha or 'HEAD'. */
export const EMPTY_REVISION = 'empty';

export class HeadContentProvider implements vscode.Disposable {
  private readonly registration: vscode.Disposable;

  constructor(private readonly client: EngineClient, private readonly root: vscode.Uri) {
    // Revision content for diffs: "git-bamboo:/abs/path?ref" URIs.
    const contentProvider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: (uri) => this.readRevisionContent(uri),
    };
    this.registration = vscode.workspace.registerTextDocumentContentProvider(ENGINE_SCHEME, contentProvider);
  }

  dispose(): void {
    this.registration.dispose();
  }

  /** Engine `getBlob` RPC: blob content at the revision named by the URI's
   *  query, for diff side-by-sides. */
  private async readRevisionContent(uri: vscode.Uri): Promise<string> {
    const relativePath = toRelativePath(this.root, uri);
    // The provider is a trust boundary in both directions: like the
    // build side (commitRevisionUri), inbound paths that could escape the
    // repository are refused before reaching the engine.
    if (!isValidRepoPath(relativePath)) {
      console.error(`[git-bamboo] refusing untrusted path in URI: ${uri.toString(true)}`);
      return '';
    }
    const revision = parseRevisionQuery(uri.query);
    // Both the synthetic empty revision and untrusted queries resolve to the
    // empty diff side — the former by design, the latter without an engine call.
    if (revision === null || revision === '') return '';
    try {
      return await this.client.getBlob(revision, relativePath);
    } catch (err) {
      console.error(`[git-bamboo] getBlob ${revision}:${relativePath} failed:`, err);
      return ''; // empty diff side, like the old git-CLI fallback
    }
  }
}

/**
 * Validate the revision in a git-bamboo: URI's query. The provider is a
 * trust boundary — a user can open a crafted git-bamboo: URI, so anything
 * that isn't a known token or a full commit sha is rejected rather than
 * forwarded to the engine.
 *
 * Returns the revision string to fetch: 'HEAD', '' for EMPTY_REVISION
 * (caller serves empty content without an engine call), or a full commit
 * sha. Returns null when the query is untrusted — the caller resolves ''
 * rather than calling the engine.
 */
function parseRevisionQuery(query: string): string | null {
  if (query === 'HEAD') return 'HEAD';
  if (query === EMPTY_REVISION) return '';
  if (COMMIT_ID_RE.test(query)) return query;
  console.error(`[git-bamboo] refusing untrusted revision in URI query: ${query}`);
  return null;
}

/**
 * Build a git-bamboo: URI serving `relativePath` at `ref` (a full commit sha,
 * 'HEAD', or EMPTY_REVISION) for diff side-by-sides. The path is validated
 * the same way openResource does, because callers reach this with data
 * originating from webview postMessages.
 */
export function commitRevisionUri(root: vscode.Uri, relativePath: string, ref: string): vscode.Uri {
  const uri = resolveRepoPath(root, relativePath);
  return uri.with({ scheme: ENGINE_SCHEME, query: ref });
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

/** True for plain repo-relative slash paths — rejects anything that could
 *  escape the repository (absolute paths, drive letters, backslashes,
 *  `.`/`..` segments). Paths arriving from the webview via postMessage are
 *  a trust boundary, so they are validated before any file access or
 *  engine write RPC. */
export function isValidRepoPath(p: string): boolean {
  if (typeof p !== 'string' || p.length === 0 || p.includes('\\')) return false;
  if (path.isAbsolute(p) || /^[a-zA-Z]:/.test(p)) return false;
  for (const segment of p.split('/')) {
    if (segment === '' || segment === '.' || segment === '..') return false;
  }
  return true;
}

/** Resolve a repo-relative (slash-separated) path against the repo root;
 *  throws on paths that could escape the repository. */
function resolveRepoPath(root: vscode.Uri, relativePath: string): vscode.Uri {
  if (!isValidRepoPath(relativePath)) {
    throw new Error(`path escapes the repository: ${relativePath}`);
  }
  return vscode.Uri.joinPath(root, ...relativePath.split('/'));
}

/** Repo-relative path (forward slashes) for engine write requests. */
function toRelativePath(root: vscode.Uri, uri: vscode.Uri): string {
  return path.relative(root.fsPath, uri.fsPath).replace(/\\/g, '/');
}
