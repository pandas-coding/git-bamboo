/**
 * Shared network operation flows (fetch / pull / push / force push) for
 * the command palette. Phase 4's toolbar reuses these functions — the
 * interaction logic lives here once, never duplicated per surface.
 *
 * All flows are zero-typing: remotes come from the engine's `getRemotes`,
 * upstream state from `getBranchStatus`, and every git mutation goes
 * through engine RPC (host code never shells out to git — that would
 * bypass the engine's epoch/undo tracking).
 */
import * as vscode from 'vscode';
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import type { BranchStatus, ForceMode, PullStrategy, RemoteInfo } from './types';
import { errorMessage } from './util';

/** What the netOps flows need from the host: the engine client and the
 *  repo root (for opening a terminal at the right cwd on auth failure). */
export interface NetOpsContext {
  readonly client: EngineClient;
  readonly repoRoot?: vscode.Uri;
}

const CONFIG_SECTION = 'gitbamboo';

interface RemotePickItem extends vscode.QuickPickItem {
  remote: string | null;
}

interface StrategyPickItem extends vscode.QuickPickItem {
  strategy: PullStrategy;
}

// ---------------------------------------------------------------------------
// Settings readers
// ---------------------------------------------------------------------------

function pullStrategySetting(): PullStrategy {
  const value = vscode.workspace.getConfiguration(CONFIG_SECTION).get<string>('pull.strategy');
  return value === 'merge' || value === 'rebase' || value === 'ff-only' ? value : 'default';
}

function pullAutoStashSetting(): boolean {
  return vscode.workspace.getConfiguration(CONFIG_SECTION).get<boolean>('pull.autoStash') ?? false;
}

function pushForceModeSetting(): ForceMode {
  const value = vscode.workspace.getConfiguration(CONFIG_SECTION).get<string>('push.forceMode');
  return value === 'force' ? 'force' : 'lease';
}

