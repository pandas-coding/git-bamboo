/**
 * Shared webview helpers (same rollup shared-chunk setup as icons.js —
 * vite emits this once for every entry that imports it). `shortSha` and
 * `formatTime` previously drifted into per-bundle copies across the
 * graph/commitDetail/rebase entries; one module stops that compounding.
 */

/** First 7 characters of a commit id (defensive for short/absent ids). */
export function shortSha(id) {
  return typeof id === 'string' && id.length >= 7 ? id.slice(0, 7) : String(id);
}

/** Locale-formatted date (plus hh:mm unless `withTime` is false) for a
 *  unix-seconds timestamp; '' on failure (invalid timestamps render as
 *  empty, never as a crash). */
export function formatTime(unixSeconds, withTime = true) {
  try {
    const options = withTime
      ? { year: 'numeric', month: 'short', day: '2-digit', hour: '2-digit', minute: '2-digit' }
      : { year: 'numeric', month: 'short', day: '2-digit' };
    return new Date(unixSeconds * 1000).toLocaleString(undefined, options);
  } catch {
    return '';
  }
}
