use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use git_workbench_protocol::UndoEntry;

use crate::write_queue::WriteResult;

/// Marker file written while an undo is in progress. Crash recovery
/// (`UndoEngine::new` → `recover`) uses it to roll the undo forward:
/// the journal is truncated BEFORE any refs are restored, and the marker
/// holds the snapshot needed to (idempotently) finish the restoration.
const UNDO_MARKER: &str = "undo-in-progress.json";

/// The complete ref state of a repository at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Raw `HEAD` content: either `ref: refs/heads/<name>` or a detached sha.
    pub head: String,
    /// Local branch refs (full name → sha).
    pub refs: HashMap<String, String>,
    /// `refs/stash` target, if a stash exists (loose or packed).
    pub stash_ref: Option<String>,
    /// Tree sha of the index at snapshot time (`git write-tree`).
    /// `None` when the index could not be captured (e.g. unmerged entries).
    /// Undo restores the index from this when present.
    #[serde(default)]
    pub index_tree: Option<String>,
}

/// A journaled write transaction with pre- and post-write snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: u64,
    pub timestamp: i64,
    pub description: String,
    pub epoch_at_creation: u64,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub enum UndoOutcome {
    Restored { restored_refs: usize },
    Noop,
}

/// Serializable summary of a completed undo (crosses the write-queue channel).
#[derive(Debug, Clone)]
pub struct UndoSummary {
    pub restored_refs: usize,
    pub noop: bool,
}

/// Clone-able projection of [`UndoError`] used to carry structured undo
/// failures across the write-queue channel without cloning `anyhow::Error`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum UndoErrorKind {
    #[error("transaction not found")]
    NotFound,
    #[error("undo blocked: {reason}")]
    Blocked { reason: String },
    #[error("{msg}")]
    Failure { msg: String },
}

pub struct UndoEngine {
    journal_path: PathBuf,
    undo_dir: PathBuf,
    repo_path: PathBuf,
    /// Resolved git directory (NOT necessarily `repo_path/.git`: linked
    /// worktrees keep theirs elsewhere). All snapshot reads go through it.
    git_dir: PathBuf,
    next_id: AtomicU64,
    audit_log: PathBuf,
}

impl UndoEngine {
    pub fn new(repo_path: &Path, git_dir: &Path) -> anyhow::Result<Self> {
        let undo_dir = git_dir.join("git-workbench").join("undo");
        fs::create_dir_all(&undo_dir)?;

        let journal_path = undo_dir.join("journal.jsonl");
        let audit_log = git_dir
            .join("git-workbench")
            .join("audit.log");

        // Determine next transaction id from existing journal and snapshot
        // files (crashed `begin`s leave files without journal entries).
        let mut next_id = 1u64;
        if journal_path.exists() {
            for line in fs::read_to_string(&journal_path)?.lines() {
                if let Ok(tx) = serde_json::from_str::<Transaction>(line) {
                    next_id = next_id.max(tx.id + 1);
                }
            }
        }
        for entry in fs::read_dir(&undo_dir)? {
            let entry = entry?;
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                if let Ok(id) = stem.parse::<u64>() {
                    next_id = next_id.max(id + 1);
                }
            }
        }

        info!(next_id, "undo engine initialized");
        let engine = Self {
            journal_path,
            undo_dir,
            repo_path: repo_path.to_path_buf(),
            git_dir: git_dir.to_path_buf(),
            next_id: AtomicU64::new(next_id),
            audit_log,
        };

