//! Filesystem durability helpers shared by the memory backends and the
//! history store.

use std::path::Path;

use anyhow::Result;

/// Open an existing file and fsync it. Missing files are a no-op.
///
/// `append` only selects the open flag; neither mode truncates, so this is
/// safe for both log files (`append = true`) and documents (`append = false`).
pub fn sync_if_exists(path: &Path, append: bool) -> Result<()> {
    if path.exists() {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .append(append)
            .open(path)?;
        let _ = file.sync_all();
    }
    Ok(())
}

/// fsync a directory so newly created entries survive a crash. A missing or
/// unreadable directory is a no-op.
pub fn sync_dir(path: &Path) -> Result<()> {
    if let Ok(dir) = std::fs::OpenOptions::new().read(true).open(path) {
        let _ = dir.sync_all();
    }
    Ok(())
}
