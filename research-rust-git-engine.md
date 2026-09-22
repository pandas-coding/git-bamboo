# Research: Rust-Based Git Engine for VS Code

## 1. gix (gitoxide) 0.87+ APIs

### Scoped Status Walks
Entry point: `Repository::status<P>(progress: P) -> Result<status::Platform<'_, P>, Error>`
- `P: Progress + 'static` — requires a progress implementation; can use `gix::progress::Discard` if unneeded.
- Returns a `status::Platform` builder for configuring the walk.

Key `Platform` methods:
- `into_iter(patterns) -> Result<Iter, Error>` — full status (index/worktree + tree/index), yields `status::Item`.
- `into_index_worktree_iter(patterns) -> Result<Iter, Error>` — scoped to index vs working tree only.
- `untracked_files(UntrackedFiles)` — control untracked file handling (`None`, `Files`, `All`).
- `dirwalk_options(cb)` / `dirwalk_options_mut(cb)` — configure directory traversal (symlinks, recursion, etc.).
- `index_worktree_rewrites(Rewrites)` — detect renames/copies in index/worktree diff.
- `tree_index_track_renames(TrackRenames)` — detect renames in tree/index diff.
- `should_interrupt_shared(&'static AtomicBool)` / `should_interrupt_owned(Arc<AtomicBool>)` — cancellation.
- `head_tree(ObjectId)` — use a specific tree as baseline instead of HEAD.

Iterator yields `status::Item` with `location() -> &BStr`. The enum has variants like `IndexWorktree` and `TreeIndex` changes (concrete variants not fully enumerated in docs, but `Item` is `From<Change>`).

**Gotcha:** `Platform` is `!Send + !Sync` — status iteration must happen on a single thread or be spawned with its data extracted.

### Log Traversal
Entry point: `Repository::rev_walk(tips) -> revision::walk::Platform<'_>`
- `tips: impl IntoIterator<Item = impl Into<ObjectId>>` — starting commits (e.g., `["HEAD"]`).

Key `revision::walk::Platform` methods:
- `sorting(Sorting) -> Self` — topological/date ordering. `Sorting` enum variants include `ByCommitTimeCutoff` and topological defaults.
- `first_parent_only() -> Self` — follow only first parents.
- `use_commit_graph(impl Into<Option<bool>>) -> Self` — toggle commit-graph acceleration (respects `core.commitGraph`).
- `with_commit_graph(Option<Graph>) -> Self` — provide an external `gix_commitgraph::Graph`.
- `with_hidden(ids) -> Self` — exclude commits (equivalent to `^branch` in git).
- `with_boundary(ids) -> Self` — treat these as boundaries.
- `all() -> Result<Walk<'repo>, Error>` — return all commits.
- `selected(filter) -> Result<Walk<'repo>, Error>` — filter with `FnMut(&oid) -> bool`.

`Walk` is the actual iterator over commits.

**Gotcha:** `use_commit_graph(false)` disables the commit-graph even if present. If you want deterministic graph lane calculation, you likely want topological sorting + full parent traversal (not `first_parent_only`).

### Commit Graph Lane Calculation
Two different "Graph" types exist:

1. **`gix_commitgraph::Graph`** — read-only on-disk commit-graph index (`.git/objects/info/commit-graph`).
   - `Graph::at(path)` — load from `.git/objects/info` directory.
   - `commit_by_id(id) -> Option<Commit>` — fast O(1) lookup.
   - `iter_commits() -> impl Iterator<Item = Commit>` — iterate all commits in graph.
   - `lookup(id) -> Option<Position>` — get numeric position.
   - `num_commits() -> u32`.
   - `Commit` provides `id()`, `generation()`, `parent_positions()`, `commit_time()`.

2. **`gix_revwalk::Graph<'find, 'cache, T>`** — in-memory traversal graph that associates custom data `T` with each commit.
   - Used internally by `merge_base()` and other algorithms.
   - `try_lookup_or_insert(id, update_data)` — lazy commit resolution with data association.
   - `insert_data(id, make_data)` — insert with custom data factory.
   - `get(id) / get_mut(id) / contains(id)` — hashmap-like access.
   - `len() / is_empty()`.

