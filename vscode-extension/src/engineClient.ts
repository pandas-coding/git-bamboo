/**
 * JSON-RPC 2.0 client over the engine process stdio with LSP-style framing:
 *   Content-Length: <n>\r\n\r\n<json>
 *
 * Responses and server-initiated notifications arrive interleaved on stdout;
 * they are demultiplexed by the presence of "id" (response) vs "method"
 * (notification). Pure Node — no vscode dependency, easy to unit test.
 */
import { spawn, type ChildProcess } from 'node:child_process';
import type { GetHeadResult, Ref } from './types';

/** Error carrying the engine's JSON-RPC error code (e.g. -32007 UNDO_BLOCKED). */
export class RpcError extends Error {
  /** Optional structured error payload from the engine (e.g. epoch data). */
  readonly data?: unknown;

  constructor(message: string, readonly code: number, data?: unknown) {
    super(message);
    this.name = 'RpcError';
    this.data = data;
  }
}

export type NotificationHandler = (params: Record<string, unknown>) => void;

export type ExitHandler = (code: number | null, signal: string | null) => void;

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  /** Timeout handle for requests made with an explicit timeout. */
  timer?: NodeJS.Timeout;
}

const HEADER_SEPARATOR = Buffer.from('\r\n\r\n');

export class EngineClient {
  private readonly proc: ChildProcess;
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private buffer: Buffer = Buffer.alloc(0);
  private readonly notificationHandlers = new Map<string, Set<NotificationHandler>>();
  private readonly exitHandlers = new Set<ExitHandler>();
  private shutdownTimer: NodeJS.Timeout | undefined;
  private disposed = false;

