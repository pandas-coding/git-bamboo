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
import type { BranchStatus, ForceMode, PullStrategy, RemoteInfo, RepoState } from './types';
import { errorMessage, isDetachedError } from './util';

/** What the netOps flows need from the host: the engine client and the
 *  repo root (for opening a terminal at the right cwd on auth failure).
 *  Every consumer (palette now, Phase 4 toolbar later) has both. */
export interface NetOpsContext {
  readonly client: EngineClient;
  readonly repoRoot: vscode.Uri;
  /** Focuses the Commit view and expands its Conflicts section (the pull
   *  conflict banner's "Open Conflict Files" button). */
  readonly showConflicts: () => Promise<void>;
}

/** Build the shared net-ops context: the engine client, repo root, and
 *  the conflict-flow entry point (focuses the Commit view's conflicts
 *  section via its command, registered per-session in extension.ts).
 *  Lives here — next to the NetOpsContext type it constructs — so every
 *  consumer (palette commands, graph toolbar) imports it without pulling
 *  in the whole command module. */
export function netOpsContext(client: EngineClient, state: RepoState): NetOpsContext {
  return {
    client,
    repoRoot: state.root,
    showConflicts: async () => {
      await vscode.commands.executeCommand('gitBamboo.commit.showConflicts');
    },
  };
}

// ---------------------------------------------------------------------------
// In-flight guard
// ---------------------------------------------------------------------------

/** Repository sessions with a net op currently running. While an op
 *  runs, further ops from ANY surface (graph toolbar, command palette,
 *  the other graph host, auto-fetch) are dropped, not queued — concurrent
 *  mutating git operations on one repo must not interleave. WeakSet: the
 *  guard lives and dies with the engine client. */
const netOpInFlight = new WeakSet<EngineClient>();

/** Begins a net op for `client`; false when one is already running (the
 *  caller must then drop the request). Every true is paired with
 *  endNetOp — the release belongs in a finally. */
export function tryBeginNetOp(client: EngineClient): boolean {
  if (netOpInFlight.has(client)) return false;
  netOpInFlight.add(client);
  return true;
}

/** Releases the in-flight slot taken by tryBeginNetOp. */
export function endNetOp(client: EngineClient): void {
  netOpInFlight.delete(client);
}

/** Runs a flow under the shared in-flight guard: dropped while another op
 *  runs for the same repository session. Errors propagate to the caller's
 *  error surface (the palette's uniform command handler in extension.ts). */
export async function withNetOpGuard(
  client: EngineClient,
  run: () => Promise<void>,
): Promise<void> {
  if (!tryBeginNetOp(client)) return;
  try {
    await run();
  } finally {
    endNetOp(client);
  }
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

/** Read a setting from user/global scope only. Workspace settings live
 *  inside the opened repository, so a cloned untrusted repo could
 *  otherwise flip safety knobs via its own `.vscode/settings.json`.
 *  Exported for extension.ts's auto-fetch interval (same reason). */
export function trustedSetting<T>(key: string, fallback: T): T {
  const inspected = vscode.workspace.getConfiguration(CONFIG_SECTION).inspect<T>(key);
  return inspected?.globalValue ?? inspected?.defaultValue ?? fallback;
}

function pullStrategySetting(): PullStrategy {
  // Pull preferences are not safety-critical, so workspace scope is fine.
  const value = vscode.workspace.getConfiguration(CONFIG_SECTION).get<string>('pull.strategy');
  return value === 'merge' || value === 'rebase' || value === 'ff-only' ? value : 'default';
}

function pullAutoStashSetting(): boolean {
  return vscode.workspace.getConfiguration(CONFIG_SECTION).get<boolean>('pull.autoStash') ?? false;
}

// The force-push knobs are deliberately scope-restricted: an untrusted
// repo must not be able to silence the confirmation or upgrade to a
// unconditional --force (see trustedSetting).
function pushForceModeSetting(): ForceMode {
  return trustedSetting<string>('push.forceMode', 'lease') === 'force' ? 'force' : 'lease';
}

function pushConfirmForceSetting(): boolean {
  return trustedSetting('push.confirmForce', true);
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/** QuickPick items for the configured remotes, default remote first —
 *  the single mapping shared by fetch and publish. */
function remotePickItems(remotes: RemoteInfo[]): RemotePickItem[] {
  return [...remotes]
    .sort((a, b) => Number(b.default) - Number(a.default))
    .map((remote) => ({
      label: remote.name,
      description: remote.url ?? '(no fetch URL)',
      detail: remote.default ? 'Default remote' : undefined,
      remote: remote.name,
    }));
}

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
      ...remotePickItems(remotes),
    ],
    { placeHolder: 'Fetch from', matchOnDescription: true },
  );
  if (!picked) return; // cancelled
  const target = picked.remote ?? 'all remotes';
  await fetchFrom(ctx, picked.remote, target);
}

