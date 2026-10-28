/**
 * JSON-RPC 2.0 client over the engine process stdio with LSP-style framing:
 *   Content-Length: <n>\r\n\r\n<json>
 *
 * Responses and server-initiated notifications arrive interleaved on stdout;
 * they are demultiplexed by the presence of "id" (response) vs "method"
 * (notification). Pure Node — no vscode dependency, easy to unit test.
 */
import { spawn, type ChildProcess } from 'node:child_process';
import type {
  BranchStatus,
  CommitDetail,
  CommitFile,
  ConflictResolution,
  ForceMode,
  GetHeadResult,
  PullStrategy,
  Ref,
  RemoteInfo,
} from './types';

/** Engine JSON-RPC error codes the client acts on (beyond raw display). */
export const RpcErrorCode = {
  /** Plain git failure (non-zero exit, no structured classification). */
  GIT_ERROR: -32002,
  /** Rejected request arguments (resolveConflict kind-mismatch /
   *  stale-snapshot rejection). */
  INVALID_ARGUMENT: -32005,
  /** Stash pop/apply conflict — the stash entry is retained. */
  STASH_CONFLICT: -32008,
  /** Push rejected (non-fast-forward / stale lease) — recovery is client-side. */
  PUSH_REJECTED: -32009,
  /** Auth failed / credentials unavailable — guide the user to a terminal. */
  AUTH_REQUIRED: -32010,
} as const;

/** Error code of `err` when it is an engine RpcError, else undefined —
 *  lets callers switch on structured codes without instanceof chains. */
