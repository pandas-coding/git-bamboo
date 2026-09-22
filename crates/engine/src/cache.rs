use std::path::Path;

pub struct Cache;

impl Cache {
    pub fn open(_path: &Path) -> anyhow::Result<Self> {
        Ok(Self)
    }
}
