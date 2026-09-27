/** Small helpers shared across extension host modules. */

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
