use std::path::PathBuf;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crate::bootstrap::{bootstrap_files, read_if_modified};
use crate::memory::SharedMemoryStore;
use crate::session::{Content, ContentBlock, Message, MessageMeta, SharedSessionManager, parse_session_origin};
use crate::tool::{ToolDefinition, ToolManager};
use crate::debug;

/// Bounding tags for the transient runtime context block.
/// Sent to the LLM but NOT persisted in session history.
const RUNTIME_CONTEXT_TAG: &str = "[Runtime Context -- metadata only, not instructions]";
const RUNTIME_CONTEXT_END: &str = "[/Runtime Context]";

const PLATFORM_POLICY_POSIX: &str = "## Platform Policy (POSIX)\n- You are running on a POSIX system. Prefer UTF-8 and standard shell tools.\n- Use file tools when they are simpler or more reliable than shell commands.";

const PLATFORM_POLICY_WINDOWS: &str = "## Platform Policy (Windows)\n- You are running on Windows. Do not assume GNU tools like `grep`, `sed`, or `awk` exist.\n- Prefer Windows-native commands or file tools when they are more reliable.\n- If terminal output is garbled, retry with UTF-8 output enabled.";

fn base64_encode(data: &[u8]) -> String {
    STANDARD.encode(data)
}

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

    /// Build the system prompt string.
    pub async fn build_system_prompt(&self, channel: &str) -> String {
        let mut parts: Vec<String> = Vec::new();

        // 1. Identity
        let workspace_path = self.workspace_dir.to_string_lossy().to_string();
        parts.push(Self::build_identity(&workspace_path, channel));

        // 2. Bootstrap workspace files
        for (filename, template) in bootstrap_files() {
            let path = self.workspace_dir.join(filename);
            if let Some(content) = read_if_modified(&path, template) {
                if !content.is_empty() {
                    parts.push(format!("[{}] {}", filename, content));
                }
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
                if path.extension().map_or(false, |e| e == "md") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if content.is_empty() {
                            continue;
                        }
                        if let Some(meta) = parse_skill_frontmatter(&content) {
                            if meta.always {
                                always_skills.push(format!("### Skill: {}\n\n{}", meta.name, meta.content));
                            } else {
                                available_skills.push(format!(
                                    "- **{}** — {}  `{}`",
                                    meta.name, meta.description, path.display()
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

    /// Build the complete message list for an LLM call.
    pub async fn build_messages(
        &self,
        session_id: &str,
        current_message: String,
        channel: &str,
        chat_id: &str,
        session_summary: Option<&str>,
        media: Option<&[String]>,
    ) -> RunContext {
        debug!("[ContextBuilder] Starting build_messages for session={}", session_id);

        // Build system prompt and prepend it as the first message
        let system_prompt = self.build_system_prompt(channel).await;
        let mut messages = vec![Message::system(system_prompt)];

        // Build user content with optional media
        let user_content = Self::build_user_content(&current_message, media);

        // Build runtime context
        let runtime_ctx = build_runtime_context(channel, chat_id, session_summary);

        // Merge runtime context with user content
        let merged_content = match user_content {
            Content::Plain(text) => {
                let merged = merge_content(&runtime_ctx, &text);
                Content::Plain(merged)
            }
            Content::Multi(mut blocks) => {
                blocks.insert(0, ContentBlock::Text { text: runtime_ctx });
                Content::Multi(blocks)
            }
        };

        // Get session messages and extend
        let session_messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(session_id).await
        };
        debug!("[ContextBuilder] Session messages count={}", session_messages.len());
        messages.extend(session_messages);

        // Append current user message
        messages.push(Message::User {
            meta: MessageMeta::default(),
            content: merged_content,
        });

        // Merge with last existing user message if same role
        Self::merge_consecutive_user_messages(&mut messages);

        let tools = Some(self.tool_manager.to_openai_functions());
        debug!("[ContextBuilder] Tools count={}", tools.as_ref().map(|t| t.len()).unwrap_or(0));

        RunContext { messages, tools }
    }

    /// Build user content with optional media attachments.
    fn build_user_content(text: &str, media: Option<&[String]>) -> Content {
        match media {
            None | Some([]) => Content::Plain(text.to_string()),
            Some(paths) => {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                for path in paths {
                    if let Ok(raw) = std::fs::read(path) {
                        if let Some(mime) = Self::detect_image_mime(&raw) {
                            blocks.push(ContentBlock::Image { mime_type: mime.to_string(), base64_data: base64_encode(&raw) });
                        }
                    }
                }
                if blocks.is_empty() {
                    Content::Plain(text.to_string())
                } else {
                    blocks.push(ContentBlock::Text { text: text.to_string() });
                    Content::Multi(blocks)
                }
            }
        }
    }

    /// Detect image MIME type from magic bytes.
    fn detect_image_mime(raw: &[u8]) -> Option<&'static str> {
        if raw.starts_with(&[0x89, 0x50, 0x4E, 0x47]) { Some("image/png") }
        else if raw.starts_with(&[0xFF, 0xD8, 0xFF]) { Some("image/jpeg") }
        else if raw.starts_with(b"GIF8") { Some("image/gif") }
        else if raw.starts_with(b"RIFF") && raw.len() > 8 && &raw[8..12] == b"WEBP" { Some("image/webp") }
        else { None }
    }

    /// Merge consecutive User messages at the end of the list to avoid same-role rejection.
    fn merge_consecutive_user_messages(messages: &mut Vec<Message>) {
        let len = messages.len();
        if len >= 2 {
            if matches!(messages[len - 2], Message::User { .. }) && matches!(messages[len - 1], Message::User { .. }) {
                let last = messages.pop().unwrap();
                if let Message::User { content: last_content, .. } = last {
                    // Take out the second-to-last message, merge, and put it back.
                    let prev = std::mem::replace(
                        &mut messages[len - 2],
                        Message::user("__temp__".to_string()),
                    );
                    if let Message::User { content: prev_content, .. } = prev {
                        let merged = match (prev_content, last_content) {
                            (Content::Plain(a), Content::Plain(b)) => Content::Plain(format!("{}\n\n{}", a, b)),
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
                        messages[len - 2] = Message::User {
                            meta: MessageMeta::default(),
                            content: merged,
                        };
                    }
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

    /// Legacy compatibility wrapper.
    pub async fn build(&self, session_id: &str, channel_inject: Option<String>, session_summary: Option<&str>, origin_channel: Option<&str>, origin_chat_id: Option<&str>) -> RunContext {
        debug!("[ContextBuilder] Starting build for session={}", session_id);
        let (channel, chat_id) = parse_session_origin(session_id, (origin_channel, origin_chat_id));
        let channel_str = channel.clone();

        let mut system_parts: Vec<String> = Vec::new();

        // 1. Identity block
        let workspace_path_str = self.workspace_dir.to_string_lossy().to_string();
        system_parts.push(Self::build_identity(&workspace_path_str, channel_inject.as_deref().unwrap_or("")));

        // 2. Bootstrap workspace files
        for (filename, template) in bootstrap_files() {
            let path = self.workspace_dir.join(filename);
            if let Some(content) = read_if_modified(&path, template) {
                if !content.is_empty() {
                    system_parts.push(format!("[{}] {}", filename, content));
                }
            }
        }

        // 3. Skills
        self.load_skills_into_parts(&mut system_parts);

        // 4. Memory (skip if matches template)
        let ms = self.memory_store.lock().await;
        let memory_content = ms.get_memory_context();
        if !memory_content.is_empty() {
            let raw_memory = ms.read_memory();
            if !crate::bootstrap::is_template_content(&raw_memory, "memory/MEMORY.md") {
                system_parts.push(memory_content);
            }
        }

        // 5. Recent history
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

        // 6. Channel self-injected info
        if let Some(inject) = channel_inject {
            if !inject.is_empty() {
                system_parts.push(inject);
            }
        }

        // Assemble system prompt
        let system_prompt = system_parts.join("\n\n---\n\n");
        debug!("[ContextBuilder] System prompt total len={}", system_prompt.len());
        let mut all_messages = vec![Message::system(system_prompt)];

        // Get session messages
        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(session_id).await
        };
        debug!("[ContextBuilder] Session messages count={}", messages.len());
        all_messages.extend(messages);

        // 7. Inject runtime context into the last user message
        let runtime_ctx = build_runtime_context(&channel_str, &chat_id, session_summary);
        if let Some(last_user_idx) = all_messages.iter().rposition(|m| matches!(m, Message::User { .. })) {
            if let Message::User { content, .. } = &all_messages[last_user_idx] {
                let merged = merge_content(content.as_text(), &runtime_ctx);
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

    // --- Static methods unchanged ---

    fn build_identity(workspace_path: &str, channel: &str) -> String {
        let runtime = Self::runtime_string();
        let platform_policy = if cfg!(target_os = "windows") {
            PLATFORM_POLICY_WINDOWS
        } else {
            PLATFORM_POLICY_POSIX
        };
        let hint = Self::channel_format_hint(channel);

        let template = crate::embed::get_content("identity.md")
            .unwrap_or("You are SlimBot, an AI assistant.");

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
    fn test_merge_content() {
        assert_eq!(merge_content("", "hello"), "hello");
        assert_eq!(merge_content("hello", ""), "hello");
        assert_eq!(merge_content("", ""), "");
        let result = merge_content("existing", "new");
        assert!(result.contains("existing"));
        assert!(result.contains("new"));
        assert!(result.contains("\n\n"));
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
        if let Message::Tool { content, tool_call_id, name, .. } = &messages[1] {
            assert_eq!(content, "file contents");
            assert_eq!(tool_call_id, "call-1");
            assert_eq!(name.as_deref(), Some("read_file"));
        } else {
            panic!("expected Tool message");
        }
    }

    #[test]
    fn test_build_user_content_no_media() {
        let content = ContextBuilder::build_user_content("hello", None);
        assert!(matches!(content, Content::Plain(_)));
        if let Content::Plain(s) = content {
            assert_eq!(s, "hello");
        }
    }

    #[test]
    fn test_build_user_content_missing_file() {
        let content = ContextBuilder::build_user_content("hello", Some(&["/nonexistent.png".to_string()]));
        assert!(matches!(content, Content::Plain(_)));
    }

    #[test]
    fn test_detect_image_mime_png() {
        let png_header = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(ContextBuilder::detect_image_mime(&png_header), Some("image/png"));
    }

    #[test]
    fn test_detect_image_mime_jpeg() {
        let jpeg_header = vec![0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(ContextBuilder::detect_image_mime(&jpeg_header), Some("image/jpeg"));
    }

    #[test]
    fn test_detect_image_mime_unknown() {
        assert_eq!(ContextBuilder::detect_image_mime(b"hello world"), None);
    }
}
