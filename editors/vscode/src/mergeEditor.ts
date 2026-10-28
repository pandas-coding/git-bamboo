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
 *  This is the structural hook for the rebase follow-up: deeper rebase
 *  semantics (flipping the accept-side buttons in the conflicts bucket to
 *  match these titles) key off `order`, not off openFor internals. */
type MergeInputOrder = 'merge' | 'rebase';

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
   * The three sides are fetched up front so real failures (engine down,
   * transport error) fail loudly here instead of opening an editor with
   * silently empty sides that a save would then stage. An absent stage
   * (no :3 for a deleted side, no :1 base for both-added) resolves to
   * the empty string — that side's actual state — and errors propagate.
   */
  async openFor(relativePath: string, kind: ConflictKind | undefined): Promise<void> {
    const base = await this.client.getBlob(STAGE_BASE, relativePath);
    const ours = await this.client.getBlob(STAGE_OURS, relativePath);
    const theirs = await this.client.getBlob(STAGE_THEIRS, relativePath);

    // Binary fast-path: 3-way merge of NUL-bearing content is noise.
    if (base.includes('\0') || ours.includes('\0') || theirs.includes('\0')) {
      await this.offerBinaryResolution(relativePath, kind);
      return;
    }

    const output = vscode.Uri.joinPath(this.root, ...relativePath.split('/'));
    const order: MergeInputOrder = (await isRebaseActive(this.root)) ? 'rebase' : 'merge';
    const { input1, input2 } = mergeInputs(this.root, relativePath, order);
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

  /** Save hook: a tracked result document without conflict markers is a
   *  finished resolution — stage it through the engine write queue. The
   *  engine's index/worktree notifications then refresh the view. */
  private async onSaved(doc: vscode.TextDocument): Promise<void> {
    if (this.disposed) return;
    const relativePath = this.tracked.get(doc.uri.toString());
    if (!relativePath) return;
    if (hasConflictMarkers(doc.getText())) {
      vscode.window.setStatusBarMessage(
        `Git Bamboo: ${path.posix.basename(relativePath)} still has conflict markers — not staged`,
        4000,
      );
      return;
    }
    // Freshness: the user may have resolved this file through another
    // path (e.g. the Accept Yours button) while the merge-editor tab
    // stayed open — the session was never untracked then, and staging
    // here would overwrite the accepted resolution with the merge
    // editor's (stale) result. Re-check against a fresh status first.
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      const stillConflicted =
        Array.isArray(items) &&
        items.some((item) => item.path === relativePath && item.status === 'conflict');
      if (!stillConflicted) {
        this.tracked.delete(doc.uri.toString()); // someone else resolved it; stop watching
        vscode.window.setStatusBarMessage(
          `Git Bamboo: ${path.posix.basename(relativePath)} was already resolved — not staged`,
          4000,
        );
        return;
      }
    } catch {
      // Status unavailable (engine restarting): fall through and let the
      // stage RPC itself arbitrate.
    }
    try {
      await this.client.request('stage', { paths: [relativePath] });
      this.tracked.delete(doc.uri.toString()); // resolved; stop watching
      vscode.window.setStatusBarMessage(
        `Git Bamboo: resolved and staged ${path.posix.basename(relativePath)}`,
        4000,
      );
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: staging the resolved ${relativePath} failed — ${errorMessage(err)}`,
      );
    }
  }
}
