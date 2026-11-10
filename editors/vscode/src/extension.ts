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
import {
  BLAME_ANNOTATE_PREVIOUS_COMMAND,
  BLAME_DIFF_LINE_COMMAND,
  BLAME_JUMP_TO_COMMIT_COMMAND,
  BLAME_SHOW_COMMIT_COMMAND,
  BLAME_TOGGLE_COMMAND,
  BlameController,
} from './blame';
import { ConflictFlow } from './conflictFlow';
import { CommitViewProvider } from './commitView';
import { EngineClient } from './engineClient';
import { GraphWebviewProvider } from './graphWebview';
import { openGraphPanel, registerGraphPanelSerializer } from './graphPanel';
import { openCommitDetailPanel } from './commitDetail';
import { openRebasePlanPanel, registerRebasePanelSerializer, runRebasePanelCommand } from './rebasePanel';
import { applyGraphFilter, currentGraphFilter, takeGraphContextTarget, type GraphContextCommit } from './graphSession';
import { HeadContentProvider } from './headContent';
import { HunkStagingController, STAGE_HUNK_COMMAND, UNSTAGE_HUNK_COMMAND } from './hunkStaging';
import { trustedSetting, withNetOpGuard } from './netOps';
import type { GraphFilter, InitializeResult, RepoState } from './types';
import { BRANCH_STATUS_DEBOUNCE_MS, debounce, errorMessage } from './util';

