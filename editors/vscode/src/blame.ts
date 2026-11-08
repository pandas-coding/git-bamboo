/**
 * JetBrains-style blame (phase 3): gutter annotations, hover details and
 * cursor-line navigation for repository files, all rendered from the
 * engine's `getBlame` RPC.
 *
 * LAYERING DECISION — decoration vs CodeLens (recorded here as required by
 * .ai-workflow/learnings/20261029-codelens-hunk-actions.md): attribution
 * TEXT rides on a TextEditorDecorationType `before` attachment, while the
 * two per-line ACTIONS ride on CodeLens. The learnings doc measured
 * CodeLens as the better surface for actions (keyboard accessible, no
 * margin-hover affordance to build, scoped by document selector) — but it
 * cannot carry text at all, and VS Code's gutter API only accepts images
 * (`gutterIconPath`), so annotation text has no alternative carrier. Both
 * layers therefore coexist instead of one winning, and the CodeLens layer
 * keeps the doc's central rule: lens targets are derived STATELESSLY from
 * the editor showing the document plus its cursor line, recomputed on
 * every selection change. There is no open-intent state to go stale — the
 * bug class the hunk-staging controller was rewritten to avoid.
 *
 * Everything renders from ONE getBlame round trip per (path, rev, buffer
 * version): the engine joins commit metadata server-side, so a per-line or
 * per-entry RPC from here would be a protocol violation, not just slow.
 */
import * as path from 'node:path';
import * as vscode from 'vscode';
import { openCommitDetailPanel } from './commitDetail';
import { EngineClient } from './engineClient';
import { commitRevisionUri, EMPTY_REVISION, ENGINE_SCHEME, isValidRepoPath } from './headContent';
import { isUncommittedBlame, type BlameCommit, type BlameEntry, type GetBlameResult } from './types';
import { COMMIT_ID_RE, debounce, errorMessage, relativeTime } from './util';

/** Command ids (package.json registers all five; only `toggle` is in the
 *  palette — the rest are hover-link / CodeLens entry points). */
export const BLAME_TOGGLE_COMMAND = 'gitBamboo.blame.toggle';
export const BLAME_SHOW_COMMIT_COMMAND = 'gitBamboo.blame.showCommit';
export const BLAME_DIFF_LINE_COMMAND = 'gitBamboo.blame.diffLine';
export const BLAME_JUMP_TO_COMMIT_COMMAND = 'gitBamboo.blame.jumpToCommit';
export const BLAME_ANNOTATE_PREVIOUS_COMMAND = 'gitBamboo.blame.annotatePrevious';

/** CodeLens titles (the lens display text). */
const JUMP_LENS_TITLE = 'Jump to Introducing Commit';
const PREVIOUS_LENS_TITLE = 'Annotate Previous Revision';

/** Documents blame applies to: worktree files, plus the engine scheme so
 *  the revision documents opened by annotatePrevious are themselves
 *  annotatable (that is what makes the walk-back chainable). */
const SELECTOR: vscode.DocumentSelector = [{ scheme: 'file' }, { scheme: ENGINE_SCHEME }];

/** Trailing debounce for re-rendering (editor switch, edits) and the
 *  throttle interval for scroll re-slicing: both fire in bursts and each
 *  settled render costs one decoration push (plus at most one RPC).
 *  GitLens' 150ms recipe. */
const RENDER_DEBOUNCE_MS = 150;

/** Trailing debounce for the cursor-line CodeLens recompute: arrowing
 *  through a file fires a selection event per key. */
const LENS_DEBOUNCE_MS = 100;

/** How long a blame response is served before a refresh is kicked. Short
 *  on purpose: engine notifications invalidate immediately, the TTL only
 *  covers changes made behind the engine's back. */
const CACHE_TTL_MS = 3_000;

/** TTL for a FAILED blame (untracked file, binary, engine gone). Longer:
 *  retrying every 3s while the user scrolls a file that can never be
 *  blamed is pure noise, and the failure is reported once anyway. */
const NEGATIVE_TTL_MS = 30_000;

/** Above this line count only the visible viewport is decorated
 *  (re-sliced on every scroll), the GitLens recipe for large files: a
 *  whole-file decoration push per scroll tick is what makes blame feel
 *  laggy on multi-thousand-line files. */
const VIEWPORT_SLICE_LINES = 1_000;

/** Dirty-buffer blame keys carry the document version, so typing would
 *  grow the cache without bound — oldest entries are evicted past this. */
const MAX_CACHE_ENTRIES = 32;

/** Annotation column budget, in monospace cells. Compact on purpose: the
 *  column is reserved to the left of the code on every annotated line, so
 *  a long author name costs the whole file horizontal space. */
const MAX_ANNOTATION_WIDTH_CELLS = 32;

/** Extra cells after the longest annotation, so text never butts against
 *  the code. */
const ANNOTATION_PADDING_CELLS = 2;

/** GitLens' gutter-annotation CSS (plan step 4). The `before` attachment is
 *  an inline box at the line start; the column is reserved by the space
 *  padding in padToWidth (with `width` as a CSS hint for hosts that honour
 *  it on inline boxes) and the right margin keeps the gap before the code.
 *  Tune the margin here if annotations ever sit too close to (or over) the
 *  line numbers. */
const ANNOTATION_MARGIN = '0 26px -1px 0';

/** Deliberately a `gitDecoration*` foreground: every built-in theme defines
 *  the ignored-resource colour as a dimmed grey, so annotations read as
 *  metadata and never compete with code or with the real change colours. */
const ANNOTATION_COLOR = 'gitDecoration.ignoredResourceForeground';

