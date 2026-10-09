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
};

/** Wraps an ICONS body in the shared 16x16 stroke SVG element markup. */
export function iconSvg(name) {
  return (
    `<svg width="16" height="16" viewBox="0 0 16 16" fill="none" ` +
    `stroke="currentColor" stroke-width="1.4" stroke-linecap="round" ` +
    `stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`
  );
}
