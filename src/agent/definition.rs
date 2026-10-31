//! Agent definitions: `agents/<name>.md` scanning, frontmatter parsing and
//! validation.
//!
//! The frontmatter declares only the *extra* tools an agent needs on top of
//! the default set, so adding a tool to `config.tools` never invalidates every
//! definition. Declaring a tool that does not exist — or one of the two
//! reserved tools — rejects the whole agent: a half-capable agent fails in the
//! middle of a task in a way that looks like the model misusing a tool.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::ToolEntry;

/// Tools a subagent may never declare. `cron` would let a task create timers
/// behind the trigger layer's back; `subagent` would let the concurrency cap
/// multiply itself.
pub const FORBIDDEN_AGENT_TOOLS: &[&str] = &["cron", "subagent"];

/// Default subagent tool set: the configured tools (or the nine built-ins),
/// plus `message` and `memory_search`, minus the reserved tools.
pub fn default_tool_set(configured: &[ToolEntry]) -> Vec<String> {
    let mut names = super::catalog::effective_builtin_names(configured);
    names.push("message".to_string());
    names.push("memory_search".to_string());
    names.retain(|n| !FORBIDDEN_AGENT_TOOLS.contains(&n.as_str()));
    names.sort();
    names.dedup();
    names
}

/// A validated agent definition.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    /// Matches the file stem; also the `subagent` tool's `agent` argument.
    pub name: String,
    /// One line, rendered in `# Available Agents`.
    pub description: String,
    /// Tools appended to the default set.
    pub tools: Vec<String>,
    /// Whether finished runs are delivered back to the origin conversation.
    pub deliver: bool,
    /// Hard per-run timeout.
    pub timeout_secs: u64,
    /// The markdown body, used verbatim as the system prompt.
    pub system_prompt: String,
    pub path: PathBuf,
}

impl AgentDefinition {
    /// Effective tool names for one run: default set plus declared increments.
    pub fn tool_names(&self, default_set: &[String]) -> Vec<String> {
        let mut names: Vec<String> = default_set.to_vec();
        names.extend(self.tools.iter().cloned());
        names.sort();
        names.dedup();
        names
    }
}

/// Why a definition was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDefError {
    MissingName,
    MissingDescription,
    UnknownTool(String),
    ForbiddenTool(String),
}

impl std::fmt::Display for AgentDefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentDefError::MissingName => write!(f, "missing `name` in frontmatter"),
            AgentDefError::MissingDescription => write!(f, "missing `description` in frontmatter"),
            AgentDefError::UnknownTool(t) => write!(f, "declares unregistered tool '{}'", t),
            AgentDefError::ForbiddenTool(t) => write!(f, "declares reserved tool '{}'", t),
        }
    }
}

/// Result of scanning the agents directory: usable definitions plus the
/// rejections that were logged instead of silently dropped.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
    rejected: Vec<(String, AgentDefError)>,
}

impl AgentRegistry {
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents.values()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn rejected(&self) -> &[(String, AgentDefError)] {
        &self.rejected
    }

    /// Markdown body of `# Available Agents`, or `None` when no agent is
    /// usable (the whole section is omitted then).
    pub fn prompt_section(&self) -> Option<String> {
        if self.agents.is_empty() {
            return None;
        }
        let mut lines = vec![
            "# Available Agents".to_string(),
            String::new(),
            "A subagent runs a task in its own session with its own tool set and reports back here later."
                .to_string(),
            "Start one with the `subagent` tool; the call returns immediately.".to_string(),
            String::new(),
        ];
        for agent in self.agents.values() {
            lines.push(format!("- **{}** — {}", agent.name, agent.description));
        }
        lines.push(String::new());
        lines.push(
            "Reach for a subagent when the task needs a tool you don't have, or when it should outlive this turn."
                .to_string(),
        );
        Some(lines.join("\n"))
    }
}

