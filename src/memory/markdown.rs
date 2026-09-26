use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;

use super::backend::{EditOutcome, MemoryBackend, MemoryBackendKind};
use super::category::{MemoryCategory, is_valid_name};
use super::fs::{sync_dir, sync_if_exists};

/// Markdown-file memory backend.
///
/// Category → file layout (kept stable so existing workspaces and the path
/// contract in identity.md / SKILL.md / dream.md keep working):
///
/// | Category | Path |
/// |----------|------|
/// | Soul     | `{workspace}/SOUL.md` |
/// | User     | `{workspace}/USER.md` |
/// | Agent    | `{workspace}/AGENTS.md` |
/// | Tools    | `{workspace}/TOOLS.md` |
/// | Core     | `{workspace}/memory/MEMORY.md` |
/// | Daily    | `{workspace}/memory/YYYY-MM-DD.md` |
/// | Skill    | `{workspace}/skills/<name>/SKILL.md` |
/// | Custom   | `{workspace}/memory/<name>.md` |
pub struct MarkdownBackend {
    workspace_dir: PathBuf,
}

/// Fixed categories that map to a single workspace document.
///
/// Each entry is `(category, workspace-relative path, bootstrap template
/// dest)`. `Core`'s document lives under `memory/` while its template lives at
/// the embed root, hence the two separate columns. This table is the single
/// source of truth shared by `path_for`, `template` and `sync`.
const FIXED_DOCS: [(MemoryCategory, &str, &str); 5] = [
    (MemoryCategory::Soul, "SOUL.md", "SOUL.md"),
    (MemoryCategory::User, "USER.md", "USER.md"),
    (MemoryCategory::Agent, "AGENTS.md", "AGENTS.md"),
    (MemoryCategory::Tools, "TOOLS.md", "TOOLS.md"),
    (MemoryCategory::Core, "memory/MEMORY.md", "MEMORY.md"),
];

/// Look up the `(relative path, template dest)` of a fixed category.
/// Returns `None` for `Daily` / `Skill` / `Custom`.
fn fixed_doc(category: &MemoryCategory) -> Option<(&'static str, &'static str)> {
    FIXED_DOCS
        .iter()
        .find(|(c, _, _)| c == category)
        .map(|(_, path, template)| (*path, *template))
}

/// Require and validate a dynamic name for `Skill` / `Custom` documents.
fn require_name<'a>(name: Option<&'a str>, category: &str) -> Result<&'a str> {
    match name {
        Some(n) if is_valid_name(n) => Ok(n),
        Some(n) => bail!("invalid {category} name '{n}': must be 1..=64 chars of [a-z0-9-]"),
        None => bail!("{category} category requires a name"),
    }
}

impl MarkdownBackend {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
        }
    }

    /// Resolve the file path for a category (+ optional dynamic name).
    /// Fixed categories ignore `name` (tag semantics, reserved for future
    /// namespace filtering); `Skill` / `Custom` require a valid name.
    fn path_for(&self, category: &MemoryCategory, name: Option<&str>) -> Result<PathBuf> {
        match category {
            MemoryCategory::Daily => Ok(self
                .workspace_dir
                .join("memory")
                .join(format!("{}.md", chrono::Local::now().format("%Y-%m-%d")))),
            MemoryCategory::Skill => {
                let name = require_name(name, "skill")?;
                Ok(self
                    .workspace_dir
                    .join("skills")
                    .join(name)
                    .join("SKILL.md"))
            }
            MemoryCategory::Custom => {
                let name = require_name(name, "custom")?;
                Ok(self.workspace_dir.join("memory").join(format!("{name}.md")))
            }
            fixed => {
                let (rel_path, _) = fixed_doc(fixed)
                    .ok_or_else(|| anyhow::anyhow!("no document mapped for category '{fixed}'"))?;
                Ok(self.workspace_dir.join(rel_path))
            }
        }
    }

    fn read_file(path: &Path) -> Result<Option<String>> {
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    fn write_file(path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))
    }
}

#[async_trait]
impl MemoryBackend for MarkdownBackend {
    fn name(&self) -> &str {
        "markdown"
    }

    fn kind(&self) -> MemoryBackendKind {
        MemoryBackendKind::Markdown
    }

    async fn get(&self, category: &MemoryCategory, name: Option<&str>) -> Result<Option<String>> {
        let path = self.path_for(category, name)?;
        Self::read_file(&path)
    }

    async fn set(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
        content: &str,
    ) -> Result<()> {
        let path = self.path_for(category, name)?;
        Self::write_file(&path, content)
    }

