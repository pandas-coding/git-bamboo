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

export async function activate(context: vscode.ExtensionContext): Promise<void> {
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
    currentBranch: readCurrentBranch(folder.uri.fsPath),
  };

  const scm = new WorkbenchSCMProvider(client, folder.uri);
  context.subscriptions.push(scm);

  const graph = new GraphWebviewProvider(context, client, state);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(GraphWebviewProvider.viewId, graph, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

  // Engine notifications drive all UI refreshes.
  onNotification(client, context, 'refsChanged', () => scm.refresh());
  onNotification(client, context, 'worktreeChanged', () => scm.refresh());
  onNotification(client, context, 'indexChanged', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    scm.refresh();
  });
  onNotification(client, context, 'headChanged', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    const newHead = params['new_head'];
    if (typeof newHead === 'string') state.currentBranch = branchFromHead(newHead);
    scm.refresh();
  });
  onNotification(client, context, 'graphInvalidated', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    graph.invalidate(state.epoch);
  });

  registerCommand(context, 'gitWorkbench.openGraph', () =>
    vscode.commands.executeCommand('gitWorkbench.graph.focus'),
  );
  registerCommand(context, 'gitWorkbench.undo', () => commands.undoLast(client));
  registerCommand(context, 'gitWorkbench.switchBranch', () => commands.switchBranch(client));
  registerCommand(context, 'gitWorkbench.createBranch', () => commands.createBranch(client));
  registerCommand(context, 'gitWorkbench.deleteBranch', () => commands.deleteBranch(client, state));
  registerCommand(context, 'gitWorkbench.fetch', () => commands.fetchRemotes(client));
  registerCommand(context, 'gitWorkbench.pull', () => commands.pull(client, state));
  registerCommand(context, 'gitWorkbench.push', () => commands.push(client, state));
  // gitWorkbench.commit / stage / unstage / openResource are registered by WorkbenchSCMProvider.

  // Enables commandPalette `when: git-workbench:active` visibility rules.
  await vscode.commands.executeCommand('setContext', 'git-workbench:active', true);
}

export async function deactivate(): Promise<void> {
  await activeClient?.dispose();
  activeClient = undefined;
}

/**
 * Dev-first binary resolution: bundled server/ copy, then the repo's
 * target/release and target/debug builds (context.asAbsolutePath is
 * relative to the extension install root).
 */
function resolveEngineBinary(context: vscode.ExtensionContext): string | undefined {
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const name = `git-workbench-engine${suffix}`;
  const candidates = [
    context.asAbsolutePath(path.join('server', name)),
    context.asAbsolutePath(path.join('..', '..', 'target', 'release', name)),
    context.asAbsolutePath(path.join('..', '..', 'target', 'debug', name)),
  ];
  return candidates.find((candidate) => fs.existsSync(candidate));
}

function readCurrentBranch(root: string): string | undefined {
  try {
    const head = fs.readFileSync(path.join(root, '.git', 'HEAD'), 'utf8');
    return branchFromHead(head);
  } catch {
    return undefined; // detached HEAD, worktree .git file, or unreadable
  }
}

/** "ref: refs/heads/main" -> "main"; detached HEAD -> undefined. */
function branchFromHead(rawHead: string): string | undefined {
  const match = /^ref:\s*refs\/heads\/(.+)$/m.exec(rawHead.trim());
  return match ? match[1] : undefined;
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
