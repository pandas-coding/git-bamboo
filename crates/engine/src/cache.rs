use std::path::Path;

use redb::{Database, ReadableTable, TableDefinition};
use tracing::{info, warn};

use git_workbench_protocol::Commit;

const COMMIT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("commit_meta");
const COMMIT_LANES: TableDefinition<&str, u8> = TableDefinition::new("commit_lanes");
const CACHE_EPOCH: TableDefinition<&str, u64> = TableDefinition::new("cache_epoch");

const EPOCH_KEY: &str = "epoch";

pub struct Cache {
    db: Database,
}

impl Cache {
    pub fn open(repo_path: &Path) -> anyhow::Result<Self> {
        let cache_dir = repo_path.join(".git").join("git-workbench");
        std::fs::create_dir_all(&cache_dir)?;
        let db_path = cache_dir.join("cache.redb");
        let db = Database::create(&db_path)?;

        // Ensure tables exist.
        let txn = db.begin_write()?;
        {
            let _ = txn.open_table(COMMIT_META)?;
            let _ = txn.open_table(COMMIT_LANES)?;
            let _ = txn.open_table(CACHE_EPOCH)?;
        }
        txn.commit()?;

        info!(db_path = %db_path.display(), "cache opened");
        Ok(Self { db })
    }

    pub fn get_commit_meta(&self, id: &str) -> anyhow::Result<Option<Commit>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COMMIT_META)?;
        match table.get(id)? {
            Some(bytes) => {
                let commit = serde_json::from_slice(bytes.value())?;
                Ok(Some(commit))
            }
            None => Ok(None),
        }
    }

    pub fn put_commit_meta(&self, id: &str, commit: &Commit) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COMMIT_META)?;
            let bytes = serde_json::to_vec(commit)?;
            table.insert(id, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn get_lane(&self, id: &str) -> anyhow::Result<Option<u8>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(COMMIT_LANES)?;
        Ok(table.get(id)?.map(|v| v.value()))
    }

    pub fn put_lane(&self, id: &str, lane: u8, epoch: u64) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(COMMIT_LANES)?;
            table.insert(id, lane)?;
        }
        txn.commit()?;
        self.set_cached_epoch(epoch)?;
        Ok(())
    }

    pub fn get_cached_epoch(&self) -> anyhow::Result<Option<u64>> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(CACHE_EPOCH)?;
        Ok(table.get(EPOCH_KEY)?.map(|v| v.value()))
    }

    fn set_cached_epoch(&self, epoch: u64) -> anyhow::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(CACHE_EPOCH)?;
            table.insert(EPOCH_KEY, epoch)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn invalidate_on_epoch_change(&self, current_epoch: u64) -> anyhow::Result<bool> {
        let cached = self.get_cached_epoch()?;
        if cached != Some(current_epoch) {
            warn!(cached = ?cached, current_epoch, "cache epoch mismatch, clearing lanes");
            let txn = self.db.begin_write()?;
            {
                let mut table = txn.open_table(COMMIT_LANES)?;
                // Remove all entries (redb has no Table::clear in 2.x).
                let keys: Vec<String> = table
                    .iter()?
                    .filter_map(|e| e.ok())
                    .map(|(k, _)| k.value().to_string())
                    .collect();
                for k in keys {
                    table.remove(k.as_str())?;
                }
            }
            txn.commit()?;
            self.set_cached_epoch(current_epoch)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
