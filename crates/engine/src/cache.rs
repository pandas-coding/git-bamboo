use std::collections::HashMap;
use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const COMMIT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("commit_meta");
const COMMIT_LANES: TableDefinition<&str, u16> = TableDefinition::new("commit_lanes");
const CACHE_EPOCH: TableDefinition<&str, u64> = TableDefinition::new("cache_epoch");
const REPO_FINGERPRINT: TableDefinition<&str, &str> = TableDefinition::new("repo_fingerprint");

const EPOCH_KEY: &str = "epoch";
const FINGERPRINT_KEY: &str = "fingerprint";

/// The immutable, lane-less part of [`git_workbench_protocol::Commit`]
/// stored in COMMIT_META. Keyed by object id, so entries never go stale —
/// only the lane table is epoch-guarded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedCommit {
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    pub author_time: i64,
    pub parent_ids: Vec<String>,
}

pub struct Cache {
    db: Database,
}

impl Cache {
    /// Open (or create) the cache inside `git_dir/git-workbench` and
    /// reconcile the durable epoch with the repository fingerprint.
    ///
    /// Returns the cache and the epoch the session must start with:
    /// - no stored state → epoch 1;
    /// - stored fingerprint matches → reuse the stored epoch;
    /// - stored fingerprint differs (refs moved while the engine was down,
    ///   possibly a different history) → clear COMMIT_META/COMMIT_LANES and
    ///   bump the epoch (stored + 1).
    ///
    /// The fingerprint check, table clearing and epoch write happen in ONE
    /// redb write transaction, so a crash can never leave a poisoned cache
    /// (cleared tables with a stale epoch or vice versa).
    pub fn open(git_dir: &Path, fingerprint: &str) -> anyhow::Result<(Self, u64)> {
        let cache_dir = git_dir.join("git-workbench");
        std::fs::create_dir_all(&cache_dir)?;
        let db_path = cache_dir.join("cache.redb");
        // redb takes an exclusive file lock; a just-replaced session may
        // still be releasing it (its watcher thread can hold the last
        // Arc briefly), so retry for a short while.
        let db = open_with_retry(&db_path)?;

        // Ensure all tables exist before reading them.
        {
            let txn = db.begin_write()?;
            {
                let _ = txn.open_table(COMMIT_META)?;
                let _ = txn.open_table(COMMIT_LANES)?;
                let _ = txn.open_table(CACHE_EPOCH)?;
                let _ = txn.open_table(REPO_FINGERPRINT)?;
            }
            txn.commit()?;
        }

        // Read the stored state first.
        let (stored_fp, stored_epoch) = {
            let txn = db.begin_read()?;
            let fp = txn.open_table(REPO_FINGERPRINT)?.get(FINGERPRINT_KEY)?.map(|v| v.value().to_string());
            let epoch = txn.open_table(CACHE_EPOCH)?.get(EPOCH_KEY)?.map(|v| v.value());
            (fp, epoch)
        };

        let epoch = match (&stored_fp, stored_epoch) {
            (Some(fp), Some(e)) if fp == fingerprint => {
                // Repository unchanged since the last run: reuse the epoch.
                e
            }
            _ => {
                let new_epoch = stored_epoch.unwrap_or(0) + 1;
                if stored_epoch.is_some() {
                    warn!(
                        new_epoch,
                        "repo fingerprint changed since last run; clearing cache and bumping epoch"
                    );
                }
                let txn = db.begin_write()?;
                {
                    let mut meta = txn.open_table(COMMIT_META)?;
                    let mut lanes = txn.open_table(COMMIT_LANES)?;
                    let mut epoch_table = txn.open_table(CACHE_EPOCH)?;
                    let mut fp_table = txn.open_table(REPO_FINGERPRINT)?;
                    clear_table(&mut meta)?;
                    clear_table(&mut lanes)?;
                    epoch_table.insert(EPOCH_KEY, new_epoch)?;
                    fp_table.insert(FINGERPRINT_KEY, fingerprint)?;
                }
                txn.commit()?;
                new_epoch
            }
        };

        info!(db_path = %db_path.display(), epoch, "cache opened");
        Ok((Self { db }, epoch))
    }

    /// Look up commit metadata for many ids in a single read transaction.
    /// COMMIT_META is keyed by immutable object ids, so hits are always
    /// fresh regardless of the epoch.
    pub fn get_commit_metas(
        &self,
        ids: &[String],
    ) -> anyhow::Result<HashMap<String, CachedCommit>> {
        let mut out = HashMap::with_capacity(ids.len());
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COMMIT_META)?;
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            if let Some(bytes) = table.get(id.as_str())? {
                if let Ok(meta) = serde_json::from_slice::<CachedCommit>(bytes.value()) {
                    out.insert(id.clone(), meta);
                }
            }
        }
        Ok(out)
    }

    /// Persist commit metadata for many commits in a single write
    /// transaction.
    pub fn put_commit_metas(&self, entries: &[(String, CachedCommit)]) -> anyhow::Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COMMIT_META)?;
            for (id, meta) in entries {
                let bytes = serde_json::to_vec(meta)?;
                table.insert(id.as_str(), bytes.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Persist lane assignments together with the epoch they are valid for
    /// in ONE write transaction (crash between the two writes would
    /// otherwise poison the cache).
    pub fn put_lanes_with_epoch(&self, lanes: &[(String, u16)], epoch: u64) -> anyhow::Result<()> {
        if lanes.is_empty() {
            return Ok(());
        }
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COMMIT_LANES)?;
            for (id, lane) in lanes {
                table.insert(id.as_str(), *lane)?;
            }
            let mut epoch_table = txn.open_table(CACHE_EPOCH)?;
            epoch_table.insert(EPOCH_KEY, epoch)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_cached_epoch(&self) -> anyhow::Result<Option<u64>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CACHE_EPOCH)?;
        Ok(table.get(EPOCH_KEY)?.map(|v| v.value()))
    }

    /// If the cached epoch does not match `current_epoch`, clear the lane
    /// table and record the new epoch — both in a single write transaction.
    /// Returns whether a clear happened.
    pub fn invalidate_on_epoch_change(&self, current_epoch: u64) -> anyhow::Result<bool> {
        let cached = self.get_cached_epoch()?;
        if cached != Some(current_epoch) {
            warn!(cached = ?cached, current_epoch, "cache epoch mismatch, clearing lanes");
            let txn = self.db.begin_write()?;
            {
                let mut table = txn.open_table(COMMIT_LANES)?;
                clear_table(&mut table)?;
                let mut epoch_table = txn.open_table(CACHE_EPOCH)?;
                epoch_table.insert(EPOCH_KEY, current_epoch)?;
            }
            txn.commit()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Remove all entries from a redb table (redb 2.x has no `clear`).
fn clear_table<'t, V: redb::Value + 'static>(
    table: &mut redb::Table<'t, &str, V>,
) -> anyhow::Result<()> {
    let keys: Vec<String> = table
        .iter()?
        .filter_map(|e| e.ok())
        .map(|(k, _)| k.value().to_string())
        .collect();
    for k in keys {
        table.remove(k.as_str())?;
    }
    Ok(())
}

/// Open the cache database, retrying briefly while a previous handle in
/// this process is still releasing the exclusive file lock.
fn open_with_retry(db_path: &Path) -> anyhow::Result<Database> {
    let mut last_err = None;
    for _ in 0..20 {
        match Database::create(db_path) {
            Ok(db) => return Ok(db),
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    Err(anyhow::anyhow!(
        "failed to open cache database {}: {last_err:?}",
        db_path.display()
    ))
}