/// Parse frontmatter + body of an agent definition file.
///
/// `stem` is the file name without `.md`; it is the canonical agent name, so a
/// mismatched `name:` field cannot shadow another file.
fn parse_agent(content: &str, stem: &str, path: &Path) -> Result<AgentDefinition, AgentDefError> {
    let (front, body) = split_frontmatter(content).ok_or(AgentDefError::MissingName)?;
    let mut description = String::new();
    let mut declared_name = String::new();
    let mut tools: Vec<String> = Vec::new();
    let mut deliver = true;
    let mut timeout_secs = 600u64;

    for line in front.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name:") {
            declared_name = rest.trim().trim_matches('"').to_string();
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = rest.trim().trim_matches('"').to_string();
        } else if let Some(rest) = line.strip_prefix("tools:") {
            tools = parse_list(rest);
        } else if let Some(rest) = line.strip_prefix("deliver:") {
            deliver = rest.trim() != "false";
        } else if let Some(rest) = line.strip_prefix("timeout_secs:")
            && let Ok(v) = rest.trim().parse::<u64>()
        {
            timeout_secs = v.max(1);
        }
    }

    if declared_name.is_empty() {
        return Err(AgentDefError::MissingName);
    }
    if description.is_empty() {
        return Err(AgentDefError::MissingDescription);
    }

    Ok(AgentDefinition {
        name: stem.to_string(),
        description,
        tools,
        deliver,
        timeout_secs,
        system_prompt: body.trim().to_string(),
        path: path.to_path_buf(),
    })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---\n")?;
    Some((&rest[..end], &rest[end + 5..]))
}

fn parse_list(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Validate `def.tools` against the known tool names and the reserved list.
pub fn validate_tools(def: &AgentDefinition, known: &[String]) -> Result<(), AgentDefError> {
    for tool in &def.tools {
        if FORBIDDEN_AGENT_TOOLS.contains(&tool.as_str()) {
            return Err(AgentDefError::ForbiddenTool(tool.clone()));
        }
        if !known.iter().any(|k| k == tool) {
            return Err(AgentDefError::UnknownTool(tool.clone()));
        }
    }
    Ok(())
}

/// Load every `*.md` under `agents_dir`, validating against `known` tool names.
/// Rejected definitions are collected (and logged by the caller) rather than
/// aborting the scan, so one broken file never hides the rest.
pub fn load_registry(agents_dir: &Path, known: &[String]) -> AgentRegistry {
    let mut registry = AgentRegistry::default();
    let entries = match std::fs::read_dir(agents_dir) {
        Ok(entries) => entries,
        Err(_) => return registry,
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "md").unwrap_or(false))
        .collect();
    paths.sort();

    for path in paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                crate::error!("[agent] failed to read {}: {}", path.display(), e);
                continue;
            }
        };
        match parse_agent(&content, &stem, &path).and_then(|def| {
            validate_tools(&def, known)?;
            Ok(def)
        }) {
            Ok(def) => {
                registry.agents.insert(def.name.clone(), def);
            }
            Err(err) => {
                crate::error!("[agent] rejected definition {}: {}", path.display(), err);
                registry.rejected.push((stem, err));
            }
        }
    }
    registry
}

