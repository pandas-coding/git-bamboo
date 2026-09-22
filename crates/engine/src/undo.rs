use std::path::Path;

pub struct UndoEngine;

impl UndoEngine {
    pub fn new(_repo_path: &Path) -> anyhow::Result<Self> {
        Ok(Self)
    }
}