let activeClient: EngineClient | undefined;
let activeState: RepoState | undefined;
let activeCommitView: CommitViewProvider | undefined;
let activeConflictFlow: ConflictFlow | undefined;
let activeBlame: BlameController | undefined;

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
    if (!activeClient || !activeState || !activeConflictFlow) {
      noSessionWarning();
      return;
    }
    openGraphPanel(context, activeClient, activeState, activeConflictFlow);
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
    activeClient && activeState
      ? commands.fetchRemotes(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.pull', () =>
    activeClient && activeState
      ? commands.pull(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.pullWithStrategy', () =>
    activeClient && activeState
      ? commands.pullWithStrategy(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.push', () =>
    activeClient && activeState
      ? commands.push(activeClient, activeState)
      : noSessionWarning());
  registerCommand(context, 'gitBamboo.forcePush', () =>
    activeClient && activeState
      ? commands.forcePush(activeClient, activeState)
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

  // View title "Discard All {bucket}" — acts on the Commit view's current
  // status items; the IRREVERSIBLE modal confirmation lives in the provider.
  const discardAll = (bucket: 'changes' | 'untracked', label: string) => async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    if (!(await activeCommitView.discardBucket(bucket))) {
      vscode.window.showInformationMessage(`Git Bamboo: no ${label} to discard`);
    }
  };
  registerCommand(
    context,
    'gitBamboo.commitView.discardAllChanges',
    discardAll('changes', 'changes'),
  );
  registerCommand(
    context,
    'gitBamboo.commitView.discardAllUntracked',
    discardAll('untracked', 'untracked files'),
  );

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
  // "Rebase onto this commit…": the entry into the rebase plan panel.
  // The plan is fetched here (empty → "nothing to rebase" info, no panel);
  // the panel itself gets the host-held onto as its trust anchor. Only
  // valid from the graph context menu (palette entry is `when: false`).
  registerCommand(context, 'gitBamboo.graph.rebaseOnto', () =>
    withGraphTarget(async ({ client, commit }) => {
      if (!activeConflictFlow) {
        noSessionWarning();
        return;
      }
      // Check if a rebase is already in progress — if so, open the panel
      // directly (it will dock) instead of fetching a new plan (which
      // fails when HEAD is detached).
      try {
        const status = await client.getRebaseStatus();
        if (status.state !== 'idle') {
          openRebasePlanPanel(context, client, activeConflictFlow, commit.id);
          return;
        }
      } catch {
        // Status unreadable: fall through — the plan fetch surfaces its
        // own error banner if it fails.
      }
      let plan;
      try {
        plan = await client.getRebasePlan(commit.id);
      } catch (err) {
        vscode.window.showErrorMessage(`Git Bamboo: rebase plan failed — ${errorMessage(err)}`);
        return;
      }
      if (plan.commits.length === 0) {
        vscode.window.showInformationMessage(
          `Git Bamboo: nothing to rebase — the current branch is already based on ${commit.id.slice(0, 8)}`,
        );
        return;
      }
      openRebasePlanPanel(context, client, activeConflictFlow, commit.id);
    }));
  // Rebase-panel palette commands (hidden — `when: false` in
  // package.json): thin wrappers over the open panel so keybindings and
  // programmatic invocation have a stable surface. `execute` asks the
  // webview to run its confirmation modal; the user entry point is the
  // graph context menu.
  for (const op of ['execute', 'continue', 'skip', 'abort', 'close'] as const) {
    registerCommand(context, `gitBamboo.rebasePanel.${op}`, () => {
      if (!runRebasePanelCommand(op)) {
        vscode.window.showInformationMessage('Git Bamboo: no rebase panel is open.');
      }
    });
  }
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
  // Palette "Filter Commit Graph…": one condition per invocation (multi-
  //  condition filters are built in the graph toolbar's chips), composed
  //  onto the current filter so picking a different type stacks rather
  //  than replaces. Same session API the toolbar input uses, so every
  //  open graph surface of the repo updates together.
  registerCommand(context, 'gitBamboo.graph.filter', async () => {
    if (!activeClient) {
      noSessionWarning();
      return;
    }
    const current = currentGraphFilter(activeClient);
    if (current === undefined) {
      vscode.window.showInformationMessage(
        'Git Bamboo: open the Commit Graph view first — its filter toolbar lives there.',
      );
      return;
    }
    type FilterField = 'message' | 'author' | 'path';
    const items: { label: string; description: string; field: FilterField | null }[] = [
      { label: 'Message', description: 'Case-insensitive substring of the summary line', field: 'message' },
      { label: 'Author', description: 'Case-insensitive substring of name or email', field: 'author' },
      { label: 'Path', description: 'Case-sensitive substring of a changed path', field: 'path' },
    ];
    if (current) {
      items.unshift({ label: 'Clear Filter', description: 'Remove all conditions', field: null });
    }
    const pick = await vscode.window.showQuickPick(items, {
      placeHolder: 'Filter the commit graph by…',
    });
    if (!pick) return;
    const base: GraphFilter = current ?? { author: null, message: null, path: null };
    if (pick.field === null) {
      applyGraphFilter(activeClient, { author: null, message: null, path: null });
      return;
    }
    const text = await vscode.window.showInputBox({
      title: `Filter commits by ${pick.label.toLowerCase()}`,
      prompt: 'Empty input clears this condition; other conditions are kept.',
      value: base[pick.field] ?? '',
      // Early user feedback only — the engine's `validate_graph_filter`
      // is the single enforcement point. Code points (not UTF-16 units)
      // so the count matches the engine's `chars().count()`.
      validateInput: (value: string) =>
        [...value].length > 256
          ? 'Filter text is limited to 256 characters'
          : /[\x00-\x1f\x7f]/.test(value)
            ? 'Filter text must not contain control characters'
            : '',
    });
    if (text === undefined) return; // cancelled — keep the current filter
    const next: GraphFilter = { author: base.author, message: base.message, path: base.path };
    next[pick.field] = text.trim() || null;
    applyGraphFilter(activeClient, next);
  });
  // Context-menu "Open Commit Details": reuses the parked right-click
  // target (the panel host re-targets an existing panel, so consecutive
  // opens never stack). The commit id was already validated against the
  // full-sha pattern when the target was parked, so it is safe to hand to
  // the detail panel as-is.
  registerCommand(context, 'gitBamboo.graphView.openCommitDetail', () =>
    withGraphTarget(async ({ client, commit }) => {
      // activeState carries the repo root the panel resolves paths against;
      // without a session there is no graph to have right-clicked in the
      // first place, so this guard is a safety net.
      if (!activeState) {
        noSessionWarning();
        return;
      }
      openCommitDetailPanel(context, client, activeState.root, commit.id);
    }));

  // Commit-view file-row context menu (native webview/context menus): the
  // webview reports the right-clicked file right before the menu opens, and
  // the provider parks it for whichever menu command the user picks.
  // Palette entries are hidden (`when: false`) — without a pending target
  // these commands have nothing to act on.
  const commitViewContextActions = [
    ['openChanges', 'openChanges'],
    ['stageFile', 'stageFile'],
    ['unstageFile', 'unstageFile'],
    // discardFile's IRREVERSIBLE confirmation flow lives inside
    // CommitViewProvider.runContextCommand — the handler shape is the same.
    ['discardFile', 'discardFile'],
  ] as const;
  for (const [action, command] of commitViewContextActions) {
    registerCommand(context, `gitBamboo.commitView.${command}`, async () => {
      if (!activeCommitView) {
        noSessionWarning();
        return;
      }
      if (!(await activeCommitView.runContextCommand(action))) noCommitTargetWarning();
    });
  }
  // Conflict-row context actions (conflicts bucket only; runContextCommand
  // bucket-checks). Accept-side buttons pass the UI side — the provider
  // maps it to the resolution the engine accepts for the file's kind.
  const commitViewConflictActions = [
    ['openMergeEditor', 'openMergeEditor', undefined],
    ['resolveConflict', 'resolveOurs', 'ours'],
    ['resolveConflict', 'resolveTheirs', 'theirs'],
  ] as const;
  for (const [action, command, side] of commitViewConflictActions) {
    registerCommand(context, `gitBamboo.commitView.${command}`, async () => {
      if (!activeCommitView) {
        noSessionWarning();
        return;
      }
      if (!(await activeCommitView.runContextCommand(action, side))) noCommitTargetWarning();
    });
  }
  // Net-ops conflict banner "Open Conflict Files": focus the Commit view
  // and expand its conflicts section.
  registerCommand(context, 'gitBamboo.commit.showConflicts', async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    await activeCommitView.showConflicts();
  });
  // Blame (phase 3): gutter annotations + hover + cursor-line CodeLens.
  // Registered unconditionally like every other command so the palette and
  // the editor/title button never dead-end silently; the controller only
  // exists once a repository session is up.
  registerCommand(context, BLAME_TOGGLE_COMMAND, () =>
    activeBlame ? activeBlame.toggle() : noSessionWarning());
  registerCommand(context, BLAME_SHOW_COMMIT_COMMAND, (sha) =>
    activeBlame ? activeBlame.showCommit(sha) : noSessionWarning());
  // The CodeLens "jump to introducing commit" lands on the same commit
  // detail panel as the hover link — a separate command id keeps the two
  // entry points independently keybindable.
  registerCommand(context, BLAME_JUMP_TO_COMMIT_COMMAND, (sha) =>
    activeBlame ? activeBlame.showCommit(sha) : noSessionWarning());
  registerCommand(context, BLAME_DIFF_LINE_COMMAND, (sha, blamePath) =>
    activeBlame ? activeBlame.openCommitDiff(sha, blamePath) : noSessionWarning());
  registerCommand(context, BLAME_ANNOTATE_PREVIOUS_COMMAND, (uri, line) =>
    activeBlame ? activeBlame.annotatePrevious(uri, line) : noSessionWarning());

  registerCommand(context, 'gitBamboo.commitView.copyPath', async () => {
    if (!activeCommitView) {
      noSessionWarning();
      return;
    }
    const target = activeCommitView.takeContextTarget();
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
    // Repo root the graph's commit detail panel (and future consumers)
    // resolve webview-supplied paths against.
    root: folder.uri,
  };
  activeState = state;

  // HEAD-revision content provider backing the Commit view's diffs.
  const headContent = new HeadContentProvider(client, folder.uri);
  context.subscriptions.push(headContent);

  // Shared conflict flow for the session: conflict queries, resolution
  // RPCs, and merge-editor opens. Owned here (not by the Commit view) so
  // the rebase panel and the net-ops flows share the same controller —
  // one conflicts bucket, one merge-editor save tracking, no copies.
  const conflictFlow = new ConflictFlow(client, folder.uri);
  context.subscriptions.push(conflictFlow);
  activeConflictFlow = conflictFlow;

  // Commit view: message box + change list + commit/amend, one webview.
  const commitView = new CommitViewProvider(context, client, folder.uri, conflictFlow);
  activeCommitView = commitView;
  context.subscriptions.push(commitView);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(CommitViewProvider.viewId, commitView, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );

  // Hunk-level staging: [Stage Hunk]/[Unstage Hunk] CodeLenses on the
  // diffs the Commit view opens. Palette-hidden — invoked from lenses only.
  const hunkStaging = new HunkStagingController(client, folder.uri, () => commitView.refresh());
  context.subscriptions.push(hunkStaging);
  registerCommand(context, STAGE_HUNK_COMMAND, (path, range) => hunkStaging.stageHunk(path, range));
  registerCommand(context, UNSTAGE_HUNK_COMMAND, (path, range) => hunkStaging.unstageHunk(path, range));

  // Blame annotations (phase 3): one getBlame round trip per (path, rev,
  // buffer version) drives the gutter decorations, the hover card and the
  // cursor-line CodeLens actions. OFF by default; the toggle opts each
  // editor in.
  const blame = new BlameController(context, client, folder.uri);
  activeBlame = blame;
  context.subscriptions.push(blame);

  // Sidebar commit graph (compact mode) + editor-area graph panel (full
  // width mode) share the same webview bundle via GraphSession.
  const graph = new GraphWebviewProvider(context, client, state, conflictFlow);
  context.subscriptions.push(graph);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider(GraphWebviewProvider.viewId, graph, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );
  registerGraphPanelSerializer(context, client, state, conflictFlow);
  // Rebase panel restores as the read-only docked view when a rebase is
  // still on disk across a window reload (idle → empty state).
  registerRebasePanelSerializer(context, client, conflictFlow);

  // Status bar branch item: branch name plus ahead/behind counts (↑n↓n;
  // omitted without an upstream, e.g. detached HEAD or an unpublished
  // branch). getBranchStatus (Phase 1) is the data source; the same
  // refsChanged/headChanged notifications that refresh the graph drive it.
  const branchItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 99);
  context.subscriptions.push(branchItem);
  let branchLabel = '';
  let branchTooltip = '';
  let aheadBehindText = '';
  const renderBranchItem = (): void => {
    branchItem.text = `$(git-branch) ${branchLabel}${aheadBehindText}`;
    branchItem.tooltip = branchTooltip;
  };

  /** Refreshes current branch + status bar from the engine's getHead RPC. */
  const refreshHead = async (): Promise<void> => {
    try {
      const { head, branch } = await client.getHead();
      state.currentBranch = branch ?? undefined;
      branchLabel = branch ?? head.slice(0, 8);
      branchTooltip = branch
        ? `Git Bamboo: on branch ${branch}`
        : `Git Bamboo: detached HEAD at ${head}`;
      renderBranchItem();
      branchItem.show();
    } catch (err) {
      console.error('[git-bamboo] getHead failed:', err);
    }
  };
  await refreshHead();

  /** Refreshes the ahead/behind suffix from getBranchStatus. Failures
   *  (detached HEAD, RPC error) clear the suffix — there are no counts
   *  to show, and the branch label itself still renders. */
  const refreshBranchStatus = async (): Promise<void> => {
    try {
      const status = await client.getBranchStatus();
      aheadBehindText = status?.upstream ? ` \u2191${status.ahead}\u2193${status.behind}` : '';
    } catch {
      aheadBehindText = '';
    }
    renderBranchItem();
  };
  const scheduleBranchStatus = debounce(
    () => void refreshBranchStatus(),
    BRANCH_STATUS_DEBOUNCE_MS,
  );
  // Initial ahead/behind: refsChanged/headChanged only fire on changes, so
  // a fresh window would otherwise show no counts until the first external
  // ref move (or the first auto-fetch tick — 180s away by default).
  void refreshBranchStatus();
  // Auto-fetch: a silent periodic `git fetch --all` (refsChanged then
  // refreshes the graph/commit view on its own). Completely quiet — no
  // progress notification, no completion toast; failures log to the
  // console only. The manual fetch flows keep their notification —
  // silence distinguishes automatic from user-initiated. Ticks while this
  // window is inactive are skipped so a backgrounded VS Code stops
  // hitting the network; `gitbamboo.autoFetch.intervalSeconds` (0 = off)
  // is read live, so a settings change re-arms the timer. Read from user
  // scope only (trustedSetting) and clamped: a cloned untrusted repo must
  // not be able to crank the silent timer into a per-second network beacon
  // via its own `.vscode/settings.json`.
  let autoFetchTimer: NodeJS.Timeout | undefined;
  const autoFetchIntervalMs = (): number => {
    const seconds = trustedSetting<number>('autoFetch.intervalSeconds', 180);
    const safe =
      typeof seconds === 'number' && Number.isFinite(seconds) ? Math.floor(seconds) : 180;
    if (safe <= 0) return 0; // 0 = disabled
    return Math.min(3600, Math.max(10, safe)) * 1000;
  };
  const scheduleAutoFetch = (): void => {
    if (autoFetchTimer) clearInterval(autoFetchTimer);
    autoFetchTimer = undefined;
    const intervalMs = autoFetchIntervalMs();
    if (intervalMs <= 0) return; // 0 = disabled
    autoFetchTimer = setInterval(() => {
      if (!vscode.window.state.active) return; // window backgrounded
      // Guarded like every other net op: a pull/push/sync in flight skips
      // the tick (fetch is read-only, so a dropped tick costs nothing).
      void withNetOpGuard(client, async () => {
        try {
          await client.fetch(null);
        } catch (err) {
          console.error('[git-bamboo] auto-fetch failed:', errorMessage(err));
        }
      });
    }, intervalMs);
  };
  scheduleAutoFetch();
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (event.affectsConfiguration('gitbamboo.autoFetch.intervalSeconds')) {
        scheduleAutoFetch();
      }
    }),
    { dispose: () => { if (autoFetchTimer) clearInterval(autoFetchTimer); } },
    { dispose: () => scheduleBranchStatus.cancel() },
  );

  // Engine notifications drive all UI refreshes.
  onNotification(client, context, 'refsChanged', () => {
    commitView.refresh();
    // Stash entries live in refs/stash reflog — external `git stash`
    // commands move it, so keep the Commit view's stash list current.
    void commitView.refreshStashes();
    scheduleBranchStatus();
  });
  onNotification(client, context, 'worktreeChanged', () => {
    commitView.refresh();
    hunkStaging.refreshLenses();
    // Blame attributes the worktree/HEAD content the watcher just saw move.
    blame.invalidate();
  });
  onNotification(client, context, 'indexChanged', (params) => {
    state.epoch = numberField(params, 'epoch', state.epoch);
    commitView.refresh();
    // Open index-side (`:0`) diff documents re-fetch their content, and
    // the hunk CodeLenses re-render against the new index state.
    headContent.notifyIndexChanged();
    hunkStaging.refreshLenses();
    // Staging changes which lines are still "not committed yet".
    blame.invalidate();
  });
  onNotification(client, context, 'headChanged', () => {
    void refreshHead();
    scheduleBranchStatus();
    commitView.refresh();
    hunkStaging.refreshLenses();
    // HEAD moved: every cached (path, HEAD) attribution is stale.
    blame.invalidate();
  });

  // Enables commandPalette `when: git-bamboo:active` visibility rules.
  await vscode.commands.executeCommand('setContext', 'git-bamboo:active', true);
}

export async function deactivate(): Promise<void> {
  await activeClient?.dispose();
  activeClient = undefined;
  activeState = undefined;
  activeCommitView = undefined;
  activeBlame = undefined;
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

/** Commit-view counterpart of noGraphTargetWarning: safety net for
 *  programmatic invocation of the palette-hidden commands. */
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
