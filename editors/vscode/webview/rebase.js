/**
 * Rebase plan webview (gitBamboo.rebasePanel): the interactive-rebase
 * todo editor — reorder rows (drag from the handle, or the up/down
 * buttons), change actions (pick/reword/squash/drop), inline-edit
 * reword/squash messages, drop rows — plus the execution flow (confirm
 * modal, then engine RPC via the host) and the read-only docked view for
 * an in-progress rebase (done/remaining sections + continue/skip/abort).
 *
 * Host protocol (see src/rebasePanel.ts):
 *   outbound: { type: 'ready' }
 *             { type: 'executeRebase', onto, todo: [{ id, action, message? }] }
 *             { type: 'rebaseContinue' | 'rebaseSkip' | 'rebaseAbort' }
 *             { type: 'refreshPlan' } / { type: 'openConflicts' } / { type: 'close' }
 *   inbound:  { type: 'plan', onto, branch, upstream, commits } (host-validated)
 *             { type: 'status', status, conflictCount?, reason? }
 *             { type: 'stale' } / { type: 'blocked', message } / { type: 'finished', outcome }
 *             { type: 'executing', active, op? } / { type: 'requestExecute' }
 *             { type: 'loading' } / { type: 'empty' } / { type: 'error', message }
 *
 * Inbound payloads are field-validated by the host before they are
 * pushed, and every outbound field is plain data (message strings are
 * only sent non-empty for reword/squash rows — empty means git's folding
 * semantics keep the default message). All untrusted strings (subjects,
 * authors, raw todo lines, engine messages) are rendered via
 * textContent/DOM APIs — never innerHTML; the only innerHTML use is the
 * static, trusted SVG icon markup (icons.js). User edits are never
 * cleared by a refresh: an idle status push is ignored while the plan
 * view is up; staleness is reported, and only the user (reload) or a new
 * host-pushed plan replaces the todo.
 */
import { iconSvg } from './icons.js';

const vscode = acquireVsCodeApi();

const els = {
  views: {
    plan: document.getElementById('plan-view'),
    docked: document.getElementById('docked-view'),
    loading: document.getElementById('loading-view'),
    empty: document.getElementById('empty-view'),
    error: document.getElementById('error-view'),
  },
  banners: {
    stale: document.getElementById('stale-banner'),
    conflict: document.getElementById('conflict-banner'),
    blocked: document.getElementById('blocked-banner'),
    finished: document.getElementById('finished-banner'),
  },
  planBranch: document.getElementById('plan-branch'),
  planOnto: document.getElementById('plan-onto'),
  planMeta: document.getElementById('plan-meta'),
  todoList: document.getElementById('todo-list'),
  todoCount: document.getElementById('todo-count'),
  execute: document.getElementById('execute'),
  cancel: document.getElementById('cancel'),
  dockedTitle: document.getElementById('docked-title'),
  dockedSub: document.getElementById('docked-sub'),
  progressFill: document.getElementById('progress-fill'),
  doneList: document.getElementById('done-list'),
  remainingList: document.getElementById('remaining-list'),
  continueBtn: document.getElementById('continue'),
  skipBtn: document.getElementById('skip'),
  abortBtn: document.getElementById('abort'),
  emptyText: document.getElementById('empty-text'),
  errorText: document.getElementById('error-text'),
  conflictText: document.getElementById('conflict-text'),
  blockedText: document.getElementById('blocked-text'),
  finishedText: document.getElementById('finished-text'),
  modalBackdrop: document.getElementById('modal-backdrop'),
  modalSummary: document.getElementById('modal-summary'),
  modalConfirm: document.getElementById('modal-confirm'),
  modalCancel: document.getElementById('modal-cancel'),
};

/** Buttons disabled while a rebase RPC is in flight (host 'executing'). */
const actionButtons = [
  els.execute,
  els.cancel,
  els.continueBtn,
  els.skipBtn,
  els.abortBtn,
  document.getElementById('stale-reload'),
  document.getElementById('conflict-open'),
  document.getElementById('finished-close'),
  document.getElementById('empty-close'),
  document.getElementById('error-close'),
];

