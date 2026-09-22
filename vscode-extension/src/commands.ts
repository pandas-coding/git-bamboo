/**
 * User-facing command implementations: branch switch/create/delete,
 * undo picker, fetch/pull/push wrappers. All go through the engine RPC.
 */
import * as vscode from 'vscode';
import { EngineClient, RpcError } from './engineClient';
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
    vscode.window.showWarningMessage('Git Workbench: no branches found');
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

async function pickRemote(): Promise<string | undefined> {
  const remote = await vscode.window.showInputBox({ prompt: 'Remote name', value: 'origin' });
  if (remote === undefined) return undefined; // cancelled
  return remote.trim() || undefined;
}

export async function switchBranch(client: EngineClient): Promise<void> {
  const name = await pickBranch(client, 'Switch to branch');
  if (!name) return;
  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `Git Workbench: switching to ${name}…` },
    async () => {
      try {
        await client.request('switchBranch', { name, auto_stash: true });
        vscode.window.showInformationMessage(`Git Workbench: switched to ${name}`);
      } catch (err) {
        vscode.window.showErrorMessage(`Git Workbench: switch failed — ${errorMessage(err)}`);
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
    vscode.window.showInformationMessage(`Git Workbench: created branch ${name}`);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Workbench: create branch failed — ${errorMessage(err)}`);
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
    vscode.window.showInformationMessage(`Git Workbench: deleted branch ${name}`);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Workbench: delete failed — ${errorMessage(err)}`);
  }
}

interface UndoPickItem extends vscode.QuickPickItem {
  entryId: number;
}

export async function undoLast(client: EngineClient): Promise<void> {
  const entries = await client.request<UndoEntry[]>('listUndoStack', { limit: 20 });
  if (!Array.isArray(entries) || entries.length === 0) {
    vscode.window.showInformationMessage('Git Workbench: nothing to undo');
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
    vscode.window.showInformationMessage('Git Workbench: operation undone');
  } catch (err) {
    if (err instanceof RpcError && err.code === UNDO_BLOCKED) {
      vscode.window.showWarningMessage(`Git Workbench: undo blocked — ${err.message}`);
    } else {
      vscode.window.showErrorMessage(`Git Workbench: undo failed — ${errorMessage(err)}`);
    }
  }
}

export async function fetchRemotes(client: EngineClient): Promise<void> {
  const remote = await pickRemote();
  if (remote === undefined) return; // cancelled
  await runNetworkCommand(client, 'Git Workbench: fetching…', 'fetch', remote ? { remote } : {});
  vscode.window.showInformationMessage('Git Workbench: fetch complete');
}

export async function pull(client: EngineClient, state: RepoState): Promise<void> {
  const params: Record<string, unknown> = {};
  if (state.currentBranch) params.branch = state.currentBranch;
  await runNetworkCommand(client, `Git Workbench: pulling${state.currentBranch ? ` ${state.currentBranch}` : ''}…`, 'pull', params);
  vscode.window.showInformationMessage('Git Workbench: pull complete');
}

export async function push(client: EngineClient, state: RepoState): Promise<void> {
  const remote = await pickRemote();
  if (remote === undefined) return; // cancelled
  let branch = state.currentBranch;
  if (!branch) {
    branch = await pickBranch(client, 'Push which branch?');
    if (!branch) return;
  }
  await runNetworkCommand(client, `Git Workbench: pushing ${branch}…`, 'push', {
    remote: remote ?? 'origin',
    branch,
    force: false,
  });
  vscode.window.showInformationMessage(`Git Workbench: pushed ${branch}`);
}

async function runNetworkCommand(
  client: EngineClient,
  title: string,
  method: string,
  params: Record<string, unknown>,
): Promise<void> {
  await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title }, async () => {
    try {
      await client.request(method, params);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: ${method} failed — ${errorMessage(err)}`);
    }
  });
}
