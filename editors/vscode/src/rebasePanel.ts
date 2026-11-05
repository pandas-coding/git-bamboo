/**
 * Rebase plan panel (phase 2): editor-area singleton webview
 * (gitBamboo.rebasePanel) where the user edits an interactive-rebase
 * todo — reorder (drag), change actions (pick/reword/squash/drop),
 * inline-edit reword/squash messages — and executes it. Opened from the
 * graph's "Rebase onto this commit…" context entry; while a rebase is
 * stopped (conflict), the panel switches to a read-only docked view with
 * done/remaining sections and continue/skip/abort, sharing the conflict
 * flow (conflictFlow.ts) with pull/stash conflicts.
 *
 * Host protocol (see webview/rebase/rebase.js):
 *   inbound:  { type: 'ready' }
 *             { type: 'executeRebase', onto, todo: [{ id, action, message? }] }
 *             { type: 'rebaseContinue' | 'rebaseSkip' | 'rebaseAbort' }
 *             { type: 'refreshPlan' }
 *             { type: 'openConflicts' }
 *             { type: 'close' }
 *   outbound: { type: 'plan', onto, branch, upstream, commits }
 *             { type: 'status', status, conflictCount?, reason? }
 *             { type: 'stale' }            — engine -32005: plan expired
 *             { type: 'blocked', message } — continue refused (unresolved
 *                                            conflicts / rebase wedged)
 *             { type: 'finished', outcome }— rebase done or aborted
 *             { type: 'executing', active, op? }
 *             { type: 'requestExecute' }   — hidden palette command echo
 *             { type: 'loading' } / { type: 'empty' } / { type: 'error', message }
 *
 * Security red lines honored here:
 *  - the constructor never postMessages (the webview drops them until its
 *    listener is up); the first data push rides on the 'ready' handshake;
 *  - inbound messages are validated field-by-field before any engine RPC
 *    (rewriting history is the most dangerous surface in the extension):
 *    `onto` must echo the host-held plan target, the todo is bounded
 *    (≤ 500 entries), every id is a full sha, every action is one of the
 *    four, and messages are single-paragraph and bounded — an invalid
 *    payload is dropped with a console log, never echoed back (the
 *    webview is a trust boundary, and error echoes probe it);
 *  - user edits are never cleared by a refresh: engine notifications only
 *    refresh the DOCKED view; in plan mode a stale plan is reported
 *    ('stale') and the user decides whether to reload it (refreshPlan).
 */
import * as vscode from 'vscode';
import { ConflictFlow, showConflictsView } from './conflictFlow';
import { EngineClient, RpcErrorCode, rpcErrorCode } from './engineClient';
import type {
  GetRebaseStatusResult,
  RebaseTodoEntry,
  RebaseResult,
  RebaseStopReason,
} from './types';
import { COMMIT_ID_RE, debounce, errorMessage } from './util';
import { buildWebviewHtml } from './webviewHtml';

const PANEL_TYPE = 'gitBamboo.rebasePanel';

/** Debounce for notification-driven status refreshes (rebase steps fire
 *  head/worktree/index notifications in bursts). */
const STATUS_DEBOUNCE_MS = 200;

/** Host-side todo bounds — the engine re-validates (Phase 1), this gate
 *  keeps malformed webview payloads from reaching the RPC at all. */
const MAX_TODO_ENTRIES = 500;
/** Mirrors the engine's MAX_REBASE_MESSAGE_LEN (single paragraph, no
 *  control characters — the engine rejects multi-line messages). */
const MAX_MESSAGE_LEN = 8192;
/** Only the four plan-editable actions are valid in an executeRebase todo
 *  (fixup/edit appear only in engine-reported status entries). */
const TODO_ACTIONS = new Set(['pick', 'reword', 'squash', 'drop']);

/** Inbound webview messages (fields validated field-by-field). */
interface RebasePanelMessage {
  type?: unknown;
  onto?: unknown;
  todo?: unknown;
}

/** The currently open rebase plan panel (singleton). */
let activePanel: RebasePanel | undefined;

/**
 * Opens (or re-targets) the rebase plan panel for `onto`. The plan is
 * fetched host-side and pushed after the webview's 'ready' handshake.
 */
export function openRebasePlanPanel(
  context: vscode.ExtensionContext,
  client: EngineClient,
  conflictFlow: ConflictFlow,
  onto: string,
): void {
  if (typeof onto !== 'string' || !COMMIT_ID_RE.test(onto)) return;
  if (activePanel) {
    activePanel.retarget(client, conflictFlow, onto);
    return;
  }
  const panel = vscode.window.createWebviewPanel(PANEL_TYPE, 'Rebase', vscode.ViewColumn.Beside, {
    enableScripts: true,
    retainContextWhenHidden: true,
    localResourceRoots: [
      vscode.Uri.joinPath(context.extensionUri, 'out', 'webview'),
      vscode.Uri.joinPath(context.extensionUri, 'webview'),
    ],
  });
  activePanel = new RebasePanel(context, panel, client, conflictFlow, onto);
}

