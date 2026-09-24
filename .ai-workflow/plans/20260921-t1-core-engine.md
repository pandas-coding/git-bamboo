---
title: T1 Core Engine — Rust Daemon + VS Code Extension Skeleton + Status/Graph/Undo Foundation
date: 2026-09-21
status: in-progress
ideas:
  - .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md
group: git-workbench
phase: 1
tags: [git, rust, vscode-extension, daemon, undo, t1]
---

# T1 Core Engine — Rust Daemon + VS Code Extension Skeleton + Status/Graph/Undo Foundation

## Goal

Deliver a working VS Code extension backed by a Rust daemon that can open a git repository, display a virtual-scrolled commit graph with 60fps performance, show millisecond-level status, and perform basic git operations (stage/unstage, amend, branch CRUD, fetch/pull/push) with an undo engine supporting one-click rollback. This phase establishes the entire architecture stack: Rust engine (gix read + CLI write), JSON-RPC protocol, redb persistent cache, epoch invalidation, fs watcher, and VS Code frontend (webview Canvas graph + extension host SCM integration).

## Background

See [idea file](../ideas/20260921-rust-vscode-git-workbench.md) for full context. Key constraints from research:

- **Hybrid read/write**: gix for read paths (log/graph/status/diff/blame), git CLI subprocess for write paths (rebase/merge/push are out of scope in T1 except basic commit/push/fetch).
- **Epoch mechanism**: gix caches are tied to an epoch counter bumped on any CLI write or fs event; prevents stale reads after external git operations.
- **Persistent cache**: redb stores commit metadata + pre-computed graph lanes; cold start <1s target depends on it.
- **Protocol**: JSON-RPC 2.0 over stdio via `tower-lsp`; schema kept generic for future MCP/Zed compatibility.
- **Distribution**: platform-specific VSIX bundling pre-compiled Rust binaries.

## Research Summary

- **Greenfield project**: No existing code, build config, or scaffolding.
- **gix 0.87+**: `Repository::status()` and `rev_walk()` provide read APIs; no built-in graph lane calculation (must build our own on top of `Walk` + `commitgraph::Graph`); `Platform` is `!Send + !Sync` (must isolate in single thread or use channels).
- **redb**: Pure Rust embedded KV, ~7.6x faster writes than SQLite, MVCC read concurrency, single writer requirement.
- **tower-lsp**: Mature JSON-RPC 2.0 over stdio with automatic `Content-Length` framing, request routing, and concurrency control.
- **watchexec**: Cross-platform fs watching via `notify` backend; inotify limits may require fallback to polling for huge repos.
- **VS Code platform-specific VSIX**: Supported since `vsce` 1.99.0; rust-analyzer bundles `server/rust-analyzer${ext}` per target.

## Steps

### 1. Project Scaffolding & Workspace Structure

Create the monorepo layout with three top-level directories and CI configuration.

**Create `Cargo.toml` (workspace root)**:
- Define workspace members: `crates/engine`, `crates/protocol`, `crates/gix-adapt`
- Set resolver = "2", edition = "2021", rust-version = "1.78"
- Shared dependencies in `[workspace.dependencies]`: `gix = "0.87"`, `redb = "2"`, `tokio = "1"`, `serde = "1"`, `tracing = "0.1"`, `tower-lsp = "0.20"`, `watchexec = "8.4"`

**Create `package.json` (VS Code extension root at `vscode-extension/`)**:
- Name: `git-workbench`
- Engines: `vscode ^1.90.0`
- Activation events: `onStartupFinished` (or `onCommand:gitWorkbench.openGraph` for T1)
- Contributes: `scm.provider` registration, commands (`gitWorkbench.openGraph`, `gitWorkbench.undo`, `gitWorkbench.switchBranch`), custom view container for graph webview
- Scripts: `compile` (`vite build`), `watch` (`vite build --watch`), `package`, `package-all` (matrix build script); devDependencies: `vite@^8`, `@vscode/vsce`, `typescript`

