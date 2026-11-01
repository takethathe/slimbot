use crate::embed::{EMBEDDED_FILES, get_content_by_dest};

/// Workspace-relative path of the one memory document the reacting model may
/// write. Every other bootstrapped document is owner-only (ADR 0012).
pub const MEMORY_DOCUMENT: &str = "memory/MEMORY.md";

/// The memory document templates written into a new workspace, plus the one
/// built-in agent definition.
/// Returns (workspace-relative dest, content): AGENTS, USER, SOUL, TOOLS in
/// the workspace root, MEMORY in the memory/ dir, the `trigger` agent in the
/// agents/ dir.
/// `identity.md` / `dream.md` are kept embedded-only (never written to disk);
/// their content is injected directly from the template at build/run time.
pub fn bootstrap_files() -> impl Iterator<Item = (&'static str, &'static str)> {
    [
        ("AGENTS.md", "AGENTS.md"),
        ("USER.md", "USER.md"),
        ("SOUL.md", "SOUL.md"),
        ("TOOLS.md", "TOOLS.md"),
        ("memory/MEMORY.md", "MEMORY.md"),
        ("agents/trigger.md", "agents/trigger.md"),
    ]
    .into_iter()
    .filter_map(|(dest, template)| get_content_by_dest(template).map(|content| (dest, content)))
}

/// Bootstrapped documents that an agent-issued file write must never touch.
///
/// Derived from [`bootstrap_files`] rather than a second hardcoded list, minus
/// [`MEMORY_DOCUMENT`] -- the one document the reacting model writes through
/// the ordinary file tools. `src/tools/protected.rs` turns this set into the
/// code-level refusal; nothing here is prompt text.
pub fn protected_documents() -> impl Iterator<Item = &'static str> {
    bootstrap_files()
        .map(|(dest, _)| dest)
        .filter(|dest| *dest != MEMORY_DOCUMENT)
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
        assert!(names.contains(&"agents/trigger.md"));
    }

    #[test]
    fn test_trigger_agent_template_declares_its_tool() {
        let content = bootstrap_files()
            .find(|(dest, _)| *dest == "agents/trigger.md")
            .map(|(_, content)| content)
            .expect("trigger agent must be bootstrapped");
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: trigger"));
        assert!(content.contains("tools: trigger_source"));
        assert!(content.contains("timeout_secs: 300"));
    }

    #[test]
    fn test_bootstrap_excludes_identity_and_dream() {
        let names: Vec<_> = bootstrap_files().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 6);
        assert!(!names.contains(&"identity.md"));
        assert!(!names.contains(&"dream.md"));
    }

    #[test]
    fn test_protected_documents_are_the_bootstrapped_identity_docs() {
        // One list, not two: the protected set is the bootstrap list minus the
        // single document the reacting model may write (ADR 0012).
        let protected: Vec<_> = protected_documents().collect();
        for doc in ["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"] {
            assert!(protected.contains(&doc), "{doc} must be protected");
        }
        assert!(
            !protected.contains(&MEMORY_DOCUMENT),
            "MEMORY.md is the writable funnel, not a protected document"
        );

        let bootstrapped: Vec<_> = bootstrap_files().map(|(dest, _)| dest).collect();
        assert!(bootstrapped.contains(&MEMORY_DOCUMENT));
        for doc in &protected {
            assert!(
                bootstrapped.contains(doc),
                "{doc} is protected but not bootstrapped"
            );
        }
    }

    #[test]
    fn test_memory_skill_documents_the_provenance_convention() {
        // The provenance note is a writing convention carried by the prompt
        // surface; no parser enforces it, so this text is the whole contract.
        let (_, skill, _) = skill_files()
            .find(|(_, _, dest)| *dest == "skills/memory/SKILL.md")
            .expect("the memory skill must be bootstrapped");
        assert!(
            skill.contains("MEMORY.md"),
            "the skill must name the funnel"
        );
        assert!(
            skill.contains("source"),
            "the skill must document the provenance source note"
        );
        // Ticket 06 #7: provenance keeps the source but never a date — age is
        // derived from snapshots, so a written date would be a restatable lie.
        assert!(
            !skill.contains("YYYY-MM-DD"),
            "the provenance note must no longer prescribe a date format"
        );
        assert!(
            skill.contains("digest"),
            "the skill must ask for digested text, not raw content"
        );
    }

    #[test]
    fn test_memory_template_is_the_agent_writable_landing_document() {
        let (_, template) = bootstrap_files()
            .find(|(dest, _)| *dest == MEMORY_DOCUMENT)
            .expect("MEMORY.md must be bootstrapped");
        assert!(
            !template.contains("Do NOT edit"),
            "the template must not forbid the write path it exists for"
        );
        assert!(template.contains("MEMORY.md"));
        assert!(template.contains("provenance"));
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
