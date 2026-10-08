/** Small helpers shared across extension host modules. */

/** Full commit ids as the engine reports them (SHA-1: 40 hex, SHA-256: 64).
 *  Shared by host modules that validate commit ids before use — webview-
 *  supplied ids are untrusted. */
export const COMMIT_ID_RE = /^[0-9a-f]{40,64}$/i;

export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
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