/** Label for the engine's all-zero commit id (lines that exist only in the
 *  blamed buffer). */
const NOT_COMMITTED_LABEL = 'Uncommitted changes';

/** Dirty-buffer payload ceiling, in UTF-8 BYTES of the buffer text. Two
 *  engine limits bound this and the tighter one is fatal rather than
 *  graceful: a request frame over 16 MiB is a FRAMING violation that closes
 *  the connection (the whole session dies), while `contents` over the
 *  engine's own 8 MiB cap is an in-band INVALID_ARGUMENT. JSON escaping can
 *  inflate a string up to 6x (control characters become `\u00XX`), so 2 MiB
 *  of text is the largest payload that provably stays under the frame cap in
 *  the worst case — a bigger buffer is blamed as the saved file instead. */
const MAX_DIRTY_CONTENTS_BYTES = 2 * 1024 * 1024;

/** Memoized blameTarget derivation, keyed by buffer state. */
interface TargetMemo {
  readonly version: number;
  readonly dirty: boolean;
  readonly target: BlameTarget | undefined;
}

/** One `(path, rev, buffer)` blame result plus its derived render state. */
interface CachedBlame {
  /** undefined = the RPC failed; the entry is then a negative cache hit. */
  readonly result: GetBlameResult | undefined;
  readonly fetchedAt: number;
  readonly negative: boolean;
  /** Built on first render, dies with the entry (so relative times are
   *  rebuilt on the next fetch instead of freezing at "3m ago"). */
  render?: BlameRender;
}

/** Derived, per-document render state: the annotation texts and the ONE
 *  column width shared by every line. */
interface BlameRender {
  /** Column width in monospace cells. Every line reserves the same width —
   *  Monaco lays line content out in inline flow, so per-line widths would
   *  leave the code ragged. */
  readonly widthCells: number;
  readonly text: ReadonlyMap<string, string>;
  /** Per-commit attachment options: allocating one style object per line
   *  of a heavily-annotated file is what makes blame renders expensive. */
  readonly options: Map<string, vscode.ThemableDecorationAttachmentRenderOptions>;
}

/** What one document needs blamed, and the cache key it answers to. */
interface BlameTarget {
  readonly key: string;
  /** Repo-relative slash path for the engine RPC. */
  readonly path: string;
  /** null = blame the worktree file (what a saved/dirty editor shows);
   *  'HEAD' or a full sha = blame that revision's blob (what a git-bamboo:
   *  revision document shows). The engine picks the blamed content from
   *  this plus `contents`, so the two must describe the same bytes the
   *  document renders. */
  readonly rev: string | null;
  /** Unsaved buffer text for dirty-buffer blame, else null. */
  readonly contents: string | null;
}

/** Per-document identity for the toggle state (an editor is identified by
 *  its document; splits of one document share the state). */
function documentKey(document: vscode.TextDocument): string {
  return document.uri.toString(true);
}

/** Cache key for one (path, rev, buffer) blame; `dirtyVersion` null means
 *  the worktree/saved content. Shared by deriveTarget and the
 *  annotatePrevious parent pre-check so a pre-fetched parent revision is a
 *  cache hit for the document opened afterwards. */
function cacheKey(blamePath: string, rev: string | null, dirtyVersion: number | null): string {
  return `${blamePath}\u0000${rev ?? 'HEAD'}\u0000${dirtyVersion === null ? 'worktree' : `v${dirtyVersion}`}`;
}

function isExpired(entry: CachedBlame): boolean {
  return Date.now() - entry.fetchedAt > (entry.negative ? NEGATIVE_TTL_MS : CACHE_TTL_MS);
}

export class BlameController implements vscode.Disposable {
  private readonly subscriptions: vscode.Disposable[] = [];
  private readonly lensesChanged = new vscode.EventEmitter<void>();
  /** The single shared annotation style; per-commit content/width ride on
   *  the decoration instances. One type (not one per commit) means there
   *  is nothing to dispose per file — a createTextEditorDecorationType
   *  registers a workbench CSS class that lives until disposed, so a
   *  per-commit map would leak styles over a long session. */
  private readonly annotationType: vscode.TextEditorDecorationType;
  /** One-shot highlight of the line annotatePrevious preserved. */
  private readonly lineHighlightType: vscode.TextEditorDecorationType;
  /** Documents the user opted in to (blame is OFF by default). */
  private readonly enabled = new Set<string>();
  private readonly cache = new Map<string, CachedBlame>();
  /** In-flight getBlame per cache key: decorations, hover and lenses share
   *  one round trip instead of racing three. */
  private readonly inflight = new Map<string, Promise<GetBlameResult | undefined>>();
  /** Paths already reported as un-blamable (one status message each). */
  private readonly warned = new Set<string>();
  /** documentKey → highlighted line (annotatePrevious). */
  private readonly highlighted = new Map<string, number>();
  /** documentKey → derived blame target per buffer state (see blameTarget). */
  private readonly targets = new Map<string, TargetMemo>();
  /** Bumped on invalidate(): in-flight responses from before an
   *  invalidation are dropped instead of re-populating the cache. */
  private generation = 0;
  private readonly scheduleRender = debounce(() => this.renderVisibleEditors(), RENDER_DEBOUNCE_MS);
  private readonly scheduleLenses = debounce(() => this.lensesChanged.fire(), LENS_DEBOUNCE_MS);
  /** Last scroll-driven re-slice, for the throttle in onScrolled. */
  private lastSliceRender = 0;
  private disposed = false;

