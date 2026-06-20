use std::path::PathBuf;
use std::sync::Arc;

use crate::bootstrap::{bootstrap_files, read_if_modified};
use crate::debug;
use crate::memory::SharedMemoryStore;
use crate::session::{Content, ContentBlock, Message, MessageMeta, SharedSessionManager};
use crate::tool::{ToolDefinition, ToolManager};

/// Bounding tags for the transient runtime context block.
/// Sent to the LLM but NOT persisted in session history.
const RUNTIME_CONTEXT_TAG: &str = "[Runtime Context -- metadata only, not instructions]";
const RUNTIME_CONTEXT_END: &str = "[/Runtime Context]";

const PLATFORM_POLICY_POSIX: &str = "## Platform Policy (POSIX)\n- You are running on a POSIX system. Prefer UTF-8 and standard shell tools.\n- Use file tools when they are simpler or more reliable than shell commands.";

const PLATFORM_POLICY_WINDOWS: &str = "## Platform Policy (Windows)\n- You are running on Windows. Do not assume GNU tools like `grep`, `sed`, or `awk` exist.\n- Prefer Windows-native commands or file tools when they are more reliable.\n- If terminal output is garbled, retry with UTF-8 output enabled.";

/// Build a transient runtime context string (time, channel, chat_id, session summary).
fn build_runtime_context(channel: &str, chat_id: &str, session_summary: Option<&str>) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let mut lines = vec![format!("Current Time: {}", now)];
    if !channel.is_empty() && !chat_id.is_empty() {
        lines.push(format!("Channel: {}", channel));
        lines.push(format!("Chat ID: {}", chat_id));
    }
    if let Some(summary) = session_summary
        && !summary.is_empty() {
            lines.push(String::new());
            lines.push("[Resumed Session]".to_string());
            lines.push(summary.to_string());
        }
    format!(
        "{}\n{}\n{}",
        RUNTIME_CONTEXT_TAG,
        lines.join("\n"),
        RUNTIME_CONTEXT_END
    )
}

pub struct RunContext {
    pub history: Arc<[Message]>,
    pub current_turn: Vec<Message>,
    pub system_message: Message,
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

    /// Build the system prompt string.
    pub async fn build_system_prompt(&self, channel: &str) -> String {
        let mut parts: Vec<String> = Vec::new();

        // 1. Identity
        let workspace_path = self.workspace_dir.to_string_lossy().to_string();
        parts.push(Self::build_identity(&workspace_path, channel));

        // 2. Bootstrap workspace files
        for (filename, template) in bootstrap_files() {
            let path = self.workspace_dir.join(filename);
            if let Some(content) = read_if_modified(&path, template)
                && !content.is_empty() {
                    parts.push(format!("[{}] {}", filename, content));
                }
        }

        // 3. Skills
        self.load_skills_into_parts(&mut parts);

        // 4. Memory (skip if matches template)
        let ms = self.memory_store.lock().await;
        let memory_content = ms.get_memory_context();
        if !memory_content.is_empty() {
            let raw_memory = ms.read_memory();
            if !crate::bootstrap::is_template_content(&raw_memory, "memory/MEMORY.md") {
                parts.push(memory_content);
            }
        }

        // 5. Recent history
        let recent_entries = ms.read_recent_history(50);
        drop(ms);
        if !recent_entries.is_empty() {
            let history_text = recent_entries
                .iter()
                .map(|e| format!("- [{}] {}", e.timestamp, e.content))
                .collect::<Vec<_>>()
                .join("\n");
            parts.push(format!("# Recent History\n\n{history_text}"));
        }

        parts.join("\n\n---\n\n")
    }

