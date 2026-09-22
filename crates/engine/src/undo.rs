use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::info;

use git_workbench_protocol::UndoEntry;

use crate::write_queue::WriteResult;

/// The complete ref state of a repository at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Raw `HEAD` content: either `ref: refs/heads/<name>` or a detached sha.
    pub head: String,
    /// Local branch refs (full name → sha).
    pub refs: HashMap<String, String>,
    /// Whether `refs/stash` exists.
    pub has_stash: bool,
    pub stash_ref: Option<String>,
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

#[derive(Debug)]
pub enum UndoOutcome {
    Restored { restored_refs: usize },
    Noop,
}

pub struct UndoEngine {
    journal_path: PathBuf,
    undo_dir: PathBuf,
    repo_path: PathBuf,
    next_id: AtomicU64,
    audit_log: PathBuf,
}

impl UndoEngine {
    pub fn new(repo_path: &Path) -> anyhow::Result<Self> {
        let undo_dir = repo_path.join(".git").join("git-workbench").join("undo");
        fs::create_dir_all(&undo_dir)?;

        let journal_path = undo_dir.join("journal.jsonl");
        let audit_log = repo_path
            .join(".git")
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
        Ok(Self {
            journal_path,
            undo_dir,
            repo_path: repo_path.to_path_buf(),
            next_id: AtomicU64::new(next_id),
            audit_log,
        })
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
        fs::write(&path, data)?;
        Ok(id)
    }

    /// Commit a transaction: capture the post-write snapshot and append the
    /// journal entry.
    pub fn commit(&self, id: u64, description: &str, epoch: u64, success: bool) -> anyhow::Result<()> {
        let snapshot = self.read_snapshot()?;
        let meta = TransactionMeta {
            id,
            description: description.to_string(),
            epoch,
        };
        let path = self.post_path(id);
        let data = serde_json::to_vec_pretty(&TransactionFile { meta, snapshot })?;
        fs::write(&path, data)?;

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
        Ok(())
    }

    /// Read the current repository snapshot.
    pub fn read_snapshot(&self) -> anyhow::Result<Snapshot> {
        let repo_path = &self.repo_path;

        // Read HEAD.
        let head_path = repo_path.join(".git").join("HEAD");
        let head = fs::read_to_string(&head_path)
            .unwrap_or_default()
            .trim()
            .to_string();

        // Read local loose refs.
        let mut refs = read_loose_refs(
            &repo_path.join(".git").join("refs").join("heads"),
            "refs/heads",
        )?;

        // Merge in packed refs (loose refs take precedence).
        if let Ok(content) = fs::read_to_string(repo_path.join(".git").join("packed-refs")) {
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

        // Check stash.
        let stash = read_loose_refs(&repo_path.join(".git").join("refs"), "refs")?
            .get("refs/stash")
            .cloned();
        let has_stash = stash.is_some();

        Ok(Snapshot {
            head,
            refs,
            has_stash,
            stash_ref: stash,
        })
    }

    /// Undo a transaction: restore the repository to its pre-write state.
    ///
    /// Safety: the current repository state must match the post-write
    /// snapshot of the most recent journaled transaction. If refs changed
    /// externally (outside the workbench), undo is blocked.
    /// All transactions at or after `transaction_id` are rolled back and
    /// removed from the journal (cascade rollback semantics).
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

        let mut restored_refs = 0;

        // HEAD first.
        self.restore_head(&pre.head).map_err(UndoError::Git)?;

        // Restore refs: update moved refs, delete refs created after the
        // snapshot, recreate refs deleted since.
        for (name, target_sha) in &pre.refs {
            let current_sha = current.refs.get(name);
            if current_sha != Some(target_sha) {
                self.run_git(&["update-ref", name, target_sha])
                    .map_err(UndoError::Git)?;
                restored_refs += 1;
            }
        }
        for name in current.refs.keys() {
            if !pre.refs.contains_key(name) {
                self.run_git(&["update-ref", "-d", name])
                    .map_err(UndoError::Git)?;
                restored_refs += 1;
            }
        }

        // Remove the rolled-back transactions from the journal.
        self.truncate_journal_at(transaction_id).map_err(UndoError::Io)?;

        info!(transaction_id, restored_refs, "undo completed");
        Ok(UndoOutcome::Restored { restored_refs })
    }

    fn restore_head(&self, head: &str) -> anyhow::Result<()> {
        if head.starts_with("ref: ") {
            let ref_name = head
                .strip_prefix("ref: ")
                .expect("checked prefix")
                .trim();
            self.run_git(&["symbolic-ref", "HEAD", ref_name])?;
        } else if !head.is_empty() {
            // Detached HEAD.
            self.run_git(&["update-ref", "--no-deref", "HEAD", head.trim()])?;
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

    fn truncate_journal_at(&self, transaction_id: u64) -> anyhow::Result<()> {
        let transactions = self.read_journal()?;
        let kept: Vec<String> = transactions
            .iter()
            .filter(|t| t.id < transaction_id)
            .map(|t| serde_json::to_string(t).unwrap())
            .collect();
        let mut content = kept.join("\n");
        if !content.is_empty() {
            content.push('\n');
        }
        fs::write(&self.journal_path, content)?;
        Ok(())
    }

    fn read_journal(&self) -> anyhow::Result<Vec<Transaction>> {
        let mut transactions = Vec::new();
        if self.journal_path.exists() {
            for line in fs::read_to_string(&self.journal_path)?.lines() {
                if let Ok(tx) = serde_json::from_str::<Transaction>(line) {
                    transactions.push(tx);
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
            WriteResult::Err(_) => "err",
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

    pub fn get_transaction(&self, id: u64) -> anyhow::Result<Option<Transaction>> {
        let transactions = self.read_journal()?;
        Ok(transactions.into_iter().find(|t| t.id == id))
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
