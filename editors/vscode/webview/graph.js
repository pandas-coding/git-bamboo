/**
 * Commit graph webview: virtual scrolling (fixed 24px rows), canvas overlay
 * with Bézier branch curves drawn from commit lane + parent_ids, and
 * viewport paging driven by postMessage round-trips to the extension host.
 * Vanilla JS, no framework.
 */
import { formatTime, shortSha } from './util.js';

(() => {
  'use strict';

  const vscode = acquireVsCodeApi();

  const ROW_HEIGHT = 24;
  const ROW_BUFFER = 20; // rows rendered beyond each edge of the viewport
  const VIEWPORT_DEBOUNCE_MS = 16;
  const LANE_X_START = 14;
  const LANE_X_STEP = 12;

  /** Lane-area width bounds: below MIN the first lane is cramped, above MAX
   *  there is no point widening further (the old hard-coded width). */
  const GRAPH_MIN_WIDTH = 44;
  const GRAPH_MAX_WIDTH = 240;
  /** Right padding inside the lane area after the last lane's column. */
  const GRAPH_RIGHT_PAD = 14;

  /** dataset.commitId sentinel that forces fillRow to re-run (see
   *  refillRows): never equal to a real commit id or to '' (skeleton). */
  const REFILL_SENTINEL = '\u0000';

  const LANE_COLORS = [
    '#4fc1ff', '#ffd479', '#b180d7', '#89d185',
    '#e2a9a8', '#d7ba7d', '#75beff', '#f69d50',
  ];

  const scroller = document.getElementById('scroller');
  const spacer = document.getElementById('spacer');
  const rowsEl = document.getElementById('rows');
  const canvas = document.getElementById('overlay');
  const splitter = document.getElementById('splitter');
  const ctx = canvas.getContext('2d');

  // ---------------------------------------------------------- rebase banner

  const rebaseBanner = document.getElementById('rebase-banner');
  const rebaseText = document.getElementById('rebase-text');
  const rebaseButtons = {
    continue: document.getElementById('rebase-continue'),
    skip: document.getElementById('rebase-skip'),
    abort: document.getElementById('rebase-abort'),
  };
  /** Latest rebaseStatus from the host (null = idle or unknown → banner
   *  hidden). Only `stopped` shows controls; `running` is read-only. */
  let rebaseStatus = null;
  /** True while the host runs a rebase control op (buttons disabled). */
  let rebaseControlActive = false;

  /** Derives the banner from rebaseStatus: hidden unless a rebase is on
   *  disk. Controls enabled only while stopped AND no other mutating op
   *  is in flight (net op or rebase control). */
  function updateRebaseBanner() {
    const state = rebaseStatus && rebaseStatus.state;
    if (state !== 'stopped' && state !== 'running') {
      rebaseBanner.hidden = true;
      return;
    }
    rebaseBanner.hidden = false;
    let text = 'Rebase in progress';
    if (rebaseStatus.msgnum && rebaseStatus.end) {
      // git's msgnum counts the entry being processed (1-based): it is
      // bumped when the entry STARTS, so a conflict stop already counts
      // the stopped entry. Progress = completed entries = msgnum - 1.
      const completed = Math.max(rebaseStatus.msgnum - 1, 0);
      text += ` ${completed}/${rebaseStatus.end}`;
    }
    if (state === 'stopped') {
      text += ` — stopped at ${shortSha(rebaseStatus.stopped_sha)}`;
      const conflicts = rebaseStatus.conflicts;
      if (conflicts > 0) {
        text += ` · ${conflicts} unresolved conflict${conflicts === 1 ? '' : 's'}`;
      }
    }
    rebaseText.textContent = text;
    const blocked = state !== 'stopped' || rebaseControlActive || netOpActive;
    for (const btn of Object.values(rebaseButtons)) btn.disabled = blocked;
  }

  /** Validates a host rebaseStatus payload. Returns the parsed state for
   *  stopped/running, null for idle (valid: banner must hide), and
   *  undefined for malformed shapes (keep the last known state — mirrors
   *  parseBranchStatus's discipline, but idle is a real state here, not a
   *  failure). */
  function parseRebaseStatus(message) {
    const status = message.status;
    if (!status || typeof status !== 'object' || typeof status.state !== 'string') return undefined;
    if (status.state === 'idle') return null;
    if (status.state !== 'stopped' && status.state !== 'running') return undefined;
    const parsed = { state: status.state };
    if (typeof status.msgnum === 'number' && Number.isFinite(status.msgnum)) parsed.msgnum = status.msgnum;
    if (typeof status.end === 'number' && Number.isFinite(status.end)) parsed.end = status.end;
    if (typeof status.stopped_sha === 'string') parsed.stopped_sha = status.stopped_sha;
    parsed.conflicts =
      typeof message.conflicts === 'number' && Number.isFinite(message.conflicts)
        ? message.conflicts
        : 0;
    return parsed;
  }

  for (const [op, btn] of Object.entries(rebaseButtons)) {
    btn.addEventListener('click', () => {
      // The host validates `rebaseOp` against its control set before any
      // RPC runs (trust boundary) — the field name must match its
      // GraphViewMessage exactly.
      if (!btn.disabled) vscode.postMessage({ type: 'rebaseControl', rebaseOp: op });
    });
  }

  // ---------------------------------------------------------- network toolbar

  const netButtons = {
    fetch: document.getElementById('btn-fetch'),
    pull: document.getElementById('btn-pull'),
    push: document.getElementById('btn-push'),
    sync: document.getElementById('btn-sync'),
  };
  const aheadBehind = document.getElementById('ahead-behind');
  /** op -> which button spins while it runs (publish spins push). */
  const OP_SPIN = { fetch: 'fetch', pull: 'pull', push: 'push', sync: 'sync', publish: 'push' };

  /** op -> button title, captured from the HTML at load (single source of
   *  truth: updateToolbar restores these after detached tooltips). */
  const OP_TITLES = Object.fromEntries(
    Object.entries(netButtons).map(([op, btn]) => [op, btn.title]),
  );
  const DETACHED_TITLE = 'HEAD is detached — switch to a branch to pull or push';

  /** True while a net op runs (netOpState from the host). */
  let netOpActive = false;
  /** Which op is running (for the spinner); null when idle. */
  let activeOp = null;
  /** Latest branchStatus from the host (null until first push). */
  let branchStatus = null;

  /** Derives all toolbar state from netOpActive + branchStatus: busy
   *  spinner, detached disablement with explanatory tooltips, and the
   *  badge (counts / ↑✱ publish entry / detached). */
  function updateToolbar() {
    for (const btn of Object.values(netButtons)) btn.classList.remove('busy');
    if (netOpActive && activeOp && netButtons[OP_SPIN[activeOp]]) {
      netButtons[OP_SPIN[activeOp]].classList.add('busy');
    }

    const detached = branchStatus && branchStatus.detached;
    netButtons.fetch.disabled = netOpActive;
    for (const op of ['pull', 'push', 'sync']) {
      const btn = netButtons[op];
      // Branch-bound ops need a branch; fetch works from anywhere.
      btn.disabled = netOpActive || !branchStatus || detached;
      btn.title = detached ? DETACHED_TITLE : OP_TITLES[op];
    }

    if (!branchStatus) {
      // No data yet (pre-handshake): show nothing rather than an empty pill.
      aheadBehind.hidden = true;
      aheadBehind.textContent = '';
      aheadBehind.disabled = true;
      aheadBehind.title = '';
      aheadBehind.setAttribute('aria-label', 'Ahead and behind counts');
      return;
    }
    aheadBehind.hidden = false;
    if (branchStatus.detached) {
      aheadBehind.textContent = 'detached';
      aheadBehind.disabled = true;
      aheadBehind.title = DETACHED_TITLE;
      aheadBehind.setAttribute('aria-label', 'Detached HEAD');
    } else if (branchStatus.upstream === null) {
      aheadBehind.textContent = '\u2191\u2731'; // ↑✱ — unpublished
      aheadBehind.disabled = netOpActive;
      aheadBehind.title = netOpActive
        ? 'A network operation is running'
        : 'Branch has no upstream — click to publish';
      aheadBehind.setAttribute('aria-label', 'Publish branch');
    } else {
      // textContent, not innerHTML: the upstream name is remote-controlled.
      aheadBehind.textContent = `\u2191${branchStatus.ahead} \u2193${branchStatus.behind}`;
      aheadBehind.disabled = true;
      aheadBehind.title = `ahead ${branchStatus.ahead}, behind ${branchStatus.behind} of ${branchStatus.upstream}`;
      aheadBehind.setAttribute('aria-label',
        `Ahead ${branchStatus.ahead}, behind ${branchStatus.behind} of ${branchStatus.upstream}`);
    }
  }

  /** Validates a host branchStatus payload; invalid shapes keep the last
   *  known state (host pushes are trusted in principle, but a malformed
   *  message must not blank the toolbar mid-session). */
  function parseBranchStatus(message) {
    if (
      typeof message.detached !== 'boolean' ||
      (typeof message.upstream !== 'string' && message.upstream !== null) ||
      !Number.isFinite(message.ahead) ||
      !Number.isFinite(message.behind) ||
      message.ahead < 0 ||
      message.behind < 0
    ) {
      return null;
    }
    return {
      detached: message.detached,
      upstream: message.upstream,
      ahead: Math.floor(message.ahead),
      behind: Math.floor(message.behind),
    };
  }

  for (const [op, btn] of Object.entries(netButtons)) {
    btn.addEventListener('click', () => {
      // The host re-checks in-flight state (netOp arriving mid-op is
      // dropped) — this guard just avoids obviously dead clicks.
      if (netOpActive || btn.disabled) return;
      vscode.postMessage({ type: 'netOp', op });
    });
  }

  aheadBehind.addEventListener('click', () => {
    if (aheadBehind.disabled || netOpActive) return;
    vscode.postMessage({ type: 'netOp', op: 'publish' });
  });

  updateToolbar();

  // -------------------------------------------------------------- filter bar

  const filterInput = document.getElementById('filter-input');
  const filterType = document.getElementById('filter-type');
  const filterClear = document.getElementById('filter-clear');
  const filterCount = document.getElementById('filter-count');
  const filterChips = document.getElementById('filter-chips');

  /** Typing debounce: each keystroke would be a full engine query — wait
   *  for the user to pause. */
  const FILTER_DEBOUNCE_MS = 300;

  /** Local mirror of the host's normalized filter (null = unfiltered).
   *  The host is authoritative: every filterState echo — including ones
   *  caused by this webview's own input — resyncs this (e.g. a filter
   *  set from the command palette). */
  let filter = null;

  let filterTimer = 0;

  function sendFilter(next) {
    vscode.postMessage({ type: 'setFilter', filter: next });
  }

  /** Applies the input's current text to the selected condition type and
   *  sends the whole filter (empty text clears that condition). */
  function commitFilterInput() {
    clearTimeout(filterTimer);
    const base = filter ?? { author: null, message: null, path: null };
    const text = filterInput.value.trim();
    sendFilter({ ...base, [filterType.value]: text || null });
  }

  filterInput.addEventListener('input', () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(commitFilterInput, FILTER_DEBOUNCE_MS);
  });
  // Enter commits immediately (bypasses the debounce).
  filterInput.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') commitFilterInput();
  });

  filterType.addEventListener('change', () => {
    // Swap the box to the newly selected condition's current text. A
    // pending debounced edit targeted the OLD type — drop it.
    clearTimeout(filterTimer);
    filterInput.value = filter ? filter[filterType.value] || '' : '';
    filterInput.focus();
  });

  filterClear.addEventListener('click', () => {
    clearTimeout(filterTimer);
    filterInput.value = '';
    sendFilter(null);
  });

  /** Inline DOM SVG × for chip remove buttons (CSP allows inline SVG).
   *  Invariant, so built once and cloned per chip. */
  const CHIP_REMOVE_ICON = (() => {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('width', '10');
    svg.setAttribute('height', '10');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('fill', 'none');
    path.setAttribute('stroke', 'currentColor');
    path.setAttribute('stroke-width', '1.4');
    path.setAttribute('stroke-linecap', 'round');
    path.setAttribute('d', 'M4 4l8 8 M12 4l-8 8');
    svg.appendChild(path);
    return svg;
  })();

  /** Rebuilds the condition chips from `filter` (textContent only:
   *  filter text is untrusted). Each chip removes its own condition. */
  function renderFilterChips() {
    filterChips.textContent = '';
    const active = filter
      ? ['author', 'message', 'path'].filter((type) => filter[type])
      : [];
    filterClear.hidden = active.length === 0;
    for (const type of active) {
      const chip = document.createElement('span');
      chip.className = 'filter-chip';
      const label = document.createElement('span');
      label.className = 'chip-label';
      label.textContent = `${type} ${filter[type]}`;
      label.title = `${type}: ${filter[type]}`;
      chip.appendChild(label);
      const remove = document.createElement('button');
      remove.className = 'chip-remove';
      remove.type = 'button';
      remove.title = `Remove the ${type} condition`;
      remove.setAttribute('aria-label', `Remove the ${type} condition`);
      remove.appendChild(CHIP_REMOVE_ICON.cloneNode(true));
      remove.addEventListener('click', () => {
        clearTimeout(filterTimer);
        if (type === filterType.value) filterInput.value = '';
        sendFilter({ ...filter, [type]: null });
      });
      chip.appendChild(remove);
      filterChips.appendChild(chip);
    }
  }

  /** Validates a host filterState payload: a filter object with
   *  string-or-null fields, or null. Returns undefined for malformed
   *  shapes (ignored — keeps the last known state). */
  function parseFilterState(payload) {
    if (payload === null || payload === undefined) return null;
    if (typeof payload !== 'object' || Array.isArray(payload)) return undefined;
    const field = (value) =>
      typeof value === 'string' ? value : value === null || value === undefined ? null : undefined;
    const author = field(payload.author);
    const message = field(payload.message);
    const path = field(payload.path);
    if (author === undefined || message === undefined || path === undefined) return undefined;
    return author || message || path ? { author, message, path } : null;
  }

  /** A new filter is a new query: the hit list occupies a different row
   *  space, so drop every cached row and refetch from the top — a fresh
   *  document, exactly. */
  function resetListForFilter() {
    commits.clear();
    idToRow = new Map();
    total = 0;
    anchorCommit = null;
    selectedRow = -1;
    spacer.style.height = `${ROW_HEIGHT}px`;
    scroller.scrollTop = 0;
    refillRows(); // skeleton-reset via fillRow (clears stale row state)
    applyWidth();
    renderRows();
    requestViewport();
  }

  /** filterState from the host (every change, including ones this webview
   *  initiated): resync the UI, and reset the list when the filter
   *  actually changed — the host dedups no-op changes, so an unchanged
   *  echo (the 'ready' handshake with no filter) costs nothing. */
  function applyFilterState(payload) {
    const parsed = parseFilterState(payload);
    if (parsed === undefined) return;
    const same =
      (parsed ? parsed.author : null) === (filter ? filter.author : null) &&
      (parsed ? parsed.message : null) === (filter ? filter.message : null) &&
      (parsed ? parsed.path : null) === (filter ? filter.path : null);
    filter = parsed;
    clearTimeout(filterTimer);
    filterInput.value = filter ? filter[filterType.value] || '' : '';
    renderFilterChips();
    filterCount.hidden = true; // re-shown by the next graphPage
    if (!same) resetListForFilter();
  }

  /** Hit count for the toolbar: exact while a filter is active (the
   *  engine's filtered total is exact), "\u2265 N" when the walk hit a
   *  safety cap. Hidden for unfiltered views (total_approx is an
   *  estimate there — a precise-looking count would lie). */
  function updateFilterCount(page) {
    if (!filter) {
      filterCount.hidden = true;
      return;
    }
    filterCount.hidden = false;
    filterCount.textContent = `${page.truncated ? '\u2265 ' : ''}${total} commit${total === 1 ? '' : 's'}`;
    filterCount.title = page.truncated
      ? 'At least this many commits match — the walk hit a safety cap'
      : 'Commits matching the filter';
  }

  /** Refs by commit id (branch/tag/remote tips, from getRefs). */
  let branchTips = new Map();
  /** HEAD commit id, for marking the HEAD row (null while unknown). */
  let headSha = null;

  /** Virtual row index -> commit (trimmed to roughly the last 2 viewports). */
  const commits = new Map();
  /** commit id -> virtual row index (for parent edge lookup). */
  let idToRow = new Map();
  /** Rendered row elements: virtual row index -> element. */
  const rowEls = new Map();

  let total = 0;
  let cacheLimit = 100; // rows per fetched page; trimmed against this
  let anchorCommit = null;
  let selectedRow = -1;
  let needsRedraw = false;
  /** Lane-area width override set by the user (splitter drag / keyboard);
   *  null = auto-size to the visible lane count. */
  let manualWidth = null;
  /** The lane-area width currently applied (via the --graph-width CSS var). */
  let graphWidth = GRAPH_MAX_WIDTH;

  // ---------------------------------------------------------------- scrolling

  function firstVisibleRow() {
    return Math.max(0, Math.floor(scroller.scrollTop / ROW_HEIGHT));
  }

  function visibleCount() {
    return Math.max(1, Math.ceil(scroller.clientHeight / ROW_HEIGHT));
  }

  function requestViewport() {
    const offset = firstVisibleRow();
    const limit = visibleCount() + ROW_BUFFER * 2;
    const top = commits.get(offset);
    if (top) anchorCommit = top.id;
    vscode.postMessage({ type: 'viewportChanged', offset, limit, anchorCommit });
  }

  let viewportTimer = 0;
  function scheduleViewportRequest() {
    clearTimeout(viewportTimer);
    viewportTimer = setTimeout(requestViewport, VIEWPORT_DEBOUNCE_MS);
  }

  scroller.addEventListener('scroll', () => {
    renderRows();
    scheduleViewportRequest();
  });

  new ResizeObserver(() => {
    renderRows();
    needsRedraw = true;
    scheduleViewportRequest();
  }).observe(scroller);

  // Fires before the native webview/context menu opens: report the
  // right-clicked commit so the host can park it for the menu commands
  // (contextmenu -> menu command is the hand-off path; command arguments
  // cannot carry webview data). A null commit (skeleton row or any row the
  // delegated lookup misses) clears the host's parked target so a stale
  // one can't outlive the row it came from — skeleton rows carry no
  // data-vscode-context and thus show no native menu either.
  rowsEl.addEventListener('contextmenu', (e) => {
    const rowEl = e.target instanceof Element ? e.target.closest('.row') : null;
    if (!rowEl) return;
    const commit = commits.get(Number(rowEl.dataset.row));
    vscode.postMessage({
      type: 'contextTarget',
      commit: commit ? { id: commit.id, message: commit.message } : null,
    });
  });

  // ------------------------------------------------------------- row rendering

  function renderRows() {
    const first = Math.max(0, firstVisibleRow() - ROW_BUFFER);
    const last = Math.min(Math.max(total - 1, 0), first + visibleCount() + ROW_BUFFER * 2);

    for (const [row, el] of rowEls) {
      if (row < first || row > last) {
        el.remove();
        rowEls.delete(row);
      }
    }
    for (let row = first; row <= last; row++) {
      let el = rowEls.get(row);
      if (!el) {
        el = document.createElement('div');
        el.className = 'row';
        el.style.top = `${row * ROW_HEIGHT}px`;
        // Single click opens the commit detail panel — the chosen UX over
        // select-then-double-click (inspection stays one gesture away) — and
        // also moves the selection highlight. The detail message is only
        // posted for real commits: skeleton rows have no commit to show,
        // and the host re-validates the id anyway (webview → host is a
        // trust boundary).
        el.addEventListener('click', () => {
          const commit = commits.get(row);
          if (commit) vscode.postMessage({ type: 'openCommitDetail', id: commit.id });
          selectRow(row);
        });
        // Row index for the delegated contextmenu handler (element -> row).
        el.dataset.row = String(row);
        rowsEl.appendChild(el);
        rowEls.set(row, el);
      }
      const commit = commits.get(row);
      const commitId = commit ? commit.id : '';
      if (el.dataset.commitId !== commitId) {
        fillRow(el, commit, row);
      }
      el.classList.toggle('selected', row === selectedRow);
    }
    needsRedraw = true;
  }

  function fillRow(el, commit, row) {
    el.dataset.commitId = commit ? commit.id : '';
    // textContent-only population: commit messages are untrusted.
    el.textContent = '';
    // Native webview/context menu targeting: only real commit rows opt in
    // (skeleton rows show no menu). The value is static JSON, not markup.
    if (commit) {
      el.setAttribute(
        'data-vscode-context',
        JSON.stringify({ webviewSection: 'commitRow', preventDefaultContextMenuItems: true }),
      );
    } else {
      el.removeAttribute('data-vscode-context');
    }
    const gap = document.createElement('span');
    gap.className = 'graph-gap';
    el.appendChild(gap);
    if (!commit) {
      el.classList.remove('head-row');
      el.removeAttribute('title');
      return; // placeholder skeleton row while data loads
    }

    const message = document.createElement('span');
    message.className = 'message';
    message.textContent = commit.message.split('\n')[0];
    el.appendChild(message);

    const tips = branchTips.get(commit.id);
    const isHead = commit.id === headSha;
    el.classList.toggle('head-row', isHead);
    if (isHead || tips) {
      const tags = document.createElement('span');
      tags.className = 'branch-tags';
      if (isHead) {
        const headPill = document.createElement('span');
        headPill.className = 'head-pill';
        headPill.textContent = 'HEAD';
        tags.appendChild(headPill);
      }
      if (tips) {
        // Render order: local branches, tags, then remote-tracking branches.
        const ordered = [...tips].sort((a, b) => REF_ORDER[a.kind] - REF_ORDER[b.kind]);
        for (const ref of ordered) {
          const pill = document.createElement('span');
          // Tags are distinguished by the tag icon + per-name border color
          // (no dedicated class); remote branches by the remote-pill style.
          pill.className =
            ref.kind === 'remote_branch' ? 'pill branch-tag remote-pill' : 'pill branch-tag';
          if (ref.kind !== 'remote_branch') pill.style.borderColor = tagColor(ref.name);
          if (ref.kind === 'tag') pill.appendChild(tagIcon());
          // Text node, not innerHTML: ref names are untrusted.
          pill.appendChild(document.createTextNode(ref.name));
          tags.appendChild(pill);
        }
      }
      el.appendChild(tags);
    }

    const meta = document.createElement('span');
    meta.className = 'meta';
    const author = document.createElement('span');
    author.className = 'author';
    author.textContent = commit.author_name;
    const time = document.createElement('span');
    time.className = 'time';
    time.textContent = formatTime(commit.author_time);
    meta.appendChild(author);
    meta.appendChild(time);
    el.appendChild(meta);
    // Full author + timestamp on hover (Git Graph convention).
    el.title = `${commit.author_name} ${formatTime(commit.author_time)}`;
  }

  function selectRow(row) {
    if (selectedRow >= 0) {
      rowEls.get(selectedRow)?.classList.remove('selected');
    }
    selectedRow = row;
    rowEls.get(row)?.classList.add('selected');
  }

  /** Stable color per ref name (same palette as the lanes). */
  function tagColor(name) {
    let hash = 0;
    for (let i = 0; i < name.length; i++) {
      hash = (hash * 31 + name.charCodeAt(i)) | 0;
    }
    return LANE_COLORS[Math.abs(hash) % LANE_COLORS.length];
  }

  /** Pill render order by ref kind (branch → tag → remote). */
  const REF_ORDER = { branch: 0, tag: 1, remote_branch: 2 };

  /** Inline DOM SVG tag icon (CSP allows inline SVG; no markup injection).
   *  The icon is invariant, so build it once and clone per pill. */
  const TAG_ICON_TEMPLATE = (() => {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('class', 'tag-icon');
    svg.setAttribute('aria-hidden', 'true');
    const shape = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    shape.setAttribute('fill-rule', 'evenodd');
    shape.setAttribute(
      'd',
      'M1 1h6.6l7.4 7.4-6.6 6.6L1 7.6V1zm4 2.8a1.2 1.2 0 1 0 0 2.4 1.2 1.2 0 0 0 0-2.4z',
    );
    svg.appendChild(shape);
    return svg;
  })();

  function tagIcon() {
    return TAG_ICON_TEMPLATE.cloneNode(true);
  }

  // ------------------------------------------------------- lane-area width

  /** Last width computed from real commit data (never shrinks back to the
   *  minimum when the cache is momentarily empty, so invalidation bursts
   *  don't make the gutter visually collapse and re-expand). */
  let lastAutoWidth = GRAPH_MAX_WIDTH;

  /** Auto width from the widest lane in the cached commits (the cache keeps
   *  ~3 pages of rows, so it is representative of the visible window). */
  function computeAutoWidth() {
    if (commits.size === 0) return lastAutoWidth; // invalidated/empty cache
    let maxLane = 0;
    for (const commit of commits.values()) {
      if (typeof commit.lane === 'number' && commit.lane > maxLane) maxLane = commit.lane;
    }
    const width = LANE_X_START + (maxLane + 1) * LANE_X_STEP + GRAPH_RIGHT_PAD;
    lastAutoWidth = Math.min(GRAPH_MAX_WIDTH, Math.max(GRAPH_MIN_WIDTH, width));
    return lastAutoWidth;
  }

  /** Applies manualWidth ?? computeAutoWidth() to the CSS variable the rows,
   *  overlay, and splitter all derive their geometry from. */
  function applyWidth() {
    graphWidth = manualWidth !== null ? manualWidth : computeAutoWidth();
    document.documentElement.style.setProperty('--graph-width', `${graphWidth}px`);
    needsRedraw = true; // canvas re-reads clientWidth on the next draw
  }

  /** Upper clamp for manual widths: keep at least ~40% for the messages. */
  function maxWidth() {
    return Math.max(GRAPH_MIN_WIDTH, Math.floor(scroller.clientWidth * 0.6));
  }

  function clampWidth(width) {
    return Math.min(maxWidth(), Math.max(GRAPH_MIN_WIDTH, Math.round(width)));
  }

  // ------------------------------------------------------------- splitter drag

  const KEYBOARD_STEP = 8;
  const KEYBOARD_STEP_LARGE = 32; // Shift+arrow

  let dragging = false;

  splitter.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    // No text selection and no synthesized dblclick during the drag.
    e.preventDefault();
    try {
      splitter.setPointerCapture(e.pointerId);
    } catch {
      // The pointer vanished before capture (NotFoundError): abort the drag
      // instead of leaving the splitter stuck in the dragging state.
      return;
    }
    dragging = true;
    splitter.classList.add('dragging');
  });

  splitter.addEventListener('pointermove', (e) => {
    if (!dragging) return;
    manualWidth = clampWidth(e.clientX);
    applyWidth();
  });

  /** Ends the drag and persists the final width (only here — persisting on
   *  every pointermove would hammer globalState with a write storm). */
  function endDrag() {
    if (!dragging) return;
    dragging = false;
    splitter.classList.remove('dragging');
    if (manualWidth !== null) {
      vscode.postMessage({ type: 'graphWidthChanged', width: manualWidth });
    }
  }

  splitter.addEventListener('pointerup', endDrag);
  splitter.addEventListener('lostpointercapture', endDrag);

  // The splitter sits outside #scroller (it must overlay the rows), so wheel
  // events over it would not reach the scroller — forward them manually,
  // normalizing delta modes (line/page) so the scroll distance matches what
  // the scroller itself would do. preventDefault only when we consumed a
  // delta, so zero-delta events keep their default behavior (e.g. zoom).
  splitter.addEventListener(
    'wheel',
    (e) => {
      const unit =
        e.deltaMode === WheelEvent.DOM_DELTA_LINE
          ? ROW_HEIGHT
          : e.deltaMode === WheelEvent.DOM_DELTA_PAGE
            ? Math.max(ROW_HEIGHT, scroller.clientHeight)
            : 1;
      const delta = e.deltaY * unit;
      if (delta === 0) return;
      scroller.scrollTop += delta;
      e.preventDefault();
    },
    { passive: false },
  );

  splitter.addEventListener('dblclick', resetToAutoWidth);

  splitter.addEventListener('keydown', (e) => {
    const step = e.shiftKey ? KEYBOARD_STEP_LARGE : KEYBOARD_STEP;
    let handled = true;
    switch (e.key) {
      case 'ArrowLeft':
        setManualWidth((manualWidth ?? graphWidth) - step);
        break;
      case 'ArrowRight':
        setManualWidth((manualWidth ?? graphWidth) + step);
        break;
      case 'Home':
        setManualWidth(GRAPH_MIN_WIDTH);
        break;
      case 'End':
        setManualWidth(maxWidth());
        break;
      case 'Enter':
        resetToAutoWidth();
        break;
      default:
        handled = false;
    }
    if (handled) e.preventDefault();
  });

  function setManualWidth(width) {
    manualWidth = clampWidth(width);
    applyWidth();
    vscode.postMessage({ type: 'graphWidthChanged', width: manualWidth });
  }

  /** Drops the manual override and returns to lane-count-based auto width. */
  function resetToAutoWidth() {
    manualWidth = null;
    applyWidth();
    vscode.postMessage({ type: 'graphWidthChanged', width: null });
  }

  // ------------------------------------------------------------------- canvas

  function laneX(lane) {
    return LANE_X_START + lane * LANE_X_STEP;
  }

  function laneColor(lane) {
    return LANE_COLORS[lane % LANE_COLORS.length];
  }

  function rowCenter(row) {
    return row * ROW_HEIGHT + ROW_HEIGHT / 2;
  }

  function draw() {
    const dpr = window.devicePixelRatio || 1;
    const width = canvas.clientWidth || graphWidth;
    const height = canvas.clientHeight || 0;
    if (canvas.width !== Math.round(width * dpr) || canvas.height !== Math.round(height * dpr)) {
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, width, height);
    if (height === 0) return;

    ctx.lineWidth = 2;
    ctx.lineCap = 'round';
    const scrollTop = scroller.scrollTop;
    const clipTop = -ROW_HEIGHT;
    const clipBottom = height + ROW_HEIGHT;

    // Edges: child (row, lane) -> parent (parentRow, parentLane).
    for (const [row, commit] of commits) {
      const y1 = rowCenter(row) - scrollTop;
      const x1 = laneX(commit.lane);
      for (const parentId of commit.parent_ids) {
        const parentRow = idToRow.get(parentId);
        const parent = parentRow !== undefined ? commits.get(parentRow) : undefined;
        const x2 = parent ? laneX(parent.lane) : x1;
        // Parent below the fetched window: stub one row downward.
        const y2 = parent ? rowCenter(parentRow) - scrollTop : y1 + ROW_HEIGHT;
        ctx.strokeStyle = laneColor(commit.lane);
        if (x1 === x2) {
          // Straight vertical edge — clip to the canvas since these can span
          // thousands of rows.
          const sy = Math.max(Math.min(y1, y2), clipTop);
          const ey = Math.min(Math.max(y1, y2), clipBottom);
          if (sy <= ey) {
            ctx.beginPath();
            ctx.moveTo(x1, sy);
            ctx.lineTo(x1, ey);
            ctx.stroke();
          }
        } else if (y1 >= clipTop - ROW_HEIGHT && y1 <= clipBottom + ROW_HEIGHT) {
          // Lane change: smooth Bézier (down, then across).
          const mid = (y1 + y2) / 2;
          ctx.beginPath();
          ctx.moveTo(x1, y1);
          ctx.bezierCurveTo(x1, mid, x2, mid, x2, y2);
          ctx.stroke();
        }
      }
    }

    // Commit dots drawn on top of the edges.
    for (const [row, commit] of commits) {
      const y = rowCenter(row) - scrollTop;
      if (y < clipTop || y > clipBottom) continue;
      ctx.fillStyle = laneColor(commit.lane);
      ctx.beginPath();
      ctx.arc(laneX(commit.lane), y, 3.5, 0, Math.PI * 2);
      ctx.fill();
    }
  }

  function frame() {
    if (needsRedraw) {
      draw();
      needsRedraw = false;
    }
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);

  // ---------------------------------------------------------- data management

  function applyPage(offset, page) {
    total = page.total_approx;
    if (page.commits.length > 0) cacheLimit = Math.max(50, page.commits.length);
    spacer.style.height = `${Math.max(1, total) * ROW_HEIGHT}px`;
    for (let i = 0; i < page.commits.length; i++) {
      commits.set(offset + i, page.commits[i]);
    }
    trimCache();
    rebuildIdIndex();
    applyWidth(); // new data may change the visible lane count
    renderRows();
    updateFilterCount(page);
  }

  /** Keep roughly the last two viewports (± current window) client-side. */
  function trimCache() {
    const capacity = cacheLimit * 3;
    if (commits.size <= capacity) return;
    const keepFrom = firstVisibleRow() - cacheLimit;
    const keepTo = firstVisibleRow() + cacheLimit * 2;
    for (const row of commits.keys()) {
      if (row < keepFrom || row > keepTo) commits.delete(row);
    }
  }

  function rebuildIdIndex() {
    idToRow = new Map();
    for (const [row, commit] of commits) {
      idToRow.set(commit.id, row);
    }
  }

  /** Forces the next renderRows() pass to re-run fillRow on every rendered
   *  row. '' is the skeleton commitId, so a plain clear would NOT re-run
   *  fillRow on skeleton rows and their stale attributes (context-menu
   *  target, title, head-row class) would survive; the sentinel never
   * matches a real commit id, and fillRow's skeleton path clears them. */
  function refillRows() {
    for (const el of rowEls.values()) el.dataset.commitId = REFILL_SENTINEL;
    renderRows();
  }

  function invalidate() {
    const topRow = firstVisibleRow();
    commits.clear();
    idToRow = new Map();
    refillRows(); // skeleton-reset via fillRow (clears stale row state)
    applyWidth(); // auto mode: the lane count of the refetched data may differ
    // Preserve the scroll anchor; the server uses anchor_commit to re-pin.
    vscode.postMessage({
      type: 'viewportChanged',
      offset: topRow,
      limit: visibleCount() + ROW_BUFFER * 2,
      anchorCommit,
    });
  }

  window.addEventListener('message', (event) => {
    const message = event.data;
    if (!message || typeof message.type !== 'string') return;
    if (message.type === 'graphPage' && message.page) {
      applyPage(message.offset | 0, message.page);
    } else if (message.type === 'filterState') {
      applyFilterState(message.filter);
    } else if (message.type === 'graphInvalidated') {
      invalidate();
    } else if (message.type === 'refs' && Array.isArray(message.refs)) {
      applyRefs(message.refs);
    } else if (message.type === 'head' && typeof message.sha === 'string') {
      // Force re-fill of rendered rows so the HEAD marker moves.
      headSha = message.sha;
      refillRows();
    } else if (message.type === 'graphWidth') {
      // Host-persisted width from the 'ready' handshake (null = auto). Clamp
      // to the static bounds only — the container is typically not laid out
      // yet at handshake time, and the 60%-of-container bound (maxWidth)
      // applies to user resizing, not to restoring a persisted value.
      manualWidth =
        typeof message.width === 'number'
          ? Math.min(GRAPH_MAX_WIDTH, Math.max(GRAPH_MIN_WIDTH, Math.round(message.width)))
          : null;
      applyWidth();
    } else if (message.type === 'netOpState' && typeof message.active === 'boolean') {
      // Host echo: the op starts (spinner + row disable) and ends (reset).
      netOpActive = message.active;
      activeOp =
        message.active && typeof message.op === 'string' && Object.hasOwn(OP_SPIN, message.op)
          ? message.op
          : null;
      updateToolbar();
      updateRebaseBanner();
    } else if (message.type === 'rebaseControlState' && typeof message.active === 'boolean') {
      // Host echo for the banner's own controls (same disable-while-running
      // discipline as the toolbar's netOpState).
      rebaseControlActive = message.active;
      updateRebaseBanner();
    } else if (message.type === 'rebaseStatus') {
      const parsed = parseRebaseStatus(message);
      if (parsed !== undefined) rebaseStatus = parsed;
      updateRebaseBanner();
    } else if (message.type === 'branchStatus') {
      const parsed = parseBranchStatus(message);
      if (parsed) branchStatus = parsed;
      updateToolbar();
    }
  });

  /** Index ref tips (branch/tag/remote) by target commit id and re-render. */
  function applyRefs(refs) {
    branchTips = new Map();
    for (const ref of refs) {
      if (!ref || typeof ref.name !== 'string' || !ref.target) continue;
      if (!Object.hasOwn(REF_ORDER, ref.kind)) continue; // branch | tag | remote_branch
      const list = branchTips.get(ref.target);
      if (list) list.push({ name: ref.name, kind: ref.kind });
      else branchTips.set(ref.target, [{ name: ref.name, kind: ref.kind }]);
    }
    // Force re-fill of already-rendered rows so tags appear without a scroll.
    refillRows();
  }

  // Initial load. The 'ready' message triggers the host to send back the
  // persisted graph width (see the 'graphWidth' handler above).
  renderRows();
  applyWidth();
  requestViewport();
  vscode.postMessage({ type: 'ready' });
})();