  constructor(
    private readonly context: vscode.ExtensionContext,
    private readonly client: EngineClient,
    private readonly root: vscode.Uri,
  ) {
    this.annotationType = vscode.window.createTextEditorDecorationType({
      before: { color: new vscode.ThemeColor(ANNOTATION_COLOR), fontStyle: 'italic' },
    });
    this.lineHighlightType = vscode.window.createTextEditorDecorationType({
      isWholeLine: true,
      backgroundColor: new vscode.ThemeColor('editor.selectionHighlightBackground'),
    });
    this.subscriptions.push(
      vscode.languages.registerHoverProvider(SELECTOR, {
        provideHover: (document, position) => this.provideHover(document, position),
      }),
      vscode.languages.registerCodeLensProvider(SELECTOR, {
        onDidChangeCodeLenses: this.lensesChanged.event,
        provideCodeLenses: (document) => this.provideLenses(document),
      }),
      vscode.window.onDidChangeActiveTextEditor(() => {
        this.scheduleRender();
        this.scheduleLenses();
      }),
      // Scroll: only viewport-sliced files need a re-render here, and it is
      // throttled rather than purely trailing — see onScrolled.
      vscode.window.onDidChangeTextEditorVisibleRanges((event) => this.onScrolled(event.textEditor)),
      vscode.window.onDidChangeTextEditorSelection((event) => {
        this.clearLineHighlight(event.textEditor);
        this.scheduleLenses();
      }),
      // Edits change the dirty-buffer blame (the cache key carries the
      // document version), so a settled burst re-blames the buffer.
      vscode.workspace.onDidChangeTextDocument(() => {
        this.scheduleRender();
        this.scheduleLenses();
      }),
      vscode.workspace.onDidSaveTextDocument((document) => {
        // Saving does not fire a content change, but it turns a dirty-buffer
        // blame into a worktree blame — re-render to drop the uncommitted
        // labels the engine can no longer reproduce.
        for (const editor of this.visibleEditorsFor(document)) this.render(editor);
      }),
      vscode.workspace.onDidCloseTextDocument((document) => this.forgetDocument(document)),
    );
  }

  dispose(): void {
    this.disposed = true;
    this.scheduleRender.cancel();
    this.scheduleLenses.cancel();
    for (const subscription of this.subscriptions) subscription.dispose();
    this.subscriptions.length = 0;
    this.annotationType.dispose();
    this.lineHighlightType.dispose();
    this.lensesChanged.dispose();
    this.cache.clear();
    this.inflight.clear();
    this.enabled.clear();
    this.highlighted.clear();
    this.targets.clear();
    this.warned.clear();
  }

  /** Drop every cached blame and re-render. Wired to the engine's
   *  worktreeChanged / indexChanged / headChanged notifications:
   *  annotations attributed to a superseded HEAD are worse than none. The
   *  generation bump makes in-flight responses die rather than land in the
   *  freshly cleared cache. */
  invalidate(): void {
    if (this.disposed) return;
    this.generation++;
    this.cache.clear();
    // In-flight responses are doomed by the generation bump, so they must
    // not stay registered as the key's pending fetch: a render attaching to
    // one would resolve with nothing and the document would stay
    // unannotated until some later event re-triggered it.
    this.inflight.clear();
    this.renderVisibleEditors();
    this.lensesChanged.fire();
  }

  /** Scroll re-slicing, throttled: a viewport-sliced file has to re-render
   *  DURING the gesture (a purely trailing debounce would not fire until
   *  the user pauses, leaving every newly visible line unannotated), but at
   *  most once per RENDER_DEBOUNCE_MS so a fast fling stays smooth. Files
   *  small enough to be decorated whole are skipped entirely — their
   *  decorations do not depend on the viewport. */
  private onScrolled(editor: vscode.TextEditor): void {
    if (this.disposed) return;
    if (
      !this.enabled.has(documentKey(editor.document)) ||
      editor.document.lineCount <= VIEWPORT_SLICE_LINES
    ) {
      return;
    }
    const now = Date.now();
    if (now - this.lastSliceRender < RENDER_DEBOUNCE_MS) {
      this.scheduleRender();
      return;
    }
    this.lastSliceRender = now;
    this.render(editor);
  }

  // ---------------------------------------------------------------------
  // Commands
  // ---------------------------------------------------------------------

  /** `gitBamboo.blame.toggle`: per-editor opt-in, OFF by default. */
  toggle(): void {
    const editor = vscode.window.activeTextEditor;
    if (!editor) {
      vscode.window.showInformationMessage('Git Bamboo: no active editor to annotate.');
      return;
    }
    const document = editor.document;
    const key = documentKey(document);
    if (this.enabled.has(key)) {
      this.enabled.delete(key);
      for (const shown of this.visibleEditorsFor(document)) {
        shown.setDecorations(this.annotationType, []);
      }
      this.lensesChanged.fire();
      return;
    }
    if (!this.blameTarget(document)) {
      // Output panels, webview documents, files outside the repository and
      // index/conflict-stage revisions have no commit attribution to show —
      // say so instead of toggling a state that renders nothing.
      vscode.window.showInformationMessage(
        'Git Bamboo: blame annotations are not available for this document.',
      );
      return;
    }
    this.enabled.add(key);
    // Immediate render: the toggle is an explicit user action, so waiting
    // out the debounce would read as a dead button.
    this.render(editor);
    this.lensesChanged.fire();
  }

