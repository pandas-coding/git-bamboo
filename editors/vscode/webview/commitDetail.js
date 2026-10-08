/**
 * Commit detail webview (gitBamboo.commitDetail): full commit metadata —
 * author/committer identity lines, multi-line message, and the changed-file
 * list with per-file +/- counts and an "Open Changes" action per row.
 *
 * Host protocol (see src/commitDetail.ts):
 *   outbound: { type: 'openChanges', path, oldPath? }  (oldPath only when non-null)
 *   inbound:  { type: 'commitDetail', detail }         — engine CommitDetail (snake_case)
 *             { type: 'error' }                        — fetch failed; render error state
 *
 * All untrusted strings (message, names, emails, paths) go in via
 * textContent/DOM APIs — never innerHTML. The only innerHTML use is the
 * static, trusted SVG icon markup (ICONS/iconSvg, codicon-style 16x16
 * stroke SVGs), which is CSP-safe.
 */
const vscode = acquireVsCodeApi();

const els = {
  avatar: document.getElementById('avatar'),
  authorLine: document.getElementById('author-line'),
  authorName: document.getElementById('author-name'),
  authorTime: document.getElementById('author-time'),
  committerLine: document.getElementById('committer-line'),
  committerName: document.getElementById('committer-name'),
  committerTime: document.getElementById('committer-time'),
  message: document.getElementById('message'),
  filesSection: document.getElementById('files-section'),
  filesCount: document.getElementById('files-count'),
  rows: document.getElementById('rows'),
  empty: document.getElementById('empty-state'),
  error: document.getElementById('error-state'),
};

/** Inline SVG icon bodies (16x16 viewBox, stroke = currentColor) —
 *  codicon-style, fully static markup (CSP-safe). */
const ICONS = {
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
  openChanges:
    '<path d="M2.75 5.25h9.5M9.75 3l2.5 2.25-2.5 2.25"/>' +
    '<path d="M13.25 10.75h-9.5M6.25 8.5l-2.5 2.25 2.5 2.25"/>',
};

/** Engine file status → icon name. */
const STATUS_ICON = { A: 'added', D: 'deleted', M: 'modified', R: 'renamed', C: 'copied', T: 'typechanged' };

/** Engine file status → CSS color class. */
const STATUS_CLASS = { A: 'added', D: 'deleted', M: 'modified', R: 'renamed', C: 'copied', T: 'typechanged' };

/** Wraps an ICONS body in the shared 16x16 stroke SVG element markup. */
function iconSvg(name) {
  return (
    `<svg width="16" height="16" viewBox="0 0 16 16" fill="none" ` +
    `stroke="currentColor" stroke-width="1.4" stroke-linecap="round" ` +
    `stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`
  );
}

/** Defensive field coercion — the detail comes from the host, but the
 *  engine's data is still external, so rendering must never crash on a
 *  malformed payload. */
function str(value) {
  return typeof value === 'string' ? value : '';
}