**Create `.github/workflows/ci.yml`**:
- Matrix: `ubuntu-latest`, `macos-latest`, `windows-latest` (x64); plus `macos-14` for arm64, `ubuntu-24.04-arm` for linux-arm64 if available
- Steps: checkout → install Rust stable → `cargo test --workspace` → `cargo build --release` → `vsce package --target ${{ matrix.vscode_target }}`
- Artifact upload of `.vsix` and `target/release/git-workbench-engine${ext}`

**Create `vscode-extension/.vscodeignore`**:
- Exclude `src/`, `node_modules/`, `*.map` from VSIX; only include `out/`, `server/`, `package.json`, `README.md`, `LICENSE`

### 2. Protocol Crate (`crates/protocol/`)

Define the JSON-RPC schema shared between Rust engine and TypeScript frontend. This crate contains only types and serialization logic, no I/O.

**Create `crates/protocol/src/lib.rs`**:
- Define core types as `serde::Deserialize/Serialize`:
  - `Commit { id: String, message: String, author_name: String, author_email: String, author_time: i64, parent_ids: Vec<String>, lane: u8 }`
  - `Ref { name: String, kind: RefKind, target: String }` where `RefKind` = Branch/Tag/RemoteBranch
  - `StatusItem { path: String, status: StatusCode, old_path: Option<String> }`
  - `GraphViewport { offset: u64, limit: u32, anchor_commit: Option<String> }`
  - `GraphPage { commits: Vec<Commit>, total_approx: u64, has_more: bool, epoch: u64, anchor_commit: Option<String> }`
- Define JSON-RPC method enums:
  - `Initialize { repo_path: String } → SessionId`
  - `GetGraph { viewport: GraphViewport } → GraphPage`
  - `GetStatus { paths: Option<Vec<String>> } → Vec<StatusItem>`
  - `Stage { paths: Vec<String> } → ()` (write path → CLI)
  - `Unstage { paths: Vec<String> } → ()` (write path → CLI)
  - `Commit { message: String, amend: bool } → CommitResult`
  - `CreateBranch { name: String, base: Option<String> } → ()`
  - `DeleteBranch { name: String, force: bool } → ()`
  - `SwitchBranch { name: String, auto_stash: bool } → ()`
  - `Fetch { remote: Option<String> } → ()`
  - `Pull { remote: Option<String>, branch: Option<String> } → ()`
  - `Push { remote: String, branch: String, force: bool } → ()`
  - `Undo { transaction_id: u64 } → UndoResult`
  - `ListUndoStack { limit: u32 } → Vec<UndoEntry>`
- Define notification types (server → client):
  - `RefsChanged { changed_refs: Vec<String> }`
  - `WorktreeChanged { paths: Vec<String> }`
  - `IndexChanged`
  - `HeadChanged { new_head: String }`
  - `GraphInvalidated { epoch: u64 }`
- Define error type: `WorkbenchError { code: i32, message: String }` with codes for RepoNotFound, GitError, ConcurrentOperation, EpochMismatch, etc.

**Create `crates/protocol/Cargo.toml`**:
- Only dependencies: `serde`, `serde_json`

### 3. Engine Core — Repository Session & Epoch (`crates/engine/src/session.rs`)

The central state holder for an opened repository. Must be `Send` to live in a Tokio task.

**Create `crates/engine/src/session.rs`**:
- `struct Session { repo_path: PathBuf, epoch: AtomicU64, gix_repo: Mutex<gix::Repository>, cache: redb::Database, command_queue: mpsc::Sender<WriteCommand>, fs_watcher: Option<watchexec::Watchexec> }`
- `impl Session`:
  - `async fn open(repo_path: &Path) -> Result<Session, Error>`: open gix repo, initialize or open redb at `.git/git-workbench/cache.redb`, spawn fs watcher, start command queue consumer
  - `fn bump_epoch(&self) -> u64`: atomic increment + return new value; called after any write operation or fs event
  - `fn current_epoch(&self) -> u64`: atomic read
  - `fn gix(&self) -> MutexGuard<gix::Repository>`: lock gix repo for read operations
  - `async fn enqueue_write(&self, cmd: WriteCommand) -> Result<Output, Error>`: send to command queue, await result
