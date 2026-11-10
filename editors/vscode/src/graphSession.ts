/**
 * Host-agnostic core of the commit graph webview: viewport request
 * debouncing, stale-response dropping (request sequence numbers),
 * EPOCH_MISMATCH single retry, invalidation coalescing, ref pushes, and
 * engine notification subscriptions. Shared by the sidebar view host
 * (graphWebview.ts) and the editor-area panel host (graphPanel.ts) —
 * both attach a vscode.Webview and get identical behavior.
 */
import * as vscode from 'vscode';
import { EngineClient, RpcError } from './engineClient';
import { ConflictFlow } from './conflictFlow';
import { openCommitDetailPanel } from './commitDetail';
import { createContextTargetSlot } from './contextTargetSlot';
import * as netOps from './netOps';
import { runRebaseStep } from './rebaseControl';
import type { GraphFilter, GraphPage, RepoState } from './types';
import {
  BRANCH_STATUS_DEBOUNCE_MS,
  COMMIT_ID_RE,
  debounce,
  errorMessage,
  isDetachedError,
} from './util';

/** JSON-RPC error code the engine returns when the client's epoch is stale. */
const EPOCH_MISMATCH = -32004;

/** Debounce for viewport requests coming from scroll events. */
const VIEWPORT_DEBOUNCE_MS = 16;

/** Debounce for rebase-status refreshes: every rebase step (ours, the
 *  panel's, or an external CLI's) moves HEAD/worktree/index in a burst,
 *  and the banner only needs the settled state. */
const REBASE_STATUS_DEBOUNCE_MS = 200;

/** The rebase control ops the graph banner can request (webview → host). */
const REBASE_CONTROLS = new Set(['continue', 'skip', 'abort']);

/** The net-op kinds the graph toolbar can request (webview → host). */
type NetOp = 'fetch' | 'pull' | 'push' | 'sync' | 'publish';

/** Toolbar op → shared netOps flow (the toolbar runs the exact
 *  implementations the command palette uses — no per-surface copies).
 *  The webview's op string is validated against this table's keys (trust
 *  boundary) and dispatched through it: one table, no double dispatch. */
const NET_OPS: Record<NetOp, (ctx: netOps.NetOpsContext) => Promise<void>> = {
  fetch: netOps.runFetchAll,
  pull: netOps.runPull,
  push: netOps.runPush,
  sync: netOps.runSync,
  publish: netOps.runPublish,
};

/** host → webview branchStatus payload (upstream/ahead/behind for the
 *  toolbar badge; `detached` disables the branch-bound buttons). */
interface BranchStatusMessage {
  detached: boolean;
  upstream: string | null;
  ahead: number;
  behind: number;
}

/** Lane-area width bounds, mirrored from the webview (graph.js) so the host
 *  can validate webview-supplied widths before persisting them. */
const GRAPH_MIN_WIDTH = 44;
const GRAPH_MAX_WIDTH = 240;

interface GraphViewMessage {
  type?: string;
  offset?: number;
  limit?: number;
  anchorCommit?: string | null;
  /** setFilter payload: the full filter conditions (webview → host).
   *  Parsed by `parseWebviewFilter` before use — the webview is a trust
   *  boundary. */
  filter?: unknown;
  /** graphWidthChanged payload: the new manual width, or null to clear the
   *  override and return to auto width. */
  width?: number | null;
  /** contextTarget payload: the commit under the webview's context menu
   *  (null = right-click landed on a non-commit row; clears any target). */
  commit?: { id?: unknown; message?: unknown } | null;
  /** openCommitDetail payload: the full id of the single-clicked commit. */
  id?: unknown;
  /** netOp payload: which toolbar operation the webview requested. */
  op?: unknown;
  /** rebaseControl payload: which banner control the webview requested. */
  rebaseOp?: unknown;
}

/** Which webview host owns a graph session — widths are persisted per host. */
export type GraphHost = 'sidebar' | 'panel';