function num(value) {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

/** Mirrors graph.js's formatTime (webviews are separate bundles; a ~10-line
 *  JS duplication here is by design). */
function formatTime(unixSeconds) {
  try {
    return new Date(unixSeconds * 1000).toLocaleString(undefined, {
      year: 'numeric',
      month: 'short',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
    });
  } catch {
    return '';
  }
}

/** First character of the author name for the avatar placeholder. */
function initial(name) {
  const trimmed = name.trim();
  return trimmed.length > 0 ? trimmed.charAt(0) : '?';
}

function render(detail) {
  els.error.classList.remove('visible');

  const authorName = str(detail.author_name);
  els.avatar.textContent = initial(authorName);
  els.authorName.textContent = authorName;
  const authorTime = num(detail.author_time);
  els.authorTime.textContent = formatTime(authorTime);
  // Email as a hover tooltip (Git Graph convention).
  els.authorLine.title = `${authorName} <${str(detail.author_email)}>`;
  const committerName = str(detail.committer_name);
  const committerTime = num(detail.committer_time);
  if (committerName === authorName && committerTime === authorTime) {
    els.committerLine.classList.remove('visible');
  } else {
    els.committerLine.classList.add('visible');
    els.committerName.textContent = committerName;
    els.committerTime.textContent = formatTime(committerTime);
    els.committerLine.title = `${committerName} <${str(detail.committer_email)}>`;
  }

  // Full multi-line commit message.
  els.message.textContent = str(detail.message);

  const files = Array.isArray(detail.files) ? detail.files : [];
  els.filesCount.textContent = String(files.length);
  els.filesSection.style.display = '';
  els.rows.replaceChildren(...files.map(createFileRow));
  els.empty.textContent = 'This commit changed no files.';
  els.empty.classList.toggle('visible', files.length === 0);
}

function renderError() {
  els.message.textContent = '';
  els.filesSection.style.display = 'none';
  els.rows.replaceChildren();
  els.empty.classList.remove('visible');
  els.error.classList.add('visible');
}

function createFileRow(file) {
  const filePath = str(file.path);
  const status = str(file.status);
  const oldPath = file.old_path == null ? null : str(file.old_path);

  const row = document.createElement('div');
  row.className = 'file-row';
  row.dataset.path = filePath;

  const icon = document.createElement('span');
  icon.className = `status-icon ${STATUS_CLASS[status] ?? 'modified'}`;
  icon.innerHTML = iconSvg(STATUS_ICON[status] ?? 'modified');
  row.appendChild(icon);

  // Repo paths are always LTR (dir="ltr"), even in RTL locales.
  const pathEl = document.createElement('span');
  pathEl.className = 'path';
  pathEl.setAttribute('dir', 'ltr');
  if (oldPath) {
    const old = document.createElement('span');
    old.className = 'old-path';
    old.textContent = oldPath;
    pathEl.appendChild(old);
    const arrow = document.createElement('span');
    arrow.className = 'rename-arrow';
    arrow.innerHTML = iconSvg('renamed');
    pathEl.appendChild(arrow);
  }
  const name = document.createElement('span');
  name.className = 'filename';
  name.textContent = filePath;
  pathEl.appendChild(name);
  row.appendChild(pathEl);
  row.title = oldPath ? `${oldPath} → ${filePath}` : filePath;

  const stats = document.createElement('span');
  stats.className = 'stats';
  const additions = document.createElement('span');
  additions.className = 'additions';
  additions.textContent = `+${num(file.additions)}`;
  const deletions = document.createElement('span');
  deletions.className = 'deletions';
  deletions.textContent = `−${num(file.deletions)}`;
  stats.appendChild(additions);
  stats.appendChild(deletions);
  row.appendChild(stats);

  // Hover-revealed "Open Changes" action (opacity pattern — keyboard
  // focus keeps it visible via :focus-within on the row).
  const actions = document.createElement('span');
  actions.className = 'actions';
  const btn = document.createElement('button');
  btn.className = 'action-btn';
  btn.innerHTML = iconSvg('openChanges');
  btn.title = 'Open Changes';
  btn.setAttribute('aria-label', 'Open Changes');
  btn.addEventListener('click', (event) => {
    event.stopPropagation();
    postOpenChanges(filePath, oldPath);
  });
  actions.appendChild(btn);
  row.appendChild(actions);

  // Row click opens the diff too (same gesture as the commit view).
  row.addEventListener('click', () => postOpenChanges(filePath, oldPath));

  return row;
}

function postOpenChanges(filePath, oldPath) {
  const message = { type: 'openChanges', path: filePath };
  if (oldPath != null) message.oldPath = oldPath;
  vscode.postMessage(message);
}

window.addEventListener('message', (event) => {
  const message = event.data;
  if (!message || typeof message.type !== 'string') return;
  if (message.type === 'commitDetail' && message.detail && typeof message.detail === 'object') {
    render(message.detail);
  } else if (message.type === 'error') {
    renderError();
  }
});

// Handshake: by the time this fires, the message listener above is
// installed — the host waits for it before pushing the first data.
vscode.postMessage({ type: 'ready' });