  /** `gitBamboo.blame.showCommit` (hover link) and
   *  `gitBamboo.blame.jumpToCommit` (CodeLens): the same landing surface,
   *  two ids so the two entry points stay independently keybindable. The
   *  argument arrives from a Markdown command link / lens payload, i.e. a
   *  trust boundary — a non-sha is ignored, never forwarded. */
  showCommit(shaArg: unknown): void {
    if (typeof shaArg !== 'string' || !COMMIT_ID_RE.test(shaArg)) return;
    openCommitDetailPanel(this.context, this.client, this.root, shaArg);
  }

  /** `gitBamboo.blame.diffLine`: the introducing commit's diff for the
   *  blamed file — the commit-detail panel's "Open Changes" landing surface
   *  (parent↔commit side-by-side). Commit-level precision only this phase:
   *  the diff is not scrolled to the blamed line. */
  async openCommitDiff(shaArg: unknown, pathArg: unknown): Promise<void> {
    if (typeof shaArg !== 'string' || !COMMIT_ID_RE.test(shaArg)) return;
    if (typeof pathArg !== 'string' || !isValidRepoPath(pathArg)) return;
    const detail = await this.client.getCommitDetail(shaArg);
    if (!detail) {
      vscode.window.showErrorMessage(
        `Git Bamboo: commit details unavailable — ${shaArg.slice(0, 8)}`,
      );
      return;
    }
    const file = detail.files.find((entry) => entry.path === pathArg || entry.old_path === pathArg);
    const newPath = file?.path ?? pathArg;
    // The parent side lived at old_path when the commit renamed the file.
    const oldPath = file?.old_path ?? pathArg;
    const parent = detail.parent_ids[0];
    const leftRef =
      file?.status === 'A' || parent === undefined ? EMPTY_REVISION : parent;
    if (leftRef !== EMPTY_REVISION && !COMMIT_ID_RE.test(leftRef)) {
      vscode.window.showErrorMessage(
        `Git Bamboo: cannot resolve the parent of ${shaArg.slice(0, 8)}`,
      );
      return;
    }
    try {
      const left = commitRevisionUri(this.root, oldPath, leftRef);
      const right = commitRevisionUri(this.root, newPath, detail.id);
      const base = path.basename(newPath);
      const short = detail.id.slice(0, 8);
      const title =
        leftRef === EMPTY_REVISION
          ? `${base} (${short})`
          : `${base} (${short}^) ↔ ${base} (${short})`;
      await vscode.commands.executeCommand('vscode.diff', left, right, title);
    } catch (err) {
      vscode.window.showErrorMessage(`Git Bamboo: open changes failed — ${errorMessage(err)}`);
    }
  }

  /** `gitBamboo.blame.annotatePrevious {uri, line}`: open the parent
   *  revision of the commit that introduced the line, annotated, at (near)
   *  the same line. Re-running it on the opened document continues the
   *  chain — the opened document is opted into blame here, because the
   *  per-editor toggle state would otherwise drop the annotations after
   *  the first step. */
  async annotatePrevious(uriArg?: unknown, lineArg?: unknown): Promise<void> {
    const source = this.sourceEditor(uriArg);
    if (!source) {
      vscode.window.showInformationMessage('Git Bamboo: no editor to annotate backwards from.');
      return;
    }
    const document = source.document;
    const target = this.blameTarget(document);
    if (!target) {
      vscode.window.showInformationMessage(
        'Git Bamboo: blame annotations are not available for this document.',
      );
      return;
    }
    const requested =
      typeof lineArg === 'number' && Number.isFinite(lineArg)
        ? Math.max(0, Math.floor(lineArg))
        : source.selection.active.line;
    const result = await this.blameData(target);
    if (this.disposed || !result) return; // failure already surfaced (status bar)
    const line = Math.min(requested, Math.max(document.lineCount - 1, 0));
    const entry = blameEntryAtLine(result.entries, line);
    if (!entry) {
      vscode.window.showInformationMessage('Git Bamboo: no blame entry for this line.');
      return;
    }
    if (isUncommittedBlame(entry.commit)) {
      vscode.window.showInformationMessage(
        'Git Bamboo: this line is not committed yet — save or commit it first.',
      );
      return;
    }
    const detail = await this.client.getCommitDetail(entry.commit.id);
    if (!detail) {
      vscode.window.showErrorMessage(
        `Git Bamboo: commit details unavailable — ${entry.commit.id.slice(0, 8)}`,
      );
      return;
    }
    const parent = detail.parent_ids[0];
    if (parent === undefined || !COMMIT_ID_RE.test(parent)) {
      vscode.window.showInformationMessage(
        `Git Bamboo: ${entry.commit.id.slice(0, 8)} is a root commit — there is no earlier revision of this file.`,
      );
      return;
    }
    // Which path did the file have in the parent revision? A rename means
    // `source_path` (the engine's answer), but an entry cannot express a
    // rename happening AT the introducing commit, so the blamed path is the
    // second candidate. Blaming each candidate settles it: the winner is the
    // path the parent tree actually has, and losing everywhere means the
    // introducing commit created the file — the chain ends here, said out
    // loud instead of opening an empty document (headContent resolves a
    // missing blob to '' rather than failing).
    const parentPaths = [
      ...new Set(
        [entry.source_path, target.path].filter(
          (candidate): candidate is string =>
            typeof candidate === 'string' && isValidRepoPath(candidate),
        ),
      ),
    ];
    if (parentPaths.length === 0) {
      vscode.window.showWarningMessage('Git Bamboo: refusing untrusted blame path');
      return;
    }
    let parentPath: string | undefined;
    for (const candidate of parentPaths) {
      // No extra round trip: this is the cache key the opened document
      // renders from, so the pre-check doubles as its prefetch.
      const candidateBlame = await this.blameData({
        key: cacheKey(candidate, parent, null),
        path: candidate,
        rev: parent,
        contents: null,
      });
      if (this.disposed) return;
      if (candidateBlame && candidateBlame.entries.length > 0) {
        parentPath = candidate;
        break;
      }
    }
    if (!parentPath) {
      vscode.window.showInformationMessage(
        `Git Bamboo: ${entry.commit.id.slice(0, 8)} introduced this file — there is no earlier revision to annotate.`,
      );
      return;
    }
    try {
      // The revision travels in the URI query and the content provider only
      // serves 'HEAD' or a full sha (headContent.parseRevisionQuery rejects
      // everything else), hence the resolved parent sha rather than `I^`.
      const opened = await vscode.workspace.openTextDocument(
        commitRevisionUri(this.root, parentPath, parent),
      );
      const editor = await vscode.window.showTextDocument(opened, {
        viewColumn: source.viewColumn,
        preserveFocus: false,
      });
      // Line mapping is keep + clamp this phase (GitLens' simplification):
      // diff-based mapping is a later enhancement, so the preserved line is
      // an approximation whenever the parent revision is shorter.
      const preserved = Math.min(line, Math.max(opened.lineCount - 1, 0));
      const range = new vscode.Range(preserved, 0, preserved, 0);
      this.highlighted.set(documentKey(opened), preserved);
      editor.setDecorations(this.lineHighlightType, [range]);
      editor.selection = new vscode.Selection(preserved, 0, preserved, 0);
      editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
      this.enabled.add(documentKey(opened));
      this.render(editor);
      this.lensesChanged.fire();
    } catch (err) {
      vscode.window.showErrorMessage(
        `Git Bamboo: annotate previous revision failed — ${errorMessage(err)}`,
      );
    }
  }

