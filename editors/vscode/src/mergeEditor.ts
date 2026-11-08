/**
 * Merge-editor conflict flow (phase 3): opens VS Code's built-in 3-way
 * merge editor (the undocumented-but-stable `_open.mergeEditor` command)
 * for one conflicted file, with the three sides served from the unmerged
 * index stages (base :1 / ours :2 / theirs :3) through the ENGINE_SCHEME
 * content provider.
 *
 * Completion is tracked on the merge editor's tab close: the Complete
 * Merge button saves the result into the worktree file and closes the
 * editor, and the close hook stages the worktree content through the
 * engine write queue — the index/worktree notifications then refresh the
 * Commit view like any other engine-side write. Plain saves inside the
 * editor deliberately do NOT stage: an unresolved result document
 * serializes as the base content without conflict markers, so a save
 * hook would silently stage the base version and collapse the conflict
 * with the wrong content.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import { STAGE_BASE, STAGE_OURS, STAGE_THEIRS, stageUri } from './headContent';
import { sideResolution } from './types';
import type { ConflictKind, StatusItem } from './types';
import { errorMessage } from './util';

/** Conflict markers the merge editor may leave behind when regions are
 *  not fully resolved. `=======` alone is legitimate file content
 *  (markdown underlines), so block delimiters anchor the check: a line
 *  starting with `<<<<<<<` or `>>>>>>>` means unresolved regions. */
function hasConflictMarkers(content: string): boolean {
  return /^<{7}|^>{7}/m.test(content);
}

/** Resolve the repository's git dir: `.git` is a directory in the main
 *  worktree, but a "gitdir: <path>" pointer file in linked worktrees —
 *  rebase state dirs live under the real git dir either way. Falls back
 *  to `.git` itself when it cannot be read (callers treat errors as
 *  "not rebasing"). */
async function resolveGitDir(root: vscode.Uri): Promise<vscode.Uri> {
  const dotGit = vscode.Uri.joinPath(root, '.git');
  try {
    const stat = await vscode.workspace.fs.stat(dotGit);
    if ((stat.type & vscode.FileType.Directory) === 0) {
      const raw = new TextDecoder().decode(await vscode.workspace.fs.readFile(dotGit));
      const target = raw.trim().match(/^gitdir:\s*(.+)$/)?.[1];
      if (target) return vscode.Uri.file(target);
    }
  } catch {
    // unreadable .git — fall through with .git itself
  }
  return dotGit;
}

/** Whether the repository is mid-rebase (rebase-merge for interactive
 *  rebases, rebase-apply for am/legacy). During a rebase the stage
 *  semantics flip for the *user* — "ours" is the upstream side. */
async function isRebaseActive(root: vscode.Uri): Promise<boolean> {
  const gitDir = await resolveGitDir(root);
  for (const state of ['rebase-merge', 'rebase-apply']) {
    try {
      await vscode.workspace.fs.stat(vscode.Uri.joinPath(gitDir, state));
      return true;
    } catch {
      // absent — try the next state dir
    }
  }
  return false;
}

/** Merge-editor input assignment for one conflicted file. In both orders
 *  stage :2 (ours) feeds input1 "Current" and :3 (theirs) input2
 *  "Incoming" — only the *titles* differ, because during a rebase "ours"
 *  is the upstream side and "theirs" is the user's own replayed change.
 *  The order is decided in ONE place — openFor's on-disk rebase detection
 *  (isRebaseActive): engine- and externally-started rebases, pull/merge
 *  conflicts, and every consumer get the right titles from the same
 *  mechanism, with no per-caller parameter plumbing. */
export type MergeInputOrder = 'merge' | 'rebase';

function mergeInputs(root: vscode.Uri, relativePath: string, order: MergeInputOrder) {
  const titles =
    order === 'merge'
      ? { input1: 'Current (Yours)', input2: 'Incoming (Theirs)' }
      : { input1: 'Current (Upstream)', input2: 'Incoming (Your change)' };
  return {
    input1: { uri: stageUri(root, relativePath, STAGE_OURS), title: titles.input1 },
    input2: { uri: stageUri(root, relativePath, STAGE_THEIRS), title: titles.input2 },
  };
}

export class MergeEditorManager implements vscode.Disposable {
  /** Result documents of currently open merge editors (worktree file URIs)
   *  mapped to the repo-relative path to stage when they resolve. */
  private readonly tracked = new Map<string, string>();
  private readonly tabListener: vscode.Disposable;
  private disposed = false;

