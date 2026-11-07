//! Code-level refusal for protected workspace documents (ADR 0012).
//!
//! The reacting model persists memory by editing `memory/MEMORY.md` with the
//! ordinary file tools, and nothing else. Two sets are refused here, in code,
//! because an accidental rewrite of them is silent and permanent:
//!
//! - the bootstrapped identity documents ([`crate::bootstrap::protected_documents`]), i.e. who the
//!   agent is and how it works;
//! - the documents Dream maintains (`skills/*/SKILL.md`, custom `memory/*.md`).
//!
//! Workspace runtime files (`cron/jobs.json`, `sessions/*.jsonl`,
//! `HEARTBEAT.md`, `memory/history.jsonl`) stay writable on purpose: turning
//! them read-only would be feature removal dressed up as safety. `shell` can
//! still reach any of these files; that gap is documented in `docs/tool.md`
//! and is out of scope for this gate.

use std::path::{Component, Path};

use crate::bootstrap;

/// Which rule protects a document. Only the refusal wording differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedKind {
    /// A bootstrapped identity / built-in document defined by the owner.
    Bootstrap,
    /// A document Dream owns (`skills/*/SKILL.md`, custom `memory/*.md`).
    Dream,
}

/// Classify a workspace-relative document path (`/`-separated).
///
/// Comparison is ASCII case-insensitive: the default macOS file system is
/// case-insensitive, so `soul.md` must not be a way around the gate.
pub fn classify(relative_path: &str) -> Option<ProtectedKind> {
    let lower = relative_path.to_ascii_lowercase();

    // The one writable landing document. Checked first so the `memory/*.md`
    // rule below cannot swallow it.
    if lower == bootstrap::MEMORY_DOCUMENT.to_ascii_lowercase() {
        return None;
    }

    if bootstrap::protected_documents().any(|doc| doc.eq_ignore_ascii_case(relative_path)) {
        return Some(ProtectedKind::Bootstrap);
    }

    if lower.starts_with("skills/") && lower.ends_with("/skill.md") {
        return Some(ProtectedKind::Dream);
    }

    if let Some(rest) = lower.strip_prefix("memory/") {
        // A custom memory document sits directly in `memory/`; deeper paths
        // (`.snapshots/...`) and non-markdown files (`history.jsonl`) are not
        // memory documents.
        if rest.ends_with(".md") && !rest.contains('/') {
            return Some(ProtectedKind::Dream);
        }
    }

    None
}

/// Refusal text for `resolved` when it names a protected document.
///
/// `None` means the target is writable, so the caller proceeds as before.
/// A `Some` value is a normal tool result (never an error): the run keeps
/// going, the model sees why nothing happened and what to do instead.
pub fn refusal_for_write(resolved: &Path, workspace_dir: &Path) -> Option<String> {
    let relative = relative_document_path(resolved, workspace_dir)?;
    let kind = classify(&relative)?;
    Some(refusal_text(&relative, kind))
}

