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
import type { GraphPage, RepoState } from './types';
import { errorMessage } from './util';

/** JSON-RPC error code the engine returns when the client's epoch is stale. */
const EPOCH_MISMATCH = -32004;

/** Debounce for viewport requests coming from scroll events. */
const VIEWPORT_DEBOUNCE_MS = 16;

interface GraphViewMessage {
  type?: string;
  offset?: number;
  limit?: number;
  anchorCommit?: string | null;
}

export class GraphSession implements vscode.Disposable {
  private debounceTimer: NodeJS.Timeout | undefined;
  /** Coalesces invalidation bursts from the engine's fs watcher. */
  private invalidateTimer: NodeJS.Timeout | undefined;
  /** Sequence number so stale getGraph responses are dropped. */
  private requestSeq = 0;
  private disposed = false;
  private readonly disposables: vscode.Disposable[] = [];

  constructor(
    private readonly client: EngineClient,
    private readonly state: RepoState,
    private readonly webview: vscode.Webview,
    /** Fired by the host when its view/panel goes away (unbind + cleanup). */
    onDispose: () => void,
  ) {
    this.disposables.push(
      this.webview.onDidReceiveMessage((message: GraphViewMessage) => this.onMessage(message)),
      client.onNotification('graphInvalidated', (params) => {
        const epoch = numberField(params, 'epoch', this.state.epoch);
        this.invalidate(epoch);
      }),
      client.onNotification('refsChanged', () => void this.sendRefs()),
      { dispose: onDispose },
    );
    void this.sendRefs();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.debounceTimer) clearTimeout(this.debounceTimer);
    if (this.invalidateTimer) clearTimeout(this.invalidateTimer);
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
    }
  }

  private async requestGraph(offset: number, limit: number, anchor: string | null, retried = false): Promise<void> {
    if (this.disposed) return;
    const seq = ++this.requestSeq;
    try {
      const page = await this.client.request<GraphPage>('getGraph', {
        viewport: { offset, limit, anchor_commit: anchor, epoch: this.state.epoch },
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
