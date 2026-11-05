/**
 * Shared inline SVG icon bodies (16x16 viewBox, stroke = currentColor) —
 * codicon-style, fully static markup (CSP-safe). Imported by the commit
 * and commitDetail webview entries; vite emits it as one shared chunk.
 */

export const ICONS = {
  stage: '<path d="M8 3.25v9.5M3.25 8h9.5"/>',
  unstage: '<path d="M3.25 8h9.5"/>',
  openChanges:
    '<path d="M2.75 5.25h9.5M9.75 3l2.5 2.25-2.5 2.25"/>' +
    '<path d="M13.25 10.75h-9.5M6.25 8.5l-2.5 2.25 2.5 2.25"/>',
  added: '<path d="M8 3.25v9.5M3.25 8h9.5"/>',
  deleted:
    '<path d="M3.25 4.5h9.5"/>' +
    '<path d="M6.5 4.5V3h3v1.5"/>' +
    '<path d="M4.75 4.5l.5 8.25h5.5l.5-8.25"/>' +
    '<path d="M6.75 7.25v3.5M9.25 7.25v3.5"/>',
  modified: '<path d="M11.25 3.75l1 1L6.5 9.5l-2 .5.5-2z"/>',
  renamed: '<path d="M3.25 8h9.5"/><path d="M9 4.25 12.75 8 9 11.75"/>',
  copied: '<path d="M5.5 5.5h7v7h-7z"/><path d="M10.5 5.5v-2h-7v7h2"/>',
  typechanged: '<path d="M8 2.75 14.25 13.25H1.75z"/><path d="M8 6.75v2.75"/><path d="M8 11.5v.01"/>',
  /** Merge-editor entry (conflict rows): two panes joined in the middle. */
  mergeEditor:
    '<path d="M2.5 2.5h11v11h-11z"/>' +
    '<path d="M8 2.5v11"/>' +
    '<path d="M6 6.75 4.75 8 6 9.25"/><path d="M10 6.75 11.25 8 10 9.25"/>',
  /** Accept one side of a conflict (check with a direction hint). */
  acceptOurs: '<path d="M3 8.5l2.25 2.25L9 6.25"/><path d="M11.75 3.75v3.75h-3.75"/>',
  acceptTheirs: '<path d="M3 8.5l2.25 2.25L9 6.25"/><path d="M4.25 3.75v3.75h3.75"/>',
  /** Stash section (phase 5): create (save/box), pop (out of box),
   *  apply (copy from box), drop (delete). */
  stashCreate: '<path d="M2.5 6h11v7h-11z"/><path d="M5.5 3.5h5V6h-5z"/><path d="M6.75 9.25v1.5M9.25 9.25v1.5"/>',
  stashPop: '<path d="M2.5 6h11v7h-11z"/><path d="M2.5 8.25h11"/><path d="M8 3.25 6 5.25l2 2"/>',
  stashApply: '<path d="M2.5 6h11v7h-11z"/><path d="M2.5 8.25h11"/><path d="M5.75 10.75h4.5"/>',
  stashDrop: '<path d="M3.25 4.5h9.5"/><path d="M6.5 4.5V3h3v1.5"/><path d="M4.75 4.5l.5 8.25h5.5l.5-8.25"/><path d="M6.75 7.25v3.5M9.25 7.25v3.5"/>',
  /** Rebase plan panel (phase 2): todo-row movement (drag handle, move
   *  up/down), execution controls (play/skip/stop), close, reload. */
  dragHandle: '<path d="M6 4h.01M10 4h.01M6 8h.01M10 8h.01M6 12h.01M10 12h.01"/>',
  moveUp: '<path d="M8 12.75V3.25"/><path d="M4.75 6.5 8 3.25l3.25 3.25"/>',
  moveDown: '<path d="M8 3.25v9.5"/><path d="M4.75 9.5 8 12.75l3.25-3.25"/>',
  play: '<path d="M5.25 3.75v8.5l7-4.25z"/>',
  skip: '<path d="M4 3.75v8.5l5.5-4.25z"/><path d="M11.75 3.75v8.5"/>',
  stop: '<path d="M4.5 4.5h7v7h-7z"/>',
  closeX: '<path d="M4.25 4.25l7.5 7.5M11.75 4.25l-7.5 7.5"/>',
  refresh:
    '<path d="M13.25 8a5.25 5.25 0 1 1-1.6-3.8"/>' +
    '<path d="M13.25 3.25v3.25h-3.25"/>',
};

/** Wraps an ICONS body in the shared 16x16 stroke SVG element markup. */
export function iconSvg(name) {
  return (
    `<svg width="16" height="16" viewBox="0 0 16 16" fill="none" ` +
    `stroke="currentColor" stroke-width="1.4" stroke-linecap="round" ` +
    `stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`
  );
}
