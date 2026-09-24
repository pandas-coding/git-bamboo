use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use git_workbench_protocol::{
    Capabilities, CommitMsg, CreateBranch, DeleteBranch, Fetch, GetBlob, GetGraph, GetHeadResult,
    GetStatus, GraphPage, HeadChanged, IndexChanged, Initialize, InitializeResult, ListUndoStack,
    OutputResult, Pull, Push, RefsChanged, Stage, StatusItem, SwitchBranch, Unstage, Undo,
    UndoResponse, WorkbenchError, WorktreeChanged,
};

use crate::session::{EngineEvent, Session};
use crate::write_queue::WriteCommand;

const PROTOCOL_VERSION: u32 = 1;

// Framing caps: bound the resources a single client message can consume.
// A violation closes the connection (a desynced stream cannot be recovered).
/// Maximum accepted `Content-Length` (16 MiB).
pub const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;
/// Maximum accepted total header block size (64 KiB).
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Maximum accepted single header line length (8 KiB).
pub const MAX_HEADER_LINE: usize = 8 * 1024;

/// One parsed incoming JSON-RPC message. `id: None` marks a notification
/// (no response is expected).
pub struct IncomingMessage {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

pub struct WorkbenchServer {
    /// T1 is single-session: the current session (replaced by `initialize`,
    /// cleared by session close). The old session is dropped on replacement
    /// — its watcher and write-queue tasks hold only Weak handles.
    session: tokio::sync::Mutex<Option<Arc<Session>>>,
    notify_tx: mpsc::UnboundedSender<EngineEvent>,
    /// Triggered by the `shutdown` RPC (and `request_shutdown`) to stop the
    /// serve loop.
    shutdown_tx: mpsc::UnboundedSender<()>,
    /// Taken (consumed) by `run()` exactly once.
    shutdown_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<()>>>,
    /// Set to true once a session exists; requests that need a session
    /// wait on it so a request racing a pending `initialize` sees the
    /// session instead of a spurious `not_initialized` (the sequential
    /// server processed initialize first).
    session_ready_tx: tokio::sync::watch::Sender<bool>,
}

impl WorkbenchServer {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<EngineEvent>) {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();
        let (session_ready_tx, _) = tokio::sync::watch::channel(false);
        (
            Arc::new(Self {
                session: tokio::sync::Mutex::new(None),
                notify_tx,
                shutdown_tx,
                shutdown_rx: tokio::sync::Mutex::new(Some(shutdown_rx)),
                session_ready_tx,
            }),
            notify_rx,
        )
    }

    /// Ask the serve loop to shut down (used by SIGINT handling in main).
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    pub async fn run(self: Arc<Self>, mut notify_rx: mpsc::UnboundedReceiver<EngineEvent>) {
        let (out_tx, out_rx) = mpsc::channel::<String>(64);

        // Single writer task: all frames (responses and notifications) go
        // through one channel so writes are serialized. `stop_tx` lets the
        // run loop stop the writer even while the (blocking) stdin reader
        // task still holds an `out_tx` clone.
        let (writer, writer_stop_tx) = {
            let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
            let mut out_rx = out_rx;
            let writer = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        maybe = out_rx.recv() => match maybe {
                            Some(body) => {
                                // Lock per write: a StdoutLock must not be
                                // held across awaits (it is not Send).
                                let stdout = std::io::stdout();
                                let mut lock = stdout.lock();
                                write_frame(&mut lock, &body);
                            }
                            None => break,
                        },
                        _ = stop_rx.changed() => {
                            if *stop_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
                debug!("writer task exiting");
            });
            (writer, stop_tx)
        };

        // Dedicated stdin reader: framed JSON-RPC messages. On a framing
        // violation it emits one error response and closes the connection
        // (a desynced stream cannot be recovered). It holds a writer
        // channel clone for that purpose.
        let (msg_tx, mut msg_rx) = mpsc::channel::<IncomingMessage>(32);
        let err_tx = out_tx.clone();
        let reader_loop = tokio::task::spawn_blocking(move || {
            let stdin = std::io::stdin();
            let mut reader = std::io::BufReader::new(stdin.lock());
            loop {
                match read_message(&mut reader) {
                    Ok(Some((id, method, params))) => {
                        let msg = IncomingMessage { id, method, params };
                        if msg_tx.blocking_send(msg).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!(error = %e, "protocol framing violation; closing connection");
                        let body = serde_json::to_string(&json!({
                            "jsonrpc": "2.0",
                            "id": Value::Null,
                            "error": {
                                "code": -32700,
                                "message": format!("parse error: {e}; connection closed"),
                            }
                        }))
                        .unwrap_or_default();
                        let _ = err_tx.blocking_send(body);
                        break;
                    }
                }
            }
            debug!("stdin reader closed");
        });