  constructor(
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {
    // Completion = the merge editor tab closing (Complete Merge saves the
    // result into the worktree file first); an unresolved close leaves the
    // worktree conflicted or base-identical and the guards below refuse to
    // stage. The tab input type (TabInputTextMerge) is not in
    // @types/vscode — duck-type on `resultUri` instead of `any`-casting.
    this.tabListener = vscode.window.tabGroups.onDidChangeTabs((event) => {
      for (const closed of event.closed) {
        const input = closed.input as { resultUri?: unknown } | undefined;
        if (input && input.resultUri instanceof vscode.Uri) {
          const key = input.resultUri.toString();
          const relativePath = this.tracked.get(key);
          if (!relativePath) continue;
          this.tracked.delete(key);
          void this.stageResolvedResult(input.resultUri, relativePath);
        }
      }
    });
  }

  dispose(): void {
    this.disposed = true;
    this.tabListener.dispose();
    this.tracked.clear();
  }

  /**
   * Open the 3-way merge editor for a conflicted file. Binary files
   * (NUL bytes on any side) cannot be merged textually — a quick pick
   * offers taking one side instead.
   *
   * The three sides are fetched up front so real failures (engine down,
   * transport error) fail loudly here instead of opening an editor with
   * silently empty sides that a save would then stage. An absent stage
   * (no :3 for a deleted side, no :1 base for both-added) resolves to
   * the empty string — that side's actual state — and errors propagate.
   * `kind` selects the binary-resolution offer for binary conflicts.
   * The input-title order (merge vs. rebase) is decided here from the
   *  on-disk rebase state (isRebaseActive) — the single mechanism, valid
   *  for engine- and externally-started rebases alike.
   */
  async openFor(
    relativePath: string,
    kind: ConflictKind | undefined,
  ): Promise<void> {
    const base = await this.client.getBlob(STAGE_BASE, relativePath);
    const ours = await this.client.getBlob(STAGE_OURS, relativePath);
    const theirs = await this.client.getBlob(STAGE_THEIRS, relativePath);

    // Binary fast-path: 3-way merge of NUL-bearing content is noise.
    if (base.includes('\0') || ours.includes('\0') || theirs.includes('\0')) {
      await this.offerBinaryResolution(relativePath, kind);
      return;
    }

    const output = vscode.Uri.joinPath(this.root, ...relativePath.split('/'));
    const resolvedOrder: MergeInputOrder = (await isRebaseActive(this.root)) ? 'rebase' : 'merge';
    const { input1, input2 } = mergeInputs(this.root, relativePath, resolvedOrder);
    const args: Record<string, unknown> = {
      // input1 = current (ours :2), input2 = incoming (theirs :3); the
      // output is the worktree file the editor writes resolutions into.
      input1,
      input2,
      output,
    };
    // Base is optional: both-added conflicts have no stage 1 (the engine
    // serves an absent stage as an empty string), and the merge editor
    // degrades to a two-way diff without it.
    if (base !== '') {
      args.base = stageUri(this.root, relativePath, STAGE_BASE);
    }

    await vscode.commands.executeCommand('_open.mergeEditor', args);
    this.tracked.set(output.toString(), relativePath);
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

  /**
   * Merge editor closed: stage the worktree result if it is a finished
   * resolution. Guards, cheapest first:
   *  - conflict markers (a forced close left unresolved regions behind);
   *  - freshness: the conflict may already be resolved through another
   *    path (Accept Yours button) while the editor stayed open;
   *  - the untouched-result trap: an unresolved merge editor serializes
   *    as the base content without markers, so a result identical to the
   *    base while both sides differ from it means the conflict blocks
   *    were never resolved — staging it would silently collapse the
   *    conflict with the wrong content.
   */
  private async stageResolvedResult(
    resultUri: vscode.Uri,
    relativePath: string,
  ): Promise<void> {
    if (this.disposed) return;
    const name = path.posix.basename(relativePath);
    let content: string | undefined;
    for (let attempt = 0; attempt < 2; attempt++) {
      try {
        content = new TextDecoder().decode(await vscode.workspace.fs.readFile(resultUri));
      } catch {
        return; // worktree file gone (deleted side, repo moved) — nothing to stage
      }
      if (!hasConflictMarkers(content)) break;
      // Complete Merge writes the result just before closing the tab;
      // give a racing write one chance to land before refusing.
      await new Promise((resolve) => setTimeout(resolve, 200));
    }
    if (content === undefined) return;
    if (hasConflictMarkers(content)) {
      vscode.window.setStatusBarMessage(
        `Git Bamboo: ${name} still has conflict markers — not staged`,
        4000,
      );
      return;
    }
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      const stillConflicted =
        Array.isArray(items) &&
        items.some((item) => item.path === relativePath && item.status === 'conflict');
      if (!stillConflicted) {
        vscode.window.setStatusBarMessage(
          `Git Bamboo: ${name} was already resolved — not staged`,
          4000,
        );
        return;
      }
      const [base, ours, theirs] = await Promise.all([
        this.client.getBlob(STAGE_BASE, relativePath),
        this.client.getBlob(STAGE_OURS, relativePath),
        this.client.getBlob(STAGE_THEIRS, relativePath),
      ]);
      if (content === base && ours !== base && theirs !== base) {
        vscode.window.setStatusBarMessage(
          `Git Bamboo: ${name} merge result is still the base version — ` +
            'resolve the conflict blocks (Complete Merge) before staging',
          6000,
        );
        return;
      }
      await this.client.request('stage', { paths: [relativePath] });
      vscode.window.setStatusBarMessage(`Git Bamboo: resolved and staged ${name}`, 4000);
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: staging the resolved ${relativePath} failed — ${errorMessage(err)}`,
      );
    }
  }
}