- **Epoch invalidation logic**: on fs event from `.git/index`, `.git/refs/`, `.git/HEAD`, `.git/sequencer/`, call `bump_epoch()` and broadcast `GraphInvalidated` / `IndexChanged` / `HeadChanged` notifications to all connected clients

**Create `crates/engine/src/write_queue.rs`**:
- `enum WriteCommand { Stage(Vec<String>), Unstage(Vec<String>), Commit { message: String, amend: bool }, ... }`
- `async fn run_command_queue(receiver: mpsc::Receiver<WriteCommand>, session: Arc<Session>)`: single consumer loop ensuring only one git CLI operation runs at a time per repo
- Before executing any command: capture undo snapshot (see Step 6)
- After executing: parse output, bump epoch, send notifications

### 4. Engine Core — Gix Read Paths (`crates/engine/src/gix_read.rs`)

Wrap gix APIs with epoch validation and cache integration.

**Create `crates/engine/src/gix_read.rs`**:
- `fn read_graph_page(session: &Session, viewport: &GraphViewport) -> Result<GraphPage, Error>`:
  - Check viewport.epoch against session.current_epoch(); if mismatch return `EpochMismatch` error
  - Use `session.gix().rev_walk([tips])` where tips = all ref targets
  - Apply sorting (`Sorting::ByCommitTimeNewestFirst`), use commit graph acceleration (`use_commit_graph(true)`)
  - Skip `viewport.offset`, take `viewport.limit + 500` (preload margin) for local caching
  - **Lane calculation**: for each commit in the walked range, compute lane assignment using a sweep-line algorithm tracking active branches:
    - Maintain `Vec<Option<String>> lanes` where each slot holds the commit ID of the branch tip currently occupying that lane
    - For each commit (in topological order from newest), assign the lowest free lane; if commit has multiple children (merge), occupy a new lane; if commit closes a branch (no remaining descendants), free the lane
    - Store `(commit_id, lane, epoch)` in redb table `COMMIT_LANES`
  - Return `GraphPage` with `epoch` set to current epoch
- `fn read_status(session: &Session, paths: Option<&[String]>) -> Result<Vec<StatusItem>, Error>`:
  - Use `session.gix().status(None)?` → `Platform`
  - If paths provided, filter via `into_iter(Some(patterns))`; otherwise full walk
  - Map `status::Item` to `StatusItem`; resolve renames via `index_worktree_rewrites()`
  - Target: <100ms for 50k files (gix scoped walk)
- `fn read_refs(session: &Session) -> Result<Vec<Ref>, Error>`:
  - Iterate `session.gix().references()?.all()`
  - Classify into local branches, remote branches, tags

### 5. Engine Core — Persistent Cache (`crates/engine/src/cache.rs`)

redb integration for commit metadata and graph lanes.

