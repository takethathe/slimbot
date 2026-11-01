pub mod cron;
pub mod file_editor;
pub mod file_reader;
pub mod file_writer;
pub mod find_files;
pub mod grep;
pub mod list_dir;
pub mod make_dir;
pub mod memory_search;
pub mod memory_tools;
pub mod message;
pub mod protected;
pub mod shell;
pub mod web_fetch;

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

    let ancestor_abs = ancestor
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot resolve base directory for '{}': {}", user_path, e))?;

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
        "file_reader" => Some(Box::new(file_reader::FileReaderTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "file_writer" => Some(Box::new(file_writer::FileWriterTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "file_editor" => Some(Box::new(file_editor::FileEditorTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "grep" => Some(Box::new(grep::GrepTool::new(workspace_dir.to_path_buf()))),
        "find_files" => Some(Box::new(find_files::FindFilesTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "list_dir" => Some(Box::new(list_dir::ListDirTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "make_dir" => Some(Box::new(make_dir::MakeDirTool::new(
            workspace_dir.to_path_buf(),
        ))),
        "web_fetch" => {
            // Read WebFetchConfig from the process-wide Config singleton.
            // Config::init() must be called before tool construction (it is,
            // in gateway/runner startup). If a caller builds tools before init
            // (e.g. unit tests), try_get() returns None and the tool falls
            // back to WebFetchConfig::default() -- safe, but config.json
            // settings are not applied. Threading &Config through the factory
            // signature would remove this global, at the cost of changing
            // ~30 call sites; left as a documented trade-off.
            let web_fetch_config = match crate::config::Config::try_get() {
                Some(c) => c.web_fetch.clone(),
                None => {
                    crate::debug!("[tools] Config not initialized; web_fetch uses default config");
                    None
                }
            };
            Some(Box::new(web_fetch::WebFetchTool::new(
                web_fetch_config.as_ref(),
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_tools_self_report_context_exemption() {
        // Exemption is a self-report from the tool, not a central name list: a
        // tool that bounds its own output says so, everything else is trimmed.
        let tmp = tempfile::tempdir().unwrap();
        let exempt = |name: &str| {
            create_tool(name, tmp.path())
                .unwrap_or_else(|| panic!("tool {name} not registered"))
                .context_exempt()
        };

        // `memory_search` and `message` are registered by the agent loop, so
        // their self-report is covered by their own module tests.
        assert!(exempt("file_reader"), "file_reader bounds its own output");
        assert!(!exempt("web_fetch"), "a lost page is only a refetch");
        assert!(!exempt("shell"), "shell output is trimmed under pressure");
        assert!(!exempt("grep"), "grep output is trimmed under pressure");
    }

    #[test]
    fn test_builtin_tools_declare_their_risk_class() {
        // Risk is a fact the tool states about itself; this table is the one
        // place that would break if a tool's class drifted (ADR 0011).
        let tmp = tempfile::tempdir().unwrap();
        let risk = |name: &str| {
            create_tool(name, tmp.path())
                .unwrap_or_else(|| panic!("tool {name} not registered"))
                .risk_level()
        };

        use crate::approval::RiskLevel::*;
        assert_eq!(risk("file_reader"), ReadOnly);
        assert_eq!(risk("list_dir"), ReadOnly);
        assert_eq!(risk("grep"), ReadOnly);
        assert_eq!(risk("find_files"), ReadOnly);
        assert_eq!(risk("file_writer"), WorkspaceWrite);
        assert_eq!(risk("file_editor"), WorkspaceWrite);
        assert_eq!(risk("make_dir"), WorkspaceWrite);
        assert_eq!(risk("shell"), Exec);
        assert_eq!(risk("web_fetch"), NetworkEgress);
    }

    // -- memory write funnel (ADR 0012) ------------------------------------

    #[test]
    fn test_tool_factory_registers_no_memory_write_tool() {
        // The reacting model persists memory by editing one document with the
        // ordinary file tools. A memory-write tool here would be a second
        // write path with its own arguments to get wrong; Dream keeps its own
        // `memory_write` / `memory_edit` inside its sandbox
        // (`DreamService::build_tools`), which is a different registry.
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            "memory_write",
            "memory_append",
            "memory_edit",
            "memory_store",
            "memory_forget",
            "memory_supersede",
        ] {
            assert!(
                create_tool(name, tmp.path()).is_none(),
                "`{name}` must not be offered to the agent"
            );
        }
        assert!(create_tool("file_editor", tmp.path()).is_some());
        assert!(create_tool("file_writer", tmp.path()).is_some());
    }

    #[tokio::test]
    async fn test_memory_write_funnel_keeps_verbatim_external_blob_out_of_memory() {
        // The smallest honest seam: the file tools that write MEMORY.md plus
        // the upstream bound that already reduces external content to evidence
        // (ADR 0009). No content filter is invented here -- see
        // docs/memory.md for the residual risk on the unbounded paths.
        use crate::trigger::subscription::evidence::{
            EVIDENCE_TEXT_MAX_CHARS, clamp_verdict, evidence_block,
        };

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path();

        // A fresh workspace: the bootstrapped MEMORY.md template is the document
        // the agent will edit.
        let (_, template) = crate::bootstrap::bootstrap_files()
            .find(|(dest, _)| *dest == crate::bootstrap::MEMORY_DOCUMENT)
            .expect("MEMORY.md must be bootstrapped");
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        std::fs::write(
            workspace.join(crate::bootstrap::MEMORY_DOCUMENT),
            format!("{template}\n- no durable facts yet\n"),
        )
        .unwrap();

        // An external source hands over a whole page; the sentinel lives in the
        // tail, past the evidence budget.
        let sentinel = "zqrawblobzq";
        let filler = "The whole page text is quoted here. ".repeat(300);
        let raw = format!("Release notes: v2 ships Monday. {filler}{sentinel}");
        assert!(raw.chars().count() > EVIDENCE_TEXT_MAX_CHARS * 10);

        let clamped = clamp_verdict(&serde_json::json!({
            "fire": true,
            "detail": "release feed",
            "evidence": [{"source_id": "release-feed", "at": 1_760_000_000_000i64, "text": raw}],
        }))
        .unwrap();
        let visible = evidence_block(&clamped.evidence);
        assert!(visible.chars().count() <= EVIDENCE_TEXT_MAX_CHARS + 64);
        assert!(
            !visible.contains(sentinel),
            "the tail of an external payload must not reach the model"
        );

        // The raw payload stays recoverable at its reference path (ADR 0013),
        // outside the memory corpus.
        let reference = "tool-results/ev-1.txt";
        create_tool("file_writer", workspace)
            .unwrap()
            .execute(serde_json::json!({"path": reference, "content": raw}))
            .await
            .unwrap();

        // The write the convention asks for: `file_editor` adds one digested
        // bullet with a provenance note to the one writable document.
        let added = create_tool("file_editor", workspace)
            .unwrap()
            .execute(serde_json::json!({
                "path": "memory/MEMORY.md",
                "old_string": "- no durable facts yet",
                "new_string": "- Release feed now requires two named approvers (source: release-feed · 2026-11-02)",
            }))
            .await
            .unwrap();
        assert!(added.contains("1 replacement"), "result was: {added}");

        let memory = tokio::fs::read_to_string(workspace.join("memory/MEMORY.md"))
            .await
            .unwrap();
        assert!(
            !memory.contains("no durable facts yet"),
            "the placeholder must be gone"
        );
        assert!(memory.contains("two named approvers"));
        assert!(memory.contains("release-feed"), "provenance must survive");
        assert!(!memory.contains(sentinel), "memory held raw external text");
        assert!(!memory.contains("The whole page text is quoted here"));
        assert!(memory.len() < raw.len() / 10);
        assert_eq!(
            tokio::fs::read_to_string(workspace.join(reference))
                .await
                .unwrap(),
            raw,
            "the raw payload must stay at its reference path"
        );

        // Retrieval sees the digest only: the corpus is memory documents, not
        // raw artifacts or history.
        let mut memory_service = crate::memory::MemoryService::new(workspace);
        memory_service.init().unwrap();
        let hits = memory_service
            .search("approvers", None, None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.source, "memory/MEMORY.md");
        assert!(
            memory_service
                .search(sentinel, None, None)
                .await
                .unwrap()
                .is_empty(),
            "the verbatim payload must not be retrievable as memory"
        );
    }

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
