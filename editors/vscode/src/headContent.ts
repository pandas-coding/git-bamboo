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

/** Conflict-stage revisions the provider serves from the unmerged index:
 *  base / ours / theirs of a conflicted path (engine getBlob ":N"). The
 *  merge editor's three-way inputs are built from these. */
export const STAGE_BASE = ':1';
export const STAGE_OURS = ':2';
export const STAGE_THEIRS = ':3';

/** Stage-0 (normal) index revision, served by the provider via engine
 *  getBlob ":0": the left side of a worktree diff and the right side of
 *  a staged diff (hunk staging). An absent index entry resolves to the
 *  empty diff side, like any missing path. */
export const INDEX_REVISION = ':0';

/** Which bucket of the Commit view a file row sits in — decides the diff
 *  sides openResource picks (staged → index vs HEAD, changes → worktree
 *  vs index). */
export type CommitBucket = 'conflicts' | 'staged' | 'changes' | 'untracked';

export class HeadContentProvider implements vscode.Disposable {
  private readonly registration: vscode.Disposable;
  /** Fired for every OPEN index-side (`:0`) document when the engine
   *  reports an index change: VS Code re-requests content for those URIs,
   * so open staged diffs reflect hunk staging/unstaging without a reopen
   * (the worktree side refreshes itself via the file watcher). */
  private readonly indexChanged = new vscode.EventEmitter<vscode.Uri>();

  constructor(private readonly client: EngineClient, private readonly root: vscode.Uri) {
    // Revision content for diffs: "git-bamboo:/abs/path?ref" URIs.
    const contentProvider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: (uri) => this.readRevisionContent(uri),
      onDidChange: this.indexChanged.event,
    };
    this.registration = vscode.workspace.registerTextDocumentContentProvider(ENGINE_SCHEME, contentProvider);
  }

  dispose(): void {
    this.indexChanged.dispose();
    this.registration.dispose();
  }

  /** Fire change events for the `:0` (index) sides currently open in
   *  editors — call on the engine's `indexChanged` notification. */
  notifyIndexChanged(): void {
    for (const doc of vscode.workspace.textDocuments) {
      if (doc.uri.scheme === ENGINE_SCHEME && doc.uri.query === INDEX_REVISION) {
        this.indexChanged.fire(doc.uri);
      }
    }
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
  // Conflict stages (base/ours/theirs of the unmerged index entries) and
  // the stage-0 index revision — tokens, not rev-parse inputs, same trust
  // posture as 'HEAD'.
  if (
    query === STAGE_BASE ||
    query === STAGE_OURS ||
    query === STAGE_THEIRS ||
    query === INDEX_REVISION
  ) {
    return query;
  }
  if (COMMIT_ID_RE.test(query)) return query;
  console.error(`[git-bamboo] refusing untrusted revision in URI query: ${query}`);
  return null;
}

/**
 * Build a git-bamboo: URI serving `relativePath` at `ref` (a full commit sha,
 * 'HEAD', EMPTY_REVISION, or a conflict stage :1/:2/:3) for diff
 * side-by-sides. The path is validated the same way openResource does,
 * because callers reach this with data originating from webview postMessages.
 */
export function commitRevisionUri(root: vscode.Uri, relativePath: string, ref: string): vscode.Uri {
  const uri = resolveRepoPath(root, relativePath);
  return uri.with({ scheme: ENGINE_SCHEME, query: ref });
}

/** git-bamboo: URI serving one conflict stage (base/ours/theirs) of a
 *  conflicted file — the merge editor's input sides. */
export function stageUri(root: vscode.Uri, relativePath: string, stage: typeof STAGE_BASE | typeof STAGE_OURS | typeof STAGE_THEIRS): vscode.Uri {
  return commitRevisionUri(root, relativePath, stage);
}

/** git-bamboo: URI serving the file's stage-0 INDEX entry — the left side
 *  of a worktree diff, the right side of a staged diff. */
export function indexUri(root: vscode.Uri, relativePath: string): vscode.Uri {
  return commitRevisionUri(root, relativePath, INDEX_REVISION);
}

/**
 * Opens a working-tree change as a diff. The bucket decides the sides
 * (VS Code SCM semantics, which hunk staging relies on):
 *   - `staged`  → index (`:0`) vs HEAD — [Unstage hunk] targets
 *   - `changes` → worktree vs index (`:0`) — [Stage hunk] targets; the
 *     worktree-vs-index base (not HEAD) is what makes incremental hunk
 *     staging correct server-side
 *   - `conflicts`/`untracked` and untracked/added/ignored statuses keep
 *     the legacy behavior (HEAD-vs-worktree diff / plain open) — hunk
 *     staging never applies to them.
 * When `bucket` is absent (legacy callers), the old status-based behavior
 * applies (HEAD-vs-worktree), so nothing silently changes for callers
 * that have not been updated.
 */
export async function openResource(
  root: vscode.Uri,
  relativePath: string,
  status: string,
  bucket?: CommitBucket,
): Promise<void> {
  const uri = resolveRepoPath(root, relativePath);
  if (bucket === 'staged') {
    const left = uri.with({ scheme: ENGINE_SCHEME, query: 'HEAD' });
    const right = indexUri(root, relativePath);
    const title = `${path.basename(uri.fsPath)} (Index — HEAD)`;
    await vscode.commands.executeCommand('vscode.diff', left, right, title);
    return;
  }
  if (bucket === 'changes' && status !== 'untracked' && status !== 'added' && status !== 'ignored') {
    const left = indexUri(root, relativePath);
    const title = `${path.basename(uri.fsPath)} (Working Tree — Index)`;
    await vscode.commands.executeCommand('vscode.diff', left, uri, title);
    return;
  }
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