**Create `crates/engine/src/cache.rs`**:
- `const COMMIT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("commit_meta")`
- `const COMMIT_LANES: TableDefinition<&str, u8> = TableDefinition::new("commit_lanes")`
- `const CACHE_EPOCH: TableDefinition<&str, u64> = TableDefinition::new("cache_epoch")`
- `struct Cache { db: redb::Database, repo_path: PathBuf }`
- `impl Cache`:
  - `fn open(path: &Path) -> Result<Cache, Error>`: open or create redb file at `.git/git-workbench/cache.redb`; create tables if missing
  - `fn get_commit_meta(&self, id: &str) -> Result<Option<Commit>, Error>`: read from `COMMIT_META`, deserialize via `bincode` or `serde_json`
  - `fn put_commit_meta(&self, id: &str, commit: &Commit) -> Result<(), Error>`: write under write transaction (redb allows only one writer — serialize via `command_queue` or a dedicated cache write queue)
  - `fn get_lane(&self, id: &str) -> Result<Option<u8>, Error>`
  - `fn put_lane(&self, id: &str, lane: u8, epoch: u64) -> Result<(), Error>`: also store epoch for validation
  - `fn get_cached_epoch(&self) -> Result<Option<u64>, Error>`: stored epoch from last cache write
  - `fn invalidate_on_epoch_change(&self, current_epoch: u64) -> Result<bool, Error>`: if `get_cached_epoch()` != current_epoch, clear `COMMIT_LANES` table and return true (indicating cold rebuild needed)
- **Cache warming strategy**: on daemon start, if cache epoch matches repo epoch, cache is warm; if not, first `read_graph_page` call triggers background warm-up of first N (e.g., 10k) commits

### 6. Engine Core — Undo Engine (`crates/engine/src/undo.rs`)

Transaction snapshots supporting one-click rollback. Critical foundation for future LLM agent safety.

**Create `crates/engine/src/undo.rs`**:
- `struct UndoEngine { journal_path: PathBuf, next_id: AtomicU64 }`
- `struct Transaction { id: u64, timestamp: i64, description: String, epoch_at_creation: u64, snapshot: Snapshot }`
- `struct Snapshot { head: String, refs: HashMap<String, String>, has_stash: bool, stash_ref: Option<String> }`
- `impl UndoEngine`:
  - `fn new(repo_path: &Path) -> UndoEngine`: ensure `.git/git-workbench/undo/` exists; load existing journal
  - `fn capture_snapshot(&self, description: &str, epoch: u64) -> Result<u64, Error>`:
    - Read current `HEAD` ref target
    - Read all local ref SHAs from `.git/refs/`
    - Check if stash exists (`refs/stash`)
    - Serialize `Snapshot` to `.git/git-workbench/undo/{id}.snapshot`
    - Append `Transaction` to `.git/git-workbench/undo/journal.redb` (or append-only log file)
    - Return transaction id
  - `fn undo(&self, transaction_id: u64) -> Result<(), Error>`:
    - Read transaction from journal
    - Verify current epoch == transaction.epoch_at_creation OR current epoch has only changed via recoverable refs; if external changes detected, return `UndoBlocked { reason }`
    - For each ref in snapshot: `git update-ref {name} {target}` (via CLI queue)
    - Restore HEAD: `git symbolic-ref HEAD {snapshot.head}` or `git checkout {sha} --`
    - If `has_stash` and stash was created by this transaction, pop it; otherwise warn
  - `fn list_transactions(&self, limit: u32) -> Result<Vec<UndoEntry>, Error>`: read journal newest-first
- **Audit logging**: every write operation (from CLI queue) appends to `.git/git-workbench/audit.log` with `{timestamp, operation, args, result, transaction_id}`

### 7. Engine Core — JSON-RPC Server (`crates/engine/src/server.rs`)

tower-lsp based server wiring protocol types to engine operations.

**Create `crates/engine/src/server.rs`**:
- `struct WorkbenchService { session: Arc<Session>, client: Client }` (implements `tower_lsp::LanguageServer` trait as transport shim)
- Map LSP `initialize` → `Initialize` request: create `Session`, store in `Arc<Mutex<HashMap<SessionId, Arc<Session>>>>`
- Map each protocol method to engine call:
  - `GetGraph` → `gix_read::read_graph_page`
  - `GetStatus` → `gix_read::read_status`
  - `Stage`/`Unstage`/`Commit`/`Push`/`Pull`/... → `session.enqueue_write()` + capture undo snapshot before execution
  - `Undo` → `undo_engine.undo()`
- Notification broadcasting: when epoch bumps or fs events fire, call `client.send_notification::<RefsChanged>(...)` to all connected clients
- Error mapping: `WorkbenchError` → LSP JSON-RPC error response with custom code