  // ---------------------------------------------------------------------
  // Rendering
  // ---------------------------------------------------------------------

  private renderVisibleEditors(): void {
    if (this.disposed) return;
    for (const editor of vscode.window.visibleTextEditors) this.render(editor);
  }

  /** Paint one editor from the cache. Never throws into the user's face:
   *  a document with no blame target, no data, or a failed RPC simply gets
   *  its annotations cleared. */
  private render(editor: vscode.TextEditor): void {
    if (this.disposed) return;
    const document = editor.document;
    // Editors the user never opted in to are left alone — our decoration
    // type is private to this module, so this is not a shared surface.
    if (!this.enabled.has(documentKey(document))) return;
    const target = this.blameTarget(document);
    if (!target) {
      editor.setDecorations(this.annotationType, []);
      return;
    }
    const cached = this.cache.get(target.key);
    if (cached === undefined) {
      editor.setDecorations(this.annotationType, []);
      void this.blameData(target); // re-renders when it lands
      return;
    }
    if (isExpired(cached)) {
      // Stale-while-revalidate: blanking on every TTL expiry makes scrolling
      // flicker, and invalidate() already clears the cache the moment the
      // engine reports a HEAD/worktree move.
      void this.blameData(target);
    }
    if (cached.result === undefined) {
      editor.setDecorations(this.annotationType, []);
      return;
    }
    const styles = cached.render ?? buildRender(cached.result);
    cached.render = styles;
    const lineCount = document.lineCount;
    const slice = lineCount > VIEWPORT_SLICE_LINES;
    const visible = editor.visibleRanges;
    const decorations: vscode.DecorationOptions[] = [];
    for (const entry of cached.result.entries) {
      // Defensive normalization: a broken engine must not be able to push a
      // negative line into a Range (the validator only checks the type).
      const start = Math.max(0, Math.trunc(entry.start_line));
      // Entries ascend and tile the file, so the first one past the buffer
      // ends the loop (an unsaved buffer can be shorter than the blamed
      // revision's file).
      if (start >= lineCount) break;
      const end = Math.min(start + Math.max(Math.trunc(entry.len), 1) - 1, lineCount - 1);
      if (slice && !intersectsVisible(visible, start, end)) continue;
      const options = annotationOptions(styles, entry.commit);
      for (let line = start; line <= end; line++) {
        decorations.push({
          range: new vscode.Range(line, 0, line, 0),
          renderOptions: { before: options },
        });
      }
    }
    editor.setDecorations(this.annotationType, decorations);
  }

  // ---------------------------------------------------------------------
  // Hover + CodeLens
  // ---------------------------------------------------------------------

  private async provideHover(
    document: vscode.TextDocument,
    position: vscode.Position,
  ): Promise<vscode.Hover | undefined> {
    if (!this.enabled.has(documentKey(document))) return undefined;
    const target = this.blameTarget(document);
    if (!target) return undefined;
    const version = document.version;
    const result = await this.blameData(target);
    // Drop the hover when the buffer moved under us: attributing a stale
    // line to a shifted position is worse than no hover.
    if (this.disposed || !result || version !== document.version) return undefined;
    if (position.line >= document.lineCount) return undefined;
    const entry = blameEntryAtLine(result.entries, position.line);
    if (!entry) return undefined;
    return new vscode.Hover(this.hoverMarkdown(target, entry), document.lineAt(position.line).range);
  }