/** The four plan-editable actions (engine todo validation mirrors this). */
const ACTIONS = ['pick', 'reword', 'squash', 'drop'];

/** Current view mode: one of els.views' keys. */
let mode = 'loading';
/** The pushed plan (host-validated). `commits` are the original rows. */
let plan = null;
/** The editable todo (plan.commits copies + user edits). Entries with
 *  `dropped: true` stay in place but are dimmed and excluded at submit. */
let todo = [];
/** A rebase RPC is in flight (host 'executing'); controls disabled. */
let executing = false;
/** Drag source index (HTML5 DnD is initiated from the row handle only). */
let dragIndex = -1;

/* ---------------------------------------------------------------- helpers */

function shortSha(id) {
  return typeof id === 'string' && id.length >= 7 ? id.slice(0, 7) : String(id);
}

/** Mirrors commitDetail.js's formatTime (webviews are separate bundles;
 *  a ~10-line JS duplication here is by design). */
function formatTime(unixSeconds) {
  try {
    return new Date(unixSeconds * 1000).toLocaleString(undefined, {
      year: 'numeric',
      month: 'short',
      day: '2-digit',
    });
  } catch {
    return '';
  }
}

function setMode(next) {
  mode = next;
  for (const [name, el] of Object.entries(els.views)) {
    el.classList.toggle('hidden', name !== next);
  }
}

function showBanner(name, show) {
  els.banners[name].classList.toggle('hidden', !show);
}

function clearBanners() {
  for (const banner of Object.values(els.banners)) banner.classList.add('hidden');
}

function setBusyIndicator(active) {
  executing = active;
  for (const button of actionButtons) button.disabled = active;
  els.execute.textContent = active ? 'Running…' : 'Execute';
}

/* ------------------------------------------------------------- plan view */

function setPlan(message) {
  plan = {
    onto: String(message.onto ?? ''),
    branch: String(message.branch ?? ''),
    upstream: String(message.upstream ?? ''),
    commits: message.commits,
  };
  todo = plan.commits.map((commit) => ({
    id: commit.id,
    action: 'pick',
    message: '',
    dropped: false,
    subject: commit.subject,
    author: commit.author,
    time: commit.time,
  }));
  clearBanners();
  renderPlanHeader();
  renderTodo();
  setMode('plan');
}

function renderPlanHeader() {
  els.planBranch.textContent = plan.branch || 'current branch';
  els.planOnto.textContent = shortSha(plan.onto);
  els.planMeta.textContent = plan.commits.length + ' commits to replay';
}

function renderTodo() {
  els.todoList.replaceChildren();
  const fragment = document.createDocumentFragment();
  todo.forEach((entry, index) => fragment.appendChild(todoRow(entry, index)));
  els.todoList.appendChild(fragment);
  updateTodoCount();
}

function updateTodoCount() {
  const active = todo.filter((entry) => !entry.dropped).length;
  els.todoCount.textContent = active + ' of ' + todo.length + ' commits';
}