### 8. Engine Binary (`crates/engine/src/main.rs`)

Thin main that starts the tower-lsp server over stdio.

**Create `crates/engine/src/main.rs`**:
- `#[tokio::main] async fn main()`: init `tracing_subscriber`, parse optional `--repo-path` arg, create `stdio` transport, start `tower_lsp::Server::new(stdin, stdout, socket).serve(service)`
- On `stdin` close or parent process death (poll parent PID if provided via `--parent-pid`), graceful shutdown: drop sessions, flush redb, exit

### 9. VS Code Extension — Extension Host (`vscode-extension/src/extension.ts`)

TypeScript entry point, engine process lifecycle, SCM provider registration.

**Create `vscode-extension/src/extension.ts`**:
- `export function activate(context: vscode.ExtensionContext)`:
  - Register `scm.createSourceControl('git-workbench', 'Git Workbench', workspaceFolder)` for each workspace folder containing `.git`
  - Spawn Rust engine binary: resolve path via `context.asAbsolutePath(`./server/git-workbench-engine${platformExt}`)`, fallback to bundled vsix path
  - Connect via `stdio` pipe: `child_process.spawn(binaryPath, ['--parent-pid', process.pid.toString()])`
  - Send `Initialize` JSON-RPC request with `repo_path`
  - Set up notification handlers: on `RefsChanged`/`WorktreeChanged`/`IndexChanged`/`HeadChanged` → refresh SCM resource groups; on `GraphInvalidated` → notify webview
  - Register commands: `gitWorkbench.openGraph`, `gitWorkbench.undo`, `gitWorkbench.switchBranch`
- `export function deactivate()`: send shutdown request, kill child process, cleanup

### 10. VS Code Extension — Graph Webview (`vscode-extension/src/graphWebview.ts` + `vscode-extension/webview/graph.html` + `vscode-extension/webview/graph.js`)

Virtual-scrolled commit graph using DOM rows + Canvas overlay for branch lines.

**Create `vscode-extension/src/graphWebview.ts`**:
- `class GraphWebviewProvider implements vscode.WebviewViewProvider`:
  - `resolveWebviewView(panel)`: set HTML content from `graph.html`, set up message passing
  - On visibility change: send `GetGraph` request with current viewport
  - On scroll event from webview: debounce 16ms, send new viewport request
  - On `GraphInvalidated` notification: send `listInvalidated` to webview with anchor commit preserved
  - Pre-fetch: maintain cache of last 2 viewports worth of commits in webview

**Create `vscode-extension/webview/graph.html`**:
- Minimal HTML: container div, one Canvas element (absolute positioned, pointer-events none for line overlay), scroll container with virtualized row divs
- Load `graph.js` as module

**Create `vscode-extension/webview/graphview.js`**:
- Virtual scrolling: fixed-height rows (e.g., 24px), total height = `total_approx * 24px`, only render rows in viewport ± buffer (e.g., 20 rows)
- Each row: commit message (truncated), author, time, branch tags; click to select; double-click to checkout
- Canvas overlay: draw Bézier curves connecting commits to their parents based on `lane` field; use `requestAnimationFrame` for smooth scroll sync
- Scroll handler: send `viewportChanged` postMessage to extension host; receive `graphPage` message → update rows + redraw canvas
- Performance targets: 60fps scroll, <50ms between scroll event and visual update (including JSON-RPC roundtrip)

### 11. VS Code Extension — SCM Provider Integration (`vscode-extension/src/scmProvider.ts`)

Bridge engine status to VS Code's native SCM view.