  private async provideLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    if (!this.enabled.has(documentKey(document))) return [];
    const editor = this.editorFor(document);
    if (!editor) return [];
    const target = this.blameTarget(document);
    if (!target) return [];
    const version = document.version;
    const result = await this.blameData(target);
    if (this.disposed || !result || version !== document.version) return [];
    // Re-read the cursor: it may have moved during the RPC, and a lens on
    // the line the user left is exactly the stale-target bug the stateless
    // rule exists to prevent.
    const line = editor.selection.active.line;
    if (line >= document.lineCount) return [];
    const entry = blameEntryAtLine(result.entries, line);
    if (!entry || isUncommittedBlame(entry.commit)) return [];
    const at = new vscode.Range(line, 0, line, 0);
    return [
      new vscode.CodeLens(at, {
        title: JUMP_LENS_TITLE,
        command: BLAME_JUMP_TO_COMMIT_COMMAND,
        arguments: [entry.commit.id],
      }),
      new vscode.CodeLens(at, {
        title: PREVIOUS_LENS_TITLE,
        command: BLAME_ANNOTATE_PREVIOUS_COMMAND,
        arguments: [document.uri.toString(true), line],
      }),
    ];
  }

  /** The hover card for one line's blame entry. Trusted Markdown (it carries
   *  command links); every argument embedded in a link is host-derived and
   *  re-validated at the command boundary, and repository strings (author,
   *  subject, renamed path) go through appendText so untrusted content can
   *  neither forge a command URI nor break the layout. */
  private hoverMarkdown(target: BlameTarget, entry: BlameEntry): vscode.MarkdownString {
    const md = new vscode.MarkdownString(undefined, true);
    md.isTrusted = true;
    const commit = entry.commit;
    if (isUncommittedBlame(commit)) {
      md.appendMarkdown(`**${NOT_COMMITTED_LABEL}**\n\n`);
      md.appendText(commit.summary);
      return md;
    }
    const author = commit.author_name.trim() || commit.author_email;
    md.appendMarkdown('$(account) **');
    md.appendText(author);
    md.appendMarkdown('**');
    if (commit.author_email.length > 0) {
      md.appendMarkdown(' · ');
      md.appendText(commit.author_email);
    }
    const when = new Date(commit.author_time * 1000).toLocaleString();
    md.appendMarkdown(`\n\n$(calendar) ${when} (${relativeTime(commit.author_time)})\n\n`);
    md.appendMarkdown(`$(git-commit) \`${commit.id.slice(0, 8)}\``);
    if (commit.summary.length > 0) {
      md.appendMarkdown(' · ');
      md.appendText(commit.summary);
    }
    if (entry.source_path && entry.source_path !== target.path) {
      md.appendMarkdown('\n\n$(file) Renamed at this commit from ');
      md.appendText(entry.source_path);
    }
    md.appendMarkdown(
      `\n\n[Open Commit](${commandLink(BLAME_SHOW_COMMIT_COMMAND, commit.id)})` +
        ` · [Open Changes](${commandLink(BLAME_DIFF_LINE_COMMAND, commit.id, target.path)})`,
    );
    return md;
  }

  // ---------------------------------------------------------------------
  // Data
  // ---------------------------------------------------------------------

  /** Blame for `target`, from cache or one shared in-flight RPC. Resolves
   *  undefined when the RPC failed (already reported) or was superseded by
   *  an invalidation. */
  private blameData(target: BlameTarget): Promise<GetBlameResult | undefined> {
    const cached = this.cache.get(target.key);
    if (cached !== undefined && !isExpired(cached)) return Promise.resolve(cached.result);
    const pending = this.inflight.get(target.key);
    if (pending) return pending;
    const promise: Promise<GetBlameResult | undefined> = this.fetch(target).finally(() => {
      // Identity check: an invalidation may have started a newer fetch for
      // the same key, whose entry this late `finally` must not delete.
      if (this.inflight.get(target.key) === promise) this.inflight.delete(target.key);
    });
    this.inflight.set(target.key, promise);
    return promise;
  }

  private async fetch(target: BlameTarget): Promise<GetBlameResult | undefined> {
    const generation = this.generation;
    try {
      const result = await this.client.getBlame(target.path, target.rev, target.contents);
      if (this.disposed || generation !== this.generation) return undefined;
      this.store(target.key, { result, fetchedAt: Date.now(), negative: false });
      this.renderKey(target.key);
      return result;
    } catch (err) {
      if (this.disposed || generation !== this.generation) return undefined;
      console.error(`[git-bamboo] getBlame ${target.path} failed:`, err);
      this.store(target.key, { result: undefined, fetchedAt: Date.now(), negative: true });
      this.renderKey(target.key);
      this.notifyOnce(
        target.path,
        `blame unavailable for ${target.path} — ${errorMessage(err)}`,
      );
      return undefined;
    }
  }

  private store(key: string, entry: CachedBlame): void {
    this.cache.set(key, entry);
    // Map iteration order is insertion order → evict the oldest.
    while (this.cache.size > MAX_CACHE_ENTRIES) {
      const oldest = this.cache.keys().next();
      if (oldest.done) return;
      this.cache.delete(oldest.value);
    }
  }

  /** Re-render the visible editors whose document currently answers to
   *  `key` (a fetch landing must not paint over an editor that has since
   *  moved to another buffer version). */
  private renderKey(key: string): void {
    if (this.disposed) return;
    for (const editor of vscode.window.visibleTextEditors) {
      if (!this.enabled.has(documentKey(editor.document))) continue;
      if (this.blameTarget(editor.document)?.key === key) this.render(editor);
    }
  }

  /** One non-blocking notice per path per session: an untracked/binary file,
   *  a dead engine, or a buffer too large to blame unsaved must not spam the
   *  user on every retry or scroll tick. */
  private notifyOnce(notifyPath: string, message: string): void {
    if (this.warned.has(notifyPath)) return;
    this.warned.add(notifyPath);
    vscode.window.setStatusBarMessage(`Git Bamboo: ${message}`, 5_000);
  }

  /** What (if anything) this document can be blamed as, memoized per buffer
   *  state: deriving it reads the WHOLE dirty buffer, and render, hover and
   *  lenses each ask on every event — recomputing per scroll tick would copy
   *  megabytes for nothing. Saving flips isDirty without bumping the
   *  version, so both are part of the memo identity. */
  private blameTarget(document: vscode.TextDocument): BlameTarget | undefined {
    const key = documentKey(document);
    const memo = this.targets.get(key);
    if (memo && memo.version === document.version && memo.dirty === document.isDirty) {
      return memo.target;
    }
    const target = this.deriveTarget(document);
    this.targets.set(key, { version: document.version, dirty: document.isDirty, target });
    return target;
  }

  /** Returns undefined for documents outside the two supported schemes,
   *  outside the repository, and for engine-scheme revisions that are not
   *  commits (the `:0`-`:3` index/conflict stages and the synthetic empty
   *  side). */
  private deriveTarget(document: vscode.TextDocument): BlameTarget | undefined {
    const uri = document.uri;
    if (uri.scheme !== 'file' && uri.scheme !== ENGINE_SCHEME) return undefined;
    const relative = path.relative(this.root.fsPath, uri.fsPath).replace(/\\/g, '/');
    // The provider-side validation, applied on the way in: a URI crafted to
    // escape the repository never reaches the engine.
    if (!isValidRepoPath(relative)) return undefined;
    let rev: string | null = null;
    let contents: string | null = null;
    if (uri.scheme === ENGINE_SCHEME) {
      // The revision travels in the query (headContent's URI shape). 'HEAD'
      // is sent EXPLICITLY rather than as null: null means "blame the
      // worktree file" to the engine, and this document shows HEAD's blob —
      // with a dirty worktree the two differ, and annotations keyed to
      // worktree line numbers would slide against the content on screen.
      if (uri.query === 'HEAD') rev = 'HEAD';
      else if (COMMIT_ID_RE.test(uri.query)) rev = uri.query;
      else return undefined;
    } else if (document.isDirty) {
      const text = document.getText();
      if (Buffer.byteLength(text, 'utf8') <= MAX_DIRTY_CONTENTS_BYTES) {
        contents = text;
      } else {
        // Blame the saved file instead: stale for the unsaved edits but
        // still attributed, where an oversized payload would at best be
        // rejected in band and at worst kill the engine session.
        this.notifyOnce(
          relative,
          `buffer too large to blame unsaved (${relative}) — annotating the saved file`,
        );
      }
    }
    const key = cacheKey(relative, rev, contents === null ? null : document.version);
    return { key, path: relative, rev, contents };
  }

  private visibleEditorsFor(document: vscode.TextDocument): vscode.TextEditor[] {
    return vscode.window.visibleTextEditors.filter((editor) => editor.document === document);
  }

  /** The editor a CodeLens belongs to: the active one when it shows this
   *  document (a split can show it twice), else the first that does. */
  private editorFor(document: vscode.TextDocument): vscode.TextEditor | undefined {
    const active = vscode.window.activeTextEditor;
    if (active?.document === document) return active;
    return this.visibleEditorsFor(document)[0];
  }

  /** The editor an annotatePrevious walks back from: the lens argument's
   *  document when it is open, else the active editor (keybinding /
   *  programmatic invocation). */
  private sourceEditor(uriArg: unknown): vscode.TextEditor | undefined {
    if (typeof uriArg === 'string' && uriArg.length > 0) {
      let uri: vscode.Uri | undefined;
      try {
        uri = vscode.Uri.parse(uriArg, true);
      } catch {
        uri = undefined; // malformed lens payload: fall back to the active editor
      }
      if (uri) {
        const wanted = uri.toString(true);
        const editor = vscode.window.visibleTextEditors.find(
          (candidate) => candidate.document.uri.toString(true) === wanted,
        );
        if (editor) return editor;
      }
    }
    return vscode.window.activeTextEditor;
  }

  private clearLineHighlight(editor: vscode.TextEditor): void {
    const key = documentKey(editor.document);
    const line = this.highlighted.get(key);
    if (line === undefined) return;
    // Kept while the cursor sits on the highlighted line: annotatePrevious
    // moves the cursor there programmatically, which fires this handler —
    // clearing unconditionally would flash the highlight away at once.
    if (editor.selection.active.line === line) return;
    this.highlighted.delete(key);
    editor.setDecorations(this.lineHighlightType, []);
  }

  /** Drop the toggle state and cached blame of a closed document (its
   *  dirty-version keys would otherwise survive until eviction). */
  private forgetDocument(document: vscode.TextDocument): void {
    const key = documentKey(document);
    this.enabled.delete(key);
    this.highlighted.delete(key);
    this.targets.delete(key);
    // deriveTarget, not blameTarget: the memo was just dropped and a closed
    // document must not re-populate it.
    const target = this.deriveTarget(document);
    if (!target) return;
    this.warned.delete(target.path);
    const prefix = `${target.path}\u0000`;
    for (const stale of this.cache.keys()) {
      if (stale.startsWith(prefix)) this.cache.delete(stale);
    }
  }
}

