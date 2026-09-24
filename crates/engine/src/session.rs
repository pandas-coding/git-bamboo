use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::Watcher;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::cache::Cache;
use crate::gitdir;
use crate::undo::UndoEngine;
use crate::write_queue::{run_write_queue, WriteCommand, WriteResult};
use crate::undo::{UndoErrorKind, UndoSummary};

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

/// Cap on the number of events coalesced into one watcher batch; a longer
/// burst is drained into subsequent batches instead of growing unboundedly.
const MAX_EVENTS_PER_BATCH: usize = 4096;

/// Debounce window for coalescing fs events (a single git command writes
/// many .git files).
const EVENT_DEBOUNCE: Duration = Duration::from_millis(50);

pub struct Session {
    pub repo_path: PathBuf,
    /// The resolved git directory (differs from `repo_path/.git` for linked
    /// worktrees, where `.git` is a file).
    pub git_dir: PathBuf,
    /// Invalidation counter. Starts from the epoch durably persisted in the
    /// redb cache (surviving engine restarts) and is bumped on every
    /// observed repo mutation.
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
        // Resolve the git directory and the repo fingerprint on a blocking
        // thread (both shell out to git).
        let repo_path = repo_path.to_path_buf();
        let (git_dir, fingerprint) = {
            let repo_path = repo_path.clone();
            tokio::task::spawn_blocking(move || {
                let git_dir = gitdir::resolve_git_dir(&repo_path);
                let fingerprint = gitdir::repo_fingerprint(&repo_path);
                (git_dir, fingerprint)
            })
            .await?
        };

        let gix_repo = gix::open(&repo_path)?;
        // The durable epoch is reconciled with the repo fingerprint inside
        // Cache::open (clears stale COMMIT_META/lanes and bumps the epoch
        // when refs moved while the engine was down).
        let (cache, epoch) = Cache::open(&git_dir, &fingerprint)?;
        let undo = UndoEngine::new(&repo_path, &git_dir)?;

        let (write_tx, write_rx) = mpsc::channel::<(
            WriteCommand,
            tokio::sync::oneshot::Sender<WriteResult>,
        )>(32);

        let session = Arc::new(Self {
            repo_path: repo_path.clone(),
            git_dir,
            epoch: AtomicU64::new(epoch),
            gix_repo: Mutex::new(gix_repo),
            cache: Mutex::new(cache),
            undo,
            write_tx,
            notify_tx,
            watcher: Mutex::new(None),
        });

        // Start the single-consumer write queue. It only holds a Weak
        // reference so that replacing the session (re-initialize) drops
        // the old session instead of leaking it forever.
        let weak = Arc::downgrade(&session);
        tokio::spawn(async move {
            run_write_queue(write_rx, weak).await;
        });

        // Start the fs watcher.
        session.start_fs_watcher();