**Lane calculation:** There is no built-in "graph lane" API equivalent to `git log --graph`. You must build this yourself:
- Use `Repository::rev_walk()` with topological sorting to get the commit stream.
- Maintain a `HashMap<ObjectId, LaneId>` mapping commits to visual lanes.
- When a commit has multiple children, assign new lanes; when lanes merge, retire them.
- Use `gix_revwalk::Graph<YourLaneState>` to cache lane state per commit if pre-computing.
- Use `gix_commitgraph::Graph` to accelerate parent lookups if the repo has a commit-graph written.

**Gotcha:** `gix_revwalk::Graph` and `gix_commitgraph::Graph` are different crates/types. The former is for algorithm state; the latter is the on-disk index. Use `use_commit_graph(true)` on the walk Platform to automatically leverage the on-disk graph.

---

## 2. VS Code Extension with Platform-Specific Native Binaries

### Official VS Code Support
VS Code has supported **platform-specific extensions** since VS Code 1.61.0. `vsce` supports `--target` since 1.99.0.

Supported platforms:
- `win32-x64`, `win32-arm64`
- `linux-x64`, `linux-arm64`, `linux-armhf`
- `alpine-x64`, `alpine-arm64`
- `darwin-x64`, `darwin-arm64`
- `web`

Packaging:
```bash
vsce package --target darwin-arm64
vsce publish --target darwin-arm64
```

Publishing multiple platforms:
```bash
vsce publish --target win32-x64 win32-arm64 linux-x64 linux-arm64 darwin-x64 darwin-arm64
```

A package without `--target` serves as the **fallback** for platforms that lack a specific package.

### How rust-analyzer Does It
rust-analyzer is the canonical example of a VS Code extension shipping a pre-compiled Rust binary:

- The TypeScript extension looks for the server binary in this order:
  1. Explicit config (`config.serverPath` or `__RA_LSP_SERVER_DEBUG` env var)
  2. Toolchain override (`rustup which rust-analyzer`)
  3. **Bundled binary:** `context.extensionUri + "server/rust-analyzer" + (win32 ? ".exe" : "")`
- The bundled binary is packaged into the VSIX at build time.
- The extension uses `vscode-languageclient` to spawn the binary and communicate over stdio.

### CI Pattern (from Microsoft sample)
GitHub Actions matrix builds per platform:
```yaml
strategy:
  matrix:
    include:
      - os: windows-latest
        platform: win32
        arch: x64
        npm_config_arch: x64
      - os: ubuntu-latest
        platform: linux
        arch: x64
        npm_config_arch: x64
      - os: macos-latest
        platform: darwin
        arch: arm64
        npm_config_arch: arm64
steps:
  - run: npm install
    env:
      npm_config_arch: ${{ matrix.npm_config_arch }}
  - run: echo "target=${{ matrix.platform }}-${{ matrix.arch }}" >> $env:GITHUB_ENV
  - run: npx vsce package --target ${{ env.target }}
  - uses: actions/upload-artifact@v4
    with:
      name: ${{ env.target }}
      path: "*.vsix"
```

A separate `publish` job downloads all artifacts and runs:
```bash
npx vsce publish --packagePath $(find . -iname *.vsix)
```

**Gotchas:**
- `npm_config_arch` must be set during `npm install` so that any native Node.js dependencies (if your extension has them) are built for the target architecture. For a pure Rust binary, this is less relevant, but good practice.
- Alpine Linux uses `musl` — your Rust binary must be statically linked or built against `musl` (use `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` targets).
- The `web` platform requires a separate entry point (`browser` in `package.json`). If your extension uses a native binary, the web version will need to disable those features via `when` clauses.

---

## 3. Rust Persistent Embedded Cache: redb vs SQLite

### redb (v4.3.0)
- **Pure Rust**, zero C dependencies.
- **API:** BTreeMap-like, type-safe via `Key` and `Value` traits.
- **Transactions:** ACID, MVCC, single writer + multiple concurrent readers (readers never block).
- **Storage:** Single file, copy-on-write B+trees, crash-safe by default.
- **Performance:** Competitive with LMDB/RocksDB. From published benchmarks:
  - Bulk load: ~17s (SQLite: ~15s, LMDB: ~9s)
  - Individual writes: **920ms** (SQLite: **7040ms**)
  - Random reads: 1138ms (SQLite: not directly comparable; SQLite random reads are often slower due to SQL parsing)