// -----------------------------------------------------------------------
// Pure helpers
// -----------------------------------------------------------------------

/** The entry covering a 0-based line, or undefined. Entries ascend and tile
 *  the file, so a binary search is exact — the hover/lens path runs per
 *  keystroke-driven recompute and must not scan a large file's entries. */
function blameEntryAtLine(
  entries: readonly BlameEntry[],
  line: number,
): BlameEntry | undefined {
  let low = 0;
  let high = entries.length - 1;
  while (low <= high) {
    const mid = (low + high) >> 1;
    const entry = entries[mid];
    const start = Math.max(0, entry.start_line);
    const len = Math.max(entry.len, 1);
    if (line < start) high = mid - 1;
    else if (line >= start + len) low = mid + 1;
    else return entry;
  }
  return undefined;
}

function intersectsVisible(
  visible: readonly vscode.Range[],
  startLine: number,
  endLine: number,
): boolean {
  return visible.some((range) => range.start.line <= endLine && range.end.line >= startLine);
}

/** Build the per-commit annotation texts and the document-wide column
 *  width from one pass over the entries. Done at fetch time (not per
 *  render) so viewport slicing cannot change the width while scrolling —
 *  a width change would shift the code horizontally on every scroll tick. */
function buildRender(result: GetBlameResult): BlameRender {
  const text = new Map<string, string>();
  let widthCells = 0;
  for (const entry of result.entries) {
    const id = entry.commit.id;
    if (text.has(id)) continue;
    const label = annotationText(entry.commit);
    text.set(id, label);
    widthCells = Math.max(widthCells, displayWidthCells(label));
  }
  return { widthCells: widthCells + ANNOTATION_PADDING_CELLS, text, options: new Map() };
}