/** globalState key for the manual lane-area width of a given host. */
function widthKey(host: GraphHost): string {
  return `gitBamboo.graphWidth.${host}`;
}

/** The commit a graph webview right-clicked, as shown in its row. */
export interface GraphContextCommit {
  id: string;
  message: string;
}

/** The parked context-menu target: the commit plus the engine client of the
 *  session whose webview was right-clicked (commands use that client). */
interface GraphContextTarget {
  client: EngineClient;
  commit: GraphContextCommit;
  /** Owning session — the target is dropped when the session goes away. */
  owner: GraphSession;
}

/** The pending webview context-menu target (weak single-slot registry). */
const graphContextSlot = createContextTargetSlot<GraphContextTarget>();

/** Live graph sessions (both hosts). The command palette's filter entry
 *  (`gitBamboo.graph.filter`) applies a filter to every open graph
 *  surface of one engine client through this registry; sessions remove
 *  themselves on dispose. */
const liveSessions = new Set<GraphSession>();

/** Apply a graph filter to every live graph session served by `client`
 * (the command-palette entry point — the same session API the webview's
 * filter box uses). */
export function applyGraphFilter(client: EngineClient, filter: GraphFilter): void {
  for (const session of liveSessions) {
    if (session.serves(client)) session.setFilter(filter);
  }
}

/** The current filter of the first live session served by `client`: a
 *  filter object, null (no filter), or undefined when no graph surface
 *  is open (the command palette uses that to explain itself). */
export function currentGraphFilter(client: EngineClient): GraphFilter | null | undefined {
  for (const session of liveSessions) {
    if (session.serves(client)) return session.activeFilter;
  }
  return undefined;
}

/** Trim a filter's string fields and drop empty ones; an all-empty
 *  filter normalizes to null (no filter — the unfiltered path). */
export function normalizeGraphFilter(filter: GraphFilter): GraphFilter | null {
  const trim = (value: string | null): string | null => {
    const trimmed = value?.trim();
    return trimmed ? trimmed : null;
  };
  const normalized: GraphFilter = {
    author: trim(filter.author),
    message: trim(filter.message),
    path: trim(filter.path),
  };
  return normalized.author || normalized.message || normalized.path ? normalized : null;
}

/** Whether two normalized filters are the same (null = unfiltered). */
function sameFilter(a: GraphFilter | null, b: GraphFilter | null): boolean {
  return a?.author === b?.author && a?.message === b?.message && a?.path === b?.path;
}

/** Parse a webview-supplied filter payload (trust boundary). Every
 *  field must be null/absent or a plain string; the payload is then
 *  normalized (trim, drop empty fields). The LENGTH and
 *  control-character rules live ONLY in the engine
 *  (`validate_graph_filter` — the single enforcement point); a
 *  rules-violating payload surfaces as a typed RPC error instead of
 *  being silently dropped here. Returns a normalized filter (null = no
 *  conditions), or undefined when the payload is malformed — the caller
 *  ignores those. */
function parseWebviewFilter(payload: unknown): GraphFilter | null | undefined {
  const field = (value: unknown): string | null | undefined => {
    if (value === null || value === undefined) return null;
    if (typeof value !== 'string') return undefined;
    return value;
  };
  if (payload === null || payload === undefined) return null;
  if (typeof payload !== 'object' || Array.isArray(payload)) return undefined;
  const record = payload as Record<string, unknown>;
  const author = field(record.author);
  const message = field(record.message);
  const path = field(record.path);
  if (author === undefined || message === undefined || path === undefined) {
    return undefined;
  }
  return normalizeGraphFilter({ author, message, path });
}

/** Returns (and clears) the commit targeted by the graph context menu, if
 *  any. Called by the context-menu command handlers in extension.ts. */
export function takeGraphContextTarget():
  | { client: EngineClient; commit: GraphContextCommit }
  | undefined {
  return graphContextSlot.take();
}