  constructor(binaryPath: string, args: string[] = []) {
    this.proc = spawn(binaryPath, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    // EPIPE when the engine exits while we are still writing (e.g. during
    // shutdown) must not crash the extension host.
    this.proc.stdin!.on('error', (err) => {
      console.error(`[git-workbench] engine stdin error: ${err.message}`);
    });
    this.proc.stdout!.on('data', (chunk: Buffer) => this.onStdoutData(chunk));
    this.proc.stderr!.on('data', (chunk: Buffer) => {
      const text = chunk.toString('utf8').trim();
      if (text) console.error(`[git-workbench-engine] ${text}`);
    });
    this.proc.on('error', (err) => {
      // Spawn failure or runtime I/O error: tear the client down before
      // surfacing the error to pending callers.
      if (this.disposed) return;
      console.error(`[git-workbench] engine process error: ${err.message}`);
      this.rejectAllPending(new Error(`engine process error: ${err.message}`));
      void this.dispose();
    });
    this.proc.on('exit', (code, signal) => {
      if (this.disposed) {
        // Clean exit during dispose: stop the shutdown timeout.
        if (this.shutdownTimer) clearTimeout(this.shutdownTimer);
        return;
      }
      // Limitation: no auto-restart is implemented — the user must reload
      // the window to restart Git Workbench.
      console.error(
        `[git-workbench] engine exited unexpectedly (${signal ?? `code ${code}`}); ` +
          'auto-restart is not implemented — reload the window to restart Git Workbench',
      );
      this.rejectAllPending(new Error(`engine exited unexpectedly (${signal ?? `code ${code}`})`));
      for (const handler of [...this.exitHandlers]) {
        try {
          handler(code, signal);
        } catch (err) {
          console.error('[git-workbench] exit handler failed:', err);
        }
      }
    });
  }

  /** Send a JSON-RPC request; resolves with the "result" field. */
  request<T = unknown>(method: string, params?: Record<string, unknown>, timeoutMs?: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const stdin = this.proc.stdin;
      if (this.disposed || !stdin || stdin.destroyed) {
        reject(new Error('Git Workbench engine is not running'));
        return;
      }
      const id = this.nextId++;
      let timer: NodeJS.Timeout | undefined;
      if (timeoutMs !== undefined) {
        timer = setTimeout(() => {
          this.pending.delete(id);
          reject(new Error(`Git Workbench: '${method}' request timed out after ${timeoutMs}ms`));
        }, timeoutMs);
      }
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject, timer });
      const message: Record<string, unknown> = { jsonrpc: '2.0', id, method };
      if (params !== undefined) message.params = params;
      this.writeMessage(message);
    });
  }

  /** Register a handler for a server-initiated notification; returns a disposer. */
  onNotification(method: string, handler: NotificationHandler): { dispose(): void } {
    let handlers = this.notificationHandlers.get(method);
    if (!handlers) {
      handlers = new Set();
      this.notificationHandlers.set(method, handlers);
    }
    handlers.add(handler);
    return { dispose: () => handlers!.delete(handler) };
  }

  /** Register a handler fired when the engine exits unexpectedly; returns a disposer. */
  onExit(handler: ExitHandler): { dispose(): void } {
    this.exitHandlers.add(handler);
    return { dispose: () => this.exitHandlers.delete(handler) };
  }

  /**
   * Engine RPC `getBlob`: UTF-8 content of `path` at `revision`. Resolves
   * with an empty string when the path doesn't exist in that revision.
   */
  async getBlob(revision: string, path: string): Promise<string> {
    const result = await this.request<unknown>('getBlob', { revision, path }, 15_000);
    return typeof result === 'string' ? result : '';
  }

  /**
   * Engine RPC `getHead`: the current HEAD commit sha plus the short
   * branch name (null when HEAD is detached). Works for linked worktrees,
   * unlike parsing `.git/HEAD` directly.
   */
  async getHead(): Promise<GetHeadResult> {
    const result = await this.request<Partial<GetHeadResult>>('getHead', {}, 5_000);
    return { head: typeof result?.head === 'string' ? result.head : '', branch: result?.branch ?? null };
  }

  /** Engine RPC `getRefs`: branches, tags, and remote-tracking refs. */
  async getRefs(): Promise<Ref[]> {
    const result = await this.request<unknown>('getRefs', {}, 5_000);
    return Array.isArray(result) ? (result as Ref[]) : [];
  }

  /** Graceful shutdown request (best-effort, 2s cap), then kill the child. */
  async dispose(): Promise<void> {
    // Guard against double-dispose (deactivate + context disposal).
    if (this.disposed) return;
    this.disposed = true;
    try {
      // Send shutdown directly: request() refuses new calls once disposed.
      const id = this.nextId++;
      const settled = new Promise<void>((resolve) => {
        this.pending.set(id, { resolve: () => resolve(), reject: () => resolve() });
      });
      this.writeMessage({ jsonrpc: '2.0', id, method: 'shutdown' });
      const timeout = new Promise<void>((resolve) => {
        this.shutdownTimer = setTimeout(resolve, 2000);
      });
      await Promise.race([settled, timeout]);
    } catch {
      // Engine may already be gone — fall through to kill.
    }
    // Clear the shutdown timeout (the 'exit' handler also clears it on
    // clean exit, but the engine may respond without exiting).
    if (this.shutdownTimer) clearTimeout(this.shutdownTimer);
    this.exitHandlers.clear();
    this.notificationHandlers.clear();
    this.rejectAllPending(new Error('engine client disposed'));
    this.proc.kill();
  }

  private writeMessage(message: Record<string, unknown>): void {
    const body = Buffer.from(JSON.stringify(message), 'utf8');
    this.proc.stdin!.write(`Content-Length: ${body.length}\r\n\r\n`, 'utf8');
    this.proc.stdin!.write(body);
  }

  private onStdoutData(chunk: Buffer): void {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    // Extract as many complete frames as the buffer currently holds.
    for (;;) {
      const separatorIndex = this.buffer.indexOf(HEADER_SEPARATOR);
      if (separatorIndex < 0) return; // incomplete header — wait for more data
      const headerText = this.buffer.subarray(0, separatorIndex).toString('utf8');
      const bodyStart = separatorIndex + HEADER_SEPARATOR.length;
      const match = /content-length:\s*(\d+)/i.exec(headerText);
      if (!match) {
        // Malformed header: drop it and resync at the next frame boundary.
        this.buffer = this.buffer.subarray(bodyStart);
        continue;
      }
      const contentLength = Number(match[1]);
      if (this.buffer.length < bodyStart + contentLength) return; // incomplete body
      const body = this.buffer.subarray(bodyStart, bodyStart + contentLength);
      this.buffer = this.buffer.subarray(bodyStart + contentLength);
      let message: unknown;
      try {
        message = JSON.parse(body.toString('utf8'));
      } catch {
        console.error('[git-workbench] dropping malformed JSON-RPC frame');
        continue;
      }
      this.handleMessage(message as Record<string, unknown>);
    }
  }

  private handleMessage(message: Record<string, unknown>): void {
    // Notification (no id): dispatch to registered handlers.
    if (typeof message.method === 'string' && message.id === undefined) {
      const handlers = this.notificationHandlers.get(message.method);
      const params = (message.params ?? {}) as Record<string, unknown>;
      for (const handler of handlers ?? []) {
        try {
          handler(params);
        } catch (err) {
          console.error(`[git-workbench] handler for ${message.method} failed:`, err);
        }
      }
      return;
    }
    // Response: resolve/reject the matching pending request.
    const id = message.id;
    if (typeof id !== 'number') return;
    const pending = this.pending.get(id);
    if (!pending) return;
    this.pending.delete(id);
    if (pending.timer) clearTimeout(pending.timer);
    if (message.error) {
      const error = message.error as { code?: number; message?: string; data?: unknown };
      pending.reject(new RpcError(error.message ?? 'engine error', error.code ?? -32603, error.data));
    } else {
      pending.resolve(message.result);
    }
  }

  private rejectAllPending(err: Error): void {
    for (const pending of this.pending.values()) {
      if (pending.timer) clearTimeout(pending.timer);
      pending.reject(err);
    }
    this.pending.clear();
  }
}
