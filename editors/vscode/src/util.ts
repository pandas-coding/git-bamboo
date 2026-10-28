/** Small helpers shared across extension host modules. */

/** Full commit ids as the engine reports them (SHA-1: 40 hex, SHA-256: 64).
 *  Shared by host modules that validate commit ids before use — webview-
 *  supplied ids are untrusted. Lowercase hex only, mirroring the engine's
 *  `is_valid_commit_id` (the engine never reports uppercase ids). */
export const COMMIT_ID_RE = /^[0-9a-f]{40,64}$/;

export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}

/** Engine-side detached-HEAD failures arrive as ordinary errors whose
 *  message mentions "detached" (there is no dedicated RPC error code);
 *  the status surfaces (graph badge, netOps flows) detect them through
 *  this shared check instead of each re-matching the string. */
export function isDetachedError(err: unknown): boolean {
  return /detached/i.test(errorMessage(err));
}

/** Trailing-debounce interval for branchStatus reads: refsChanged fires
 *  in bursts (auto-fetch, pushes, branch switches) and each refresh costs
 *  one RPC — one trailing refresh per burst is enough. Shared by the
 *  status bar (extension.ts) and the graph sessions. */
export const BRANCH_STATUS_DEBOUNCE_MS = 200;

/** A trailing-debounce-wrapped `fn`: repeated calls collapse into one
 *  invocation `ms` after the last call. Also disposable — `cancel()` drops
 *  a pending invocation (for teardown, so no doomed RPC fires post-dispose). */
export interface Debounced {
  (): void;
  cancel(): void;
}

export function debounce(fn: () => void, ms: number): Debounced {
  let timer: NodeJS.Timeout | undefined;
  const wrapped = (): void => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = undefined;
      fn();
    }, ms);
  };
  wrapped.cancel = (): void => {
    if (timer) clearTimeout(timer);
    timer = undefined;
  };
  return wrapped;
}

/** Human-friendly relative age for undo entries, QuickPick details, etc. */
export function relativeTime(unixSeconds: number, nowMs: number = Date.now()): string {
  const seconds = Math.max(0, Math.floor(nowMs / 1000 - unixSeconds));
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days}d ago`;
  return new Date(unixSeconds * 1000).toLocaleDateString();
}