        // Main loop: parse-dispatch requests in per-request tasks (gix
        // reads run on spawn_blocking), forward engine notifications, and
        // watch for shutdown. JSON-RPC permits out-of-order responses; the
        // write queue remains the single serialized mutation path.
        let mut join_set: JoinSet<()> = JoinSet::new();
        let mut reader_done = false;
        let mut shutdown = false;
        let mut shutdown_rx = self
            .shutdown_rx
            .lock()
            .await
            .take()
            .expect("run() may only be called once");

        loop {
            tokio::select! {
                msg = msg_rx.recv() => {
                    match msg {
                        Some(IncomingMessage { id, method, params }) => {
                            let server = Arc::clone(&self);
                            let out_tx = out_tx.clone();
                            join_set.spawn(async move {
                                let response = server
                                    .handle_message(id, &method, params)
                                    .await;
                                if let Some(body) = response {
                                    if out_tx.send(body).await.is_err() {
                                        debug!("connection closing; response dropped");
                                    }
                                }
                            });
                        }
                        None => { reader_done = true; }
                    }
                }
                notif = notify_rx.recv() => {
                    match notif {
                        Some(event) => {
                            let n = self.engine_event_to_notification(event);
                            let body = serde_json::to_string(&n).unwrap_or_default();
                            if out_tx.send(body).await.is_err() {
                                debug!("connection closing; notification dropped");
                            }
                        }
                        None => {
                            // Notify channel closed: impossible while the
                            // server holds a sender; exit defensively.
                            break;
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    shutdown = true;
                }
            }
            if reader_done || shutdown {
                if reader_done {
                    // Drain remaining notifications briefly, then exit.
                    while let Ok(event) = notify_rx.try_recv() {
                        let n = self.engine_event_to_notification(event);
                        let body = serde_json::to_string(&n).unwrap_or_default();
                        let _ = out_tx.try_send(body);
                    }
                    info!("stdin closed; shutting down");
                } else {
                    info!("shutdown requested; stopping serve loop");
                }
                break;
            }
        }

        // Let in-flight handlers finish so their responses are written,
        // then stop the writer and exit. A handler panic only aborts its
        // own task (JoinSet absorbs it). Releasing the session-ready
        // watch first lets any handler still waiting for `initialize`
        // fail fast instead of blocking the join.
        let _ = self.session_ready_tx.send(true);
        while let Some(res) = join_set.join_next().await {
            if let Err(e) = res {
                warn!(error = %e, "request handler task failed");
            }
        }
        drop(out_tx);
        // Stop the writer even if the (blocking) stdin reader still holds a
        // channel clone and is blocked in a read.
        let _ = writer_stop_tx.send(true);
        let _ = writer.await;
        // The reader may still be blocked reading stdin (client sent
        // `shutdown` without closing stdin); give it a brief grace period,
        // then exit regardless — awaiting it forever would hang shutdown.
        let reader_done = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            reader_loop,
        )
        .await
        .is_ok();
        info!(reader_done, "server loop exited");
    }