**Create `vscode-extension/src/scmProvider.ts`**:
- `class WorkbenchSCMProvider`:
  - Implements `vscode.SourceControl` with resource groups: `staged`, `unstaged`, `untracked`
  - On `IndexChanged`/`WorktreeChanged` notification: call `getStatus()` via JSON-RPC, map `StatusItem` to `vscode.SourceControlResourceState`
  - Action commands on resource states: stage/unstage (file level; hunk/line level out of scope for T1)
  - Input box commit: when user types message and clicks ✓, send `Commit { message, amend: false }`; if amend toggle on, `amend: true`
  - Status bar integration: show current branch, ahead/behind counts (fetch required)

### 12. VS Code Extension — Branch QuickPick (`vscode-extension/src/commands.ts`)

Fuzzy branch switcher and CRUD operations.

**Create `vscode-extension/src/commands.ts`**:
- `switchBranch()`: fetch branch list via `GetRefs`, show `vscode.QuickPick` with fzy-style scoring (or simple `includes` for T1), on select send `SwitchBranch { name, auto_stash: true }`
- `createBranch()`: input box for name, optional base picker, send `CreateBranch`
- `deleteBranch()`: pick branch, confirm if unmerged, send `DeleteBranch`
- `undoLast()`: show `QuickPick` of recent undoable transactions (from `ListUndoStack`), on select send `Undo`

### 13. Build & Packaging Scripts (Vite 8)

**Create `vscode-extension/vite.config.ts`**:
- Root config: `build.lib.entry = 'src/extension.ts'`, `target = 'node20'`, `format = 'cjs'`, `outDir = 'out/'`
- Webview sub-config (via `defineConfig` array or separate `vite.webview.config.ts`): `build.rollupOptions.input = 'webview/graph.html'`, `outDir = 'out/webview/'`, `assetsInlineLimit: 0` (CSP compliance — never inline JS/CSS into HTML; VS Code webview requires external script refs)
- `sourcemap: true` for both targets
- No HMR in production build; for webview dev, run `vite dev --config vite.webview.config.ts` with a mock JSON-RPC backend to iterate Canvas graph interaction without reloading the full VS Code window

**Create `vscode-extension/build.js`** (Vite 8 CLI wrapper):
- `vite build` → extension host
- `vite build --config vite.webview.config.ts` → webview
- `vite build --watch` for development loop

**Create `scripts/package-all.sh`**:
- Build Rust release for each target (`cargo build --release --target {triple}`)
- Copy binary to `vscode-extension/server/git-workbench-engine{ext}`
- Run `vsce package --target {vscode_target}` for each platform
- Output: `git-workbench-{version}-{target}.vsix`

### 14. PoC / Benchmark Validation

Before marking Phase 1 complete, run the following benchmarks and spikes:

- **Benchmark A**: Clone `torvalds/linux` (~1M commits) or `chromium` (~100k+ commits), measure `read_graph_page` cold start with/without redb cache. Target: <1s with cache, <3s cold.
- **Benchmark B**: Create or find a 50k-file repository, measure `read_status` after fs event. Target: <100ms.
- **Spike C**: Open engine, run `git commit` in terminal, verify epoch bumps and `GraphInvalidated` fires within 200ms.
- **Spike D**: Perform 5 sequential write operations (stage + commit + push), verify each has an undo snapshot, test undo of the last 2. Verify audit.log has 5 entries.
- **Spike E**: Scroll graph webview in VS Code with 100k commits loaded, verify 60fps via Chrome DevTools Performance tab (webview inspector).

## Acceptance Criteria

