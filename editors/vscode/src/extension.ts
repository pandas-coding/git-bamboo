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
import { takeGraphContextTarget, type GraphContextCommit } from './graphSession';
import { HeadContentProvider } from './headContent';
import type { InitializeResult, RepoState } from './types';
import { errorMessage } from './util';

let activeClient: EngineClient | undefined;
let activeState: RepoState | undefined;
let activeCommitView: CommitViewProvider | undefined;

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
    openGraphPanel(context, activeClient, activeState);
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
  // View title "Refresh Changes" — also a palette entry; registered with the
  // other commands (unconditionally) so it never dead-ends without a repo.
  registerCommand(context, 'gitBamboo.refreshChanges', () =>
    activeCommitView ? activeCommitView.refresh() : noSessionWarning());

  // Commit-row context menu (native webview/context menus): the webview
  // reports the right-clicked commit right before the menu opens, and
  // takeGraphContextTarget() hands it (plus its engine client) to whichever
  // menu command the user picks. Palette entries are hidden (`when: false`) —
  // without a pending target these commands have nothing to act on.
  registerCommand(context, 'gitBamboo.graph.copySha', () =>
    withGraphTarget(async ({ commit }) => {
      await vscode.env.clipboard.writeText(commit.id);
      vscode.window.setStatusBarMessage(`Git Bamboo: copied ${commit.id.slice(0, 8)}`, 3000);
    }));
  registerCommand(context, 'gitBamboo.graph.copyMessage', () =>
    withGraphTarget(async ({ commit }) => {
      await vscode.env.clipboard.writeText(commit.message);
      vscode.window.setStatusBarMessage('Git Bamboo: copied commit message', 3000);
    }));
  registerCommand(context, 'gitBamboo.graph.createBranchAt', () =>
    withGraphTarget(async ({ client, commit }) => {
      const name = await promptBranchName(`Create branch at ${commit.id.slice(0, 8)}`);
      if (!name) return;
      try {
        await createBranchAt(client, commit.id, name);
      } catch (err) {
        vscode.window.showErrorMessage(`Git Bamboo: create branch failed — ${errorMessage(err)}`);
      }
    }));
  // "Checkout" = create a branch at the commit, then switch to it (the
  // branch-based form needs no confirmation: nothing is discarded, and
  // auto-stash protects a dirty worktree). Bare detached checkout stays in
  // phase B per the UX polish plan.
  registerCommand(context, 'gitBamboo.graph.checkoutNewBranch', () =>
    withGraphTarget(async ({ client, commit }) => {
      const name = await promptBranchName(`Checkout at ${commit.id.slice(0, 8)} (new branch)`);
      if (!name) return;
      try {
        await createBranchAt(client, commit.id, name);
      } catch (err) {
        // A pre-existing branch with this name may point at a different
        // commit, so silently switching to it would betray the right-clicked
        // commit — ask instead of aborting outright.
        const msg = errorMessage(err);
        if (!/already exists/i.test(msg)) {
          vscode.window.showErrorMessage(`Git Bamboo: create branch failed — ${msg}`);
          return;
        }
        const choice = await vscode.window.showWarningMessage(
          `Git Bamboo: branch ${name} already exists (it may point at a different commit).`,
          'Switch to it',
        );
        if (choice !== 'Switch to it') return;
      }
      try {
        await switchToBranch(client, name);
      } catch (err) {
        // Two-step flow, not atomic: be explicit that step 1 did land, and
        // offer a one-click retry (the failure may be transient).
        const retry = await vscode.window.showWarningMessage(
          `Git Bamboo: branch ${name} exists but was not checked out — ${errorMessage(err)}`,
          'Switch Now',
        );
        if (retry !== 'Switch Now') return;
        try {
          await switchToBranch(client, name);
        } catch (retryErr) {
          vscode.window.showErrorMessage(
            `Git Bamboo: switch failed — ${errorMessage(retryErr)}`,
          );
        }
      }
    }));

  // Commit-view file-row context menu (native webview/context menus): the
  // webview reports the right-clicked file right before the menu opens, and
  // the provider parks it for whichever menu command the user picks.
  // Palette entries are hidden (`when: false`) — without a pending target
  // these commands have nothing to act on.
  registerCommand(context, 'gitBamboo.commitView.openChanges', async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    if (!(await activeCommitView.runContextCommand('openChanges'))) noCommitTargetWarning();
  });
  registerCommand(context, 'gitBamboo.commitView.stageFile', async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    if (!(await activeCommitView.runContextCommand('stageFile'))) noCommitTargetWarning();
  });
  registerCommand(context, 'gitBamboo.commitView.unstageFile', async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    if (!(await activeCommitView.runContextCommand('unstageFile'))) noCommitTargetWarning();
  });
  registerCommand(context, 'gitBamboo.commitView.copyPath', async () => {
    const target = activeCommitView?.takeContextTarget();
    if (!target) {
      noCommitTargetWarning();
      return;
    }
    await vscode.env.clipboard.writeText(target.path);
    vscode.window.setStatusBarMessage(`Git Bamboo: copied ${target.path}`, 3000);
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
  const commitView = new CommitViewProvider(context, client, folder.uri);
  activeCommitView = commitView;
  context.subscriptions.push(commitView);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(CommitViewProvider.viewId, commitView, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

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
  activeCommitView = undefined;
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

/** Shown when a graph context-menu command runs with no pending target
 *  (e.g. invoked programmatically). Palette entries are `when: false`, so
 *  this is a safety net, not the normal path. */
function noGraphTargetWarning(): void {
  vscode.window.showWarningMessage(
    'Git Bamboo: no commit selected — right-click a commit row in the graph first.',
  );
}

/** Commit-view counterpart of noGraphTargetWarning (Copy Path has no
 *  session guard of its own, so this fires without a repo too). */
function noCommitTargetWarning(): void {
  vscode.window.showWarningMessage(
    'Git Bamboo: no file selected — right-click a file in the Commit view first.',
  );
}

/** Runs a graph context-menu handler against the pending right-click
 *  target, warning (safety net) when none is parked. */
async function withGraphTarget(
  fn: (target: { client: EngineClient; commit: GraphContextCommit }) => Promise<void>,
): Promise<void> {
  const target = takeGraphContextTarget();
  if (!target) {
    noGraphTargetWarning();
    return;
  }
  await fn(target);
}

/** Creates a branch at a commit and reports success (refsChanged from the
 *  engine refreshes the graph pills automatically). Throws on failure. */
async function createBranchAt(client: EngineClient, id: string, name: string): Promise<void> {
  await client.request('createBranch', { name, base: id });
  vscode.window.showInformationMessage(`Git Bamboo: created branch ${name}`);
}

/** Switches to a branch (auto-stash protects a dirty worktree) and reports
 *  success. Throws on failure. */
async function switchToBranch(client: EngineClient, name: string): Promise<void> {
  await client.request('switchBranch', { name, auto_stash: true });
  vscode.window.showInformationMessage(`Git Bamboo: checked out ${name}`);
}

/** Mirrors the engine's ref validation (validate_ref_arg in
 *  write_queue.rs) so malformed names are rejected at the input box
 *  instead of costing an engine round-trip and a raw error message. */
function validateBranchName(value: string): string | undefined {
  const name = value.trim();
  if (!name) return 'Name is required';
  if (name.length > 512) return 'Name is too long (max 512 characters)';
  if (/^[-/]/.test(name)) return "Name cannot start with '-' or '/'";
  if (name.endsWith('/') || name.endsWith('.')) return "Name cannot end with '/' or '.'";
  if (name.includes('..') || name.includes('//') || name.includes('@')) {
    return "Name cannot contain '..', '//', or '@'";
  }
  if (name.endsWith('.lock')) return "Name cannot end with '.lock'";
  if (/[\u0000-\u001f\u007f ~^:?*[\\]/.test(name)) {
    return 'Name contains a character that is not allowed in refs';
  }
  for (const part of name.split('/')) {
    if (part.startsWith('.') || part.endsWith('.')) {
      return "Each path segment must not start or end with '.'";
    }
  }
  return undefined;
}

/** Input box for context-menu branch creation; trimmed name or undefined. */
async function promptBranchName(prompt: string): Promise<string | undefined> {
  const value = await vscode.window.showInputBox({
    prompt,
    validateInput: validateBranchName,
  });
  return value?.trim() || undefined; // cancelled or invalid/empty
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