/** Builds one todo row. All commit metadata goes in via textContent. */
function todoRow(entry, index) {
  const row = document.createElement('li');
  row.className = 'todo-row' + (entry.dropped ? ' dropped' : '');

  // Drag reorder: initiated ONLY from the handle (mousedown arms the
  // row's draggable flag) so select/input interactions stay untouched.
  const handle = document.createElement('span');
  handle.className = 'drag-handle';
  handle.title = 'Drag to reorder';
  handle.innerHTML = iconSvg('dragHandle');
  handle.addEventListener('mousedown', () => {
    row.draggable = true;
  });
  row.addEventListener('dragstart', (event) => {
    if (!row.draggable) {
      event.preventDefault();
      return;
    }
    dragIndex = index;
    row.classList.add('dragging');
    event.dataTransfer.effectAllowed = 'move';
    try {
      event.dataTransfer.setData('text/plain', String(index));
    } catch {
      /* dataTransfer is best-effort; dragIndex is the source of truth */
    }
  });
  row.addEventListener('dragend', () => {
    row.draggable = false;
    row.classList.remove('dragging');
    dragIndex = -1;
    clearDropMarkers();
  });
  row.addEventListener('dragover', (event) => {
    if (dragIndex < 0 || dragIndex === index) return;
    event.preventDefault();
    clearDropMarkers();
    row.classList.add(index > dragIndex ? 'drop-below' : 'drop-above');
  });
  row.addEventListener('drop', (event) => {
    event.preventDefault();
    const dropped = row.classList.contains('drop-below');
    clearDropMarkers();
    if (dragIndex < 0 || dragIndex === index) return;
    moveEntry(dragIndex, index + (dropped ? 1 : 0));
  });
  row.appendChild(handle);

  // Action selector (colored per action class). Choosing `drop` marks
  // the row dropped (dimmed, excluded at submit); Restore — or Escape —
  // brings back the previous action losslessly.
  const select = document.createElement('select');
  select.className = 'action-' + entry.action;
  select.setAttribute('aria-label', 'Action for commit ' + shortSha(entry.id));
  for (const action of ACTIONS) {
    const option = document.createElement('option');
    option.value = action;
    option.textContent = action;
    if (action === entry.action) option.selected = true;
    select.appendChild(option);
  }
  select.addEventListener('change', () => {
    if (select.value === 'drop' && entry.action !== 'drop') {
      entry.prevAction = entry.action;
    }
    entry.action = select.value;
    entry.dropped = select.value === 'drop';
    select.className = 'action-' + entry.action;
    row.classList.toggle('dropped', entry.dropped);
    rebuildRowActions(row, entry, index);
    updateRowMessage(row, entry);
    updateTodoCount();
  });
  row.appendChild(select);

  // Commit line: subject + author/time meta (plain text only).
  const line = document.createElement('div');
  line.className = 'commit-line';
  const subject = document.createElement('span');
  subject.className = 'subject';
  subject.textContent = entry.subject || shortSha(entry.id);
  const meta = document.createElement('span');
  meta.className = 'meta';
  meta.textContent = (entry.author || '') + (entry.time ? ' · ' + formatTime(entry.time) : '');
  line.appendChild(subject);
  line.appendChild(meta);
  row.appendChild(line);

  // Row actions: move up / move down (drag alternative), restore when
  // dropped.
  const actions = document.createElement('span');
  actions.className = 'row-actions';
  row.appendChild(actions);
  rebuildRowActions(row, entry, index);

  // Escape restores a dropped row (keyboard alternative to Restore).
  row.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && entry.dropped) {
      event.preventDefault();
      restoreEntry(row, entry);
    }
  });

  // Inline message editor for reword/squash (single paragraph by input
  // element design; empty = keep the commit's default message — the
  // engine/git folding semantics apply).
  const messageWrap = document.createElement('div');
  messageWrap.className = 'row-message hidden';
  row.appendChild(messageWrap);

  updateRowMessage(row, entry);
  return row;
}

/** (Re)builds a row's action-button cluster (up/down, restore when
 *  dropped) — re-run when the dropped state changes. */
function rebuildRowActions(row, entry, index) {
  const actions = row.querySelector('.row-actions');
  actions.replaceChildren();
  const up = document.createElement('button');
  up.className = 'row-btn';
  up.title = 'Move up';
  up.setAttribute('aria-label', 'Move commit up');
  up.innerHTML = iconSvg('moveUp');
  up.addEventListener('click', () => moveEntry(index, index - 1));
  const down = document.createElement('button');
  down.className = 'row-btn';
  down.title = 'Move down';
  down.setAttribute('aria-label', 'Move commit down');
  down.innerHTML = iconSvg('moveDown');
  down.addEventListener('click', () => moveEntry(index, index + 2));
  actions.appendChild(up);
  actions.appendChild(down);
  if (entry.dropped) {
    const restore = document.createElement('button');
    restore.className = 'row-btn restore-btn';
    restore.title = 'Restore this commit';
    restore.setAttribute('aria-label', 'Restore this commit');
    restore.textContent = 'Restore';
    restore.addEventListener('click', () => restoreEntry(row, entry));
    actions.appendChild(restore);
  }
}

