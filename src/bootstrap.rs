use std::path::Path;

use crate::embed::{EMBEDDED_FILES, get_content_by_dest};

/// All embedded resources: (filename, content, dest_path).
pub fn embedded_files() -> &'static [(&'static str, &'static str, &'static str)] {
    EMBEDDED_FILES
}

/// Returns the file content if it exists and differs from the embedded template,
/// or `None` if the file is missing, unreadable, or matches the template.
pub fn read_if_modified(path: &Path, template: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim() != template.trim() => Some(content),
        _ => None,
    }
}

/// Embedded bootstrap templates (dest is workspace root).
pub fn bootstrap_files() -> impl Iterator<Item = (&'static str, &'static str)> {
    EMBEDDED_FILES
        .iter()
        .filter(|(_, _, dest)| !dest.starts_with("skills/"))
        .map(|(name, content, _)| (*name, *content))
}

/// Embedded skill files (dest is workspace/skills/).
pub fn skill_files() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    EMBEDDED_FILES
        .iter()
        .filter(|(_, _, dest)| dest.starts_with("skills/"))
        .map(|(name, content, dest)| (*name, *content, *dest))
}

/// Get embedded content by filename.
pub fn get_template(name: &str) -> Option<&'static str> {
    EMBEDDED_FILES
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, c, _)| *c)
}

/// Check if file content is identical to the embedded template.
/// Returns true if content matches the template for the given dest_path.
pub fn is_template_content(content: &str, dest_path: &str) -> bool {
    match get_content_by_dest(dest_path) {
        Some(template) => content.trim() == template.trim(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_read_if_modified_returns_none_for_missing_file() {
        assert!(read_if_modified(Path::new("/nonexistent/file.md"), "template").is_none());
    }

    #[test]
    fn test_read_if_modified_returns_none_for_identical_content() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = get_template("AGENTS.md").unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        assert!(read_if_modified(tmp.path(), content).is_none());
    }

    #[test]
    fn test_read_if_modified_returns_some_for_changed_content() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"different content").unwrap();
        let result = read_if_modified(tmp.path(), "original content");
        assert!(result.is_some());
        assert_eq!(result.unwrap(), "different content");
    }

    #[test]
    fn test_read_if_modified_ignores_trailing_whitespace() {
        let content = get_template("AGENTS.md").unwrap();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(format!("{}\n\n  ", content).as_bytes())
            .unwrap();
        assert!(read_if_modified(tmp.path(), content).is_none());
    }

    #[test]
    fn test_templates_are_nonempty() {
        for (name, content) in bootstrap_files() {
            assert!(
                !content.trim().is_empty(),
                "template {} should not be empty",
                name
            );
        }
    }

    #[test]
    fn test_bootstrap_contains_expected_files() {
        let names: Vec<_> = bootstrap_files().map(|(n, _)| n).collect();
        assert!(names.contains(&"AGENTS.md"));
        assert!(names.contains(&"USER.md"));
        assert!(names.contains(&"SOUL.md"));
        assert!(names.contains(&"TOOLS.md"));
    }

    #[test]
    fn test_skill_files_include_memory() {
        let skills: Vec<_> = skill_files().collect();
        assert!(!skills.is_empty());
        assert!(skills.iter().any(|(name, _, _dest)| name.contains("SKILL.md")));
        assert!(skills.iter().all(|(_, _, dest)| dest.starts_with("skills/")));
    }

    #[test]
    fn test_is_template_content_matches_template() {
        let template = get_template("AGENTS.md").unwrap();
        assert!(is_template_content(template, "AGENTS.md"));
        assert!(is_template_content(&format!("{}\n\n  ", template), "AGENTS.md"));
    }

    #[test]
    fn test_is_template_content_differs() {
        assert!(!is_template_content("custom content", "AGENTS.md"));
    }

    #[test]
    fn test_is_template_content_missing_template() {
        assert!(!is_template_content("anything", "nonexistent.md"));
    }
}
