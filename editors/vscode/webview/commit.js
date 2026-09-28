/**
 * Commit view webview (gitBamboo.commit):
 * commit message + grouped change list + commit/amend actions in one view
 * (JetBrains-style commit panel).
 *
 * Host protocol (see src/commitView.ts):
 *   outbound: { type: 'stage' | 'unstage', paths }
 *             { type: 'openResource', path, status }
 *             { type: 'commit', message, amend }
 *   inbound:  { type: 'status', items }  — raw engine StatusItem[], bucketed here
 *             { type: 'committed' }      — clear the message box
 */

const vscode = acquireVsCodeApi();

/** @typedef {{ path: string, status: string, old_path?: string, staged: boolean }} StatusItem */

/** Bucketing rule mirrored from the old scmProvider: staged flag first,
 *  then untracked; ignored items are filtered out entirely. */
const BUCKETS = ['staged', 'changes', 'untracked'];

/** UI state preserved across re-renders (only the list DOM is replaced). */
const collapsed = new Set();

/** Map from engine status code to the compact status letter badge. */
const STATUS_LETTER = {
  added: 'A',
  untracked: 'A',
  modified: 'M',
  renamed: 'R',
  copied: 'C',
  deleted: 'D',
  conflict: 'U',
};

const els = {
  message: document.getElementById('commit-message'),
  empty: document.getElementById('empty-state'),
  sections: {
    staged: document.getElementById('staged-section'),
    changes: document.getElementById('changes-section'),
    untracked: document.getElementById('untracked-section'),
  },
  counts: {
    staged: document.getElementById('staged-count'),
    changes: document.getElementById('changes-count'),
    untracked: document.getElementById('untracked-count'),
  },
  bodies: {
    staged: document.getElementById('staged-body'),
    changes: document.getElementById('changes-body'),
    untracked: document.getElementById('untracked-body'),
  },
  btnCommit: document.getElementById('btn-commit'),
  btnAmend: document.getElementById('btn-amend'),
  stats: document.getElementById('stats'),
};

function bucketOf(item) {
  if (item.staged) return 'staged';
  if (item.status === 'untracked') return 'untracked';
  return 'changes';
}

/** Splits a repo-relative path into { filename, dirname } for the compact
 *  JetBrains-style row layout. */
function splitPath(p) {
  const slash = p.lastIndexOf('/');
  if (slash < 0) return { filename: p, dirname: '' };
  return { filename: p.slice(slash + 1), dirname: p.slice(0, slash) };
}

function createButton(label, title, onClick) {
  const btn = document.createElement('button');
  btn.className = 'action-btn';
  btn.textContent = label;
  btn.title = title;
  btn.addEventListener('click', (event) => {
    event.stopPropagation();
    onClick();
  });
  return btn;
}

function createFileRow(item) {
  const bucket = bucketOf(item);
  const row = document.createElement('div');
  row.className = 'file-row';
  row.dataset.path = item.path;

  const { filename, dirname } = splitPath(item.path);

  const badge = document.createElement('span');
  badge.className = `status-badge ${item.status}`;
  badge.textContent = STATUS_LETTER[item.status] ?? 'M';
  row.appendChild(badge);

  const name = document.createElement('span');
  name.className = 'filename';
  name.textContent = filename;
  row.appendChild(name);

  if (dirname) {
    const dir = document.createElement('span');
    dir.className = 'dirname';
    dir.textContent = dirname;
    row.appendChild(dir);
  }

  // Inline actions: staged rows offer unstage + diff; others stage + open.
  const actions = document.createElement('span');
  actions.className = 'actions';
  if (bucket === 'staged') {
    actions.appendChild(createButton('−', 'Unstage Changes', () => {
      vscode.postMessage({ type: 'unstage', paths: [item.path] });
    }));
    actions.appendChild(createButton('⇄', 'Open Changes', () => {
      vscode.postMessage({ type: 'openResource', path: item.path, status: item.status });
    }));
  } else {
    actions.appendChild(createButton('+', 'Stage Changes', () => {
      vscode.postMessage({ type: 'stage', paths: [item.path] });
    }));
    actions.appendChild(createButton('⇄', item.status === 'untracked' ? 'Open File' : 'Open Changes', () => {
      vscode.postMessage({ type: 'openResource', path: item.path, status: item.status });
    }));
  }
  row.appendChild(actions);

  // Row click opens the diff (or the file itself for untracked/added).
  row.addEventListener('click', () => {
    vscode.postMessage({ type: 'openResource', path: item.path, status: item.status });
  });

  if (item.old_path !== undefined) {
    row.title = `Renamed from ${item.old_path}`;
  } else {
    row.title = `${item.status}: ${item.path}`;
  }

  return row;
}

function render(items) {
  const buckets = { staged: [], changes: [], untracked: [] };
  for (const item of items) {
    if (item.status === 'ignored') continue; // hidden, as in the old SCM view
    buckets[bucketOf(item)].push(item);
  }

  let total = 0;
  for (const key of BUCKETS) {
    const section = els.sections[key];
    const body = els.bodies[key];
    const list = buckets[key];
    total += list.length;

    body.replaceChildren(...list.map(createFileRow));
    els.counts[key].textContent = String(list.length);
    section.style.display = list.length > 0 ? '' : 'none';
    applyCollapsed(key);
  }

  els.empty.classList.toggle('visible', total === 0);
  els.stats.textContent = total === 0 ? '' : `${total} file${total === 1 ? '' : 's'} · ${buckets.staged.length} staged`;
}

function sendCommit(amend) {
  vscode.postMessage({ type: 'commit', message: els.message.value.trim(), amend });
}

/** Auto-grow the textarea to its content (capped by max-height CSS). */
function autoGrow() {
  els.message.style.height = 'auto';
  els.message.style.height = `${els.message.scrollHeight}px`;
}
els.message.addEventListener('input', autoGrow);

els.message.addEventListener('keydown', (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
    event.preventDefault();
    sendCommit(false);
  }
});
els.btnCommit.addEventListener('click', () => sendCommit(false));
els.btnAmend.addEventListener('click', () => sendCommit(true));

for (const key of BUCKETS) {
  els.sections[key].querySelector('.section-header').addEventListener('click', () => {
    if (collapsed.has(key)) collapsed.delete(key);
    else collapsed.add(key);
    applyCollapsed(key);
  });
}

/** Syncs a section's collapsed state to the DOM (shared by render() and
 *  the header click handler). */
function applyCollapsed(key) {
  const header = els.sections[key].querySelector('.section-header');
  header.classList.toggle('collapsed', collapsed.has(key));
  els.bodies[key].style.display = collapsed.has(key) ? 'none' : '';
}

window.addEventListener('message', (event) => {
  const message = event.data;
  if (!message || typeof message.type !== 'string') return;
  if (message.type === 'status' && Array.isArray(message.items)) {
    render(message.items);
  } else if (message.type === 'committed') {
    els.message.value = '';
    autoGrow(); // programmatic clears don't fire the input event
  }
});

// The host's first status push rides on this ready handshake: postMessage
// before the webview finishes loading would be dropped, so the host waits
// for 'ready' instead of pushing at resolve time.
vscode.postMessage({ type: 'ready' });
