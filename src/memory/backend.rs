use anyhow::Result;
use async_trait::async_trait;

use super::category::MemoryCategory;

/// Storage backend kind. One variant per concrete backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBackendKind {
    /// Markdown files on disk (current default layout).
    Markdown,
}

impl MemoryBackendKind {
    /// Canonical config string for this kind.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
        }
    }

    /// Parse a config string into a backend kind. Unknown strings return `None`.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }
}

/// Result of a surgical `edit` operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The old text was found exactly once and replaced.
    Replaced,
    /// The old text was not found.
    NotFound,
    /// The old text was found multiple times; the value is the match count.
    Ambiguous(usize),
}

/// Document-level memory backend.
///
/// Addresses whole documents by category (+ optional dynamic name), matching
/// SlimBot's actual consumption pattern: whole-file context injection and
/// surgical edits from the Dream process. Entry-level APIs (store/recall per
/// entry, semantic search) are deliberately out of scope for now.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    /// Backend name, e.g. "markdown".
    fn name(&self) -> &str;

    /// Backend kind.
    fn kind(&self) -> MemoryBackendKind;

    /// Read the document for `category`. Returns `None` if it does not exist.
    /// `name` addresses `Skill` / `Custom` documents; fixed categories ignore
    /// it (tag semantics, reserved for future namespace filtering).
    async fn get(&self, category: &MemoryCategory, name: Option<&str>) -> Result<Option<String>>;

    /// Write the document for `category`, creating parent directories.
    async fn set(&self, category: &MemoryCategory, name: Option<&str>, content: &str)
    -> Result<()>;

    /// Replace `old` with `new` inside the document. `old` must match exactly
    /// once; zero matches → `NotFound`, multiple → `Ambiguous(n)`.
    async fn edit(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
        old: &str,
        new: &str,
    ) -> Result<EditOutcome>;

    /// Delete the document. Returns whether a file was removed.
    async fn remove(&self, category: &MemoryCategory, name: Option<&str>) -> Result<bool>;

    /// Enumerate existing skill names (for `Skill` documents).
    async fn list_skill_names(&self) -> Result<Vec<String>>;

    /// Enumerate existing custom category names (for `Custom` documents).
    async fn list_custom_names(&self) -> Result<Vec<String>>;

    /// Bootstrap template content for a fixed category, if any. Used by the
    /// facade's `get_effective` to skip unmodified template documents.
    /// `Skill` / `Custom` / `Daily` have no template.
    fn template(&self, category: &MemoryCategory) -> Option<String>;

    /// Lightweight health probe.
    async fn health_check(&self) -> bool {
        true
    }

    /// Flush to durable storage (fsync). Default no-op.
    async fn sync(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kind_as_str_roundtrip() {
        let kind = MemoryBackendKind::Markdown;
        assert_eq!(MemoryBackendKind::from_str_opt(kind.as_str()), Some(kind));
    }

    #[test]
    fn test_kind_from_unknown_string() {
        assert_eq!(MemoryBackendKind::from_str_opt("sqlite"), None);
        assert_eq!(MemoryBackendKind::from_str_opt(""), None);
    }

    #[test]
    fn test_edit_outcome_variants_are_distinct() {
        assert_ne!(EditOutcome::Replaced, EditOutcome::NotFound);
        assert_ne!(EditOutcome::Replaced, EditOutcome::Ambiguous(1));
        assert_ne!(EditOutcome::NotFound, EditOutcome::Ambiguous(0));
        assert_eq!(EditOutcome::Ambiguous(2), EditOutcome::Ambiguous(2));
    }
}