function pushConfirmForceSetting(): boolean {
  return vscode.workspace.getConfiguration(CONFIG_SECTION).get<boolean>('push.confirmForce') ?? true;
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/** Fetch flow: QuickPick of "All remotes" (fetch --all) plus each
 *  configured remote — never a hand-typed remote name. */
export async function runFetch(ctx: NetOpsContext): Promise<void> {
  const remotes = await getRemotesOrWarn(ctx);
  if (!remotes) return;
  const picked = await vscode.window.showQuickPick<RemotePickItem>(
    [
      {
        label: 'All Remotes',
        description: 'git fetch --all',
        remote: null,
      },
      ...remotes.map((remote) => ({
        label: remote.name,
        description: remote.url ?? '(no fetch URL)',
        detail: remote.default ? 'Default remote' : undefined,
        remote: remote.name,
      })),
    ],
    { placeHolder: 'Fetch from', matchOnDescription: true },
  );
  if (!picked) return; // cancelled
  const target = picked.remote ?? 'all remotes';
  try {
    await withNetworkProgress(`Fetching from ${target}…`, () => ctx.client.fetch(picked.remote));
    // The engine's refsChanged notification refreshes the graph/commit
    // view on its own — nothing to nudge here.
    vscode.window.showInformationMessage(`Git Bamboo: fetched from ${target}`);
  } catch (err) {
    showNetworkError(ctx, err, 'fetch');
  }
}

// ---------------------------------------------------------------------------
// Pull
// ---------------------------------------------------------------------------

/** Pull flow for the current branch. `strategyOverride` comes from the
 *  one-shot "Pull with strategy…" command; the persisted
 *  `gitbamboo.pull.strategy` setting is used when absent. */
export async function runPull(ctx: NetOpsContext, strategyOverride?: PullStrategy): Promise<void> {
  const status = await currentBranchStatus(ctx);
  if (!status) return;
  if (!status.upstream) {
    const choice = await vscode.window.showInformationMessage(
      `Git Bamboo: branch '${status.branch}' has no upstream`,
      'Set Upstream & Pull…',
    );
    if (choice !== 'Set Upstream & Pull…') return;
    const published = await publishBranch(ctx, status.branch);
    if (!published) return; // cancelled or failed
    return runPull(ctx, strategyOverride); // upstream now exists — pull
  }
  const strategy = strategyOverride ?? pullStrategySetting();
  const autoStash = pullAutoStashSetting();
  try {
    const output = await withNetworkProgress(`Pulling ${status.branch}…`, () =>
      ctx.client.pull(strategy, autoStash),
    );
    vscode.window.showInformationMessage(
      `Git Bamboo: pulled ${status.branch}${outputSummary(output)}`,
    );
  } catch (err) {
    showPullError(ctx, err, status.branch);
  }
}

/** One-shot strategy picker for the `pullWithStrategy` command. */
export async function runPullWithStrategy(ctx: NetOpsContext): Promise<void> {
  const picked = await vscode.window.showQuickPick<StrategyPickItem>(
    [
      { label: 'Rebase', description: 'git pull --rebase', strategy: 'rebase' },
      { label: 'Merge', description: 'git pull --no-rebase', strategy: 'merge' },
      { label: 'Fast-forward Only', description: 'git pull --ff-only', strategy: 'ff-only' },
      { label: 'Default (git config)', description: "respect pull.rebase / pull.ff", strategy: 'default' },
    ],
    { placeHolder: 'Pull with which strategy?' },
  );
  if (!picked) return; // cancelled
  await runPull(ctx, picked.strategy);
}

/** Pull failure surface: conflicts keep the worktree conflicted (never
 *  auto-aborted — same semantic as discard's refusal) and offer to focus
 *  the Commit view, where Phase 3's conflict section will pick them up. */
function showPullError(ctx: NetOpsContext, err: unknown, branch: string): void {
  if (isPullConflict(err)) {
    void vscode.window
      .showErrorMessage(
        `Git Bamboo: pull of ${branch} hit conflicts — resolve them in the Commit view ` +
          '(nothing was aborted)',
        'Open Conflict Files',
      )
      .then((choice) => {
        // Placeholder behavior until Phase 3's conflict section: focusing
        // the Commit view already lists the conflicted files as changes.
        if (choice === 'Open Conflict Files') {
          void vscode.commands.executeCommand('gitBamboo.commit.focus');
        }
      });
    return;
  }
  showNetworkError(ctx, err, `pull of ${branch}`);
}

/** Conflict failures arrive as -32008 (stash/apply conflicts) or as a
 *  GIT_ERROR whose stderr mentions conflicts (merge/rebase conflict). */
function isPullConflict(err: unknown): boolean {
  const code = rpcErrorCode(err);
  if (code === RpcErrorCode.STASH_CONFLICT) return true;
  if (code !== undefined && code !== -32002) return false;
  return /conflict/i.test(errorMessage(err));
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

/** Push flow for the current branch: unpublished branches go through the
 *  publish QuickPick (`push -u`), published ones are a one-click push. */
export async function runPush(ctx: NetOpsContext): Promise<void> {
  const status = await currentBranchStatus(ctx);
  if (!status) return;
  if (!status.upstream) {
    const choice = await vscode.window.showInformationMessage(
      `Git Bamboo: branch '${status.branch}' is not published yet`,
      'Publish Branch…',
    );
    if (choice !== 'Publish Branch…') return;
    await publishBranch(ctx, status.branch);
    return;
  }
  if (status.ahead === 0) {
    // Nothing to push — force remains reachable via its explicit command.
    vscode.window.showInformationMessage(
      `Git Bamboo: ${status.branch} is already up to date with ${status.upstream}`,
    );
    return;
  }
  await pushWithRecovery(ctx, status.branch, status.upstream);
}

/** Explicit force-push entry (`gitBamboo.forcePush` command or the push
 *  rejection recovery button). Plain pushes never reach this path. */
export async function runForcePush(ctx: NetOpsContext): Promise<void> {
  const status = await currentBranchStatus(ctx);
  if (!status) return;
  if (!status.upstream) {
    vscode.window.showInformationMessage(
      `Git Bamboo: branch '${status.branch}' is not published yet — publishing needs no force`,
    );
    return;
  }
  await confirmAndForcePush(ctx, status.branch, status.upstream);
}

/** Publish flow: pick a remote (default remote first) and push with
 *  set_upstream so later pushes are one-click. Returns true on success. */
async function publishBranch(ctx: NetOpsContext, branch: string): Promise<boolean> {
  const remotes = await getRemotesOrWarn(ctx);
  if (!remotes) return false;
  const remote = await pickRemoteForPublish(remotes);
  if (!remote) return false; // cancelled
  try {
    await withNetworkProgress(`Publishing ${branch} → ${remote}…`, () =>
      ctx.client.push(remote, branch, 'none', true),
    );
    vscode.window.showInformationMessage(
      `Git Bamboo: published ${branch} → ${remote}/${branch} (upstream set)`,
    );
    return true;
  } catch (err) {
    showNetworkError(ctx, err, `publish of ${branch}`);
    return false;
  }
}

/** Remote QuickPick for publish: the engine-flagged default remote
 *  first, then the rest — no hand-typed names. */
async function pickRemoteForPublish(remotes: RemoteInfo[]): Promise<string | undefined> {
  const ordered = [...remotes].sort((a, b) => Number(b.default) - Number(a.default));
  const picked = await vscode.window.showQuickPick<RemotePickItem>(
    ordered.map((remote) => ({
      label: remote.name,
      description: remote.url ?? '(no fetch URL)',
      detail: remote.default ? 'Default remote' : undefined,
      remote: remote.name,
    })),
    { placeHolder: 'Publish to which remote?' },
  );
  return picked?.remote ?? undefined;
}

/** Plain push of `branch` to its `upstream`; a -32009 rejection opens
 *  the recovery button group instead of a dead-end error. */
async function pushWithRecovery(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
): Promise<void> {
  try {
    await withNetworkProgress(`Pushing ${branch} → ${upstream}…`, () =>
      ctx.client.push(remoteOf(upstream), branch, 'none', false),
    );
    vscode.window.showInformationMessage(`Git Bamboo: pushed ${branch} → ${upstream}`);
  } catch (err) {
    if (rpcErrorCode(err) === RpcErrorCode.PUSH_REJECTED) {
      await offerPushRejectionRecovery(ctx, branch, upstream);
      return;
    }
    showNetworkError(ctx, err, `push of ${branch}`);
  }
}

/** Rejected-push closure: pull (rebase or merge) then automatically
 *  retry the push, or go through the force-push confirmation. A pull
 *  conflict stops the flow — the conflict notification takes over. */
async function offerPushRejectionRecovery(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
): Promise<void> {
  const choice = await vscode.window.showErrorMessage(
    `Git Bamboo: push rejected — ${upstream} has commits you don't have locally`,
    'Pull & Push (Rebase)',
    'Pull & Push (Merge)',
    'Force Push…',
  );
  if (!choice) return; // dismissed
  if (choice === 'Force Push…') {
    await confirmAndForcePush(ctx, branch, upstream);
    return;
  }
  const strategy: PullStrategy = choice === 'Pull & Push (Rebase)' ? 'rebase' : 'merge';
  try {
    await withNetworkProgress(`Pulling ${branch} (rebase/merge recovery)…`, () =>
      ctx.client.pull(strategy, pullAutoStashSetting()),
    );
  } catch (err) {
    // Stop here: the pull conflict flow (or error surface) already told
    // the user what to do — blindly pushing on would betray the state.
    showPullError(ctx, err, branch);
    return;
  }
  await pushWithRecovery(ctx, branch, upstream);
}

/** Force-push confirmation modal (configurable via `gitbamboo.push.*`
 *  settings), then the force push itself with the configured mode. */
async function confirmAndForcePush(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
): Promise<void> {
  const forceMode = pushForceModeSetting();
  if (pushConfirmForceSetting()) {
    const detail =
      forceMode === 'lease'
        ? "Overwrites the remote branch only while it still points at the commit you last " +
          'fetched (--force-with-lease).'
        : 'Unconditionally overwrites the remote branch — commits that diverged on the ' +
          'remote will be lost (--force).';
    const confirmed = await vscode.window.showWarningMessage(
      `Force push '${branch}' to '${upstream}'?`,
      { modal: true, detail },
      'Force Push',
    );
    if (confirmed !== 'Force Push') return; // cancelled
  }
  try {
    await withNetworkProgress(`Force pushing ${branch} → ${upstream}…`, () =>
      ctx.client.push(remoteOf(upstream), branch, forceMode, false),
    );
    vscode.window.showInformationMessage(
      `Git Bamboo: force pushed ${branch} → ${upstream} (--force${
        forceMode === 'lease' ? '-with-lease' : ''
      })`,
    );
  } catch (err) {
    if (rpcErrorCode(err) === RpcErrorCode.PUSH_REJECTED) {
      // A tripped lease is exactly the protection working: the remote
      // moved since our last fetch, so offer the pull recovery again.
      await offerPushRejectionRecovery(ctx, branch, upstream);
      return;
    }
    showNetworkError(ctx, err, `force push of ${branch}`);
  }
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/** Remote part of a short upstream name like "origin/main". */
function remoteOf(upstream: string): string {
  const slash = upstream.indexOf('/');
  return slash > 0 ? upstream.slice(0, slash) : upstream;
}

/** getBranchStatus for the current branch; null (with a readable
 *  notification) when HEAD is detached or the RPC fails. */
async function currentBranchStatus(ctx: NetOpsContext): Promise<BranchStatus | null> {
  try {
    return await ctx.client.getBranchStatus();
  } catch (err) {
    const message = errorMessage(err);
    if (/detached/i.test(message)) {
      vscode.window.showWarningMessage(
        'Git Bamboo: HEAD is detached — switch to a branch to pull or push',
      );
    } else {
      vscode.window.showErrorMessage(`Git Bamboo: branch status unavailable — ${message}`);
    }
    return null;
  }
}

/** getRemotes; null (with a warning) when the repo has none configured
 *  or the RPC fails. */
async function getRemotesOrWarn(ctx: NetOpsContext): Promise<RemoteInfo[] | null> {
  let remotes: RemoteInfo[];
  try {
    remotes = await ctx.client.getRemotes();
  } catch (err) {
    vscode.window.showErrorMessage(`Git Bamboo: remotes unavailable — ${errorMessage(err)}`);
    return null;
  }
  if (remotes.length === 0) {
    vscode.window.showWarningMessage('Git Bamboo: this repository has no remotes configured');
    return null;
  }
  return remotes;
}

/** Network commands run under a progress notification. Without
 *  engine-side `--progress` parsing the stages are client-known only:
 *  connecting → transferring → done. */
async function withNetworkProgress(
  title: string,
  run: () => Promise<string>,
): Promise<string> {
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `Git Bamboo: ${title}` },
    async (progress) => {
      progress.report({ message: 'connecting…' });
      const output = await run();
      progress.report({ message: 'done' });
      return output;
    },
  );
}

/** Uniform error surface for network failures: auth failures get the
 *  terminal guidance button, everything else a readable message. */
function showNetworkError(ctx: NetOpsContext, err: unknown, action: string): void {
  if (rpcErrorCode(err) === RpcErrorCode.AUTH_REQUIRED) {
    void vscode.window
      .showErrorMessage(`Git Bamboo: ${action} needs authentication — ${errorMessage(err)}`, {
        title: 'Open Terminal & Fetch',
        isCloseAffordance: true,
      })
      .then((item) => {
        if (item?.title === 'Open Terminal & Fetch') openTerminalAndFetch(ctx);
      });
    return;
  }
  vscode.window.showErrorMessage(`Git Bamboo: ${action} failed — ${errorMessage(err)}`);
}

/** Non-interactive engine auth cannot prompt — guide the user to run one
 *  `git fetch` in a terminal so the system credential helper can ask. */
function openTerminalAndFetch(ctx: NetOpsContext): void {
  const terminal = vscode.window.createTerminal({
    name: 'Git Bamboo auth',
    ...(ctx.repoRoot ? { cwd: ctx.repoRoot } : {}),
  });
  terminal.show();
  terminal.sendText('git fetch');
}

/** First meaningful line of git output as a completion summary (e.g.
 *  "Fast-forward", "Already up to date."), capped for a notification. */
function outputSummary(output: string): string {
  const firstLine = output.trim().split('\n').find((line) => line.trim().length > 0);
  if (!firstLine) return '';
  const summary = firstLine.trim().slice(0, 120);
  return ` — ${summary}`;
}