        info!(repo_path = %repo_path.display(), epoch, "session opened");
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
            WriteResult::Undo(r) => r
                .map(|_| String::new())
                .map_err(|e| anyhow::anyhow!("{e}")),
        }
    }

    /// Enqueue a cascade undo through the same single-consumer write queue,
    /// so undo is serialized against all other mutations.
    pub async fn enqueue_undo(
        &self,
        transaction_id: u64,
    ) -> anyhow::Result<Result<UndoSummary, UndoErrorKind>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.write_tx
            .send((WriteCommand::Undo { transaction_id }, tx))
            .await
            .map_err(|_| anyhow::anyhow!("write queue closed"))?;
        let result = rx.await.map_err(|_| anyhow::anyhow!("write result dropped"))?;
        match result {
            WriteResult::Undo(r) => Ok(r),
            WriteResult::Ok(_) => Err(anyhow::anyhow!(
                "internal error: undo command returned a write result"
            )),
            WriteResult::Err(e) => Err(anyhow::anyhow!("{}", e)),
        }
    }

    pub fn emit(&self, event: EngineEvent) {
        if let Err(e) = self.notify_tx.send(event) {
            warn!(error = %e, "notification channel closed");
        }
    }

    fn start_fs_watcher(self: &Arc<Self>) {
        let git_dir = self.git_dir.clone();
        let git_dir_inside = gitdir::git_dir_inside_worktree(&git_dir, &self.repo_path);

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "failed to create fs watcher; falling back to no watching");
                return;
            }
        };

        let mut watched_any = false;
        if git_dir_inside {
            // Normal repository: a recursive watch of the worktree root
            // covers both the worktree and the gitdir; unwanted .git noise
            // (index.lock churn, pack files, ...) is filtered out when
            // classifying events.
            match watcher.watch(&self.repo_path, notify::RecursiveMode::Recursive) {
                Ok(()) => watched_any = true,
                Err(e) => warn!(error = %e, "failed to watch worktree recursively"),
            }
        } else {
            // Linked worktree (`.git` is a file pointing elsewhere): watch
            // the worktree for content changes and the resolved gitdir's
            // ref/HEAD files explicitly.
            match watcher.watch(&self.repo_path, notify::RecursiveMode::Recursive) {
                Ok(()) => watched_any = true,
                Err(e) => warn!(error = %e, "failed to watch worktree recursively; skipping worktree watching"),
            }
        }
        // Always watch the ref/HEAD gitdir entries explicitly (this also
        // covers linked worktrees whose gitdir is outside the worktree —
        // including their per-worktree index — and adds robustness when a
        // recursive watch of the worktree failed).
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
        let refs_dir = git_dir.join("refs");
        if refs_dir.exists() {
            match watcher.watch(&refs_dir, notify::RecursiveMode::Recursive) {
                Ok(()) => watched_any = true,
                Err(e) => warn!(path = %refs_dir.display(), error = %e, "failed to watch"),
            }
        }

        if !watched_any {
            warn!("fs watcher active but watching nothing");
        }

        // The watcher thread holds only a Weak handle: when the session is
        // replaced (re-initialize) it exits instead of keeping the session
        // alive forever.
        let weak = Arc::downgrade(self);
        let repo_path = self.repo_path.clone();
        std::thread::Builder::new()
            .name("git-workbench-watcher".into())
            .spawn(move || {
                while let Ok(event) = rx.recv() {
                    let Ok(event) = event else { continue };
                    let mut batch = vec![event];
                    // Coalesce events arriving within the debounce window
                    // into one update cycle, capped so a burst cannot grow
                    // the batch unboundedly (excess events are processed in
                    // the next batch).
                    while batch.len() < MAX_EVENTS_PER_BATCH {
                        match rx.recv_timeout(EVENT_DEBOUNCE) {
                            Ok(event) => {
                                let Ok(event) = event else { continue };
                                batch.push(event);
                            }
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                // Drain anything left, then exit.
                                while let Ok(event) = rx.try_recv() {
                                    if batch.len() < MAX_EVENTS_PER_BATCH {
                                        if let Ok(event) = event {
                                            batch.push(event);
                                        }
                                    }
                                }
                                break;
                            }
                        }
                    }
                    let Some(session) = weak.upgrade() else { break };
                    process_event_batch(&session, &repo_path, &git_dir, batch);
                }
                debug!("fs watcher thread exiting");
            })
            .ok();

        *self
            .watcher
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(watcher);
    }
}

/// Classify and emit notifications for a coalesced batch of fs events.
/// A single epoch bump covers the whole batch so clients refresh once.
fn process_event_batch(
    session: &Session,
    repo_path: &Path,
    git_dir: &Path,
    events: Vec<notify::Event>,
) {
    let mut head_changed = false;
    let mut index_changed = false;
    let mut graph_invalidated = false;
    let mut changed_refs: Vec<String> = Vec::new();
    let mut worktree_paths: Vec<String> = Vec::new();

    for event in events {
        // Ignore pure-access events (open/read/close): the engine itself
        // reads .git/HEAD when building headChanged notifications, and
        // feeding those events back would loop forever (read → notify →
        // read). Only actual mutations are interesting.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            continue;
        }
        for path in event.paths {
            // Skip transient editor temp files.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if (name.starts_with('.') && name.ends_with(".swp"))
                || name.ends_with('~')
                || name.starts_with('#')
            {
                continue;
            }

            if path.starts_with(git_dir) {
                let rel = path.strip_prefix(git_dir).unwrap_or(&path);
                let rel_str = rel.to_string_lossy();

                // Skip lock files and other transient gitdir noise
                // (index.lock, refs/*.lock, config.lock, ...): they churn
                // constantly and never represent a durable state change.
                if rel_str.ends_with(".lock") {
                    continue;
                }

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
                // Other gitdir internals (objects, packs, ...) are ignored.
            } else if path.starts_with(repo_path) {
                // Worktree change: refresh status.
                let rel = path.strip_prefix(repo_path).unwrap_or(&path);
                worktree_paths.push(rel.to_string_lossy().to_string());
            } else {
                // Neither worktree nor gitdir (can happen with odd watcher
                // backends): ignore.
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