    /// Handle one parsed message. Returns the serialized JSON-RPC response
    /// for requests, `None` for notifications (fire-and-forget).
    async fn handle_message(
        self: &Arc<Self>,
        id: Option<Value>,
        method: &str,
        params: Value,
    ) -> Option<String> {
        debug!(method, "handling message");
        let result = self.dispatch(method, params).await;
        let id = id?;
        let response = match result {
            Ok(result) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": serde_json::to_value(&e).unwrap_or(Value::Null),
            }),
        };
        Some(serde_json::to_string(&response).unwrap_or_default())
    }

    async fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, WorkbenchError> {
        match method {
            "initialize" => self.handle_initialize(params).await,
            "shutdown" => {
                info!("shutdown requested");
                self.request_shutdown();
                Ok(Value::Null)
            }
            _ => {
                let session = self.get_any_session().await?;
                self.dispatch_with_session(session, method, params).await
            }        }
    }

    async fn handle_initialize(&self, params: Value) -> Result<Value, WorkbenchError> {
        let params: Initialize = parse_params("initialize", params)?;
        let repo_path = PathBuf::from(&params.repo_path);
        // Canonicalize so watcher paths, gitdir resolution and undo
        // snapshots all agree on one absolute spelling of the worktree.
        let repo_path = repo_path.canonicalize().map_err(|e| {
            WorkbenchError::invalid_argument(format!(
                "initialize: cannot resolve repo_path {:?}: {e}",
                params.repo_path
            ))
        })?;
        // `.git` may be a directory (normal repo) or a file (linked
        // worktree); either is valid, neither must be missing.
        if !repo_path.join(".git").exists() {
            return Err(WorkbenchError::repo_not_found(repo_path.display().to_string()));
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut slot = self.session.lock().await;
            // Drop the previous session FIRST: its redb cache holds an
            // exclusive file lock the new session needs, and dropping it
            // (its watcher/write-queue tasks hold only Weak handles) frees
            // it instead of leaking the session forever.
            *slot = None;
            drop(slot);
        }

        let session = match Session::open(&repo_path, self.notify_tx.clone()).await {
            Ok(session) => session,
            Err(e) => {
                // Release waiters so a request racing this failed initialize
                // fails fast instead of waiting until shutdown.
                let _ = self.session_ready_tx.send(true);
                return Err(WorkbenchError::git_error(format!(
                    "failed to open session: {e}"
                )));
            }
        };

        {
            let mut slot = self.session.lock().await;
            *slot = Some(session.clone());
        }
        let _ = self.session_ready_tx.send(true);

        let result = InitializeResult {
            session_id,
            protocol_version: PROTOCOL_VERSION,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            // The real epoch (the fs watcher may have bumped it between
            // session open and this response).
            epoch: session.current_epoch(),
            capabilities: Capabilities {
                graph: true,
                status: true,
                write_ops: true,
                undo: true,
                notifications: true,
            },
        };
        to_json_value(&result)
    }

    async fn get_any_session(&self) -> Result<Arc<Session>, WorkbenchError> {
        let err = || WorkbenchError::not_initialized("no session; call initialize first");
        if let Some(session) = self.session.lock().await.clone() {
            return Ok(session);
        }
        // No session yet. Wait for one to appear: a concurrent `initialize`
        // may be mid-flight (the old sequential server always processed
        // it first). If the watch sender is gone (server shutting down),
        // fail immediately.
        let mut ready = self.session_ready_tx.subscribe();
        while !*ready.borrow_and_update() {
            if ready.changed().await.is_err() {
                return Err(err());
            }
        }
        self.session.lock().await.clone().ok_or_else(err)
    }

    async fn dispatch_with_session(
        self: &Arc<Self>,
        session: Arc<Session>,
        method: &str,
        params: Value,
    ) -> Result<Value, WorkbenchError> {
        match method {
            "getRefs" => {
                let refs = spawn_blocking_read(session, |session| {
                    crate::gix_read::read_refs(session)
                })
                .await
                .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&refs)
            }
            "getBlob" => {
                let req: GetBlob = parse_params("getBlob", params)?;
                let content: String = spawn_blocking_read(session, move |s| {
                    crate::gix_read::read_blob(s, &req.revision, &req.path)
                })
                .await
                .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&content)
            }
            "getHead" => {
                let head: GetHeadResult =
                    spawn_blocking_read(session, crate::gix_read::read_head)
                        .await
                        .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&head)
            }
            "getGraph" => {
                let req: GetGraph = parse_params("getGraph", params)?;
                let mut viewport = req.viewport;
                viewport.limit = viewport.limit.min(1000);
                let page: GraphPage = spawn_blocking_read(session, move |s| {
                    crate::gix_read::read_graph_page(s, &viewport)
                })
                .await
                .map_err(|e| {
                    // Preserve structured errors (e.g. EPOCH_MISMATCH).
                    e.downcast::<WorkbenchError>()
                        .unwrap_or_else(|e| WorkbenchError::git_error(e.to_string()))
                })?;
                to_json_value(&page)
            }
            "getStatus" => {
                let req: GetStatus = parse_params("getStatus", params)?;
                let items: Vec<StatusItem> =
                    spawn_blocking_read(session, move |s| {
                        crate::gix_read::read_status(s, req.paths.as_deref())
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&items)
            }
            "stage" => {
                let req: Stage = parse_params("stage", params)?;
                session
                    .enqueue_write(WriteCommand::Stage(req.paths))
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "unstage" => {
                let req: Unstage = parse_params("unstage", params)?;
                session
                    .enqueue_write(WriteCommand::Unstage(req.paths))
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "commit" => {
                let req: CommitMsg = parse_params("commit", params)?;
                session
                    .enqueue_write(WriteCommand::Commit {
                        message: req.message,
                        amend: req.amend,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "createBranch" => {
                let req: CreateBranch = parse_params("createBranch", params)?;
                session
                    .enqueue_write(WriteCommand::CreateBranch {
                        name: req.name,
                        base: req.base,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "deleteBranch" => {
                let req: DeleteBranch = parse_params("deleteBranch", params)?;
                session
                    .enqueue_write(WriteCommand::DeleteBranch {
                        name: req.name,
                        force: req.force,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "switchBranch" => {
                let req: SwitchBranch = parse_params("switchBranch", params)?;
                let output = session
                    .enqueue_write(WriteCommand::SwitchBranch {
                        name: req.name,
                        auto_stash: req.auto_stash,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&OutputResult { output })
            }
            "fetch" => {
                let req: Fetch = parse_params("fetch", params)?;
                let output = session
                    .enqueue_write(WriteCommand::Fetch { remote: req.remote })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&OutputResult { output })
            }
            "pull" => {
                let req: Pull = parse_params("pull", params)?;
                let output = session
                    .enqueue_write(WriteCommand::Pull {
                        remote: req.remote,
                        branch: req.branch,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&OutputResult { output })
            }
            "push" => {
                let req: Push = parse_params("push", params)?;
                session
                    .enqueue_write(WriteCommand::Push {
                        remote: req.remote,
                        branch: req.branch,
                        force: req.force,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "listUndoStack" => {
                let req: ListUndoStack = parse_params("listUndoStack", params)?;
                // Clamp instead of a blind cast: a huge or negative value
                // must neither wrap nor blow up the response.
                let limit = u32::try_from(req.limit).unwrap_or(1).clamp(1, 500);
                let session_for_list = Arc::clone(&session);
                let entries = tokio::task::spawn_blocking(move || {
                    session_for_list.undo.list_transactions(limit)
                })
                .await
                .map_err(|e| WorkbenchError::git_error(format!("list task failed: {e}")))?
                .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                to_json_value(&entries)
            }
            "undo" => {
                let req: Undo = parse_params("undo", params)?;

                // Routed through the write queue so undo is serialized
                // against all other mutations (no concurrent journal writes).
                let result = session
                    .enqueue_undo(req.transaction_id)
                    .await
                    .map_err(|e| WorkbenchError::git_error(format!("{e}")))?;

                match result {
                    Ok(summary) => {
                        let epoch = session.current_epoch();
                        to_json_value(&UndoResponse {
                            restored_refs: summary.restored_refs,
                            noop: summary.noop,
                            epoch,
                        })
                    }
                    Err(crate::undo::UndoErrorKind::NotFound) => {
                        Err(WorkbenchError::invalid_argument(format!(
                            "undo: transaction {} not found",
                            req.transaction_id
                        )))
                    }
                    Err(crate::undo::UndoErrorKind::Blocked { reason }) => {
                        Err(WorkbenchError::undo_blocked(reason))
                    }
                    Err(crate::undo::UndoErrorKind::Failure { msg }) => {
                        Err(WorkbenchError::git_error(msg))
                    }
                }
            }
            _ => Err(WorkbenchError::invalid_argument(format!(
                "unknown method: {method}"
            ))),
        }
    }

    fn engine_event_to_notification(&self, event: EngineEvent) -> Value {
        let (method, params): (&str, Result<Value, serde_json::Error>) = match event {
            EngineEvent::RefsChanged { changed_refs, epoch } => (
                "refsChanged",
                serde_json::to_value(RefsChanged { changed_refs, epoch }),
            ),
            EngineEvent::WorktreeChanged { paths } => (
                "worktreeChanged",
                serde_json::to_value(WorktreeChanged { paths }),
            ),
            EngineEvent::IndexChanged { epoch } => (
                "indexChanged",
                serde_json::to_value(IndexChanged { epoch }),
            ),
            EngineEvent::HeadChanged { new_head, epoch } => (
                "headChanged",
                serde_json::to_value(HeadChanged { new_head, epoch }),
            ),
            EngineEvent::GraphInvalidated { epoch } => (
                "graphInvalidated",
                serde_json::to_value(git_workbench_protocol::GraphInvalidated { epoch }),
            ),
        };
        let params = params.unwrap_or(Value::Null);
        json!({"jsonrpc": "2.0", "method": method, "params": params})
    }
}

/// Parse typed request params, mapping serde failures to invalid_argument.
fn parse_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Value,
) -> Result<T, WorkbenchError> {
    serde_json::from_value(params)
        .map_err(|e| WorkbenchError::invalid_argument(format!("{method}: {e}")))
}

/// Serialize a typed response, mapping (impossible) failures to a git error
/// instead of panicking in a request path.
fn to_json_value<T: serde::Serialize>(value: &T) -> Result<Value, WorkbenchError> {
    serde_json::to_value(value)
        .map_err(|e| WorkbenchError::git_error(format!("response serialization failed: {e}")))
}

/// Run a blocking gix read on the blocking pool, returning its result.
async fn spawn_blocking_read<T, F>(session: Arc<Session>, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Session) -> anyhow::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&session))
        .await
        .map_err(|e| anyhow::anyhow!("read task failed: {e}"))?
}

fn write_frame(out: &mut impl Write, body: &str) {
    if write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body).is_ok() {
        let _ = out.flush();
    }
}

/// Read one Content-Length-framed JSON-RPC message. Returns `Ok(None)` on
/// EOF (including a clean EOF mid-header). Any framing violation is an
/// error: the caller must answer with an error response and close the
/// connection, since the stream can no longer be trusted.
///
/// Caps: total header block 64 KiB, single header line 8 KiB,
/// Content-Length 16 MiB.
pub fn read_message(
    reader: &mut impl BufRead,
) -> anyhow::Result<Option<(Option<Value>, String, Value)>> {
    // Parse headers.
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        // Bounded read: a single header line without a newline can never
        // grow the buffer past the line cap.
        let n = reader.take((MAX_HEADER_LINE + 1) as u64).read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF (clean whether or not headers started)
        }
        header_bytes += n;
        if header_bytes > MAX_HEADER_BYTES {
            anyhow::bail!("header block exceeds {MAX_HEADER_BYTES} bytes");
        }
        if n > MAX_HEADER_LINE {
            anyhow::bail!("header line exceeds {MAX_HEADER_LINE} bytes");
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // End of headers.
        }
        if let Some(len) = line.strip_prefix("Content-Length:") {
            let len = len.trim();
            content_length = Some(
                len.parse()
                    .map_err(|e| anyhow::anyhow!("invalid Content-Length {len:?}: {e}"))?,
            );
        }
        // Other headers (Content-Type, ...) are ignored.
    }

    let content_length =
        content_length.ok_or_else(|| anyhow::anyhow!("missing Content-Length header"))?;
    if content_length > MAX_CONTENT_LENGTH {
        anyhow::bail!(
            "Content-Length {content_length} exceeds the maximum of {MAX_CONTENT_LENGTH}"
        );
    }
    let mut buf = vec![0u8; content_length];
    // EOF mid-body: an error — the stream is truncated, so close.
    reader.read_exact(&mut buf)?;

    let value: Value = serde_json::from_slice(&buf)?;
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("message missing method"))?
        .to_string();
    // Absent `params` behaves like `{}` (JSON-RPC allows omitting it);
    // the old parser treated both the same way.
    let params = match value.get("params") {
        Some(v) if !v.is_null() => v.clone(),
        _ => Value::Object(Default::default()),
    };
    // Absent `id` marks a JSON-RPC notification.
    let id = value.get("id").cloned();
    Ok(Some((id, method, params)))
}
