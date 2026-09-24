/**
 * Bridges engine getStatus() results to VS Code's native SCM view:
 * staged / unstaged / untracked resource groups, per-resource stage &
 * unstage commands, commit via the SCM input box, and diff-on-click
 * against the HEAD revision (via a TextDocumentContentProvider).
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { EngineClient } from './engineClient';
import type { StatusItem } from './types';
import { errorMessage } from './util';

export const ENGINE_SCHEME = 'git-workbench';

interface WorkbenchResourceState extends vscode.SourceControlResourceState {
  resourceUri: vscode.Uri;
  /** The raw engine status string — usable as a `when`-clause context key. */
  contextValue: string;
  command?: vscode.Command;
  decorations?: vscode.SourceControlResourceDecorations;
}

export class WorkbenchSCMProvider implements vscode.Disposable {
  readonly sourceControl: vscode.SourceControl;
  private readonly stagedGroup: vscode.SourceControlResourceGroup;
  private readonly unstagedGroup: vscode.SourceControlResourceGroup;
  private readonly untrackedGroup: vscode.SourceControlResourceGroup;
  private readonly disposables: vscode.Disposable[] = [];
  private refreshTimer: NodeJS.Timeout | undefined;
  private refreshQueued = false;
  private refreshing = false;

  constructor(
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {
    this.sourceControl = vscode.scm.createSourceControl('git-workbench', 'Git Workbench', root);

    this.sourceControl.acceptInputCommand = { command: 'gitWorkbench.commit', title: 'Commit', arguments: [] };

    this.stagedGroup = this.sourceControl.createResourceGroup('staged', 'Staged Changes');
    this.unstagedGroup = this.sourceControl.createResourceGroup('unstaged', 'Changes');
    this.untrackedGroup = this.sourceControl.createResourceGroup('untracked', 'Untracked Changes');
    for (const group of [this.stagedGroup, this.unstagedGroup, this.untrackedGroup]) {
      group.hideWhenEmpty = true;
    }

    // HEAD revision content for diffs: "git-workbench:/abs/path" URIs.
    const headContentProvider: vscode.TextDocumentContentProvider = {
      provideTextDocumentContent: (uri) => this.readHeadRevision(uri),
    };
    this.disposables.push(vscode.workspace.registerTextDocumentContentProvider(ENGINE_SCHEME, headContentProvider));

    this.registerCommands();
    this.disposables.push(this.sourceControl);
    void this.doRefresh();
  }

  dispose(): void {
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
  }
  /** Debounced refresh entry point (engine coalesces fs events at 50ms already). */
  refresh(): void {
    if (this.refreshTimer) clearTimeout(this.refreshTimer);
    this.refreshTimer = setTimeout(() => void this.doRefresh(), 50);
  }

  private registerCommands(): void {
    this.disposables.push(
      vscode.commands.registerCommand('gitWorkbench.commit', (message?: string) => this.commit(message)),
      vscode.commands.registerCommand('gitWorkbench.commitAmend', () => this.commitAmend()),
      vscode.commands.registerCommand('gitWorkbench.stage', (...states: vscode.SourceControlResourceState[]) =>
        this.changePaths('stage', states),
      ),
      vscode.commands.registerCommand('gitWorkbench.unstage', (...states: vscode.SourceControlResourceState[]) =>
        this.changePaths('unstage', states),
      ),
      vscode.commands.registerCommand('gitWorkbench.openResource', (state: vscode.SourceControlResourceState) =>
        this.openResource(state),
      ),
    );
  }

  private async doRefresh(): Promise<void> {
    if (this.refreshing) {
      this.refreshQueued = true;
      return;
    }
    this.refreshing = true;
    try {
      const items = await this.client.request<StatusItem[]>('getStatus', {});
      this.applyStatus(Array.isArray(items) ? items : []);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: status refresh failed — ${errorMessage(err)}`);
    } finally {
      this.refreshing = false;
      if (this.refreshQueued) {
        this.refreshQueued = false;
        this.refresh();
      }
    }
  }

  private applyStatus(items: StatusItem[]): void {
    const staged: WorkbenchResourceState[] = [];
    const unstaged: WorkbenchResourceState[] = [];
    const untracked: WorkbenchResourceState[] = [];
    for (const item of items) {
      if (item.status === 'ignored') continue; // hidden from the SCM view
      const state = this.makeState(item);
      // Bucket on the engine's staged flag: tree-index (staged) changes go
      // to the staged group regardless of their status code.
      if (item.staged) staged.push(state);
      else if (item.status === 'untracked') untracked.push(state);
      else unstaged.push(state);
    }
    this.stagedGroup.resourceStates = staged;
    this.unstagedGroup.resourceStates = unstaged;
    this.untrackedGroup.resourceStates = untracked;
  }

  private makeState(item: StatusItem): WorkbenchResourceState {
    const uri = this.resolveRepoPath(item.path);
    const state: WorkbenchResourceState = {
      resourceUri: uri,
      contextValue: item.status,
      command: undefined,
      decorations:
        item.old_path !== undefined
          ? { tooltip: `Renamed from ${item.old_path}` }
          : { tooltip: `${item.status}: ${item.path}` },
    };
    state.command = { command: 'gitWorkbench.openResource', title: 'Open Changes', arguments: [state] };
    return state;
  }

  /** Resolve a repo-relative (slash-separated) path against the workspace root. */
  private resolveRepoPath(relativePath: string): vscode.Uri {
    return vscode.Uri.joinPath(this.root, ...relativePath.split('/'));
  }

  /** Repo-relative path (forward slashes) for engine write requests. */
  private toRelativePath(uri: vscode.Uri): string {
    return path.relative(this.root.fsPath, uri.fsPath).replace(/\\/g, '/');
  }

  private async commit(message?: string): Promise<void> {
    const text = (message ?? this.sourceControl.inputBox.value).trim();
    if (!text) {
      vscode.window.showWarningMessage('Git Workbench: commit message is empty');
      return;
    }
    try {
      await this.client.request('commit', { message: text, amend: false });
      this.sourceControl.inputBox.value = '';
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: commit failed — ${errorMessage(err)}`);
    }
  }