/**
 * Restores the panel across window reloads: a rebase still on disk
 * re-attaches as the read-only docked view (done/remaining + continue/
 * skip/abort); an idle repository gets the panel's empty state.
 */
export function registerRebasePanelSerializer(
  context: vscode.ExtensionContext,
  client: EngineClient,
  conflictFlow: ConflictFlow,
): void {
  context.subscriptions.push(
    vscode.window.registerWebviewPanelSerializer(PANEL_TYPE, {
      deserializeWebviewPanel: async (panel: vscode.WebviewPanel) => {
        if (activePanel) return; // a live panel wins over the restored shell
        activePanel = new RebasePanel(context, panel, client, conflictFlow, undefined);
      },
    }),
  );
}

/**
 * Runs one of the hidden `gitBamboo.rebasePanel.*` commands against the
 * open panel. Returns false when no panel is open — the palette entries
 * are `when: false`, so this is the safety net for programmatic
 * invocation. `execute` asks the webview to run its confirmation modal
 * (the todo lives webview-side; the host never guesses it).
 */
export function runRebasePanelCommand(
  command: 'execute' | 'continue' | 'skip' | 'abort' | 'close',
): boolean {
  const panel = activePanel;
  if (!panel) return false;
  switch (command) {
    case 'execute':
      panel.requestExecute();
      break;
    case 'continue':
    case 'skip':
    case 'abort':
      void panel.step(command);
      break;
    case 'close':
      panel.close();
      break;
  }
  return true;
}

/** Field-by-field validation of an executeRebase payload (trust
 *  boundary). Invalid shapes resolve undefined — dropped with a log, the
 *  RPC never sees them, and no error is echoed back to the webview. */
function parseExecutePayload(
  message: RebasePanelMessage,
  expectedOnto: string | undefined,
): { onto: string; todo: RebaseTodoEntry[] } | undefined {
  if (typeof message.onto !== 'string' || !COMMIT_ID_RE.test(message.onto)) return undefined;
  // The onto must echo the host-held plan target: the host pushed the
  // plan, so a diverging value means the webview state (or the payload)
  // is not what the user is looking at.
  if (expectedOnto === undefined || message.onto !== expectedOnto) return undefined;
  const rawTodo = message.todo;
  if (!Array.isArray(rawTodo) || rawTodo.length === 0 || rawTodo.length > MAX_TODO_ENTRIES) {
    return undefined;
  }
  const todo: RebaseTodoEntry[] = [];
  for (const raw of rawTodo) {
    if (typeof raw !== 'object' || raw === null) return undefined;
    const entry = raw as Record<string, unknown>;
    if (typeof entry.id !== 'string' || !COMMIT_ID_RE.test(entry.id)) return undefined;
    if (typeof entry.action !== 'string' || !TODO_ACTIONS.has(entry.action)) return undefined;
    const item: RebaseTodoEntry = { id: entry.id, action: entry.action as RebaseTodoEntry['action'] };
    if (entry.message !== undefined && entry.message !== null) {
      if (typeof entry.message !== 'string') return undefined;
      if (entry.message.length === 0 || entry.message.length > MAX_MESSAGE_LEN) return undefined;
      if (/[\u0000-\u001f\u007f]/.test(entry.message)) return undefined; // single paragraph
      item.message = entry.message;
    }
    todo.push(item);
  }
  return { onto: message.onto, todo };
}

class RebasePanel implements vscode.Disposable {
  private webviewReady = false;
  /** Plan to fetch and push once the webview completes its handshake
   *  (undefined on serializer restore — the panel starts docked/idle). */
  private pendingOnto: string | undefined;
  /** Host-held trust anchor: the onto of the plan currently pushed. Only
   *  its echo is accepted in executeRebase. */
  private onto: string | undefined;
  /** Monotonic fetch sequence: a re-target mid-flight drops the older
   *  getRebasePlan response. */
  private fetchSeq = 0;
  /** In-flight rebase RPC guard: one start/continue/skip/abort at a time. */
  private executing = false;
  /** Last engine-reported stop reason, replayed with status pushes so the
   *  docked view knows a conflict stop from a reword wait. */
  private lastStopReason: RebaseStopReason | undefined;
  private disposed = false;
  /** Shared conflict flow (conflict set query + conflicts-bucket reveal);
   *  refreshed on re-target. */
  private conflictFlow: ConflictFlow;
  private readonly messageSub: vscode.Disposable;
  private readonly disposables: vscode.Disposable[] = [];
  private readonly scheduleStatusRefresh = debounce(
    () => void this.pushStatus(),
    STATUS_DEBOUNCE_MS,
  );