/// Workspace-relative, `/`-separated form of an already-resolved path.
///
/// Returns `None` for anything that is not a plain path inside the workspace;
/// `resolve_workspace_path` rejects escapes before this runs, so a `..` or root
/// component here is not a document we can classify.
fn relative_document_path(resolved: &Path, workspace_dir: &Path) -> Option<String> {
    let root = workspace_dir.canonicalize().ok()?;
    let relative = resolved.strip_prefix(&root).ok()?;

    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn refusal_text(relative: &str, kind: ProtectedKind) -> String {
    let (what, hint) = match kind {
        ProtectedKind::Bootstrap => (
            "a protected identity document -- edits to it are owner-only",
            "tell the owner what you wanted to change",
        ),
        ProtectedKind::Dream => (
            "a Dream-owned document -- edits to it are owner-only",
            "Dream will promote it from there, or tell the owner what you wanted to change",
        ),
    };
    format!(
        "REFUSED: `{relative}` is {what}. Nothing was written. Do not retry with another \
         tool or path; record durable facts in `{}` and {hint}.",
        bootstrap::MEMORY_DOCUMENT
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_classify_each_bootstrap_identity_document_is_protected() {
        for doc in ["AGENTS.md", "SOUL.md", "USER.md", "TOOLS.md"] {
            assert_eq!(classify(doc), Some(ProtectedKind::Bootstrap), "{doc}");
        }
    }

    #[test]
    fn test_classify_memory_document_is_the_writable_funnel() {
        assert_eq!(classify("memory/MEMORY.md"), None);
    }

    #[test]
    fn test_classify_the_connector_reference_is_owner_only() {
        // A reference is host-authored knowledge, the same class as an agent
        // definition: one run must not rewrite the rules the next run reads.
        assert_eq!(
            classify("references/connector.md"),
            Some(ProtectedKind::Bootstrap)
        );
    }

    #[test]
    fn test_classify_runtime_and_ordinary_files_stay_writable() {
        for doc in [
            "memory/history.jsonl",
            "memory/.snapshots/run-1/SOUL.md",
            "memory/2026-11-02.md.bak",
            "cron/jobs.json",
            "sessions/cli:1.jsonl",
            "HEARTBEAT.md",
            "tool-results/ev-1.txt",
            "notes/deploy.md",
        ] {
            assert_eq!(classify(doc), None, "{doc} must stay writable");
        }
    }

    #[test]
    fn test_classify_dream_owned_documents_are_protected() {
        assert_eq!(
            classify("skills/release/SKILL.md"),
            Some(ProtectedKind::Dream)
        );
        assert_eq!(
            classify("skills/nested/dir/SKILL.md"),
            Some(ProtectedKind::Dream)
        );
        assert_eq!(classify("memory/glossary.md"), Some(ProtectedKind::Dream));
    }

    #[test]
    fn test_classify_protected_documents_case_insensitively() {
        // The default macOS file system is case-insensitive: `soul.md` would
        // overwrite `SOUL.md`, so the gate must not be a case-sensitive match.
        assert_eq!(classify("soul.md"), Some(ProtectedKind::Bootstrap));
        assert_eq!(classify("Memory/memory.md"), None);
        assert_eq!(
            classify("skills/release/skill.md"),
            Some(ProtectedKind::Dream)
        );
    }

    #[test]
    fn test_refusal_text_names_the_document_and_the_owner_only_rule() {
        let tmp = workspace();
        let resolved = crate::tools::resolve_workspace_path("SOUL.md", tmp.path()).unwrap();
        let text = refusal_for_write(&resolved, tmp.path()).expect("SOUL.md must be refused");
        assert!(text.contains("REFUSED"), "{text}");
        assert!(text.contains("SOUL.md"), "{text}");
        assert!(text.contains("owner-only"), "{text}");
        assert!(text.contains("Nothing was written"), "{text}");
        assert!(text.contains("memory/MEMORY.md"), "{text}");
    }

    #[test]
    fn test_refusal_text_for_dream_owned_document_points_at_promotion() {
        let tmp = workspace();
        std::fs::create_dir_all(tmp.path().join("skills/release")).unwrap();
        let resolved =
            crate::tools::resolve_workspace_path("skills/release/SKILL.md", tmp.path()).unwrap();
        let text = refusal_for_write(&resolved, tmp.path()).expect("SKILL.md must be refused");
        assert!(text.contains("skills/release/SKILL.md"), "{text}");
        assert!(text.contains("owner-only"), "{text}");
        assert!(text.contains("Dream will promote it"), "{text}");
        assert_eq!(
            text.matches("memory/MEMORY.md").count(),
            1,
            "the landing document must be named once: {text}"
        );
    }

    #[test]
    fn test_refusal_for_write_matches_a_path_that_walks_through_a_subdir() {
        let tmp = workspace();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "rules").unwrap();

        let resolved =
            crate::tools::resolve_workspace_path("sub/../AGENTS.md", tmp.path()).unwrap();
        let text = refusal_for_write(&resolved, tmp.path()).expect("AGENTS.md must be refused");
        assert!(text.contains("AGENTS.md"), "{text}");
    }

    #[test]
    fn test_refusal_for_write_follows_a_symlink_to_a_protected_document() {
        let tmp = workspace();
        std::fs::write(tmp.path().join("TOOLS.md"), "tools").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("TOOLS.md"), tmp.path().join("alias.md"))
            .unwrap();

        let resolved = crate::tools::resolve_workspace_path("alias.md", tmp.path()).unwrap();
        assert!(
            refusal_for_write(&resolved, tmp.path()).is_some(),
            "a symlink must not be a way around the protected set"
        );
    }

    #[test]
    fn test_refusal_for_write_ignores_paths_outside_the_workspace() {
        let tmp = workspace();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("AGENTS.md");
        assert_eq!(refusal_for_write(&target, tmp.path()), None);
    }
}
