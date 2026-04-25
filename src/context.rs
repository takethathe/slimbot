use std::path::PathBuf;
use std::sync::Arc;

use crate::session::{Message, SharedSessionManager};
use crate::tool::{ToolDefinition, ToolManager};

pub struct RunContext {
    pub messages: Vec<Message>,
    pub tools: Option<Vec<ToolDefinition>>,
}

pub struct ContextBuilder {
    session_manager: SharedSessionManager,
    tool_manager: Arc<ToolManager>,
    data_dir: PathBuf,
}

impl ContextBuilder {
    pub fn new(
        session_manager: SharedSessionManager,
        tool_manager: Arc<ToolManager>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            session_manager,
            tool_manager,
            data_dir,
        }
    }

    pub async fn build(&self, session_id: &str, channel_inject: Option<String>) -> RunContext {
        let mut system_parts: Vec<String> = Vec::new();

        // 1. Fixed brief intro
        system_parts.push(Self::fixed_intro());

        // 2. Fixed workspace files
        for file in ["agent.md", "user.md", "soul.md", "tools.md"] {
            let path = self.data_dir.join(file);
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.is_empty() {
                        system_parts.push(format!("[{}] {}", file, content));
                    }
                }
            }
        }

        // 3. Skills import
        let skills_dir = self.data_dir.join("skills");
        if skills_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map_or(false, |e| e == "md") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if !content.is_empty() {
                                let name = path.file_stem()
                                    .map(|s| s.to_string_lossy().to_string())
                                    .unwrap_or_default();
                                system_parts.push(format!("[Skill: {}] {}", name, content));
                            }
                        }
                    }
                }
            }
        }

        // 4. History messages
        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(session_id).await
        };

        // 5. Channel self-injected info
        if let Some(inject) = channel_inject {
            if !inject.is_empty() {
                system_parts.push(inject);
            }
        }

        // Assemble system prompt
        let system_prompt = system_parts.join("\n\n---\n\n");
        let mut all_messages = vec![Message::System { content: system_prompt }];
        all_messages.extend(messages);

        let tools = Some(self.tool_manager.to_openai_functions());

        RunContext {
            messages: all_messages,
            tools,
        }
    }

    fn fixed_intro() -> String {
        "You are SlimBot, an AI assistant. You can call tools to help the user complete tasks.".to_string()
    }
}