/** Shared fetch execution: progress, completion toast, error surface —
 *  one implementation behind both the palette's picked remote and the
 *  toolbar's one-click --all fetch. */
async function fetchFrom(ctx: NetOpsContext, remote: string | null, target: string): Promise<void> {
  try {
    await withNetworkProgress(`Fetching from ${target}…`, () => ctx.client.fetch(remote));
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
    if (await offerPublish(ctx, status.branch, 'has no upstream', 'Set Upstream & Pull…')) {
      return runPull(ctx, strategyOverride); // upstream now exists — pull
    }
    return;
  }
  await pullCurrentBranch(ctx, status.branch, strategyOverride ?? pullStrategySetting());
}

/** No-upstream fallback shared by the pull/push/sync flows: an info
 *  message offering publish, then the publish QuickPick. Returns true
 *  when the branch was published (the caller may continue its flow). */
async function offerPublish(
  ctx: NetOpsContext,
  branch: string,
  reason: string,
  button = 'Publish Branch…',
): Promise<boolean> {
  const choice = await vscode.window.showInformationMessage(
    `Git Bamboo: branch '${branch}' ${reason}`,
    button,
  );
  if (choice !== button) return false;
  return publishBranch(ctx, branch);
}

/** Pull of a branch whose upstream is known to exist. Returns true when
 *  the pull completed cleanly; on failure the error surface (conflict
 *  notification, network error) has already run. Shared by runPull and
 *  runSync's pull phase — one implementation, no per-surface copies. */