- **Append-only ergonomics:** Excellent. `insert()` into a `TableDefinition<K, V>` is native Rust. No SQL strings.
- **no_std support:** Available (with `alloc` and custom `StorageBackend`).

Example:
```rust
use redb::{Database, TableDefinition};
const TABLE: TableDefinition<&str, u64> = TableDefinition::new("my_data");
let db = Database::create("my_db.redb")?;
let write_txn = db.begin_write()?;
{
    let mut table = write_txn.open_table(TABLE)?;
    table.insert("my_key", &123)?;
}
write_txn.commit()?;
```

### SQLite (via rusqlite)
- **Mature C library** with Rust bindings.
- **API:** SQL interface; can use `rusqlite` with `serde_rusqlite` or raw SQL.
- **Transactions:** WAL mode provides concurrent reads + single writer. Very mature.
- **Performance:** Excellent for complex queries, indexing. Slower for simple key-value inserts due to SQL parsing and generic architecture.
- **Flexibility:** SQL schema migrations, complex queries, full-text search.

### Recommendation for Commit Metadata + Graph Lanes
For a **read-heavy, append-only workload** storing commit metadata and pre-computed graph lanes:

**Use redb.**
- Zero C dependency simplifies cross-compilation for VS Code platform targets.
- Native Rust `Key`/`Value` traits (with `serde` or `bincode`) eliminate SQL boilerplate.
- MVCC means read queries from the VS Code extension never block the write daemon.
- Append-only maps cleanly to redb's BTreeMap semantics.
- Faster individual writes mean the daemon can ingest new commits quickly.

**When to prefer SQLite:**
- If you need complex ad-hoc queries (e.g., "find all commits touching path X between dates Y and Z").
- If you need full-text search on commit messages.
- If you need to share the cache with non-Rust tools.

**Gotchas for redb:**
- Single writer per database file. If your daemon has multiple writer threads, you must serialize access to `begin_write()`.
- File format is stable but still younger than SQLite. Plan for format upgrade paths (redb provides upgrade support).
- Table schemas are defined at compile time via `TableDefinition`. Dynamic schemas require more work.

---

## 4. watchexec Rust Library (v8.4.2)

### API Pattern
`watchexec` is a library built on Tokio for cross-platform file watching.

Core usage:
```rust
use watchexec::{Config, Watchexec};
use std::time::Duration;

let mut config = Config::default();
config.pathset(["/path/to/watch"]);
config.file_watcher(Watcher::Native); // or Watcher::Poll(Duration::from_secs(2))
config.throttle(Duration::from_millis(100));
config.on_action(|action| {
    // Process action.events
    action
});

let wx = Watchexec::new(config)?;
wx.run().await?;
```

### Cross-Platform Backends
The `Watcher` enum:
- `Watcher::Native` (default) — uses `notify::RecommendedWatcher`.
  - **Linux:** inotify (via `notify` crate).
  - **macOS:** FSEvents.
  - **Windows:** ReadDirectoryChangesW.
- `Watcher::Poll(Duration)` — polling fallback.

**Platform-specific behaviors handled by watchexec:**
- **FSEvents (macOS):** Cannot watch non-recursively. Watchexec watches the entire tree and applies filtering at the event level rather than the watch-registration level.
- **Kqueue (BSD):** If `notify` recommends kqueue, Watchexec **explicitly uses Poll instead** because kqueue is unreliable for large directory trees.
- **inotify (Linux):** Uses `notify` crate under the hood. inotify watch limits (`fs.inotify.max_user_watches`) are an OS-level concern. If you hit the limit, the `notify` backend returns errors which Watchexec surfaces through the `on_error` hook as `CriticalError::FsWatcherInit` or `RuntimeError` events.

### Configurable Fields
- `pathset: Vec<WatchedPath>` — directories/files to watch.
- `file_watcher: Watcher` — backend selection.
- `follow_symlinks: bool`.
- `filterer: impl Filterer` — custom ignore/filter logic.
- `throttle: Duration` — debounce window.
- `on_action` / `on_action_async` — action handler (sync or async).
- `on_error` — error handler.
- `keyboard_events: bool` — watch for keyboard input (Ctrl-C, etc.).
- `signal_job_control: bool` — SIGTSTP/SIGCONT handling.

