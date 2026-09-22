use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use git_workbench_protocol::{GraphViewport, WorkbenchError};

use crate::session::{EngineEvent, Session};
use crate::write_queue::WriteCommand;

const PROTOCOL_VERSION: u32 = 1;

pub struct WorkbenchServer {
    /// Multiple sessions are keyed by session id. In T1 all requests use
    /// `get_any_session` (single-repo simplification); multi-root support
    /// arrives with per-session routing in T2.
    sessions: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
    notify_tx: mpsc::UnboundedSender<EngineEvent>,
}

impl WorkbenchServer {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<EngineEvent>) {
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                sessions: tokio::sync::Mutex::new(HashMap::new()),
                notify_tx,
            }),
            notify_rx,
        )
    }

    pub async fn run(self: Arc<Self>, mut notify_rx: mpsc::UnboundedReceiver<EngineEvent>) {
        let (req_tx, mut req_rx) =
            mpsc::channel::<(Value, String, Value)>(32);

        // Dedicated stdin reader: framed JSON-RPC messages.
        let reader_loop = tokio::task::spawn_blocking(move || {
            let stdin = std::io::stdin();
            let mut reader = std::io::BufReader::new(stdin.lock());
            let tx = req_tx;
            loop {
                match read_message(&mut reader) {
                    Ok(Some((id, method, params))) => {
                        if tx.blocking_send((id, method, params)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        warn!(error = %e, "malformed message from client");
                    }
                }
            }
            debug!("stdin reader closed");
        });

        // Main loop: process requests and forward engine notifications.
        // (tokio mpsc recv() is cancel-safe, so re-creating the futures in
        // each select! arm is safe.)
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        let mut reader_done = false;

        loop {
            tokio::select! {
                msg = req_rx.recv() => {
                    match msg {
                        Some((id, method, params)) => {
                            let response = self.handle_request(&id, &method, params).await;
                            write_jsonrpc(&mut out, &response);
                        }
                        None => { reader_done = true; }
                    }
                }
                notif = notify_rx.recv() => {
                    match notif {
                        Some(event) => {
                            let n = self.engine_event_to_notification(event);
                            write_jsonrpc(&mut out, &n);
                        }
                        None => {
                            // Notify channel closed: impossible while the
                            // server holds a sender; exit defensively.
                            break;
                        }
                    }
                }
            }
            if reader_done {
                // Drain remaining notifications briefly, then exit.
                while let Ok(event) = notify_rx.try_recv() {
                    let n = self.engine_event_to_notification(event);
                    write_jsonrpc(&mut out, &n);
                }
                info!("stdin closed; shutting down");
                break;
            }
        }
        let _ = reader_loop.await;
    }

    fn engine_event_to_notification(&self, event: EngineEvent) -> Value {
        match event {
            EngineEvent::RefsChanged { changed_refs, epoch } => {
                json!({"jsonrpc": "2.0", "method": "refsChanged", "params": {"changed_refs": changed_refs, "epoch": epoch}})
            }
            EngineEvent::WorktreeChanged { paths } => {
                json!({"jsonrpc": "2.0", "method": "worktreeChanged", "params": {"paths": paths}})
            }
            EngineEvent::IndexChanged { epoch } => {
                json!({"jsonrpc": "2.0", "method": "indexChanged", "params": {"epoch": epoch}})
            }
            EngineEvent::HeadChanged { new_head, epoch } => {
                json!({"jsonrpc": "2.0", "method": "headChanged", "params": {"new_head": new_head, "epoch": epoch}})
            }
            EngineEvent::GraphInvalidated { epoch } => {
                json!({"jsonrpc": "2.0", "method": "graphInvalidated", "params": {"epoch": epoch}})
            }
        }
    }

    async fn handle_request(&self, id: &Value, method: &str, params: Value) -> Value {
        debug!(method, "handling request");
        let result = self.dispatch(method, params).await;
        match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => {
                json!({"jsonrpc": "2.0", "id": id, "error": serde_json::to_value(&e).unwrap()})
            }
        }
    }

    async fn dispatch(&self, method: &str, params: Value) -> Result<Value, WorkbenchError> {
        match method {
            "initialize" => self.handle_initialize(params).await,
            "shutdown" => {
                info!("shutdown requested");
                Ok(Value::Null)
            }
            _ => {
                let session = self.get_any_session().await?;
                self.dispatch_with_session(session, method, params).await
            }
        }
    }

    async fn handle_initialize(&self, params: Value) -> Result<Value, WorkbenchError> {
        let repo_path = params
            .get("repo_path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| WorkbenchError::invalid_argument("initialize: repo_path required"))?;
        let repo_path = PathBuf::from(repo_path);
        if !repo_path.join(".git").exists() {
            return Err(WorkbenchError::repo_not_found(repo_path.display().to_string()));
        }

        let session = Session::open(&repo_path, self.notify_tx.clone())
            .await
            .map_err(|e| WorkbenchError::git_error(format!("failed to open session: {e}")))?;

        let session_id = uuid::Uuid::new_v4().to_string();
        {
            let mut sessions = self.sessions.lock().await;
            // T1: single session; replace any previous.
            sessions.clear();
            sessions.insert(session_id.clone(), session);
        }

        Ok(json!({
            "session_id": session_id,
            "protocol_version": PROTOCOL_VERSION,
            "engine_version": env!("CARGO_PKG_VERSION"),
            "epoch": 1,
            "capabilities": {
                "graph": true,
                "status": true,
                "writeOps": true,
                "undo": true,
                "notifications": true,
            },
        }))
    }

    async fn get_any_session(&self) -> Result<Arc<Session>, WorkbenchError> {
        let sessions = self.sessions.lock().await;
        sessions
            .values()
            .next()
            .cloned()
            .ok_or_else(|| WorkbenchError::not_initialized("no session; call initialize first"))
    }

    async fn dispatch_with_session(
        &self,
        session: Arc<Session>,
        method: &str,
        params: Value,
    ) -> Result<Value, WorkbenchError> {
        match method {
            "getGraph" => {
                let viewport = parse_graph_viewport(&params)?;
                let page = crate::gix_read::read_graph_page(&session, &viewport)
                    .map_err(|e| {
                        // Preserve structured errors (e.g. EPOCH_MISMATCH).
                        e.downcast::<WorkbenchError>()
                            .unwrap_or_else(|e| WorkbenchError::git_error(e.to_string()))
                    })?;
                Ok(serde_json::to_value(page).unwrap())
            }
            "getStatus" => {
                // Optional path filter from params.
                let paths: Option<Vec<String>> = params
                    .get("paths")
                    .and_then(|p| p.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.as_str().map(|s| s.to_string()))
                            .collect()
                    });
                let items = crate::gix_read::read_status(&session, paths.as_deref())
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(serde_json::to_value(items).unwrap())
            }
            "stage" => {
                let paths = parse_paths(&params)?;
                session
                    .enqueue_write(WriteCommand::Stage(paths.clone()))
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "unstage" => {
                let paths = parse_paths(&params)?;
                session
                    .enqueue_write(WriteCommand::Unstage(paths))
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "commit" => {
                let message = params
                    .get("message")
                    .and_then(|m| m.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("commit: message required"))?
                    .to_string();
                let amend = params
                    .get("amend")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(false);
                session
                    .enqueue_write(WriteCommand::Commit { message, amend })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "createBranch" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("createBranch: name required"))?
                    .to_string();
                let base = params
                    .get("base")
                    .and_then(|b| b.as_str())
                    .map(|b| b.to_string());
                session
                    .enqueue_write(WriteCommand::CreateBranch { name, base })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "deleteBranch" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("deleteBranch: name required"))?
                    .to_string();
                let force = params
                    .get("force")
                    .and_then(|f| f.as_bool())
                    .unwrap_or(false);
                session
                    .enqueue_write(WriteCommand::DeleteBranch { name, force })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "switchBranch" => {
                let name = params
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("switchBranch: name required"))?
                    .to_string();
                let auto_stash = params
                    .get("auto_stash")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(true);
                let output = session
                    .enqueue_write(WriteCommand::SwitchBranch { name, auto_stash })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(json!({ "output": output }))
            }
            "fetch" => {
                let remote = params
                    .get("remote")
                    .and_then(|r| r.as_str())
                    .map(|r| r.to_string());
                let output = session
                    .enqueue_write(WriteCommand::Fetch { remote })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(json!({ "output": output }))
            }
            "pull" => {
                let remote = params
                    .get("remote")
                    .and_then(|r| r.as_str())
                    .map(|r| r.to_string());
                let branch = params
                    .get("branch")
                    .and_then(|b| b.as_str())
                    .map(|b| b.to_string());
                let output = session
                    .enqueue_write(WriteCommand::Pull { remote, branch })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(json!({ "output": output }))
            }
            "push" => {
                let remote = params
                    .get("remote")
                    .and_then(|r| r.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("push: remote required"))?
                    .to_string();
                let branch = params
                    .get("branch")
                    .and_then(|b| b.as_str())
                    .ok_or_else(|| WorkbenchError::invalid_argument("push: branch required"))?
                    .to_string();
                let force = params
                    .get("force")
                    .and_then(|f| f.as_bool())
                    .unwrap_or(false);
                session
                    .enqueue_write(WriteCommand::Push {
                        remote,
                        branch,
                        force,
                    })
                    .await
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(Value::Null)
            }
            "listUndoStack" => {
                let limit = params
                    .get("limit")
                    .and_then(|l| l.as_u64())
                    .unwrap_or(50) as u32;
                let entries = session
                    .undo
                    .list_transactions(limit)
                    .map_err(|e| WorkbenchError::git_error(e.to_string()))?;
                Ok(serde_json::to_value(entries).unwrap())
            }
            "undo" => {
                let transaction_id = params
                    .get("transaction_id")
                    .and_then(|t| t.as_u64())
                    .ok_or_else(|| {
                        WorkbenchError::invalid_argument("undo: transaction_id required")
                    })?;

                // Run undo on a blocking thread (it shells out to git).
                let undo_engine = session.clone();
                let result = tokio::task::spawn_blocking(move || {
                    undo_engine.undo.undo(transaction_id)
                })
                .await
                .map_err(|e| WorkbenchError::git_error(format!("undo task failed: {e}")))?;

                match result {
                    Ok(crate::undo::UndoOutcome::Restored { restored_refs }) => {
                        let epoch = session.bump_epoch();
                        Ok(json!({
                            "restored_refs": restored_refs,
                            "epoch": epoch,
                        }))
                    }
                    Ok(crate::undo::UndoOutcome::Noop) => {
                        let epoch = session.bump_epoch();
                        Ok(json!({ "restored_refs": 0, "epoch": epoch }))
                    }
                    Err(crate::undo::UndoError::NotFound) => Err(WorkbenchError::invalid_argument(
                        format!("undo: transaction {transaction_id} not found"),
                    )),
                    Err(crate::undo::UndoError::Blocked { reason }) => {
                        Err(WorkbenchError::undo_blocked(reason))
                    }
                    Err(crate::undo::UndoError::Io(e)) | Err(crate::undo::UndoError::Git(e)) => {
                        Err(WorkbenchError::git_error(format!("{e}")))
                    }
                }
            }
            _ => Err(WorkbenchError::invalid_argument(format!(
                "unknown method: {method}"
            ))),
        }
    }
}

