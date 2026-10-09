/**
 * Commit detail webview (gitBamboo.commitDetail): full commit metadata —
 * author/committer identity lines, multi-line message, and the changed-file
 * list with per-file +/- counts and an "Open Changes" action per row.
 *
 * Host protocol (see src/commitDetail.ts):
 *   outbound: { type: 'openChanges', path, oldPath? }  (oldPath only when non-null)
 *   inbound:  { type: 'commitDetail', detail }         — engine CommitDetail (snake_case)
 *             { type: 'loading' }                       — re-target: clear to the loading state
 *             { type: 'error' }                         — fetch failed; render error state
 *
 * The detail payload is field-validated by the host before it is pushed
 * (engineClient's isValidCommitDetail), so rendering accesses fields
 * directly. All untrusted strings (message, names, emails, paths) go in
 * via textContent/DOM APIs — never innerHTML. The only innerHTML use is
 * the static, trusted SVG icon markup (icons.js, codicon-style 16x16
 * stroke SVGs), which is CSP-safe.
 */
import { iconSvg } from './icons.js';

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

/** Engine file status → icon name / CSS color class (same names). */
const STATUS = { A: 'added', D: 'deleted', M: 'modified', R: 'renamed', C: 'copied', T: 'typechanged' };

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

  const authorName = detail.author_name;
  els.avatar.textContent = initial(authorName);
  els.authorName.textContent = authorName;
  els.authorTime.textContent = formatTime(detail.author_time);
  // Email as a hover tooltip (Git Graph convention).
  els.authorLine.title = `${authorName} <${detail.author_email}>`;
  const committerName = detail.committer_name;
  const committerTime = detail.committer_time;
  if (committerName === authorName && committerTime === detail.author_time) {
    els.committerLine.classList.remove('visible');
  } else {
    els.committerLine.classList.add('visible');
    els.committerName.textContent = committerName;
    els.committerTime.textContent = formatTime(committerTime);
    els.committerLine.title = `${committerName} <${detail.committer_email}>`;
  }

  // Full multi-line commit message.
  els.message.textContent = detail.message;

  const files = detail.files;
  els.filesCount.textContent = String(files.length);
  els.filesSection.style.display = '';
  els.rows.replaceChildren(...files.map(createFileRow));
  els.empty.textContent = 'This commit changed no files.';
  els.empty.classList.toggle('visible', files.length === 0);
}

/** Re-target: clear the previous commit's header/message/rows back to the
 *  initial loading state while the new detail is fetched. */
function renderLoading() {
  els.error.classList.remove('visible');
  els.avatar.textContent = '';
  els.authorName.textContent = '';
  els.authorTime.textContent = '';
  els.authorLine.title = '';
  els.committerLine.classList.remove('visible');
  els.committerName.textContent = '';
  els.committerTime.textContent = '';
  els.committerLine.title = '';
  els.message.textContent = '';
  els.filesSection.style.display = 'none';
  els.rows.replaceChildren();
  els.empty.textContent = 'Loading…';
  els.empty.classList.add('visible');
}

function renderError() {
  els.message.textContent = '';
  els.filesSection.style.display = 'none';
  els.rows.replaceChildren();
  els.empty.classList.remove('visible');
  els.error.classList.add('visible');
}

function createFileRow(file) {
  const filePath = file.path;
  const iconName = STATUS[file.status] ?? 'modified';
  const oldPath = file.old_path;

  const row = document.createElement('div');
  row.className = 'file-row';
  row.dataset.path = filePath;

  const icon = document.createElement('span');
  icon.className = `status-icon ${iconName}`;
  icon.innerHTML = iconSvg(iconName);
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
  additions.textContent = `+${file.additions}`;
  const deletions = document.createElement('span');
  deletions.className = 'deletions';
  deletions.textContent = `−${file.deletions}`;
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
  } else if (message.type === 'loading') {
    renderLoading();
  } else if (message.type === 'error') {
    renderError();
  }
});

// Handshake: by the time this fires, the message listener above is
// installed — the host waits for it before pushing the first data.
vscode.postMessage({ type: 'ready' });
