/**
 * Single-slot registry for webview context-menu targets, shared by the
 * Commit view and the graph sessions. A webview parks the row its native
 * context menu opened on (the contextmenu event fires before the menu
 * opens); the menu command the user picks then takes (and clears) it.
 *
 * Parked targets expire after CONTEXT_TARGET_TTL_MS: the native menu is
 * acted on (or dismissed) immediately, so anything not consumed within
 * the window is stale — this bounds how long programmatic invocation of
 * the hidden palette commands (`when: false`) can reach a target.
 */

/** How long a parked context-menu target stays usable. */
export const CONTEXT_TARGET_TTL_MS = 30_000;

export interface ContextTargetSlot<T> {
  /** Parks the target (replacing any prior one) and restarts the timer. */
  park(target: T): void;
  /** Returns and clears the parked target, if any. */
  take(): T | undefined;
  /** Clears unconditionally (view disposed / non-row right-click). */
  clear(): void;
  /** Clears only when the parked target matches (owner-scoped clearing). */
  clearIf(predicate: (target: T) => boolean): void;
}

export function createContextTargetSlot<T>(
  ttlMs: number = CONTEXT_TARGET_TTL_MS,
): ContextTargetSlot<T> {
  let target: T | undefined;
  let timer: NodeJS.Timeout | undefined;
  const clearTimer = (): void => {
    if (timer) clearTimeout(timer);
    timer = undefined;
  };
  return {
    park(next) {
      target = next;
      clearTimer();
      timer = setTimeout(() => {
        target = undefined;
        timer = undefined;
      }, ttlMs);
    },
    take() {
      clearTimer();
      const parked = target;
      target = undefined;
      return parked;
    },
    clear() {
      clearTimer();
      target = undefined;
    },
    clearIf(predicate) {
      if (target !== undefined && predicate(target)) this.clear();
    },
  };
}