/** Undrops a row, restoring its previous action (lossless). */
function restoreEntry(row, entry) {
  entry.dropped = false;
  if (entry.action === 'drop' && entry.prevAction) {
    entry.action = entry.prevAction;
  }
  if (entry.action === 'drop') entry.action = 'pick';
  const select = row.querySelector('select');
  if (select) {
    select.value = entry.action;
    select.className = 'action-' + entry.action;
  }
  row.classList.remove('dropped');
  const index = todo.indexOf(entry);
  if (index >= 0) rebuildRowActions(row, entry, index);
  updateRowMessage(row, entry);
  updateTodoCount();
}

/** (Re)builds a row's inline message editor for the current action. */
function updateRowMessage(row, entry) {
  const wrap = row.querySelector('.row-message');
  wrap.replaceChildren();
  if (entry.action !== 'reword' && entry.action !== 'squash') {
    wrap.classList.add('hidden');
    return;
  }
  wrap.classList.remove('hidden');
  const input = document.createElement('input');
  input.type = 'text';
  input.value = entry.message;
  input.placeholder =
    entry.action === 'reword' ? 'New message (empty keeps the original)' : 'Squash message (empty folds into the previous message)';
  input.setAttribute('aria-label', 'Message for ' + entry.action);
  input.addEventListener('input', () => {
    entry.message = input.value;
  });
  wrap.appendChild(input);
}

function clearDropMarkers() {
  for (const row of els.todoList.querySelectorAll('.drop-above, .drop-below')) {
    row.classList.remove('drop-above');
    row.classList.remove('drop-below');
  }
}

/** Moves todo[from] to insertion position `to` (clamped, no-op at bounds). */
function moveEntry(from, to) {
  const clamped = Math.max(0, Math.min(todo.length, to));
  if (from < 0 || from >= todo.length || from === clamped || from === clamped - 1) return;
  const [entry] = todo.splice(from, 1);
  todo.splice(clamped, 0, entry);
  renderTodo();
}

/** Submits the edited todo: dropped rows excluded; message only sent when
 *  non-empty for reword/squash (empty = git folding semantics). */
function submitTodo() {
  const payload = [];
  for (const entry of todo) {
    if (entry.dropped) continue;
    const item = { id: entry.id, action: entry.action };
    if ((entry.action === 'reword' || entry.action === 'squash') && entry.message) {
      item.message = entry.message;
    }
    payload.push(item);
  }
  if (payload.length === 0) return; // nothing to replay; keep the editor open
  vscode.postMessage({ type: 'executeRebase', onto: plan.onto, todo: payload });
}

/* --------------------------------------------------------- confirm modal */

function openModal() {
  const counts = { pick: 0, reword: 0, squash: 0 };
  let dropped = 0;
  for (const entry of todo) {
    if (entry.dropped) {
      dropped++;
    } else if (counts[entry.action] !== undefined) {
      counts[entry.action]++;
    }
  }
  const parts = [];
  if (counts.pick) parts.push(counts.pick + ' pick');
  if (counts.reword) parts.push(counts.reword + ' reword');
  if (counts.squash) parts.push(counts.squash + ' squash');
  if (dropped) parts.push(dropped + ' dropped');
  const active = todo.length - dropped;
  let summary = 'Rebase ' + (plan.branch || 'the current branch') + ' onto ' + shortSha(plan.onto) +
    ' — ' + active + ' commit' + (active === 1 ? '' : 's') + ': ' + parts.join(', ') + '.';
  // Reorder detection: id sequence vs the original plan order.
  const ids = todo.map((entry) => entry.id);
  const original = plan.commits.map((commit) => commit.id);
  if (ids.length === original.length && ids.some((id, i) => id !== original[i])) {
    summary += ' The order differs from the original plan.';
  }
  els.modalSummary.textContent = summary;
  els.modalBackdrop.classList.remove('hidden');
  els.modalConfirm.focus();
}

