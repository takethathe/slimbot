use crate::embed::{EMBEDDED_FILES, get_content_by_dest};

/// Workspace-relative path of the one memory document the reacting model may
/// write. Every other bootstrapped document is owner-only (ADR 0012).
pub const MEMORY_DOCUMENT: &str = "memory/MEMORY.md";

/// The memory document templates written into a new workspace, the two
/// built-in agent definitions, and the connector reference the two agent
/// definitions point at.
/// Returns (workspace-relative dest, content): AGENTS, USER, SOUL, TOOLS in
/// the workspace root, MEMORY in the memory/ dir, the `trigger` and `executor`
/// agents in the agents/ dir, and the shared connector contract under
/// references/.
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
        ("agents/executor.md", "agents/executor.md"),
        ("references/connector.md", "references/connector.md"),
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
        assert!(names.contains(&"agents/executor.md"));
        assert!(names.contains(&"references/connector.md"));
    }

    #[test]
    fn test_connector_reference_is_bootstrapped_knowledge() {
        // Ticket 18: the shared connector contract is a plain bootstrapped
        // document, reached by a pointer from the two agent definitions.
        let content = bootstrap_files()
            .find(|(dest, _)| *dest == "references/connector.md")
            .map(|(_, content)| content)
            .expect("the connector reference must be bootstrapped");
        assert!(content.contains("searchTools"), "{content}");
        assert!(content.contains("mcp__<server>__<tool>"), "{content}");
    }

    #[test]
    fn test_connector_reference_never_reaches_available_skills() {
        // A skill is listed under `# Available Skills`, which every system
        // prompt carries — the main loop included. A reference must never be
        // one, so nothing under references/ may be a skill file.
        assert!(
            !skill_files().any(|(_, _, dest)| dest.starts_with("references/")),
            "a references/ document must never be a skill file"
        );
    }

    #[test]
    fn test_both_agent_templates_point_at_the_connector_reference() {
        // A pointer, not a copy: the agent body is the subagent's system prompt,
        // so the line costs context only inside that run.
        for dest in ["agents/trigger.md", "agents/executor.md"] {
            let body = bootstrap_files()
                .find(|(d, _)| *d == dest)
                .map(|(_, body)| body)
                .unwrap_or_else(|| panic!("{dest} must be bootstrapped"));
            assert!(
                body.contains("file_reader"),
                "{dest} must name the read tool"
            );
            assert!(
                body.contains("references/connector.md"),
                "{dest} must point at the shared reference"
            );
        }
    }

    #[test]
    fn test_the_connector_contract_has_exactly_one_home() {
        // Two competing sources of the same rules is how a subagent starts
        // inventing tool names: the mechanism lives in the reference, the role
        // steps live in the agent files.
        for dest in ["agents/trigger.md", "agents/executor.md"] {
            let body = bootstrap_files()
                .find(|(d, _)| *d == dest)
                .map(|(_, body)| body)
                .unwrap();
            for mechanism in ["searchTools", "mcp__", "ctx.tools"] {
                assert!(
                    !body.contains(mechanism),
                    "{dest} restates the {mechanism} mechanism; it belongs to the reference"
                );
            }
        }

        let reference = bootstrap_files()
            .find(|(d, _)| *d == "references/connector.md")
            .map(|(_, body)| body)
            .unwrap();
        for step in ["trigger_source", "on_fire", "every_n_ticks", "ctx.args"] {
            assert!(
                !reference.contains(step),
                "the reference carries the role-specific step `{step}`"
            );
        }
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
    fn test_executor_agent_template_declares_its_tool() {
        let content = bootstrap_files()
            .find(|(dest, _)| *dest == "agents/executor.md")
            .map(|(_, content)| content)
            .expect("executor agent must be bootstrapped");
        assert!(content.starts_with("---\n"));
        assert!(content.contains("name: executor"));
        assert!(content.contains("tools: executor"));
        // `deliver` is true unless the literal is `false`, so a typo would
        // silently keep delivering; state it explicitly.
        assert!(content.contains("deliver: true"));
        assert!(content.contains("timeout_secs: 180"));
    }

    #[test]
    fn test_bootstrap_excludes_identity_and_dream() {
        let names: Vec<_> = bootstrap_files().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 8);
        assert!(names.contains(&"agents/executor.md"));
        assert!(names.contains(&"references/connector.md"));
        assert!(!names.contains(&"identity.md"));
        assert!(!names.contains(&"dream.md"));
    }

    #[test]
    fn test_protected_documents_are_the_bootstrapped_identity_docs() {
        // One list, not two: the protected set is the bootstrap list minus the
        // single document the reacting model may write (ADR 0012).
        let protected: Vec<_> = protected_documents().collect();
        for doc in [
            "AGENTS.md",
            "SOUL.md",
            "USER.md",
            "TOOLS.md",
            "agents/trigger.md",
            "agents/executor.md",
            "references/connector.md",
        ] {
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
