use crate::embed::{EMBEDDED_FILES, get_content_by_dest};

/// The five memory document templates written into a new workspace.
/// Returns (workspace-relative dest, content): AGENTS, USER, SOUL, TOOLS in
/// the workspace root, MEMORY in the memory/ dir.
/// `identity.md` / `dream.md` are kept embedded-only (never written to disk);
/// their content is injected directly from the template at build/run time.
pub fn bootstrap_files() -> impl Iterator<Item = (&'static str, &'static str)> {
    [
        ("AGENTS.md", "AGENTS.md"),
        ("USER.md", "USER.md"),
        ("SOUL.md", "SOUL.md"),
        ("TOOLS.md", "TOOLS.md"),
        ("memory/MEMORY.md", "MEMORY.md"),
    ]
    .into_iter()
    .filter_map(|(dest, template)| get_content_by_dest(template).map(|content| (dest, content)))
}

/// Embedded skill files (dest is workspace/skills/).
pub fn skill_files() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    EMBEDDED_FILES
        .iter()
        .filter(|(_, _, dest)| dest.starts_with("skills/"))
        .map(|(name, content, dest)| (*name, *content, *dest))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(names.contains(&"memory/MEMORY.md"));
    }

    #[test]
    fn test_bootstrap_excludes_identity_and_dream() {
        let names: Vec<_> = bootstrap_files().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 5);
        assert!(!names.contains(&"identity.md"));
        assert!(!names.contains(&"dream.md"));
    }

    #[test]
    fn test_skill_files_include_memory() {
        let skills: Vec<_> = skill_files().collect();
        assert!(!skills.is_empty());
        assert!(
            skills
                .iter()
                .any(|(name, _, _dest)| name.contains("SKILL.md"))
        );
        assert!(
            skills
                .iter()
                .all(|(_, _, dest)| dest.starts_with("skills/"))
        );
    }
}