/// Load the registry for a workspace, using the process-wide known tool names.
pub fn load_workspace_registry(workspace_dir: &Path) -> AgentRegistry {
    load_registry(
        &workspace_dir.join("agents"),
        &super::catalog::known_tool_names(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agent(dir: &Path, file: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
    }

    fn known() -> Vec<String> {
        crate::agent::known_tool_names()
    }

    #[test]
    fn test_load_registry_parses_definition() {
        let tmp = tempfile::tempdir().unwrap();
        write_agent(
            &tmp.path().join("agents"),
            "news.md",
            "---\nname: news\ndescription: Fetch and summarize news.\ntools: web_fetch, file_reader\ndeliver: false\ntimeout_secs: 120\n---\n\nYou are a news agent.\n",
        );
        let reg = load_registry(&tmp.path().join("agents"), &known());
        let def = reg.get("news").expect("news agent must load");
        assert_eq!(def.description, "Fetch and summarize news.");
        assert_eq!(
            def.tools,
            vec!["web_fetch".to_string(), "file_reader".to_string()]
        );
        assert!(!def.deliver);
        assert_eq!(def.timeout_secs, 120);
        assert_eq!(def.system_prompt, "You are a news agent.");
    }

    #[test]
    fn test_load_registry_skips_missing_name_or_description() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        write_agent(&agents, "noname.md", "---\ndescription: x\n---\nbody\n");
        write_agent(&agents, "nodesc.md", "---\nname: nodesc\n---\nbody\n");
        write_agent(
            &agents,
            "fine.md",
            "---\nname: fine\ndescription: ok\n---\nbody\n",
        );
        let reg = load_registry(&agents, &known());
        assert!(reg.get("noname").is_none());
        assert!(reg.get("nodesc").is_none());
        assert!(reg.get("fine").is_some(), "valid definitions still load");
        assert_eq!(reg.rejected().len(), 2);
    }

    #[test]
    fn test_load_registry_rejects_unknown_tool() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        write_agent(
            &agents,
            "bad.md",
            "---\nname: bad\ndescription: x\ntools: made_up_tool\n---\nbody\n",
        );
        let reg = load_registry(&agents, &known());
        assert!(reg.get("bad").is_none());
        assert_eq!(
            reg.rejected()[0].1,
            AgentDefError::UnknownTool("made_up_tool".into())
        );
    }

    #[test]
    fn test_load_registry_rejects_reserved_tools() {
        for tool in ["cron", "subagent"] {
            let dir = tempfile::tempdir().unwrap();
            let agents = dir.path().join("agents");
            write_agent(
                &agents,
                "bad.md",
                &format!(
                    "---\nname: bad\ndescription: x\ntools: {}\n---\nbody\n",
                    tool
                ),
            );
            let reg = load_registry(&agents, &known());
            assert!(reg.get("bad").is_none(), "{} must be rejected", tool);
            assert_eq!(
                reg.rejected()[0].1,
                AgentDefError::ForbiddenTool(tool.to_string())
            );
        }
    }

    #[test]
    fn test_prompt_section_omitted_when_empty() {
        let reg = load_registry(Path::new("/nonexistent/agents"), &known());
        assert!(reg.prompt_section().is_none());
    }

    #[test]
    fn test_prompt_section_lists_agents_with_description() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        write_agent(
            &agents,
            "trigger.md",
            "---\nname: trigger\ndescription: Create triggers.\n---\nbody\n",
        );
        let reg = load_registry(&agents, &known());
        let section = reg.prompt_section().unwrap();
        assert!(section.starts_with("# Available Agents"));
        assert!(section.contains("- **trigger** — Create triggers."));
        assert!(section.contains("`subagent` tool"));
    }

    #[test]
    fn test_default_tool_set_contains_message_and_memory_search_only() {
        let set = default_tool_set(&[]);
        assert!(set.contains(&"message".to_string()));
        assert!(set.contains(&"memory_search".to_string()));
        assert!(!set.contains(&"cron".to_string()));
        assert!(!set.contains(&"subagent".to_string()));
        // Nine built-ins + the two always-available tools.
        assert_eq!(set.len(), 11);
    }

    #[test]
    fn test_default_tool_set_follows_config() {
        let entries = vec![ToolEntry {
            name: "grep".into(),
            enabled: true,
        }];
        let set = default_tool_set(&entries);
        assert_eq!(
            set,
            vec![
                "grep".to_string(),
                "memory_search".to_string(),
                "message".to_string()
            ]
        );
    }

    #[test]
    fn test_tool_names_appends_increments() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("agents");
        write_agent(
            &agents,
            "a.md",
            "---\nname: a\ndescription: d\ntools: web_fetch\n---\nbody\n",
        );
        let reg = load_registry(&agents, &known());
        let def = reg.get("a").unwrap();
        let names = def.tool_names(&default_tool_set(&[]));
        assert!(names.contains(&"web_fetch".to_string()));
        assert_eq!(names.len(), 11, "increments deduplicate against defaults");
    }
}
