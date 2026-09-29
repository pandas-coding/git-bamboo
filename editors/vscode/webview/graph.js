/**
 * Commit graph webview: virtual scrolling (fixed 24px rows), canvas overlay
 * with Bézier branch curves drawn from commit lane + parent_ids, and
 * viewport paging driven by postMessage round-trips to the extension host.
 * Vanilla JS, no framework.
 */
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

  /** Branch names by commit id (branch tips, from the extension's getRefs). */
  let branchTips = new Map();

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
  // cannot carry webview data). Skeleton rows have no data-vscode-context
  // attribute and thus no menu — and no target either.
  rowsEl.addEventListener('contextmenu', (e) => {
    const rowEl = e.target instanceof Element ? e.target.closest('.row') : null;
    if (!rowEl) return;
    const commit = commits.get(Number(rowEl.dataset.row));
    if (commit) {
      vscode.postMessage({
        type: 'contextTarget',
        commit: { id: commit.id, message: commit.message, author_name: commit.author_name },
      });
    }
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
        el.addEventListener('click', () => selectRow(row));
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
    if (!commit) return; // placeholder skeleton row while data loads

    const message = document.createElement('span');
    message.className = 'message';
    message.textContent = commit.message.split('\n')[0];
    el.appendChild(message);

    const tips = branchTips.get(commit.id);
    if (tips) {
      const tags = document.createElement('span');
      tags.className = 'branch-tags';
      for (const name of tips) {
        const tag = document.createElement('span');
        tag.className = 'branch-tag';
        tag.style.borderColor = tagColor(name);
        tag.textContent = name; // textContent-only: ref names are untrusted
        tags.appendChild(tag);
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
  }

  function selectRow(row) {
    if (selectedRow >= 0) {
      rowEls.get(selectedRow)?.classList.remove('selected');
    }
    selectedRow = row;
    rowEls.get(row)?.classList.add('selected');
  }

  function formatTime(unixSeconds) {
    try {
      return new Date(unixSeconds * 1000).toLocaleDateString();
    } catch {
      return '';
    }
  }

  /** Stable color per branch name (same palette as the lanes). */
  function tagColor(name) {
    let hash = 0;
    for (let i = 0; i < name.length; i++) {
      hash = (hash * 31 + name.charCodeAt(i)) | 0;
    }
    return LANE_COLORS[Math.abs(hash) % LANE_COLORS.length];
  }

  // ------------------------------------------------------- lane-area width

  /** Auto width from the widest lane in the cached commits (the cache keeps
   *  ~3 pages of rows, so it is representative of the visible window). */
  function computeAutoWidth() {
    let maxLane = 0;
    for (const commit of commits.values()) {
      if (typeof commit.lane === 'number' && commit.lane > maxLane) maxLane = commit.lane;
    }
    const width = LANE_X_START + (maxLane + 1) * LANE_X_STEP + GRAPH_RIGHT_PAD;
    return Math.min(GRAPH_MAX_WIDTH, Math.max(GRAPH_MIN_WIDTH, width));
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
  // events over it would not reach the scroller — forward them manually.
  splitter.addEventListener(
    'wheel',
    (e) => {
      scroller.scrollTop += e.deltaY;
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

  function invalidate() {
    const topRow = firstVisibleRow();
    commits.clear();
    idToRow = new Map();
    for (const el of rowEls.values()) {
      el.textContent = '';
      el.dataset.commitId = '';
    }
    applyWidth(); // auto mode: the lane count of the refetched data may differ
    renderRows(); // skeletons
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
    } else if (message.type === 'graphInvalidated') {
      invalidate();
    } else if (message.type === 'refs' && Array.isArray(message.refs)) {
      applyRefs(message.refs);
    } else if (message.type === 'graphWidth') {
      // Host-persisted width from the initial 'ready' handshake (null = auto).
      manualWidth = typeof message.width === 'number' ? clampWidth(message.width) : null;
      applyWidth();
    }
  });

  /** Index branch tips by target commit id and re-render visible rows. */
  function applyRefs(refs) {
    branchTips = new Map();
    for (const ref of refs) {
      if (!ref || ref.kind !== 'branch' || typeof ref.name !== 'string' || !ref.target) continue;
      const list = branchTips.get(ref.target);
      if (list) list.push(ref.name);
      else branchTips.set(ref.target, [ref.name]);
    }
    // Force re-fill of already-rendered rows so tags appear without a scroll.
    for (const el of rowEls.values()) el.dataset.commitId = '';
    renderRows();
  }

  // Initial load. The 'ready' message triggers the host to send back the
  // persisted graph width (see the 'graphWidth' handler above).
  renderRows();
  applyWidth();
  requestViewport();
  vscode.postMessage({ type: 'ready' });
})();
