/**
 * Merge-editor conflict flow (phase 3): opens VS Code's built-in 3-way
 * merge editor (the undocumented-but-stable `_open.mergeEditor` command)
 * for one conflicted file, with the three sides served from the unmerged
 * index stages (base :1 / ours :2 / theirs :3) through the ENGINE_SCHEME
 * content provider.
 *
 * Completion is tracked on the save hook: when the merge editor's result
 * document (the worktree file) is saved without conflict markers, the
 * file is staged through the engine write queue — the index/worktree
 * change notifications then refresh the Commit view like any other
 * engine-side write.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { STAGE_BASE, STAGE_OURS, STAGE_THEIRS, stageUri } from './headContent';
import type { ConflictKind, ConflictResolution } from './types';
import { errorMessage } from './util';

/** Map a UI side to the resolution the engine accepts for the kind —
 *  same rule as commitView.sideResolution (delete/modify conflicts need
 *  the keep_* variants). Duplicated here so the module stays
 *  self-contained for the binary fast-path, which bypasses commitView. */
function sideResolution(kind: ConflictKind | undefined, side: 'ours' | 'theirs'): ConflictResolution {
  const deleteModify =
    kind === 'deleted_by_us' || kind === 'deleted_by_them' || kind === 'both_deleted';
  if (side === 'ours') return deleteModify ? 'keep_ours' : 'ours';
  return deleteModify ? 'keep_theirs' : 'theirs';
}

/** Read one conflict stage's blob; null when that side is absent (e.g.
 *  no :3 for a deleted-by-them conflict) — the merge editor then shows
 *  that side as empty, which is exactly its state. */
async function stageContent(client: EngineClient, relativePath: string, stage: string): Promise<string | null> {
  try {
    return await client.getBlob(stage, relativePath);
  } catch {
    return null;
  }
}

/** Conflict markers the merge editor may leave behind when regions are
 *  not fully resolved. `=======` alone is legitimate file content
 *  (markdown underlines), so block delimiters anchor the check: a line
 *  starting with `<<<<<<<` or `>>>>>>>` means unresolved regions. */
function hasConflictMarkers(content: string): boolean {
  return /^<{7}|^>{7}/m.test(content);
}

/** Whether the repository is mid-rebase (rebase-merge for interactive
 *  rebases, rebase-apply for am/legacy). During a rebase the stage
 *  semantics flip for the *user* — "ours" is the upstream side — which
 *  only affects the input titles in phase 3. */
async function isRebaseActive(root: vscode.Uri): Promise<boolean> {
  const dotGit = vscode.Uri.joinPath(root, '.git');
  for (const state of ['rebase-merge', 'rebase-apply']) {
    try {
      await vscode.workspace.fs.stat(vscode.Uri.joinPath(dotGit, state));
      return true;
    } catch {
      // absent — try the next state dir
    }
  }
  return false;
}

/** One tracked merge-editor session: the result (output) document whose
 *  saves we watch, and the repo-relative path to stage when it resolves. */
interface TrackedMerge {
  relativePath: string;
}

export class MergeEditorManager implements vscode.Disposable {
  /** Result documents of currently open merge editors (worktree file URIs). */
  private readonly tracked = new Map<string, TrackedMerge>();
  private readonly listener: vscode.Disposable;
  private readonly tabListener: vscode.Disposable;
  private disposed = false;