export function rpcErrorCode(err: unknown): number | undefined {
  return err instanceof RpcError ? err.code : undefined;
}

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
      console.error(`[git-bamboo] engine stdin error: ${err.message}`);
    });
    this.proc.stdout!.on('data', (chunk: Buffer) => this.onStdoutData(chunk));
    this.proc.stderr!.on('data', (chunk: Buffer) => {
      const text = chunk.toString('utf8').trim();
      if (text) console.error(`[bamboo-engine] ${text}`);
    });
    this.proc.on('error', (err) => {
      // Spawn failure or runtime I/O error: tear the client down before
      // surfacing the error to pending callers.
      if (this.disposed) return;
      console.error(`[git-bamboo] engine process error: ${err.message}`);
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
      // the window to restart Git Bamboo.
      console.error(
        `[git-bamboo] engine exited unexpectedly (${signal ?? `code ${code}`}); ` +
          'auto-restart is not implemented — reload the window to restart Git Bamboo',
      );
      this.rejectAllPending(new Error(`engine exited unexpectedly (${signal ?? `code ${code}`})`));
      for (const handler of [...this.exitHandlers]) {
        try {
          handler(code, signal);
        } catch (err) {
          console.error('[git-bamboo] exit handler failed:', err);
        }
      }
    });
  }

  /** Send a JSON-RPC request; resolves with the "result" field. */
  request<T = unknown>(method: string, params?: Record<string, unknown>, timeoutMs?: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      const stdin = this.proc.stdin;
      if (this.disposed || !stdin || stdin.destroyed) {
        reject(new Error('Git Bamboo engine is not running'));
        return;
      }
      const id = this.nextId++;
      let timer: NodeJS.Timeout | undefined;
      if (timeoutMs !== undefined) {
        timer = setTimeout(() => {
          this.pending.delete(id);
          reject(new Error(`Git Bamboo: '${method}' request timed out after ${timeoutMs}ms`));
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

  /**
   * Engine RPC `getCommitDetail`: full metadata plus per-file stat for one
   * commit. Resolves null when the response is malformed or references a
   * missing/invalid commit, so callers can show an error surface instead of
   * a half-populated commit view.
   */
  async getCommitDetail(id: string): Promise<CommitDetail | null> {
    let result: unknown;
    try {
      result = await this.request<unknown>('getCommitDetail', { id }, 15_000);
    } catch (err) {
      console.error(`[git-bamboo] getCommitDetail ${id} failed:`, err);
      return null; // engine error (e.g. unknown commit): treat as unavailable
    }
    return isValidCommitDetail(result) ? result : null;
  }

  /**
   * Engine RPC `fetch`: `remote == null` means `--all`. Resolves with
   * git's (redacted) combined output.
   */
  fetch(remote: string | null): Promise<string> {
    const params: Record<string, unknown> = remote === null ? {} : { remote };
    return this.request<string>('fetch', params);
  }

  /**
   * Engine RPC `pull` of the current branch from its upstream (omit
   * remote/branch so the engine never silently re-targets another
   * branch). `strategy` defaults to `default` (git config decides);
   * `auto_stash` stashes a dirty worktree and re-applies afterwards.
   * Resolves with git's output; conflicts surface as -32008 or a
   * conflict-bearing GIT_ERROR.
   */
  pull(strategy: PullStrategy = 'default', autoStash = false): Promise<string> {
    return this.request<string>('pull', { strategy, auto_stash: autoStash });
  }

  /**
   * Engine RPC `push`. `force` is the three-tier enum (`none` for plain
   * pushes — force is only reachable from an explicit user-confirmed
   * flow); `setUpstream` publishes an unpublished branch (`push -u`).
   * Resolves with git's (redacted) output. Rejections surface as
   * -32009, auth failures as -32010.
   */
  push(
    remote: string,
    branch: string,
    force: ForceMode = 'none',
    setUpstream = false,
  ): Promise<string> {
    return this.request<string>('push', {
      remote,
      branch,
      force,
      set_upstream: setUpstream,
    });
  }

  /** Engine RPC `getRemotes`: all configured remotes (URLs credential-
   *  redacted server-side), with the default push target flagged.
   *  Malformed entries are dropped rather than half-trusted. */
  async getRemotes(): Promise<RemoteInfo[]> {
    const result = await this.request<unknown>('getRemotes', {}, 5_000);
    return Array.isArray(result) ? result.filter(isValidRemoteInfo) : [];
  }

  /** Engine RPC `getBranchStatus`: upstream + ahead/behind counts of
   *  `branch` (defaults to the current HEAD branch). Resolves null on a
   *  malformed response (same convention as `getCommitDetail`) so callers
   *  show a readable error instead of acting on a half-valid status. */
  async getBranchStatus(branch?: string): Promise<BranchStatus | null> {
    const result = await this.request<unknown>(
      'getBranchStatus',
      branch === undefined ? {} : { branch },
      5_000,
    );
    return isValidBranchStatus(result) ? result : null;
  }

  /**
   * Engine RPC `discard`: restore tracked paths from the index and delete
   * untracked-only paths. Irreversible by design — callers must show the
   * IRREVERSIBLE confirmation dialog before invoking this.
   */
  discard(paths: string[]): Promise<void> {
    return this.request<void>('discard', { paths });
  }

  /**
   * Engine RPC `resolveConflict`: resolve one conflicted path. The engine
   * re-classifies the conflict fresh at execution time and rejects a
   * resolution that does not fit the kind (e.g. `theirs` on a
   * deleted-by-them conflict) with INVALID_ARGUMENT. Resolves with the
   * engine's human-readable summary line — the wire result is an
   * `OutputResult` object (`{ output }`), unwrapped here.
   */
  async resolveConflict(path: string, resolution: ConflictResolution): Promise<string> {
    const result = await this.request<{ output?: unknown }>('resolveConflict', {
      path,
      resolution,
    });
    return typeof result?.output === 'string' ? result.output : '';
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
        console.error('[git-bamboo] dropping malformed JSON-RPC frame');
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
          console.error(`[git-bamboo] handler for ${message.method} failed:`, err);
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

/** Field-by-field guard for the `getCommitDetail` result: one wrong type
 *  (e.g. `files` not an array) makes the whole commit view unreliable, so
 *  callers get null rather than a partially valid object. */
function isValidCommitDetail(value: unknown): value is CommitDetail {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.id === 'string' &&
    typeof v.message === 'string' &&
    typeof v.author_name === 'string' &&
    typeof v.author_email === 'string' &&
    typeof v.author_time === 'number' &&
    typeof v.committer_name === 'string' &&
    typeof v.committer_email === 'string' &&
    typeof v.committer_time === 'number' &&
    Array.isArray(v.parent_ids) &&
    v.parent_ids.every((p) => typeof p === 'string') &&
    Array.isArray(v.files) &&
    v.files.every(isValidCommitFile)
  );
}

function isValidCommitFile(value: unknown): value is CommitFile {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  const status = v.status;
  return (
    typeof v.path === 'string' &&
    (typeof v.old_path === 'string' || v.old_path === null) &&
    (status === 'A' || status === 'D' || status === 'M' || status === 'R' || status === 'C' || status === 'T') &&
    typeof v.additions === 'number' &&
    typeof v.deletions === 'number'
  );
}

function isValidRemoteInfo(value: unknown): value is RemoteInfo {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.name === 'string' &&
    (typeof v.url === 'string' || v.url === null) &&
    typeof v.default === 'boolean'
  );
}

function isValidBranchStatus(value: unknown): value is BranchStatus {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.branch === 'string' &&
    (typeof v.upstream === 'string' || v.upstream === null) &&
    typeof v.ahead === 'number' &&
    typeof v.behind === 'number'
  );
}