fn parse_graph_viewport(params: &Value) -> Result<GraphViewport, WorkbenchError> {
    let viewport = params
        .get("viewport")
        .ok_or_else(|| WorkbenchError::invalid_argument("getGraph: viewport required"))?;
    Ok(GraphViewport {
        offset: viewport
            .get("offset")
            .and_then(|o| o.as_u64())
            .unwrap_or(0),
        limit: viewport
            .get("limit")
            .and_then(|l| l.as_u64())
            .unwrap_or(100)
            .min(1000) as u32,
        anchor_commit: viewport
            .get("anchor_commit")
            .and_then(|a| a.as_str())
            .map(|a| a.to_string()),
        epoch: viewport
            .get("epoch")
            .and_then(|e| e.as_u64())
            .unwrap_or(0),
    })
}

fn parse_paths(params: &Value) -> Result<Vec<String>, WorkbenchError> {
    let paths = params
        .get("paths")
        .and_then(|p| p.as_array())
        .ok_or_else(|| WorkbenchError::invalid_argument("paths (array) required"))?;
    let paths = paths
        .iter()
        .filter_map(|p| p.as_str().map(|s| s.to_string()))
        .collect();
    Ok(paths)
}

fn write_jsonrpc(out: &mut impl Write, value: &Value) {
    let body = serde_json::to_string(value).unwrap();
    if write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body).is_ok() {
        let _ = out.flush();
    }
}

/// Read one Content-Length-framed JSON-RPC message. Returns Ok(None) on EOF.
fn read_message(reader: &mut impl BufRead) -> anyhow::Result<Option<(Value, String, Value)>> {
    // Parse headers.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // End of headers.
        }
        if let Some(len) = line.strip_prefix("Content-Length: ") {
            content_length = Some(len.trim().parse()?);
        }
    }

    let content_length =
        content_length.ok_or_else(|| anyhow::anyhow!("missing Content-Length header"))?;
    let mut buf = vec![0u8; content_length];
    std::io::Read::read_exact(reader, &mut buf)?;

    let value: Value = serde_json::from_slice(&buf)?;
    let method = value
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| anyhow::anyhow!("message missing method"))?
        .to_string();
    let params = value.get("params").cloned().unwrap_or(Value::Null);
    Ok(Some((
        value.get("id").cloned().unwrap_or(Value::Null),
        method,
        params,
    )))
}