    async fn edit(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
        old: &str,
        new: &str,
    ) -> Result<EditOutcome> {
        let path = self.path_for(category, name)?;
        let Some(content) = Self::read_file(&path)? else {
            return Ok(EditOutcome::NotFound);
        };
        let matches = content.matches(old).count();
        if matches == 0 {
            return Ok(EditOutcome::NotFound);
        }
        if matches > 1 {
            return Ok(EditOutcome::Ambiguous(matches));
        }
        let updated = content.replacen(old, new, 1);
        Self::write_file(&path, &updated)?;
        Ok(EditOutcome::Replaced)
    }

    async fn remove(&self, category: &MemoryCategory, name: Option<&str>) -> Result<bool> {
        let path = self.path_for(category, name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("Failed to remove {}", path.display())),
        }
    }

    async fn list_skill_names(&self) -> Result<Vec<String>> {
        let skills_dir = self.workspace_dir.join("skills");
        let mut names = Vec::new();
        if !skills_dir.exists() {
            return Ok(names);
        }
        let entries = std::fs::read_dir(&skills_dir)
            .with_context(|| format!("Failed to read {}", skills_dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !(path.is_dir() && path.join("SKILL.md").exists()) {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    async fn list_custom_names(&self) -> Result<Vec<String>> {
        let memory_dir = self.workspace_dir.join("memory");
        let mut names = Vec::new();
        if !memory_dir.exists() {
            return Ok(names);
        }
        let entries = std::fs::read_dir(&memory_dir)
            .with_context(|| format!("Failed to read {}", memory_dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // Exclude Core (MEMORY.md) and Daily (date-named) documents.
            if stem == "MEMORY" || is_daily_stem(stem) {
                continue;
            }
            names.push(stem.to_string());
        }
        names.sort();
        Ok(names)
    }

    fn template(&self, category: &MemoryCategory) -> Option<String> {
        let (_, template_dest) = fixed_doc(category)?;
        crate::embed::get_content_by_dest(template_dest).map(str::to_string)
    }

    async fn health_check(&self) -> bool {
        self.workspace_dir.exists()
    }

    async fn sync(&self) -> Result<()> {
        for (_, rel_path, _) in FIXED_DOCS {
            sync_if_exists(&self.workspace_dir.join(rel_path), false)?;
        }
        sync_dir(&self.workspace_dir.join("memory"))?;
        Ok(())
    }
}

/// Whether a file stem looks like a `YYYY-MM-DD` daily document name.
fn is_daily_stem(stem: &str) -> bool {
    let bytes = stem.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && stem.chars().all(|c| c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_backend(tmp: &Path) -> MarkdownBackend {
        MarkdownBackend::new(tmp)
    }

    #[tokio::test]
    async fn test_path_for_fixed_categories() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert_eq!(
            b.path_for(&MemoryCategory::Soul, None).unwrap(),
            tmp.path().join("SOUL.md")
        );
        assert_eq!(
            b.path_for(&MemoryCategory::User, None).unwrap(),
            tmp.path().join("USER.md")
        );
        assert_eq!(
            b.path_for(&MemoryCategory::Agent, None).unwrap(),
            tmp.path().join("AGENTS.md")
        );
        assert_eq!(
            b.path_for(&MemoryCategory::Tools, None).unwrap(),
            tmp.path().join("TOOLS.md")
        );
        assert_eq!(
            b.path_for(&MemoryCategory::Core, None).unwrap(),
            tmp.path().join("memory").join("MEMORY.md")
        );
    }

    #[tokio::test]
    async fn test_path_for_daily_is_today() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(
            b.path_for(&MemoryCategory::Daily, None).unwrap(),
            tmp.path().join("memory").join(format!("{today}.md"))
        );
    }

    #[tokio::test]
    async fn test_path_for_skill_and_custom() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert_eq!(
            b.path_for(&MemoryCategory::Skill, Some("memory")).unwrap(),
            tmp.path().join("skills").join("memory").join("SKILL.md")
        );
        assert_eq!(
            b.path_for(&MemoryCategory::Custom, Some("notes")).unwrap(),
            tmp.path().join("memory").join("notes.md")
        );
    }

    #[tokio::test]
    async fn test_path_for_skill_requires_valid_name() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert!(b.path_for(&MemoryCategory::Skill, None).is_err());
        assert!(
            b.path_for(&MemoryCategory::Skill, Some("Bad Name"))
                .is_err()
        );
        assert!(b.path_for(&MemoryCategory::Custom, Some("")).is_err());
    }

    #[tokio::test]
    async fn test_get_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert_eq!(b.get(&MemoryCategory::Core, None).await.unwrap(), None);
        assert_eq!(
            b.get(&MemoryCategory::Skill, Some("nope")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_set_creates_dirs_and_get_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Core, None, "long term facts")
            .await
            .unwrap();
        assert_eq!(
            b.get(&MemoryCategory::Core, None).await.unwrap(),
            Some("long term facts".to_string())
        );
        assert!(tmp.path().join("memory").join("MEMORY.md").exists());

        b.set(&MemoryCategory::Skill, Some("grep"), "# Grep skill")
            .await
            .unwrap();
        assert_eq!(
            b.get(&MemoryCategory::Skill, Some("grep")).await.unwrap(),
            Some("# Grep skill".to_string())
        );
        assert!(
            tmp.path()
                .join("skills")
                .join("grep")
                .join("SKILL.md")
                .exists()
        );

        b.set(&MemoryCategory::Custom, Some("notes"), "note body")
            .await
            .unwrap();
        assert_eq!(
            b.get(&MemoryCategory::Custom, Some("notes")).await.unwrap(),
            Some("note body".to_string())
        );
    }

    #[tokio::test]
    async fn test_edit_unique_match_replaces_text() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Soul, None, "Be friendly. Be concise.")
            .await
            .unwrap();
        let outcome = b
            .edit(&MemoryCategory::Soul, None, "Be friendly.", "Be warm.")
            .await
            .unwrap();
        assert_eq!(outcome, EditOutcome::Replaced);
        assert_eq!(
            b.get(&MemoryCategory::Soul, None).await.unwrap(),
            Some("Be warm. Be concise.".to_string())
        );
    }

    #[tokio::test]
    async fn test_edit_not_found_and_ambiguous() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Soul, None, "repeat repeat.")
            .await
            .unwrap();
        assert_eq!(
            b.edit(&MemoryCategory::Soul, None, "missing", "x")
                .await
                .unwrap(),
            EditOutcome::NotFound
        );
        assert_eq!(
            b.edit(&MemoryCategory::Soul, None, "repeat", "X")
                .await
                .unwrap(),
            EditOutcome::Ambiguous(2)
        );
    }

    #[tokio::test]
    async fn test_edit_missing_document_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert_eq!(
            b.edit(&MemoryCategory::Core, None, "old", "new")
                .await
                .unwrap(),
            EditOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn test_remove_existing_and_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Custom, Some("temp"), "x")
            .await
            .unwrap();
        assert!(
            b.remove(&MemoryCategory::Custom, Some("temp"))
                .await
                .unwrap()
        );
        assert!(
            !b.remove(&MemoryCategory::Custom, Some("temp"))
                .await
                .unwrap()
        );
        assert!(
            !b.remove(&MemoryCategory::Custom, Some("never"))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn test_list_skill_names_ignores_dirs_without_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Skill, Some("memory"), "a")
            .await
            .unwrap();
        b.set(&MemoryCategory::Skill, Some("grep"), "b")
            .await
            .unwrap();
        // A directory without SKILL.md is not a skill.
        std::fs::create_dir_all(tmp.path().join("skills").join("junk")).unwrap();
        let names = b.list_skill_names().await.unwrap();
        assert_eq!(names, vec!["grep", "memory"]);
    }

    #[tokio::test]
    async fn test_list_custom_names_excludes_core_and_daily() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        b.set(&MemoryCategory::Core, None, "core").await.unwrap();
        b.set(&MemoryCategory::Custom, Some("notes"), "n")
            .await
            .unwrap();
        b.set(&MemoryCategory::Custom, Some("project-a"), "p")
            .await
            .unwrap();
        // A date-named file is a Daily document, not custom.
        std::fs::write(tmp.path().join("memory").join("2026-09-26.md"), "daily").unwrap();
        let names = b.list_custom_names().await.unwrap();
        assert_eq!(names, vec!["notes", "project-a"]);
    }

    #[tokio::test]
    async fn test_template_fixed_categories() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert!(b.template(&MemoryCategory::Soul).is_some());
        assert!(b.template(&MemoryCategory::User).is_some());
        assert!(b.template(&MemoryCategory::Agent).is_some());
        assert!(b.template(&MemoryCategory::Tools).is_some());
        // Core has a bootstrap template too, so an untouched MEMORY.md is
        // treated as unmodified by get_effective and never injected.
        assert!(b.template(&MemoryCategory::Core).is_some());
        assert!(b.template(&MemoryCategory::Daily).is_none());
        assert!(b.template(&MemoryCategory::Skill).is_none());
        assert!(b.template(&MemoryCategory::Custom).is_none());
    }

    #[tokio::test]
    async fn test_health_check_and_sync_succeed() {
        let tmp = tempfile::tempdir().unwrap();
        let b = make_backend(tmp.path());
        assert!(b.health_check().await);
        b.set(&MemoryCategory::Core, None, "x").await.unwrap();
        b.sync().await.unwrap();
    }

    #[test]
    fn test_is_daily_stem_recognizes_date_names() {
        assert!(is_daily_stem("2026-09-26"));
        assert!(!is_daily_stem("MEMORY"));
        assert!(!is_daily_stem("notes"));
        assert!(!is_daily_stem("2026-9-26"));
    }
}
