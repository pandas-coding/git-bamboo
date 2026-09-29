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
          pill.className = ref.kind === 'remote_branch' ? 'branch-tag remote-pill' : 'branch-tag';
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
