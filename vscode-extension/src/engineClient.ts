/**
 * JSON-RPC 2.0 client over the engine process stdio with LSP-style framing:
 *   Content-Length: <n>\r\n\r\n<json>
 *
 * Responses and server-initiated notifications arrive interleaved on stdout;
 * they are demultiplexed by the presence of "id" (response) vs "method"
 * (notification). Pure Node — no vscode dependency, easy to unit test.
 */
import { spawn, type ChildProcess } from 'node:child_process';

/** Error carrying the engine's JSON-RPC error code (e.g. -32007 UNDO_BLOCKED). */
export class RpcError extends Error {
  constructor(message: string, readonly code: number) {
    super(message);
    this.name = 'RpcError';
  }
}

export type NotificationHandler = (params: Record<string, unknown>) => void;

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
}

const HEADER_SEPARATOR = Buffer.from('\r\n\r\n');

export class EngineClient {
  private readonly proc: ChildProcess;
  private nextId = 1;
  private readonly pending = new Map<number, PendingRequest>();
  private buffer: Buffer = Buffer.alloc(0);
  private readonly notificationHandlers = new Map<string, Set<NotificationHandler>>();
  private disposed = false;

  constructor(binaryPath: string, args: string[] = []) {
    this.proc = spawn(binaryPath, args, { stdio: ['pipe', 'pipe', 'pipe'] });
    this.proc.stdout!.on('data', (chunk: Buffer) => this.onStdoutData(chunk));
    this.proc.stderr!.on('data', (chunk: Buffer) => {
      const text = chunk.toString('utf8').trim();
      if (text) console.error(`[git-workbench-engine] ${text}`);
    });
    this.proc.on('error', (err) => this.rejectAllPending(new Error(`engine process error: ${err.message}`)));
    this.proc.on('exit', (code, signal) => {
      if (!this.disposed) {
        this.rejectAllPending(new Error(`engine exited unexpectedly (${signal ?? `code ${code}`})`));
      }
    });
  }

  /** Send a JSON-RPC request; resolves with the "result" field. */
  request<T = unknown>(method: string, params?: Record<string, unknown>): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const stdin = this.proc.stdin;
      if (this.disposed || !stdin || stdin.destroyed) {
        reject(new Error('Git Workbench engine is not running'));
        return;
      }
      const id = this.nextId++;
      this.pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
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

  /** Graceful shutdown request (best-effort, 2s cap), then kill the child. */
  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    try {
      // Send shutdown directly: request() refuses new calls once disposed.
      const id = this.nextId++;
      const settled = new Promise<void>((resolve) => {
        this.pending.set(id, { resolve: () => resolve(), reject: () => resolve() });
      });
      this.writeMessage({ jsonrpc: '2.0', id, method: 'shutdown' });
      await Promise.race([
        settled,
        new Promise((_resolve, reject) =>
          setTimeout(() => reject(new Error('shutdown timeout')), 2000),
        ),
      ]);
    } catch {
      // Engine may already be gone — fall through to kill.
    }
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
    if (message.error) {
      const error = message.error as { code?: number; message?: string };
      pending.reject(new RpcError(error.message ?? 'engine error', error.code ?? -32603));
    } else {
      pending.resolve(message.result);
    }
  }

  private rejectAllPending(err: Error): void {
    for (const pending of this.pending.values()) pending.reject(err);
    this.pending.clear();
  }
}