async function pullCurrentBranch(
  ctx: NetOpsContext,
  branch: string,
  strategy: PullStrategy,
): Promise<boolean> {
  try {
    const output = await withNetworkProgress(`Pulling ${branch}…`, () =>
      ctx.client.pull(strategy, pullAutoStashSetting()),
    );
    vscode.window.showInformationMessage(
      `Git Bamboo: pulled ${branch}${outputSummary(output)}`,
    );
    return true;
  } catch (err) {
    showPullError(ctx, err, branch);
    return false;
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
 *  auto-aborted — same semantic as discard's refusal) and offer to jump to
 *  the Commit view's conflicts section. */
function showPullError(ctx: NetOpsContext, err: unknown, branch: string): void {
  if (isPullConflict(err)) {
    void vscode.window
      .showErrorMessage(
        `Git Bamboo: pull of ${branch} hit conflicts — resolve them in the Commit view ` +
          '(nothing was aborted)',
        'Open Conflict Files',
      )
      .then((choice) => {
        if (choice === 'Open Conflict Files') {
          void ctx.showConflicts();
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
  if (code !== undefined && code !== RpcErrorCode.GIT_ERROR) return false;
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
  const { upstream } = status;
  if (!upstream) {
    await offerPublish(ctx, status.branch, 'is not published yet');
    return;
  }
  await pushCurrentBranch(ctx, status.branch, upstream, status.ahead, status.behind);
}

/** Push of a branch that has an upstream (the parameter types carry the
 * invariant — every caller routes upstream-less branches to offerPublish
 * first): up-to-date / behind branches get the appropriate message instead
 * of a pointless RPC; otherwise push with rejection recovery. Shared by
 * runPush and runSync's push phase. */
async function pushCurrentBranch(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
  ahead: number,
  behind: number,
): Promise<void> {
  if (ahead === 0) {
    if (behind > 0) {
      // Nothing to push, but the branch is behind — offer the pull that
      // actually moves it forward instead of a misleading "up to date".
      const choice = await vscode.window.showInformationMessage(
        `Git Bamboo: ${branch} is ${behind} commit${behind === 1 ? '' : 's'} ` +
          `behind ${upstream} — nothing to push`,
        'Pull',
      );
      if (choice === 'Pull') await runPull(ctx);
      return;
    }
    // Truly in sync — force remains reachable via its explicit command.
    vscode.window.showInformationMessage(
      `Git Bamboo: ${branch} is already up to date with ${upstream}`,
    );
    return;
  }
  await pushWithRecovery(ctx, branch, upstream);
}

// ---------------------------------------------------------------------------
// Graph toolbar entry points (Phase 4)
// ---------------------------------------------------------------------------

/** One-click fetch of all remotes (graph toolbar ⟳ button). The palette
 *  entry keeps the per-remote QuickPick — the command palette stays the
 *  capability superset; the toolbar button always fetches --all. */
export async function runFetchAll(ctx: NetOpsContext): Promise<void> {
  await fetchFrom(ctx, null, 'all remotes');
}

/** Publish flow for the current branch (the graph toolbar's `↑✱` badge
 *  click on an unpublished branch). Already-published branches just get
 *  told so — the badge should not have been clickable, this is the
 *  safety net for a stale click. */
export async function runPublish(ctx: NetOpsContext): Promise<void> {
  const status = await currentBranchStatus(ctx);
  if (!status) return;
  if (status.upstream) {
    vscode.window.showInformationMessage(
      `Git Bamboo: ${status.branch} already tracks ${status.upstream}`,
    );
    return;
  }
  await publishBranch(ctx, status.branch);
}

/** Sync flow (graph toolbar ⇧ button): pull, then push. Any failure — a
 *  pull conflict, a dismissed dialog, a rejected push the user chose not
 *  to recover — stops the chain; nothing is retried automatically. An
 *  unpublished branch degrades to the publish flow (same fallback the
 *  pull and push flows use for upstream-less branches). */
export async function runSync(ctx: NetOpsContext): Promise<void> {
  const status = await currentBranchStatus(ctx);
  if (!status) return;
  if (!status.upstream) {
    await offerPublish(ctx, status.branch, 'has no upstream');
    return;
  }
  // Pull phase: the shared pull path (progress, conflict surfacing).
  if (!(await pullCurrentBranch(ctx, status.branch, pullStrategySetting()))) return;
  // Push phase: re-read the status first — the pull just moved refs, so
  // ahead/behind (and the up-to-date messaging) must reflect the new state.
  const fresh = await currentBranchStatus(ctx);
  if (!fresh || !fresh.upstream) return; // unreachable post-pull; engine says otherwise
  await pushCurrentBranch(ctx, fresh.branch, fresh.upstream, fresh.ahead, fresh.behind);
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
  const picked = await vscode.window.showQuickPick<RemotePickItem>(remotePickItems(remotes), {
    placeHolder: 'Publish to which remote?',
  });
  return picked?.remote ?? undefined;
}

/** Push target derived from an upstream short name: the remote and the
 *  pushspec to send. A branch tracking a differently named remote branch
 *  (e.g. local `foo` tracking `origin/bar`) pushes `foo:bar` so the
 *  update lands on the ref the user actually tracks. */
interface PushTarget {
  readonly remote: string;
  readonly pushspec: string;
}

/** Resolve an upstream like "origin/main" against the repo's configured
 *  remotes. Malformed tracking names or a remote that is no longer
 *  configured fail with a readable error instead of feeding an
 *  unvalidated name into push. */
async function resolvePushTarget(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
): Promise<PushTarget | null> {
  const slash = upstream.indexOf('/');
  if (slash <= 0 || slash === upstream.length - 1) {
    vscode.window.showErrorMessage(
      `Git Bamboo: unexpected upstream '${upstream}' — cannot determine the remote branch`,
    );
    return null;
  }
  const remote = upstream.slice(0, slash);
  const remoteBranch = upstream.slice(slash + 1);
  let remotes: RemoteInfo[];
  try {
    remotes = await ctx.client.getRemotes();
  } catch (err) {
    vscode.window.showErrorMessage(
      `Git Bamboo: remotes unavailable — ${displaySafe(errorMessage(err))}`,
    );
    return null;
  }
  if (!remotes.some((r) => r.name === remote)) {
    vscode.window.showErrorMessage(
      `Git Bamboo: upstream remote '${remote}' is not configured in this repository`,
    );
    return null;
  }
  return { remote, pushspec: remoteBranch === branch ? branch : `${branch}:${remoteBranch}` };
}

/** Plain push of `branch` to its `upstream`; a -32009 rejection opens
 *  the recovery button group instead of a dead-end error. */
async function pushWithRecovery(
  ctx: NetOpsContext,
  branch: string,
  upstream: string,
): Promise<void> {
  const target = await resolvePushTarget(ctx, branch, upstream);
  if (!target) return;
  try {
    await withNetworkProgress(`Pushing ${branch} → ${upstream}…`, () =>
      ctx.client.push(target.remote, target.pushspec, 'none', false),
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
  const target = await resolvePushTarget(ctx, branch, upstream);
  if (!target) return;
  try {
    await withNetworkProgress(`Force pushing ${branch} → ${upstream}…`, () =>
      ctx.client.push(target.remote, target.pushspec, forceMode, false),
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

/** Notifications must stay plain strings: git output and engine error
 *  messages embed remote-controlled text (stderr, URLs, ref names), which
 *  is inert as text but would render as markup if ever passed as a
 *  MarkdownString. Strip control characters and angle brackets at the
 *  notification boundary so such a refactor can't turn hostile remote
 *  content into an injection surface. */
function displaySafe(text: string): string {
  return text.replace(/[\x00-\x1f\x7f]/g, ' ').replace(/[<>]/g, '');
}

/** getBranchStatus for the current branch; null (with a readable
 *  notification) when HEAD is detached, the RPC fails, or the engine
 *  returns a malformed payload. */
async function currentBranchStatus(ctx: NetOpsContext): Promise<BranchStatus | null> {
  let status: BranchStatus | null;
  try {
    status = await ctx.client.getBranchStatus();
  } catch (err) {
    if (isDetachedError(err)) {
      vscode.window.showWarningMessage(
        'Git Bamboo: HEAD is detached — switch to a branch to pull or push',
      );
    } else {
      vscode.window.showErrorMessage(`Git Bamboo: branch status unavailable — ${displaySafe(errorMessage(err))}`);
    }
    return null;
  }
  if (!status) {
    vscode.window.showErrorMessage(
      'Git Bamboo: branch status unavailable — malformed engine response',
    );
    return null;
  }
  return status;
}

/** getRemotes; null (with a warning) when the repo has none configured
 *  or the RPC fails. */
async function getRemotesOrWarn(ctx: NetOpsContext): Promise<RemoteInfo[] | null> {
  let remotes: RemoteInfo[];
  try {
    remotes = await ctx.client.getRemotes();
  } catch (err) {
    vscode.window.showErrorMessage(
      `Git Bamboo: remotes unavailable — ${displaySafe(errorMessage(err))}`,
    );
    return null;
  }
  if (remotes.length === 0) {
    vscode.window.showWarningMessage('Git Bamboo: this repository has no remotes configured');
    return null;
  }
  return remotes;
}

/** Network commands run under an indeterminate progress notification.
 *  Engine-side `--progress` parsing is deliberately deferred (original
 *  design §2.6), and the client cannot observe git's transfer stages, so
 *  this is a single "transferring…" stage held until the RPC resolves. */
async function withNetworkProgress(
  title: string,
  run: () => Promise<string>,
): Promise<string> {
  return vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title: `Git Bamboo: ${title}` },
    async (progress) => {
      progress.report({ message: 'transferring…' });
      return run();
    },
  );
}

/** Uniform error surface for network failures: auth failures get the
 *  terminal guidance button, everything else a readable message. */
function showNetworkError(ctx: NetOpsContext, err: unknown, action: string): void {
  if (rpcErrorCode(err) === RpcErrorCode.AUTH_REQUIRED) {
    void vscode.window
      .showErrorMessage(`Git Bamboo: ${action} needs authentication — ${displaySafe(errorMessage(err))}`, {
        title: 'Open Terminal & Fetch',
        isCloseAffordance: true,
      })
      .then((item) => {
        if (item?.title === 'Open Terminal & Fetch') openTerminalAndFetch(ctx);
      });
    return;
  }
  vscode.window.showErrorMessage(`Git Bamboo: ${action} failed — ${displaySafe(errorMessage(err))}`);
}

/** Non-interactive engine auth cannot prompt — guide the user to run one
 *  `git fetch` in a terminal so the system credential helper can ask. */
function openTerminalAndFetch(ctx: NetOpsContext): void {
  const terminal = vscode.window.createTerminal({
    name: 'Git Bamboo auth',
    cwd: ctx.repoRoot,
  });
  terminal.show();
  terminal.sendText('git fetch');
}

/** First meaningful line of git output as a completion summary (e.g.
 *  "Fast-forward", "Already up to date."), capped and sanitized for a
 *  notification (the output embeds remote-controlled text). */
function outputSummary(output: string): string {
  const firstLine = output.trim().split('\n').find((line) => line.trim().length > 0);
  if (!firstLine) return '';
  const summary = displaySafe(firstLine.trim()).slice(0, 120);
  return ` — ${summary}`;
}
