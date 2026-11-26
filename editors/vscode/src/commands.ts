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

/** One entry of the branch switcher: a branch to switch to, or the
 *  "create new branch" entry. */
interface SwitchPickItem extends vscode.QuickPickItem {
  branchName?: string;
  create?: boolean;
}

/** The branch switcher shared by the status bar item, the graph toolbar's
 *  branch pill and the command palette: current branch first, then the
 *  other local branches, then remote-tracking branches (checking one out
 *  creates a local tracking branch — the engine handles that), plus a
 *  "create new branch" entry. */
export async function switchBranch(client: EngineClient): Promise<void> {
  let refs: Ref[];
  let current: string | null;
  try {
    [refs, current] = await Promise.all([
      client.request<Ref[]>('getRefs', {}),
      client.getHead().then((head) => head.branch),
    ]);
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: could not read branches — ${errorMessage(err)}`);
    return;
  }
  const locals = refs
    .filter((ref) => ref.kind === 'branch')
    .map((ref) => ref.name)
    .sort((a, b) => (a === current ? -1 : b === current ? 1 : a.localeCompare(b)));
  const remotes = refs
    .filter((ref) => ref.kind === 'remote_branch')
    .map((ref) => ref.name)
    .sort((a, b) => a.localeCompare(b));

  const items: SwitchPickItem[] = [
    {
      label: '$(add) Create new branch…',
      description: 'from a base you pick next',
      create: true,
      alwaysShow: true,
    },
  ];
  if (locals.length > 0) {
    items.push({ label: 'Local branches', kind: vscode.QuickPickItemKind.Separator });
    for (const name of locals) {
      items.push({
        label: name === current ? `$(check) ${name}` : name,
        description: name === current ? 'current branch' : undefined,
        branchName: name,
      });
    }
  }
  if (remotes.length > 0) {
    items.push({ label: 'Remote branches', kind: vscode.QuickPickItemKind.Separator });
    for (const name of remotes) {
      // A remote branch whose short name is already local switches to that
      // local branch (engine rule) — say so instead of promising a checkout.
      const short = name.slice(name.indexOf('/') + 1);
      items.push({
        label: name,
        description: locals.includes(short)
          ? `switches to local ${short}`
          : 'checks out as a local tracking branch',
        branchName: name,
      });
    }
  }

  const picked = await vscode.window.showQuickPick(items, {
    placeHolder: 'Switch to branch (type to search)',
    matchOnDescription: true,
  });
  if (!picked) return;
  if (picked.create) {
    // Creating from the switcher means "create and go there".
    const created = await createBranch(client);
    if (created) await switchToBranch(client, created);
    return;
  }
  if (picked.branchName) await switchToBranch(client, picked.branchName);
}

/** Switch to `name` with auto-stash, surfacing progress and errors. The
 *  engine checks a remote-tracking name out as a local tracking branch. */
export async function switchToBranch(client: EngineClient, name: string): Promise<void> {
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

/** Create a branch; resolves to its name on success (the switcher checks
 *  the new branch out afterwards), undefined when cancelled or failed. */
export async function createBranch(client: EngineClient): Promise<string | undefined> {
  const name = await vscode.window.showInputBox({
    prompt: 'Branch name',
    validateInput: (value) => (value.trim() ? undefined : 'Name is required'),
  });
  if (!name) return undefined;
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
  if (picked === undefined) return undefined; // treat ESC as abort of the whole command
  const base = picked.branchName === 'HEAD' ? undefined : picked.branchName;
  try {
    await client.request('createBranch', { name, ...(base ? { base } : {}) });
    vscode.window.showInformationMessage(`Git Bamboo: created branch ${name}`);
    return name;
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: create branch failed — ${errorMessage(err)}`);
    return undefined;
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

/** Fetch wrapper: remote pick and status surfacing live in netOps, which
 *  also needs the engine's remote list. Runs under the shared net-op
 *  in-flight guard (an op already running for this repo session drops
 *  the command — see netOps.withNetOpGuard). */
export async function fetchRemotes(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.withNetOpGuard(client, () => netOps.runFetch(netOps.netOpsContext(client, state)));
}

/** Palette pull: upstream detection, publish fallback, strategy and
 *  auto-stash settings, conflict surfacing — all in netOps. */
export async function pull(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.withNetOpGuard(client, () => netOps.runPull(netOps.netOpsContext(client, state)));
}

/** Palette "Pull with strategy…": one-shot strategy override. */
export async function pullWithStrategy(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.withNetOpGuard(
    client,
    () => netOps.runPullWithStrategy(netOps.netOpsContext(client, state)),
  );
}

/** Palette push: publish flow for unpublished branches, one-click push
 *  (and rejection recovery) otherwise — all in netOps. */
export async function push(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.withNetOpGuard(client, () => netOps.runPush(netOps.netOpsContext(client, state)));
}

/** Explicit force-push entry — the only path (besides the rejection
 *  recovery button) that can ever produce --force*. */
export async function forcePush(client: EngineClient, state: RepoState): Promise<void> {
  await netOps.withNetOpGuard(
    client,
    () => netOps.runForcePush(netOps.netOpsContext(client, state)),
  );
}
