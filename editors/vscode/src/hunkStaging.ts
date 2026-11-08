/**
 * Hunk-level staging UI (phase 5): [Stage Hunk] / [Unstage Hunk] CodeLens
 * actions on the diff views opened from the Commit view.
 *
 * The engine is the single source of hunk data (getDiff RPC): the lenses
 * are rendered from a cached engine response, and clicking one sends only
 * a line RANGE back — the engine re-computes the diff and applies the
 * intersecting hunks (VS Code's approach: blob reconstruction via
 * hash-object + update-index, never a client-supplied patch). A stale
 * selection is rejected engine-side with INVALID_ARGUMENT, surfaced here
 * as a warning plus a lens refresh.
 *
 * Lenses attach only to the RIGHT (new) side of diffs this extension
 * opened: worktree file (changes bucket → stage) or the `:0` index
 * document (staged bucket → unstage). Targeting is derived statelessly
 * from the open diff tabs, so unrelated editors never get lenses.
 */
import * as vscode from 'vscode';
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import { ENGINE_SCHEME, INDEX_REVISION, isValidRepoPath, toRelativePath } from './headContent';
import { hunkNewInterval, type DiffHunk, type LineRange } from './types';
import { errorMessage } from './util';

/** Palette-hidden command IDs, invoked only from CodeLenses. */
export const STAGE_HUNK_COMMAND = 'gitBamboo.stageHunk';
export const UNSTAGE_HUNK_COMMAND = 'gitBamboo.unstageHunk';

/** Lens titles (also the CodeLens display text). */
const STAGE_TITLE = 'Stage Hunk';
const UNSTAGE_TITLE = 'Unstage Hunk';

/** How long a cached getDiff response is reused before the provider
 *  re-fetches on the next render. Short: hunk actions and engine
 *  notifications invalidate immediately; the TTL only covers edits made
 *  outside our own flows. */
const CACHE_TTL_MS = 3_000;

/** Upper bound for line-range endpoints — mirrors the engine's bound so
 *  host-side clamping and engine-side validation agree. */
const MAX_LINE_RANGE = 10_000_000;

/** Which diff side a document plays, when it is one worth decorating. */
interface DiffTarget {
  /** Repo-relative path (forward slashes) for the engine RPCs. */
  readonly path: string;
  /** true = staged diff (index vs HEAD) → [Unstage Hunk]; false =
   *  worktree diff (worktree vs index) → [Stage Hunk]. */
  readonly staged: boolean;
}

interface CachedDiff {
  readonly hunks: readonly DiffHunk[];
  readonly binary: boolean;
  fetchedAt: number;
}

export class HunkStagingController implements vscode.Disposable {
  private readonly lensesChanged = new vscode.EventEmitter<void>();
  private readonly provider: vscode.Disposable;
  /** getDiff cache keyed by `${staged}:${path}`. */
  private readonly cache = new Map<string, CachedDiff>();
  /** In-flight getDiff fetches (one per key — no thundering herd). */
  private readonly fetching = new Set<string>();
  private disposed = false;

  constructor(
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
    /** Called after a successful hunk action so the Commit view refreshes
     *  its staged/changes lists. */
    private readonly onApplied: () => void,
  ) {
    this.provider = vscode.languages.registerCodeLensProvider(
      [{ scheme: 'file' }, { scheme: ENGINE_SCHEME }],
      {
        onDidChangeCodeLenses: this.lensesChanged.event,
        provideCodeLenses: (document) => this.provideLenses(document),
      },
    );
  }

  dispose(): void {
    this.disposed = true;
    this.provider.dispose();
    this.lensesChanged.dispose();
    this.cache.clear();
  }

  /** Drop cached hunks and re-render the lenses (engine
   *  indexChanged/worktreeChanged notifications, and after a hunk action). */
  refreshLenses(): void {
    if (this.disposed) return;
    this.cache.clear();
    this.lensesChanged.fire();
  }

  /** [Stage Hunk] command entry (CodeLens argument list). Host-side
   *  clamping is a UX nicety — the engine re-validates everything. */
  async stageHunk(pathArg: unknown, rangeArg: unknown): Promise<void> {
    await this.applyRange(false, pathArg, rangeArg);
  }

  /** [Unstage Hunk] command entry. */
  async unstageHunk(pathArg: unknown, rangeArg: unknown): Promise<void> {
    await this.applyRange(true, pathArg, rangeArg);
  }

