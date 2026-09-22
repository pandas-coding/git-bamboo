use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use git_workbench_protocol as protocol;

pub struct Session {
    pub repo_path: PathBuf,
    pub epoch: AtomicU64,
    pub gix_repo: Mutex<gix::Repository>,
}

impl Session {
    pub fn open(repo_path: &std::path::Path) -> anyhow::Result<Arc<Self>> {
        let gix_repo = gix::open(repo_path)?;
        Ok(Arc::new(Self {
            repo_path: repo_path.to_path_buf(),
            epoch: AtomicU64::new(1),
            gix_repo: Mutex::new(gix_repo),
        }))
    }

    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}
