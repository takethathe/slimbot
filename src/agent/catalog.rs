//! Process-level tool catalog: name → factory.
//!
//! `ToolManager` freezes its tool table (and the cached function descriptions)
//! at registration time and is shared through `Arc` by concurrent runs, so a
//! run cannot add or remove tools safely. Instead, every run assembles its own
//! `ToolManager` from this catalog; the main loop's tool list never changes and
//! its prompt cache stays stable.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::tool::Tool;

/// Factory producing a fresh tool instance for a single run.
pub type ToolFactory = Arc<dyn Fn() -> Option<Box<dyn Tool>> + Send + Sync>;

/// Names every SlimBot build can construct, used to validate agent
/// definitions even before the catalog is assembled (e.g. while building a
/// system prompt). `message` / `memory_search` / trigger tools are registered
/// by the wiring layer rather than by `config.tools`.
const STATIC_TOOL_NAMES: &[&str] = &[
    "shell",
    "file_reader",
    "file_writer",
    "file_editor",
    "list_dir",
    "make_dir",
    "grep",
    "find_files",
    "web_fetch",
    "message",
    "memory_search",
    "trigger",
    "trigger_source",
];

pub struct ToolCatalog {
    factories: HashMap<String, ToolFactory>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a tool factory under `name`.
    pub fn register(&mut self, name: &str, factory: ToolFactory) {
        self.factories.insert(name.to_string(), factory);
    }

    /// Register a built-in tool by name, resolved against `workspace_dir`.
    pub fn register_builtin(&mut self, name: &str, workspace_dir: &Path) {
        let ws = workspace_dir.to_path_buf();
        let tool_name = name.to_string();
        self.register(
            name,
            Arc::new(move || crate::tools::create_tool(&tool_name, &ws)),
        );
    }

    /// Register the configured tool set (all built-ins when the list is empty).
    pub fn register_builtin_set(&mut self, entries: &[crate::config::ToolEntry], ws: &Path) {
        for name in effective_builtin_names(entries) {
            self.register_builtin(&name, ws);
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.factories.keys().cloned().collect();
        names.sort();
        names
    }

    /// Assemble a `ToolManager` containing exactly `wanted`, skipping names the
    /// catalog does not know (validation happened before the run started).
    pub fn build_tool_manager(
        &self,
        wanted: &[String],
        workspace_dir: &Path,
    ) -> crate::tool::ToolManager {
        let mut tm = crate::tool::ToolManager::new(workspace_dir.to_path_buf());
        for name in wanted {
            match self.factories.get(name) {
                Some(factory) => {
                    if let Some(tool) = factory() {
                        tm.register(tool);
                    } else {
                        crate::warn_log!("[agent] tool '{}' could not be built", name);
                    }
                }
                None => crate::warn_log!("[agent] tool '{}' is not in the catalog", name),
            }
        }
        tm
    }

    /// Install the catalog used by prompt building (agent validation) and by
    /// the subagent runtime. Only the first installation wins; later calls are
    /// ignored so a test cannot clobber production wiring.
    pub fn install_global(catalog: Arc<ToolCatalog>) {
        let _ = GLOBAL_CATALOG.set(catalog);
    }

    pub fn global() -> Option<Arc<ToolCatalog>> {
        GLOBAL_CATALOG.get().cloned()
    }
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_CATALOG: OnceLock<Arc<ToolCatalog>> = OnceLock::new();

/// Names that may be referenced by an agent definition.
pub fn known_tool_names() -> Vec<String> {
    let mut names: Vec<String> = STATIC_TOOL_NAMES.iter().map(|s| s.to_string()).collect();
    if let Some(catalog) = ToolCatalog::global() {
        names.extend(catalog.names());
    }
    names.sort();
    names.dedup();
    names
}

/// The nine built-in tools enabled when `config.tools` is empty.
pub(crate) fn effective_builtin_names(entries: &[crate::config::ToolEntry]) -> Vec<String> {
    const DEFAULT_NAMES: &[&str] = &[
        "shell",
        "file_reader",
        "file_writer",
        "file_editor",
        "list_dir",
        "make_dir",
        "grep",
        "find_files",
        "web_fetch",
    ];
    if entries.is_empty() {
        return DEFAULT_NAMES.iter().map(|s| s.to_string()).collect();
    }
    entries
        .iter()
        .filter(|e| e.enabled)
        .map(|e| e.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_tool_names_include_runtime_registered_tools() {
        let names = known_tool_names();
        for expected in [
            "message",
            "memory_search",
            "trigger",
            "trigger_source",
            "web_fetch",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{} must be a known tool name",
                expected
            );
        }
    }

    #[test]
    fn test_build_tool_manager_only_contains_requested_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let mut catalog = ToolCatalog::new();
        catalog.register_builtin("shell", tmp.path());
        catalog.register_builtin("list_dir", tmp.path());

        let tm = catalog.build_tool_manager(&["shell".to_string()], tmp.path());
        let names: Vec<_> = tm
            .to_openai_functions()
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(names, vec!["shell".to_string()]);
    }

    #[test]
    fn test_effective_builtin_names_defaults_to_nine() {
        assert_eq!(effective_builtin_names(&[]).len(), 9);
    }

    #[test]
    fn test_effective_builtin_names_respects_enabled_flag() {
        let entries = vec![
            crate::config::ToolEntry {
                name: "shell".into(),
                enabled: true,
            },
            crate::config::ToolEntry {
                name: "grep".into(),
                enabled: false,
            },
        ];
        assert_eq!(effective_builtin_names(&entries), vec!["shell".to_string()]);
    }
}