        // Roll forward any undo that was interrupted by a crash.
        if let Err(e) = engine.recover() {
            warn!(error = %e, "undo crash recovery failed; undo stack may be blocked");
        }
        Ok(engine)
    }

    /// Roll forward an interrupted undo. The marker records the target
    /// transaction and its pre-write snapshot; both journal truncation and
    /// snapshot restoration are idempotent, so re-running them is safe.
    fn recover(&self) -> anyhow::Result<()> {
        let marker_path = self.undo_dir.join(UNDO_MARKER);
        if !marker_path.exists() {
            return Ok(());
        }
        let data = fs::read_to_string(&marker_path)
            .with_context(|| format!("failed to read undo marker {}", marker_path.display()))?;
        let file: TransactionFile = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse undo marker {}", marker_path.display()))?;

        info!(transaction_id = file.meta.id, "recovering interrupted undo");
        self.truncate_journal_at(file.meta.id)?;
        if let Err(e) = self.restore_snapshot(&file.snapshot) {
            // Refs are already partially/fully restored; the repo may need
            // manual attention, but the journal is consistent. Log loudly.
            warn!(error = %e, "undo recovery could not finish restoring refs");
        }
        fs::remove_file(&marker_path)?;
        Ok(())
    }

    /// Begin a transaction: capture the pre-write snapshot.
    /// The journal entry is only appended once the transaction is committed,
    /// so crashed writes never appear in the undo stack.
    pub fn begin(&self, description: &str, epoch: u64) -> anyhow::Result<u64> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.read_snapshot()?;
        let meta = TransactionMeta {
            id,
            description: description.to_string(),
            epoch,
        };
        let path = self.pre_path(id);
        let data = serde_json::to_vec_pretty(&TransactionFile { meta, snapshot })?;
        write_durable(&path, &data)?;
        Ok(id)
    }

    /// Commit a transaction: capture the post-write snapshot and append the
    /// journal entry. The snapshot file is fsynced BEFORE the journal line
    /// referencing it is appended, so a journaled entry always has its
    /// snapshot on disk.
    pub fn commit(&self, id: u64, description: &str, epoch: u64, success: bool) -> anyhow::Result<()> {
        let snapshot = self.read_snapshot()?;
        let meta = TransactionMeta {
            id,
            description: description.to_string(),
            epoch,
        };
        let path = self.post_path(id);
        let data = serde_json::to_vec_pretty(&TransactionFile { meta, snapshot })?;
        write_durable(&path, &data)?;

        let tx = Transaction {
            id,
            timestamp: Utc::now().timestamp(),
            description: description.to_string(),
            epoch_at_creation: epoch,
            success,
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)?;
        writeln!(file, "{}", serde_json::to_string(&tx)?)?;
        file.sync_all()?;
        Ok(())
    }

    /// Read the current repository snapshot.
    pub fn read_snapshot(&self) -> anyhow::Result<Snapshot> {
        let git_dir = &self.git_dir;

        // Read HEAD. A missing/unreadable HEAD is a hard error: snapshots
        // must never silently record an empty HEAD.
        let head_path = git_dir.join("HEAD");
        let head = fs::read_to_string(&head_path)
            .with_context(|| format!("failed to read {}", head_path.display()))?
            .trim()
            .to_string();

        // Read local loose refs.
        let mut refs = read_loose_refs(
            &git_dir.join("refs").join("heads"),
            "refs/heads",
        )?;

        // Merge in packed refs (loose refs take precedence).
        let packed_refs = fs::read_to_string(git_dir.join("packed-refs")).ok();
        if let Some(content) = &packed_refs {
            for line in content.lines() {
                if line.starts_with('#') || line.starts_with('^') {
                    continue;
                }
                let mut parts = line.splitn(2, ' ');
                if let (Some(sha), Some(name)) = (parts.next(), parts.next()) {
                    let name = name.trim();
                    if name.starts_with("refs/heads/") && !refs.contains_key(name) {
                        refs.insert(name.to_string(), sha.to_string());
                    }
                }
            }
        }

        // Check stash (loose first, then packed).
        let mut stash = read_loose_refs(&git_dir.join("refs"), "refs")?
            .get("refs/stash")
            .cloned();
        if stash.is_none() {
            if let Some(content) = &packed_refs {
                for line in content.lines() {
                    if line.starts_with('#') || line.starts_with('^') {
                        continue;
                    }
                    let mut parts = line.splitn(2, ' ');
                    if let (Some(sha), Some(name)) = (parts.next(), parts.next()) {
                        if name.trim() == "refs/stash" {
                            stash = Some(sha.to_string());
                        }
                    }
                }
            }
        }

        // Capture the index tree so undo can restore the index.
        // `--index-output` is unnecessary: write-tree reads the worktree's
        // index via GIT_DIR/GIT_WORK_TREE resolution from `repo_path`.
        let index_tree = self
            .run_git_capture(&["write-tree"])
            .ok()
            .map(|s| s.trim().to_string());

        Ok(Snapshot {
            head,
            refs,
            stash_ref: stash,
            index_tree,
        })
    }

    /// Undo a transaction: restore the repository to its pre-write state.
    ///
    /// Safety: the current repository state must match the post-write
    /// snapshot of the most recent journaled transaction. If refs changed
    /// externally (outside the workbench), undo is blocked.
    /// All transactions at or after `transaction_id` are rolled back and
    /// removed from the journal (cascade rollback semantics).
    ///
    /// Crash safety: the journal is truncated (atomically) BEFORE any refs
    /// are restored, with an on-disk marker recording the snapshot to
    /// restore; `recover()` at engine start rolls the restoration forward.
    /// Ref restoration itself is a single `git update-ref --stdin`
    /// transaction (atomic at the git level); HEAD is restored last so its
    /// target ref always exists.
    ///
    /// Limitation: worktree file contents are not restored (only refs, HEAD
    /// and the index); clients should refresh and warn the user.
    pub fn undo(&self, transaction_id: u64) -> Result<UndoOutcome, UndoError> {
        let transactions = self.read_journal().map_err(UndoError::Io)?;

        let target = transactions
            .iter()
            .find(|t| t.id == transaction_id)
            .ok_or_else(|| UndoError::NotFound)?;

        if !target.success {
            // The write failed; nothing changed. Just pop it from the stack.
            self.truncate_journal_at(transaction_id).map_err(UndoError::Io)?;
            return Ok(UndoOutcome::Noop);
        }

        // Safety check: current state must equal the latest transaction's
        // post-write snapshot (i.e. no external ref changes since the last
        // workbench write).
        let latest = transactions.last().expect("non-empty journal");
        let expected = self
            .load_snapshot(latest.id, SnapshotKind::Post)
            .map_err(UndoError::Io)?;
        let current = self.read_snapshot().map_err(UndoError::Io)?;
        if current != expected {
            return Err(UndoError::Blocked {
                reason: "repository state changed externally since the last workbench operation"
                    .to_string(),
            });
        }

        // Restore the target's pre-write snapshot.
        let pre = self
            .load_snapshot(transaction_id, SnapshotKind::Pre)
            .map_err(UndoError::Io)?;

        // Write the recovery marker, then truncate the journal, then mutate.
        // If we crash anywhere past this point, `recover()` finishes the job.
        let marker = TransactionFile {
            meta: TransactionMeta {
                id: transaction_id,
                description: target.description.clone(),
                epoch: target.epoch_at_creation,
            },
            snapshot: pre.clone(),
        };
        let marker_data = serde_json::to_vec_pretty(&marker)
            .map_err(|e| UndoError::Io(anyhow::anyhow!("failed to serialize undo marker: {e}")))?;
        write_durable(&self.undo_dir.join(UNDO_MARKER), &marker_data)
            .map_err(UndoError::Io)?;

        self.truncate_journal_at(transaction_id).map_err(UndoError::Io)?;

        let restored_refs = self.restore_snapshot(&pre).map_err(UndoError::Git)?;

        fs::remove_file(self.undo_dir.join(UNDO_MARKER))
            .map_err(|e| UndoError::Io(anyhow::anyhow!("failed to remove undo marker: {e}")))?;

        // Snapshot files for removed transactions are no longer needed.
        self.prune_snapshots(transaction_id);

        info!(transaction_id, restored_refs, "undo completed");
        Ok(UndoOutcome::Restored { restored_refs })
    }

    /// Idempotently restore a snapshot: only refs that currently differ are
    /// touched (so re-running on a partially restored repo is a no-op).
    /// Uses one `git update-ref --stdin` batch (atomic), then restores the
    /// index, then HEAD last.
    fn restore_snapshot(&self, pre: &Snapshot) -> anyhow::Result<usize> {
        let current = self.read_snapshot()?;
        let mut restored_refs = 0;

        // Build a single atomic update-ref --stdin script with old-value
        // guards so any concurrent drift aborts the whole batch.
        let mut script = String::new();
        for (name, target_sha) in &pre.refs {
            validate_full_ref_name(name)?;
            validate_sha(target_sha)?;
            let old = current.refs.get(name).cloned().unwrap_or_else(|| "0".repeat(40));
            if current.refs.get(name) != Some(target_sha) {
                restored_refs += 1;
            }
            script.push_str(&format!("update {name} {target_sha} {old}\n"));
        }
        for name in current.refs.keys() {
            if !pre.refs.contains_key(name) {
                validate_full_ref_name(name)?;
                let old = current.refs[name].clone();
                script.push_str(&format!("delete {name} {old}\n"));
                restored_refs += 1;
            }
        }

        if !script.is_empty() {
            self.run_git_stdin(&["update-ref", "--stdin"], &script)
                .context("atomic ref restoration failed")?;
        }

        // Restore the index (not the worktree; documented limitation).
        if let Some(tree) = &pre.index_tree {
            validate_sha(tree)?;
            if let Err(e) = self.run_git(&["read-tree", tree]) {
                warn!(error = %e, "failed to restore index during undo");
            }
        }

        // HEAD last: its target ref now exists.
        self.restore_head(&pre.head)?;

        Ok(restored_refs)
    }

    fn restore_head(&self, head: &str) -> anyhow::Result<()> {
        if head.starts_with("ref: ") {
            let ref_name = head.strip_prefix("ref: ").expect("checked prefix").trim();
            validate_full_ref_name(ref_name)?;
            self.run_git(&["symbolic-ref", "HEAD", ref_name])?;
        } else if !head.is_empty() {
            // Detached HEAD.
            validate_sha(head.trim())?;
            self.run_git(&["update-ref", "--no-deref", "HEAD", head.trim()])?;
        } else {
            anyhow::bail!("snapshot has empty HEAD; refusing to restore");
        }
        Ok(())
    }

    fn run_git(&self, args: &[&str]) -> anyhow::Result<()> {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    fn run_git_capture(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .output()
            .context("failed to run git")?;
        if !out.status.success() {
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn run_git_stdin(&self, args: &[&str], input: &str) -> anyhow::Result<()> {
        let mut child = StdCommand::new("git")
            .args(args)
            .current_dir(&self.repo_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to run git")?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            anyhow::bail!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }

    /// Atomically rewrite the journal keeping only transactions before
    /// `transaction_id`: write a temp file, fsync, rename over the target.
    fn truncate_journal_at(&self, transaction_id: u64) -> anyhow::Result<()> {
        let transactions = self.read_journal()?;
        let kept: Vec<String> = transactions
            .iter()
            .filter(|t| t.id < transaction_id)
            .map(|t| serde_json::to_string(t).context("failed to serialize journal entry"))
            .collect::<anyhow::Result<_>>()?;
        let mut content = kept.join("\n");
        if !content.is_empty() {
            content.push('\n');
        }
        let tmp = self.journal_path.with_extension("jsonl.tmp");
        write_durable(&tmp, content.as_bytes())?;
        fs::rename(&tmp, &self.journal_path)?;
        Ok(())
    }

    fn read_journal(&self) -> anyhow::Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        if self.journal_path.exists() {
            let content = fs::read_to_string(&self.journal_path)?;
            for (idx, line) in content.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Transaction>(line) {
                    Ok(tx) => transactions.push(tx),
                    Err(e) => {
                        // A truncated FINAL line is expected after a crash
                        // (append was interrupted); a corrupt line in the
                        // middle indicates real damage.
                        let is_last = idx + 1 >= content.lines().count();
                        if is_last {
                            warn!(error = %e, "journal ends with a truncated line; dropping it");
                        } else {
                            warn!(
                                error = %e,
                                line = idx,
                                "journal contains a corrupt line; skipping it"
                            );
                        }
                    }
                }
            }
        }
        transactions.sort_by_key(|t| t.id);
        Ok(transactions)
    }

    fn load_snapshot(&self, id: u64, kind: SnapshotKind) -> anyhow::Result<Snapshot> {
        let path = match kind {
            SnapshotKind::Pre => self.pre_path(id),
            SnapshotKind::Post => self.post_path(id),
        };
        let data = fs::read_to_string(&path)?;
        let file: TransactionFile = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse snapshot {}", path.display()))?;
        Ok(file.snapshot)
    }

    fn pre_path(&self, id: u64) -> PathBuf {
        self.undo_dir.join(format!("{id}.pre"))
    }

    fn post_path(&self, id: u64) -> PathBuf {
        self.undo_dir.join(format!("{id}.post"))
    }

    /// Delete `{id}.pre`/`{id}.post` files for transactions at or after
    /// `transaction_id` (they are no longer referenced by the journal).
    fn prune_snapshots(&self, transaction_id: u64) {
        let Ok(entries) = fs::read_dir(&self.undo_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(id) = stem.parse::<u64>() {
                if id >= transaction_id {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }

    pub fn list_transactions(&self, limit: u32) -> anyhow::Result<Vec<UndoEntry>> {
        let transactions = self.read_journal()?;
        let mut entries: Vec<UndoEntry> = transactions
            .into_iter()
            .map(|tx| UndoEntry {
                id: tx.id,
                timestamp: tx.timestamp,
                description: tx.description,
                epoch_at_creation: tx.epoch_at_creation,
            })
            .collect();
        entries.reverse(); // newest first
        entries.truncate(limit as usize);
        Ok(entries)
    }

    pub fn append_audit_log(
        &self,
        operation: &str,
        epoch: u64,
        result: &WriteResult,
        transaction_id: u64,
    ) -> anyhow::Result<()> {
        let timestamp = Utc::now().timestamp();
        let status = match result {
            WriteResult::Ok(_) => "ok",
            WriteResult::Undo(Ok(_)) => "ok",
            WriteResult::Err(_) | WriteResult::Undo(Err(_)) => "err",
        };
        let line = serde_json::json!({
            "timestamp": timestamp,
            "operation": operation,
            "epoch": epoch,
            "status": status,
            "transaction_id": transaction_id,
        });

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_log)?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UndoError {
    #[error("transaction not found")]
    NotFound,
    #[error("undo blocked: {reason}")]
    Blocked { reason: String },
    #[error(transparent)]
    Io(anyhow::Error),
    #[error(transparent)]
    Git(anyhow::Error),
}

impl UndoError {
    /// Clone-able projection for transport across the write-queue channel.
    pub fn kind(&self) -> UndoErrorKind {
        match self {
            UndoError::NotFound => UndoErrorKind::NotFound,
            UndoError::Blocked { reason } => UndoErrorKind::Blocked {
                reason: reason.clone(),
            },
            UndoError::Io(e) | UndoError::Git(e) => UndoErrorKind::Failure {
                msg: format!("{e}"),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SnapshotKind {
    Pre,
    Post,
}

#[derive(Debug, Serialize, Deserialize)]
struct TransactionMeta {
    id: u64,
    description: String,
    epoch: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct TransactionFile {
    meta: TransactionMeta,
    snapshot: Snapshot,
}

fn read_loose_refs(dir: &Path, prefix: &str) -> anyhow::Result<HashMap<String, String>> {
    let mut refs = HashMap::new();
    if !dir.exists() {
        return Ok(refs);
    }
    for entry in walkdir::WalkDir::new(dir).min_depth(1) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dir) else {
            continue;
        };
        // Skip git internals in refs (e.g. refs/heads, refs/tags dir files
        // like "locked" are not valid refs).
        let target = fs::read_to_string(entry.path())?.trim().to_string();
        if target.len() < 40 {
            // Not a sha (could be a directory marker or garbage).
            continue;
        }
        let name = format!("{}/{}", prefix, rel.to_string_lossy().replace('\\', "/"));
        refs.insert(name, target);
    }
    Ok(refs)
}

/// Write `data` to `path` durably: create/truncate, write, fsync the file,
/// then fsync the parent directory so the file entry itself survives.
fn write_durable(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let mut file = File::create(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Full ref names (`refs/...`) restored by undo must match a strict pattern
/// before they are passed to git — they come from on-disk snapshot files.
fn validate_full_ref_name(name: &str) -> anyhow::Result<()> {
    if !name.starts_with("refs/") {
        anyhow::bail!("ref name must start with 'refs/': {name:?}");
    }
    if name.len() > 512
        || name.contains("..")
        || name.contains("//")
        || name.contains('@')
        || name.ends_with('/')
        || name.ends_with(".lock")
        || name.ends_with('.')
    {
        anyhow::bail!("invalid ref name: {name:?}");
    }
    for ch in name.chars() {
        if ch.is_ascii_control() || " ~^:?*[\\".contains(ch) {
            anyhow::bail!("invalid character {ch:?} in ref name {name:?}");
        }
    }
    for comp in name.split('/') {
        if comp.is_empty() || comp.starts_with('.') || comp.ends_with('.') {
            anyhow::bail!("invalid ref component in {name:?}");
        }
    }
    Ok(())
}

/// Object ids must be hex (sha1 40 / sha256 64 chars).
fn validate_sha(sha: &str) -> anyhow::Result<()> {
    let valid = (sha.len() == 40 || sha.len() == 64) && sha.chars().all(|c| c.is_ascii_hexdigit());
    if !valid {
        anyhow::bail!("invalid object id: {sha:?}");
    }
    Ok(())
}