    /// Load skills into system prompt parts (helper for build_system_prompt).
    fn load_skills_into_parts(&self, parts: &mut Vec<String>) {
        let skills_dir = self.workspace_dir.join("skills");
        if !skills_dir.exists() {
            return;
        }
        let mut always_skills: Vec<String> = Vec::new();
        let mut available_skills: Vec<String> = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md")
                    && let Ok(content) = std::fs::read_to_string(&path) {
                        if content.is_empty() {
                            continue;
                        }
                        if let Some(meta) = parse_skill_frontmatter(&content) {
                            if meta.always {
                                always_skills
                                    .push(format!("### Skill: {}\n\n{}", meta.name, meta.content));
                            } else {
                                available_skills.push(format!(
                                    "- **{}** — {}  `{}`",
                                    meta.name,
                                    meta.description,
                                    path.display()
                                ));
                            }
                        } else {
                            let name = path
                                .file_stem()
                                .map(|s| s.to_string_lossy().to_string())
                                .unwrap_or_default();
                            always_skills.push(format!("### Skill: {}\n\n{}", name, content));
                        }
                    }
            }
        }

        if !always_skills.is_empty() {
            parts.push("# Active Skills\n\n".to_string() + &always_skills.join("\n\n---\n\n"));
        }
        if !available_skills.is_empty() {
            let list = available_skills.join("\n");
            parts.push(format!(
                "# Available Skills\n\nThe following skills are available. Use file_reader to load them when needed.\n{list}"
            ));
        }
    }

    /// Build the complete message list for an LLM call using two-list approach.
    pub async fn build_messages(
        &self,
        session_id: &str,
        channel: &str,
        chat_id: &str,
        session_summary: Option<&str>,
    ) -> RunContext {
        debug!(
            "[ContextBuilder] Starting build_messages for session={}",
            session_id
        );

        // Build system prompt
        let system_prompt = self.build_system_prompt(channel).await;
        let system_message = Message::system(system_prompt);

        // Get history Arc and current_turn Vec from SessionManager
        let (history, mut current_turn) = {
            let sm = self.session_manager.lock().await;
            (
                sm.get_history_arc(session_id),
                sm.get_current_turn_messages(session_id),
            )
        };

        // Build runtime context and set on first user message in current_turn that doesn't have it
        let runtime_ctx = build_runtime_context(channel, chat_id, session_summary);
        for msg in &mut current_turn {
            if let Message::User {
                runtime_content, ..
            } = msg
                && runtime_content.is_none() {
                    *runtime_content = Some(runtime_ctx);
                    break;
                }
        }

        // Merge consecutive user messages at the end
        Self::merge_consecutive_user_messages(&mut current_turn);

        let tools = Some(self.tool_manager.to_openai_functions());
        debug!(
            "[ContextBuilder] Tools count={}",
            tools.as_ref().map(|t| t.len()).unwrap_or(0)
        );

        RunContext {
            history,
            current_turn,
            system_message,
            tools,
        }
    }

    /// Merge consecutive User messages at the end of the list to avoid same-role rejection.
    fn merge_consecutive_user_messages(messages: &mut Vec<Message>) {
        let len = messages.len();
        if len >= 2
            && matches!(messages[len - 2], Message::User { .. })
                && matches!(messages[len - 1], Message::User { .. })
            {
                let last = messages.pop().unwrap();
                if let Message::User {
                    content: last_content,
                    runtime_content: last_runtime,
                    ..
                } = last
                {
                    let prev = std::mem::replace(
                        &mut messages[len - 2],
                        Message::user("__temp__".to_string()),
                    );
                    if let Message::User {
                        content: prev_content,
                        runtime_content: prev_runtime,
                        ..
                    } = prev
                    {
                        let merged = match (prev_content, last_content) {
                            (Content::Plain(a), Content::Plain(b)) => {
                                Content::Plain(format!("{}\n\n{}", a, b))
                            }
                            (Content::Plain(a), Content::Multi(b)) => {
                                let mut blocks = vec![ContentBlock::Text { text: a.clone() }];
                                blocks.extend(b);
                                Content::Multi(blocks)
                            }
                            (Content::Multi(a), Content::Plain(b)) => {
                                let mut blocks = a.clone();
                                blocks.push(ContentBlock::Text { text: b });
                                Content::Multi(blocks)
                            }
                            (Content::Multi(a), Content::Multi(b)) => {
                                let mut blocks = a.clone();
                                blocks.extend(b);
                                Content::Multi(blocks)
                            }
                        };
                        let runtime = prev_runtime.or(last_runtime);
                        messages[len - 2] = Message::User {
                            meta: MessageMeta::default(),
                            content: merged,
                            runtime_content: runtime,
                        };
                    }
                }
            }
    }

    /// Append a tool result message to the message list.
    pub fn add_tool_result(
        messages: &mut Vec<Message>,
        tool_call_id: String,
        tool_name: String,
        result: String,
    ) {
        messages.push(Message::tool(result, tool_call_id, Some(tool_name)));
    }

    // --- Static methods unchanged ---

    fn build_identity(workspace_path: &str, channel: &str) -> String {
        let runtime = Self::runtime_string();
        let platform_policy = if cfg!(target_os = "windows") {
            PLATFORM_POLICY_WINDOWS
        } else {
            PLATFORM_POLICY_POSIX
        };
        let hint = Self::channel_format_hint(channel);

        let template =
            crate::embed::get_content("identity.md").unwrap_or("You are SlimBot, an AI assistant.");

        template
            .replace("<<runtime>>", &runtime)
            .replace("<<workspace_path>>", workspace_path)
            .replace("<<platform_policy>>", platform_policy)
            .replace("<<channel_format_hint>>", hint)
    }

    fn runtime_string() -> String {
        let os_name = if cfg!(target_os = "macos") {
            "macOS"
        } else if cfg!(target_os = "linux") {
            "Linux"
        } else if cfg!(target_os = "windows") {
            "Windows"
        } else {
            "Unknown"
        };
        let arch = std::env::consts::ARCH;
        format!("{} {}, Rust", os_name, arch)
    }

    fn channel_format_hint(channel: &str) -> &'static str {
        match channel {
            "telegram" | "qq" | "discord" => {
                "## Format Hint\nThis conversation is on a messaging app. Use short paragraphs. Avoid large headings (#, ##). Use **bold** sparingly. No tables — use plain lists."
            }
            "whatsapp" | "sms" => {
                "## Format Hint\nThis conversation is on a text messaging app that does not render markdown. Use plain text only."
            }
            "email" => {
                "## Format Hint\nThis conversation is via email. Structure with clear sections. Markdown may not render — keep formatting simple."
            }
            "cli" | "mochat" => {
                "## Format Hint\nOutput is rendered in a terminal. Avoid markdown headings and tables. Use plain text with minimal formatting."
            }
            _ => "",
        }
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
    fn test_channel_format_hint_known() {
        assert!(ContextBuilder::channel_format_hint("telegram").contains("messaging app"));
        assert!(ContextBuilder::channel_format_hint("whatsapp").contains("plain text"));
        assert!(ContextBuilder::channel_format_hint("email").contains("email"));
        assert!(ContextBuilder::channel_format_hint("cli").contains("terminal"));
    }

    #[test]
    fn test_channel_format_hint_unknown() {
        assert!(ContextBuilder::channel_format_hint("unknown").is_empty());
    }

    #[test]
    fn test_runtime_string_nonempty() {
        let rt = ContextBuilder::runtime_string();
        assert!(!rt.is_empty());
        assert!(rt.contains("Rust"));
    }

    #[test]
    fn test_add_tool_result() {
        let mut messages = vec![Message::assistant(Some("ok".to_string()), None, None, None)];
        ContextBuilder::add_tool_result(
            &mut messages,
            "call-1".to_string(),
            "read_file".to_string(),
            "file contents".to_string(),
        );
        assert_eq!(messages.len(), 2);
        if let Message::Tool {
            content,
            tool_call_id,
            name,
            ..
        } = &messages[1]
        {
            assert_eq!(content, "file contents");
            assert_eq!(tool_call_id, "call-1");
            assert_eq!(name.as_deref(), Some("read_file"));
        } else {
            panic!("expected Tool message");
        }
    }

    #[test]
    fn test_merge_consecutive_user_plain_plain() {
        let mut msgs = vec![
            Message::user("first".to_string()),
            Message::user("second".to_string()),
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User { content, .. } = &msgs[0] {
            match content {
                Content::Plain(s) => assert_eq!(s, "first\n\nsecond"),
                _ => panic!("expected Plain content"),
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_merge_consecutive_user_plain_multi() {
        let multi = Content::Multi(vec![ContentBlock::Text {
            text: "multi".to_string(),
        }]);
        let mut msgs = vec![
            Message::user("plain".to_string()),
            Message::User {
                meta: MessageMeta::default(),
                content: multi,
                runtime_content: None,
            },
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User { content, .. } = &msgs[0] {
            match content {
                Content::Multi(blocks) => {
                    assert!(
                        blocks.len() >= 2,
                        "expected at least 2 blocks, got {}",
                        blocks.len()
                    );
                }
                _ => panic!("expected Multi content"),
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_merge_consecutive_user_multi_plain() {
        let multi = Content::Multi(vec![ContentBlock::Text {
            text: "multi".to_string(),
        }]);
        let mut msgs = vec![
            Message::User {
                meta: MessageMeta::default(),
                content: multi,
                runtime_content: None,
            },
            Message::user("plain".to_string()),
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User { content, .. } = &msgs[0] {
            match content {
                Content::Multi(blocks) => {
                    assert!(
                        blocks.len() >= 2,
                        "expected at least 2 blocks, got {}",
                        blocks.len()
                    );
                }
                _ => panic!("expected Multi content"),
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_merge_consecutive_user_multi_multi() {
        let multi1 = Content::Multi(vec![ContentBlock::Text {
            text: "first".to_string(),
        }]);
        let multi2 = Content::Multi(vec![ContentBlock::Text {
            text: "second".to_string(),
        }]);
        let mut msgs = vec![
            Message::User {
                meta: MessageMeta::default(),
                content: multi1,
                runtime_content: None,
            },
            Message::User {
                meta: MessageMeta::default(),
                content: multi2,
                runtime_content: None,
            },
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User { content, .. } = &msgs[0] {
            match content {
                Content::Multi(blocks) => {
                    assert!(
                        blocks.len() >= 2,
                        "expected at least 2 blocks, got {}",
                        blocks.len()
                    );
                }
                _ => panic!("expected Multi content"),
            }
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_merge_consecutive_non_user_no_change() {
        let mut msgs = vec![
            Message::assistant(Some("ok".to_string()), None, None, None),
            Message::user("hello".to_string()),
        ];
        let before_len = msgs.len();
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), before_len); // no merge for non-consecutive user
    }

    #[test]
    fn test_merge_consecutive_single_user_no_change() {
        let mut msgs = vec![Message::user("hello".to_string())];
        let before_len = msgs.len();
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), before_len);
    }

    #[test]
    fn test_merge_consecutive_preserves_runtime_content() {
        let mut msgs = vec![
            Message::User {
                meta: MessageMeta::default(),
                content: Content::Plain("first".to_string()),
                runtime_content: Some("runtime1".to_string()),
            },
            Message::User {
                meta: MessageMeta::default(),
                content: Content::Plain("second".to_string()),
                runtime_content: Some("runtime2".to_string()),
            },
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User {
            runtime_content, ..
        } = &msgs[0]
        {
            // First user's runtime_content takes precedence (it was already set)
            assert_eq!(runtime_content, &Some("runtime1".to_string()));
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_merge_consecutive_runtime_content_or() {
        let mut msgs = vec![
            Message::User {
                meta: MessageMeta::default(),
                content: Content::Plain("first".to_string()),
                runtime_content: None,
            },
            Message::User {
                meta: MessageMeta::default(),
                content: Content::Plain("second".to_string()),
                runtime_content: Some("runtime".to_string()),
            },
        ];
        ContextBuilder::merge_consecutive_user_messages(&mut msgs);
        assert_eq!(msgs.len(), 1);
        if let Message::User {
            runtime_content, ..
        } = &msgs[0]
        {
            // Second user's runtime_content fills in
            assert_eq!(runtime_content, &Some("runtime".to_string()));
        } else {
            panic!("expected user message");
        }
    }

    #[test]
    fn test_parse_skill_frontmatter_empty_name_falls_back() {
        let input = r#"---
name:
description: A skill.
---

Body
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "unknown");
    }

    #[test]
    fn test_parse_skill_frontmatter_empty_content_skipped() {
        let input = "---\nname: test\ndescription: desc\n---\n";
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.content, "");
    }

    #[test]
    fn test_parse_skill_frontmatter_with_always_false() {
        let input = r#"---
name: optional-skill
description: An optional skill.
always: false
---

Skill content here.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "optional-skill");
        assert!(!meta.always);
        assert_eq!(meta.content, "Skill content here.");
    }

    #[test]
    fn test_parse_skill_frontmatter_with_always_true() {
        let input = r#"---
name: required-skill
description: A required skill.
always: true
---

Required skill content.
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert_eq!(meta.name, "required-skill");
        assert!(meta.always);
        assert_eq!(meta.content, "Required skill content.");
    }

    #[test]
    fn test_parse_skill_frontmatter_multiline_content() {
        let input = r#"---
name: multi
description: Multi-line content.
---

# Heading

Paragraph 1.

Paragraph 2.

```rust
let x = 42;
```
"#;
        let meta = parse_skill_frontmatter(input).unwrap();
        assert!(meta.content.contains("# Heading"));
        assert!(meta.content.contains("Paragraph 1."));
        assert!(meta.content.contains("Paragraph 2."));
        assert!(meta.content.contains("```rust"));
    }

    #[test]
    fn test_build_runtime_context_with_empty_summary() {
        let ctx = build_runtime_context("webui", "chat-123", Some(""));
        assert!(!ctx.contains("[Resumed Session]"));
    }

    #[test]
    fn test_build_runtime_context_with_summary_variant() {
        let ctx = build_runtime_context("cli", "default", Some("User prefers Rust"));
        assert!(ctx.contains("[Resumed Session]"));
        assert!(ctx.contains("User prefers Rust"));
    }

    #[test]
    fn test_runtime_string_contains_rust() {
        let rt = ContextBuilder::runtime_string();
        assert!(rt.contains("Rust"));
    }

    #[test]
    fn test_runtime_string_contains_os() {
        let rt = ContextBuilder::runtime_string();
        // Should contain macOS, Linux, or Windows
        assert!(rt.contains("macOS") || rt.contains("Linux") || rt.contains("Windows"));
    }

    #[test]
    fn test_channel_format_hint_telegram() {
        let hint = ContextBuilder::channel_format_hint("telegram");
        assert!(hint.contains("messaging app"));
    }

    #[test]
    fn test_channel_format_hint_discord() {
        let hint = ContextBuilder::channel_format_hint("discord");
        assert!(hint.contains("messaging app"));
    }

    #[test]
    fn test_channel_format_hint_qq() {
        let hint = ContextBuilder::channel_format_hint("qq");
        assert!(hint.contains("messaging app"));
    }

    #[test]
    fn test_channel_format_hint_whatsapp() {
        let hint = ContextBuilder::channel_format_hint("whatsapp");
        assert!(hint.contains("plain text"));
    }

    #[test]
    fn test_channel_format_hint_sms() {
        let hint = ContextBuilder::channel_format_hint("sms");
        assert!(hint.contains("plain text"));
    }

    #[test]
    fn test_channel_format_hint_email() {
        let hint = ContextBuilder::channel_format_hint("email");
        assert!(hint.contains("email"));
    }

    #[test]
    fn test_channel_format_hint_cli() {
        let hint = ContextBuilder::channel_format_hint("cli");
        assert!(hint.contains("terminal"));
    }

    #[test]
    fn test_channel_format_hint_mochat() {
        let hint = ContextBuilder::channel_format_hint("mochat");
        assert!(hint.contains("terminal"));
    }

    #[test]
    fn test_channel_format_hint_unknown_returns_empty() {
        let hint = ContextBuilder::channel_format_hint("unknown-channel");
        assert!(hint.is_empty());
    }
}