  constructor(
    context: vscode.ExtensionContext,
    private readonly panel: vscode.WebviewPanel,
    private client: EngineClient,
    conflictFlow: ConflictFlow,
    onto: string | undefined,
  ) {
    this.pendingOnto = onto;
    this.conflictFlow = conflictFlow;
    panel.webview.html = buildWebviewHtml(context.extensionUri, panel.webview, 'rebase.html');
    this.messageSub = panel.webview.onDidReceiveMessage((message: RebasePanelMessage) =>
      this.onMessage(message),
    );
    // Engine notifications refresh the docked view (a rebase step moves
    // HEAD/index/worktree in bursts — the 200ms debounce coalesces them).
    // In plan mode these pushes are ignored webview-side (user edits are
    // never cleared by a refresh — validation happens at execute time).
    this.bindNotifications();
    panel.onDidDispose(() => {
      if (activePanel !== this) return;
      activePanel = undefined;
      this.dispose();
    });
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.fetchSeq++; // any in-flight plan fetch for this panel is dead
    this.messageSub.dispose();
    this.scheduleStatusRefresh.cancel();
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
  }

  /** Re-targets the panel at a new onto (a second graph right-click while
   *  the panel is open), refreshing the client/state/flow references so a
   *  restarted engine keeps the panel working. */
  retarget(client: EngineClient, conflictFlow: ConflictFlow, onto: string): void {
    this.teardownNotifications();
    this.client = client;
    this.conflictFlow = conflictFlow;
    this.onto = undefined;
    this.pendingOnto = onto;
    this.lastStopReason = undefined;
    this.bindNotifications();
    this.panel.reveal();
    if (this.webviewReady) void this.fetchAndPushPlan(onto);
  }

  /** Subscribes the panel to the engine's mutation notifications (see the
   *  constructor); re-bound on re-target against the refreshed client. */
  private bindNotifications(): void {
    for (const method of ['refsChanged', 'headChanged', 'worktreeChanged', 'indexChanged']) {
      this.disposables.push(
        this.client.onNotification(method, () => this.scheduleStatusRefresh()),
      );
    }
  }

  /** Drops the engine notification subscriptions (re-target re-binds them
   *  against the refreshed client; leaving the old ones would double-fire). */
  private teardownNotifications(): void {
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
    this.scheduleStatusRefresh.cancel();
  }

  /** Field-by-field dispatch of inbound webview messages (trust boundary). */
  private onMessage(message: RebasePanelMessage): void {
    if (this.disposed || !message || typeof message !== 'object') return;
    switch (message.type) {
      case 'ready':
        // First point where pushes reach the webview — the initial data
        // rides on this handshake (constructor postMessages are dropped).
        this.webviewReady = true;
        if (this.pendingOnto) {
          const onto = this.pendingOnto;
          void this.fetchAndPushPlan(onto);
        } else {
          void this.pushStatus(); // serializer restore: docked or idle view
        }
        return;
      case 'executeRebase': {
        const parsed = parseExecutePayload(message, this.onto);
        if (!parsed) {
          console.warn('[git-bamboo] rebase panel: dropped malformed executeRebase payload');
          return;
        }
        void this.execute(parsed.onto, parsed.todo);
        return;
      }
      case 'rebaseContinue':
        void this.step('continue');
        return;
      case 'rebaseSkip':
        void this.step('skip');
        return;
      case 'rebaseAbort':
        void this.step('abort');
        return;
      case 'refreshPlan':
        // User decision after a stale report: reload the plan for the
        // host-held onto (replacing the webview's edited todo is the
        // point of this action).
        if (this.onto) void this.fetchAndPushPlan(this.onto);
        return;
      case 'openConflicts':
        void showConflictsView();
        return;
      case 'close':
        this.close();
        return;
    }
  }

  /** Sends a message to the webview (no-op before the handshake). */
  private push(message: Record<string, unknown>): void {
    if (this.webviewReady && !this.disposed) {
      void this.panel.webview.postMessage(message);
    }
  }

  /** Fetches the plan for `onto` and pushes it; superseded fetches (a
   *  re-target mid-flight) are dropped. */
  private async fetchAndPushPlan(onto: string): Promise<void> {
    const seq = ++this.fetchSeq;
    this.push({ type: 'loading' });
    let plan;
    try {
      plan = await this.client.getRebasePlan(onto);
    } catch (err) {
      if (seq !== this.fetchSeq || this.disposed) return;
      this.push({ type: 'error', message: errorMessage(err) });
      return;
    }
    if (seq !== this.fetchSeq || this.disposed) return;
    if (plan.commits.length === 0) {
      // The graph entry gates this ("nothing to rebase"), so reaching
      // here means the range emptied between the click and the fetch.
      this.push({ type: 'empty' });
      return;
    }
    this.onto = plan.onto !== '' ? plan.onto : onto;
    this.lastStopReason = undefined;
    this.push({
      type: 'plan',
      onto: this.onto,
      branch: plan.branch,
      upstream: plan.upstream,
      commits: plan.commits,
    });
  }

