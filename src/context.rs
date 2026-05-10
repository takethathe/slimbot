use std::path::PathBuf;
use std::sync::Arc;

use crate::bootstrap::{bootstrap_files, read_if_modified};
use crate::memory::SharedMemoryStore;
use crate::session::{Message, SharedSessionManager, parse_session_origin};
use crate::tool::{ToolDefinition, ToolManager};
use crate::debug;

/// Bounding tags for the transient runtime context block.
/// Sent to the LLM but NOT persisted in session history.
const RUNTIME_CONTEXT_TAG: &str = "[Runtime Context -- metadata only, not instructions]";
const RUNTIME_CONTEXT_END: &str = "[/Runtime Context]";

/// Build a transient runtime context string (time, channel, chat_id, session summary).
fn build_runtime_context(channel: &str, chat_id: &str, session_summary: Option<&str>) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let mut lines = vec![format!("Current Time: {}", now)];
    if !channel.is_empty() && !chat_id.is_empty() {
        lines.push(format!("Channel: {}", channel));
        lines.push(format!("Chat ID: {}", chat_id));
    }
    if let Some(summary) = session_summary {
        if !summary.is_empty() {
            lines.push(String::new());
            lines.push("[Resumed Session]".to_string());
            lines.push(summary.to_string());
        }
    }
    format!("{}\n{}\n{}", RUNTIME_CONTEXT_TAG, lines.join("\n"), RUNTIME_CONTEXT_END)
}

/// Merge two content strings. If either is empty, return the other; otherwise concatenate with blank line separator.
fn merge_content(left: &str, right: &str) -> String {
    if left.is_empty() {
        right.to_string()
    } else if right.is_empty() {
        left.to_string()
    } else {
        format!("{}\n\n{}", left, right)
    }
}

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
    memory_store: SharedMemoryStore,
}

impl ContextBuilder {
    pub fn new(
        session_manager: SharedSessionManager,
        tool_manager: Arc<ToolManager>,
        workspace_dir: PathBuf,
        memory_store: SharedMemoryStore,
    ) -> Self {
        Self {
            session_manager,
            tool_manager,
            workspace_dir,
            memory_store,
        }
    }

    pub async fn build(&self, session_id: &str, channel_inject: Option<String>, session_summary: Option<&str>, origin_channel: Option<&str>, origin_chat_id: Option<&str>) -> RunContext {
        debug!("[ContextBuilder] Starting build for session={}", session_id);
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
        let ms = self.memory_store.lock().await;
        let memory_content = ms.get_memory_context();
        if !memory_content.is_empty() {
            system_parts.push(memory_content);
        }

        // 4.5. Session summary from consolidation (replaces evicted messages)
        if let Some(summary) = session_summary {
            if !summary.is_empty() {
                system_parts.push(format!("[Resumed Session] {summary}"));
            }
        }

        // 5. Recent history from history.jsonl (last 50 entries, nanobot default)
        let recent_entries = ms.read_recent_history(50);
        if !recent_entries.is_empty() {
            let history_text = recent_entries
                .iter()
                .map(|e| format!("- [{}] {}", e.timestamp, e.content))
                .collect::<Vec<_>>()
                .join("\n");
            system_parts.push(format!("# Recent History\n\n{history_text}"));
        }
        drop(ms);

        // 6. History messages (unconsolidated session messages)
        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(session_id).await
        };
        debug!("[ContextBuilder] Session messages count={}", messages.len());

        // 7. Channel self-injected info
        if let Some(inject) = channel_inject {
            if !inject.is_empty() {
                system_parts.push(inject);
            }
        }

        // Assemble system prompt
        let system_prompt = system_parts.join("\n\n---\n\n");
        debug!("[ContextBuilder] System prompt total len={}", system_prompt.len());
        let mut all_messages = vec![Message::system(system_prompt)];
        all_messages.extend(messages);

        // 8. Inject runtime context into the last user message (transient, not persisted).
        // Use origin channel/chat_id if provided (e.g. from cron payload), otherwise parse from session_id.
        let (channel, chat_id) = parse_session_origin(session_id, (origin_channel, origin_chat_id));
        let runtime_ctx = build_runtime_context(&channel, &chat_id, session_summary);

        // Find the last User message and prepend runtime context to it.
        // If the last message is already User, merge into it to avoid consecutive same-role messages.
        if let Some(last_user_idx) = all_messages.iter().rposition(|m| matches!(m, Message::User { .. })) {
            if let Message::User { content, .. } = &all_messages[last_user_idx] {
                let merged = merge_content(content, &runtime_ctx);
                all_messages[last_user_idx] = Message::user(merged);
            }
        }

        let tools = Some(self.tool_manager.to_openai_functions());
        debug!("[ContextBuilder] Tools count={}", tools.as_ref().map(|t| t.len()).unwrap_or(0));

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

    #[test]
    fn test_build_runtime_context_basic() {
        let ctx = build_runtime_context("cli", "default", None);
        assert!(ctx.starts_with(RUNTIME_CONTEXT_TAG));
        assert!(ctx.ends_with(RUNTIME_CONTEXT_END));
        assert!(ctx.contains("Current Time:"));
        assert!(ctx.contains("Channel: cli"));
        assert!(ctx.contains("Chat ID: default"));
        assert!(!ctx.contains("[Resumed Session]"));
    }

    #[test]
    fn test_build_runtime_context_with_summary() {
        let ctx = build_runtime_context("webui", "chat-1", Some("User asked about files"));
        assert!(ctx.starts_with(RUNTIME_CONTEXT_TAG));
        assert!(ctx.ends_with(RUNTIME_CONTEXT_END));
        assert!(ctx.contains("Channel: webui"));
        assert!(ctx.contains("Chat ID: chat-1"));
        assert!(ctx.contains("[Resumed Session]"));
        assert!(ctx.contains("User asked about files"));
    }

    #[test]
    fn test_build_runtime_context_empty_channel() {
        let ctx = build_runtime_context("", "", None);
        assert!(ctx.starts_with(RUNTIME_CONTEXT_TAG));
        assert!(!ctx.contains("Channel:"));
        assert!(!ctx.contains("Chat ID:"));
    }

    #[test]
    fn test_build_runtime_context_empty_summary() {
        let ctx = build_runtime_context("cli", "default", Some(""));
        assert!(!ctx.contains("[Resumed Session]"));
    }

    #[test]
    fn test_merge_content() {
        assert_eq!(merge_content("", "hello"), "hello");
        assert_eq!(merge_content("hello", ""), "hello");
        assert_eq!(merge_content("", ""), "");
        let result = merge_content("existing", "new");
        assert!(result.contains("existing"));
        assert!(result.contains("new"));
        assert!(result.contains("\n\n"));
    }
}