export class GraphSession implements vscode.Disposable {
  private debounceTimer: NodeJS.Timeout | undefined;
  /** Coalesces invalidation bursts from the engine's fs watcher. */
  private invalidateTimer: NodeJS.Timeout | undefined;
  /** Debounced branchStatus refresh (refsChanged/headChanged bursts) —
   *  shared trailing-debounce helper, cancelled on dispose. */
  private readonly scheduleBranchStatus = debounce(
    () => void this.sendBranchStatus(),
    BRANCH_STATUS_DEBOUNCE_MS,
  );
  /** Debounced rebaseStatus refresh (rebase step notification bursts). */
  private readonly scheduleRebaseStatus = debounce(
    () => void this.sendRebaseStatus(),
    REBASE_STATUS_DEBOUNCE_MS,
  );
  /** Sequence number so stale getRebaseStatus responses are dropped. */
  private rebaseStatusSeq = 0;
  /** Sequence number so stale getGraph responses are dropped. */
  private requestSeq = 0;
  /** Current graph filter (normalized; null = unfiltered). Carried on
   *  every getGraph request and the EPOCH_MISMATCH retry so the filtered
   *  view survives invalidations (the epoch probe stays unfiltered — it
   *  must stay cheap; see `refreshAfterInvalidation`). */
  private filter: GraphFilter | null = null;
  private disposed = false;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly client: EngineClient,
    private readonly state: RepoState,
    /** Shared conflict flow: conflict-count queries for the rebase banner
     *  (the same controller the Commit view and rebase panel use). */
    private readonly conflictFlow: ConflictFlow,
    private readonly webview: vscode.Webview,
    /** Which host this session serves — width persistence is per-host. */
    private readonly host: GraphHost,
    /** Used for globalState-backed width persistence. */
    private readonly context: vscode.ExtensionContext,
    /** Fired by the host when its view/panel goes away (unbind + cleanup). */
    onDispose: () => void,
  ) {
    this.disposables.push(
      this.webview.onDidReceiveMessage((message: GraphViewMessage) => this.onMessage(message)),
      client.onNotification('graphInvalidated', (params) => {
        const epoch = numberField(params, 'epoch', this.state.epoch);
        this.invalidate(epoch);
      }),
      client.onNotification('refsChanged', () => {
        void this.sendRefs();
        this.scheduleBranchStatus();
        // Rebase banner: any mutation that can change the on-disk rebase
        // state schedules a disk-state re-read (debounced).
        this.scheduleRebaseStatus();
      }),
      client.onNotification('headChanged', () => {
        void this.sendHead();
        this.scheduleBranchStatus();
        this.scheduleRebaseStatus();
      }),
      client.onNotification('worktreeChanged', () => this.scheduleRebaseStatus()),
      client.onNotification('indexChanged', () => this.scheduleRebaseStatus()),
      { dispose: onDispose },
    );
    // Initial refs/HEAD are pushed from the 'ready' handler: messages posted
    // here (before the webview document loads its listener) are dropped.
    liveSessions.add(this);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    liveSessions.delete(this);
    graphContextSlot.clearIf((target) => target.owner === this);
    if (this.debounceTimer) clearTimeout(this.debounceTimer);
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    this.scheduleBranchStatus.cancel();
    this.scheduleRebaseStatus.cancel();
    // Unsubscribe engine notification listeners and webview subscriptions.
    for (const disposable of this.disposables) disposable.dispose();
    this.disposables.length = 0;
  }

  /** Called on engine graphInvalidated notifications (coalesced). */
  invalidate(epoch: number): void {
    this.state.epoch = epoch;
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
    this.invalidateTimer = setTimeout(() => {
      this.invalidateTimer = undefined;
      void this.refreshAfterInvalidation(epoch);
    }, VIEWPORT_DEBOUNCE_MS);
  }

  /**
   * Refresh the epoch BEFORE re-requesting: after an invalidation the
   * client's epoch is stale by definition, so re-requesting with it would
   * guarantee an EPOCH_MISMATCH loop. The probe sends epoch 0, which the
   * engine treats as "no validation", and adopts the fresh epoch from the
   * response before the webview re-requests its viewport.
   */
  private async refreshAfterInvalidation(notifiedEpoch: number): Promise<void> {
    try {
      const probe = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset: 0, limit: 1, anchor_commit: null, epoch: 0 },
        // The probe deliberately carries NO filter: it is a cheap epoch
        // probe, and any epoch bump invalidates the engine's memoized
        // filtered views — carrying the filter would rebuild the whole
        // (potentially expensive) view just to answer "what epoch is
        // it?". The webview's re-request does that work exactly once,
        // under the new epoch.
      });
      this.state.epoch = probe.epoch;
    } catch (err) {
      console.error('[git-bamboo] epoch probe after invalidation failed:', err);
      this.state.epoch = notifiedEpoch;
    }
    await this.webview.postMessage({ type: 'graphInvalidated', epoch: this.state.epoch });
  }

  private onMessage(message: GraphViewMessage): void {
    if (message?.type === 'viewportChanged') {
      const offset = Math.max(0, message.offset ?? 0);
      const limit = Math.min(2000, Math.max(1, message.limit ?? 50));
      const anchor = message.anchorCommit ?? null;
      if (this.debounceTimer) clearTimeout(this.debounceTimer);
      this.debounceTimer = setTimeout(() => void this.requestGraph(offset, limit, anchor), VIEWPORT_DEBOUNCE_MS);
    } else if (message?.type === 'ready') {
      // Webview finished initializing: by the time it can send this, its
      // message listener is up — so this is the first point where pushes
      // actually reach it. Hand it the persisted width (sanitized on read:
      // globalState is durable and older/corrupt entries must not leak
      // through), plus the current refs and HEAD.
      const stored = this.context.globalState.get<unknown>(widthKey(this.host), null);
      const width =
        typeof stored === 'number' && Number.isFinite(stored)
          ? Math.min(GRAPH_MAX_WIDTH, Math.max(GRAPH_MIN_WIDTH, stored))
          : null;
      void this.webview.postMessage({ type: 'graphWidth', width });
      void this.sendRefs();
      void this.sendHead();
      // Same handshake rule: this is the first point where pushes reach the
      // webview, so the initial branchStatus rides on 'ready' too — a
      // postMessage from the constructor would be silently dropped.
      void this.sendBranchStatus();
      // And the initial rebase banner state (a rebase already on disk —
      // engine- or externally-started — shows immediately).
      void this.sendRebaseStatus();
      // And the active filter, so a reloaded webview restores it (the
      // webview resets its local list on filterState and re-requests).
      void this.webview.postMessage({ type: 'filterState', filter: this.filter });
    } else if (message?.type === 'setFilter') {
      // Filter box change (debounced 300ms in the webview). Parsed and
      // sanitized before use; malformed payloads are dropped, the engine
      // double-validates everything again anyway.
      const filter = parseWebviewFilter(message.filter);
      if (filter !== undefined) {
        this.setFilter(filter);
      } else {
        console.warn('[git-bamboo] ignored malformed graph filter payload:', message.filter);
      }
    } else if (message?.type === 'graphWidthChanged') {
      // null clears the override (back to auto width); update() with
      // undefined removes the key from globalState. The webview is a trust
      // boundary and globalState is durable: only a finite in-range width
      // (or an explicit null) is persisted; anything else is ignored.
      const width = message.width;
      if (
        width === null ||
        (typeof width === 'number' &&
          Number.isFinite(width) &&
          width >= GRAPH_MIN_WIDTH &&
          width <= GRAPH_MAX_WIDTH)
      ) {
        void this.context.globalState.update(widthKey(this.host), width ?? undefined);
      }
    } else if (message?.type === 'contextTarget') {
      // Right-click on a commit row (fires before the native menu opens);
      // anything but a well-formed commit (null non-row click or a malformed
      // payload) clears the parked target so it can't outlive its gesture.
      const commit = message.commit;
      if (
        commit !== null &&
        typeof commit?.id === 'string' &&
        COMMIT_ID_RE.test(commit.id) &&
        typeof commit.message === 'string'
      ) {
        graphContextSlot.park({
          client: this.client,
          commit: { id: commit.id, message: commit.message },
          owner: this,
        });
      } else {
        graphContextSlot.clear();
      }
    } else if (message?.type === 'netOp') {
      // Toolbar button / badge click. The op is validated against the
      // NET_OPS table's keys before anything runs — the webview is a trust
      // boundary, so an unknown op is dropped, not stringly dispatched.
      const op = message.op;
      if (typeof op === 'string' && Object.hasOwn(NET_OPS, op)) {
        void this.runNetOp(op as NetOp);
      }
    } else if (message?.type === 'rebaseControl') {
      // Rebase banner control click. Same trust-boundary discipline as
      // netOp: the op is validated against the known control set before
      // any engine write RPC runs.
      const op = message.rebaseOp;
      if (typeof op === 'string' && REBASE_CONTROLS.has(op)) {
        void this.runRebaseControl(op as 'continue' | 'skip' | 'abort');
      }
    } else if (message?.type === 'openCommitDetail') {
      // Single-click on a commit row opens the detail panel — the chosen UX
      // over select-then-double-click (inspection stays one gesture away).
      // The webview never fires this for skeleton rows, but it is still a
      // trust boundary: a malformed payload (missing/non-string id, or an
      // id that is not a full commit sha) is ignored, mirroring the
      // contextTarget validation above — ids from the webview are
      // validated host-side before any use.
      const id = message.id;
      if (typeof id === 'string' && COMMIT_ID_RE.test(id)) {
        openCommitDetailPanel(this.context, this.client, this.state.root, id);
      }
    }
  }

  /** Set (or clear) the graph filter — the session API shared by the
   *  webview's filter box and the command palette. The webview resets
   *  its local list and re-requests from offset 0 on the pushed
   *  filterState, so both entry points get identical behavior; a filter
   *  equal to the current one is a no-op (no reset, no re-request). */
  setFilter(raw: GraphFilter | null): void {
    const filter = raw === null ? null : normalizeGraphFilter(raw);
    if (sameFilter(filter, this.filter)) return;
    this.filter = filter;
    // Every row is about to re-target: drop any parked right-click target
    // so a still-open context menu cannot act on a pre-filter commit
    // (e.g. checkout on a row that now shows different content).
    graphContextSlot.clearIf((target) => target.owner === this);
    // Invalidate in-flight getGraph responses: they carry the OLD
    // filter's data and would otherwise land on the freshly reset list
    // until the new page arrives.
    this.requestSeq++;
    void this.webview.postMessage({ type: 'filterState', filter });
  }

  /** The active filter (normalized; null = unfiltered). */
  get activeFilter(): GraphFilter | null {
    return this.filter;
  }

  /** Whether this session serves the given engine client — used by the
   *  palette filter entry to find the graph surfaces of one repo. */
  serves(client: EngineClient): boolean {
    return this.client === client;
  }

  private async requestGraph(offset: number, limit: number, anchor: string | null, retried = false): Promise<void> {
    if (this.disposed) return;
    const seq = ++this.requestSeq;
    try {
      const page = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset, limit, anchor_commit: anchor, epoch: this.state.epoch },
        ...(this.filter && { filter: this.filter }),
      });
      if (seq !== this.requestSeq || this.disposed) return; // superseded by a newer scroll position
      this.state.epoch = page.epoch;
      await this.webview.postMessage({ type: 'graphPage', offset, page });
    } catch (err) {
      // Retry only if this request is still the newest: a newer viewport
      // request already carries fresh data, and retrying here would bump
      // requestSeq past it and clobber its response with stale data.
      if (
        !retried &&
        seq === this.requestSeq &&
        !this.disposed &&
        err instanceof RpcError &&
        err.code === EPOCH_MISMATCH
      ) {
        // Our epoch is stale: retry ONCE with the fresh epoch from the
        // error (or epoch 0 = "no validation" if unavailable), then adopt
        // the epoch from the successful response.
        this.state.epoch = epochFromError(err);
        await this.requestGraph(offset, limit, anchor, true);
        return;
      }
      if (seq !== this.requestSeq || this.disposed) return;
      vscode.window.showErrorMessage(`Git Bamboo: graph request failed — ${errorMessage(err)}`);
    }
  }

  /** Pushes refs to the webview so it can render branch-tip tags. */
  private async sendRefs(): Promise<void> {
    if (this.disposed) return;
    try {
      const refs = await this.client.getRefs();
      await this.webview.postMessage({ type: 'refs', refs });
    } catch (err) {
      console.error('[git-bamboo] refs fetch for graph failed:', err);
    }
  }

  /** Pushes the current HEAD so the webview can mark the HEAD row. */
  private async sendHead(): Promise<void> {
    if (this.disposed) return;
    try {
      const { head, branch } = await this.client.getHead();
      await this.webview.postMessage({ type: 'head', sha: head, branch });
    } catch (err) {
      console.error('[git-bamboo] getHead for graph failed:', err);
    }
  }

  /** Runs one toolbar net op via the shared netOps flows (identical to
   *  the command palette: upstream detection, publish fallback, rejection
   *  recovery, conflict surfacing). The shared per-repo in-flight guard
   *  (netOps.tryBeginNetOp — one slot per engine client, shared with the
   *  palette commands and the other graph host) drops the op while any
   *  surface runs one: queueing it would let a stale click mutate refs
   *  mid-operation, the same freshness discipline the context-target slot
   *  applies. While it runs, netOpState disables the whole toolbar row. */
  private async runNetOp(op: NetOp): Promise<void> {
    if (this.disposed || !netOps.tryBeginNetOp(this.client)) return;
    const ctx = netOps.netOpsContext(this.client, this.state);
    try {
      await this.webview.postMessage({ type: 'netOpState', active: true, op });
      await NET_OPS[op](ctx);
    } catch (err) {
      // The flows surface their own errors; an escaping throw breaks that
      // contract — log it instead of leaving an unhandled rejection.
      console.error('[git-bamboo] net op failed:', err);
    } finally {
      netOps.endNetOp(this.client);
      if (!this.disposed) {
        await this.webview.postMessage({ type: 'netOpState', active: false, op: null });
      }
      // Refresh the badge even when the op moved no refs (a no-op fetch
      // emits no refsChanged): the user just waited on a spinner and
      // deserves a settled state, and the RPC is cheap.
      this.scheduleBranchStatus();
    }
  }

  /** Pushes the current on-disk rebase status so the webview can render
   *  the banner: stopped (progress + stopped-at + conflict count, controls
   *  enabled), running (read-only), or idle (banner hidden — CLI-started
   *  and engine-started rebases are indistinguishable here by design: the
   *  state is read from disk). The conflict count comes from the shared
   *  conflict flow (the same engine status the Commit view's conflicts
   *  bucket renders — one source of truth). Stale responses are dropped
   *  by sequence number; failures keep the last pushed banner. */
  private async sendRebaseStatus(): Promise<void> {
    if (this.disposed) return;
    const seq = ++this.rebaseStatusSeq;
    try {
      const status = await this.client.getRebaseStatus();
      if (seq !== this.rebaseStatusSeq || this.disposed) return;
      let conflicts: number | undefined;
      if (status.state !== 'idle') {
        const items = await this.conflictFlow.currentConflicts();
        if (seq !== this.rebaseStatusSeq || this.disposed) return;
        conflicts = items?.length ?? 0;
      }
      await this.webview.postMessage({ type: 'rebaseStatus', status, conflicts });
    } catch (err) {
      console.error('[git-bamboo] getRebaseStatus for graph failed:', err);
    }
  }

  /** Runs one rebase banner control (continue/skip/abort) through the
   *  shared rebase-step runner (rebaseControl.ts) — the same RPC,
   *  in-flight guard, and failure classification the rebase panel's
   *  docked view uses, no copies. A refused continue (unresolved
   *  conflicts) is the engine's INVALID_ARGUMENT — surfaced as a warning
   *  pointing at the Commit view's conflicts bucket. */
  private async runRebaseControl(op: 'continue' | 'skip' | 'abort'): Promise<void> {
    if (this.disposed) return;
    await this.webview.postMessage({ type: 'rebaseControlState', active: true });
    try {
      const outcome = await vscode.window.withProgress(
        {
          location: vscode.ProgressLocation.Notification,
          title: `Git Bamboo: rebase ${op}`,
        },
        () => runRebaseStep(this.client, op),
      );
      if (outcome.kind === 'refused') {
        vscode.window.showWarningMessage(
          `Git Bamboo: ${outcome.message} — resolve conflicts in the Commit view first.`,
        );
      } else if (outcome.kind === 'failed') {
        vscode.window.showErrorMessage(
          `Git Bamboo: rebase ${op} failed — ${outcome.message}`,
        );
      }
      // 'result' and 'dropped' need no UI here: the shared runner already
      // ran (or dropped) the op, and the settle call below refreshes the
      // banner either way.
    } catch (err) {
      // runRebaseStep classifies instead of throwing; an escaping throw
      // breaks that contract — log it instead of an unhandled rejection.
      console.error('[git-bamboo] rebase control failed:', err);
    } finally {
      if (!this.disposed) {
        await this.webview.postMessage({ type: 'rebaseControlState', active: false });
      }
      // Engine notifications also schedule this; the explicit call covers
      // ops that moved nothing (and settles the banner immediately).
      this.scheduleRebaseStatus();
    }
  }

  /** Pushes the current branch's upstream/ahead/behind so the webview can
   *  render the toolbar badge. A detached HEAD is reported as such (the
   *  webview disables the branch-bound buttons); other failures keep the
   *  last known state rather than blanking the badge. */
  private async sendBranchStatus(): Promise<void> {
    if (this.disposed) return;
    let payload: BranchStatusMessage | undefined;
    try {
      const status = await this.client.getBranchStatus();
      if (status) {
        payload = {
          detached: false,
          upstream: status.upstream,
          ahead: status.ahead,
          behind: status.behind,
        };
      }
    } catch (err) {
      if (isDetachedError(err)) {
        payload = { detached: true, upstream: null, ahead: 0, behind: 0 };
      } else {
        console.error('[git-bamboo] getBranchStatus for graph failed:', err);
      }
    }
    if (payload && !this.disposed) {
      await this.webview.postMessage({ type: 'branchStatus', ...payload });
    }
  }
}

/** Extracts the engine's current epoch from an EPOCH_MISMATCH error. */
function epochFromError(err: RpcError): number {
  const data = err.data as Record<string, unknown> | undefined;
  if (data) {
    for (const key of ['expected_epoch', 'expected', 'epoch']) {
      const value = data[key];
      if (typeof value === 'number') return value;
    }
  }
  const match = /expected (\d+)/.exec(err.message);
  return match ? Number(match[1]) : 0; // 0 = "no validation" probe
}

function numberField(params: Record<string, unknown>, key: string, fallback: number): number {
  const value = params[key];
  return typeof value === 'number' ? value : fallback;
}