  /** Executes the validated todo. Engine outcomes: stopped (normal —
   *  conflict or reword wait), finished, or INVALID_ARGUMENT — the stale
   *  plan (history moved under the user's edits) or a rebase already on
   *  disk. User edits are never cleared; the user decides via refreshPlan. */
  private async execute(onto: string, todo: RebaseTodoEntry[]): Promise<void> {
    if (this.executing) return; // in-flight guard: no double starts
    this.executing = true;
    this.push({ type: 'executing', active: true, op: 'execute' });
    try {
      const result = await this.client.startRebase(onto, todo);
      await this.handleResult(result, 'execute');
    } catch (err) {
      if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
        if (/already in progress/i.test(errorMessage(err))) {
          // A rebase is on disk (e.g. started externally): the docked view
          // is the honest state to show, not a stale-plan banner.
          this.push({ type: 'blocked', message: errorMessage(err) });
          await this.pushStatus();
        } else {
          // Stale todo chain: report, keep the user's edits untouched.
          this.push({ type: 'stale' });
        }
      } else {
        vscode.window.showErrorMessage(`Git Bamboo: rebase failed — ${errorMessage(err)}`);
      }
    } finally {
      this.executing = false;
      if (!this.disposed) this.push({ type: 'executing', active: false });
    }
  }

  /** continue / skip / abort on the in-progress rebase. The engine treats
   *  a continue with unresolved conflicts as INVALID_ARGUMENT (fail fast
   *  before spawning git) — surfaced as the panel's "blocked" notice; the
   *  user resolves in the conflicts bucket and retries. */
  async step(kind: 'continue' | 'skip' | 'abort'): Promise<void> {
    if (this.executing) return;
    this.executing = true;
    this.push({ type: 'executing', active: true, op: kind });
    try {
      const result =
        kind === 'continue'
          ? await this.client.rebaseContinue()
          : kind === 'skip'
            ? await this.client.rebaseSkip()
            : await this.client.rebaseAbort();
      await this.handleResult(result, kind);
    } catch (err) {
      if (rpcErrorCode(err) === RpcErrorCode.INVALID_ARGUMENT) {
        this.push({ type: 'blocked', message: errorMessage(err) });
        await this.pushStatus();
      } else {
        vscode.window.showErrorMessage(
          `Git Bamboo: rebase ${kind} failed — ${errorMessage(err)}`,
        );
      }
    } finally {
      this.executing = false;
      if (!this.disposed) this.push({ type: 'executing', active: false });
    }
  }

  /** Maps one rebase RPC result onto the panel: finished → green banner;
   *  stopped → docked view (status + conflict count + stop reason). */
  private async handleResult(result: RebaseResult, source: 'execute' | 'continue' | 'skip' | 'abort'): Promise<void> {
    if (this.disposed) return;
    if (!result.stopped) {
      this.lastStopReason = undefined;
      // Aborts report the (now gone) run as not-stopped: the branch is
      // back at the pre-rebase tip; label the banner accordingly.
      this.push({ type: 'finished', outcome: source === 'abort' ? 'aborted' : 'completed' });
      return;
    }
    this.lastStopReason = result.reason;
    await this.pushStatus();
  }

  /** Fetches and pushes the on-disk rebase status (docked view data).
   *  While stopped, the conflict count rides along (same engine status
   *  the Commit view's conflicts bucket renders — one source of truth),
   *  plus the last known stop reason. */
  private async pushStatus(): Promise<void> {
    if (this.disposed) return;
    let status: GetRebaseStatusResult;
    try {
      status = await this.client.getRebaseStatus();
    } catch {
      return; // transient engine failure: keep the last pushed view
    }
    let conflictCount: number | undefined;
    if (status.state !== 'idle') {
      const conflicts = await this.conflictFlow.currentConflicts();
      conflictCount = conflicts?.length ?? 0;
    }
    if (this.disposed) return;
    this.push({
      type: 'status',
      status,
      conflictCount,
      reason: status.state === 'idle' ? undefined : this.lastStopReason,
    });
  }

  /** Hidden palette command: ask the webview to run its confirmation
   *  modal and submit the todo (the edited todo lives webview-side). */
  requestExecute(): void {
    this.push({ type: 'requestExecute' });
  }

  close(): void {
    if (!this.disposed) this.panel.dispose();
  }
}
