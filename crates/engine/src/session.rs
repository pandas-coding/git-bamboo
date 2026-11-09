use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use notify::Watcher;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use bamboo_protocol::BambooError;

use crate::cache::Cache;
use crate::gitdir;
use crate::undo::UndoEngine;
use crate::undo::{UndoErrorKind, UndoSummary};
use crate::write_queue::{run_write_queue, WriteCommand, WriteResult};

/// The result of one write command, crossing the session boundary with
/// its structure intact: plain commands return an output string,
/// interactive-rebase commands return the typed outcome (a stop is a
/// normal result, not an error).
#[derive(Debug, Clone)]
pub enum WriteOutcome {
    Output(String),
    Rebase(bamboo_protocol::RebaseResult),
}

impl WriteOutcome {
    /// The plain output string; panics for rebase commands (tests and
    /// RPC code that must not silently drop a structured rebase result).
    pub fn expect_output(self) -> String {
        match self {
            WriteOutcome::Output(output) => output,
            WriteOutcome::Rebase(_) => {
                panic!("expected a plain write output, got a rebase result")
            }
        }
    }
}

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

/// Poll interval of the watcher re-arm thread while the repo root is gone
/// (rm -rf + recreate): inotify watches are inode-based and do not survive
/// the recreate, so the session waits for the paths to reappear.
const REARM_POLL: Duration = Duration::from_millis(250);

