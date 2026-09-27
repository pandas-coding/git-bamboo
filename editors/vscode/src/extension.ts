/**
 * Extension entry point: finds the workspace repo (T1: single repo — the
 * first folder containing .git), spawns the Rust engine process, wires
 * engine notifications to Commit view refresh / graph invalidation, and
 * registers all contributed commands. All UI lives in the gitBamboo
 * activity bar container (Commit webview + Commit Graph webview) and the
 * editor-area graph panel — the SCM API is not used.
 */
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as vscode from 'vscode';
import * as commands from './commands';
import { CommitViewProvider } from './commitView';
import { EngineClient } from './engineClient';
import { GraphWebviewProvider } from './graphWebview';
import { openGraphPanel, registerGraphPanelSerializer } from './graphPanel';
import { HeadContentProvider } from './headContent';
import type { InitializeResult, RepoState } from './types';
import { errorMessage } from './util';

let activeClient: EngineClient | undefined;
let activeState: RepoState | undefined;

/** Shown when a command runs without a repository session (no repo folder
 *  open, or the engine failed to start): silent no-ops are terrible UX. */
function noSessionWarning(): void {
  vscode.window.showWarningMessage(
    'Git Bamboo: no repository session. Open a folder containing a git ' +
      'repository in this window (WSL side, e.g. ~/my-repo) — the extension ' +
      'activates automatically once a repo folder is open.',
  );
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  // Commands are registered unconditionally (even before a repo is open) so
  // palette entries never dead-end silently; handlers guard on the session.
  registerCommand(context, 'gitBamboo.openGraph', async () => {
    if (!activeClient || !activeState) {
      noSessionWarning();
      return;
    }
    try {
      openGraphPanel(context, activeClient, activeState);
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: opening the graph panel failed — ${errorMessage(err)}`,
      );
    }
  });
  registerCommand(context, 'gitBamboo.undo', () =>
    activeClient ? commands.undoLast(activeClient) : noSessionWarning());
  registerCommand(context, 'gitBamboo.switchBranch', () =>
    activeClient ? commands.switchBranch(activeClient) : noSessionWarning());
  registerCommand(context, 'gitBamboo.createBranch', () =>
    activeClient ? commands.createBranch(activeClient) : noSessionWarning());
  registerCommand(context, 'gitBamboo.deleteBranch', () =>
    activeClient && activeState
      ? commands.deleteBranch(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.fetch', () =>
    activeClient ? commands.fetchRemotes(activeClient) : noSessionWarning());
  registerCommand(context, 'gitBamboo.pull', () =>
    activeClient && activeState
      ? commands.pull(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.push', () =>
    activeClient && activeState
      ? commands.push(activeClient, activeState)
      : noSessionWarning());
  // Palette "Commit" just focuses the Commit view — the message box and
  // commit button live there.
  registerCommand(context, 'gitBamboo.commit', async () => {
    if (!activeClient || !activeState) {
      noSessionWarning();
      return;
    }
    try {
      await vscode.commands.executeCommand('gitBamboo.commit.focus');
    } catch {
      vscode.window.showErrorMessage(
        'Git Bamboo: Commit view unavailable — the engine did not start for this workspace.',
      );
    }
  });
  // Palette amend has no input box to source a message from, so it is
  // always the engine's `--amend --no-edit` path (empty message).
  registerCommand(context, 'gitBamboo.commitAmend', async () => {
    if (!activeClient || !activeState) {
      noSessionWarning();
      return;
    }
    try {
      await activeClient.request('commit', { message: '', amend: true });
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: amend failed — ${errorMessage(err)}`);
    }
  });

  // T1 scope: single repository — use the first workspace folder with a .git.
  const folder = vscode.workspace.workspaceFolders?.find((f) =>
    fs.existsSync(path.join(f.uri.fsPath, '.git')),
  );
  if (!folder) return;

  const binaryPath = resolveEngineBinary(context);
  if (!binaryPath) {
    vscode.window.showErrorMessage(
      'Git Bamboo: engine binary not found. Expected server/bamboo-engine ' +
        '(bundled) or target/release|debug/bamboo-engine (dev build).',
    );
    return;
  }

  let client: EngineClient;
  let init: InitializeResult;
  try {
    client = new EngineClient(binaryPath, ['--parent-pid', String(process.pid)]);
    init = await client.request<InitializeResult>('initialize', { repo_path: folder.uri.fsPath });
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: failed to start engine — ${errorMessage(err)}`);
    return;
  }
  activeClient = client;

  const state: RepoState = {
    epoch: init.epoch,
    currentBranch: undefined,
  };
  activeState = state;

  // HEAD-revision content provider backing the Commit view's diffs.
  const headContent = new HeadContentProvider(client, folder.uri);
  context.subscriptions.push(headContent);

  // Commit view: message box + change list + commit/amend, one webview.
  const commitView = new CommitViewProvider(context, client, folder.uri, state);
  context.subscriptions.push(commitView);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(CommitViewProvider.viewId, commitView, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );
  // View title "Refresh Changes" (also a valid palette entry once a
  // session is live — the command only exists after the view is wired).
  registerCommand(context, 'gitBamboo.refreshChanges', () => commitView.refresh());

  // Sidebar commit graph (compact mode) + editor-area graph panel (full
  // width mode) share the same webview bundle via GraphSession.
  const graph = new GraphWebviewProvider(context, client, state);
  context.subscriptions.push(graph);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(GraphWebviewProvider.viewId, graph, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );
  registerGraphPanelSerializer(context, client, state);

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
        ? `Git Bamboo: on branch ${branch}`
        : `Git Bamboo: detached HEAD at ${head}`;
      branchItem.show();
    } catch (err) {
      console.error('[git-bamboo] getHead failed:', err);
    }
  };
  await refreshHead();

  // Engine notifications drive all UI refreshes.
  onNotification(client, context, 'refsChanged', () => commitView.refresh());
  onNotification(client, context, 'worktreeChanged', () => commitView.refresh());
  onNotification(client, context, 'indexChanged', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    commitView.refresh();
  });
  onNotification(client, context, 'headChanged', () => {
    void refreshHead();
    commitView.refresh();
  });

  // Enables commandPalette `when: git-bamboo:active` visibility rules.
  await vscode.commands.executeCommand('setContext', 'git-bamboo:active', true);
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
  const name = `bamboo-engine${suffix}`;
  const bundled = context.asAbsolutePath(path.join('server', name));
  if (fs.existsSync(bundled)) return bundled;

  const devCandidates = [
    context.asAbsolutePath(path.join('..', '..', 'target', 'debug', name)),
    context.asAbsolutePath(path.join('..', '..', 'target', 'release', name)),
  ];
  const dev = devCandidates.find((candidate) => fs.existsSync(candidate));
  if (dev) {
    console.warn(`[git-bamboo] engine binary not bundled; using dev build at ${dev}`);
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
        vscode.window.showErrorMessage(`Git Bamboo: ${errorMessage(err)}`);
      }
    }),
  );
}