- [x] `cargo test --workspace` passes (34 tests: 25 engine integration + 9 protocol — grown from 15 during the post-review hardening pass); >80% coverage target not formally measured
- [x] `cargo build --release` produces a single `git-workbench-engine` binary (verified on linux-x64; cross-platform targets defined in CI matrix)
- [x] VS Code extension compiles clean (tsc --noEmit zero errors) and builds via vite; activates/spawns engine/connects via stdio — code paths verified by the subagent's live smoke test against the real engine binary; full in-editor activation pending manual VS Code run
- [x] Opening a git repository initializes a Session, starts fs watcher, and loads/creates redb cache (verified e2e + integration tests)
- [ ] Commit graph renders in webview with virtual scrolling; 100k+ commit repo scrolls at 60fps (implementation complete; visual verification in VS Code + large-repo test pending — Spike E)
- [x] Status panel updates in <100ms after file changes (engine-side path: fs event → notification in 8ms; <100ms measured on 20 dirty files of 1000)
- [x] Stage/unstage/commit (with/without amend), fetch, pull, push all work via CLI write queue (stage/commit/branch/switch verified e2e; fetch/pull/push implemented, require remote to e2e-test)
- [x] Each mutating operation creates an undo snapshot; undo restores refs/HEAD correctly within 1 second (Spike D: cascade undo verified, audit.log 5 entries)
- [x] External git operations (terminal `git commit`) are detected via fs watcher within 200ms (measured: 8ms)
- [ ] Platform-specific VSIX builds successfully for at least darwin-arm64, linux-x64, win32-x64 (scripts/package-all.sh + CI matrix in place; actual packaging requires cross toolchains)
- [ ] All PoC benchmarks (A-E) pass with documented numbers (C: 8ms ✓, D: pass ✓, perf smoke 200ms ✓; A needs linux-scale repo clone, E needs in-editor run — deferred with documented numbers in Implementation Notes)

## Dependencies

- Rust toolchain 1.78+ installed
- Node.js 20+ and `vsce` CLI for extension packaging
- Git 2.30+ (gix compatibility baseline)
- No other plan phases required — this is Phase 1

## Related Documents

- .ai-workflow/ideas/20260921-rust-vscode-git-workbench.md
- .ai-workflow/research/20260921-git-workbench-research.md
- (Phase 2 plan: `.ai-workflow/plans/20260921-t2-advanced-workflow.md` — to be created after Phase 1 approval)
- (Phase 3 plan: `.ai-workflow/plans/20260921-t3-llm-agent.md` — to be created after Phase 1 approval)

## Implementation Notes (added during implementation)

1. **JSON-RPC server**: implemented manually (Content-Length framing over stdio) instead of `tower-lsp` — LSP machinery was unnecessary for a custom protocol; the dependency remains declared but unused and can be removed in cleanup.
2. **Undo semantics**: snapshot-based cascade rollback. Each write transaction records pre- AND post-write snapshots. `undo(tx_id)` restores the pre-write snapshot and removes all journal entries ≥ tx_id from the stack. Undo is blocked (UNDO_BLOCKED) when current refs differ from the latest transaction's post-snapshot (external changes detection).
3. **Epoch invalidation**: bumped both synchronously by the write queue (after each successful write) and by the fs watcher (coalesced 50ms batches). Double bumps are harmless — the epoch is an invalidation counter, not a sequence number.
4. **fs watcher**: notify watches `.git/HEAD`, `.git/index`, `.git/packed-refs`, `.git/refs/` (recursive), `.git/sequencer/` (if present), and the worktree (recursive; `.git` internals other than the watched paths are filtered out). Events coalesce within 50ms.
5. **Multi-session**: server supports one session (T1 simplification, `get_any_session`); multi-root arrives in T2 per plan.
6. **Multi-step undo across restarts**: journal is `.git/git-workbench/undo/journal.jsonl`; transaction ids persist across restarts (next_id scans journal + snapshot files).

## Implementation Notes (added during post-review hardening, 2026-09-24)

A full code review against this plan surfaced 5 Critical + ~20 Warning + ~10 Suggestion findings; all Criticals and Warnings were fixed:

