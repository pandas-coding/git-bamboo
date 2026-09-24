/**
 * Extension entry point: finds the workspace repo (T1: single repo — the
 * first folder containing .git), spawns the Rust engine process, wires
 * engine notifications to SCM refresh / graph invalidation, and registers
 * all contributed commands.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import * as commands from './commands';
import { EngineClient } from './engineClient';
import { GraphWebviewProvider } from './graphWebview';
import { WorkbenchSCMProvider } from './scmProvider';
import type { InitializeResult, RepoState } from './types';
import { errorMessage } from './util';

let activeClient: EngineClient | undefined;
let activeState: RepoState | undefined;

/** Shown when a command runs without a repository session (no repo folder
 *  open, or the engine failed to start): silent no-ops are terrible UX. */
function noSessionWarning(): void {
  vscode.window.showWarningMessage(
    'Git Workbench: no repository session. Open a folder containing a git ' +
      'repository in this window (WSL side, e.g. ~/my-repo) — the workbench ' +
      'activates automatically once a repo folder is open.',
  );
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  // Commands are registered unconditionally (even before a repo is open) so
  // palette entries never dead-end silently; handlers guard on the session.
  registerCommand(context, 'gitWorkbench.openGraph', async () => {
    if (!activeClient || !activeState) {
      noSessionWarning();
      return;
    }
    try {
      await vscode.commands.executeCommand('gitWorkbench.graph.focus');
    } catch {
      vscode.window.showErrorMessage(
        'Git Workbench: graph view unavailable — the engine did not start for this workspace.',
      );
    }
  });
  registerCommand(context, 'gitWorkbench.undo', () =>
    activeClient ? commands.undoLast(activeClient) : noSessionWarning());
  registerCommand(context, 'gitWorkbench.switchBranch', () =>
    activeClient ? commands.switchBranch(activeClient) : noSessionWarning());
  registerCommand(context, 'gitWorkbench.createBranch', () =>
    activeClient ? commands.createBranch(activeClient) : noSessionWarning());
  registerCommand(context, 'gitWorkbench.deleteBranch', () =>
    activeClient && activeState
      ? commands.deleteBranch(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitWorkbench.fetch', () =>
    activeClient ? commands.fetchRemotes(activeClient) : noSessionWarning());
  registerCommand(context, 'gitWorkbench.pull', () =>
    activeClient && activeState
      ? commands.pull(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitWorkbench.push', () =>
    activeClient && activeState
      ? commands.push(activeClient, activeState)
      : noSessionWarning());
  // gitWorkbench.commit / stage / unstage / openResource are registered by
  // WorkbenchSCMProvider (they only make sense with a live session).

  // T1 scope: single repository — use the first workspace folder with a .git.
  const folder = vscode.workspace.workspaceFolders?.find((f) =>
    fs.existsSync(path.join(f.uri.fsPath, '.git')),
  );
  if (!folder) return;

  const binaryPath = resolveEngineBinary(context);
  if (!binaryPath) {
    vscode.window.showErrorMessage(
      'Git Workbench: engine binary not found. Expected server/git-workbench-engine ' +
        '(bundled) or target/release|debug/git-workbench-engine (dev build).',
    );
    return;
  }

  let client: EngineClient;
  let init: InitializeResult;
  try {
    client = new EngineClient(binaryPath, ['--parent-pid', String(process.pid)]);
    init = await client.request<InitializeResult>('initialize', { repo_path: folder.uri.fsPath });
  } catch (err) {
    vscode.window.showErrorMessage(`Git Workbench: failed to start engine — ${errorMessage(err)}`);
    return;
  }
  activeClient = client;

  const state: RepoState = {
    epoch: init.epoch,
    currentBranch: undefined,
  };
  activeState = state;

  const scm = new WorkbenchSCMProvider(client, folder.uri);
  context.subscriptions.push(scm);

  const graph = new GraphWebviewProvider(context, client, state);
  context.subscriptions.push(graph);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(GraphWebviewProvider.viewId, graph, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );
  // graphInvalidated is handled inside GraphWebviewProvider (it owns its
  // own engine subscription so dispose() can unhook it).

  // TODO(ahead-behind): showing ahead/behind counts needs upstream tracking
  // info, which the engine does not expose yet.
  const branchItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
  context.subscriptions.push(branchItem);

  /** Refreshes current branch + status bar from the engine's getHead RPC. */
  const refreshHead = async (): Promise<void> => {
    try {
      const { head, branch } = await client.getHead();
      state.currentBranch = branch ?? undefined;
      branchItem.text = `$(git-branch) ${branch ?? head.slice(0, 8)}`;
      branchItem.tooltip = branch
        ? `Git Workbench: on branch ${branch}`
        : `Git Workbench: detached HEAD at ${head}`;
      branchItem.show();
    } catch (err) {
      console.error('[git-workbench] getHead failed:', err);
    }
  };
  await refreshHead();

  // Engine notifications drive all UI refreshes.
  onNotification(client, context, 'refsChanged', () => scm.refresh());
  onNotification(client, context, 'worktreeChanged', () => scm.refresh());
  onNotification(client, context, 'indexChanged', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    scm.refresh();
  });
  onNotification(client, context, 'headChanged', () => {
    void refreshHead();
    scm.refresh();
  });

  // Enables commandPalette `when: git-workbench:active` visibility rules.
  await vscode.commands.executeCommand('setContext', 'git-workbench:active', true);
}

export async function deactivate(): Promise<void> {
  await activeClient?.dispose();
  activeClient = undefined;
  activeState = undefined;
}

/**
 * Binary resolution: the bundled `server/` copy first (production order
 * unchanged), then dev fallbacks — the repo checkout's target/debug and
 * target/release builds, relative to the extension root — so dev runs
 * work without copying the binary.
 */
function resolveEngineBinary(context: vscode.ExtensionContext): string | undefined {
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const name = `git-workbench-engine${suffix}`;
  const bundled = context.asAbsolutePath(path.join('server', name));
  if (fs.existsSync(bundled)) return bundled;

  const devCandidates = [
    context.asAbsolutePath(path.join('..', 'target', 'debug', name)),
    context.asAbsolutePath(path.join('..', 'target', 'release', name)),
  ];
  const dev = devCandidates.find((candidate) => fs.existsSync(candidate));
  if (dev) {
    console.warn(`[git-workbench] engine binary not bundled; using dev build at ${dev}`);
    return dev;
  }
  return undefined;
}

function numberField(params: Record<string, unknown>, key: string, fallback: number): number {
  const value = params[key];
  return typeof value === 'number' ? value : fallback;
}

function onNotification(
  client: EngineClient,
  context: vscode.ExtensionContext,
  method: string,
  handler: (params: Record<string, unknown>) => void,
): void {
  context.subscriptions.push(client.onNotification(method, handler));
}

/** Registers a command with a uniform error surface. */
function registerCommand(
  context: vscode.ExtensionContext,
  command: string,
  callback: (...args: unknown[]) => unknown,
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand(command, async (...args: unknown[]) => {
      try {
        await callback(...args);
      } catch (err) {
        vscode.window.showErrorMessage(`Git Workbench: ${errorMessage(err)}`);
      }
    }),
  );
}