  private async applyRange(staged: boolean, pathArg: unknown, rangeArg: unknown): Promise<void> {
    const target = this.coerceTarget(pathArg, rangeArg);
    if (!target) return; // malformed CodeLens payload: silently ignore
    try {
      await (staged
        ? this.client.unstageRanges(target.path, [target.range])
        : this.client.stageRanges(target.path, [target.range]));
      this.refreshLenses();
      this.onApplied();
    } catch (err) {
      if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
        // Stale selection (the diff changed since the lens was rendered):
        // refresh and let the user retry — nothing was written.
        this.refreshLenses();
        vscode.window.showWarningMessage(`Git Bamboo: ${errorMessage(err)}`);
      } else {
        vscode.window.showErrorMessage(
          `Git Bamboo: ${staged ? 'unstage' : 'stage'} hunk failed — ${errorMessage(err)}`,
        );
      }
    }
  }

  /** Validate the CodeLens payload: a repo-relative path plus a 1-based
   *  ordered line range, clamped to the engine's bounds. */
  private coerceTarget(pathArg: unknown, rangeArg: unknown): { path: string; range: LineRange } | undefined {
    if (typeof pathArg !== 'string' || !isValidRepoPath(pathArg)) return undefined;
    if (typeof rangeArg !== 'object' || rangeArg === null) return undefined;
    const { start, end } = rangeArg as Record<string, unknown>;
    if (typeof start !== 'number' || typeof end !== 'number') return undefined;
    const clampedStart = Math.max(1, Math.floor(start));
    const clampedEnd = Math.min(Math.max(Math.floor(end), clampedStart), MAX_LINE_RANGE);
    return { path: pathArg, range: { start: clampedStart, end: clampedEnd } };
  }

  /** CodeLens rendering: hunks from the engine (cached), one lens per hunk
   *  on the hunk's first new-side line. */
  private async provideLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    const target = this.diffTarget(document.uri);
    if (!target) return [];
    const key = `${target.staged}:${target.path}`;
    const cached = this.cache.get(key);
    if (cached === undefined || Date.now() - cached.fetchedAt > CACHE_TTL_MS) {
      this.fetchDiff(target, key);
      if (cached === undefined) return [];
    }
    if (cached.binary || cached.hunks.length === 0) return [];
    const lineCount = document.lineCount;
    return cached.hunks.flatMap((hunk) => {
      const line = Math.min(Math.max(hunk.new_start, 1) - 1, Math.max(lineCount - 1, 0));
      const range = new vscode.Range(line, 0, line, 0);
      return [
        new vscode.CodeLens(range, {
          title: target.staged ? UNSTAGE_TITLE : STAGE_TITLE,
          command: target.staged ? UNSTAGE_HUNK_COMMAND : STAGE_HUNK_COMMAND,
          arguments: [target.path, hunkNewInterval(hunk)],
        }),
      ];
    });
  }

  /** Fetch the fresh hunk list in the background and re-render once it
   *  lands (the current render serves the cached lenses meanwhile). */
  private fetchDiff(target: DiffTarget, key: string): void {
    if (this.fetching.has(key)) return;
    this.fetching.add(key);
    void this.client
      .getDiff(target.path, target.staged)
      .then((result) => {
        if (this.disposed) return;
        this.cache.set(key, {
          hunks: result.hunks,
          binary: result.binary,
          fetchedAt: Date.now(),
        });
        this.lensesChanged.fire();
      })
      .catch((err) => {
        console.error(`[git-bamboo] getDiff ${target.path} failed:`, err);
      })
      .finally(() => {
        this.fetching.delete(key);
      });
  }

  /** Decide whether `uri` is the right side of a diff this extension
   *  opened for hunk staging, derived from the open tabs (stateless — no
   *  tracked-URI map to leak or expire):
   *    - file URI modified-side of worktree-vs-index diff → stage target
   *    - `:0` index URI modified-side of HEAD-vs-index diff → unstage target
   *  Anything else (plain editors, HEAD-vs-worktree diffs, conflict
   *  stages) is not a hunk-staging surface. */
  private diffTarget(uri: vscode.Uri): DiffTarget | undefined {
    for (const tab of vscode.window.tabGroups.all.flatMap((group) => group.tabs)) {
      const input = tab.input;
      if (!(input instanceof vscode.TabInputTextDiff)) continue;
      if (input.modified.toString(true) !== uri.toString(true)) continue;
      const original = input.original;
      if (original.scheme !== ENGINE_SCHEME) continue;
      if (uri.scheme === 'file' && original.query === INDEX_REVISION) {
        return { path: toRelativePath(this.root, original), staged: false };
      }
      if (uri.scheme === ENGINE_SCHEME && uri.query === INDEX_REVISION && original.query === 'HEAD') {
        return { path: toRelativePath(this.root, uri), staged: true };
      }
    }
    return undefined;
  }
}
