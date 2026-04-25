pub mod shell;
pub mod file_reader;
pub mod file_writer;
pub mod file_editor;
pub mod list_dir;
pub mod make_dir;

use std::path::{Path, PathBuf};

use crate::tool::Tool;

/// Resolve a user-provided path against the workspace directory root.
/// Returns the canonical absolute path, or Err if it escapes the workspace directory.
/// Works for both existing and non-existent paths.
pub fn resolve_workspace_path(user_path: &str, workspace_dir: &Path) -> anyhow::Result<PathBuf> {
    // Canonicalize the workspace_dir to get the absolute boundary
    let workspace_dir_abs = workspace_dir.canonicalize()?;

    // Normalize user path: strip leading slashes, treat as relative
    let clean = user_path.trim_start_matches('/').trim_start_matches("./");

    // Try to canonicalize the joined path directly (works if it exists)
    let joined = workspace_dir_abs.join(clean);
    if let Ok(resolved) = joined.canonicalize() {
        if !resolved.starts_with(&workspace_dir_abs) {
            anyhow::bail!("Path escapes workspace directory: {}", user_path);
        }
        return Ok(resolved);
    }

    // Path doesn't exist yet — find the deepest existing ancestor
    let mut ancestor = joined.clone();
    while !ancestor.exists() {
        if let Some(parent) = ancestor.parent() {
            ancestor = parent.to_path_buf();
        } else {
            break;
        }
    }

    let ancestor_abs = ancestor.canonicalize().map_err(|e| {
        anyhow::anyhow!("Cannot resolve base directory for '{}': {}", user_path, e)
    })?;

    if !ancestor_abs.starts_with(&workspace_dir_abs) {
        anyhow::bail!("Path escapes workspace directory: {}", user_path);
    }

    // Build remaining segments and reject any ".." that would escape
    let remaining = joined.strip_prefix(&ancestor)?;
    for comp in remaining.components() {
        if let std::path::Component::ParentDir = comp {
            anyhow::bail!("Path escapes workspace directory via '..': {}", user_path);
        }
    }

    Ok(joined)
}

/// Factory function to create a tool by name.
pub fn create_tool(name: &str, workspace_dir: &Path) -> Option<Box<dyn Tool>> {
    match name {
        "shell" => Some(Box::new(shell::ShellTool::default())),
        "file_reader" => Some(Box::new(file_reader::FileReaderTool::new(workspace_dir.to_path_buf()))),
        "file_writer" => Some(Box::new(file_writer::FileWriterTool::new(workspace_dir.to_path_buf()))),
        "file_editor" => Some(Box::new(file_editor::FileEditorTool::new(workspace_dir.to_path_buf()))),
        "list_dir" => Some(Box::new(list_dir::ListDirTool::new(workspace_dir.to_path_buf()))),
        "make_dir" => Some(Box::new(make_dir::MakeDirTool::new(workspace_dir.to_path_buf()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let resolved = resolve_workspace_path("a.txt", tmp.path()).unwrap();
        assert_eq!(resolved, tmp.path().join("a.txt").canonicalize().unwrap());
    }

    #[test]
    fn test_resolve_new_file_in_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        // Create the subdirectory so ancestor resolution works
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let resolved = resolve_workspace_path("sub/new.txt", tmp.path()).unwrap();
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
    }

    #[test]
    fn test_reject_dotdot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_workspace_path("../outside.txt", tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_reject_absolute_escape_via_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let result = resolve_workspace_path("sub/../../escape.txt", tmp.path());
        assert!(result.is_err());
    }
}