/// How often the watcher thread checks up on the watched roots when no events
/// arrive. Event-driven detection is not enough on its own: Windows delivers
/// nothing at all once the watched directory is gone — notify's backend takes
/// `ERROR_ACCESS_DENIED`, unwatches the directory and reports no event — so the
/// thread has to go and look. It checks both whether the roots still exist and
/// whether they are still the SAME directories (RootStamp), because a scripted
/// delete-and-recreate finishes inside one interval and existence alone misses
/// it.
const ROOT_CHECK_INTERVAL: Duration = Duration::from_millis(500);

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
    /// Memoized filtered graph views (see `graph_filter`), keyed by
    /// filter and invalidated wholesale when the epoch or tip set moves.
    pub filtered_views: Mutex<Option<crate::graph_filter::FilteredViews>>,
    pub undo: UndoEngine,
    pub write_tx: mpsc::Sender<(WriteCommand, tokio::sync::oneshot::Sender<WriteResult>)>,
    /// The undo transaction id of the in-flight interactive rebase, if
    /// any: startRebase is journaled once; the continue/skip/abort steps
    /// that advance that SAME rebase bypass the journal and instead
    /// refresh this tx's post snapshot when the rebase finishes (see
    /// write_queue::rebase_step / undo::refresh_post_snapshot). Also
    /// survives a stopped rebase so a later undo of the tx can
    /// `git rebase --abort` + restore.
    rebase_tx: std::sync::Mutex<Option<u64>>,
    notify_tx: mpsc::UnboundedSender<EngineEvent>,
    watcher: Mutex<Option<notify::RecommendedWatcher>>,
    /// True while a re-arm thread is waiting for a deleted repo root to
    /// reappear (guards against spawning more than one).
    rearm_pending: AtomicBool,
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

        let (write_tx, write_rx) =
            mpsc::channel::<(WriteCommand, tokio::sync::oneshot::Sender<WriteResult>)>(32);

        let session = Arc::new(Self {
            repo_path: repo_path.clone(),
            git_dir,
            epoch: AtomicU64::new(epoch),
            gix_repo: Mutex::new(gix_repo),
            cache: Mutex::new(cache),
            filtered_views: Mutex::new(None),
            undo,
            write_tx,
            rebase_tx: std::sync::Mutex::new(None),
            notify_tx,
            watcher: Mutex::new(None),
            rearm_pending: AtomicBool::new(false),
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

    /// Bind the undo transaction that owns the in-flight rebase (set by
    /// run_write right after journaling StartRebase).
    pub fn set_rebase_tx(&self, id: u64) {
        *self.rebase_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    /// Release the binding (the rebase finished/was aborted/failed).
    pub fn take_rebase_tx(&self) -> Option<u64> {
        self.rebase_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The bound rebase transaction, if the rebase is still in flight.
    pub fn current_rebase_tx(&self) -> Option<u64> {
        *self.rebase_tx.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Enqueue a write command. Errors are typed `BambooError`s so
    /// structured codes (STASH_CONFLICT, PUSH_REJECTED, ...) reach the
    /// RPC surface intact.
    pub async fn enqueue_write(&self, cmd: WriteCommand) -> Result<WriteOutcome, BambooError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.write_tx
            .send((cmd, tx))
            .await
            .map_err(|_| BambooError::git_error("write queue closed"))?;
        let result = rx
            .await
            .map_err(|_| BambooError::git_error("write result dropped"))?;
        match result {
            WriteResult::Ok(output) => Ok(WriteOutcome::Output(output)),
            WriteResult::Rebase(r) => Ok(WriteOutcome::Rebase(r)),
            WriteResult::Err(e) => Err(e),
            WriteResult::Undo(r) => r
                .map(|_| WriteOutcome::Output(String::new()))
                .map_err(|e| BambooError::git_error(e.to_string())),
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
        let result = rx
            .await
            .map_err(|_| anyhow::anyhow!("write result dropped"))?;
        match result {
            WriteResult::Undo(r) => Ok(r),
            WriteResult::Ok(_) => Err(anyhow::anyhow!(
                "internal error: undo command returned a write result"
            )),
            WriteResult::Rebase(_) => Err(anyhow::anyhow!(
                "internal error: undo command returned a rebase result"
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
                Err(e) => {
                    warn!(error = %e, "failed to watch worktree recursively; skipping worktree watching")
                }
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
        // Event paths are matched against both the form we watched and its
        // symlink-resolved form — macOS reports only the latter (WatchRoot).
        let repo_root = WatchRoot::new(&self.repo_path);
        let git_root = WatchRoot::new(&git_dir);
        std::thread::Builder::new()
            .name("bamboo-watcher".into())
            .spawn(move || {
                // The roots' identity as of arming, so a replacement can be
                // recognised later even though the paths look unchanged.
                let armed = RootStamp::of(&repo_root.given, &git_root.given);
                let mut pending_stamp: Option<RootStamp> = None;
                loop {
                    // A heartbeat rather than a blocking recv: see
                    // ROOT_CHECK_INTERVAL. A deleted or replaced repo must be
                    // noticed even when the backend has stopped reporting.
                    let event = match rx.recv_timeout(ROOT_CHECK_INTERVAL) {
                        Ok(event) => event,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            let Some(session) = weak.upgrade() else { break };
                            if roots_gone(&repo_root, &git_root) {
                                pending_stamp = None;
                                session.schedule_watcher_rearm();
                            } else {
                                // Gone-and-back: the paths exist but are not
                                // the directories we armed on. Require the new
                                // stamp to survive one more beat before acting,
                                // so a filesystem that reports an unstable
                                // creation time cannot start a re-arm loop.
                                let now = RootStamp::of(&repo_root.given, &git_root.given);
                                if now == armed {
                                    pending_stamp = None;
                                } else if pending_stamp == Some(now) {
                                    pending_stamp = None;
                                    session.schedule_watcher_rearm();
                                } else {
                                    pending_stamp = Some(now);
                                }
                            }
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };
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
                    // Deletion of a watched root itself (rm -rf of the repo or
                    // its gitdir) kills inode-based watches, so a recreate has
                    // to re-arm. Event kinds alone are not enough: inotify
                    // reports Remove on the root, macOS' FSEvents reports the
                    // same deletion as ROOT_CHANGED — which notify surfaces as
                    // Modify(Name(From)) — and Windows may report nothing at
                    // all (the heartbeat above covers that). A recreate inside
                    // the 50ms debounce window can also put the path back
                    // before the batch is classified, hence both signals. A
                    // false positive costs one conservative refresh burst: the
                    // re-arm thread waits for the paths to reappear.
                    let root_event = batch.iter().any(|ev| {
                        matches!(
                            ev.kind,
                            notify::EventKind::Remove(_)
                                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) && ev
                            .paths
                            .iter()
                            .any(|p| repo_root.is_self(p) || git_root.is_self(p))
                    });
                    let root_gone = roots_gone(&repo_root, &git_root);
                    process_event_batch(&session, &repo_root, &git_root, batch);
                    if root_event || root_gone {
                        session.schedule_watcher_rearm();
                    }
                }
                debug!("fs watcher thread exiting");
            })
            .ok();

        *self.watcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(watcher);
    }

    /// Re-arm the fs watcher after the repo root (or gitdir) was deleted.
    /// A polling thread waits for the paths to reappear, replaces the dead
    /// watcher, and emits a conservative full-refresh notification burst:
    /// the recreate happened while no watcher was live, so clients must
    /// assume everything changed. Exits silently when the session is
    /// replaced (re-initialize) or dropped.
    fn schedule_watcher_rearm(self: &Arc<Self>) {
        if self.rearm_pending.swap(true, Ordering::SeqCst) {
            return;
        }
        info!(
            path = %self.repo_path.display(),
            "repo root removed; scheduling fs watcher re-arm"
        );
        let weak = Arc::downgrade(self);
        let repo_path = self.repo_path.clone();
        let git_dir = self.git_dir.clone();
        std::thread::Builder::new()
            .name("bamboo-watcher-rearm".into())
            .spawn(move || loop {
                let Some(session) = weak.upgrade() else {
                    return;
                };
                if repo_path.is_dir() && git_dir.exists() {
                    session.start_fs_watcher();
                    session.rearm_pending.store(false, Ordering::SeqCst);
                    let epoch = session.bump_epoch();
                    let new_head = std::fs::read_to_string(git_dir.join("HEAD"))
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    session.emit(EngineEvent::HeadChanged { new_head, epoch });
                    session.emit(EngineEvent::RefsChanged {
                        changed_refs: Vec::new(),
                        epoch,
                    });
                    session.emit(EngineEvent::IndexChanged { epoch });
                    session.emit(EngineEvent::GraphInvalidated { epoch });
                    session.emit(EngineEvent::WorktreeChanged { paths: Vec::new() });
                    debug!(path = %repo_path.display(), "fs watcher re-armed after repo recreate");
                    return;
                }
                std::thread::sleep(REARM_POLL);
            })
            .ok();
    }
}

/// A watched root, in the form we were handed and in its symlink-resolved
/// form.
///
/// The two differ more often than it looks. macOS' FSEvents backend hands
/// notify the REAL path — a repo opened as `/var/tmp/repo` reports events
/// under `/private/var/tmp/repo`, because `/var` is a symlink — and
/// `git rev-parse --absolute-git-dir` answers with the physical path too,
/// while inotify and ReadDirectoryChangesW echo back the watched string.
/// Comparing an event path against only the form the client gave us therefore
/// drops every worktree event on macOS whenever the path traverses a symlink,
/// silently: the UI just stops refreshing. Both forms are matched instead.
///
/// `real` is None when canonicalization fails or resolves to the same path,
/// so the common case costs one failed comparison and nothing else.
#[derive(Debug)]
struct WatchRoot {
    given: PathBuf,
    real: Option<PathBuf>,
}

impl WatchRoot {
    fn new(path: &Path) -> Self {
        let real = std::fs::canonicalize(path).ok().filter(|real| real != path);
        Self {
            given: path.to_path_buf(),
            real,
        }
    }

    /// `path` relative to this root, or None when it is not inside it — in
    /// either form. The relative result is the same for both, so what the
    /// client sees does not depend on which one matched.
    fn relative<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        if let Ok(rel) = path.strip_prefix(&self.given) {
            return Some(rel);
        }
        match &self.real {
            Some(real) => path.strip_prefix(real).ok(),
            None => None,
        }
    }

    /// `path` IS this root, in either form — the case that matters when the
    /// root itself is deleted and the watches die with it.
    fn is_self(&self, path: &Path) -> bool {
        path == self.given || self.real.as_deref() == Some(path)
    }
}

/// Whether either watched root has vanished from the filesystem — the
/// backend-independent half of re-arm detection.
fn roots_gone(repo_root: &WatchRoot, git_root: &WatchRoot) -> bool {
    !repo_root.given.is_dir() || !git_root.given.exists()
}

/// The filesystem identity of the watched roots: their creation times.
///
/// Existence is not enough to notice a replacement, because a scripted
/// `rm -rf repo && mkdir repo && git init` finishes inside one poll interval —
/// and on Windows there is no event to react to either, since notify unwatches
/// a directory whose handle starts returning `ERROR_ACCESS_DENIED` without
/// reporting anything. A creation time changes PERMANENTLY when the directory
/// is replaced, so the next heartbeat sees it no matter how brief the gap was.
///
/// `None` where the filesystem exposes no creation time (statx without btime,
/// for instance). Both sides are then `None`, the stamps compare equal, and
/// this check stays inert — the event- and existence-based detection still
/// carry those platforms, as they always did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RootStamp {
    repo: Option<SystemTime>,
    git: Option<SystemTime>,
}

impl RootStamp {
    fn of(repo_root: &Path, git_root: &Path) -> Self {
        Self {
            repo: created_at(repo_root),
            git: created_at(git_root),
        }
    }
}

fn created_at(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.created()).ok()
}

/// Classify and emit notifications for a coalesced batch of fs events.
/// A single epoch bump covers the whole batch so clients refresh once.
fn process_event_batch(
    session: &Session,
    repo_root: &WatchRoot,
    git_root: &WatchRoot,
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

            if let Some(rel) = git_root.relative(&path) {
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
            } else if let Some(rel) = repo_root.relative(&path) {
                // Worktree change: refresh status.
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
        let new_head = std::fs::read_to_string(git_root.given.join("HEAD"))
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

#[cfg(test)]
mod tests {
    use super::{Duration, RootStamp, WatchRoot};
    use std::path::{Path, PathBuf};

    /// The macOS shape: watched as `/var/...`, events reported as
    /// `/private/var/...`. Both must classify, and both must yield the same
    /// relative path — that string is what the client receives.
    #[test]
    fn watch_root_matches_the_symlink_resolved_form() {
        let root = WatchRoot {
            given: PathBuf::from("/var/tmp/repo"),
            real: Some(PathBuf::from("/private/var/tmp/repo")),
        };
        for reported in ["/var/tmp/repo/a.txt", "/private/var/tmp/repo/a.txt"] {
            assert_eq!(
                root.relative(Path::new(reported)),
                Some(Path::new("a.txt")),
                "{reported}"
            );
        }
        assert!(root.is_self(Path::new("/var/tmp/repo")));
        assert!(root.is_self(Path::new("/private/var/tmp/repo")));
        // Sharing a prefix is not being inside: `/var/tmp/repo2` is a sibling.
        assert_eq!(root.relative(Path::new("/var/tmp/repo2/a.txt")), None);
        assert!(!root.is_self(Path::new("/var/tmp/repo2")));
        assert!(!root.is_self(Path::new("/private/var/tmp/repo/.git")));
    }

    /// Without an alias only the given form matches — the alias must widen
    /// matching, not replace it.
    #[test]
    fn watch_root_without_an_alias_matches_only_what_it_was_given() {
        let root = WatchRoot {
            given: PathBuf::from("/home/u/repo"),
            real: None,
        };
        assert_eq!(
            root.relative(Path::new("/home/u/repo/.git/HEAD")),
            Some(Path::new(".git/HEAD"))
        );
        assert_eq!(root.relative(Path::new("/private/home/u/repo/x")), None);
        // The root itself relative to itself is empty, as strip_prefix gives.
        assert_eq!(
            root.relative(Path::new("/home/u/repo")),
            Some(Path::new(""))
        );
    }

    /// `new` against a real directory: whatever the platform does with
    /// symlinks, the canonical form of the root must match. On macOS
    /// `tempdir()` is under `/var` and this exercises the alias; on Linux the
    /// two forms coincide and it exercises the dedupe.
    #[test]
    fn watch_root_new_resolves_a_real_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = WatchRoot::new(dir.path());
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        assert!(root.is_self(dir.path()));
        assert!(root.is_self(&canonical));
        assert_eq!(
            root.relative(&canonical.join("a.txt")),
            Some(Path::new("a.txt"))
        );
        assert_eq!(
            root.relative(&dir.path().join("a.txt")),
            Some(Path::new("a.txt"))
        );
    }

    /// A recreated root must not look like the one we armed on. That is the
    /// whole job of the stamp: it is what lets the re-arm notice a
    /// delete-and-recreate that finished inside a single poll interval, which
    /// is how it actually happens (`rm -rf repo && mkdir repo && git init`).
    /// Where the filesystem reports no creation time both sides are None and
    /// there is nothing to distinguish — the check is inert there by design.
    #[test]
    fn root_stamp_changes_when_a_directory_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let git = root.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let before = RootStamp::of(&root, &git);

        std::fs::remove_dir_all(&root).unwrap();
        // Longer than any plausible creation-time granularity on the
        // filesystems this runs on, so the test does not hinge on the
        // replacement landing in a different clock tick by luck.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::create_dir_all(&git).unwrap();
        let after = RootStamp::of(&root, &git);

        if before.repo.is_some() || before.git.is_some() {
            assert_ne!(
                before, after,
                "a replaced root must have a new stamp, or the watcher stays \
                 armed on a directory that no longer exists"
            );
        } else {
            assert_eq!(after, before, "no creation time means no stamp churn");
        }
    }
}