  /**
   * Amend Last Commit: a non-empty SCM input box becomes the new commit
   * message; an empty one amends keeping the existing message (the
   * engine's CommitMsg { message, amend: true } path runs
   * `git commit --amend --no-edit`, so the message is only used when the
   * engine chooses to apply it).
   */
  private async commitAmend(): Promise<void> {
    const text = this.sourceControl.inputBox.value.trim();
    try {
      await this.client.request('commit', { message: text, amend: true });
      this.sourceControl.inputBox.value = '';
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: amend failed — ${errorMessage(err)}`);
    }
  }

  private async changePaths(
    operation: 'stage' | 'unstage',
    states: vscode.SourceControlResourceState[],
  ): Promise<void> {
    if (states.length === 0) return;
    const paths = states.map((state) => this.toRelativePath(state.resourceUri));
    try {
      await this.client.request(operation, { paths });
      await this.doRefresh();
    } catch (err) {
      vscode.window.showErrorMessage(`Git Workbench: ${operation} failed — ${errorMessage(err)}`);
    }
  }

  private async openResource(state: vscode.SourceControlResourceState): Promise<void> {
    const uri = state.resourceUri;
    const status = (state as WorkbenchResourceState).contextValue;
    // Untracked/added files have no HEAD revision to diff against.
    if (status === 'untracked' || status === 'added' || status === 'ignored') {
      await vscode.commands.executeCommand('vscode.open', uri);
      return;
    }
    const left = uri.with({ scheme: ENGINE_SCHEME, query: 'HEAD' });
    const title = `${path.basename(uri.fsPath)} (Working Tree — HEAD)`;
    await vscode.commands.executeCommand('vscode.diff', left, uri, title);
  }

  /** Engine `getBlob` RPC: HEAD-revision content for diff side-by-sides. */
  private async readHeadRevision(uri: vscode.Uri): Promise<string> {
    const relativePath = this.toRelativePath(uri);
    try {
      return await this.client.getBlob('HEAD', relativePath);
    } catch (err) {
      console.error(`[git-workbench] getBlob HEAD:${relativePath} failed:`, err);
      return ''; // empty diff side, like the old git-CLI fallback
    }
  }
}
