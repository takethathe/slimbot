use std::path::PathBuf;
use std::sync::Arc;

use crate::bootstrap::{bootstrap_files, read_if_modified};
use crate::memory::MemoryStore;
use crate::session::{Message, SharedSessionManager};
use crate::tool::{ToolDefinition, ToolManager};

pub struct RunContext {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
}

/// Parsed YAML frontmatter from a skill file.
#[derive(Debug, Clone)]
struct SkillMeta {
    name: String,
    description: String,
    always: bool,
    content: String,
}

/// Parse YAML frontmatter from a skill file.
/// Returns None if the file has no valid `---` delimiters.
fn parse_skill_frontmatter(content: &str) -> Option<SkillMeta> {
    if !content.starts_with("---\n") {
        return None;
    }
    let rest = &content[4..];
    let end = rest.find("\n---\n")?;
    let front = &rest[..end];
    let body = &rest[end + 5..];

    let mut name = String::new();
    let mut description = String::new();
    let mut always = false;

    for line in front.lines() {
        let line = line.trim();
        if line.starts_with("name:") {
            name = line["name:".len()..].trim().to_string();
        } else if line.starts_with("description:") {
            description = line["description:".len()..].trim().to_string();
        } else if line.starts_with("always:") {
            always = line["always:".len()..].trim() == "true";
        }
    }

    Some(SkillMeta {
        name: if name.is_empty() {
            "unknown".to_string()
        } else {
            name
        },
        description,
        always,
        content: body.trim().to_string(),
    })
}

pub struct ContextBuilder {
    session_manager: SharedSessionManager,
    tool_manager: Arc<ToolManager>,
    workspace_dir: PathBuf,
    memory_store: Arc<MemoryStore>,
}

impl ContextBuilder {
    pub fn new(
        session_manager: SharedSessionManager,
        tool_manager: Arc<ToolManager>,
        workspace_dir: PathBuf,
        memory_store: Arc<MemoryStore>,
    ) -> Self {
        Self {
            session_manager,
            tool_manager,
            workspace_dir,
            memory_store,
        }
    }

    pub async fn build(&self, session_id: &str, channel_inject: Option<String>) -> RunContext {
        let mut system_parts: Vec<String> = Vec::new();

        // 1. Fixed brief intro
        system_parts.push(Self::fixed_intro());

        // 2. Bootstrap workspace files (skip if unmodified from template)
        for (filename, template) in bootstrap_files() {
            let path = self.workspace_dir.join(filename);
            if let Some(content) = read_if_modified(&path, template) {
                if !content.is_empty() {
                    system_parts.push(format!("[{}] {}", filename, content));
                }
            }
        }

        // 3. Skills import — parse frontmatter, load always skills fully, list others
        let skills_dir = self.workspace_dir.join("skills");
        if skills_dir.exists() {
            let mut always_skills: Vec<String> = Vec::new();
            let mut available_skills: Vec<String> = Vec::new();

            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map_or(false, |e| e == "md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if content.is_empty() {
                                continue;
                            }
                            if let Some(meta) = parse_skill_frontmatter(&content) {
                                if meta.always {
                                    always_skills.push(meta.content);
                                } else {
                                    available_skills.push(format!(
                                        "- **{}**: {}",
                                        meta.name, meta.description
                                    ));
                                }
                            } else {
                                // No frontmatter — treat as legacy skill, load fully
                                let name = path
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                always_skills.push(format!("[Skill: {}]\n{}", name, content));
                            }
                        }
                    }
                }
            }

            if !always_skills.is_empty() {
                system_parts.push("# Active Skills\n\n".to_string() + &always_skills.join("\n\n---\n\n"));
            }
            if !available_skills.is_empty() {
                let list = available_skills.join("\n");
                system_parts.push(format!(
                    "# Available Skills\n\nThe following skills are available. Use file_reader to load them when needed.\n{list}"
                ));
            }
        }

        // 4. Memory (long-term facts from MEMORY.md, skip if matches template)
        let memory_content = self.memory_store.get_memory_context();
        if !memory_content.is_empty() {
            system_parts.push(memory_content);
        }

        // 5. Recent history from history.jsonl (last 50 entries, nanobot default)
        let recent_entries = self.memory_store.read_recent_history(50);
        if !recent_entries.is_empty() {
            let history_text = recent_entries
                .iter()
                .map(|e| format!("- [{}] {}", e.timestamp, e.content))
                .collect::<Vec<_>>()
                .join("\n");
            system_parts.push(format!("# Recent History\n\n{history_text}"));
        }

        // 6. History messages (unconsolidated session messages)
        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(session_id).await
        };

        // 7. Channel self-injected info
        if let Some(inject) = channel_inject {
            if !inject.is_empty() {
                system_parts.push(inject);
            }
        }

        // Assemble system prompt
        let system_prompt = system_parts.join("\n\n---\n\n");
        let mut all_messages = vec![Message::System {
            content: system_prompt,
        }];
        all_messages.extend(messages);

        let tools = Some(self.tool_manager.to_openai_functions());

        RunContext {
            messages: all_messages,
            tools,
        }
    }

    fn fixed_intro() -> String {
        "You are SlimBot, an AI assistant. You can call tools to help the user complete tasks."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_full() {
        let input = r#"---
name: memory
description: Two-layer memory system.
always: true
---

# Memory content here
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "memory");
        assert_eq!(meta.description, "Two-layer memory system.");
        assert_eq!(meta.always, true);
        assert_eq!(meta.content, "# Memory content here");
    }

    #[test]
    fn test_parse_frontmatter_no_always() {
        let input = r#"---
name: custom
description: A custom skill.
---

Skill body.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "custom");
        assert_eq!(meta.always, false);
    }

    #[test]
    fn test_parse_no_frontmatter_returns_none() {
        assert!(parse_skill_frontmatter("# Just a markdown file").is_none());
    }

    #[test]
    fn test_parse_no_closing_delimiter_returns_none() {
        assert!(parse_skill_frontmatter("---\nname: test\nno closing").is_none());
    }
}
