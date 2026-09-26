use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};

use super::backend::{MemoryBackend, MemoryBackendKind};
use super::markdown::MarkdownBackend;

/// Build a backend from its kind. Currently only markdown exists.
pub fn create_backend(kind: MemoryBackendKind, workspace_dir: &Path) -> Arc<dyn MemoryBackend> {
    match kind {
        MemoryBackendKind::Markdown => Arc::new(MarkdownBackend::new(workspace_dir)),
    }
}

/// Build a backend from a config string (e.g. `memory.backend = "markdown"`).
/// Unknown backend names produce a clear error.
pub fn backend_from_name(name: &str, workspace_dir: &Path) -> Result<Arc<dyn MemoryBackend>> {
    let kind = MemoryBackendKind::from_str_opt(name)
        .with_context(|| format!("unsupported memory backend: '{name}'"))?;
    Ok(create_backend(kind, workspace_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_markdown_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = create_backend(MemoryBackendKind::Markdown, tmp.path());
        assert_eq!(backend.name(), "markdown");
        assert_eq!(backend.kind(), MemoryBackendKind::Markdown);
    }

    #[test]
    fn test_backend_from_name_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = backend_from_name("markdown", tmp.path()).unwrap();
        assert_eq!(backend.name(), "markdown");
    }

    #[test]
    fn test_backend_from_name_unknown_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = backend_from_name("sqlite", tmp.path())
            .err()
            .expect("expected error");
        assert!(err.to_string().contains("unsupported memory backend"));
        assert!(err.to_string().contains("sqlite"));
    }
}