**Gotchas:**
- inotify limits are not automatically increased by the library. For very large repos, you may need to either:
  - Increase `/proc/sys/fs/inotify/max_user_watches` (requires root).
  - Use `Watcher::Poll` as a fallback.
  - Scope the watch to specific subdirectories rather than the entire repo root.
- Watchexec does its own recursive traversal and registers watches per-directory on inotify. Very deep trees can exhaust watches even if the repo isn't that large.
- The `action` callback receives `ActionHandler` containing `events: Vec<Event>`. Events have `Tag`s including `Source::Filesystem` with paths.

---

## 5. JSON-RPC in Rust

### tower-lsp (v0.20.0)
**Best choice for LSP over stdio.**

- Built on Tower (service middleware framework).
- Transport: `Server::new(stdin, stdout, socket)` — stdio is first-class.
- `ClientSocket` handles the loopback channel for client-initiated requests.
- Example pattern from docs:

```rust
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> { /* ... */ }
    async fn shutdown(&self) -> Result<()> { Ok(()) }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend { client });
    Server::new(stdin, stdout, socket).serve(service).await;
}
```

- Provides `jsonrpc` module with `Request`, `Response`, `Error`, `ErrorCode`, `Id`.
- `concurrency_level(usize)` on `Server` controls request parallelism.

### jsonrpsee (v0.26.0)
**Best choice for general JSON-RPC (HTTP/WebSocket), not stdio.**

- Supports HTTP server/client, WebSocket server/client, WASM client.
- Server API: `Server::builder().build(addr).await?.start(methods)`.
- **No built-in stdio transport.** You would need to write a custom transport layer using `tokio::io::stdin/stdout` and feed it into `jsonrpsee`'s `Methods`/`RpcModule`.
- Proc macros (`#[rpc(method = "foo")]`) for API generation.
- Subscriptions supported over WebSocket.

### lsp-server (v0.7.9)
**What rust-analyzer actually uses.**

- Lightweight, custom JSON-RPC implementation (not Tower-based).
- Part of the `rust-analyzer` project ecosystem.
- stdio is the primary transport.
- Lower-level than `tower-lsp`: you manually handle message reading/writing and dispatch.
- Good if you want minimal dependencies and don't need Tower middleware.

### Recommendation
For a VS Code Git engine communicating over stdio:

1. **tower-lsp** — if you want a mature, ergonomic LSP framework with built-in stdio support, request routing, and middleware. Best for rapid development.
2. **lsp-server** — if you want the exact same stack as rust-analyzer, with minimal abstraction overhead. Good if you plan to deeply customize the protocol.
3. **jsonrpsee** — only if you plan to add HTTP/WebSocket transports later. Requires writing a stdio adapter.

**Gotchas:**
- `tower-lsp` uses `async-trait`, which incurs a small allocation per call. This is irrelevant for a Git engine.
- `tower-lsp`'s `Server` takes `AsyncRead + Unpin` and `AsyncWrite`. `tokio::io::Stdin` and `tokio::io::Stdout` work directly.
- JSON-RPC message boundaries over stdio require Content-Length headers (LSP standard). `tower-lsp` handles this framing automatically.
- If using `jsonrpsee` with a custom stdio transport, you must implement the LSP message framing yourself (or copy it from `tower-lsp`'s `codec` module).

---

## Summary Table

| Concern | Recommended Crate / Approach |
|---------|------------------------------|
| Git status walk | `gix::Repository::status()` -> `Platform::into_iter()` |
| Log traversal | `gix::Repository::rev_walk()` -> `Platform::all()` with `Sorting` |
| Commit graph acceleration | `gix_commitgraph::Graph` + `Platform::use_commit_graph(true)` |
| Graph lane pre-computation | Custom algorithm atop `gix_revwalk::Graph<LaneState>` |
| VS Code platform binaries | `vsce --target <platform>`, CI matrix, bundle in `server/` |
| Embedded cache | **redb** for pure-Rust, zero-copy, append-only KV |
| File watching | `watchexec` with `Watcher::Native` (fallback to `Poll`) |
| JSON-RPC over stdio | **tower-lsp** (or `lsp-server` if mimicking rust-analyzer) |