  constructor(
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {
    this.listener = vscode.workspace.onDidSaveTextDocument((doc) => void this.onSaved(doc));
    // Untrack when the merge editor tab closes unresolved, so a later
    // re-conflict of the same file starts clean. The tab input type
    // (TabInputTextMerge) is not in @types/vscode — duck-type on
    // `resultUri` instead of `any`-casting.
    this.tabListener = vscode.window.tabGroups.onDidChangeTabs((event) => {
      for (const closed of event.closed) {
        const input = closed.input as { resultUri?: unknown } | undefined;
        if (input && input.resultUri instanceof vscode.Uri) {
          this.tracked.delete(input.resultUri.toString());
        }
      }
    });
  }

  dispose(): void {
    this.disposed = true;
    this.listener.dispose();
    this.tabListener.dispose();
    this.tracked.clear();
  }

  /**
   * Open the 3-way merge editor for a conflicted file. Binary files
   * (NUL bytes on any side) cannot be merged textually — a quick pick
   * offers taking one side instead.
   *
   * Rebase handling: during a rebase the "ours" stage holds the upstream
   * side, so the input titles are relabeled (phase 3 keeps the URIs on
   * the natural current/incoming mapping; deeper rebase semantics —
   * swapping which stage feeds which input, and flipping the accept-side
   * buttons — are a follow-up).
   */
  async openFor(relativePath: string, kind: ConflictKind | undefined): Promise<void> {
    const base = await stageContent(this.client, relativePath, STAGE_BASE);
    const ours = await stageContent(this.client, relativePath, STAGE_OURS);
    const theirs = await stageContent(this.client, relativePath, STAGE_THEIRS);

    // Binary fast-path: 3-way merge of NUL-bearing content is noise.
    if ([base, ours, theirs].some((c) => c !== null && c.includes('\0'))) {
      await this.offerBinaryResolution(relativePath, kind);
      return;
    }

    const output = vscode.Uri.joinPath(this.root, ...relativePath.split('/'));
    const args: Record<string, unknown> = {
      // input1 = current (ours :2), input2 = incoming (theirs :3); the
      // output is the worktree file the editor writes resolutions into.
      input1: {
        uri: stageUri(this.root, relativePath, STAGE_OURS),
        title: 'Current (Yours)',
      },
      input2: {
        uri: stageUri(this.root, relativePath, STAGE_THEIRS),
        title: 'Incoming (Theirs)',
      },
      output,
    };
    // Base is optional: both-added conflicts have no stage 1, and the
    // merge editor degrades to a two-way diff without it.
    if (base !== null) {
      args.base = stageUri(this.root, relativePath, STAGE_BASE);
    }
    if (await isRebaseActive(this.root)) {
      // Rebase: "ours" is the upstream side — relabel, keep the mapping.
      (args.input1 as { title: string }).title = 'Current (Upstream)';
      (args.input2 as { title: string }).title = 'Incoming (Your change)';
    }

    await vscode.commands.executeCommand('_open.mergeEditor', args);
    this.tracked.set(output.toString(), { relativePath });
  }

  /** Binary conflicts: take a side directly (engine resolves and stages). */
  private async offerBinaryResolution(relativePath: string, kind: ConflictKind | undefined): Promise<void> {
    const file = path.posix.basename(relativePath);
    const pick = await vscode.window.showInformationMessage(
      `Git Bamboo: '${file}' is a binary file and cannot be merged in the editor. Which side do you want to keep?`,
      'Keep Current (Yours)',
      'Keep Incoming (Theirs)',
    );
    if (!pick) return;
    const side = pick.startsWith('Keep Current') ? 'ours' : 'theirs';
    try {
      await this.client.resolveConflict(relativePath, sideResolution(kind, side));
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: resolving ${relativePath} failed — ${errorMessage(err)}`,
      );
    }
  }

  /** Save hook: a tracked result document without conflict markers is a
   *  finished resolution — stage it through the engine write queue. The
   *  engine's index/worktree notifications then refresh the view. */
  private async onSaved(doc: vscode.TextDocument): Promise<void> {
    if (this.disposed) return;
    const entry = this.tracked.get(doc.uri.toString());
    if (!entry) return;
    if (hasConflictMarkers(doc.getText())) {
      vscode.window.setStatusBarMessage(
        `Git Bamboo: ${path.posix.basename(entry.relativePath)} still has conflict markers — not staged`,
        4000,
      );
      return;
    }
    try {
      await this.client.request('stage', { paths: [entry.relativePath] });
      this.tracked.delete(doc.uri.toString()); // resolved; stop watching
      vscode.window.setStatusBarMessage(
        `Git Bamboo: resolved and staged ${path.posix.basename(entry.relativePath)}`,
        4000,
      );
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: staging the resolved ${entry.relativePath} failed — ${errorMessage(err)}`,
      );
    }
  }
}