function closeModal() {
  els.modalBackdrop.classList.add('hidden');
}

/* ----------------------------------------------------------- docked view */

function setStatus(message) {
  const status = message.status;
  if (!status || typeof status !== 'object') return;
  if (status.state === 'idle') {
    // Idle pushes never clobber the plan editor (user edits win); a
    // docked view going idle means the rebase finished (e.g. from the
    // terminal) — report it as completed.
    if (mode === 'docked') {
      finish('completed');
    } else if (mode === 'loading' || mode === 'empty') {
      setMode('empty');
      els.emptyText.textContent = 'No rebase is in progress.';
    }
    return;
  }
  // Entering the docked view replaces whatever was shown (a start/step
  // outcome). Once docked, later status pushes only update data — a
  // `blocked` notice pushed right after a status (refused continue)
  // must survive the next status push; retrying (executing) clears it.
  if (mode !== 'docked') {
    showBanner('stale', false);
    showBanner('finished', false);
    showBanner('blocked', false);
  }
  const conflicts = typeof message.conflictCount === 'number' ? message.conflictCount : undefined;
  const stopped = status.state === 'stopped';
  const reason = message.reason;
  showBanner('conflict', Boolean(stopped && conflicts !== undefined && conflicts > 0));
  if (stopped && conflicts > 0) {
    els.conflictText.textContent =
      conflicts + ' unresolved conflict' + (conflicts === 1 ? '' : 's') +
      ' — resolve them in the Commit view, then continue.';
  }
  renderDocked(status, stopped, reason);
  setMode('docked');
}

function renderDocked(status, stopped, reason) {
  const msgnum = typeof status.msgnum === 'number' ? status.msgnum : undefined;
  const end = typeof status.end === 'number' ? status.end : undefined;
  if (msgnum !== undefined && end && end > 0) {
    els.progressFill.style.width = Math.round((msgnum / end) * 100) + '%';
  } else {
    els.progressFill.style.width = '0';
  }
  els.dockedTitle.textContent = 'Rebase in progress';
  let sub = '';
  if (msgnum !== undefined && end) sub = msgnum + ' / ' + end;
  if (stopped) {
    sub += (sub ? ' — ' : '') + 'stopped at ' + shortSha(status.stopped_sha || '');
    if (reason === 'conflict') sub += ' (conflict)';
    else if (reason === 'reword_stop') sub += ' (waiting for message)';
  }
  els.dockedSub.textContent = sub;
  renderStatusList(els.doneList, status.done || []);
  renderStatusList(els.remainingList, status.remaining || [], status.stopped_sha);
}

function renderStatusList(list, entries, stoppedSha) {
  list.replaceChildren();
  const fragment = document.createDocumentFragment();
  for (const entry of entries) {
    const li = document.createElement('li');
    if (entry.kind === 'entry') {
      const action = document.createElement('span');
      action.className = 'status-action ' + String(entry.action || '');
      action.textContent = String(entry.action || '');
      const sha = document.createElement('span');
      sha.className = 'status-sha';
      sha.textContent = shortSha(entry.id);
      const subject = document.createElement('span');
      subject.className = 'status-subject';
      subject.textContent = String(entry.subject || '');
      li.appendChild(action);
      li.appendChild(sha);
      li.appendChild(subject);
      if (stoppedSha && entry.id === stoppedSha) li.classList.add('current');
    } else {
      // Raw todo line (labels, exec, break, unknown syntax): verbatim.
      li.textContent = String(entry.line || '');
    }
    fragment.appendChild(li);
  }
  list.appendChild(fragment);
}

/* ------------------------------------------------------------- lifecycle */