/** Per-commit attachment options (the renderOptions cache): one style
 *  object reused by every line of that commit. */
function annotationOptions(
  styles: BlameRender,
  commit: BlameCommit,
): vscode.ThemableDecorationAttachmentRenderOptions {
  const cached = styles.options.get(commit.id);
  if (cached) return cached;
  const options: vscode.ThemableDecorationAttachmentRenderOptions = {
    contentText: padToWidth(styles.text.get(commit.id) ?? NOT_COMMITTED_LABEL, styles.widthCells),
    // Colour and style are repeated on every instance rather than relying
    // on the decoration type's defaults: the instance attachment is what
    // VS Code styles, and depending on how much of the type's `before` it
    // merges would risk un-dimmed annotations competing with the code.
    color: new vscode.ThemeColor(ANNOTATION_COLOR),
    fontStyle: 'italic',
    margin: ANNOTATION_MARGIN,
    width: `${styles.widthCells}ch`,
  };
  styles.options.set(commit.id, options);
  return options;
}

/** Pad an annotation to the document-wide column width. Space padding — not
 *  only the CSS `width` above — is what actually aligns the code: a `before`
 *  attachment is an inline box whose `width` is honoured only if the host
 *  lays it out as inline-block, whereas Monaco renders line content with
 *  `white-space: pre` and so preserves the padding either way. */
function padToWidth(text: string, widthCells: number): string {
  const missing = widthCells - displayWidthCells(text);
  return missing > 0 ? text + ' '.repeat(missing) : text;
}

/** `author, <short relative time>`; the uncommitted placeholder has no time
 *  (the engine reports both times as 0 for it). */
function annotationText(commit: BlameCommit): string {
  if (isUncommittedBlame(commit)) return NOT_COMMITTED_LABEL;
  const author =
    commit.author_name.trim() || commit.author_email.split('@')[0].trim() || 'Unknown';
  return truncateToWidth(
    `${author}, ${relativeTime(commit.author_time)}`,
    MAX_ANNOTATION_WIDTH_CELLS,
  );
}

/** Command URI for a trusted Markdown link: the argument list is JSON-
 *  encoded into the query and URI-encoded, so no embedded value can
 *  terminate the link or add an argument. */
function commandLink(command: string, ...args: unknown[]): string {
  return `command:${command}?${encodeURIComponent(JSON.stringify(args))}`;
}

/** Display width in monospace cells; double-width glyphs (CJK, emoji) count
 *  as two so a wide author name cannot overflow the reserved column into
 *  the code. */
function displayWidthCells(text: string): number {
  let width = 0;
  for (const char of text) width += charWidthCells(char);
  return width;
}

/** Width-truncate with an ellipsis so every annotation fits the shared
 *  column (BlameRender.widthCells). */
function truncateToWidth(text: string, maxCells: number): string {
  if (displayWidthCells(text) <= maxCells) return text;
  let width = 0;
  let truncated = '';
  for (const char of text) {
    const charWidth = charWidthCells(char);
    if (width + charWidth > maxCells - 1) break; // room for the ellipsis
    truncated += char;
    width += charWidth;
  }
  return `${truncated}\u2026`;
}

function charWidthCells(char: string): number {
  return isWideChar(char.codePointAt(0) ?? 0) ? 2 : 1;
}

/** wcwidth-style double-width ranges: the annotation column is sized in
 *  `ch`, which only holds if wide glyphs are counted as two cells. */
function isWideChar(code: number): boolean {
  return (
    (code >= 0x1100 && code <= 0x115f) || // Hangul Jamo
    (code >= 0x2e80 && code <= 0x303e) || // CJK radicals, Kangxi, CJK symbols
    (code >= 0x3041 && code <= 0x33ff) || // Kana, enclosed CJK
    (code >= 0x3400 && code <= 0x4dbf) || // CJK extension A
    (code >= 0x4e00 && code <= 0x9fff) || // CJK unified
    (code >= 0xa000 && code <= 0xa4cf) || // Yi
    (code >= 0xac00 && code <= 0xd7a3) || // Hangul syllables
    (code >= 0xf900 && code <= 0xfaff) || // CJK compatibility
    (code >= 0xfe30 && code <= 0xfe6f) || // CJK compatibility forms
    (code >= 0xff00 && code <= 0xff60) || // fullwidth forms
    (code >= 0xffe0 && code <= 0xffe6) ||
    code >= 0x1f300 // emoji and supplementary planes
  );
}
