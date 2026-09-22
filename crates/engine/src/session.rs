use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::Watcher;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::undo::UndoEngine;
use crate::write_queue::{run_write_queue, WriteCommand, WriteResult};

/// Events emitted by the engine to be forwarded to connected clients
/// as JSON-RPC notifications.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    RefsChanged {
        changed_refs: Vec<String>,
        epoch: u64,
    },
    WorktreeChanged {
        paths: Vec<String>,
    },
    IndexChanged {
        epoch: u64,
    },
    HeadChanged {
        new_head: String,
        epoch: u64,
    },
    GraphInvalidated {
        epoch: u64,
    },
}

pub struct Session {
    pub repo_path: PathBuf,
    pub epoch: AtomicU64,
    pub gix_repo: Mutex<gix::Repository>,
    pub cache: Mutex<Cache>,
    pub undo: UndoEngine,
    pub write_tx: mpsc::Sender<(WriteCommand, tokio::sync::oneshot::Sender<WriteResult>)>,
    notify_tx: mpsc::UnboundedSender<EngineEvent>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl Session {
    pub async fn open(
        repo_path: &Path,
        notify_tx: mpsc::UnboundedSender<EngineEvent>,
    ) -> anyhow::Result<Arc<Self>> {
        let gix_repo = gix::open(repo_path)?;
        let cache = Cache::open(repo_path)?;
        let undo = UndoEngine::new(repo_path)?;

        let (write_tx, write_rx) = mpsc::channel::<(
            WriteCommand,
            tokio::sync::oneshot::Sender<WriteResult>,
        )>(32);

        let session = Arc::new(Self {
            repo_path: repo_path.to_path_buf(),
            epoch: AtomicU64::new(1),
            gix_repo: Mutex::new(gix_repo),
            cache: Mutex::new(cache),
            undo,
            write_tx,
            notify_tx,
            watcher: Mutex::new(None),
        });

        // Start the single-consumer write queue.
        let session_clone = Arc::clone(&session);
        tokio::spawn(async move {
            run_write_queue(write_rx, session_clone).await;
        });

        // Start the fs watcher.
        session.start_fs_watcher();

        info!(repo_path = %repo_path.display(), "session opened");
        Ok(session)
    }

    pub fn bump_epoch(&self) -> u64 {
        let new = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        debug!(epoch = new, "epoch bumped");
        new
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    pub async fn enqueue_write(&self, cmd: WriteCommand) -> anyhow::Result<String> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.write_tx
            .send((cmd, tx))
            .await
            .map_err(|_| anyhow::anyhow!("write queue closed"))?;
        let result = rx.await.map_err(|_| anyhow::anyhow!("write result dropped"))?;
        match result {
            WriteResult::Ok(output) => Ok(output),
            WriteResult::Err(e) => Err(anyhow::anyhow!("{}", e)),
        }
    }

    pub fn emit(&self, event: EngineEvent) {
        if let Err(e) = self.notify_tx.send(event) {
            warn!(error = %e, "notification channel closed");
        }
    }

    fn start_fs_watcher(self: &Arc<Self>) {
        let git_dir = self.repo_path.join(".git");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "failed to create fs watcher; falling back to no watching");
                return;
            }
        };

        // Watch .git internals (non-recursive on .git itself; refs recursively).
        let mut watched_any = false;
        for path in [
            git_dir.join("HEAD"),
            git_dir.join("index"),
            git_dir.join("packed-refs"),
        ] {
            if path.exists() {
                match watcher.watch(&path, notify::RecursiveMode::NonRecursive) {
                    Ok(()) => watched_any = true,
                    Err(e) => warn!(path = %path.display(), error = %e, "failed to watch"),
                }
            }
        }
        // Loose refs and sequencer state.
        for dir in [git_dir.join("refs"), git_dir.join("sequencer")] {
            if dir.exists() {
                match watcher.watch(&dir, notify::RecursiveMode::Recursive) {
                    Ok(()) => watched_any = true,
                    Err(e) => warn!(path = %dir.display(), error = %e, "failed to watch"),
                }
            }
        }

        // Watch the worktree for content changes (filtered: .git internals
        // other than the paths above are ignored by the handler).
        match watcher.watch(&self.repo_path, notify::RecursiveMode::Recursive) {
            Ok(()) => {
                watched_any = true;
            }
            Err(e) => {
                warn!(error = %e, "failed to watch worktree recursively; skipping worktree watching");
            }
        }

        if !watched_any {
            warn!("fs watcher active but watching nothing");
        }

        let weak = Arc::downgrade(self);
        let repo_path = self.repo_path.clone();
        std::thread::Builder::new()
            .name("git-workbench-watcher".into())
            .spawn(move || {
                loop {
                    let event = match rx.recv() {
                        Ok(event) => event,
                        Err(_) => break,
                    };
                    let Ok(event) = event else { continue };
                    let mut batch = vec![event];
                    // Coalesce events arriving within 50ms into one update
                    // cycle (a single git command writes many .git files).
                    while let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
                        let Ok(event) = event else { continue };
                        batch.push(event);
                    }
                    let Some(session) = weak.upgrade() else { break };
                    process_event_batch(&session, &repo_path, batch);
                }
                debug!("fs watcher thread exiting");
            })
            .ok();

        *self.watcher.lock().unwrap() = Some(watcher);
    }
}