function finish(outcome) {
  clearBanners();
  els.finishedText.textContent =
    outcome === 'aborted' ? 'Rebase aborted — the branch is back at its pre-rebase state.' : 'Rebase completed.';
  showBanner('finished', true);
  setMode('empty');
  els.emptyText.textContent = '';
}

window.addEventListener('message', (event) => {
  const message = event.data;
  if (!message || typeof message !== 'object') return;
  switch (message.type) {
    case 'plan':
      setPlan(message);
      return;
    case 'status':
      setStatus(message);
      return;
    case 'stale':
      if (mode === 'plan') showBanner('stale', true);
      return;
    case 'blocked':
      if (mode === 'docked') {
        els.blockedText.textContent = String(message.message || 'The engine refused this operation.');
        showBanner('blocked', true);
      }
      return;
    case 'finished':
      finish(message.outcome === 'aborted' ? 'aborted' : 'completed');
      return;
    case 'executing':
      setBusyIndicator(message.active === true);
      if (message.active === true) showBanner('blocked', false);
      return;
    case 'requestExecute':
      if (mode === 'plan' && !executing) openModal();
      return;
    case 'loading':
      clearBanners();
      setMode('loading');
      return;
    case 'empty':
      clearBanners();
      setMode('empty');
      els.emptyText.textContent = 'Nothing to rebase — the commits are already contained in the target.';
      return;
    case 'error':
      clearBanners();
      setMode('error');
      els.errorText.textContent = String(message.message || 'The rebase plan could not be loaded.');
      return;
  }
});

/* Wire the static controls (built once; icons via trusted markup). */
els.execute.innerHTML = iconSvg('play') + '<span>Execute</span>';
els.continueBtn.innerHTML = iconSvg('play') + '<span>Continue</span>';
els.skipBtn.innerHTML = iconSvg('skip') + '<span>Skip commit</span>';
els.abortBtn.innerHTML = iconSvg('stop') + '<span>Abort</span>';
document.getElementById('stale-reload').innerHTML = iconSvg('refresh') + '<span>Reload plan</span>';

els.execute.addEventListener('click', () => {
  if (!executing && mode === 'plan') openModal();
});
els.cancel.addEventListener('click', () => vscode.postMessage({ type: 'close' }));
document.getElementById('stale-reload').addEventListener('click', () => {
  vscode.postMessage({ type: 'refreshPlan' });
});
document.getElementById('conflict-open').addEventListener('click', () => {
  vscode.postMessage({ type: 'openConflicts' });
});
document.getElementById('finished-close').addEventListener('click', () => {
  vscode.postMessage({ type: 'close' });
});
document.getElementById('empty-close').addEventListener('click', () => {
  vscode.postMessage({ type: 'close' });
});
document.getElementById('error-close').addEventListener('click', () => {
  vscode.postMessage({ type: 'close' });
});
els.continueBtn.addEventListener('click', () => {
  if (!executing) vscode.postMessage({ type: 'rebaseContinue' });
});
els.skipBtn.addEventListener('click', () => {
  if (!executing) vscode.postMessage({ type: 'rebaseSkip' });
});
els.abortBtn.addEventListener('click', () => {
  if (!executing) vscode.postMessage({ type: 'rebaseAbort' });
});

els.modalConfirm.addEventListener('click', () => {
  closeModal();
  if (mode === 'plan' && !executing) submitTodo();
});
els.modalCancel.addEventListener('click', closeModal);
els.modalBackdrop.addEventListener('click', (event) => {
  if (event.target === els.modalBackdrop) closeModal();
});
window.addEventListener('keydown', (event) => {
  if (event.key === 'Escape' && !els.modalBackdrop.classList.contains('hidden')) {
    closeModal();
  }
});

// The host's first data push rides on this ready handshake: postMessage
// for 'ready' instead of the host pushing at construction time (the
// listener here is only guaranteed up once this module has run).
vscode.postMessage({ type: 'ready' });
