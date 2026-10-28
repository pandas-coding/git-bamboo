/**
 * User-facing command implementations: branch switch/create/delete,
 * undo picker, fetch/pull/push wrappers. All go through the engine RPC.
 */
import * as vscode from 'vscode';
import { EngineClient, RpcError } from './engineClient';
import * as netOps from './netOps';
import type { Ref, RepoState, UndoEntry } from './types';
import { errorMessage, relativeTime } from './util';

/** JSON-RPC error code returned when undo is blocked. */
const UNDO_BLOCKED = -32007;

interface BranchPickItem extends vscode.QuickPickItem {
  branchName: string;
}

async function pickBranch(
  client: EngineClient,
  placeHolder: string,
  exclude?: string,
): Promise<string | undefined> {
  const refs = await client.request<Ref[]>('getRefs', {});
  const branches = refs.filter((ref) => ref.kind === 'branch' && ref.name !== exclude);
  if (branches.length === 0) {
    vscode.window.showWarningMessage('Git Bamboo: no branches found');
    return undefined;
  }
  const picked = await vscode.window.showQuickPick<BranchPickItem>(
    branches.map((branch) => ({
      label: branch.name,
      description: branch.target.slice(0, 10),
      branchName: branch.name,
    })),
    { placeHolder },
  );
  return picked?.branchName;
}

export async function switchBranch(client: EngineClient): Promise<void> {
  const name = await pickBranch(client, 'Switch to branch');
  if (!name) return;
  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `Git Bamboo: switching to ${name}…` },
    async () => {
      try {
        await client.request('switchBranch', { name, auto_stash: true });
        vscode.window.showInformationMessage(`Git Bamboo: switched to ${name}`);
      } catch (err) {
        vscode.window.showErrorMessage(`Git Bamboo: switch failed — ${errorMessage(err)}`);
      }
    },
  );
}

export async function createBranch(client: EngineClient): Promise<void> {
  const name = await vscode.window.showInputBox({
    prompt: 'Branch name',
    validateInput: (value) => (value.trim() ? undefined : 'Name is required'),
  });
  if (!name) return;
  // Optional base: HEAD by default, cancel keeps base unspecified (engine uses HEAD).
  const picked = await vscode.window.showQuickPick<BranchPickItem>(
    [
      { label: 'HEAD', description: 'current commit', branchName: 'HEAD' },
      ...(await client.request<Ref[]>('getRefs', {}))
        .filter((ref) => ref.kind === 'branch')
        .map((branch) => ({ label: branch.name, description: branch.target.slice(0, 10), branchName: branch.name })),
    ],
    { placeHolder: `Create ${name} from` },
  );
  if (picked === undefined) return; // treat ESC as abort of the whole command
  const base = picked.branchName === 'HEAD' ? undefined : picked.branchName;
  try {
    await client.request('createBranch', { name, ...(base ? { base } : {}) });
    vscode.window.showInformationMessage(`Git Bamboo: created branch ${name}`);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: create branch failed — ${errorMessage(err)}`);
  }
}

export async function deleteBranch(client: EngineClient, state: RepoState): Promise<void> {
  const name = await pickBranch(client, 'Delete branch', state.currentBranch);
  if (!name) return;
  const confirmed = await vscode.window.showWarningMessage(
    `Delete branch "${name}"?`,
    { modal: true, detail: 'This runs a non-forced delete; unmerged branches will be refused.' },
    'Delete',
  );
  if (confirmed !== 'Delete') return;
  try {
    await client.request('deleteBranch', { name, force: false });
    vscode.window.showInformationMessage(`Git Bamboo: deleted branch ${name}`);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: delete failed — ${errorMessage(err)}`);
  }
}

interface UndoPickItem extends vscode.QuickPickItem {
  entryId: number;
}

export async function undoLast(client: EngineClient): Promise<void> {
  const entries = await client.request<UndoEntry[]>('listUndoStack', { limit: 20 });
  if (!Array.isArray(entries) || entries.length === 0) {
    vscode.window.showInformationMessage('Git Bamboo: nothing to undo');
    return;
  }
  const picked = await vscode.window.showQuickPick<UndoPickItem>(
    entries.map((entry) => ({
      label: entry.description,
      description: `#${entry.id}`,
      detail: relativeTime(entry.timestamp),
      entryId: entry.id,
    })),
    { placeHolder: 'Choose an operation to undo (newest first)', matchOnDetail: true },
  );
  if (!picked) return;
  try {
    await client.request('undo', { transaction_id: picked.entryId });
    vscode.window.showInformationMessage('Git Bamboo: operation undone');
  } catch (err) {
    if (err instanceof RpcError && err.code === UNDO_BLOCKED) {
      vscode.window.showWarningMessage(`Git Bamboo: undo blocked — ${err.message}`);
    } else {
      vscode.window.showErrorMessage(`Git Bamboo: undo failed — ${errorMessage(err)}`);
    }
  }
}

/** Build the shared net-ops context: the engine client, repo root, and
 *  the conflict-flow entry point (focuses the Commit view's conflicts
 *  section via its command, registered per-session in extension.ts).
 *  Shared by the palette commands and the graph toolbar (graphSession) —
 *  every surface gets the identical interaction flows. */
export function netOpsContext(client: EngineClient, state: RepoState): netOps.NetOpsContext {
  return {
    client,
    repoRoot: state.root,
    showConflicts: async () => {
      await vscode.commands.executeCommand('gitBamboo.commit.showConflicts');
    },
  };
}

/** Fetch wrapper: remote pick and status surfacing live in netOps, which
 *  also needs the engine's remote list. */
export async function fetchRemotes(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.runFetch(netOpsContext(client, state));
}

/** Palette pull: upstream detection, publish fallback, strategy and
 *  auto-stash settings, conflict surfacing — all in netOps. */
export async function pull(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.runPull(netOpsContext(client, state));
}

/** Palette "Pull with strategy…": one-shot strategy override. */
export async function pullWithStrategy(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.runPullWithStrategy(netOpsContext(client, state));
}

/** Palette push: publish flow for unpublished branches, one-click push
 *  (and rejection recovery) otherwise — all in netOps. */
export async function push(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.runPush(netOpsContext(client, state));
}

/** Explicit force-push entry — the only path (besides the rejection
 *  recovery button) that can ever produce --force*. */
export async function forcePush(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.runForcePush(netOpsContext(client, state));
}