/// Classify and emit notifications for a coalesced batch of fs events.
/// A single epoch bump covers the whole batch so clients refresh once.
fn process_event_batch(session: &Session, repo_path: &Path, events: Vec<notify::Event>) {
    let git_dir = repo_path.join(".git");

    let mut head_changed = false;
    let mut index_changed = false;
    let mut graph_invalidated = false;
    let mut changed_refs: Vec<String> = Vec::new();
    let mut worktree_paths: Vec<String> = Vec::new();

    for event in events {
        for path in event.paths {
            // Skip transient editor temp files.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if (name.starts_with('.') && name.ends_with(".swp"))
                || name.ends_with('~')
                || name.starts_with('#')
            {
                continue;
            }

            if path.starts_with(&git_dir) {
                let rel = path.strip_prefix(&git_dir).unwrap_or(&path);
                let rel_str = rel.to_string_lossy();

                if rel_str == "HEAD" {
                    head_changed = true;
                    graph_invalidated = true;
                } else if rel_str == "index" {
                    index_changed = true;
                } else if rel_str.starts_with("refs/") || rel_str == "packed-refs" {
                    changed_refs.push(rel_str.to_string());
                    graph_invalidated = true;
                } else if rel_str.starts_with("sequencer/") {
                    graph_invalidated = true;
                }
                // Other .git internals (objects, packs, ...) are ignored.
            } else {
                // Worktree change: refresh status.
                let rel = path.strip_prefix(repo_path).unwrap_or(&path);
                worktree_paths.push(rel.to_string_lossy().to_string());
            }
        }
    }

    let refs_changed = !changed_refs.is_empty();

    if !head_changed && !index_changed && !refs_changed && !graph_invalidated {
        if worktree_paths.is_empty() {
            return;
        }
        // Worktree-only change: no epoch bump (status is un-cached).
        session.emit(EngineEvent::WorktreeChanged {
            paths: worktree_paths,
        });
        return;
    }

    let epoch = session.bump_epoch();
    if head_changed {
        let new_head = std::fs::read_to_string(git_dir.join("HEAD"))
            .unwrap_or_default()
            .trim()
            .to_string();
        session.emit(EngineEvent::HeadChanged { new_head, epoch });
    }
    if refs_changed {
        session.emit(EngineEvent::RefsChanged {
            changed_refs,
            epoch,
        });
    }
    if index_changed {
        session.emit(EngineEvent::IndexChanged { epoch });
    }
    if graph_invalidated {
        session.emit(EngineEvent::GraphInvalidated { epoch });
    }
    if !worktree_paths.is_empty() {
        session.emit(EngineEvent::WorktreeChanged {
            paths: worktree_paths,
        });
    }
}