7. **Argument-injection hardening** (Critical): every string crossing the RPC boundary into a git argv (staging paths, branch/remote names, push refspecs, snapshot refs/shas restored by undo) is validated (`validate_path_arg`/`validate_ref_arg`/`validate_pushspec` in write_queue.rs; `validate_full_ref_name`/`validate_sha` in undo.rs) and `git add`/`git reset` use `--` separators. Git never receives a client-controlled string as a flag position.
8. **Undo crash-atomicity** (Critical): undo writes an `undo-in-progress.json` marker (target snapshot included), then atomically truncates the journal (temp+rename+fsync), then restores refs; `UndoEngine::new` → `recover()` rolls an interrupted undo forward. All snapshot/journal writes are fsynced (file + parent dir). Undo restores refs via one `git update-ref --stdin` batch (with old-value guards), the index via `write-tree`/`read-tree`, and HEAD last.
9. **Undo serialization** (Critical): undo is now `WriteCommand::Undo` routed through the same single-consumer write queue as all writes — concurrent undo/write can no longer interleave journal writes. Structured `UndoErrorKind` (NotFound/Blocked/Failure) maps to invalid_argument / undo_blocked / git_error.
10. **Staged/unstaged classification** (Critical): `StatusItem` carries `staged: bool` (tree-index changes vs index-worktree changes); the SCM provider buckets on the flag instead of guessing from status codes.
11. **Graph geometry**: `total_approx` no longer double-counts the viewport offset; `anchor_commit` is now actually used to re-pin the viewport across invalidations; lanes are `u16` (T2 headroom) with a no-panic fallback when lanes are exhausted.
12. **Server robustness**: Content-Length capped at 16 MiB / headers at 64 KiB (violations close the connection); requests are handled concurrently (JoinSet + single mpsc response writer, gix reads in `spawn_blocking`); handler panics cannot kill the connection; locks are poison-tolerant.
13. **Epoch/cache durability**: the epoch and a repo fingerprint (HEAD + refs/heads) persist in redb — the epoch no longer resets to 1 on engine restart; a fingerprint mismatch (refs moved while the engine was down) clears the lane cache and bumps the epoch in one transaction. The cache now has a read path (COMMIT_META serves viewport rows on cache hits).
14. **Watchdog & process lifecycle**: `.git` internals noise (index.lock churn, Access events) is filtered; linked worktrees resolve their real gitdir via `git rev-parse --absolute-git-dir` (shared `gitdir.rs`, also used by undo snapshots and the cache); watcher/write-queue tasks hold `Weak<Session>` (no leak on re-initialize); `--parent-pid` is parsed and polled; the shutdown RPC actually terminates the serve loop.
15. **Extension architecture**: `git show` and `.git/HEAD` parsing in the extension were replaced with engine RPCs `getBlob`/`getHead`; invalidate/refresh races fixed (requestSeq stale-drop + single EPOCH_MISMATCH retry, error code -32004 defined in the protocol crate); auto-stash is a real stash push/switch/pop flow; amend command added to the SCM title bar; status bar shows the current branch.
16. **Supply chain**: gix-adapt dead crate and 7 unused dependencies removed; Cargo.lock tracked (CI builds `--locked`); CI actions pinned to commit SHAs (a non-existent `dtolnay/rust-action` reference was found and fixed); vsce runs from package-lock.json via `npm ci` (no global installs); cargo-audit job added.
17. **Still deferred** (Suggestion-level, T2 candidates): engine auto-restart after unexpected exit, ahead/behind in the status bar (needs upstream info), double-click checkout-to-detach RPC, Benchmark A (linux-scale) numbers, multi-root sessions.

## Benchmark Results (scaled-down, local)

- Spike C: external terminal commit → refsChanged + graphInvalidated notifications in **8ms** (target <200ms) ✓
- Spike D: 5 sequential writes → 5 undo snapshots, cascade undo of last 2 verified (switchBranch + stage, then createBranch deleted branch, commit reverted refs), audit.log has 5 entries ✓
- Perf smoke (2000-commit repo, 1000 files, release build): full process (init + 3 graph pages + status with 20 dirty files) in **200ms** ✓
  - Benchmark A (linux-scale repo) and Spike E (webview 60fps) require the full extension / large repo clone — deferred to validation with the extension in place.
